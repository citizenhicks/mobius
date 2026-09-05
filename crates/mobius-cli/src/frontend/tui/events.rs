use std::time::Instant;

use super::BlockKey;
use super::MAX_ENTRY_BYTES;
use super::PickerState;
use super::PreviewContent;
use super::PreviewState;
use super::TranscriptTone;
use super::TuiState;
use super::attachment_label;
use super::view::bounded_terminal_text;
use crate::frontend::terminal::terminal_text;
use mobius::protocol::AssistantMessageEvent;
use mobius::protocol::EventMsg;
use mobius::protocol::FrontendEvent;
use mobius::protocol::FrontendPreviewUpdate;
use mobius::protocol::MessageAuthor;
use mobius::protocol::MessageEvent;
use mobius::protocol::ModelStepCompletedEvent;
use mobius::protocol::ModelStepContentPhase;
use mobius::protocol::ModelStepOutcome;
use mobius::protocol::RenderedBlock;
use mobius::protocol::TokenUsageInfo;
use mobius_gateway::wire::RecordedEvent;
use mobius_gateway::wire::RenderedEvent;
use mobius_gateway::wire::RenderedPreview;

impl TuiState {
    #[cfg(test)]
    pub(super) fn handle_agent_event(&mut self, event: EventMsg, blocks: Vec<RenderedBlock>) {
        self.handle_event(event, blocks, None);
    }

    fn handle_event(
        &mut self,
        event: EventMsg,
        blocks: Vec<RenderedBlock>,
        submission_id: Option<String>,
    ) {
        self.prepare_agent_event(&event);
        let was_rendered = !blocks.is_empty();
        for block in blocks {
            self.apply_block(block);
        }
        match event {
            EventMsg::TurnStarted(turn) => {
                self.commit_stream();
                self.active_turn = Some(turn.turn_id);
                self.turn_started_at = Some(Instant::now());
                self.clear_approval();
            }
            EventMsg::Message(message) => {
                self.handle_message(message, submission_id);
            }
            EventMsg::MessageDelta(message) => {
                if let Some(id) = submission_id {
                    let key = BlockKey {
                        capability: "message".into(),
                        value: id,
                    };
                    let text = if self
                        .transcript
                        .iter()
                        .any(|entry| entry.id.as_ref() == Some(&key))
                    {
                        message.text
                    } else {
                        format!("› {}", message.text)
                    };
                    self.upsert_text(key, &text, true, TranscriptTone::User);
                }
            }
            EventMsg::AssistantContentDelta(delta) => {
                self.remember_streamed_phase(&delta.model_step_id, delta.phase);
                if delta.phase == ModelStepContentPhase::Reasoning {
                    self.append_reasoning(&delta.delta);
                } else {
                    self.append_stream(&delta.delta, delta.phase);
                }
            }
            EventMsg::ModelStepStarted(step) => {
                self.streamed_step_phases
                    .entry(step.model_step_id)
                    .or_default();
            }
            EventMsg::ModelStepCompleted(step) => {
                self.handle_model_step_completed(step);
            }
            EventMsg::AssistantMessage(message) => {
                self.handle_assistant_message(message, was_rendered);
            }
            EventMsg::ContextCompacted => {}
            EventMsg::ExecApprovalRequest(request) => {
                self.active_turn = Some(request.turn_id);
                self.turn_started_at.get_or_insert_with(Instant::now);
                self.begin_approval(request.id);
            }
            EventMsg::TokenCount(tokens) => {
                if let Some(info) = tokens.info {
                    self.usage = usage_status(&info, self.context_limit);
                }
            }
            EventMsg::ModelChanged(changed) => {
                self.model_route = changed.route;
                self.model.model = terminal_text(&changed.model);
                self.model.reasoning_effort = changed
                    .reasoning_effort
                    .map(|effort| terminal_text(&effort));
                self.usage.context_fill = None;
            }
            EventMsg::SessionResumeRequested(request) => {
                self.requested_resume = Some(request);
            }
            // Presentation for these typed actions is part of the canonical block list.
            EventMsg::WebSearchBegin(_) | EventMsg::WebSearchEnd(_) => {}
            EventMsg::TurnComplete(_) => {
                self.finish_turn();
            }
            EventMsg::TurnAborted(turn) => {
                self.finish_turn();
                if !was_rendered {
                    self.push(
                        format!("turn aborted: {}", turn.reason),
                        TranscriptTone::Warning,
                    );
                }
            }
            EventMsg::Error(error) => {
                self.commit_stream();
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
            _ => {}
        }
    }

    fn prepare_agent_event(&mut self, event: &EventMsg) {
        let preserves_commentary_stream = match event {
            EventMsg::AssistantContentDelta(delta) => {
                delta.phase == ModelStepContentPhase::Commentary
            }
            EventMsg::AssistantMessage(message) => self.streaming_phase.is_some_and(|phase| {
                self.streamed_step_phases
                    .get(&message.model_step_id)
                    .is_some_and(|streamed| streamed.contains(phase))
            }),
            _ => false,
        };
        if !preserves_commentary_stream {
            self.commit_commentary_stream();
        }
        if matches!(
            event,
            EventMsg::ModelStepCompleted(step)
                if !matches!(step.outcome, ModelStepOutcome::Completed { .. })
        ) {
            self.commit_reasoning();
            self.commit_stream();
        }
    }

    fn handle_message(&mut self, message: MessageEvent, submission_id: Option<String>) {
        let mut text = match &message.author {
            MessageAuthor::User => {
                self.remember_composer_input(message.text.clone());
                if message.text.is_empty() {
                    "›".to_string()
                } else {
                    format!("› {}", message.text)
                }
            }
            MessageAuthor::Peer { handle, symbol, .. } => {
                let icon = symbol.as_ref().map_or("", |symbol| match symbol.as_str() {
                    "voice" => "♫ ",
                    _ => "◇ ",
                });
                format!("{icon}@{handle} › {}", message.text)
            }
        };
        for attachment in &message.attachments {
            text.push_str(if text == "›" { " " } else { "\n  " });
            text.push_str(&attachment_label(attachment));
        }
        if let Some(id) = submission_id {
            self.upsert_text(
                BlockKey {
                    capability: "message".into(),
                    value: id,
                },
                &text,
                false,
                TranscriptTone::User,
            );
        } else {
            self.push(text, TranscriptTone::User);
        }
    }

    fn upsert_text(&mut self, id: BlockKey, text: &str, partial: bool, tone: TranscriptTone) {
        if let Some(entry) = self
            .transcript
            .iter_mut()
            .find(|entry| entry.id.as_ref() == Some(&id))
        {
            if partial {
                if !entry.pending {
                    return;
                }
                entry.text.push_str(text);
            } else {
                entry.text = text.into();
            }
            entry.text = bounded_terminal_text(&entry.text, MAX_ENTRY_BYTES);
            entry.pending = partial;
            entry.rendered = None;
        } else if !text.is_empty() {
            self.push(text, tone);
            if let Some(entry) = self.transcript.back_mut() {
                entry.id = Some(id);
                entry.pending = partial;
            }
        }
    }

    fn handle_model_step_completed(&mut self, step: ModelStepCompletedEvent) {
        match step.outcome {
            ModelStepOutcome::Completed { .. } => {}
            ModelStepOutcome::Failed
            | ModelStepOutcome::Interrupted
            | ModelStepOutcome::Retrying => {
                self.streamed_step_phases.remove(&step.model_step_id);
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

    fn handle_assistant_message(&mut self, message: AssistantMessageEvent, was_rendered: bool) {
        let streamed = self
            .streamed_step_phases
            .remove(&message.model_step_id)
            .unwrap_or_default();
        let buffered_stream = self
            .streaming_phase
            .filter(|phase| streamed.contains(*phase));
        if buffered_stream.is_some() {
            self.streaming.clear();
            self.streaming_phase = None;
        }
        let buffered_reasoning =
            !self.reasoning.is_empty() && streamed.contains(ModelStepContentPhase::Reasoning);
        if buffered_reasoning {
            self.reasoning.clear();
        }
        if was_rendered {
            return;
        }
        for item in message.content {
            let replaces_buffer = buffered_stream == Some(item.phase)
                || (buffered_reasoning && item.phase == ModelStepContentPhase::Reasoning);
            if item.text.is_empty() || (streamed.contains(item.phase) && !replaces_buffer) {
                continue;
            }
            self.push(item.text, narrative_tone(item.phase));
        }
    }

    fn handle_frontend_event(&mut self, update: FrontendEvent) {
        if let Some(overlay) = self.capability_overlay.as_mut() {
            overlay.apply(update.clone());
        }
        match update {
            FrontendEvent::Widget {
                capability,
                mut item,
            } => {
                item.text = bounded_terminal_text(&item.text, MAX_ENTRY_BYTES);
                let key = (capability, item.id.clone());
                if let Some((_, current)) = self
                    .widgets
                    .iter_mut()
                    .find(|(candidate, _)| candidate == &key)
                {
                    *current = item;
                } else {
                    self.widgets.push((key, item));
                }
            }
            FrontendEvent::RemoveWidget { capability, id } => {
                self.widgets
                    .retain(|((owner, widget), _)| owner != &capability || widget != &id);
            }
            // The gateway has already projected this event into `blocks`.
            FrontendEvent::Render { .. } => {}
            FrontendEvent::Picker { title, options } => {
                self.preview = None;
                self.capability_overlay = None;
                self.picker = Some(PickerState {
                    title: terminal_text(&title),
                    options: options.into_iter().map(Into::into).collect(),
                    selected: 0,
                });
            }
            // Preview transcripts arrive as gateway-rendered records.
            // Nested previews are control events, not transcript content.
            FrontendEvent::Preview { .. } => {}
        }
    }
}

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

pub(super) fn handle_gateway_event(state: &mut TuiState, record: RecordedEvent) {
    let requested = record
        .event
        .submission_id
        .as_ref()
        .is_some_and(|id| state.preview_request_id.as_ref() == Some(id));
    if requested
        && (record.preview.is_some()
            || matches!(
                record.event.msg,
                EventMsg::Error(_) | EventMsg::Warning(_) | EventMsg::SubmissionRejected(_)
            ))
    {
        state.preview_request_id = None;
    }
    if let Some(preview) = record.preview {
        apply_preview(state, preview, requested);
    } else {
        state.handle_event(record.event.msg, record.blocks, record.event.submission_id);
    }
}

fn apply_preview(state: &mut TuiState, preview: RenderedPreview, requested: bool) {
    let current = state.preview.as_ref().and_then(|preview| {
        let PreviewContent::Snapshot(snapshot) = &preview.content else {
            return None;
        };
        Some(snapshot)
    });
    if !requested && !current.is_some_and(|snapshot| snapshot.id == preview.id) {
        return;
    }
    if preview.update == FrontendPreviewUpdate::Prepend
        && !current.is_some_and(|snapshot| {
            snapshot.id == preview.id && !snapshot.page_ids.contains(&preview.page_id)
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
        events,
        next,
    } = preview;
    let steps = events
        .iter()
        .filter_map(|rendered| match &rendered.event {
            EventMsg::AssistantContentDelta(delta) => Some(delta.model_step_id.clone()),
            EventMsg::AssistantMessage(message) => Some(message.model_step_id.clone()),
            EventMsg::ModelStepCompleted(step) => Some(step.model_step_id.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut replay = TuiState::default();
    for rendered in events {
        apply_rendered_event(&mut replay, rendered);
    }
    replay.commit_reasoning();
    replay.commit_stream();

    match update {
        FrontendPreviewUpdate::Replace if !requested => {
            let Some(current) = state.preview.as_mut() else {
                return;
            };
            let PreviewContent::Snapshot(snapshot) = &mut current.content else {
                return;
            };
            // Stable message IDs replace live rows while keeping older loaded pages
            // and the reader's scroll position intact.
            let retained = snapshot
                .transcript
                .iter()
                .take_while(|entry| {
                    entry.id.as_ref().is_none_or(|id| {
                        !replay
                            .transcript
                            .iter()
                            .any(|updated| updated.id.as_ref() == Some(id))
                            && !id
                                .value
                                .split_once('/')
                                .is_some_and(|(step, _)| steps.contains(step))
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
                title,
                subtitle,
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

pub(super) fn handle_gateway_history(state: &mut TuiState, records: Vec<RecordedEvent>) {
    for record in records {
        handle_gateway_event(state, record);
    }
    state.commit_reasoning();
    state.commit_stream();
}

fn apply_rendered_event(state: &mut TuiState, rendered: RenderedEvent) {
    match rendered.event {
        EventMsg::AssistantContentDelta(delta) => {
            for block in rendered.blocks {
                state.apply_block(block);
            }
            state.upsert_text(
                narrative_key(&delta.model_step_id, delta.phase),
                &delta.delta,
                true,
                narrative_tone(delta.phase),
            );
        }
        EventMsg::AssistantMessage(message) => {
            for phase in [
                ModelStepContentPhase::Reasoning,
                ModelStepContentPhase::Commentary,
                ModelStepContentPhase::FinalAnswer,
            ] {
                let text = message
                    .content
                    .iter()
                    .filter(|part| part.phase == phase)
                    .map(|part| part.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let key = narrative_key(&message.model_step_id, phase);
                if !rendered.blocks.is_empty() || text.is_empty() {
                    state
                        .transcript
                        .retain(|entry| entry.id.as_ref() != Some(&key));
                } else {
                    state.upsert_text(key, &text, false, narrative_tone(phase));
                }
            }
            for block in rendered.blocks {
                state.apply_block(block);
            }
        }
        event => state.handle_event(event, rendered.blocks, rendered.submission_id),
    }
}

fn narrative_key(model_step_id: &str, phase: ModelStepContentPhase) -> BlockKey {
    BlockKey {
        capability: "assistant".into(),
        value: format!("{model_step_id}/{phase:?}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use mobius::protocol::TokenUsage;

    #[test]
    fn context_fill_uses_compaction_target_when_enabled() {
        let usage = TokenUsage {
            input_tokens: 90_000,
            output_tokens: 10_000,
            total_tokens: 100_000,
            ..TokenUsage::default()
        };
        let info = TokenUsageInfo {
            total_token_usage: usage.clone(),
            last_token_usage: usage,
            model_context_window: Some(1_000_000),
        };

        assert_eq!(usage_status(&info, Some(250_000)).context_fill, Some(40.0));
        assert_eq!(usage_status(&info, None).context_fill, Some(10.0));

        let mut status = usage_status(&info, Some(250_000));
        status.apply_context_limit(Some(500_000));
        assert_eq!(status.context_fill, Some(20.0));

        let mut status = UsageStatus::default();
        status.apply_context_limit(Some(250_000));
        assert_eq!(status.context_fill, None);
    }
}
