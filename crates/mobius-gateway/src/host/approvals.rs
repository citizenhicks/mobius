use super::*;

impl GatewayHost {
    pub(crate) async fn review_approval(
        &self,
        request_id: &str,
        decision: ReviewDecision,
    ) -> std::result::Result<(), Rejection> {
        let (execution_id, approval, checkpoints, bots) = {
            let state = self.state.lock().await;
            let catalog = state.activities.lock().await;
            let mut matches = catalog
                .approvals
                .iter()
                .filter(|(_, approval)| approval.request.id == request_id);
            let Some((execution_id, approval)) = matches.next() else {
                return Err(stale_approval());
            };
            if matches.next().is_some() {
                return Err(stale_approval());
            }
            (
                execution_id.clone(),
                approval.clone(),
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.bots),
            )
        };
        let checkpoint = checkpoints
            .load(&execution_id)
            .await
            .map_err(internal)?
            .ok_or_else(stale_approval)?;
        if !checkpoint
            .pending_approval
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id && !pending.decision_received)
        {
            return Err(stale_approval());
        }
        let host = if let Some(chat_id) = approval.chat_id {
            self.open_session(&chat_id).await?
        } else {
            if bots
                .routine_session_bot_id(&execution_id)
                .map_err(internal)?
                .as_deref()
                != Some(approval.bot_id.as_str())
            {
                return Err(stale_approval());
            }
            self.open_execution_with_cache(&execution_id, true).await?.0
        };
        host.submit(Submission {
            id: Uuid::new_v4().to_string(),
            op: Op::ExecApproval {
                id: request_id.into(),
                decision,
            },
        })
        .await
    }
}

pub(super) fn stale_approval() -> Rejection {
    Rejection {
        code: "stale_approval",
        message: "This approval is no longer waiting for a decision.".into(),
        fatal: false,
    }
}
