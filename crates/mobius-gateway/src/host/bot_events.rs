//! Committed lifecycle projection and delivery through ordinary sessions.

use super::*;
use crate::wire::{BotAction, BotSubscription, HookData, HookEvent, HookSelector, HookSource};
use mobius::backend::checkpoint::ExecutionPageRequest;
use mobius::protocol::{ActiveMessageDelivery, MessageSource};

impl GatewayHost {
    pub(crate) async fn set_bot_subscription(
        &self,
        subscription: BotSubscription,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
        state.bots.bot(&subscription.bot_id).map_err(invalid_bot)?;
        let after =
            validate_selector(&state, &subscription.bot_id, &subscription.binding.on).await?;
        validate_action(&state, &subscription.bot_id, &subscription.binding.action).await?;
        state
            .bots
            .set_subscription(&subscription, after, Utc::now().timestamp())
            .map_err(invalid_config)
    }

    pub(crate) async fn execute_session_command(
        &self,
        session_id: &str,
        mut op: Op,
        bot_id: &str,
        cause: Option<&HookEvent>,
        command_id: &str,
    ) -> std::result::Result<(), Rejection> {
        match &mut op {
            Op::Message { message } => {
                if !message.attachments.is_empty() || message.reply.is_some() {
                    return Err(invalid_config(
                        "session hook messages cannot attach files or target transcript replies",
                    ));
                }
                message.author = source_author(command_id, cause, session_id);
                message.requested_delivery = Some(ActiveMessageDelivery::Queue);
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
        }
        host.deliver_source(
            Submission {
                id: command_id.into(),
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
            id: format!("custom-{command_id}"),
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
        Ok(event)
    }

    pub(crate) async fn bot_reporting_snapshot(
        &self,
        bot_id: &str,
    ) -> std::result::Result<serde_json::Value, Rejection> {
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        Ok(
            serde_json::json!({"subscriptions":state.bots.subscriptions(bot_id).map_err(invalid_bot)?, "webhooks":state.bots.webhooks(bot_id).map_err(invalid_bot)?}),
        )
    }

    pub(crate) async fn set_webhook_endpoint(&self, public_hostname: Option<&str>) -> Result<()> {
        let mut state = self.state.lock().await;
        let config = state
            .config
            .lock()
            .map_err(|_| Error::Config("configuration lock poisoned".into()))?;
        let endpoint = public_hostname
            .map(|host| format!("https://{host}"))
            .or_else(|| {
                (!config.listen.ip().is_unspecified()).then(|| {
                    format!(
                        "{}://{}",
                        if config.tls.is_some() {
                            "https"
                        } else {
                            "http"
                        },
                        config.listen
                    )
                })
            });
        drop(config);
        state.webhook_base_url = endpoint;
        Ok(())
    }

    pub(crate) async fn create_bot_webhook(
        &self,
        bot_id: &str,
        name: &str,
        instruction: &str,
    ) -> std::result::Result<serde_json::Value, Rejection> {
        use sha2::Digest as _;
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let record = state
            .bots
            .create_webhook(
                bot_id,
                name,
                instruction,
                sha2::Sha256::digest(token.as_bytes()).into(),
            )
            .map_err(invalid_config)?;
        let path = format!("/webhooks/{}", record.id);
        Ok(
            serde_json::json!({"source":record,"url":state.webhook_base_url.as_ref().map(|base|format!("{base}{path}")),"path":path,"bearer_token":token,"headers":{"X-Mobius-Delivery-Id":"unique stable ID per event","X-Mobius-Timestamp":"current Unix seconds","Content-Type":"application/json"},"body":{"text":"bounded outage or log report"}}),
        )
    }

    pub(crate) async fn configure_bot_webhook(
        &self,
        bot_id: &str,
        id: &str,
        enabled: bool,
        rotate_token: bool,
        delete: bool,
    ) -> std::result::Result<serde_json::Value, Rejection> {
        use sha2::Digest as _;
        let _mutation = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
        if delete {
            state
                .bots
                .delete_webhook(bot_id, id)
                .map_err(invalid_config)?;
            return Ok(serde_json::json!({"deleted":id}));
        }
        let token =
            rotate_token.then(|| format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()));
        state
            .bots
            .configure_webhook(
                bot_id,
                id,
                enabled,
                token
                    .as_ref()
                    .map(|token| sha2::Sha256::digest(token.as_bytes()).into()),
            )
            .map_err(invalid_config)?;
        Ok(serde_json::json!({"id":id,"enabled":enabled,"bearer_token":token}))
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
            let checkpoint = checkpoints.load(session_id).await?;
            if checkpoint.as_ref().is_some_and(|checkpoint| {
                checkpoint
                    .pending_approval
                    .as_ref()
                    .is_some_and(|approval| !approval.decision_received)
            }) {
                self.open_session(session_id)
                    .await
                    .map_err(|error| Error::Config(error.message))?;
                continue;
            }
            let outcome = if checkpoint.is_some() {
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
        let host = self.clone();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || runtime.block_on(host.dispatch_stored_bot_events()))
            .await
            .map_err(internal)?
    }

    async fn dispatch_stored_bot_events(&self) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (bots, checkpoints) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.bots), Arc::clone(&state.checkpoints))
        };
        for summary in gateway_session_summaries(&checkpoints)
            .await
            .map_err(internal)?
        {
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
            let id = event.id.clone();
            let _ = self
                .events
                .send(ServerFrame::new(ServerMessage::HookEvent { event }));
            bots.event_published(&id).map_err(internal)?;
        }
        drop(_mutation);
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
                        let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                            code: "hook_action".into(),
                            message: error.message,
                            fatal: false,
                        }));
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
                let host = self
                    .open_session(&crate::bots::conversation_session_id(&pending.bot_id))
                    .await?;
                let _delivery = self.begin_mutation().await?;
                if !bots.action_pending(&pending.id).map_err(internal)? {
                    return Ok(());
                }
                let text =
                    crate::bots::report_text(&pending.event, instruction).map_err(internal)?;
                host.deliver_source(
                    Submission {
                        id: pending.id.clone(),
                        op: Op::Message {
                            message: MessageSubmission {
                                author: source_author(&pending.id, Some(&pending.event), "hook"),
                                text,
                                attachments: Vec::new(),
                                reply: None,
                                requested_delivery: Some(ActiveMessageDelivery::Queue),
                                target_turn_id: None,
                            },
                        },
                    },
                    pending.bot_id.clone(),
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
                    op.as_ref().clone(),
                    &pending.bot_id,
                    Some(&pending.event),
                    &pending.id,
                )
                .await
            }
        }
    }
}

pub(super) async fn validate_selector(
    state: &GatewayState,
    bot_id: &str,
    selector: &HookSelector,
) -> std::result::Result<u64, Rejection> {
    let HookSelector::Event { source, .. } = selector else {
        return Err(invalid_config("timer selectors belong to routine bindings"));
    };
    match source {
        HookSource::Routine { routine_id } => {
            require_routine_owner_local(&state.bots, routine_id, bot_id)?;
            Ok(0)
        }
        HookSource::Session { session_id } => {
            if *session_id == crate::bots::conversation_session_id(bot_id) {
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
            if summary.session_context.owner_id != bot_id {
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
        HookSource::Custom { source_id } => {
            if !state
                .bots
                .webhooks(bot_id)
                .map_err(invalid_bot)?
                .iter()
                .any(|source| source.id == *source_id)
            {
                return Err(unknown_session());
            }
            Ok(0)
        }
        HookSource::Bot { bot_id: source } => {
            state.bots.bot(source).map_err(invalid_bot)?;
            Ok(0)
        }
        HookSource::Client { client_id } => {
            if !crate::auth::AuthStore::open(state.store.auth_path())
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
                    if matches!(binding.on, HookSelector::Event { .. }) {
                        validate_selector(state, bot_id, &binding.on).await?;
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
    if bots.routine(routine_id).map_err(invalid_routine)?.bot_id != bot_id {
        return Err(unknown_session());
    }
    Ok(())
}
async fn require_session_target(
    state: &GatewayState,
    bot_id: &str,
    session_id: &str,
) -> std::result::Result<(), Rejection> {
    let summary = state
        .checkpoints
        .session_summary(session_id)
        .await
        .map_err(internal)?
        .ok_or_else(unknown_session)?;
    if summary.session_context.owner_id != bot_id
        || !summary.catalog_visible
        || summary.parent_session_id.is_some()
        || session_id == crate::bots::conversation_session_id(bot_id)
    {
        return Err(unknown_session());
    }
    Ok(())
}
fn source_author(command_id: &str, cause: Option<&HookEvent>, source_id: &str) -> MessageAuthor {
    MessageAuthor::Source {
        cause_id: cause.map(|event| event.id.clone()),
        ancestry: cause.map_or_else(Vec::new, |event| {
            let mut chain = event.ancestry.clone();
            chain.push(event.id.clone());
            chain
        }),
        message_id: command_id.into(),
        source: MessageSource::External {
            source_id: source_id.into(),
            event_id: cause.map_or(command_id, |event| event.id.as_str()).into(),
        },
        handle: "source report".into(),
        symbol: None,
    }
}

async fn lifecycle_event(
    checkpoints: &Arc<dyn CheckpointStore>,
    bots: &BotStore,
    bot_id: &str,
    session_id: &str,
    record: &JournalEvent,
) -> Result<Option<HookEvent>> {
    let (data, turn_id) = match &record.event.msg {
        EventMsg::SessionConfigured(_) if record.sequence == 1 => (
            HookData::SessionCreated {
                session_id: session_id.into(),
            },
            None,
        ),
        EventMsg::TurnStarted(turn) => (
            HookData::SessionTurnStarted {
                session_id: session_id.into(),
                turn_id: turn.turn_id.clone(),
            },
            Some(turn.turn_id.as_str()),
        ),
        EventMsg::ExecApprovalRequest(approval) => (
            HookData::SessionApproval {
                session_id: session_id.into(),
                turn_id: approval.turn_id.clone(),
                request_id: approval.id.clone(),
            },
            Some(approval.turn_id.as_str()),
        ),
        EventMsg::TurnComplete(turn) => {
            let execution = execution_for_turn(checkpoints, session_id, &turn.turn_id)
                .await?
                .ok_or_else(|| Error::Config("terminal event has no execution record".into()))?;
            (
                HookData::SessionTurnFinished {
                    session_id: session_id.into(),
                    turn_id: turn.turn_id.clone(),
                    outcome: execution.outcome,
                },
                Some(turn.turn_id.as_str()),
            )
        }
        EventMsg::TurnAborted(turn) => {
            let execution = execution_for_turn(checkpoints, session_id, &turn.turn_id)
                .await?
                .ok_or_else(|| Error::Config("terminal event has no execution record".into()))?;
            (
                HookData::SessionTurnFinished {
                    session_id: session_id.into(),
                    turn_id: turn.turn_id.clone(),
                    outcome: execution.outcome,
                },
                Some(turn.turn_id.as_str()),
            )
        }
        _ => return Ok(None),
    };
    let author = if let Some(turn_id) = turn_id {
        match execution_for_turn(checkpoints, session_id, turn_id).await? {
            Some(execution) => Some(execution.author),
            None => checkpoints.load(session_id).await?.and_then(|checkpoint| {
                checkpoint
                    .active_execution
                    .filter(|execution| execution.turn_id == turn_id)
                    .map(|execution| execution.author)
            }),
        }
    } else {
        None
    };
    let (mut cause_id, mut ancestry) = author
        .as_ref()
        .map_or((None, Vec::new()), session_causality);
    // Routine prompts are user-authored, but their invocation retains the schedule/hook cause.
    if cause_id.is_none()
        && let Some(run) = bots
            .history(None)?
            .into_iter()
            .find(|run| run.session_id.as_deref() == Some(session_id))
        && let Some(cause) = bots.hook_event(&format!("run-{}-running", run.id))?
    {
        cause_id = Some(cause.id.clone());
        ancestry = cause.ancestry;
        if ancestry.len() < crate::bots::MAX_HOOK_ANCESTRY {
            ancestry.push(cause.id);
        }
    }
    Ok(Some(HookEvent {
        id: format!("session-{session_id}-{}", record.sequence),
        bot_id: bot_id.into(),
        source: HookSource::Session {
            session_id: session_id.into(),
        },
        data,
        occurred_at: record.recorded_at_ms.div_euclid(1000),
        cause_id,
        ancestry,
    }))
}

fn session_causality(author: &MessageAuthor) -> (Option<String>, Vec<String>) {
    // At the boundary, preserve the terminal fact without authorizing another action.
    if let MessageAuthor::Source {
        message_id,
        ancestry,
        ..
    } = author
        && ancestry.len() == crate::bots::MAX_HOOK_ANCESTRY
    {
        return (Some(message_id.clone()), ancestry.clone());
    }
    author.causal_origin()
}

async fn session_owner_at(
    checkpoints: &Arc<dyn CheckpointStore>,
    session_id: &str,
    sequence: u64,
) -> Result<Option<String>> {
    if sequence == 0 {
        return Ok(None);
    }
    let mut before_sequence = sequence.checked_add(1);
    loop {
        let page = checkpoints
            .event_page(
                session_id,
                EventPageRequest {
                    before_sequence,
                    limit: 100,
                },
            )
            .await?;
        for record in page.events {
            if let EventMsg::SessionConfigured(configured) = record.event.msg {
                return Ok(Some(configured.context.owner_id));
            }
        }
        match page.next_before_sequence {
            Some(next) => before_sequence = Some(next),
            None => return Ok(None),
        }
    }
}

async fn project_session_journal(
    checkpoints: &Arc<dyn CheckpointStore>,
    bots: &BotStore,
    session_id: &str,
    fallback_owner: &str,
    through: Option<u64>,
) -> Result<()> {
    let mut cursor = bots.source_cursor(session_id)?;
    let mut owner = session_owner_at(checkpoints, session_id, cursor).await?;
    loop {
        if through.is_some_and(|sequence| cursor >= sequence) {
            return Ok(());
        }
        let previous_cursor = cursor;
        for record in checkpoints.events_after(session_id, cursor, 100).await? {
            if through.is_some_and(|sequence| record.sequence > sequence) {
                break;
            }
            cursor = record.sequence;
            if let EventMsg::SessionConfigured(configured) = &record.event.msg {
                let next = &configured.context.owner_id;
                if let Some(previous) = &owner
                    && previous != next
                {
                    close_owner_change(bots, session_id, previous, next, &record)?;
                    owner = Some(next.clone());
                    continue;
                }
                owner = Some(next.clone());
            }
            let bot_id = owner.as_deref().unwrap_or(fallback_owner);
            if bots.bot(bot_id).is_ok()
                && let Some(event) =
                    lifecycle_event(checkpoints, bots, bot_id, session_id, &record).await?
            {
                bots.project_session(&event, record.sequence)?;
            } else {
                bots.advance_source_cursor(session_id, bot_id, record.sequence)?;
            }
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
    session_id: &str,
    previous: &str,
    next: &str,
    record: &JournalEvent,
) -> Result<()> {
    bots.close_session_sources(&[HookEvent {
        id: format!("session-{session_id}-{}", record.sequence),
        bot_id: previous.into(),
        source: HookSource::Session {
            session_id: session_id.into(),
        },
        data: HookData::SessionOwnerChanged {
            session_id: session_id.into(),
            previous_bot_id: previous.into(),
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
    session_id: &str,
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
    session_id: &str,
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
            .find(|execution| execution.turn_id == turn_id)
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

    #[test]
    fn source_terminal_facts_keep_the_bound_and_causal_message_identity() {
        for length in [15, 16] {
            let chain = (0..length)
                .map(|id| format!("cause-{id}"))
                .collect::<Vec<_>>();
            let author = MessageAuthor::Source {
                message_id: "command".into(),
                source: MessageSource::External {
                    source_id: "hook".into(),
                    event_id: "cause-0".into(),
                },
                cause_id: Some("cause-0".into()),
                ancestry: chain.clone(),
                handle: "source report".into(),
                symbol: None,
            };
            let (cause_id, ancestry) = session_causality(&author);
            assert_eq!(cause_id.as_deref(), Some("command"));
            assert_eq!(ancestry.len(), crate::bots::MAX_HOOK_ANCESTRY);
            assert!(ancestry.starts_with(&chain));
        }
    }
}
