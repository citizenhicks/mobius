//! Providers (WP5): the setup list, the Add dialog and each setup's editor.

use super::*;
use mobius_gateway::wire::{
    ClientMessage, ConfiguredModel, ProviderAuthKind, ProviderConfig, ProviderEndpointAuth,
    ProviderInstance, ProviderStatus, ProviderTint, ServerMessage,
};
use std::collections::BTreeSet;

mod credential;
mod editor;
use editor::{ProviderDraft, SavedModel};

/// What the last credential or sign-in request is doing (ProviderActionState).
#[derive(Clone, Default, PartialEq, Debug)]
enum ProviderAction {
    #[default]
    Idle,
    Sending,
    Saved(String),
    StartingLogin(String),
    DeviceCode {
        label: String,
        url: String,
        code: String,
    },
    LoginFinished(String),
    Failed(Arc<str>),
}

#[derive(Default)]
pub(in crate::ui) struct ProviderSetup {
    generation: u64,
    /// Credential or sign-in request id to the registration sent once it succeeds.
    pending: BTreeMap<Arc<str>, ClientMessage>,
    /// Every request the editor sent, so its rejections show inline.
    requests: BTreeSet<Arc<str>>,
    action: ProviderAction,
    editor: Option<ProviderDraft>,
}

fn registration_label(message: &ClientMessage) -> String {
    match message {
        ClientMessage::RegisterProvider { label, .. } => label.clone().unwrap_or_default(), // rules-allow: the action and the pending registration each own the label
        _ => String::new(),
    }
}

impl ProviderSetup {
    pub(in crate::ui) fn reset(&mut self) {
        *self = Self {
            generation: self.generation + 1,
            ..Default::default()
        };
    }

    fn apply(&mut self, message: &ServerMessage) -> Option<ClientMessage> {
        match message {
            ServerMessage::Ready { .. } => self.reset(),
            ServerMessage::ProviderLoginStarted {
                request_id,
                verification_url,
                user_code,
                ..
            } => {
                let label = registration_label(self.pending.get(request_id)?);
                self.action = ProviderAction::DeviceCode {
                    label,
                    url: verification_url.clone(), // rules-allow: the server message is borrowed; the action keeps the URL
                    code: user_code.clone(), // rules-allow: the server message is borrowed; the action keeps the code
                };
            }
            ServerMessage::ProviderCredentialSaved { request_id, .. }
            | ServerMessage::ProviderLoginFinished { request_id, .. } => {
                let registration = self.pending.remove(request_id)?;
                let label = registration_label(&registration);
                self.action = if matches!(message, ServerMessage::ProviderLoginFinished { .. }) {
                    ProviderAction::LoginFinished(label)
                } else {
                    ProviderAction::Saved(label)
                };
                if let ClientMessage::RegisterProvider { request_id, .. } = &registration {
                    self.requests.insert(Arc::clone(request_id));
                }
                return Some(registration);
            }
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } if self.requests.remove(request_id) => {
                self.pending.remove(request_id);
                self.action = ProviderAction::Failed(Arc::clone(message));
            }
            _ => {}
        }
        None
    }
}

impl Workspace {
    pub(in crate::ui) fn provider_event(
        &mut self,
        message: &ServerMessage,
        cx: &mut Context<Self>,
    ) {
        let before = std::mem::discriminant(&self.provider_setup.action);
        let registration = self.provider_setup.apply(message);
        if std::mem::discriminant(&self.provider_setup.action) != before {
            self.announce_provider_action();
        }
        if let Some(registration) = registration {
            self.send(registration, cx);
        }
    }

    /// Credential progress and results arrive as toasts; a device code stays on the page
    /// because it is needed there.
    fn announce_provider_action(&mut self) {
        let (tone, message) = match &self.provider_setup.action {
            ProviderAction::Idle | ProviderAction::DeviceCode { .. } => return,
            ProviderAction::Sending => {
                (Tone::Info, "Sending the credential to the gateway…".into())
            }
            ProviderAction::Saved(label) => (
                Tone::Success,
                format!("{label} is configured on the gateway."),
            ),
            ProviderAction::StartingLogin(label) => {
                (Tone::Info, format!("Starting {label} sign-in…"))
            }
            ProviderAction::LoginFinished(label) => {
                (Tone::Success, format!("{label} is ready on the gateway."))
            }
            ProviderAction::Failed(message) => (Tone::Error, message.to_string()),
        };
        self.announce(tone, message);
    }

    /// Opens a new setup's page once the gateway lists it.
    pub(in crate::ui) fn provider_registered(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.provider_setup.editor.as_ref();
        let editor = editor.filter(|editor| editor.is_new);
        let id = editor.map(|editor| editor.record.selection.instance.clone()); // rules-allow: the page tab owns the id while the editor keeps its own
        let Some(id) = id else {
            return;
        };
        if self.provider_record(&id).is_some() {
            if let Some(editor) = &mut self.provider_setup.editor {
                editor.is_new = false;
            }
            self.open_page(PageTab::Provider(id), window, cx);
        }
    }

    fn provider_record(&self, id: &str) -> Option<&ProviderInstance> {
        self.client.state.gateway.as_ref().and_then(|gateway| {
            gateway
                .provider_instances
                .iter()
                .find(|provider| provider.selection.instance == id)
        })
    }

    /// Opens setup `id` with model row `expand` open and, optionally, one more local model;
    /// nothing is sent to the gateway.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn snapshot_model_rows(
        &mut self,
        id: &str,
        expand: Option<usize>,
        extra: Option<(&str, &str)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_page(PageTab::Provider(id.to_owned()), window, cx);
        self.render_provider(id, window, cx);
        if let Some((model, efforts)) = extra {
            self.push_provider_model(model, efforts, window, cx);
        }
        if let Some(editor) = &mut self.provider_setup.editor {
            editor.expand(expand);
        }
    }

    /// The setup's own models; a new setup of an editable provider starts from its catalog,
    /// or from its selected model when the provider has none.
    fn seeded_models(record: &ProviderInstance, definition: &ProviderStatus) -> Vec<SavedModel> {
        let saved = |model: &mobius_gateway::wire::ProviderModel| -> SavedModel {
            let efforts = model.reasoning.iter();
            let efforts = efforts.map(|effort| effort.id.clone()).collect(); // rules-allow: the editor owns its copy of the gateway's models
            let id = model.id.clone(); // rules-allow: the editor owns its copy of the gateway's models
            let default = model.default_reasoning.clone(); // rules-allow: the editor owns its copy of the gateway's models
            (id, efforts, default)
        };
        if record.selection.model.is_empty() {
            Vec::new()
        } else if !record.models.is_empty() {
            record.models.iter().map(saved).collect()
        } else if !definition.models.is_empty() {
            definition.models.iter().map(saved).collect()
        } else {
            vec![(record.selection.model.clone(), Vec::new(), None)] // rules-allow: the editor owns its copy of the selected model
        }
    }

    fn provider_definition(&self, service: &str) -> Option<&ProviderStatus> {
        self.client.state.gateway.as_ref().and_then(|gateway| {
            gateway
                .providers
                .iter()
                .find(|provider| provider.provider == service)
        })
    }

    fn provider_usage_error(&self, id: &str) -> Option<&str> {
        self.client
            .state
            .profile
            .as_ref()?
            .provider_usage
            .iter()
            .find(|usage| usage.provider == id)?
            .error
            .as_deref()
    }

    pub(in crate::ui) fn render_providers(&self, cx: &Context<Self>) -> AnyElement {
        let add = button("add-provider-page", "Add provider")
            .primary()
            .disabled(!self.client.link.is_ready())
            .on_click(cx.listener(|this, _, window, cx| this.add_provider_dialog(window, cx)));
        page(
            "providers",
            PageHeader::new("Providers").subtitle(
                "Model services this gateway can reach · One setup per account or endpoint",
            ),
        )
        .child(
            EmptyState::new("hi.plugsConnected", "No provider yet")
                .detail("Add a subscription or API provider so Bots have models to use.")
                .action(add),
        )
        .into_any_element()
    }

    /// An existing setup, its editor built from the live record on first sight.
    pub(in crate::ui) fn render_provider(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self
            .provider_setup
            .editor
            .as_ref()
            .is_some_and(|editor| !editor.is_new && editor.record.selection.instance == id);
        if !current {
            let record = self.provider_record(id).cloned(); // rules-allow: the editor edits its own snapshot of the setup
            let definition = record
                .as_ref()
                .and_then(|record| self.provider_definition(&record.selection.provider));
            let definition = definition.cloned(); // rules-allow: the editor keeps its own snapshot of the provider
            let (Some(record), Some(definition)) = (record, definition) else {
                return self.provider_unavailable();
            };
            let models = Self::seeded_models(&record, &definition);
            self.provider_setup.action = ProviderAction::Idle;
            self.provider_setup.editor = Some(ProviderDraft::new(
                record, definition, false, models, window, cx,
            ));
        }
        self.provider_editor(cx)
    }

    /// A setup that is not on the gateway yet.
    pub(in crate::ui) fn render_provider_draft(
        &mut self,
        service: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self
            .provider_setup
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_new && editor.definition.provider == service);
        if !current {
            let definition = self.provider_definition(service).cloned(); // rules-allow: the editor keeps its own snapshot of the provider
            let Some(definition) = definition else {
                return self.provider_unavailable();
            };
            let record = new_record(&definition);
            let models = Self::seeded_models(&record, &definition);
            self.provider_setup.action = ProviderAction::Idle;
            self.provider_setup.editor = Some(ProviderDraft::new(
                record, definition, true, models, window, cx,
            ));
        }
        self.provider_editor(cx)
    }

    /// The provider editor's actions, while it edits the setup `page` shows.
    pub(in crate::ui) fn provider_actions_for(
        &self,
        page: &PageTab,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let current = self
            .provider_setup
            .editor
            .as_ref()
            .is_some_and(|editor| match page {
                PageTab::Provider(id) => !editor.is_new && editor.record.selection.instance == *id,
                PageTab::ProviderDraft(service) => {
                    editor.is_new && editor.definition.provider == *service
                }
                _ => false,
            });
        if current {
            self.provider_page_actions(cx)
        } else {
            Vec::new()
        }
    }

    fn provider_unavailable(&self) -> AnyElement {
        let empty = if self.client.state.gateway.is_none() {
            EmptyState::new("hi.plugsConnected", "Loading provider").loading()
        } else {
            EmptyState::new("hi.plugsConnected", "Provider unavailable")
                .detail("It is no longer configured on this gateway.")
        };
        page("provider", PageHeader::new("Provider"))
            .child(empty)
            .into_any_element()
    }

    pub(in crate::ui) fn add_provider_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let definitions = self
            .client
            .state
            .gateway
            .as_ref()
            .map_or_else(Vec::new, |gateway| gateway.providers.clone()); // rules-allow: the 'static dialog builder keeps its own provider list
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let mut content = div()
                .id("provider-services")
                .max_h(height::DIALOG_LIST_MAX)
                .pt(space::S)
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(space::L);
            for (auth, title) in [
                (ProviderAuthKind::DeviceCode, "Subscription providers"),
                (ProviderAuthKind::ApiKey, "API providers"),
            ] {
                let mut group = Group::new();
                for definition in definitions.iter().filter(|d| d.auth == auth) {
                    let view = WeakEntity::clone(&view);
                    let service = definition.provider.clone(); // rules-allow: the 'static row listener owns the provider id
                    group = group.child(
                        Row::new(SharedString::from(service.as_str()), definition.label.as_str())
                        .leading(Leading::Glyph(
                            symbol_icon_or(Some(&definition.symbol), "hi.hardDrives").into(),
                        ))
                        .subtitle(definition.description.as_str())
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let page = PageTab::ProviderDraft(service.clone()); // rules-allow: each click opens a page tab that owns the id
                            if let Err(error) =
                                view.update(cx, |this, cx| this.open_page(page, window, cx))
                            {
                                tracing::debug!(%error, "the workspace closed before the provider draft opened");
                            }
                        }),
                    );
                }
                content = content.child(Section::new(title).child(group));
            }
            dialog
                .title(heading("Add provider"))
                .w(width::DIALOG)
                .child(content)
        });
    }

    /// Sends the credential first when a key was entered, then the registration.
    fn save_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.provider_setup.editor else {
            return;
        };
        let registration = match editor.registration(cx) {
            Ok(Some(registration)) => registration,
            Ok(None) => return,
            Err(error) => {
                self.report(error.to_string());
                cx.notify();
                return;
            }
        };
        let key = editor.api_key.read(cx).value().trim().to_owned();
        let ClientMessage::RegisterProvider {
            request_id, config, ..
        } = &registration
        else {
            return;
        };
        if key.is_empty() || editor.definition.auth == ProviderAuthKind::DeviceCode {
            self.provider_setup.requests.insert(Arc::clone(request_id));
            self.send(registration, cx);
            return;
        }
        let id = crate::gateway::request_id();
        let instance = config.instance.clone(); // rules-allow: the credential request and the pending registration each own it
        let provider = config.provider.clone(); // rules-allow: the credential request and the pending registration each own it
        let base_url = config
            .base_url
            .as_ref()
            .map(mobius::backend::model::provider::BaseUrl::share);
        let message = ClientMessage::SetProviderCredential {
            request_id: Arc::clone(&id),
            instance,
            provider,
            base_url,
            api_key: key,
            expires_at: None,
        };
        editor
            .api_key
            .update(cx, |input, cx| input.set_value("", window, cx));
        let setup = &mut self.provider_setup;
        setup.pending.insert(Arc::clone(&id), registration);
        setup.requests.insert(id);
        setup.action = ProviderAction::Sending;
        self.announce_provider_action();
        self.send(message, cx);
    }

    fn start_provider_login(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.provider_setup.editor else {
            return;
        };
        let registration = match editor.registration(cx) {
            Ok(Some(registration)) => registration,
            Ok(None) => return,
            Err(error) => {
                self.report(error.to_string());
                cx.notify();
                return;
            }
        };
        let id = crate::gateway::request_id();
        let provider = editor.definition.provider.clone(); // rules-allow: the editor keeps its definition
        let label = editor.definition.label.clone(); // rules-allow: the editor keeps its definition
        let setup = &mut self.provider_setup;
        setup.action = ProviderAction::StartingLogin(label);
        setup.pending.insert(Arc::clone(&id), registration);
        setup.requests.insert(Arc::clone(&id));
        self.announce_provider_action();
        self.send(
            ClientMessage::StartProviderLogin {
                request_id: id,
                provider,
            },
            cx,
        );
    }

    fn remove_provider_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.provider_setup.editor else {
            return;
        };
        let id = editor.record.selection.instance.clone(); // rules-allow: the 'static confirm callback owns the id
        self.confirm_destructive(
            format!("Remove {}?", editor.record.label),
            "This removes the provider setup from the gateway. This cannot be undone.",
            "Remove provider",
            window,
            cx,
            move |this, window, cx| {
                this.send(
                    ClientMessage::RemoveProvider {
                        request_id: crate::gateway::request_id(),
                        instance: id.clone(), // rules-allow: the confirm callback can run again and keeps its id
                    },
                    cx,
                );
                let next = this.client.state.gateway.as_ref().and_then(|gateway| {
                    gateway
                        .provider_instances
                        .iter()
                        .find(|record| record.selection.instance != id)
                        .map(|record| record.selection.instance.clone()) // rules-allow: the page tab owns the id
                });
                let page = next.map_or(PageTab::Providers, PageTab::Provider);
                this.open_page(page, window, cx);
            },
        );
    }
}

/// A fresh setup of `definition` with its default model and endpoint.
fn new_record(definition: &ProviderStatus) -> ProviderInstance {
    let model = definition.default_model();
    ProviderInstance {
        label: definition.label.clone(), // rules-allow: the new setup owns its copy of the provider defaults
        tint: ProviderTint::default(),
        configured: false,
        credential_hint: None,
        selection: ProviderConfig {
            instance: uuid::Uuid::new_v4().to_string(),
            provider: definition.provider.clone(), // rules-allow: the new setup owns its copy of the provider defaults
            model: model.map_or_else(String::new, |model| model.id.clone()), // rules-allow: the new setup owns its copy of the provider defaults
            base_url: Some(definition.default_base_url.clone()), // rules-allow: the new setup owns its copy of the provider defaults
            endpoint_auth: ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: model.and_then(|model| model.default_reasoning.clone()), // rules-allow: the new setup owns its copy of the provider defaults
            web_search: Default::default(),
            service_tier: None,
            tool_discovery: None,
        },
        models: Vec::new(),
        image_model_ids: Vec::new(),
        operator_managed: false,
    }
}

impl Workspace {
    /// The Providers list (WP5): Add provider, then each setup.
    /// Add provider, then each provider setup.
    pub(in crate::ui) fn provider_rows(&self, cx: &Context<Self>) -> Vec<SidebarRow> {
        let mut rows = vec![
            SidebarRow::new("add-provider", "Add provider")
                .leading(Leading::glyph("hi.plus"))
                .disabled(!self.client.link.is_ready())
                .on_click(cx.listener(|this, _, window, cx| this.add_provider_dialog(window, cx))),
        ];
        let Some(gateway) = &self.client.state.gateway else {
            return rows;
        };
        for record in &gateway.provider_instances {
            let id = &record.selection.instance;
            let definition = self.provider_definition(&record.selection.provider);
            let service =
                definition.map_or(record.selection.provider.as_str(), |d| d.label.as_str());
            let page = PageTab::Provider(id.clone()); // rules-allow: the page tab owns the id
            let mut row = self
                .page_row(
                    format!("provider:{id}"),
                    record.label.as_str(),
                    Leading::Gutter,
                    page,
                    cx,
                )
                .meta(service);
            if self.provider_usage_error(id).is_some() {
                row = row.status(icon("hi.warning", &palette(cx)).size(glyph::MARK));
            } else if !record.configured {
                row = row.status(icon("hi.key", &palette(cx)).size(glyph::MARK));
            }
            rows.push(row);
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderAction, ProviderSetup};
    use mobius_gateway::wire::{ClientMessage, ServerMessage};

    #[test]
    fn provider_registration_requires_its_own_credential_confirmation() {
        let mut setup = ProviderSetup::default();
        setup.pending.insert(
            "expected".into(),
            ClientMessage::ListSessions {
                request_id: "registration".into(),
            },
        );
        let confirmation = |id: &str| ServerMessage::ProviderCredentialSaved {
            request_id: id.into(),
            instance: "provider".into(),
            provider: "provider".into(),
        };
        assert!(setup.apply(&confirmation("unrelated")).is_none());
        assert!(setup.apply(&confirmation("expected")).is_some());
        assert!(setup.apply(&confirmation("expected")).is_none());
        assert_eq!(setup.action, ProviderAction::Saved(String::new()));
    }
}
