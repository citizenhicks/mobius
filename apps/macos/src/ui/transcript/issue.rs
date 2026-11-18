use crate::gateway::TranscriptEntry;
use crate::ui::Workspace;
use crate::ui::navigation::Section as Rail;
use crate::ui::pages::SUBSCRIPTIONS;
use crate::ui::primitives::icon_button;
use crate::ui::primitives::{Shape as _, Type, Typography as _, height, space};
use crate::ui::theme::mix;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
};

use mobius::FailureClass;
use mobius_gateway::wire::TranscriptItemContent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum ProviderIssue {
    OutOfCredit,
    LiveMinutesUsed,
    LiveUnavailable,
    RejectedKey,
    Declined,
    Busy,
    Unavailable,
    ContextTooLong,
}

/// The one thing the app offers to do about an issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum IssueAction {
    ManageSubscription,
    OpenProviders,
}

pub(in crate::ui) struct Explanation {
    pub(in crate::ui) text: String,
    pub(in crate::ui) action: Option<IssueAction>,
    pub(in crate::ui) cloud: bool,
}

impl ProviderIssue {
    pub(in crate::ui) const fn from_failure(failure: FailureClass, live: bool) -> Option<Self> {
        Some(match failure {
            FailureClass::OutOfCredits if live => Self::LiveMinutesUsed,
            FailureClass::Forbidden if live => Self::LiveUnavailable,
            FailureClass::OutOfCredits => Self::OutOfCredit,
            FailureClass::Authentication => Self::RejectedKey,
            FailureClass::Forbidden | FailureClass::ContentPolicy => Self::Declined,
            FailureClass::RateLimited => Self::Busy,
            FailureClass::Transient => Self::Unavailable,
            FailureClass::ContextLengthExceeded => Self::ContextTooLong,
            _ => return None,
        })
    }
}

/// An error notice's issue and the gateway's text it came from.
pub(super) fn provider_issue(entry: &TranscriptEntry) -> Option<(ProviderIssue, &str)> {
    let TranscriptItemContent::Notice {
        text,
        failure: Some(failure),
        ..
    } = &entry.content
    else {
        return None;
    };
    Some((ProviderIssue::from_failure(*failure, false)?, text))
}

/// The raw text for Details. Links into möbius Cloud's own provider account name its key and
/// lead to a dashboard only Cloud can use, so they are dropped.
fn details(message: &str, cloud: bool) -> String {
    if !cloud {
        return message.to_owned();
    }
    message
        .split(' ')
        .map(|word| {
            if word.contains("://openrouter.ai/") {
                "OpenRouter"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Workspace {
    /// The open chat's model and its provider instance, when the gateway has said which.
    fn chat_provider(&self) -> Option<&str> {
        let route = &self.client.state.session.as_ref()?.session.model.route;
        self.client
            .state
            .gateway
            .as_ref()?
            .agent_catalog
            .model_providers
            .get(route)
            .map(String::as_str)
    }

    /// The chat's model runs on the möbius Cloud allowance rather than a key the person added.
    fn chat_uses_cloud_allowance(&self) -> bool {
        match self.chat_provider() {
            Some(instance) => self.client.state.gateway.as_ref().is_some_and(|gateway| {
                gateway
                    .provider_instances
                    .iter()
                    .any(|setup| setup.selection.instance == instance && setup.operator_managed)
            }),
            None => self.on_cloud_gateway(),
        }
    }

    fn chat_provider_label(&self) -> String {
        let label = self
            .chat_provider()
            .and_then(|instance| self.provider_record(instance))
            .map(|provider| provider.label.as_str());
        label.unwrap_or("The model provider").to_owned()
    }

    pub(in crate::ui) fn explain(&self, issue: ProviderIssue) -> Explanation {
        let cloud = self.chat_uses_cloud_allowance()
            || matches!(
                issue,
                ProviderIssue::LiveMinutesUsed | ProviderIssue::LiveUnavailable
            );
        let status = self.cloud_status();
        let renews = status.allowance_renews_on.as_deref();
        let (text, action) = match issue {
            ProviderIssue::OutOfCredit if cloud => (
                match (
                    status.access_ends_on.as_deref(),
                    renews,
                    status.allowance_left,
                ) {
                    (Some(ends), _, _) => format!(
                        "Your möbius Cloud allowance is used up. Your access ends on {ends}."
                    ),
                    (None, Some(date), Some(true)) => format!(
                        "Your möbius Cloud allowance is nearly used up, so there isn’t enough \
                         left for this reply. It renews on {date}."
                    ),
                    (None, Some(date), _) => format!(
                        "You’ve used this billing period’s möbius Cloud allowance. It renews on \
                         {date}."
                    ),
                    (None, None, _) => {
                        "You’ve used this billing period’s möbius Cloud allowance.".to_owned()
                    }
                },
                Some(IssueAction::ManageSubscription),
            ),
            ProviderIssue::OutOfCredit => (
                format!(
                    "{} is out of credit. Add credit there, then try again.",
                    self.chat_provider_label()
                ),
                Some(IssueAction::OpenProviders),
            ),
            ProviderIssue::LiveMinutesUsed => (
                match renews {
                    Some(date) => {
                        format!("You’ve used up your Live minutes. They renew on {date}.")
                    }
                    None => "You’ve used up your Live minutes.".to_owned(),
                },
                Some(IssueAction::ManageSubscription),
            ),
            ProviderIssue::LiveUnavailable => (
                "Live voice needs an active möbius Cloud plan.".to_owned(),
                Some(IssueAction::ManageSubscription),
            ),
            ProviderIssue::RejectedKey if cloud => (
                "möbius Cloud is renewing its access to the model. Try again in a moment."
                    .to_owned(),
                None,
            ),
            ProviderIssue::RejectedKey => (
                format!(
                    "{} rejected its API key. Check it in Providers.",
                    self.chat_provider_label()
                ),
                Some(IssueAction::OpenProviders),
            ),
            ProviderIssue::Declined => (
                "The model’s provider declined this request.".to_owned(),
                None,
            ),
            ProviderIssue::Busy => (
                "The model is busy right now. Try again in a moment.".to_owned(),
                None,
            ),
            ProviderIssue::Unavailable => (
                "This model is unavailable right now. Try again, or choose another model."
                    .to_owned(),
                None,
            ),
            ProviderIssue::ContextTooLong => (
                "This chat is longer than the model can read. Start a new chat to continue."
                    .to_owned(),
                None,
            ),
        };
        Explanation {
            text,
            action,
            cloud,
        }
    }

    pub(in crate::ui) fn failure_text(
        &self,
        message: &str,
        failure: Option<FailureClass>,
    ) -> String {
        match failure.and_then(|failure| ProviderIssue::from_failure(failure, false)) {
            Some(issue) => self.explain(issue).text,
            None => message.to_owned(),
        }
    }

    pub(in crate::ui) fn perform_issue_action(
        &mut self,
        action: IssueAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            IssueAction::ManageSubscription => cx.open_url(SUBSCRIPTIONS),
            IssueAction::OpenProviders => self.select_section(Rail::Mobius, window, cx),
        }
    }

    /// A provider failure in plain words with the one action that fixes it; the gateway's own
    /// text stays behind Details (TranscriptNoticeView's ProviderIssueNotice).
    pub(super) fn provider_issue_row(
        &self,
        entry: &TranscriptEntry,
        issue: ProviderIssue,
        message: &str,
        scope: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let explanation = self.explain(issue);
        let toggle = format!("{}:issue", entry.id);
        let open = self.expanded.contains(&toggle);
        let scope = scope.to_owned();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(space::S)
            .px(space::L)
            .py(space::S)
            .tile_corners()
            .border_1()
            .border_color(p.danger.opacity(0.25))
            .bg(p.danger.opacity(0.06))
            .typography(Type::Body)
            .text_color(mix(p.danger, p.text, 0.15))
            .whitespace_normal()
            .child(explanation.text)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::S)
                    .child(
                        div()
                            .id(SharedString::from(format!("{}:details", entry.id)))
                            .cursor_pointer()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(if open { "Hide details" } else { "Details" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_disclosure(&toggle, &scope, cx);
                            })),
                    )
                    .children(explanation.action.map(|action| {
                        let label = match action {
                            IssueAction::ManageSubscription => "Manage subscription",
                            IssueAction::OpenProviders => "Open Providers",
                        };
                        icon_button(
                            SharedString::from(format!("{}:action", entry.id)),
                            label,
                            "hi.arrowUpRight01",
                            height::COMPACT,
                            Type::Caption.size(),
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.perform_issue_action(action, window, cx);
                            },
                        ))
                    })),
            )
            .when(open, |card| {
                card.child(
                    div()
                        .typography(Type::Caption)
                        .text_color(p.muted)
                        .child(details(message, explanation.cloud)),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::prelude::v1::test;

    #[test]
    fn cloud_links_leave_details_only_off_cloud() {
        let exhausted = "provider error: Responses HTTP 402 Payment Required: To increase, \
            visit https://openrouter.ai/workspaces/default/keys/e370b92d";
        assert!(!details(exhausted, true).contains("e370b92d"));
        assert!(details(exhausted, false).contains("e370b92d"));
    }
}
