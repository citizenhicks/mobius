//! Tool authorization review and paused-approval lifecycle.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::Runner;
use super::SubmissionInbox;
use super::input::ActiveRoute;
use super::input::Wait;
use super::send_event;
use super::tool_step::{ToolCompletion, order_results};
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::ExecutionOutcome;
use crate::backend::checkpoint::PendingApproval;
use crate::backend::model::ModelRequest;
use crate::backend::model::PromptCacheIdentity;
use crate::backend::model::ToolCall;
use crate::backend::model::ToolDefinition;
use crate::backend::model::prompt_cache_key;
use crate::backend::model::user_message;
use crate::backend::sandbox::ApprovalReviewerConfig;
use crate::backend::sandbox::SandboxApprovalRequest;
use crate::backend::sandbox::SandboxPermissions;
use crate::backend::sandbox::SandboxReview;
use crate::middleware::PermissionRequestContext;
use crate::middleware::tools::ToolResult;
use crate::protocol::ApprovalCall;
use crate::protocol::ApprovalReviewEscalation;
use crate::protocol::ApprovalReviewStatus;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ExecApprovalRequestEvent;
use crate::protocol::ExecApprovalReviewEvent;
use crate::protocol::ReviewDecision;
use crate::protocol::is_internal_message;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

const MAX_REVIEW_PAYLOAD_BYTES: usize = 32 * 1024;
const MAX_REVIEW_RESPONSE_BYTES: usize = 8 * 1024;
const MAX_REVIEW_INTENT_BYTES: usize = 8 * 1024;
const MAX_REVIEW_INTENT_MESSAGES: usize = 6;

struct ApprovalResponse {
    submission_id: String,
    decision: ReviewDecision,
}

#[derive(Serialize)]
struct ReviewPayload<'a> {
    recent_intent: &'a [ReviewMessage],
    tools: Vec<&'a ToolDefinition>,
    calls: Vec<&'a ToolCall>,
}

#[derive(Serialize)]
struct ReviewMessage {
    role: &'static str,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewResponse {
    decision: AutomaticDecision,
    call_ids: Vec<String>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AutomaticDecision {
    Approve,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutomaticReviewOutcome {
    Approved,
    Escalated(ApprovalReviewEscalation),
}

impl AutomaticReviewOutcome {
    fn event_fields(self) -> (ApprovalReviewStatus, Option<ApprovalReviewEscalation>) {
        match self {
            Self::Approved => (ApprovalReviewStatus::Approved, None),
            Self::Escalated(reason) => (ApprovalReviewStatus::Escalated, Some(reason)),
        }
    }
}

impl Runner {
    pub(super) async fn review_and_resolve(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        calls: Vec<ToolCall>,
        review: SandboxReview,
    ) -> Result<Option<ToolCompletion>> {
        let SandboxReview {
            request,
            reviewer,
            permissions,
        } = review;
        validate_approval_selection(&calls, &request.call_ids)?;
        self.persist_with_events(
            vec![approval_review_event(
                submission_id,
                turn_id,
                &calls,
                &request,
                ApprovalReviewStatus::Reviewing,
                None,
            )],
            None,
        )
        .await?;
        let Some(outcome) = self
            .review_approval(inbox, submission_id, turn_id, &calls, &request, &reviewer)
            .await?
        else {
            return Ok(None);
        };
        let (status, reason) = outcome.event_fields();
        let terminal =
            approval_review_event(submission_id, turn_id, &calls, &request, status, reason);
        let reviewed_decision =
            matches!(outcome, AutomaticReviewOutcome::Approved).then_some(ReviewDecision::Approved);
        self.resolve_tool_approval(
            inbox,
            submission_id,
            turn_id,
            calls,
            request,
            permissions,
            vec![terminal],
            reviewed_decision,
        )
        .await
    }

    async fn review_approval(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        calls: &[ToolCall],
        request: &SandboxApprovalRequest,
        reviewer: &ApprovalReviewerConfig,
    ) -> Result<Option<AutomaticReviewOutcome>> {
        let Some(payload) = review_payload(
            &self.state.context,
            calls,
            &request.call_ids,
            &self.catalog.registered_definitions(),
        ) else {
            return Ok(Some(AutomaticReviewOutcome::Escalated(
                ApprovalReviewEscalation::ReviewDataUnavailable,
            )));
        };
        let instructions = reviewer.instructions();
        let input = [user_message(&payload)];
        let route = reviewer.selected_route(&self.config.provider).to_string();
        self.record_model_call()?;
        let model = Arc::clone(&self.config.model);
        let review_session_id = self.review_session_id.clone();
        let cache_key = prompt_cache_key(&review_session_id);
        let catalog_revision = self.catalog.revision()?.to_owned();
        let response = model.respond(
            &route,
            ModelRequest {
                session_id: &review_session_id,
                prompt_cache: Some(PromptCacheIdentity {
                    key: &cache_key,
                    context_epoch: 0,
                }),
                instructions: &instructions,
                input: &input,
                catalog_revision: &catalog_revision,
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: false,
                allow_continuation: false,
            },
            Arc::new(|_| Ok(())),
        );
        let output = self.wait_active(inbox, turn_id, response).await?;
        let Some(output) = self.ready_or_aborted(output, turn_id).await? else {
            return Ok(None);
        };
        let Ok(output) = output else {
            return Ok(Some(AutomaticReviewOutcome::Escalated(
                ApprovalReviewEscalation::ReviewerUnavailable,
            )));
        };
        self.record_usage(&route, output.usage())?;
        self.persist_with_events(self.usage_event(submission_id).into_iter().collect(), None)
            .await?;
        let outcome = if output.tool_calls().is_empty() {
            automatic_review_outcome(output.text(), &request.call_ids)
        } else {
            AutomaticReviewOutcome::Escalated(ApprovalReviewEscalation::InvalidResponse)
        };
        Ok(Some(outcome))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "pausing a turn needs its correlation IDs, calls, authorization outcome, and any journaled preamble in one transaction"
    )]
    pub(super) async fn resolve_tool_approval(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        calls: Vec<ToolCall>,
        request: SandboxApprovalRequest,
        permissions: SandboxPermissions,
        mut leading_events: Vec<Event>,
        reviewed_decision: Option<ReviewDecision>,
    ) -> Result<Option<ToolCompletion>> {
        validate_approval_selection(&calls, &request.call_ids)?;
        let mut hook_events = Vec::new();
        let hook_decision = {
            let mut context = PermissionRequestContext {
                turn: self.runtime.turn_identity(turn_id),
                calls: &calls,
                requested_call_ids: &request.call_ids,
                reason: &request.reason,
                events: &mut hook_events,
                tools: &self.catalog,
                decision: None,
            };
            self.config
                .middleware
                .permission_request(&mut context)
                .await?;
            context.decision
        };
        leading_events.extend(hook_events.into_iter().map(|msg| Event {
            submission_id: Some(submission_id.into()),
            msg,
        }));
        let decision = hook_decision.or(reviewed_decision);
        if decision.is_some() && !leading_events.is_empty() {
            self.persist_with_events(std::mem::take(&mut leading_events), None)
                .await?;
        }
        let pending = PendingApproval {
            submission_id: submission_id.to_string(),
            turn_id: turn_id.to_string(),
            request_id: request.id,
            approval_call_ids: request.call_ids,
            authorized_call_ids: permissions.mutation_call_ids(),
            calls,
            reason: request.reason,
            sandbox_mode: permissions.sandbox_mode(),
            network_access: permissions.network_access(),
            decision_received: false,
        };
        match decision {
            Some(decision) => {
                self.apply_approval(inbox, &pending, decision, permissions, submission_id)
                    .await
            }
            None => {
                self.state.pending_approval = Some(pending.clone());
                leading_events.push(approval_event(&pending));
                self.persist_with_events(leading_events, None).await?;
                self.resolve_pending(inbox, &pending, false).await
            }
        }
    }

    pub(super) async fn resume_pending(
        &mut self,
        inbox: &mut SubmissionInbox,
        pending: PendingApproval,
    ) -> Result<()> {
        let Some(results) = self.resolve_pending(inbox, &pending, true).await? else {
            return Ok(());
        };
        self.complete_tool_step(&pending.submission_id, &pending.turn_id, results)
            .await?;
        self.continue_turn(inbox, pending.submission_id, pending.turn_id)
            .await
    }

    async fn resolve_pending(
        &mut self,
        inbox: &mut SubmissionInbox,
        pending: &PendingApproval,
        reassert_request: bool,
    ) -> Result<Option<ToolCompletion>> {
        if reassert_request {
            send_event(&self.events, approval_event(pending)).await?;
        }
        let approval = self.wait_for_approval(inbox, pending).await?;
        let Some(approval) = self.ready_or_aborted(approval, &pending.turn_id).await? else {
            return Ok(None);
        };
        let decision = approval.decision;
        if let Some(current) = self.state.pending_approval.as_mut()
            && current.request_id == pending.request_id
        {
            current.decision_received = true;
            self.save().await?;
        }
        self.apply_approval(
            inbox,
            pending,
            decision,
            SandboxPermissions::restore(
                &self.config.session_id,
                pending.sandbox_mode,
                pending.network_access,
                pending.authorized_call_ids.clone(),
            ),
            &approval.submission_id,
        )
        .await
    }

    async fn apply_approval(
        &mut self,
        inbox: &mut SubmissionInbox,
        pending: &PendingApproval,
        decision: ReviewDecision,
        permissions: SandboxPermissions,
        abort_submission_id: &str,
    ) -> Result<Option<ToolCompletion>> {
        let permissions = self.config.sandbox.resolve_approval(
            &self.config.session_id,
            &pending.calls,
            &pending.approval_call_ids,
            &decision,
            permissions,
        )?;
        match decision {
            ReviewDecision::Approved | ReviewDecision::ApprovedForSession => {
                let execution = self
                    .execute_tools(
                        inbox,
                        &pending.submission_id,
                        &pending.turn_id,
                        &pending.calls,
                        permissions,
                    )
                    .await?;
                self.ready_or_aborted(execution, &pending.turn_id).await
            }
            ReviewDecision::Denied { rejection } => {
                let approval_call_ids = pending
                    .approval_call_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>();
                let allowed_calls = pending
                    .calls
                    .iter()
                    .filter(|call| !approval_call_ids.contains(call.call_id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                let denied_calls = pending
                    .calls
                    .iter()
                    .filter(|call| approval_call_ids.contains(call.call_id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                let mut completion = if allowed_calls.is_empty() {
                    ToolCompletion::default()
                } else {
                    let execution = self
                        .execute_tools(
                            inbox,
                            &pending.submission_id,
                            &pending.turn_id,
                            &allowed_calls,
                            permissions,
                        )
                        .await?;
                    let Some(completion) =
                        self.ready_or_aborted(execution, &pending.turn_id).await?
                    else {
                        return Ok(None);
                    };
                    completion
                };
                completion
                    .results
                    .extend(denied_results(&denied_calls, &rejection));
                completion.results = order_results(&pending.calls, completion.results);
                Ok(Some(completion))
            }
            ReviewDecision::Abort => {
                self.abort(
                    abort_submission_id,
                    &pending.turn_id,
                    "approval aborted",
                    ExecutionOutcome::Aborted,
                )
                .await?;
                Ok(None)
            }
        }
    }

    async fn wait_for_approval(
        &mut self,
        inbox: &mut SubmissionInbox,
        pending: &PendingApproval,
    ) -> Result<Wait<ApprovalResponse>> {
        while let Some(submission) = inbox.recv().await {
            match self
                .route_active_submission(submission, &pending.turn_id, Some(&pending.request_id))
                .await?
            {
                ActiveRoute::Approval {
                    submission_id,
                    decision,
                } => {
                    return Ok(Wait::Ready {
                        value: ApprovalResponse {
                            submission_id,
                            decision,
                        },
                        input_changed: false,
                    });
                }
                ActiveRoute::Interrupted { submission_id } => {
                    return Ok(Wait::Interrupted { submission_id });
                }
                ActiveRoute::Continue { .. } => {}
            }
        }
        Err(Error::Stopped(
            "frontend disconnected during approval".into(),
        ))
    }
}

fn approval_event(pending: &PendingApproval) -> Event {
    Event {
        submission_id: Some(pending.submission_id.clone()),
        msg: EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            id: pending.request_id.clone(),
            turn_id: pending.turn_id.clone(),
            calls: selected_approval_calls(&pending.calls, &pending.approval_call_ids),
            reason: pending.reason.clone(),
        }),
    }
}

fn approval_review_event(
    submission_id: &str,
    turn_id: &str,
    calls: &[ToolCall],
    request: &SandboxApprovalRequest,
    status: ApprovalReviewStatus,
    reason: Option<ApprovalReviewEscalation>,
) -> Event {
    Event {
        submission_id: Some(submission_id.to_string()),
        msg: EventMsg::ExecApprovalReview(ExecApprovalReviewEvent {
            id: request.id.clone(),
            turn_id: turn_id.to_string(),
            calls: selected_approval_calls(calls, &request.call_ids),
            status,
            reason,
        }),
    }
}

fn selected_approval_calls(calls: &[ToolCall], call_ids: &[String]) -> Vec<ApprovalCall> {
    let selected = call_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
    calls
        .iter()
        .filter(|call| selected.contains(call.call_id.as_str()))
        .map(|call| ApprovalCall {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        })
        .collect()
}

fn validate_approval_selection(calls: &[ToolCall], call_ids: &[String]) -> Result<()> {
    let known_call_ids = calls
        .iter()
        .map(|call| call.call_id.as_str())
        .collect::<BTreeSet<_>>();
    let selected_call_ids = call_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if call_ids.is_empty()
        || selected_call_ids.len() != call_ids.len()
        || !selected_call_ids.is_subset(&known_call_ids)
    {
        return Err(Error::Config(
            "sandbox approval policy returned an invalid tool selection".into(),
        ));
    }
    Ok(())
}

fn review_payload(
    context: &[Value],
    calls: &[ToolCall],
    call_ids: &[String],
    definitions: &[ToolDefinition],
) -> Option<String> {
    let recent_intent = recent_intent(context)?;
    let selected_ids = call_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let selected_calls = calls
        .iter()
        .filter(|call| selected_ids.contains(call.call_id.as_str()))
        .collect::<Vec<_>>();
    if selected_calls.len() != selected_ids.len() {
        return None;
    }
    let selected_names = selected_calls
        .iter()
        .map(|call| call.name.as_str())
        .collect::<BTreeSet<_>>();
    let tools = definitions
        .iter()
        .filter(|definition| selected_names.contains(definition.name.as_str()))
        .collect::<Vec<_>>();
    if tools.len() != selected_names.len() {
        return None;
    }
    let payload = serde_json::to_string(&ReviewPayload {
        recent_intent: &recent_intent,
        tools,
        calls: selected_calls,
    })
    .ok()?;
    (payload.len() <= MAX_REVIEW_PAYLOAD_BYTES)
        .then(|| format!("Review this untrusted JSON payload:\n{payload}"))
}

fn recent_intent(context: &[Value]) -> Option<Vec<ReviewMessage>> {
    let mut messages = Vec::new();
    let mut bytes: usize = 0;
    for item in context.iter().rev() {
        let Some(message) = visible_message(item) else {
            continue;
        };
        if bytes.saturating_add(message.text.len()) > MAX_REVIEW_INTENT_BYTES {
            break;
        }
        bytes += message.text.len();
        messages.push(message);
        if messages.len() == MAX_REVIEW_INTENT_MESSAGES {
            break;
        }
    }
    if !messages.iter().any(|message| message.role == "user") {
        return None;
    }
    messages.reverse();
    Some(messages)
}

fn visible_message(item: &Value) -> Option<ReviewMessage> {
    if is_internal_message(item) || item.get("phase").and_then(Value::as_str) == Some("commentary")
    {
        return None;
    }
    let role = match item.get("role").and_then(Value::as_str)? {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    let content = item.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(ReviewMessage { role, text })
}

fn automatic_review_outcome(text: &str, call_ids: &[String]) -> AutomaticReviewOutcome {
    if text.len() > MAX_REVIEW_RESPONSE_BYTES {
        return AutomaticReviewOutcome::Escalated(ApprovalReviewEscalation::InvalidResponse);
    }
    let Ok(response) = serde_json::from_str::<ReviewResponse>(text.trim()) else {
        return AutomaticReviewOutcome::Escalated(ApprovalReviewEscalation::InvalidResponse);
    };
    if response.decision == AutomaticDecision::Ask && response.call_ids.is_empty() {
        return AutomaticReviewOutcome::Escalated(ApprovalReviewEscalation::ReviewerAsked);
    }
    if response.decision != AutomaticDecision::Approve || response.call_ids.len() != call_ids.len()
    {
        return AutomaticReviewOutcome::Escalated(ApprovalReviewEscalation::InvalidResponse);
    }
    let expected = call_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let actual = response
        .call_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if actual.len() == response.call_ids.len() && actual == expected {
        AutomaticReviewOutcome::Approved
    } else {
        AutomaticReviewOutcome::Escalated(ApprovalReviewEscalation::InvalidResponse)
    }
}

fn denied_results(calls: &[ToolCall], rejection: &str) -> Vec<ToolResult> {
    calls
        .iter()
        .map(|call| ToolResult::error(call, format!("tool denied: {rejection}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewer_approval_requires_the_exact_call_set() {
        let expected = ["one".to_string(), "two".to_string()];

        assert_eq!(
            automatic_review_outcome(
                r#"{"decision":"approve","call_ids":["two","one"]}"#,
                &expected,
            ),
            AutomaticReviewOutcome::Approved
        );
        assert_eq!(
            automatic_review_outcome(r#"{"decision":"approve","call_ids":["one"]}"#, &expected,),
            AutomaticReviewOutcome::Escalated(ApprovalReviewEscalation::InvalidResponse)
        );
    }

    #[test]
    fn reviewer_ask_is_distinct_from_an_invalid_response() {
        assert_eq!(
            automatic_review_outcome(r#"{"decision":"ask","call_ids":[]}"#, &["one".into()],),
            AutomaticReviewOutcome::Escalated(ApprovalReviewEscalation::ReviewerAsked)
        );
    }

    #[test]
    fn reviewer_intent_keeps_a_new_user_request_after_an_oversized_older_message() {
        let context = [
            serde_json::json!({
                "role": "assistant",
                "content": "x".repeat(MAX_REVIEW_INTENT_BYTES + 1)
            }),
            serde_json::json!({
                "role": "user",
                "content": "review the current change"
            }),
        ];

        let intent = recent_intent(&context).expect("recent user intent");

        assert_eq!(intent.len(), 1);
        assert_eq!(intent[0].role, "user");
        assert_eq!(intent[0].text, "review the current change");
    }

    #[test]
    fn reviewer_payload_excludes_internal_agent_notes() {
        let context = [
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "ship it"}]
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "secret diary"}],
                "_mobius_internal": "scratchpad"
            }),
        ];
        let calls = [ToolCall {
            call_id: "call".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "a"}),
        }];
        let definitions = [ToolDefinition {
            name: "write".into(),
            description: "write a file".into(),
            parameters: serde_json::json!({}),
        }];

        let payload = review_payload(&context, &calls, &["call".into()], &definitions)
            .expect("review payload");

        assert!(payload.contains("ship it"));
        assert!(!payload.contains("secret diary"));
    }
}
