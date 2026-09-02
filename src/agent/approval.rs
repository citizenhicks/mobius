//! Tool authorization and paused-approval lifecycle.

use std::collections::BTreeSet;

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
use crate::backend::model::ToolCall;
use crate::backend::sandbox::SandboxApprovalRequest;
use crate::backend::sandbox::SandboxPermissions;
use crate::middleware::PermissionRequestContext;
use crate::middleware::tools::ToolResult;
use crate::protocol::ApprovalCall;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ExecApprovalRequestEvent;
use crate::protocol::ReviewDecision;

struct ApprovalResponse {
    submission_id: String,
    decision: ReviewDecision,
}

impl Runner {
    pub(super) async fn resolve_tool_approval(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        calls: Vec<ToolCall>,
        request: SandboxApprovalRequest,
        permissions: SandboxPermissions,
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
        let mut events = hook_events
            .into_iter()
            .map(|msg| Event {
                submission_id: Some(submission_id.into()),
                msg,
            })
            .collect::<Vec<_>>();
        if hook_decision.is_some() && !events.is_empty() {
            self.persist_with_events(std::mem::take(&mut events), None)
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
        match hook_decision {
            Some(decision) => {
                self.apply_approval(inbox, &pending, decision, permissions, submission_id)
                    .await
            }
            None => {
                self.state.pending_approval = Some(pending.clone());
                events.push(approval_event(&pending));
                self.persist_with_events(events, None).await?;
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

fn denied_results(calls: &[ToolCall], rejection: &str) -> Vec<ToolResult> {
    calls
        .iter()
        .map(|call| ToolResult::error(call, format!("tool denied: {rejection}")))
        .collect()
}
