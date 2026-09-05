//! Ephemeral voice calls belong to the authenticated connection that opened them.

use mobius::backend::model::{RealtimeVoiceCall, RealtimeVoiceEvent, RealtimeVoiceRequest};
use mobius::middleware::messages::voice::{INSTRUCTIONS, VoiceConversation, handoff_tool};
use mobius::protocol::EventMsg;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::*;

pub(super) struct ConnectionVoice {
    pub(super) session_id: String,
    pub(super) voice_id: String,
    updates: mpsc::Receiver<ServerMessage>,
    task: JoinHandle<()>,
}

impl ConnectionVoice {
    pub(super) fn start(host: HostHandle, request_id: String, offer_sdp: String) -> Self {
        let session_id = host.session_id().to_owned();
        let (updates, receiver) = mpsc::channel(2);
        let id = request_id.clone();
        let session = session_id.clone();
        let task = tokio::spawn(async move {
            let mut events = host.subscribe();
            let started = async {
                let lease = host.claim_realtime_voice().map_err(rejected)?;
                let model = host.realtime_model().await.map_err(rejected)?;
                let call = model
                    .router
                    .start_realtime_voice(
                        &model.route,
                        RealtimeVoiceRequest {
                            session_id: session.clone(),
                            offer_sdp,
                            instructions: INSTRUCTIONS.into(),
                            handoff_tool: handoff_tool(),
                        },
                    )
                    .await?;
                Ok::<_, Error>((lease, model, call))
            }
            .await;
            match started {
                Ok((_lease, model, mut call)) => {
                    if updates
                        .send(ServerMessage::RealtimeVoiceStarted {
                            request_id: id.clone(),
                            session_id: session.clone(),
                            voice_id: id.clone(),
                            answer_sdp: call.answer_sdp.clone(),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let result = drive(&host, &model, &mut call, &mut events).await;
                    let _ = updates
                        .send(ServerMessage::RealtimeVoiceEnded {
                            session_id: session,
                            voice_id: id,
                            reason: result.err().map(|error| error.to_string()),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = updates
                        .send(ServerMessage::RealtimeVoiceFailed {
                            request_id: id,
                            session_id: session,
                            message: error.to_string(),
                        })
                        .await;
                }
            }
        });
        Self {
            session_id,
            voice_id: request_id,
            updates: receiver,
            task,
        }
    }

    pub(super) async fn stop(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for ConnectionVoice {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn rejected(rejection: Rejection) -> Error {
    Error::Protocol(rejection.message)
}

pub(super) async fn handle_message(
    message: ClientMessage,
    connection: &mut super::dispatch::ConnectionSessionState<'_>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<Option<ClientMessage>> {
    if matches!(&message, ClientMessage::CreateSession { .. })
        || matches!(&message, ClientMessage::OpenSession { session_id, .. }
            if connection.voice.as_ref().is_some_and(|voice| voice.session_id != *session_id))
    {
        end(connection.voice).await;
    }
    match message {
        ClientMessage::StartRealtimeVoice {
            request_id,
            session_id,
            offer_sdp,
        } => {
            let result = require_selected(connection.selected, &session_id)
                .cloned()
                .and_then(|host| {
                    if uuid::Uuid::parse_str(&request_id).is_err()
                        || offer_sdp.is_empty()
                        || offer_sdp.len() > 64 * 1024
                    {
                        return Err(Rejection {
                            code: "realtime_voice",
                            message: "invalid voice connection request".into(),
                            fatal: false,
                        });
                    }
                    Ok(host)
                });
            match result {
                Ok(host) => {
                    end(connection.voice).await;
                    *connection.voice = Some(ConnectionVoice::start(host, request_id, offer_sdp));
                    Ok(None)
                }
                Err(rejection) => write_frame(
                    writer,
                    &ServerFrame::new(ServerMessage::RealtimeVoiceFailed {
                        request_id,
                        session_id,
                        message: rejection.message,
                    }),
                )
                .await
                .map(|()| None),
            }
        }
        ClientMessage::EndRealtimeVoice {
            session_id,
            voice_id,
        } => {
            if connection
                .voice
                .as_ref()
                .is_some_and(|voice| voice.session_id == session_id && voice.voice_id == voice_id)
            {
                end(connection.voice).await;
                write_frame(
                    writer,
                    &ServerFrame::new(ServerMessage::RealtimeVoiceEnded {
                        session_id,
                        voice_id,
                        reason: None,
                    }),
                )
                .await?;
            }
            Ok(None)
        }
        _ => Ok(Some(message)),
    }
}

pub(super) async fn next_update(voice: &mut Option<ConnectionVoice>) -> Option<ServerMessage> {
    match voice {
        Some(voice) => voice.updates.recv().await,
        None => std::future::pending().await,
    }
}

pub(super) async fn write_update(
    voice: &mut Option<ConnectionVoice>,
    message: Option<ServerMessage>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let terminal = !matches!(&message, Some(ServerMessage::RealtimeVoiceStarted { .. }));
    if let Some(message) = message {
        write_frame(writer, &ServerFrame::new(message)).await?;
    }
    if terminal {
        end(voice).await;
    }
    Ok(())
}

pub(super) async fn end(voice: &mut Option<ConnectionVoice>) {
    if let Some(mut voice) = voice.take() {
        voice.stop().await;
    }
}

async fn drive(
    host: &HostHandle,
    model: &crate::host::RealtimeModel,
    call: &mut RealtimeVoiceCall,
    events: &mut broadcast::Receiver<ServerFrame>,
) -> Result<()> {
    let mut conversation = VoiceConversation::new(model.active_turn_id.clone());
    loop {
        let replies = tokio::select! {
            biased;
            () = host.wait_terminated() => return Ok(()),
            event = events.recv() => {
                let frame = event.map_err(|_| Error::Protocol("voice lost its conversation event stream".into()))?;
                match frame.message {
                    ServerMessage::SessionChanged { .. } => {
                        let current = host.realtime_model().await.map_err(rejected)?;
                        if !Arc::ptr_eq(&model.router, &current.router) || model.route != current.route {
                            return Ok(());
                        }
                        Vec::new()
                    }
                    ServerMessage::AgentEvent { record, .. } => {
                        if matches!(&record.event.msg, EventMsg::ModelChanged(current) if current.route != model.route) {
                            return Ok(());
                        }
                        conversation.observe(&record.event)
                    }
                    ServerMessage::Error { fatal: true, message, .. } => return Err(Error::Protocol(message)),
                    _ => Vec::new(),
                }
            }
            event = call.events.recv() => {
                let Some(event) = event else { return Ok(()) };
                match event? {
                    RealtimeVoiceEvent::Handoff { id, text } => {
                        let Some(submission) = conversation.handoff(id, text)? else { continue };
                        let submission_id = submission.id.clone();
                        match host.submit(submission).await {
                            Ok(()) => Vec::new(),
                            Err(rejection) => conversation.reject(&submission_id, &rejection.message).into_iter().collect(),
                        }
                    }
                    RealtimeVoiceEvent::Usage(usage) => {
                        host.observe_voice_usage(model.provider_instance.clone(), usage).await.map_err(rejected)?;
                        Vec::new()
                    }
                }
            }
        };
        for reply in replies {
            tokio::time::timeout(Duration::from_secs(10), call.commands.send(reply))
                .await
                .map_err(|_| Error::Protocol("voice reply timed out".into()))?
                .map_err(|_| {
                    Error::Protocol("voice connection stopped accepting replies".into())
                })?;
        }
    }
}
