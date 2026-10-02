//! Durable typed hooks and their user-authored consumers in the Bot database.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest as _, Sha256};

use super::StoredRoutine;
use super::storage::{BotStorage, save_catalog_row};
use crate::wire::{
    BotAction, BotSubscription, HookBinding, HookData, HookEvent, HookKind, HookSelector,
    HookSource, RoutineCommand, RoutineRun, RoutineRunStatus,
};
use crate::{Error, Result};

pub(crate) const MAX_HOOK_ANCESTRY: usize = 16;
pub(crate) const MAX_ROUTINE_BINDINGS: usize = 32;

#[cfg(test)]
mod tests;

pub(crate) struct PendingHookAction {
    pub(crate) bot_id: String,
    pub(crate) id: String,
    pub(crate) event: HookEvent,
    pub(crate) action: BotAction,
}

impl BotStorage {
    pub(super) fn subscriptions(&self, bot_id: &str) -> Result<Vec<BotSubscription>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        let mut query = connection.prepare("SELECT id, selector_json, action_json, enabled FROM hook_bindings WHERE bot_id = ?1 AND routine_id IS NULL ORDER BY id")?;
        query
            .query_map([bot_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })?
            .map(|row| {
                let (id, on, action, enabled) = row?;
                Ok(BotSubscription {
                    bot_id: bot_id.into(),
                    binding: HookBinding {
                        id,
                        on: serde_json::from_str(&on)?,
                        action: serde_json::from_str(&action)?,
                    },
                    enabled,
                })
            })
            .collect()
    }

    pub(super) fn set_subscription(
        &self,
        subscription: &BotSubscription,
        after_sequence: u64,
        now: i64,
    ) -> Result<()> {
        validate_bot_binding(&subscription.binding)?;
        if let BotAction::Routine { command } = &subscription.binding.action
            && let crate::wire::RoutineAction::Update { definition } = &command.action
        {
            super::validate_input_definition(definition)?;
        }
        if matches!(subscription.binding.on, HookSelector::Schedule { .. }) {
            return Err(Error::Config(
                "timer selectors belong to routine bindings".into(),
            ));
        }
        self.transaction(|tx| {
            save_binding(
                tx,
                &subscription.bot_id,
                None,
                &subscription.binding,
                subscription.enabled,
                after_sequence,
                now,
            )
        })
    }

    pub(super) fn save_catalog_with_hooks(
        &self,
        state_json: &str,
        routines: &[StoredRoutine],
        events: &[HookEvent],
        subscriptions: &[BotSubscription],
        now: i64,
        accepted_action_id: Option<&str>,
    ) -> Result<()> {
        self.transaction(|tx| {
            accept_action(tx, accepted_action_id)?;
            save_catalog_row(tx, state_json)?;
            sync_routine_bindings(tx, routines, now)?;
            for subscription in subscriptions {
                validate_bot_binding(&subscription.binding)?;
                save_binding(
                    tx,
                    &subscription.bot_id,
                    None,
                    &subscription.binding,
                    subscription.enabled,
                    0,
                    now,
                )?;
            }
            for event in events {
                record_event(tx, event, None)?;
            }
            Ok(())
        })
    }

    pub(super) fn record_hook(&self, event: &HookEvent) -> Result<bool> {
        self.transaction(|tx| record_event(tx, event, None))
    }

    pub(super) fn hook_event(&self, id: &str) -> Result<Option<HookEvent>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        connection
            .query_row(
                "SELECT event_json FROM hook_events WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| Ok(serde_json::from_str(&json)?))
            .transpose()
    }

    pub(super) fn unpublished_events(&self, limit: usize) -> Result<Vec<HookEvent>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        let mut query = connection.prepare(
            "SELECT event_json FROM hook_events WHERE published=0 ORDER BY occurred_at,rowid LIMIT ?1",
        )?;
        query
            .query_map(
                [i64::try_from(limit)
                    .map_err(|_| Error::Config("hook page is too large".into()))?],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }
    pub(super) fn event_published(&self, id: &str) -> Result<()> {
        self.transaction(|tx| {
            tx.execute("UPDATE hook_events SET published=1 WHERE id=?1", [id])?;
            Ok(())
        })
    }

    pub(super) fn source_cursor(&self, session_id: &str) -> Result<u64> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        cursor(&connection, session_id)
    }
    pub(super) fn advance_source_cursor(
        &self,
        session_id: &str,
        bot_id: &str,
        sequence: u64,
    ) -> Result<()> {
        self.transaction(|tx| {
            advance_cursor(tx, session_id, bot_id, sequence)?;
            Ok(())
        })
    }
    pub(super) fn project_session(&self, event: &HookEvent, sequence: u64) -> Result<bool> {
        let HookSource::Session { session_id } = &event.source else {
            return Err(Error::Config(
                "session projection requires a session source".into(),
            ));
        };
        self.transaction(|tx| {
            if !advance_cursor(tx, session_id, &event.bot_id, sequence)? {
                return Ok(false);
            }
            record_event(tx, event, Some(sequence))
        })
    }

    pub(super) fn pending_actions(&self, now: i64, limit: usize) -> Result<Vec<PendingHookAction>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        let limit =
            i64::try_from(limit).map_err(|_| Error::Config("hook page is too large".into()))?;
        let mut query=connection.prepare("SELECT outbox.id, events.event_json, outbox.action_json, outbox.bot_id FROM hook_outbox AS outbox JOIN hook_events AS events ON events.id=outbox.event_id WHERE outbox.accepted=0 AND outbox.retry_at<=?1 ORDER BY outbox.retry_at,outbox.rowid LIMIT ?2")?;
        query
            .query_map(params![now, limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (id, event, action, bot_id) = row?;
                Ok(PendingHookAction {
                    id,
                    event: serde_json::from_str(&event)?,
                    action: serde_json::from_str(&action)?,
                    bot_id,
                })
            })
            .collect()
    }

    pub(super) fn action_accepted(&self, id: &str) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "UPDATE hook_outbox SET accepted=1,retry_at=NULL,last_error=NULL WHERE id=?1",
                [id],
            )?;
            Ok(())
        })
    }
    pub(super) fn action_failed(&self, id: &str, error: &str, retry_at: Option<i64>) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "UPDATE hook_outbox SET last_error=?1,retry_at=?2 WHERE id=?3 AND accepted=0",
                params![error, retry_at, id],
            )?;
            Ok(())
        })
    }
    pub(super) fn action_pending(&self, id: &str) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM hook_outbox WHERE id=?1 AND accepted=0 AND retry_at IS NOT NULL)",[id],|row|row.get(0))?)
    }
    pub(super) fn has_pending_deliveries(&self) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM hook_outbox WHERE accepted=0 AND retry_at IS NOT NULL)",
            [],
            |row| row.get(0),
        )?)
    }
    pub(super) fn has_monitored_sessions(&self) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM hook_bindings WHERE enabled=1 AND json_extract(selector_json,'$.source.type')='session')",[],|row|row.get(0))?)
    }
    pub(super) fn close_session_sources(
        &self,
        events: &[HookEvent],
        state_json: &str,
    ) -> Result<()> {
        self.transaction(|tx| {
            for event in events {
                let HookSource::Session { session_id } = &event.source else {return Err(Error::Config("closing a source requires a session event".into()));};
                if !matches!(event.data, HookData::SessionDeleted { .. } | HookData::SessionOwnerChanged { .. }) {return Err(Error::Config("closing a source requires deletion or owner change".into()));}
                let existing = tx.query_row("SELECT event_json FROM hook_events WHERE id=?1", [&event.id], |row|row.get::<_,String>(0)).optional()?;
                if let Some(existing)=existing {
                    if existing!=serde_json::to_string(event)? {return Err(Error::Config("closure event identity has conflicting content".into()));}
                    continue;
                }
                cancel_source_pending(tx, &event.source)?;
                record_event(tx, event, None)?;
                tx.execute("DELETE FROM hook_bindings WHERE json_extract(selector_json,'$.source.type')='session' AND json_extract(selector_json,'$.source.session_id')=?1", [session_id])?;
                tx.execute("DELETE FROM bot_session_cursors WHERE session_id=?1", [session_id])?;
            }
            save_catalog_row(tx, state_json)
        })
    }
}

#[cfg(test)]
pub(crate) fn event_selector(source: HookSource, kind: HookKind) -> HookSelector {
    HookSelector::Event {
        source,
        kind,
        routine_outcome: None,
        session_outcome: None,
        custom_name: None,
    }
}

pub(crate) fn caused_event(
    id: String,
    bot_id: String,
    source: HookSource,
    data: HookData,
    now: i64,
    cause: Option<&HookEvent>,
) -> Result<HookEvent> {
    let mut ancestry = cause.map_or_else(Vec::new, |event| event.ancestry.clone());
    if let Some(cause) = cause {
        validate_event(cause)?;
        if ancestry.len() < MAX_HOOK_ANCESTRY {
            ancestry.push(cause.id.clone());
        }
    }
    let event = HookEvent {
        id,
        bot_id,
        source,
        data,
        occurred_at: now,
        cause_id: cause.map(|event| event.id.clone()),
        ancestry,
    };
    validate_event(&event)?;
    Ok(event)
}

fn cursor(connection: &Connection, session_id: &str) -> Result<u64> {
    let value = connection
        .query_row(
            "SELECT sequence FROM bot_session_cursors WHERE session_id=?1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    u64::try_from(value).map_err(|_| Error::Config("negative session cursor".into()))
}
fn advance_cursor(
    tx: &Transaction<'_>,
    session_id: &str,
    bot_id: &str,
    sequence: u64,
) -> Result<bool> {
    if let Some(owner) = tx
        .query_row(
            "SELECT bot_id FROM bot_session_cursors WHERE session_id=?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        && owner != bot_id
    {
        return Err(Error::Config(
            "session cursor belongs to another Bot".into(),
        ));
    }
    let sequence = i64::try_from(sequence)
        .map_err(|_| Error::Config("session sequence is too large".into()))?;
    Ok(tx.execute("INSERT INTO bot_session_cursors(session_id,bot_id,sequence) VALUES(?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET sequence=excluded.sequence WHERE excluded.sequence>bot_session_cursors.sequence",params![session_id,bot_id,sequence])?!=0)
}
fn cancel_source_pending(tx: &Transaction<'_>, source: &HookSource) -> Result<()> {
    tx.execute("DELETE FROM hook_outbox WHERE accepted=0 AND event_id IN(SELECT id FROM hook_events WHERE source_json=?1)",[serde_json::to_string(source)?])?;
    Ok(())
}

pub(super) fn accept_action(tx: &Transaction<'_>, id: Option<&str>) -> Result<()> {
    if let Some(id) = id {
        tx.execute(
            "UPDATE hook_outbox SET accepted=1,retry_at=NULL,last_error=NULL WHERE id=?1",
            [id],
        )?;
    }
    Ok(())
}
fn save_binding(
    tx: &Transaction<'_>,
    bot_id: &str,
    routine_id: Option<&str>,
    binding: &HookBinding<BotAction>,
    enabled: bool,
    after_sequence: u64,
    now: i64,
) -> Result<()> {
    let sequence = i64::try_from(after_sequence)
        .map_err(|_| Error::Config("binding sequence is too large".into()))?;
    let previous = tx
        .query_row(
            "SELECT routine_id,selector_json,action_json FROM hook_bindings WHERE bot_id=?1 AND id=?2",
            params![bot_id, binding.id],
            |row| Ok((row.get::<_, Option<String>>(0)?,row.get::<_, String>(1)?,row.get::<_, String>(2)?)),
        )
        .optional()?;
    if previous
        .as_ref()
        .is_some_and(|previous| previous.0.as_deref() != routine_id)
    {
        return Err(Error::Config(
            "hook binding identity is already in use".into(),
        ));
    }
    let selector_json = serde_json::to_string(&binding.on)?;
    let action_json = serde_json::to_string(&binding.action)?;
    let changed = previous
        .as_ref()
        .is_some_and(|previous| previous.1 != selector_json || previous.2 != action_json);
    tx.execute("INSERT INTO hook_bindings(bot_id,id,routine_id,selector_json,action_json,enabled,after_sequence,starts_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(bot_id,id) DO UPDATE SET selector_json=excluded.selector_json,action_json=excluded.action_json,enabled=excluded.enabled,after_sequence=CASE WHEN hook_bindings.enabled=0 AND excluded.enabled=1 OR hook_bindings.selector_json!=excluded.selector_json THEN excluded.after_sequence ELSE hook_bindings.after_sequence END,starts_at=CASE WHEN hook_bindings.enabled=0 AND excluded.enabled=1 OR hook_bindings.selector_json!=excluded.selector_json THEN excluded.starts_at ELSE hook_bindings.starts_at END",params![bot_id,binding.id,routine_id,serde_json::to_string(&binding.on)?,serde_json::to_string(&binding.action)?,enabled,sequence,now])?;
    if !enabled || changed {
        tx.execute(
            "DELETE FROM hook_outbox WHERE bot_id=?1 AND binding_id=?2 AND accepted=0",
            params![bot_id, binding.id],
        )?;
    }
    Ok(())
}
fn sync_routine_bindings(tx: &Transaction<'_>, routines: &[StoredRoutine], now: i64) -> Result<()> {
    let mut query =
        tx.prepare("SELECT bot_id,id,routine_id FROM hook_bindings WHERE routine_id IS NOT NULL")?;
    let held = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(query);
    for (bot_id, id, routine_id) in held {
        if !routines.iter().any(|routine| {
            routine.id == routine_id
                && routine.bot_id == bot_id
                && routine
                    .bindings
                    .iter()
                    .any(|binding| binding.definition.id == id)
        }) {
            tx.execute(
                "DELETE FROM hook_outbox WHERE bot_id=?1 AND binding_id=?2 AND accepted=0",
                params![bot_id, id],
            )?;
            tx.execute(
                "DELETE FROM hook_bindings WHERE bot_id=?1 AND id=?2",
                params![bot_id, id],
            )?;
        }
    }
    for routine in routines {
        for stored in &routine.bindings {
            let binding = HookBinding {
                id: stored.definition.id.clone(),
                on: stored.definition.on.clone(),
                action: BotAction::Routine {
                    command: RoutineCommand {
                        routine_id: routine.id.clone(),
                        action: stored.definition.action.clone(),
                    },
                },
            };
            validate_bot_binding(&binding)?;
            let after_sequence = match &binding.on {
                HookSelector::Event {
                    source: HookSource::Session { session_id },
                    ..
                } => cursor(tx, session_id)?,
                _ => 0,
            };
            save_binding(
                tx,
                &routine.bot_id,
                Some(&routine.id),
                &binding,
                true,
                after_sequence,
                now,
            )?;
        }
    }
    Ok(())
}

pub(super) fn record_run(tx: &Transaction<'_>, run: &RoutineRun) -> Result<()> {
    let cause=tx.query_row("SELECT COALESCE((SELECT cause_json FROM routine_stop_requests WHERE run_id=?1),(SELECT cause_json FROM routine_commands WHERE run_id=?1))",[&run.id],|row|row.get::<_,Option<String>>(0))?.map(|json|serde_json::from_str::<HookEvent>(&json)).transpose()?;
    let data = match run.status {
        RoutineRunStatus::Running => HookData::RunStarted {
            routine_id: run.routine_id.clone(),
            run_id: run.id.clone(),
            session_id: run.session_id.clone(),
        },
        RoutineRunStatus::Skipped => HookData::RunSkipped {
            routine_id: run.routine_id.clone(),
            run_id: run.id.clone(),
            reason: run
                .message
                .clone()
                .unwrap_or_else(|| "invocation was skipped".into()),
        },
        status => HookData::RunFinished {
            routine_id: run.routine_id.clone(),
            run_id: run.id.clone(),
            status,
            session_id: run.session_id.clone(),
            reason: run.message.clone(),
        },
    };
    let event = caused_event(
        format!("run-{}-{}", run.id, super::storage::status_text(run.status)),
        run.bot_id.clone(),
        HookSource::Routine {
            routine_id: run.routine_id.clone(),
        },
        data,
        run.finished_at.unwrap_or(run.started_at),
        cause.as_ref(),
    )?;
    record_event(tx, &event, None)?;
    Ok(())
}

pub(super) fn record_event(
    tx: &Transaction<'_>,
    event: &HookEvent,
    sequence: Option<u64>,
) -> Result<bool> {
    validate_event(event)?;
    let json = serde_json::to_string(event)?;
    if let Some(previous) = tx
        .query_row(
            "SELECT event_json FROM hook_events WHERE id=?1",
            [&event.id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        if previous != json {
            return Err(Error::Config(
                "hook event ID has conflicting content".into(),
            ));
        }
        return Ok(false);
    }
    tx.execute("INSERT INTO hook_events(id,bot_id,source_json,occurred_at,event_json) VALUES(?1,?2,?3,?4,?5)",params![event.id,event.bot_id,serde_json::to_string(&event.source)?,event.occurred_at,json])?;
    if event.ancestry.len() >= MAX_HOOK_ANCESTRY {
        return Ok(true);
    }
    let mut query=tx.prepare("SELECT id,selector_json,action_json,after_sequence,starts_at,bot_id,routine_id FROM hook_bindings WHERE enabled=1 AND (bot_id=?1 OR ?2=1)")?;
    let bindings = query
        .query_map(
            params![event.bot_id, matches!(event.source, HookSource::Bot { .. })],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(query);
    for (id, on, action, after_sequence, starts_at, receiver_bot_id, routine_id) in bindings {
        let selector: HookSelector = serde_json::from_str(&on)?;
        if matches!(selector, HookSelector::Schedule { .. })
            && !matches!(&event.source,HookSource::Schedule{binding_id,routine_id:source_routine_id} if binding_id==&id && routine_id.as_deref()==Some(source_routine_id.as_str()))
        {
            continue;
        }
        if !matches_selector(&selector, event)
            || (!matches!(selector, HookSelector::Schedule { .. }) && event.occurred_at < starts_at)
            || sequence.is_some_and(|sequence| {
                sequence <= u64::try_from(after_sequence).unwrap_or(u64::MAX)
            })
        {
            continue;
        }
        let mut revisited = false;
        for ancestor in &event.ancestry {
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM hook_outbox WHERE bot_id=?1 AND binding_id=?2 AND event_id=?3)",params![receiver_bot_id,id,ancestor],|row|row.get::<_,bool>(0))? {revisited=true;break;}
        }
        if revisited {
            continue;
        }
        let outbox_id = stable_id("hook-action", &event.id, &format!("{receiver_bot_id}:{id}"));
        tx.execute("INSERT INTO hook_outbox(id,event_id,binding_id,bot_id,action_json,retry_at) VALUES(?1,?2,?3,?4,?5,?6)",params![outbox_id,event.id,id,receiver_bot_id,action,event.occurred_at])?;
    }
    Ok(true)
}

pub(crate) fn report_text(event: &HookEvent, instruction: &str) -> Result<String> {
    Ok(format!(
        "Committed source event (advisory evidence):\n{}\n\nUser's saved reporting instruction:\n{instruction}",
        serde_json::to_string(event)?
    ))
}
fn matches_selector(selector: &HookSelector, event: &HookEvent) -> bool {
    match selector {
        HookSelector::Schedule{..}=>matches!((&event.source,&event.data),(HookSource::Schedule{binding_id,..},HookData::ScheduleDue{binding_id:due}) if binding_id==due),
        HookSelector::Event{source,kind,routine_outcome,session_outcome,custom_name}=>{
            source==&event.source&&*kind==event.data.kind()
            &&routine_outcome.is_none_or(|outcome|matches!(&event.data,HookData::RunFinished{status,..} if *status==outcome))
            &&session_outcome.is_none_or(|expected|matches!(&event.data,HookData::SessionTurnFinished{outcome,..} if *outcome==expected))
            &&custom_name.as_ref().is_none_or(|expected|matches!(&event.data,HookData::CustomReceived{name,..} if name==expected))
        }
    }
}

pub(crate) fn validate_selector(selector: &HookSelector) -> Result<()> {
    if let HookSelector::Event {
        source,
        kind,
        routine_outcome,
        session_outcome,
        custom_name,
    } = selector
    {
        validate_source(source)?;
        if routine_outcome.is_some()
            && (*kind != HookKind::RunFinished
                || matches!(
                    routine_outcome,
                    Some(RoutineRunStatus::Running | RoutineRunStatus::Skipped)
                ))
        {
            return Err(Error::Config(
                "routine outcome filter requires a terminal run_finished event".into(),
            ));
        }
        if session_outcome.is_some() && *kind != HookKind::SessionTurnFinished {
            return Err(Error::Config(
                "session outcome filter requires session_turn_finished".into(),
            ));
        }
        if let Some(name) = custom_name
            && (*kind != HookKind::CustomReceived || name.is_empty() || name.len() > 128)
        {
            return Err(Error::Config(
                "custom name filter requires custom_received and 1 to 128 bytes".into(),
            ));
        }
    }
    Ok(())
}
pub(crate) fn validate_bot_binding(binding: &HookBinding<BotAction>) -> Result<()> {
    validate_id(&binding.id)?;
    validate_selector(&binding.on)?;
    match &binding.action {
        BotAction::Report { instruction } => {
            if instruction.trim().is_empty()
                || instruction.len() > 8192
                || instruction.contains('\0')
            {
                return Err(Error::Config(
                    "reporting instruction must contain 1 to 8192 bytes".into(),
                ));
            }
        }
        BotAction::Routine { command } => {
            validate_id(&command.routine_id)?;
            super::validate_routine_action(&command.action, 0)?;
        }
        BotAction::Session { session_id, op } => {
            validate_id(session_id)?;
            match op.as_ref() {
                mobius::protocol::Op::Message { message } => {
                    message.validate(mobius::backend::session_files::session_file_limits())?;
                }
                mobius::protocol::Op::Interrupt { turn_id } => validate_id(turn_id)?,
                _ => {
                    return Err(Error::Config(
                        "session hooks support only message and interrupt operations".into(),
                    ));
                }
            }
        }
    }
    Ok(())
}
fn validate_source(source: &HookSource) -> Result<()> {
    match source {
        HookSource::Bot { bot_id } => validate_id(bot_id),
        HookSource::Routine { routine_id } => validate_id(routine_id),
        HookSource::Session { session_id } => validate_id(session_id),
        HookSource::Schedule {
            routine_id,
            binding_id,
        } => {
            validate_id(routine_id)?;
            validate_id(binding_id)
        }
        HookSource::Client { client_id } => validate_id(client_id),
        HookSource::Gateway => Ok(()),
    }
}
fn validate_event(event: &HookEvent) -> Result<()> {
    validate_id(&event.id)?;
    validate_id(&event.bot_id)?;
    validate_source(&event.source)?;
    if event.ancestry.len() > MAX_HOOK_ANCESTRY
        || event.ancestry.iter().any(|id| id == &event.id)
        || event
            .ancestry
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != event.ancestry.len()
    {
        return Err(Error::Config(
            "hook causal chain exceeds its bound or revisits an event".into(),
        ));
    }
    for id in &event.ancestry {
        validate_id(id)?;
    }
    if let Some(id) = &event.cause_id {
        validate_id(id)?;
    }
    if serde_json::to_vec(event)?.len() > mobius::protocol::MAX_MESSAGE_BYTES / 2 + 8192 {
        return Err(Error::Config("hook event is too large".into()));
    }
    Ok(())
}
fn validate_id(id: &str) -> Result<()> {
    if id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(Error::Config(
            "hook identity must contain 1 to 256 safe bytes".into(),
        ));
    }
    Ok(())
}
pub(crate) fn stable_id(prefix: &str, first: &str, second: &str) -> String {
    let mut hash = Sha256::new();
    hash.update((first.len() as u64).to_be_bytes());
    hash.update(first.as_bytes());
    hash.update(second.as_bytes());
    format!("{prefix}-{:x}", hash.finalize())
}
