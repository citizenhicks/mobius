//! The waiting line (TranscriptWaitingNote.swift): during a running turn's quiet gaps, a
//! "thinking", or the turn's reasoning title once it has one.

use super::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Type, Typography, Workspace, composer, div, glyph, height,
    icon, motion, palette, px, space, width,
};
use mobius::protocol::ModelStepContentPhase;
use mobius_gateway::wire::TranscriptItemContent;
use std::time::Duration;

const APPEAR_AFTER: Duration = Duration::from_millis(600);
/// The transcript row id of the standalone waiting line.
pub(in crate::ui) const TAIL_ID: &str = "waiting";

/// A shown waiting line.
pub(super) struct Waiting;

/// The newest reasoning summary: its last non-empty line without heading or bold marks.
pub(in crate::ui) fn reasoning_title(text: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with("<!--") {
            return None;
        }
        let title = line.trim_start_matches('#').trim().replace("**", "");
        (!title.is_empty()).then(|| title.chars().take(160).collect())
    })
}

impl Workspace {
    /// A running turn with nothing arriving: no pending step, approval or picker.
    fn is_waiting(&self) -> bool {
        let Some(session) = &self.client.state.session else {
            return false;
        };
        !session.active_turn_ids.is_empty()
            && self.client.link.is_ready()
            && session.pending_approvals.is_empty()
            && self.client.state.picker.is_none()
            && !self
                .client
                .state
                .transcript
                .entries()
                .next_back()
                .is_some_and(crate::gateway::TranscriptEntry::is_pending)
    }

    /// Shows the line once a gap has lasted 600 ms; hides it as soon as the gap ends.
    pub(super) fn update_waiting(&mut self, cx: &Context<Self>) {
        if !self.is_waiting() {
            self.chat_ui.waiting = None;
            self.chat_ui.waiting_hold = None;
            return;
        }
        if self.chat_ui.waiting.is_some() || self.chat_ui.waiting_hold.is_some() {
            return;
        }
        self.chat_ui.waiting_hold = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(APPEAR_AFTER).await;
            if let Err(error) = this.update(cx, |this, cx| {
                this.chat_ui.waiting_hold = None;
                if this.is_waiting() {
                    this.chat_ui.waiting = Some(Waiting);
                    cx.notify();
                }
            }) {
                tracing::debug!(%error, "the workspace closed before the waiting line appeared");
            }
        }));
    }

    /// The current turn's reasoning title, if it has one.
    fn turn_reasoning_title(&self) -> Option<String> {
        let turn = self
            .client
            .state
            .session
            .as_ref()?
            .active_turn_ids
            .first()?;
        self.client
            .state
            .transcript
            .entries()
            .rev()
            .take_while(|entry| entry.turn_id.as_deref() == Some(turn.as_ref()) && !entry.terminal)
            .find_map(|entry| match &entry.content {
                TranscriptItemContent::Assistant {
                    phase: ModelStepContentPhase::Reasoning,
                    text,
                    ..
                } => reasoning_title(text),
                _ => None,
            })
    }

    /// The waiting line: the turn's reasoning title, or "thinking" until there is one.
    pub(in crate::ui) fn waiting_phrase(&self) -> Option<AnyElement> {
        self.chat_ui.waiting.as_ref()?;
        let text = self
            .turn_reasoning_title()
            .unwrap_or_else(|| "thinking".into());
        Some(
            div()
                .min_w_0()
                .truncate()
                .child(composer::status_text(text))
                .into_any_element(),
        )
    }

    /// A running group's current progress summary.
    pub(in crate::ui) fn running_summary(summary: String) -> AnyElement {
        composer::status_text(summary)
    }

    /// The waiting line as the transcript's last row, when no event group owns the phrase.
    /// It has the summary row's height, so the row that replaces it lands where it stood.
    pub(in crate::ui) fn waiting_tail(
        &self,
        scope: &str,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let phrase = self.waiting_phrase()?;
        let line = div()
            .id(SharedString::from(format!("{scope}:{TAIL_ID}")))
            .mx_auto()
            .w_full()
            .max_w(width::TRANSCRIPT)
            .flex_shrink_0()
            .h(height::REGULAR)
            .flex()
            .items_center()
            .gap(space::S)
            .typography(Type::Body)
            .text_color(self.palette.muted)
            .aria_label("thinking")
            .child(icon("hi.loader", &palette(cx)).size(glyph::INLINE))
            .child(phrase);
        Some(motion::rise_in(
            line,
            SharedString::from(format!("{scope}:{TAIL_ID}:reveal")),
            motion::CROSSFADE,
            px(8.),
            cx,
        ))
    }
}
