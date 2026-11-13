//! Unsent bubbles and gateway-held messages at the end of the transcript.

use super::actions;
use super::attribution::message_meta;
use crate::ui::Workspace;
use crate::ui::primitives::{
    Shape as _, Spinner, Type, Typography as _, glyph, height, icon, palette, space,
    symbol_icon_or, width,
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, Styled as _, div,
};

use mobius::protocol::{FrontendSymbol, MessageAuthor};
use std::sync::Arc;
impl Workspace {
    /// Messages on their way to the gateway: dimmed bubbles with a spinner, until the
    /// gateway records them, holds them as a steering or queued widget, or rejects them back
    /// into the composer. Exactly one bubble stands for a message at any moment.
    pub(in crate::ui) fn pending_bubbles(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let p = self.palette;
        let session_id = self.client.state.selected_session_id.as_ref();
        self.pending_messages()
            .into_iter()
            .map(|(id, message)| {
                div()
                    .id(SharedString::from(format!("pending-message:{id}")))
                    .mx_auto()
                    .w_full()
                    .max_w(width::TRANSCRIPT)
                    .flex_shrink_0()
                    .pl(width::BUBBLE_INDENT)
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(space::S)
                    .opacity(0.6)
                    .children(self.file_row(&message.files, session_id, true, cx))
                    .when(!message.text.is_empty(), |row| {
                        row.child(
                            div()
                                .px(space::L)
                                .py(space::S)
                                .tile_corners()
                                .bg(p.accent_soft)
                                .whitespace_normal()
                                .child(message.text.clone()), // rules-allow: the bubble owns its text while the pending message keeps it
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::XS)
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(Spinner::new(glyph::INLINE).color(p.muted))
                            .child("sending · you"),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// The list keys of `transcript_tail`'s items, in the same order.
    pub(in crate::ui) fn transcript_tail_keys(&self) -> Vec<String> {
        self.client
            .state
            .session
            .as_ref()
            .map_or_else(Vec::new, |session| {
                session
                    .widgets
                    .iter()
                    .filter(|widget| {
                        widget.item.slot == mobius::protocol::FrontendSlot::TranscriptTail
                    })
                    .map(|widget| format!("tail:{}:{}", widget.capability, widget.item.id))
                    .collect()
            })
    }

    /// Capability items after the live transcript: queued messages as dashed bubbles,
    /// anything else as an event line.
    pub(in crate::ui) fn transcript_tail(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let p = self.palette;
        let Some(session) = &self.client.state.session else {
            return Vec::new();
        };
        session
            .widgets
            .iter()
            .filter(|widget| widget.item.slot == mobius::protocol::FrontendSlot::TranscriptTail)
            .map(|widget| {
                let item = &widget.item;
                let queued = matches!(
                    &item.action,
                    Some(mobius::protocol::Op::CapabilityCommand { input: Some(_), .. })
                );
                let row = if queued {
                    let meta = match item.symbol.as_ref() {
                        Some(FrontendSymbol::Steer) => {
                            message_meta(&p, "hi.arrowUpRight01", Some("steer"), 0)
                        }
                        _ => message_meta(&p, "hi.queue01", Some("queued"), 0),
                    };
                    div()
                        .pl(width::BUBBLE_INDENT)
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap(space::S)
                        .child(
                            div()
                                .px(space::L)
                                .py(space::S)
                                .tile_corners()
                                .bg(p.accent_soft.opacity(0.24))
                                .border_1()
                                .border_dashed()
                                .border_color(p.accent.opacity(0.42))
                                .whitespace_normal()
                                .child(item.text.clone()), // rules-allow: the bubble owns its text while the widget keeps it
                        )
                        .child(meta)
                } else {
                    div()
                        .min_h(height::COMPACT)
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .text_color(p.muted)
                        .child(
                            icon(
                                symbol_icon_or(item.symbol.as_ref(), "hi.queue01"),
                                &palette(cx),
                            )
                            .size(glyph::INLINE),
                        )
                        .child(item.text.clone()) // rules-allow: the element owns its text while the widget keeps it
                };
                let key = SharedString::from(format!("tail:{}:{}", widget.capability, item.id));
                let view = div()
                    .id(SharedString::clone(&key))
                    .mx_auto()
                    .w_full()
                    .max_w(width::TRANSCRIPT)
                    .flex_shrink_0()
                    .child(row);
                if !queued {
                    return view.into_any_element();
                }
                // A held message copies like a sent one (QueuedMessageView's context menu).
                let message = actions::Message {
                    key,
                    text: item.text.clone().into(), // rules-allow: the 'static message menu owns its text
                    target: None,
                    edit: Some(Arc::clone(item)),
                    author: Some(MessageAuthor::User),
                    recorded_at_ms: 0,
                    bot: None,
                    preview_author: None,
                };
                self.with_message_menu(view, message, true, cx)
            })
            .collect()
    }
}
