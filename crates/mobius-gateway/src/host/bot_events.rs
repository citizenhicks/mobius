//! Committed lifecycle projection and delivery through ordinary sessions.

use super::{
    Arc, BotStore, CheckpointStore, Error, EventMsg, EventPageRequest, ExecutionOutcome,
    ExecutionRecord, FrontendEvent, GatewayHost, GatewayState, JournalEvent, MessageAuthor,
    MessageSubmission, Op, Rejection, Result, RoutineRunStatus, ServerMessage, SessionRuntimeState,
    Submission, TurnLifecycle, Utc, gateway_session_summaries, internal, invalid_bot,
    invalid_config, invalid_routine, turn_lifecycle, unknown_session, validate_bot_workspace,
};
use crate::wire::{BotAction, BotSubscription, HookData, HookEvent, HookSelector, HookSource};
use mobius::SessionId;
use mobius::backend::checkpoint::ExecutionPageRequest;
use mobius::protocol::{ActiveMessageDelivery, MessageSource};

impl GatewayHost {
    pub(crate) async fn set_bot_subscription(
        &self,
        subscription: BotSubscription,
    ) -> std::result::Result<(), Rejection> {
        let source =
            crate::bots::subscription_source(&subscription.binding).map_err(invalid_config)?;
        let _mutation = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
        state.bots.bot(&subscription.bot_id).map_err(invalid_bot)?;
        let after = validate_source(&state, &subscription.bot_id, source).await?;
        validate_action(&state, &subscription.bot_id, &subscription.binding.action).await?;
        state
            .bots
            .set_subscription(&subscription, after, Utc::now().timestamp())
            .map_err(invalid_config)
    }

    pub(crate) async fn execute_session_command(
        &self,
        session_id: &SessionId,
        mut op: Op,
        bot_id: &str,
        cause: Option<&HookEvent>,
        command_id: &Arc<str>,
    ) -> std::result::Result<(), Rejection> {
        match &mut op {
            Op::Message { message } => {
                if !message.attachments.is_empty() || message.reply.is_some() {
                    return Err(invalid_config(
                        "session hook messages cannot attach files or target transcript replies",
                    ));
                }
                let sender = self
                    .state
                    .lock()
                    .await
                    .bots
                    .bot(bot_id)
                    .map_err(invalid_bot)?
                    .conversation_session_id;
                let handle = format!("chat #{sender}");
                message.author = source_author(
                    command_id,
                    cause,
                    MessageSource::Session { session_id: sender },
                    handle.into(),
                );
                message
                    .requested_delivery
                    .get_or_insert(ActiveMessageDelivery::Queue);
            }
            Op::Interrupt { .. } => {}
            _ => {
                return Err(invalid_config(
                    "session hooks support only message and interrupt operations",
                ));
            }
        }
        // Opening can await capacity. Recheck saved authorization at admission.
        let host = self.open_session(session_id).await?;
        let _access = self.begin_mutation().await?;
        {
            let state = self.state.lock().await;
            if !command_pending(&state.bots, cause, command_id)? {
                return Ok(());
            }
            require_session_target(&state, bot_id, session_id).await?;
            drop(state);
        }
        host.deliver_source(
            Submission {
                id: Arc::clone(command_id),
                op,
            },
            bot_id.into(),
        )
        .await
    }

    pub(crate) async fn emit_bot_hook(
        &self,
        bot_id: &str,
        name: &str,
        data: serde_json::Value,
        command_id: &str,
    ) -> std::result::Result<HookEvent, Rejection> {
        if name.trim().is_empty() || name.len() > 128 {
            return Err(invalid_config(
                "custom hook names must contain 1 to 128 bytes",
            ));
        }
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        state.bots.bot(bot_id).map_err(invalid_bot)?;
        let event = HookEvent {
            id: format!("custom-{command_id}").into(),
            bot_id: bot_id.into(),
            source: HookSource::Bot {
                bot_id: bot_id.into(),
            },
            cause_id: None,
            ancestry: Vec::new(),
            occurred_at: Utc::now().timestamp(),
            data: HookData::CustomReceived {
                name: name.into(),
                data,
            },
        };
        state.bots.record_hook(&event).map_err(invalid_config)?;
        drop(state);
        self.telemetry.notify.notify_one();
        Ok(event)
    }

    pub(crate) async fn bot_reporting_snapshot(
        &self,
        bot_id: &str,
    ) -> std::result::Result<serde_json::Value, Rejection> {
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        Ok(
            serde_json::json!({"subscriptions":state.bots.subscriptions(bot_id).map_err(invalid_bot)?}),
        )
    }

    pub(super) async fn recover_routine_outcomes(&self) -> Result<()> {
        let (bots, checkpoints) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.bots), Arc::clone(&state.checkpoints))
        };
        for run in bots
            .history(None)?
            .into_iter()
            .filter(|run| run.status == RoutineRunStatus::Running)
        {
            let Some(session_id) = &run.session_id else {
                continue;
            };
            let runtime = SessionRuntimeState::load(checkpoints.as_ref(), session_id).await?;
            if runtime
                .as_ref()
                .is_some_and(|runtime| runtime.undecided_approval.is_some())
            {
                self.open_session(session_id)
                    .await
                    .map_err(|error| Error::Config(error.message.to_string()))?;
                continue;
            }
            let outcome = if runtime.is_some() {
                checkpoints
                    .execution_page(
                        session_id,
                        ExecutionPageRequest {
                            before_sequence: None,
                            limit: 1,
                        },
                    )
                    .await?
                    .executions
                    .into_iter()
                    .next()
            } else {
                None
            };
            let (status, finished_at, message) = match outcome {
                Some(execution) => (
                    if execution.outcome == ExecutionOutcome::Completed {
                        RoutineRunStatus::Succeeded
                    } else if execution.outcome == ExecutionOutcome::Aborted
                        && bots.run_cancel_requested(&run.id)?
                    {
                        RoutineRunStatus::Cancelled
                    } else {
                        RoutineRunStatus::Failed
                    },
                    execution.finished_at_ms.div_euclid(1000),
                    (execution.outcome != ExecutionOutcome::Completed).then(|| {
                        format!(
                            "execution {}",
                            serde_json::to_string(&execution.outcome).unwrap_or_default()
                        )
                        .into()
                    }),
                ),
                None => (
                    RoutineRunStatus::Failed,
                    Utc::now().timestamp(),
                    Some("the gateway stopped before this run completed".into()),
                ),
            };
            bots.recover_run(&run.id, status, finished_at, message)?;
        }
        Ok(())
    }

    pub(crate) async fn dispatch_bot_events(&self) -> std::result::Result<(), Rejection> {
        // BotStorage is synchronous. Keep its SQLite waits off the runtime workers,
        // and retain admission guards inside the task if its caller is cancelled.
        let host = self.clone(); // rules-allow: the blocking task owns a gateway handle
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || runtime.block_on(host.dispatch_stored_bot_events()))
            .await
            .map_err(internal)?
    }

    async fn dispatch_stored_bot_events(&self) -> std::result::Result<(), Rejection> {
        let mutation = self.begin_mutation().await?;
        let (bots, checkpoints) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.bots), Arc::clone(&state.checkpoints))
        };
        for summary in gateway_session_summaries(&checkpoints)
            .await
            .map_err(internal)?
        {
            if bots.source_cursor(&summary.session_id).map_err(internal)?.0 >= summary.sequence {
                continue;
            }
            project_session_journal(
                &checkpoints,
                &bots,
                &summary.session_id,
                &summary.session_context.owner_id,
                None,
            )
            .await
            .map_err(internal)?;
        }

        for event in bots.unpublished_events(100).map_err(internal)? {
            let id = Arc::clone(&event.id);
            super::send_event(&self.events, ServerMessage::HookEvent { event });
            bots.event_published(&id).map_err(internal)?;
        }
        drop(mutation);
        for pending in bots
            .pending_actions(Utc::now().timestamp(), 32)
            .map_err(internal)?
        {
            let result = self.execute_hook_action(&pending).await;
            match result {
                Ok(()) => bots.action_accepted(&pending.id).map_err(internal)?,
                Err(error) => {
                    let retry = matches!(
                        error.code,
                        "session_limit"
                            | "agent_busy"
                            | "session_starting"
                            | "session_busy"
                            | "gateway_busy"
                            | "agent_stopped"
                            | "gateway_error"
                            | "server_busy"
                    );
                    bots.action_failed(
                        &pending.id,
                        &error.message,
                        retry.then(|| Utc::now().timestamp() + 15),
                    )
                    .map_err(internal)?;
                    if !retry {
                        super::send_warning(&self.events, "hook_action", error.message);
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) async fn execute_hook_action(
        &self,
        pending: &crate::bots::PendingHookAction,
    ) -> std::result::Result<(), Rejection> {
        let bots = Arc::clone(&self.state.lock().await.bots);
        if !bots.action_pending(&pending.id).map_err(internal)? {
            return Ok(());
        }
        match &pending.action {
            BotAction::Report { instruction } => {
                let session = bots
                    .bot(&pending.bot_id)
                    .map_err(invalid_bot)?
                    .conversation_session_id;
                let host = self.open_session(&session).await?;
                let _delivery = self.begin_mutation().await?;
                if !bots.action_pending(&pending.id).map_err(internal)? {
                    return Ok(());
                }
                let text =
                    crate::bots::report_text(&pending.event, instruction).map_err(internal)?;
                host.deliver_source(
                    Submission {
                        id: Arc::clone(&pending.id),
                        op: Op::Message {
                            message: MessageSubmission {
                                author: source_author(
                                    &pending.id,
                                    Some(&pending.event),
                                    MessageSource::External {
                                        source_id: "hook".into(),
                                        event_id: Arc::clone(&pending.event.id),
                                    },
                                    "source report".into(),
                                ),
                                text,
                                attachments: Vec::new(),
                                reply: None,
                                requested_delivery: Some(ActiveMessageDelivery::Queue),
                                target_turn_id: None,
                            },
                        },
                    },
                    Arc::clone(&pending.bot_id),
                )
                .await
            }
            BotAction::Routine { command } => {
                self.execute_routine_command(
                    command,
                    Some(&pending.bot_id),
                    Some(&pending.event),
                    &pending.id,
                )
                .await
            }
            BotAction::Session { session_id, op } => {
                self.execute_session_command(
                    session_id,
                    op.as_ref().clone(), // rules-allow: the pending action stays recorded until it is accepted
                    &pending.bot_id,
                    Some(&pending.event),
                    &pending.id,
                )
                .await
            }
        }
    }
}

pub(super) async fn validate_source(
    state: &GatewayState,
    bot_id: &str,
    source: &HookSource,
) -> std::result::Result<u64, Rejection> {
    match source {
        HookSource::Routine { routine_id } => {
            require_routine_owner_local(&state.bots, routine_id, bot_id)?;
            Ok(0)
        }
        HookSource::Session { session_id } => {
            if crate::bots::is_persistent(session_id, bot_id) {
                return Err(invalid_config(
                    "a Bot cannot subscribe to its main conversation",
                ));
            }
            let summary = state
                .checkpoints
                .session_summary(session_id)
                .await
                .map_err(internal)?
                .ok_or_else(unknown_session)?;
            if *summary.session_context.owner_id != *bot_id {
                return Err(unknown_session());
            }
            state
                .checkpoints
                .event_page(
                    session_id,
                    EventPageRequest {
                        before_sequence: None,
                        limit: 1,
                    },
                )
                .await
                .map_err(internal)
                .map(|page| page.latest_sequence)
        }
        HookSource::Bot { bot_id: source } => {
            state.bots.bot(source).map_err(invalid_bot)?;
            Ok(0)
        }
        HookSource::Client { client_id } => {
            let policy = state.config()?.auth;
            if !crate::auth::AuthStore::open_validated(state.store.auth_path(), policy)
                .map_err(internal)?
                .clients()
                .map_err(internal)?
                .iter()
                .any(|client| client.id == *client_id)
            {
                return Err(invalid_config("unknown paired client"));
            }
            Ok(0)
        }
        HookSource::Gateway => Ok(0),
        HookSource::Schedule { .. } => Err(invalid_config(
            "schedules must be expressed by a routine timer binding",
        )),
    }
}

async fn validate_action(
    state: &GatewayState,
    bot_id: &str,
    action: &BotAction,
) -> std::result::Result<(), Rejection> {
    match action {
        BotAction::Report { .. } => Ok(()),
        BotAction::Routine { command } => {
            require_routine_owner_local(&state.bots, &command.routine_id, bot_id)?;
            if let crate::wire::RoutineAction::Update { definition } = &command.action {
                validate_bot_workspace(state, bot_id, &definition.workspace)?;
                for binding in &definition.bindings {
                    if let HookSelector::Event { source, .. } = &binding.on {
                        validate_source(state, bot_id, source).await?;
                    }
                }
            }
            Ok(())
        }
        BotAction::Session { session_id, .. } => {
            require_session_target(state, bot_id, session_id).await
        }
    }
}
fn require_routine_owner_local(
    bots: &BotStore,
    routine_id: &str,
    bot_id: &str,
) -> std::result::Result<(), Rejection> {
    if *bots.routine(routine_id).map_err(invalid_routine)?.bot_id != *bot_id {
        return Err(unknown_session());
    }
    Ok(())
}
async fn require_session_target(
    state: &GatewayState,
    bot_id: &str,
    session_id: &SessionId,
) -> std::result::Result<(), Rejection> {
    let summary = state
        .checkpoints
        .session_summary(session_id)
        .await
        .map_err(internal)?
        .ok_or_else(unknown_session)?;
    if !crate::bots::is_listed_root(&summary, bot_id)
        || crate::bots::is_persistent(session_id, bot_id)
    {
        return Err(unknown_session());
    }
    Ok(())
}
fn source_author(
    command_id: &Arc<str>,
    cause: Option<&HookEvent>,
    source: MessageSource,
    handle: Arc<str>,
) -> MessageAuthor {
    MessageAuthor::Source {
        cause_id: cause.map(|event| Arc::clone(&event.id)),
        ancestry: cause.map_or_else(Vec::new, |event| {
            event
                .ancestry
                .iter()
                .chain(std::iter::once(&event.id))
                .map(Arc::clone)
                .collect()
        }),
        message_id: Arc::clone(command_id),
        source,
        handle,
        symbol: None,
    }
}

async fn lifecycle_event(
    checkpoints: &Arc<dyn CheckpointStore>,
    bots: &BotStore,
    bot_id: &str,
    session_id: &SessionId,
    record: &JournalEvent,
) -> Result<Option<HookEvent>> {
    if matches!(&record.event.msg, EventMsg::TurnStarted(turn) if turn.idle)
        || matches!(&record.event.msg, EventMsg::TurnComplete(turn) if turn.idle)
        || matches!(&record.event.msg, EventMsg::TurnAborted(turn) if turn.idle)
    {
        return Ok(None);
    }
    if matches!(record.event.msg, EventMsg::SessionConfigured(_)) && record.sequence == 1 {
        let data = HookData::SessionCreated {
            session_id: session_id.share(),
        };
        return session_hook(checkpoints, bots, bot_id, session_id, record, data, None)
            .await
            .map(Some);
    }
    let Some(lifecycle) = turn_lifecycle(&record.event.msg) else {
        return Ok(None);
    };
    let (data, turn_id) = match lifecycle {
        TurnLifecycle::Started { turn_id } => (
            HookData::SessionTurnStarted {
                session_id: session_id.share(),
                turn_id: Arc::clone(turn_id),
            },
            turn_id,
        ),
        TurnLifecycle::ApprovalRequested {
            turn_id,
            request_id,
        } => (
            HookData::SessionApproval {
                session_id: session_id.share(),
                turn_id: Arc::clone(turn_id),
                request_id: Arc::clone(request_id),
            },
            turn_id,
        ),
        TurnLifecycle::Finished { turn_id, .. } => {
            let execution = execution_for_turn(checkpoints, session_id, turn_id)
                .await?
                .ok_or_else(|| Error::Config("terminal event has no execution record".into()))?;
            (
                HookData::SessionTurnFinished {
                    session_id: session_id.share(),
                    turn_id: Arc::clone(turn_id),
                    outcome: execution.outcome,
                },
                turn_id,
            )
        }
    };
    session_hook(
        checkpoints,
        bots,
        bot_id,
        session_id,
        record,
        data,
        Some(&**turn_id),
    )
    .await
    .map(Some)
}

async fn session_hook(
    checkpoints: &Arc<dyn CheckpointStore>,
    bots: &BotStore,
    bot_id: &str,
    session_id: &SessionId,
    record: &JournalEvent,
    data: HookData,
    turn_id: Option<&str>,
) -> Result<HookEvent> {
    let author = if let Some(turn_id) = turn_id {
        match execution_for_turn(checkpoints, session_id, turn_id).await? {
            Some(execution) => Some(execution.author),
            None => SessionRuntimeState::load(checkpoints.as_ref(), session_id)
                .await?
                .and_then(|runtime| runtime.active_turn)
                .filter(|active| *active.turn_id == *turn_id)
                .map(|active| active.author),
        }
    } else {
        None
    };
    let (mut cause_id, mut ancestry) = author.map_or((None, Vec::new()), |author| {
        session_causality(Arc::unwrap_or_clone(author))
    });
    // Routine prompts are user-authored, but their invocation retains the schedule/hook cause.
    if cause_id.is_none()
        && let Some(run) = bots
            .history(None)?
            .into_iter()
            .find(|run| run.session_id.as_ref() == Some(session_id))
        && let Some(cause) = bots.hook_event(&format!("run-{}-running", run.id))?
    {
        cause_id = Some(Arc::clone(&cause.id));
        ancestry = cause.ancestry;
        if ancestry.len() < crate::bots::MAX_HOOK_ANCESTRY {
            ancestry.push(cause.id);
        }
    }
    Ok(HookEvent {
        id: format!("session-{session_id}-{}", record.sequence).into(),
        bot_id: bot_id.into(),
        source: HookSource::Session {
            session_id: session_id.share(),
        },
        data,
        occurred_at: record.recorded_at_ms.div_euclid(1000),
        cause_id,
        ancestry,
    })
}

async fn project_attention(
    checkpoints: &Arc<dyn CheckpointStore>,
    bots: &BotStore,
    bot_id: &str,
    session_id: &SessionId,
    record: &JournalEvent,
) -> Result<()> {
    use sha2::{Digest as _, Sha256};
    let EventMsg::Frontend(FrontendEvent::Widget {
        capability,
        item: widget,
    }) = &record.event.msg
    else {
        return Ok(());
    };
    for item in widget.pending_attention_items() {
        // Stable item identity makes repeated widget snapshots and crash replay idempotent.
        let key = serde_json::to_vec(&(session_id, capability, &widget.id, &item.id))?;
        let id = format!("attention-{:x}", Sha256::digest(key));
        if bots.hook_event(&id)?.is_some() {
            continue;
        }
        let mut event = session_hook(
            checkpoints,
            bots,
            bot_id,
            session_id,
            record,
            HookData::SessionAttention {
                session_id: session_id.share(),
                capability: Arc::clone(capability),
                item_id: Arc::clone(&item.id),
                text: item.text.chars().take(200).collect(),
            },
            None,
        )
        .await?;
        event.id = id.into();
        bots.record_hook(&event)?;
    }
    Ok(())
}

fn session_causality(author: MessageAuthor) -> (Option<Arc<str>>, Vec<Arc<str>>) {
    // At the boundary, preserve the terminal fact without authorizing another action.
    match author {
        MessageAuthor::Source {
            message_id,
            ancestry,
            ..
        } if ancestry.len() == crate::bots::MAX_HOOK_ANCESTRY => (Some(message_id), ancestry),
        author => author.causal_origin(),
    }
}

async fn project_session_journal(
    checkpoints: &Arc<dyn CheckpointStore>,
    bots: &BotStore,
    session_id: &SessionId,
    fallback_owner: &str,
    through: Option<u64>,
) -> Result<()> {
    let (mut cursor, mut owner) = bots.source_cursor(session_id)?;
    loop {
        if through.is_some_and(|sequence| cursor >= sequence) {
            return Ok(());
        }
        let previous_cursor = cursor;
        let mut persisted_cursor = cursor;
        for mut record in checkpoints.events_after(session_id, cursor, 100).await? {
            if through.is_some_and(|sequence| record.sequence > sequence) {
                break;
            }
            cursor = record.sequence;
            if let EventMsg::SessionConfigured(configured) = &mut record.event.msg {
                let next = Arc::clone(&configured.context.owner_id);
                if let Some(previous) = &owner
                    && previous != &next
                {
                    close_owner_change(bots, session_id, previous, &next, &record)?;
                    owner = Some(next);
                    persisted_cursor = cursor;
                    continue;
                }
                owner = Some(next);
            }
            let bot_id = owner.as_deref().unwrap_or(fallback_owner);
            if bots.bot(bot_id).is_ok() {
                project_attention(checkpoints, bots, bot_id, session_id, &record).await?;
                if let Some(event) =
                    lifecycle_event(checkpoints, bots, bot_id, session_id, &record).await?
                {
                    bots.project_session(&event, record.sequence)?;
                    persisted_cursor = cursor;
                }
            }
        }
        if cursor > persisted_cursor {
            bots.advance_source_cursor(
                session_id,
                owner.as_deref().unwrap_or(fallback_owner),
                cursor,
            )?;
        }
        if through.is_none() {
            return Ok(());
        }
        if cursor == previous_cursor {
            return Err(Error::Config(
                "session journal ended before the owner change".into(),
            ));
        }
    }
}

fn close_owner_change(
    bots: &BotStore,
    session_id: &SessionId,
    previous: &Arc<str>,
    next: &str,
    record: &JournalEvent,
) -> Result<()> {
    bots.close_session_sources(&[HookEvent {
        id: format!("session-{session_id}-{}", record.sequence).into(),
        bot_id: Arc::clone(previous),
        source: HookSource::Session {
            session_id: session_id.share(),
        },
        data: HookData::SessionOwnerChanged {
            session_id: session_id.share(),
            previous_bot_id: Arc::clone(previous),
        },
        occurred_at: record.recorded_at_ms.div_euclid(1000),
        cause_id: None,
        ancestry: Vec::new(),
    }])?;
    bots.advance_source_cursor(session_id, next, record.sequence)
}

pub(super) async fn close_reassigned_source(
    checkpoints: &Arc<dyn CheckpointStore>,
    bots: &BotStore,
    session_id: &SessionId,
    previous: &str,
) -> Result<()> {
    let page = checkpoints
        .event_page(
            session_id,
            EventPageRequest {
                before_sequence: None,
                limit: 100,
            },
        )
        .await?;
    let record = page
        .events
        .iter()
        .find(|record| matches!(record.event.msg, EventMsg::SessionConfigured(_)))
        .ok_or_else(|| Error::Config("reassigned session has no configuration event".into()))?;
    // Preserve committed facts before revoking the previous source's actions.
    project_session_journal(
        checkpoints,
        bots,
        session_id,
        previous,
        Some(record.sequence),
    )
    .await
}

async fn execution_for_turn(
    checkpoints: &Arc<dyn CheckpointStore>,
    session_id: &SessionId,
    turn_id: &str,
) -> Result<Option<ExecutionRecord>> {
    let mut before_sequence = None;
    loop {
        let page = checkpoints
            .execution_page(
                session_id,
                ExecutionPageRequest {
                    before_sequence,
                    limit: 100,
                },
            )
            .await?;
        if let Some(execution) = page
            .executions
            .into_iter()
            .find(|execution| *execution.turn_id == *turn_id)
        {
            return Ok(Some(execution));
        }
        match page.next_before_sequence {
            Some(next) => before_sequence = Some(next),
            None => return Ok(None),
        }
    }
}

// A caused command comes from a durable binding; direct user commands have no hook cause.
pub(super) fn command_pending(
    bots: &BotStore,
    cause: Option<&HookEvent>,
    command_id: &str,
) -> std::result::Result<bool, Rejection> {
    if cause.is_some() {
        bots.action_pending(command_id).map_err(internal)
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attention_projection_counts_open_items_and_deduplicates_replay() {
        use mobius::protocol::{
            Event, FrontendActionListItem, FrontendListItemState, FrontendSlot, FrontendTone,
            FrontendWidget, FrontendWidgetContent,
        };
        let temp = tempfile::tempdir().expect("temp");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            mobius::backend::checkpoint::sqlite::SqliteCheckpoint::new(
                temp.path().join("checkpoints"),
            )
            .expect("checkpoints"),
        );
        let bots = BotStore::open(temp.path()).expect("bots");
        let mut widget = FrontendWidget {
            id: "status".into(),
            count: None,
            priority: mobius::protocol::FrontendPriority::Primary,
            slot: FrontendSlot::Attention,
            text: "1".into(),
            tone: FrontendTone::Warning,
            symbol: None,
            icon_only: false,
            progress: None,
            action: None,
            content: Some(FrontendWidgetContent::ActionList {
                title: "Attention".into(),
                actions: Vec::new(),
                items: vec![FrontendActionListItem {
                    id: "call".into(),
                    text: "界".repeat(250),
                    state: FrontendListItemState::Pending,
                    actions: Vec::new(),
                }],
            }),
        };
        let record = |widget| JournalEvent {
            sequence: 1,
            recorded_at_ms: 1000,
            stream_metrics: Vec::new(),
            event: Event {
                submission_id: None,
                msg: EventMsg::Frontend(FrontendEvent::Widget {
                    capability: "test".into(),
                    item: widget,
                }),
            },
        };
        let mut widgets = Vec::new();
        super::super::replay::update_widgets(
            &mut widgets,
            &record(Arc::new(widget.clone())).event.msg,
        );
        assert_eq!(super::super::replay::attention_count(&widgets), 1);
        for _ in 0..2 {
            project_attention(
                &checkpoints,
                &bots,
                "bot",
                &SessionId::new("chat").unwrap(),
                &record(Arc::new(widget.clone())),
            )
            .await
            .expect("projection");
        }
        let events = bots.unpublished_events(10).expect("events");
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0].data, HookData::SessionAttention { text, item_id, .. } if text.chars().count() == 200 && &**item_id == "call")
        );
        if let Some(FrontendWidgetContent::ActionList { items, .. }) = &mut widget.content {
            items[0].state = FrontendListItemState::Completed;
        }
        super::super::replay::update_widgets(
            &mut widgets,
            &record(Arc::new(widget.clone())).event.msg,
        );
        assert_eq!(super::super::replay::attention_count(&widgets), 0);
        project_attention(
            &checkpoints,
            &bots,
            "bot",
            &SessionId::new("chat").unwrap(),
            &record(Arc::new(widget)),
        )
        .await
        .expect("closed projection");
        assert_eq!(bots.unpublished_events(10).expect("events").len(), 1);
    }
}
