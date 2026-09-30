use mobius::backend::checkpoint::Checkpoint;
use mobius::protocol::{SessionContext, TokenUsage};

use super::*;
use mobius::backend::checkpoint::ExecutionStats;

mod activity;
mod bots;
mod descriptors;
mod hooks;
mod lifecycle;
mod live_chats;
mod projection;
mod replay;

pub(crate) async fn ensure_test_bot(
    gateway: &GatewayHost,
) -> std::result::Result<crate::wire::BotRecord, Rejection> {
    let state = gateway.state.lock().await;
    if let Some(bot) = state.bots.bots().map_err(internal)?.into_iter().next() {
        return Ok(bot);
    }
    let mut config = state
        .config
        .lock()
        .map_err(|_| internal("gateway configuration lock is poisoned"))?;
    if config.bot_defaults.is_none() {
        let next = config
            .registering_provider(
                AgentComposition::default().provider,
                "Test".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
            .map_err(invalid_config)?;
        state.store.save(&next).map_err(internal)?;
        *config = next;
    }
    let composition = config
        .bot_defaults
        .as_ref()
        .expect("provider registration installs Bot defaults")
        .config
        .clone();
    drop(config);
    state
        .bots
        .create_bot("Test Bot", "Own gateway test work.", composition)
        .map_err(invalid_bot)
}

pub(crate) async fn create_test_session(
    gateway: &GatewayHost,
    workspace: &Path,
) -> std::result::Result<HostHandle, Rejection> {
    let bot = ensure_test_bot(gateway).await?;
    gateway.create_session(workspace, &bot.id).await
}

pub(crate) fn timer_definition(
    workspace: &Path,
    instructions: &str,
    schedule: crate::wire::RoutineSchedule,
    ends_at: Option<i64>,
) -> crate::wire::RoutineDefinition {
    crate::wire::RoutineDefinition {
        workspace: workspace.into(),
        instructions: instructions.into(),
        bindings: vec![crate::wire::RoutineBinding {
            id: Uuid::new_v4().to_string(),
            on: crate::wire::HookSelector::Schedule { schedule, ends_at },
            action: crate::wire::RoutineAction::Start,
        }],
    }
}
async fn start_routine(gateway: &GatewayHost, id: &str) -> std::result::Result<(), Rejection> {
    gateway
        .execute_routine_command(
            &crate::wire::RoutineCommand {
                routine_id: id.into(),
                action: crate::wire::RoutineAction::Start,
            },
            None,
            None,
            &Uuid::new_v4().to_string(),
        )
        .await
}
