//! Shell state kept across launches: the sidebar, drafts and each gateway's last chat.

use super::{Context, PageTab, Subscription, Tone, Window, Workspace, preferences};
use mobius::protocol::{
    FrontendSettingKind, FrontendSettingValue, FrontendTone, MiddlewareFeature,
};
use mobius_gateway::wire::AgentComposition;
use preferences::Preferences;
use std::sync::Arc;

/// The first chosen option of `defaults` whose tone asks for care, as one sentence.
fn risky_default(features: &[MiddlewareFeature], defaults: &AgentComposition) -> Option<String> {
    features
        .iter()
        .filter(|feature| feature.required || defaults.middleware.enabled(&feature.id))
        .find_map(|feature| {
            feature.settings.iter().find_map(|setting| {
                let FrontendSettingKind::Select { options, .. } = &setting.kind else {
                    return None;
                };
                let Some(FrontendSettingValue::String(value)) =
                    defaults.middleware.setting(&feature.id, &setting.id)
                else {
                    return None;
                };
                options
                    .iter()
                    .find(|option| {
                        *option.value == **value
                            && matches!(option.tone, FrontendTone::Warning | FrontendTone::Error)
                    })
                    .map(|option| {
                        format!(
                            "{} is {}: {}.",
                            setting.label, option.label, option.description
                        )
                    })
            })
        })
}

impl Workspace {
    /// Saves `edit` to the preferences file off the main thread, after every earlier edit;
    /// a failure reports as "Could not save {what}".
    pub(in crate::ui) fn save_preferences(
        &mut self,
        what: &'static str,
        edit: impl FnOnce(&mut Preferences) + Send + 'static,
        cx: &Context<Self>,
    ) {
        self.preference_writes.queue(what, edit);
        if self.preference_drain.is_some() {
            return;
        }
        let writes = Arc::clone(&self.preference_writes);
        self.preference_drain = Some(cx.spawn(async move |this, cx| {
            loop {
                let drain = Arc::clone(&writes);
                let drained = cx
                    .background_executor()
                    .spawn(async move { drain.drain() })
                    .await;
                // Edits queued after that drain began found this task running.
                let finished = this.update(cx, |this, cx| {
                    if let Err(error) = drained {
                        this.report(format!("{error:#}"));
                        cx.notify();
                    }
                    let finished = !writes.has_queued();
                    if finished {
                        this.preference_drain = None;
                    }
                    finished
                });
                if finished.unwrap_or(true) {
                    break;
                }
            }
        }));
    }

    /// Writes every waiting edit before returning, after any write already in flight.
    fn flush_preferences(&mut self) {
        if let Err(error) = self.preference_writes.drain() {
            self.report(format!("{error:#}"));
        }
    }

    /// Says once per gateway when its Bot defaults choose an option that asks for care,
    /// such as an approval policy that runs commands without asking.
    pub(super) fn remind_risky_defaults(&mut self, cx: &Context<Self>) {
        let identity = if self.client.local_gateway {
            let Some(identity) = crate::gateway::local::gateway_identity() else {
                return;
            };
            identity
        } else {
            self.client.endpoint.clone() // rules-allow: the reminder is recorded under its own copy of the endpoint
        };
        if identity.is_empty() || self.shell.full_access_notices.contains(&identity) {
            return;
        }
        let Some(reminder) = self.client.state.gateway.as_ref().and_then(|gateway| {
            let catalog = &gateway.agent_catalog;
            risky_default(
                &catalog.middleware_features,
                &catalog.bot_defaults.as_ref()?.config,
            )
        }) else {
            return;
        };
        self.shell.full_access_notices.insert(identity.clone()); // rules-allow: the shell and the saved preferences each keep the reminder
        self.save_preferences(
            "the settings reminder",
            |preferences| {
                preferences.full_access_notices.insert(identity);
            },
            cx,
        );
        self.announce(Tone::Warning, reminder);
    }

    /// Saves the sidebar, drafts and last chat; errors surface as a toast.
    pub(in crate::ui) fn persist_shell(&mut self, cx: &Context<Self>) {
        let edit = self.shell_edit(cx);
        self.save_preferences("window state", edit, cx);
    }

    /// The window and the app are going away: the shell and everything queued before it
    /// reach the disk before this returns.
    fn flush_shell(&mut self, cx: &Context<Self>) {
        let edit = self.shell_edit(cx);
        self.preference_writes.queue("window state", edit);
        self.flush_preferences();
    }

    /// The edit that records the shell as it is now.
    fn shell_edit(&mut self, cx: &Context<Self>) -> impl FnOnce(&mut Preferences) + Send + 'static {
        self.save_draft(cx);
        let width = self.sidebar_resize.read(cx).sizes().first().copied();
        let visible = self.navigation_visible;
        let last = self
            .client
            .state
            .selected_session_id
            .as_deref()
            .filter(|_| !self.client.endpoint.is_empty())
            .map(|id| (self.client.endpoint.clone(), id.to_owned())); // rules-allow: the saved last chat outlives the connection's endpoint
        if let Some((endpoint, id)) = &last {
            self.shell
                .last_sessions
                .insert(endpoint.clone(), id.clone()); // rules-allow: the shell and the saved preferences each keep the last chat
        }
        let drafts = Preferences::kept_drafts(&self.drafts);
        move |preferences| {
            preferences.sidebar_visible = visible;
            preferences.sidebar_width = width.map(f32::from).or(preferences.sidebar_width);
            preferences.drafts = drafts;
            preferences.last_sessions.extend(last);
        }
    }

    /// Closing the window persists the shell and sends möbius to the menu bar instead of
    /// quitting, so chats, the gateway connection and notifications keep running; Quit
    /// (⌘Q or the menu bar item) ends it.
    pub(in crate::ui) fn persist_on_exit(window: &Window, cx: &Context<Self>) -> Subscription {
        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            if let Err(error) = view.update(cx, |this, cx| this.flush_shell(cx)) {
                tracing::debug!(%error, "the workspace closed before its shell was saved");
            }
            crate::host::platform::go_background();
            false
        });
        cx.on_app_quit(|this, cx| {
            this.flush_shell(cx);
            async {}
        })
    }

    /// On the first Ready, an untouched draft gives way to this gateway's last chat.
    pub(super) fn restore_last_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.shell.restored, true)
            || !matches!(self.primary_page(), Some(PageTab::Draft))
            || !self.composer.read(cx).value().trim().is_empty()
        {
            return;
        }
        let Some(id) = self.shell.last_sessions.get(&self.client.endpoint) else {
            return;
        };
        if let Some(session) = self
            .client
            .state
            .sessions
            .iter()
            .find(|session| session.session_id.as_str() == id)
        {
            let id = session.session_id.share();
            self.open_page(PageTab::Chat(id), window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::risky_default;
    use mobius::protocol::{FrontendSettingValue, MiddlewareFeature};
    use mobius_gateway::wire::AgentComposition;

    #[test]
    fn reminds_of_a_chosen_default_whose_tone_asks_for_care() {
        let option = |value: &str, tone: &str| {
            serde_json::json!({"value": value, "label": value, "description": "Runs freely",
                "symbol": null, "tone": tone, "disables": []})
        };
        let sandbox: MiddlewareFeature = serde_json::from_value(serde_json::json!({
            "id": "sandbox", "label": "Sandbox", "description": "", "required": true,
            "settings": [{"id": "approval_policy", "label": "Approval policy",
                "description": "", "composer": true, "type": "select",
                "options": [option("ask", "neutral"), option("allow", "warning")]}]
        }))
        .unwrap();
        let features = [sandbox];
        let mut config = AgentComposition::default();
        assert_eq!(risky_default(&features, &config), None);
        for (value, expected) in [
            ("ask", None),
            ("allow", Some("Approval policy is allow: Runs freely.")),
        ] {
            config.middleware.set_setting(
                "sandbox",
                "approval_policy",
                Some(FrontendSettingValue::String(value.into())),
            );
            assert_eq!(risky_default(&features, &config).as_deref(), expected);
        }
    }
}
