//! The pills row above the composer (ComposerActivityView): the gateway's footer widgets,
//! code changes, subagents and the Session pill. Every pill carries a label.

use super::{
    AnyElement, App, ChatDetail, ClickEvent, Context, FrontendSlot, InteractiveElement,
    IntoElement, ParentElement, Shape, SharedString, StatefulInteractiveElement, Styled, Type,
    Typography, Window, Workspace, div, height, known_symbol_icon, pill, space,
};
use crate::numeric::ToFloat;
use mobius::SessionId;
use mobius::protocol::{FrontendListItemState, FrontendWidget, FrontendWidgetContent};
use std::collections::BTreeMap;
use std::rc::Rc;

/// A footer widget's label: counts such as "2/5" read with their title ("Tasks 2/5").
fn footer_label(widget: &FrontendWidget) -> String {
    match &widget.content {
        Some(FrontendWidgetContent::ActionList { title, .. })
            if !widget.text.chars().any(char::is_alphabetic) =>
        {
            format!("{title} {}", widget.text)
        }
        _ => widget.text.clone(), // rules-allow: the pill label owns its text while the widget stays in the gateway state
    }
}

impl Workspace {
    /// The pills row above the surface: draft choices, or the chat's widgets and status.
    /// Every chat pill opens directly beside the chat; the Session pill toggles its card.
    pub(super) fn composer_pills(&self, cx: &Context<Self>) -> AnyElement {
        if self.draft_selected() {
            return self.draft_pickers(cx);
        }
        let session = self.client.state.selected_session_id.as_ref();
        let (added, removed) = session
            .and_then(|session| self.chat_ui.changes.totals(session))
            .unwrap_or_default();
        let p = self.palette;
        let mut pills = self.footer_widgets(cx);
        if added + removed > 0 {
            pills.push(
                pill("chat-changes")
                    .button(cx)
                    .px(space::S)
                    .accessibility_label(format!("Code changes: +{added} −{removed}"))
                    .tooltip("Changed files")
                    .child(
                        div()
                            .flex()
                            .gap(space::S)
                            .typography(Type::Label)
                            .child(div().text_color(p.signal).child(format!("+{added}")))
                            .child(div().text_color(p.danger).child(format!("−{removed}"))),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_git_diff(mobius_gateway::wire::GitDiffScope::Unstaged, cx);
                    }))
                    .into_any_element(),
            );
        }
        if let Some(session) = session.filter(|_| self.attention_has_items()) {
            pills.push(
                pill("chat-attention")
                    .glyph("hi.mousePointerClick")
                    .label(self.attention_count().to_string())
                    .button(cx)
                    .on_click(Self::opener(ChatDetail::Attention(session.share()), cx))
                    .into_any_element(),
            );
        }
        pills.extend(self.context_pill(cx));
        // The row itself is clear, so the transcript runs on between the pills; each pill
        // keeps the page behind it, so what passes under it never shows through.
        div()
            .id("composer-pills")
            .min_h(height::REGULAR)
            .flex()
            .items_center()
            .justify_center()
            .gap(space::S)
            .overflow_x_scroll()
            .children(
                pills
                    .into_iter()
                    .map(|pill| div().flex_shrink_0().pill_corners().bg(p.sheet).child(pill)),
            )
            .into_any_element()
    }

    /// A click handler that opens `detail` (`open_chat_detail`).
    pub(super) fn opener(
        detail: ChatDetail,
        cx: &Context<Self>,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.open_chat_detail(detail.clone(), window, cx); // rules-allow: each click opens a tab that keeps its own ChatDetail
        })
    }

    /// The chat's composer-footer widgets by key.
    pub(super) fn footer_widget_items(&self) -> BTreeMap<String, (&FrontendWidget, bool)> {
        self.slot_widget_items(FrontendSlot::ComposerFooter)
    }

    /// The chat's widgets in `slot` by key, with whether each is gateway-wide: gateway-wide
    /// ones first, then the session's live widgets, then its static contributions.
    pub(in crate::ui) fn slot_widget_items(
        &self,
        slot: FrontendSlot,
    ) -> BTreeMap<String, (&FrontendWidget, bool)> {
        self.widget_items(Some(slot))
    }

    /// The footer widgets as labelled pills.
    fn footer_widgets(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        self.footer_widget_items()
            .into_iter()
            .map(|(key, (widget, _))| footer_pill(key.into(), widget, cx))
            .collect()
    }

    /// Opens or runs the footer widget `key`: a transcript beside the chat, its action or its
    /// content.
    fn activate_footer_widget(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((widget, global)) = self.footer_widget_items().remove(key) else {
            return;
        };
        let action = widget.action.clone(); // rules-allow: every activation sends its own Op from gateway state
        let content = widget.content.is_some();
        let title = SharedString::from(super::detail::widget_title(widget));
        let session = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share);
        match (action, content) {
            (Some(op), false) if !global => self.open_preview_command(title, op, cx),
            (action, content) => {
                if let Some(op) = action {
                    self.dispatch_widget_operation(op, global, cx);
                }
                if content && let Some(session) = session {
                    self.open_chat_detail(
                        ChatDetail::Widget {
                            session,
                            key: key.to_owned(),
                        },
                        window,
                        cx,
                    );
                }
            }
        }
    }

    /// The state of the subagent at `path`, as its preview reports it, in words.
    pub(in crate::ui) fn subagent_status(&self, path: &str) -> Option<&'static str> {
        Some(match self.client.state.previews.get(path)?.source.state? {
            FrontendListItemState::Pending => "waiting",
            FrontendListItemState::InProgress => "running",
            FrontendListItemState::Completed => "completed",
            FrontendListItemState::Failed => "failed",
            FrontendListItemState::Interrupted => "interrupted",
            FrontendListItemState::Plain => return None,
        })
    }
}

/// One primary footer widget as a pill that opens or runs it.
fn footer_pill(key: Rc<str>, widget: &FrontendWidget, cx: &Context<Workspace>) -> AnyElement {
    let mut item = pill(SharedString::from(format!("footer:{key}"))).label(footer_label(widget));
    if let Some(progress) = widget.progress {
        item = item.progress(progress.completed.to_f32() / progress.total.max(1).to_f32());
    }
    if let Some(glyph) = known_symbol_icon(widget.symbol.as_ref()) {
        item = item.glyph(glyph);
    }
    item.button(cx)
        .on_click(cx.listener(move |this, _, window, cx| {
            this.activate_footer_widget(&key, window, cx);
        }))
        .into_any_element()
}
