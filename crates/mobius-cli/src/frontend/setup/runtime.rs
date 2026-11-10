use std::ops::ControlFlow;
use std::sync::Arc;

use mobius::backend::model::provider::BaseUrl;
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEventScope, GatewayEvents, GatewaySender};
use mobius_gateway::wire::{
    AgentComposition, BotRecord, ClientMessage, HarnessConfig, ProviderConfig, ReadyPayload,
    ServerFrame, ServerMessage,
};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::time::MissedTickBehavior;

use super::SetupMode;
use super::state::{Authentication, Flow, SetupState};
use super::view::draw;
use crate::frontend::terminal::{INPUT_POLL, Screen, input_batch};
use crate::gateway_error;

pub(super) async fn edit(
    terminal: &mut Screen,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<bool> {
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;
    loop {
        if dirty {
            draw(terminal, state)?;
            dirty = false;
        }
        tick.tick().await;
        for event in input_batch() {
            let event = event?;
            dirty = true;
            let flow = match event {
                Event::Key(key) => state.handle_key(key),
                Event::Paste(text) => {
                    state.paste(&text);
                    Flow::Continue
                }
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Mouse(_) => {
                    Flow::Continue
                }
            };
            match flow {
                Flow::Continue => {}
                Flow::Authenticate => {
                    let result = authenticate(terminal, state, sender, events).await;
                    if !state.authentication_finished(result)? {
                        return Ok(false);
                    }
                    break;
                }
                Flow::Remove(instance) => {
                    state.set_progress("Removing provider", "Updating the gateway catalog…");
                    draw(terminal, state)?;
                    let configured = configure(terminal, state, sender, events, |request_id| {
                        ClientMessage::RemoveProvider {
                            request_id,
                            instance,
                        }
                    })
                    .await?;
                    gateway.update(configured);
                    return Ok(false);
                }
                Flow::Finish => return Ok(true),
                Flow::Cancel => return Ok(false),
            }
        }
    }
}

pub(super) async fn authenticate(
    terminal: &mut Screen,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
) -> Result<bool> {
    match state.authentication() {
        Authentication::Reuse => {}
        Authentication::ApiKey(api_key) => {
            state.set_progress(
                "Saving credential",
                "Sending the key securely to the gateway…",
            );
            draw(terminal, state)?;
            set_credential(terminal, state, sender, events, api_key).await?;
        }
        Authentication::DeviceCode => {
            state.set_progress("Starting device login", "Requesting a one-time login code…");
            draw(terminal, state)?;
            return device_login(terminal, state, sender, events).await;
        }
    }
    Ok(true)
}

pub(super) async fn apply(
    terminal: &mut Screen,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    bot_id: &str,
) -> Result<()> {
    let bot = gateway
        .bots
        .iter()
        .find(|bot| *bot.id == *bot_id)
        .ok_or_else(|| Error::Config("this chat's Bot is not in the gateway catalog".into()))?;
    let config = state.agent_composition(mobius_composition(bot)?)?;
    let expected_revision = bot.config.revision;
    if state.mode == SetupMode::Login {
        state.set_progress(
            "Registering provider",
            "Updating the gateway model catalog…",
        );
        draw(terminal, state)?;
        let provider = config.provider.clone(); // rules-allow: the registration message owns one, and `config` still needs its provider
        gateway.update(register_provider(terminal, state, sender, events, provider).await?);
    }
    let bot = gateway
        .bots
        .iter()
        .find(|bot| *bot.id == *bot_id)
        .ok_or_else(|| Error::Config("this chat's Bot is not in the gateway catalog".into()))?;
    if mobius_composition(bot)?.as_ref() == &config {
        return Ok(());
    }
    state.set_progress("Updating Bot", "Applying this profile to the Bot's chats…");
    draw(terminal, state)?;
    let updated = update_bot(
        terminal,
        state,
        sender,
        events,
        bot,
        expected_revision,
        config,
    )
    .await?;
    if let Some(current) = gateway
        .bots
        .iter_mut()
        .find(|current| *current.id == *bot_id)
    {
        *current = updated;
    }
    Ok(())
}

pub(super) async fn apply_gateway(
    terminal: &mut Screen,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    let expected_revision = gateway
        .agent_catalog
        .bot_defaults
        .as_ref()
        .map(|default| default.revision);
    let config = state.agent_composition(&state.original)?;
    if state.mode == SetupMode::Login {
        state.set_progress(
            "Registering provider",
            "Updating the gateway model catalog…",
        );
        draw(terminal, state)?;
        let provider = config.provider.clone(); // rules-allow: the registration message owns one, and `config` still needs its provider
        gateway.update(register_provider(terminal, state, sender, events, provider).await?);
    }
    let default = gateway
        .agent_catalog
        .bot_defaults
        .as_ref()
        .ok_or_else(|| Error::Config("configure a provider before saving defaults".into()))?;
    if config == default.config {
        return Ok(());
    }
    state.set_progress(
        "Saving Bot template",
        "Future Bots will start with this configuration…",
    );
    draw(terminal, state)?;
    gateway.update(
        configure_bot_defaults(
            terminal,
            state,
            sender,
            events,
            expected_revision.unwrap_or(default.revision),
            config,
        )
        .await?,
    );
    Ok(())
}

pub(super) async fn register_provider(
    terminal: &mut Screen,
    state: &SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    config: ProviderConfig,
) -> Result<ReadyPayload> {
    let models = state.configured_models();
    let label = state.effective_label().into();
    configure(terminal, state, sender, events, |request_id| {
        ClientMessage::RegisterProvider {
            preserve_selection: false,
            if_configured: false,
            request_id,
            config,
            label: Some(label),
            tint: None,
            models,
            image_model_ids: None,
            operator_managed: false,
        }
    })
    .await
}

pub(super) async fn configure_bot_defaults(
    terminal: &mut Screen,
    state: &SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    expected_revision: u64,
    config: AgentComposition,
) -> Result<ReadyPayload> {
    configure(terminal, state, sender, events, |request_id| {
        ClientMessage::ConfigureBotDefaults {
            request_id,
            expected_revision,
            config,
        }
    })
    .await
}

/// The composition setup edits; only möbius Bots are composed from providers and middleware.
pub(super) fn mobius_composition(bot: &BotRecord) -> Result<&Arc<AgentComposition>> {
    bot.config.config.mobius().ok_or_else(|| {
        Error::Config("this Bot does not run on möbius; configure it in the app".into())
    })
}

pub(super) async fn update_bot(
    terminal: &mut Screen,
    state: &SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    bot: &BotRecord,
    expected_revision: u64,
    config: AgentComposition,
) -> Result<BotRecord> {
    let message = |request_id| ClientMessage::UpdateBot {
        request_id,
        id: Arc::clone(&bot.id),
        expected_revision,
        name: Arc::clone(&bot.name),
        description: Arc::clone(&bot.description),
        tint: bot.tint,
        shape: bot.shape,
        config: HarnessConfig::Mobius(Arc::new(config)),
    };
    let bots = exchange(
        terminal,
        state,
        sender,
        events,
        Failure::Configuration,
        message,
        |request_id, message| match message {
            ServerMessage::Bots {
                request_id: Some(actual),
                bots,
            } if *actual == *request_id => ControlFlow::Break(bots),
            message => ControlFlow::Continue(message),
        },
    )
    .await?;
    bots.into_iter()
        .find(|updated| updated.id == bot.id)
        .ok_or_else(|| Error::Stopped("gateway did not return the updated Bot".into()))
}

/// Sends one gateway configuration change and returns the updated catalog.
async fn configure(
    terminal: &mut Screen,
    state: &SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    message: impl FnOnce(Arc<str>) -> ClientMessage,
) -> Result<ReadyPayload> {
    exchange(
        terminal,
        state,
        sender,
        events,
        Failure::Configuration,
        message,
        |request_id, message| match message {
            ServerMessage::GatewayConfigured {
                request_id: actual,
                payload,
            } if *actual == *request_id => ControlFlow::Break(payload),
            message => ControlFlow::Continue(message),
        },
    )
    .await
}

/// How a rejected setup request is reported.
#[derive(Clone, Copy)]
enum Failure {
    /// A configuration change: a stale revision ends setup, anything else may be retried.
    Configuration,
    /// A credential or login step.
    Auth,
}

/// Sends one request and waits for its correlated response, redrawing on resize meanwhile.
async fn exchange<T>(
    terminal: &mut Screen,
    state: &SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    failure: Failure,
    message: impl FnOnce(Arc<str>) -> ClientMessage,
    mut response: impl FnMut(&str, ServerMessage) -> ControlFlow<T, ServerMessage>,
) -> Result<T> {
    let request_id = crate::fresh_id();
    sender
        .send(message(Arc::clone(&request_id)))
        .await
        .map_err(gateway_error)?;
    let mut events = events.scoped();
    loop {
        let Some(ServerFrame { version, message }) =
            next_frame(terminal, state, &mut events, Cancel::Disabled).await?
        else {
            return Err(Error::Stopped("setup request cancelled".into()));
        };
        if let Some(error) = response_error(&message, &request_id, failure) {
            return Err(error);
        }
        match response(&request_id, message) {
            ControlFlow::Break(value) => return Ok(value),
            ControlFlow::Continue(ServerMessage::Accepted { request_id: actual })
                if actual == request_id => {}
            ControlFlow::Continue(message) => events
                .defer(ServerFrame { version, message })
                .map_err(gateway_error)?,
        }
    }
}

fn response_error(message: &ServerMessage, request_id: &str, failure: Failure) -> Option<Error> {
    let error = message.response_error(Some(request_id))?;
    if error.fatal {
        return Some(Error::Stopped(error.message.into()));
    }
    Some(match failure {
        Failure::Auth => Error::Auth(error.message.into()),
        Failure::Configuration if matches!(message, ServerMessage::Rejected { code, .. } if code == "revision_conflict") => {
            Error::Stopped(format!("{}; reopen setup to refresh", error.message))
        }
        Failure::Configuration => Error::Config(error.message.into()),
    })
}

pub(super) async fn set_credential(
    terminal: &mut Screen,
    state: &SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    api_key: String,
) -> Result<()> {
    let instance = state.target_instance().to_owned();
    let provider = state.definition().provider.clone(); // rules-allow: the request owns the provider id
    let base_url = Some(BaseUrl::new(state.selected_base_url())?);
    exchange(
        terminal,
        state,
        sender,
        events,
        Failure::Auth,
        |request_id| ClientMessage::SetProviderCredential {
            request_id,
            instance,
            provider,
            base_url,
            api_key,
            expires_at: None,
        },
        |request_id, message| match message {
            ServerMessage::ProviderCredentialSaved {
                request_id: actual, ..
            } if *actual == *request_id => ControlFlow::Break(()),
            message => ControlFlow::Continue(message),
        },
    )
    .await
}

pub(super) async fn device_login(
    terminal: &mut Screen,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
) -> Result<bool> {
    let request_id = crate::fresh_id();
    sender
        .send(ClientMessage::StartProviderLogin {
            request_id: Arc::clone(&request_id),
            provider: state.definition().provider.clone(), // rules-allow: the request owns the provider id
        })
        .await
        .map_err(gateway_error)?;
    let mut events = events.scoped();
    let mut accepted = false;
    let mut completed = false;
    while !(accepted && completed) {
        let Some(frame) = next_frame(terminal, state, &mut events, Cancel::Enabled).await? else {
            return Ok(false);
        };
        if let Some(error) = response_error(&frame.message, &request_id, Failure::Auth) {
            return Err(error);
        }
        match frame.message {
            ServerMessage::Accepted { request_id: actual } if actual == request_id => {
                accepted = true;
            }
            ServerMessage::ProviderLoginStarted {
                request_id: actual,
                verification_url,
                user_code,
                ..
            } if actual == request_id => {
                state.show_device_code(verification_url, user_code);
                draw(terminal, state)?;
            }
            ServerMessage::ProviderLoginFinished {
                request_id: actual, ..
            } if actual == request_id => completed = true,
            message => events
                .defer(ServerFrame { message, ..frame })
                .map_err(gateway_error)?,
        }
    }
    Ok(true)
}

/// Whether Esc or Ctrl-C/D abandons a wait.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cancel {
    Enabled,
    Disabled,
}

/// The next gateway frame, or `None` when the user cancels a cancellable wait.
async fn next_frame(
    terminal: &mut Screen,
    state: &SetupState,
    events: &mut GatewayEventScope<'_>,
    cancel: Cancel,
) -> Result<Option<ServerFrame>> {
    loop {
        tokio::select! {
            frame = events.next() => {
                return frame
                    .map_err(gateway_error)?
                    .map(Some)
                    .ok_or_else(|| Error::Stopped("gateway disconnected during setup".into()));
            }
            () = tokio::time::sleep(INPUT_POLL) => {
                for event in input_batch() {
                    let event = event?;
                    match event {
                        Event::Key(key)
                            if cancel == Cancel::Enabled
                                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                                && (key.code == KeyCode::Esc
                                    || key.modifiers.contains(KeyModifiers::CONTROL)
                                        && matches!(key.code, KeyCode::Char('c' | 'd'))) =>
                        {
                            return Ok(None);
                        }
                        Event::Resize(_, _) => draw(terminal, state)?,
                        Event::Key(_)
                        | Event::Paste(_)
                        | Event::FocusGained
                        | Event::FocusLost
                        | Event::Mouse(_) => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_conflict_exits_the_stale_setup_instead_of_offering_a_retry() {
        let message = ServerMessage::Rejected {
            request_id: "save".into(),
            code: "revision_conflict".into(),
            message: "Bot changed elsewhere".into(),
            fatal: false,
        };
        assert!(matches!(
            response_error(&message, "save", Failure::Configuration),
            Some(Error::Stopped(message)) if message.contains("reopen setup")
        ));
    }
}
