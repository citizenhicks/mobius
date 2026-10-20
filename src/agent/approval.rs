//! Tool authorization and paused-approval lifecycle.

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
use crate::backend::sandbox::SandboxApprovalRequest;
use crate::backend::sandbox::SandboxPermissions;
use crate::middleware::PermissionRequestContext;
use crate::middleware::tools::ToolResult;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ReviewDecision;
use crate::protocol::ToolCall;

struct ApprovalResponse {
    submission_id: Arc<str>,
    decision: ReviewDecision,
}

impl Runner {
    pub(super) async fn resolve_tool_approval(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
        calls: Vec<ToolCall>,
        request: SandboxApprovalRequest,
        permissions: SandboxPermissions,
    ) -> Result<Option<ToolCompletion>> {
        let mut hook_events = Vec::new();
        let hook_decision = {
            let mut context = PermissionRequestContext {
                turn: self.turn_identity(turn_id)?,
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
                submission_id: Some(Arc::clone(submission_id)),
                msg,
            })
            .collect::<Vec<_>>();
        if hook_decision.is_some() && !events.is_empty() {
            self.persist_with_events(std::mem::take(&mut events), None)
                .await?;
        }
        let mut pending = PendingApproval {
            submission_id: Arc::clone(submission_id),
            turn_id: Arc::clone(turn_id),
            request_id: request.id,
            approval_call_ids: request.call_ids,
            authorized_call_ids: permissions.mutation_call_ids(),
            calls,
            reason: request.reason,
            sandbox_mode: permissions.sandbox_mode(),
            network_access: permissions.network_access(),
            decision_received: false,
        };
        if let Some(decision) = hook_decision {
            self.apply_approval(inbox, &pending, decision, permissions, submission_id)
                .await
        } else {
            // Persist recovery data while the approval wait mutates runner state using this snapshot.
            self.state.make_mut().pending_approval = Some(pending.clone()); // rules-allow: the checkpoint keeps the approval for recovery while it is resolved
            events.push(approval_event(&pending));
            self.persist_with_events(events, None).await?;
            self.resolve_pending(inbox, &mut pending, false).await
        }
    }

    pub(super) async fn resume_pending(
        &mut self,
        inbox: &mut SubmissionInbox,
        pending: &mut PendingApproval,
    ) -> Result<()> {
        let Some(results) = self.resolve_pending(inbox, pending, true).await? else {
            return Ok(());
        };
        self.complete_tool_step(&pending.submission_id, &pending.turn_id, results)
            .await?;
        self.continue_turn(inbox, &pending.submission_id, &pending.turn_id)
            .await
    }

    async fn resolve_pending(
        &mut self,
        inbox: &mut SubmissionInbox,
        pending: &mut PendingApproval,
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
        if let Some(current) = self.state.make_mut().pending_approval.as_mut()
            && current.request_id == pending.request_id
        {
            current.decision_received = true;
            self.save().await?;
        }
        let permissions = SandboxPermissions::restore(
            &self.runtime.session_id,
            pending.sandbox_mode,
            pending.network_access,
            std::mem::take(&mut pending.authorized_call_ids),
        );
        self.apply_approval(
            inbox,
            pending,
            decision,
            permissions,
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
        abort_submission_id: &Arc<str>,
    ) -> Result<Option<ToolCompletion>> {
        let permissions = self.config.sandbox.resolve_approval(
            &self.runtime.session_id,
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
                        None,
                        permissions,
                    )
                    .await?;
                self.ready_or_aborted(execution, &pending.turn_id).await
            }
            ReviewDecision::Denied { rejection } => {
                let approval_call_ids = pending
                    .approval_call_ids
                    .iter()
                    .map(AsRef::as_ref)
                    .collect::<BTreeSet<&str>>();
                let allowed_calls = pending
                    .calls
                    .iter()
                    .filter(|call| !approval_call_ids.contains(call.call_id.as_ref()))
                    .collect::<Vec<_>>();
                let denied_calls = pending
                    .calls
                    .iter()
                    .filter(|call| approval_call_ids.contains(call.call_id.as_ref()));
                let mut completion = if allowed_calls.is_empty() {
                    ToolCompletion::default()
                } else {
                    let execution = self
                        .execute_tools(
                            inbox,
                            &pending.submission_id,
                            &pending.turn_id,
                            &allowed_calls,
                            None,
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
                completion.results.extend(denied_results(
                    denied_calls,
                    &rejection,
                    self.config.sandbox.output_limit(),
                ));
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
        submission_id: Some(Arc::clone(&pending.submission_id)),
        msg: EventMsg::ExecApprovalRequest(pending.request_event()),
    }
}

fn denied_results<'a>(
    calls: impl IntoIterator<Item = &'a ToolCall>,
    rejection: &str,
    output_limit: usize,
) -> Vec<ToolResult> {
    calls
        .into_iter()
        .map(|call| ToolResult::error(call, format!("tool denied: {rejection}"), output_limit))
        .collect()
}
