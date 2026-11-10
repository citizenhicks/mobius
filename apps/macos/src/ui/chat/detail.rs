//! One entry point for every chat pill and chat button: Chat info toggles the session card
//! pinned over the conversation; everything else opens directly as a side-pane tab.

use super::*;
use mobius::SessionId;
use mobius::protocol::FrontendWidgetContent;

/// What a pill or chat button opens.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui) enum ChatDetail {
    /// The session card of a chat: context use, model, Bot, workspace and run totals.
    Context(SessionId),
    /// Pending approvals and contributed questions for a chat.
    Attention(SessionId),
    /// A Bot's full settings.
    Bot(Arc<str>),
    /// A gateway widget's content in a chat, such as its tasks or subagents.
    Widget { session: SessionId, key: String },
}

impl Workspace {
    pub(in crate::ui) fn open_chat_detail(
        &mut self,
        detail: ChatDetail,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &detail {
            ChatDetail::Attention(_) => {
                self.open_session_tab(detail, "Attention", "hi.mousePointerClick", cx);
            }
            ChatDetail::Context(session) => self.toggle_session_card(session, cx),
            ChatDetail::Bot(id) => {
                let title = self
                    .target_bot(&ModelTarget::Bot(Arc::clone(id)))
                    .map_or_else(|| "Bot".to_owned(), |bot| bot.name.to_string());
                self.open_session_tab(detail, title, "bot.face", cx);
            }
            ChatDetail::Widget { key, .. } => {
                let (title, glyph) = self.footer_widget_items().remove(key).map_or_else(
                    || ("Details".to_owned(), "hi.info"),
                    |(widget, _)| {
                        (
                            widget_title(widget),
                            symbol_icon_or(widget.symbol.as_ref(), "hi.info"),
                        )
                    },
                );
                self.open_session_tab(detail, title, glyph, cx);
            }
        }
    }

    /// A detail tab's body: a Bot's settings or a widget's content.
    pub(in crate::ui) fn detail_tab_body(
        &mut self,
        detail: &ChatDetail,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match detail {
            ChatDetail::Attention(session)
                if self.client.state.selected_session_id.as_ref() == Some(session) =>
            {
                self.attention_body(cx)
            }
            ChatDetail::Bot(id) => {
                let harness = self
                    .client
                    .state
                    .bot(id)
                    .map_or(mobius_gateway::wire::HarnessKind::Mobius, |bot| bot.harness);
                self.agent_settings_body(
                    crate::ui::pages::bots::AgentScope::Bot(Arc::clone(id)),
                    AgentSection::for_harness(harness),
                    window,
                    cx,
                )
            }
            ChatDetail::Widget { session, key }
                if self.client.state.selected_session_id.as_ref() == Some(session) =>
            {
                self.contribution_parts(key, cx).map_or_else(
                    || EmptyState::new("hi.info", "Nothing to show yet").into_any_element(),
                    |(_, body)| body,
                )
            }
            _ => EmptyState::new("hi.info", "Open this chat to see its details").into_any_element(),
        }
    }

    /// A detail tab's toolbar actions: a Bot's status and Save, or a widget's list actions.
    pub(in crate::ui) fn detail_tab_actions(
        &self,
        detail: &ChatDetail,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        match detail {
            ChatDetail::Bot(id) => self
                .agent_status_actions(&crate::ui::pages::bots::AgentScope::Bot(Arc::clone(id)), cx),
            ChatDetail::Widget { session, key }
                if self.client.state.selected_session_id.as_ref() == Some(session) =>
            {
                self.contribution_actions(key, cx)
            }
            _ => Vec::new(),
        }
    }
}

/// A widget's content title, or its label.
pub(super) fn widget_title(widget: &mobius::protocol::FrontendWidget) -> String {
    match &widget.content {
        Some(FrontendWidgetContent::ActionList { title, .. }) => title.clone(), // rules-allow: callers keep the title in tabs and 'static listeners
        None => widget.text.clone(), // rules-allow: callers keep the title in tabs and 'static listeners
    }
}
