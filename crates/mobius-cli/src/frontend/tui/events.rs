use std::time::Instant;

use super::MAX_ENTRY_BYTES;
use super::PickerState;
use super::PreviewContent;
use super::PreviewState;
use super::TranscriptTone;
use super::TuiState;
use super::attachment_label;
use super::view::bounded_terminal_text;
use super::view::terminal_text;
use mobius::protocol::AgentMessagePhase;
use mobius::protocol::EventMsg;
use mobius::protocol::FrontendEvent;
use mobius::protocol::FrontendPreviewUpdate;
use mobius::protocol::ModelStepContentPhase;
use mobius::protocol::ModelStepOutcome;
use mobius::protocol::Op;
use mobius::protocol::RenderedBlock;
use mobius::protocol::TokenUsageInfo;
use mobius_gateway::wire::RecordedEvent;
use mobius_gateway::wire::RenderedEvent;
use mobius_gateway::wire::RenderedPreview;

impl TuiState {
    pub(super) fn handle_agent_event(&mut self, event: EventMsg, blocks: Vec<RenderedBlock>) {
        let is_commentary = matches!(
            &event,
            EventMsg::AgentMessageContentDelta(delta)
                if delta.phase == AgentMessagePhase::Commentary
        ) || matches!(
            &event,
            EventMsg::AgentMessage(message)
                if message.phase == AgentMessagePhase::Commentary
        );
        if !is_commentary {
            self.commit_commentary_stream();
        }
        if matches!(&event, EventMsg::ModelStepCompleted(_)) {
            self.commit_reasoning();
            self.commit_stream();
        }
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
            EventMsg::UserMessage(message) => {
                self.remember_composer_input(message.message.clone());
                let mut text = if message.message.is_empty() {
                    "›".to_string()
                } else {
                    format!("› {}", message.message)
                };
                for attachment in &message.attachments {
                    text.push_str(if text == "›" { " " } else { "\n  " });
                    text.push_str(&attachment_label(attachment));
                }
                self.push(text, TranscriptTone::User);
            }
            EventMsg::AgentMessageContentDelta(delta) => {
                self.remember_streamed_phase(
                    &delta.model_step_id,
                    match delta.phase {
                        AgentMessagePhase::Commentary => ModelStepContentPhase::Commentary,
                        AgentMessagePhase::FinalAnswer => ModelStepContentPhase::FinalAnswer,
                    },
                );
                self.append_stream(&delta.delta, delta.phase);
            }
            EventMsg::AgentReasoningContentDelta(delta) => {
                self.remember_streamed_phase(
                    &delta.model_step_id,
                    ModelStepContentPhase::Reasoning,
                );
                self.append_reasoning(&delta.delta);
            }
            EventMsg::ModelStepStarted(step) => {
                self.streamed_step_phases
                    .entry(step.model_step_id)
                    .or_default();
            }
            EventMsg::ModelStepCompleted(step) => {
                let streamed = self
                    .streamed_step_phases
                    .remove(&step.model_step_id)
                    .unwrap_or_default();
                match step.outcome {
                    ModelStepOutcome::Completed { content, .. } => {
                        for item in content {
                            if item.text.is_empty() || streamed.contains(item.phase) {
                                continue;
                            }
                            let tone = match item.phase {
                                ModelStepContentPhase::Reasoning => TranscriptTone::Reasoning,
                                ModelStepContentPhase::Commentary
                                | ModelStepContentPhase::FinalAnswer => TranscriptTone::Assistant,
                            };
                            self.push(item.text, tone);
                        }
                    }
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
                self.completed_model_steps.insert(step.model_step_id);
            }
            EventMsg::AgentMessage(message) => {
                if self.completed_model_steps.contains(&message.model_step_id) {
                    return;
                }
                if self.streaming_phase == Some(message.phase) {
                    self.streaming.clear();
                    self.streaming_phase = None;
                } else {
                    self.commit_stream();
                }
                if !was_rendered {
                    self.push(message.message, TranscriptTone::Assistant);
                }
            }
            EventMsg::ContextCompacted => {}
            EventMsg::ExecApprovalRequest(request) => {
                self.active_turn = Some(request.turn_id);
                self.turn_started_at.get_or_insert_with(Instant::now);
                self.begin_approval(request.id);
            }
            EventMsg::ExecApprovalReview(_) => {}
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
            EventMsg::Frontend(update) => match update {
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
                    let selected = options
                        .iter()
                        .position(|option| {
                            matches!(
                                &option.op,
                                Op::SetModel { route } if route == &self.model_route
                            )
                        })
                        .or_else(|| {
                            let group = self
                                .model_choices
                                .iter()
                                .find(|choice| choice.route == self.model_route)?
                                .group
                                .as_str();
                            options.iter().position(|option| {
                                let Op::SetModel { route } = &option.op else {
                                    return false;
                                };
                                self.model_choices
                                    .iter()
                                    .any(|choice| choice.route == *route && choice.group == group)
                            })
                        })
                        .unwrap_or_default();
                    self.picker = Some(PickerState {
                        title: terminal_text(&title),
                        options,
                        selected,
                    });
                }
                // Preview transcripts arrive as gateway-rendered records.
                // Nested previews are control events, not transcript content.
                FrontendEvent::Preview { .. } => {}
            },
            _ => {}
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
    if let Some(preview) = record.preview {
        apply_preview(state, preview);
    } else {
        state.handle_agent_event(record.event.msg, record.blocks);
    }
}

fn apply_preview(state: &mut TuiState, preview: RenderedPreview) {
    let current = state.preview.as_ref().and_then(|preview| {
        let PreviewContent::Snapshot(snapshot) = &preview.content else {
            return None;
        };
        Some(snapshot)
    });
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
    let mut replay = TuiState::default();
    for rendered in events {
        apply_rendered_event(&mut replay, rendered);
    }
    replay.commit_reasoning();
    replay.commit_stream();

    match update {
        FrontendPreviewUpdate::Replace => {
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
    state.handle_agent_event(rendered.event, rendered.blocks);
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
