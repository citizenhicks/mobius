use mobius::backend::checkpoint::Checkpoint;
use mobius::protocol::{SessionContext, TokenUsage};

use super::*;

mod bots;
mod lifecycle;
mod projection;
mod replay;
mod swarm_delivery;
mod swarm_management;

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

pub(crate) async fn create_distinct_test_session(
    gateway: &GatewayHost,
    workspace: &Path,
    handle: &str,
) -> std::result::Result<(HostHandle, crate::wire::BotRecord), Rejection> {
    let template = ensure_test_bot(gateway).await?;
    let bot = {
        let state = gateway.state.lock().await;
        state
            .bots
            .create_bot(handle, "Own distinct test work.", template.config.config)
            .map_err(invalid_bot)?
    };
    let host = gateway.create_session(workspace, &bot.id).await?;
    Ok((host, bot))
}
