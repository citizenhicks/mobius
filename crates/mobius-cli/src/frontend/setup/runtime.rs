use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender, MAX_PENDING_FRAMES};
use mobius_gateway::wire::{
    AgentComposition, ClientMessage, ProviderConfig, ReadyPayload, ServerFrame, ServerMessage,
    SessionReadyPayload,
};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use super::state::{ApplyTarget, Authentication, Flow, SetupState};
use super::view::draw;
use super::{SetupMode, SetupTerminal};
use crate::frontend::terminal::{INPUT_POLL, MAX_INPUT_BATCH, poll_event};

pub(super) async fn edit(
    terminal: &mut SetupTerminal,
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
        for _ in 0..MAX_INPUT_BATCH {
            let Some(event) = poll_event()? else {
                break;
            };
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
                    authenticate(terminal, state, sender, events).await?;
                    state.authentication_succeeded();
                    break;
                }
                Flow::Remove(instance) => {
                    state.set_progress("Removing provider", "Updating the gateway catalog…");
                    draw(terminal, state)?;
                    let request_id = Uuid::new_v4().to_string();
                    sender
                        .send(ClientMessage::RemoveProvider {
                            request_id: request_id.clone(),
                            instance,
                        })
                        .await
                        .map_err(gateway_error)?;
                    *gateway = wait_gateway_configured(
                        terminal,
                        state,
                        events,
                        &request_id,
                        "removing a provider",
                    )
                    .await?;
                    return Ok(false);
                }
                Flow::Finish => return Ok(true),
                Flow::Cancel => return Ok(false),
            }
        }
    }
}

pub(super) async fn authenticate(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
) -> Result<()> {
    match state.take_authentication()? {
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
            device_login(terminal, state, sender, events).await?;
        }
    }
    Ok(())
}

pub(super) async fn apply(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
) -> Result<()> {
    let config = state.agent_composition(&session.config.config)?;
    if state.mode == SetupMode::Login {
        state.set_progress(
            "Registering provider",
            "Updating the gateway model catalog…",
        );
        draw(terminal, state)?;
        *gateway =
            register_provider(terminal, state, sender, events, config.provider.clone()).await?;
    }
    match state.target {
        ApplyTarget::Session => {
            if config == session.config.config {
                return Ok(());
            }
            state.set_progress(
                "Applying agent configuration",
                "The gateway is restarting the agent while preserving this session…",
            );
            draw(terminal, state)?;
            let session_id = session.session.session_id.clone();
            *session = configure_session(
                terminal,
                state,
                sender,
                events,
                &session_id,
                session.config.revision,
                config,
            )
            .await?;
        }
        ApplyTarget::Default => {
            let default = gateway.default_config.as_ref().ok_or_else(|| {
                Error::Config("configure a provider before saving defaults".into())
            })?;
            if config == default.config {
                return Ok(());
            }
            state.set_progress(
                "Saving gateway defaults",
                "Future chats will use this agent configuration…",
            );
            draw(terminal, state)?;
            *gateway =
                configure_default_agent(terminal, state, sender, events, default.revision, config)
                    .await?;
        }
    }
    Ok(())
}

pub(super) async fn apply_gateway(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    let config = state.agent_composition(&state.original)?;
    if state.mode == SetupMode::Login {
        state.set_progress(
            "Registering provider",
            "Updating the gateway model catalog…",
        );
        draw(terminal, state)?;
        *gateway =
            register_provider(terminal, state, sender, events, config.provider.clone()).await?;
    }
    let default = gateway
        .default_config
        .as_ref()
        .ok_or_else(|| Error::Config("configure a provider before saving defaults".into()))?;
    if config == default.config {
        return Ok(());
    }
    state.set_progress(
        "Saving gateway defaults",
        "Future chats will use this agent configuration…",
    );
    draw(terminal, state)?;
    *gateway =
        configure_default_agent(terminal, state, sender, events, default.revision, config).await?;
    Ok(())
}

pub(super) async fn register_provider(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    config: ProviderConfig,
) -> Result<ReadyPayload> {
    let model_ids = state.configured_model_ids()?;
    let reasoning_efforts = state.instance_reasoning_efforts().to_vec();
    let label = state.effective_label();
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::RegisterProvider {
            request_id: request_id.clone(),
            config,
            label,
            tint: state
                .instance()
                .map(|instance| instance.tint)
                .unwrap_or_default(),
            model_ids,
            reasoning_efforts,
            replace_existing_selections: false,
        })
        .await
        .map_err(gateway_error)?;
    wait_gateway_configured(
        terminal,
        state,
        events,
        &request_id,
        "registering a provider",
    )
    .await
}

pub(super) async fn configure_default_agent(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    expected_revision: u64,
    config: AgentComposition,
) -> Result<ReadyPayload> {
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::ConfigureDefaultAgent {
            request_id: request_id.clone(),
            expected_revision,
            config,
        })
        .await
        .map_err(gateway_error)?;
    wait_gateway_configured(
        terminal,
        state,
        events,
        &request_id,
        "saving gateway defaults",
    )
    .await
}

pub(super) async fn wait_gateway_configured(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    events: &mut GatewayEvents,
    request_id: &str,
    operation: &str,
) -> Result<ReadyPayload> {
    let mut deferred = Vec::new();
    let result = loop {
        let frame = match next_frame(terminal, state, events, false).await {
            Ok(frame) => frame,
            Err(error) => break Err(error),
        };
        match frame.message {
            ServerMessage::GatewayConfigured {
                request_id: actual,
                payload,
            } if actual == request_id => break Ok(payload),
            ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } if actual == request_id => break Err(Error::Stopped(message)),
            ServerMessage::Error { message, .. } => break Err(Error::Stopped(message)),
            message if deferred.len() == MAX_PENDING_FRAMES => {
                break Err(Error::Stopped(format!(
                    "gateway event backlog exceeds {MAX_PENDING_FRAMES} frames while {operation}: {message:?}"
                )));
            }
            message => deferred.push(ServerFrame::new(message)),
        }
    };
    events.prepend(deferred).map_err(gateway_error)?;
    result
}

pub(super) async fn set_credential(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    api_key: String,
) -> Result<()> {
    let instance = state.target_instance();
    let provider = state.definition().provider.clone();
    let request_id = Uuid::new_v4().to_string();
    let message = match state.selected_base_url() {
        None => ClientMessage::SetProviderCredential {
            request_id: request_id.clone(),
            instance: instance.clone(),
            provider: provider.clone(),
            api_key,
        },
        Some(base_url) => ClientMessage::SetProviderEndpointCredential {
            request_id: request_id.clone(),
            instance: instance.clone(),
            provider: provider.clone(),
            base_url,
            api_key,
        },
    };
    sender.send(message).await.map_err(gateway_error)?;
    let _ = wait_for_response(
        terminal,
        state,
        events,
        &request_id,
        ExpectedResponse::Credential {
            instance: &instance,
            provider: &provider,
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn device_login(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
) -> Result<()> {
    let provider = state.definition().provider.clone();
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::StartProviderLogin {
            request_id: request_id.clone(),
            provider: provider.clone(),
        })
        .await
        .map_err(gateway_error)?;
    let _ = wait_for_response(
        terminal,
        state,
        events,
        &request_id,
        ExpectedResponse::Login(&provider),
    )
    .await?;
    Ok(())
}

pub(super) async fn configure_session(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    session_id: &str,
    expected_revision: u64,
    config: AgentComposition,
) -> Result<SessionReadyPayload> {
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::ConfigureSession {
            request_id: request_id.clone(),
            session_id: session_id.into(),
            expected_revision,
            config,
        })
        .await
        .map_err(gateway_error)?;
    wait_for_response(
        terminal,
        state,
        events,
        &request_id,
        ExpectedResponse::Configure {
            session_id,
            revision: expected_revision,
        },
    )
    .await?
    .ok_or_else(|| Error::Stopped("gateway did not return the configured chat".into()))
}

#[derive(Clone, Copy)]
pub(super) enum ExpectedResponse<'a> {
    Credential {
        instance: &'a str,
        provider: &'a str,
    },
    Login(&'a str),
    Configure {
        session_id: &'a str,
        revision: u64,
    },
}

impl ExpectedResponse<'_> {
    pub(super) fn matches_credential(&self, instance: &str, provider: &str) -> bool {
        matches!(
            self,
            Self::Credential {
                instance: expected_instance,
                provider: expected_provider,
            } if instance == *expected_instance && provider == *expected_provider
        )
    }
}

pub(super) async fn wait_for_response(
    terminal: &mut SetupTerminal,
    state: &mut SetupState,
    events: &mut GatewayEvents,
    request_id: &str,
    expected: ExpectedResponse<'_>,
) -> Result<Option<SessionReadyPayload>> {
    let mut accepted = matches!(expected, ExpectedResponse::Credential { .. });
    let mut completed = matches!(expected, ExpectedResponse::Configure { .. });
    let mut deferred = Vec::new();
    let mut snapshot = None;
    let result = loop {
        let frame = match next_frame(
            terminal,
            state,
            events,
            matches!(expected, ExpectedResponse::Login(_)),
        )
        .await
        {
            Ok(frame) => frame,
            Err(error) => break Err(error),
        };
        let defer = match &frame.message {
            ServerMessage::SessionChanged { payload }
                if matches!(
                    expected,
                    ExpectedResponse::Configure {
                        session_id,
                        revision,
                    } if payload.session.session_id == session_id
                        && payload.config.revision > revision
                ) =>
            {
                snapshot = Some((deferred.len(), payload.clone()));
                false
            }
            ServerMessage::Accepted { request_id: actual } if actual == request_id => {
                accepted = true;
                false
            }
            ServerMessage::ProviderCredentialSaved {
                request_id: actual,
                instance,
                provider,
            } if actual == request_id && expected.matches_credential(instance, provider) => {
                completed = true;
                false
            }
            ServerMessage::ProviderLoginStarted {
                request_id: actual,
                provider,
                verification_url,
                user_code,
                ..
            } if actual == request_id
                && matches!(expected, ExpectedResponse::Login(expected) if provider == expected) =>
            {
                state.show_device_code(verification_url.clone(), user_code.clone());
                if let Err(error) = draw(terminal, state) {
                    break Err(error);
                }
                false
            }
            ServerMessage::ProviderLoginFinished {
                request_id: actual,
                provider,
                ..
            } if actual == request_id
                && matches!(expected, ExpectedResponse::Login(expected) if provider == expected) =>
            {
                completed = true;
                false
            }
            ServerMessage::ProviderCredentialSaved {
                request_id: actual, ..
            }
            | ServerMessage::ProviderLoginStarted {
                request_id: actual, ..
            }
            | ServerMessage::ProviderLoginFinished {
                request_id: actual, ..
            } if actual == request_id => {
                break Err(Error::Stopped(
                    "gateway returned an invalid setup response".into(),
                ));
            }
            ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } if actual == request_id => break Err(Error::Stopped(message.clone())),
            ServerMessage::Error { message, .. } => break Err(Error::Stopped(message.clone())),
            _ => true,
        };
        if accepted && completed {
            if matches!(expected, ExpectedResponse::Configure { .. }) {
                if let Some((_, payload)) = snapshot.take() {
                    break Ok(Some(payload));
                }
            } else {
                break Ok(None);
            }
        }
        if defer && deferred.len() + usize::from(snapshot.is_some()) == MAX_PENDING_FRAMES {
            break Err(Error::Stopped(format!(
                "gateway event backlog exceeds {MAX_PENDING_FRAMES} frames"
            )));
        }
        if defer {
            deferred.push(frame);
        }
    };
    if result.is_err()
        && let Some((index, snapshot)) = snapshot
    {
        deferred.insert(
            index,
            ServerFrame::new(ServerMessage::SessionChanged { payload: snapshot }),
        );
    }
    events.prepend(deferred).map_err(gateway_error)?;
    result
}

pub(super) async fn next_frame(
    terminal: &mut SetupTerminal,
    state: &SetupState,
    events: &mut GatewayEvents,
    cancellable: bool,
) -> Result<ServerFrame> {
    loop {
        tokio::select! {
            frame = events.next() => {
                return frame
                    .map_err(gateway_error)?
                    .ok_or_else(|| Error::Stopped("gateway disconnected during setup".into()));
            }
            _ = tokio::time::sleep(INPUT_POLL) => {
                for _ in 0..MAX_INPUT_BATCH {
                    let Some(event) = poll_event()? else {
                        break;
                    };
                    match event {
                        Event::Key(key)
                            if cancellable
                                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                                && (key.code == KeyCode::Esc
                                    || key.modifiers.contains(KeyModifiers::CONTROL)
                                        && matches!(key.code, KeyCode::Char('c' | 'd'))) =>
                        {
                            return Err(Error::Config(
                                "setup cancelled; gateway login will stop when its code expires"
                                    .into(),
                            ));
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

pub(super) fn gateway_error(error: mobius_gateway::Error) -> Error {
    Error::Stopped(error.to_string())
}
