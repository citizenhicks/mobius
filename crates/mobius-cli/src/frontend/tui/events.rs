use std::sync::Arc;

use super::view::bounded_terminal_text;
use super::{
    BlockKey, MAX_ENTRY_BYTES, PickerState, PreviewContent, PreviewState, TranscriptTone, TuiState,
    attachment_label,
};
use crate::frontend::terminal::terminal_text;
use mobius::protocol::{
    EventMsg, FrontendEvent, FrontendPreviewUpdate, FrontendTone, MessageAuthor, MessageEvent,
    ModelStepCompletedEvent, ModelStepContentPhase, ModelStepOutcome, RenderedBlock,
    TokenUsageInfo,
};
use mobius_gateway::wire::{
    RecordedEvent, RenderedPreview, TranscriptItem, TranscriptItemContent, TranscriptOp,
    TranscriptPage,
};

impl TuiState {
    #[cfg(test)]
    pub(super) fn handle_agent_event(&mut self, event: EventMsg, blocks: Vec<RenderedBlock>) {
        self.handle_event(event, blocks, true);
    }

    fn handle_event(&mut self, event: EventMsg, blocks: Vec<RenderedBlock>, live: bool) {
        let was_rendered = !blocks.is_empty();
        for block in blocks {
            self.apply_block(block);
        }
        match event {
            EventMsg::TurnStarted(turn) => {
                self.reasoning_title = None;
                if live {
                    self.start_turn(turn.turn_id);
                }
            }
            EventMsg::ModelStepCompleted(step) => {
                self.handle_model_step_completed(&step);
            }
            EventMsg::ExecApprovalRequest(request) => {
                if live {
                    self.begin_approval(request);
                }
            }
            EventMsg::ExecApprovalDecided(decided) if live => {
                self.remove_approval(&decided.id);
            }
            EventMsg::TokenCount(tokens) => {
                if let Some(info) = tokens.info {
                    self.usage = usage_status(&info, self.context_limit);
                }
            }
            EventMsg::ModelChanged(changed) => {
                self.model_route = changed.route;
                self.model.model = terminal_text(&changed.model).into();
                self.model.reasoning_effort = changed
                    .reasoning_effort
                    .map(|effort| terminal_text(&effort).into());
                self.usage.context_fill = None;
            }
            EventMsg::SessionResumeRequested(request) => {
                self.requested_resume = Some(request);
            }
            EventMsg::TurnComplete(turn) => {
                if live {
                    self.finish_turn(&turn.turn_id);
                }
            }
            EventMsg::TurnAborted(turn) => {
                if live {
                    self.finish_turn(&turn.turn_id);
                }
                if !was_rendered {
                    self.push(
                        format!("turn aborted: {}", turn.reason),
                        TranscriptTone::Warning,
                    );
                }
            }
            EventMsg::Error(error) => {
                if !was_rendered {
                    self.push(format!("error: {}", error.message), TranscriptTone::Error);
                }
            }
            EventMsg::Warning(warning) => {
                if !was_rendered {
                    self.push(
                        format!("warning: {}", warning.message),
                        TranscriptTone::Warning,
                    );
                }
            }
            EventMsg::SubmissionRejected(rejection) => {
                if !was_rendered {
                    self.push(
                        format!("warning: {}", rejection.message),
                        TranscriptTone::Warning,
                    );
                }
            }
            EventMsg::Frontend(update) => self.handle_frontend_event(update),
            // Web search actions, among others, present through the canonical block list.
            _ => {}
        }
    }

    fn handle_message(&mut self, message: MessageEvent, submission_id: Option<Arc<str>>) {
        let (mut text, tone, submission_id) = match message.author {
            MessageAuthor::User => {
                let text = if message.text.is_empty() {
                    "›".into()
                } else {
                    format!("› {}", message.text)
                };
                self.remember_composer_input(message.text);
                (text, TranscriptTone::User, submission_id)
            }
            MessageAuthor::Source {
                message_id,
                source:
                    mobius::protocol::MessageSource::Session {
                        session_id: source_id,
                    }
                    | mobius::protocol::MessageSource::Call { call_id: source_id },
                handle,
                ..
            } => (
                format!("@{handle}\n{}", message.text),
                TranscriptTone::Neutral,
                Some(format!("source:{}:{source_id}:{message_id}", source_id.len()).into()),
            ),
            MessageAuthor::Source {
                source: mobius::protocol::MessageSource::External { .. },
                ..
            } => return,
        };
        for attachment in &message.attachments {
            text.push_str(if text == "›" { " " } else { "\n  " });
            text.push_str(&attachment_label(attachment));
        }
        if let Some(id) = submission_id {
            self.upsert_text(
                BlockKey {
                    capability: "message".into(),
                    value: id.to_string(),
                },
                &text,
                tone,
            );
        } else {
            self.push(text, tone);
        }
    }

    fn upsert_text(&mut self, id: BlockKey, text: &str, tone: TranscriptTone) {
        if let Some(entry) = self
            .transcript
            .iter_mut()
            .find(|entry| entry.id.as_ref() == Some(&id))
        {
            entry.text = bounded_terminal_text(text, MAX_ENTRY_BYTES);
            entry.pending = false;
            entry.rendered = None;
            entry.stream = None;
        } else if !text.is_empty() {
            self.push(text, tone);
            if let Some(entry) = self.transcript.back_mut() {
                entry.id = Some(id);
            }
        }
    }

    fn handle_model_step_completed(&mut self, step: &ModelStepCompletedEvent) {
        match step.outcome {
            ModelStepOutcome::Completed { .. } => {}
            ModelStepOutcome::Failed
            | ModelStepOutcome::Interrupted
            | ModelStepOutcome::Retrying => {
                // Block ids are namespaced by model step, so a step that ends
                // without completing can never finish its pending blocks. The
                // backend closes live ones with its own end events; this sweep
                // only keeps replay after a crash from stranding them.
                let prefix = format!("{}/", step.model_step_id);
                for entry in &mut self.transcript {
                    if entry.pending
                        && entry
                            .id
                            .as_ref()
                            .is_some_and(|id| id.value.starts_with(&prefix))
                    {
                        entry.tone = TranscriptTone::Warning;
                        entry.pending = false;
                        entry.rendered = None;
                    }
                }
            }
        }
    }

    fn handle_frontend_event(&mut self, update: FrontendEvent) {
        if matches!(
            update,
            FrontendEvent::Widget { .. } | FrontendEvent::RemoveWidget { .. }
        ) && let Some(overlay) = self.capability_overlay.as_mut()
        {
            overlay.apply(update.clone()); // rules-allow: the open overlay and the widget list each keep the update
        }
        match update {
            FrontendEvent::Widget {
                capability,
                mut item,
            } => {
                let text = bounded_terminal_text(&item.text, MAX_ENTRY_BYTES);
                if text != item.text {
                    Arc::make_mut(&mut item).text = text;
                }
                if let Some((_, current)) = self
                    .widgets
                    .iter_mut()
                    .find(|(owner, widget)| *owner == capability && widget.id == item.id)
                {
                    *current = item;
                } else {
                    self.widgets.push((capability, item));
                }
            }
            FrontendEvent::RemoveWidget { capability, id } => {
                self.widgets
                    .retain(|(owner, widget)| *owner != capability || *widget.id != *id);
            }
            FrontendEvent::Picker { title, options } => {
                self.preview = None;
                self.capability_overlay = None;
                self.picker = Some(PickerState::new(
                    terminal_text(&title),
                    options.into_iter().map(Into::into).collect(),
                    0,
                ));
            }
            // The gateway has already projected renders into `blocks`, and preview
            // transcripts arrive as gateway-rendered records; nested previews are control
            // events, not transcript content. The terminal stays on its chat after a fork; the
            // fork notice names the new chat.
            FrontendEvent::Render { .. }
            | FrontendEvent::Preview { .. }
            | FrontendEvent::OpenSession { .. } => {}
        }
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a display percentage of token counts needs no precision beyond 2^52"
)]
fn percentage(part: i64, whole: i64) -> Option<f64> {
    if whole <= 0 {
        return None;
    }
    Some((100.0 * part.max(0) as f64 / whole as f64).clamp(0.0, 100.0))
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct UsageStatus {
    pub(super) context_fill: Option<f64>,
    pub(super) cache_hit: Option<f64>,
    context_used: i64,
    context_window: i64,
}

impl UsageStatus {
    pub(super) fn apply_context_limit(&mut self, context_limit: Option<i64>) {
        if self.context_window <= 0 {
            self.context_fill = None;
            return;
        }
        self.context_fill = percentage(
            self.context_used,
            context_limit.unwrap_or(self.context_window),
        );
    }

    pub(super) fn label(self) -> String {
        format!(
            "context {} · cache {}",
            self.context_fill
                .map_or_else(|| "—".into(), |value| format!("{value:.1}%")),
            self.cache_hit
                .map_or_else(|| "—".into(), |value| format!("{value:.1}%"))
        )
    }
}

/// Applies gateway transcript items and appends under their stable ids, then handles the
/// event's control state. The terminal draws items in arrival order without row grouping.
pub(super) fn handle_gateway_event(
    state: &mut TuiState,
    mut record: RecordedEvent,
    transcript: Vec<TranscriptOp>,
    live: bool,
) {
    let requested = record
        .event
        .submission_id
        .as_ref()
        .is_some_and(|id| state.preview_request_id.as_deref() == Some(id.as_ref()));
    if requested
        && (record.preview.is_some()
            || matches!(
                record.event.msg,
                EventMsg::Error(_) | EventMsg::Warning(_) | EventMsg::SubmissionRejected(_)
            ))
    {
        state.preview_request_id = None;
    }
    if let Some(preview) = record.take_preview() {
        apply_preview(state, preview, requested);
    } else {
        let mut blocks = Vec::new();
        for op in transcript {
            match op {
                TranscriptOp::Item { item } => {
                    let item = Arc::unwrap_or_clone(item);
                    match item.content {
                        TranscriptItemContent::Block {
                            capability, block, ..
                        } => {
                            state.remember_block(item.id, &capability, &block);
                            blocks.push(RenderedBlock {
                                capability: String::from(&*capability),
                                block,
                            });
                        }
                        TranscriptItemContent::Message { .. }
                        | TranscriptItemContent::Assistant { .. } => state.apply_item(item),
                        _ => {}
                    }
                }
                // Appends extend entries already drawn, so their order among this
                // event's new blocks does not matter.
                TranscriptOp::AppendBlock { id, delta } => state.apply_block_delta(&id, delta),
                TranscriptOp::AppendText { id, text } => {
                    if let Some(entry) = state
                        .transcript
                        .iter_mut()
                        .find(|entry| entry.id.as_ref().is_some_and(|key| key.value == *id))
                    {
                        entry.text.push_str(&terminal_text(&text));
                        super::view::truncate_display_text(&mut entry.text, MAX_ENTRY_BYTES);
                        // Appends only extend the text, so the stream cache reparses just its tail.
                        entry.stream.get_or_insert_with(Box::default);
                        entry.rendered = None;
                        if matches!(entry.tone, TranscriptTone::Reasoning) {
                            state.reasoning_title = super::latest_summary_line(&entry.text);
                        }
                    }
                }
                _ => {}
            }
        }
        match record.event.msg {
            EventMsg::Message(_)
            | EventMsg::MessageDelta(_)
            | EventMsg::AssistantContentDelta(_)
            | EventMsg::AssistantMessage(_)
            | EventMsg::ModelStepStarted(_) => {
                for block in blocks {
                    state.apply_block(block);
                }
            }
            event => state.handle_event(event, blocks, live),
        }
    }
}

fn apply_preview(state: &mut TuiState, preview: RenderedPreview, requested: bool) {
    let current = state.preview.as_ref().and_then(|preview| {
        let PreviewContent::Snapshot(snapshot) = &preview.content else {
            return None;
        };
        Some(snapshot)
    });
    if !requested && current.is_none_or(|snapshot| *snapshot.id != *preview.id) {
        return;
    }
    if preview.update == FrontendPreviewUpdate::Prepend
        && !current.is_some_and(|snapshot| {
            *snapshot.id == *preview.id && !snapshot.page_ids.contains(&preview.page_id)
        })
    {
        return;
    }

    let RenderedPreview {
        id,
        title,
        subtitle,
        page_id,
        update,
        transcript,
        next,
        ..
    } = preview;
    let mut replay = TuiState::default();
    replay.apply_page(transcript);

    match update {
        FrontendPreviewUpdate::Replace if !requested => {
            let Some(current) = state.preview.as_mut() else {
                return;
            };
            let PreviewContent::Snapshot(snapshot) = &mut current.content else {
                return;
            };
            // Stable item IDs replace live rows while keeping older loaded pages
            // and the reader's scroll position intact.
            let updated_ids = replay
                .transcript
                .iter()
                .filter_map(|entry| entry.id.as_ref())
                .map(|id| (&*id.capability, id.value.as_str()))
                .collect::<std::collections::BTreeSet<_>>();
            let retained = snapshot
                .transcript
                .iter()
                .take_while(|entry| {
                    entry.id.as_ref().is_none_or(|id| {
                        !updated_ids.contains(&(&*id.capability, id.value.as_str()))
                    })
                })
                .count();
            snapshot.transcript.truncate(retained);
            snapshot.transcript.append(&mut replay.transcript);
            if retained == 0 {
                snapshot.next = next;
            }
            current.title = super::bounded_title(&title);
            current.subtitle = super::bounded_title(&subtitle);
        }
        FrontendPreviewUpdate::Replace => {
            state.picker = None;
            state.capability_overlay = None;
            state.preview = Some(PreviewState::snapshot(
                id,
                &title,
                &subtitle,
                page_id,
                replay.transcript,
                next,
            ));
        }
        FrontendPreviewUpdate::Prepend => {
            let Some(current) = state.preview.as_mut() else {
                return;
            };
            let PreviewContent::Snapshot(snapshot) = &mut current.content else {
                return;
            };
            current.title = super::bounded_title(&title);
            current.subtitle = super::bounded_title(&subtitle);
            snapshot.prepend(page_id, replay.transcript, next);
        }
    }
}

/// Puts an older page, or a fresh open's whole transcript, above what is shown.
pub(super) fn handle_gateway_history(state: &mut TuiState, page: TranscriptPage) {
    let reasoning_title = state.reasoning_title.take();
    let mut current = std::mem::take(&mut state.transcript);
    let current_len = current.len();
    state.apply_page(page);
    while state.transcript.len().saturating_add(current_len) > super::MAX_TRANSCRIPT_ENTRIES {
        state.transcript.pop_front();
    }
    state.transcript.append(&mut current);
    state.reasoning_title = reasoning_title;
}

impl TuiState {
    /// Draws a gateway transcript page's items in order, ignoring its row grouping.
    fn apply_page(&mut self, page: TranscriptPage) {
        for item in page.items {
            self.apply_item(Arc::unwrap_or_clone(item));
        }
    }

    fn apply_item(&mut self, item: TranscriptItem) {
        match item.content {
            TranscriptItemContent::Message { message } => {
                self.handle_message(message, Some(item.id));
            }
            TranscriptItemContent::Assistant { phase, text, .. } => {
                if phase == ModelStepContentPhase::Reasoning {
                    self.reasoning_title = super::latest_summary_line(&terminal_text(&text));
                }
                self.upsert_text(
                    BlockKey {
                        capability: "assistant".into(),
                        value: item.id.to_string(),
                    },
                    &text,
                    narrative_tone(phase),
                );
            }
            TranscriptItemContent::Block {
                capability, block, ..
            } => {
                self.remember_block(item.id, &capability, &block);
                self.apply_block(RenderedBlock {
                    capability: String::from(&*capability),
                    block,
                });
            }
            TranscriptItemContent::Notice { tone, text } => match tone {
                FrontendTone::Error => self.push(format!("error: {text}"), TranscriptTone::Error),
                FrontendTone::Warning => {
                    self.push(format!("warning: {text}"), TranscriptTone::Warning);
                }
                FrontendTone::Neutral | FrontendTone::Success => self.push(text, tone.into()),
            },
            TranscriptItemContent::ForkPoint { parent } => {
                self.push(format!("forked from {parent}"), TranscriptTone::Neutral);
            }
        }
    }
}

fn narrative_tone(phase: ModelStepContentPhase) -> TranscriptTone {
    match phase {
        ModelStepContentPhase::Reasoning => TranscriptTone::Reasoning,
        ModelStepContentPhase::Commentary | ModelStepContentPhase::FinalAnswer => {
            TranscriptTone::Assistant
        }
    }
}

pub(super) fn usage_status(info: &TokenUsageInfo, context_limit: Option<i64>) -> UsageStatus {
    let input = info.last_token_usage.input_tokens.max(0);
    let used = info
        .last_token_usage
        .total_tokens
        .max(input.saturating_add(info.last_token_usage.output_tokens.max(0)));
    let window = info.model_context_window.unwrap_or_default().max(0);
    let mut status = UsageStatus {
        context_fill: None,
        cache_hit: percentage(info.last_token_usage.cached_input_tokens, input),
        context_used: used,
        context_window: window,
    };
    status.apply_context_limit(context_limit);
    status
}
