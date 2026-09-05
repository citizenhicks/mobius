//! Ephemeral voice calls belong to the authenticated connection that opened them.

use std::collections::VecDeque;

use mobius::backend::model::{
    RealtimeVoiceCall, RealtimeVoiceCommand, RealtimeVoiceEvent, RealtimeVoiceRequest,
};
use mobius::middleware::messages::voice::transcript::VoiceTranscript;
use mobius::middleware::messages::voice::{
    VoiceConversation, handoff_tool, instructions, progress, reject_handoff, resolve_task,
};
use mobius::protocol::{EventMsg, TokenUsage};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use super::*;

pub(super) struct ConnectionVoice {
    pub(super) session_id: String,
    pub(super) voice_id: String,
    updates: mpsc::Receiver<ServerMessage>,
    task: JoinHandle<()>,
    stop: Option<oneshot::Sender<()>>,
}

impl ConnectionVoice {
    pub(super) fn start(host: HostHandle, request_id: String, offer_sdp: String) -> Self {
        let session_id = host.session_id().to_owned();
        let (updates, receiver) = mpsc::channel(2);
        let id = request_id.clone();
        let session = session_id.clone();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut events = host.subscribe();
            let started = async {
                let lease = host.claim_realtime_voice().map_err(rejected)?;
                let model = host.realtime_model().await.map_err(rejected)?;
                let transcript = VoiceTranscript::open(
                    Arc::clone(&model.checkpoints),
                    &session,
                    Arc::clone(&model.frontend),
                )
                .await?;
                let parent =
                    model.checkpoints.load(&session).await?.ok_or_else(|| {
                        Error::Protocol("voice parent session disappeared".into())
                    })?;
                let voice_context = transcript.task_context().await?;
                let call = model
                    .router
                    .start_realtime_voice(
                        &model.route,
                        RealtimeVoiceRequest {
                            session_id: session.clone(),
                            voice: model.voice.clone(),
                            offer_sdp,
                            instructions: instructions(&parent, &voice_context),
                            handoff_tool: handoff_tool(),
                        },
                    )
                    .await?;
                Ok::<_, Error>((lease, model, call, transcript))
            }
            .await;
            match started {
                Ok((_lease, model, mut call, mut transcript)) => {
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
                    let result = drive(
                        &host,
                        &model,
                        &mut call,
                        &mut transcript,
                        &mut events,
                        stopped,
                    )
                    .await;
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
            stop: Some(stop),
        }
    }

    pub(super) async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}

impl Drop for ConnectionVoice {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
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
    transcript: &mut VoiceTranscript,
    events: &mut broadcast::Receiver<ServerFrame>,
    stopped: oneshot::Receiver<()>,
) -> Result<()> {
    let mut conversation =
        VoiceConversation::new(transcript.session_id().into(), model.active_turn_id.clone());
    let result = drive_conversation(
        host,
        model,
        call,
        transcript,
        events,
        &mut conversation,
        stopped,
    )
    .await;
    let finalized = tokio::time::timeout(Duration::from_secs(4), async {
        transcript.finish().await?;
        Ok::<_, Error>(())
    })
    .await
    .map_err(|_| Error::Protocol("voice transcript finalization timed out".into()))?;
    result.and(finalized)
}

async fn drive_conversation(
    host: &HostHandle,
    model: &crate::host::RealtimeModel,
    call: &mut RealtimeVoiceCall,
    transcript: &mut VoiceTranscript,
    events: &mut broadcast::Receiver<ServerFrame>,
    conversation: &mut VoiceConversation,
    mut stopped: oneshot::Receiver<()>,
) -> Result<()> {
    let mut pending = VecDeque::new();
    let mut resolving = JoinSet::new();
    loop {
        start_next_task(&mut resolving, &mut pending, model, host.session_id());
        let replies = tokio::select! {
            biased;
            () = host.wait_terminated() => return Ok(()),
            _ = &mut stopped => return Ok(()),
            event = events.recv() => {
                let frame = event.map_err(|_| Error::Protocol("voice lost its conversation event stream".into()))?;
                match frame.message {
                    ServerMessage::SessionChanged { .. } => {
                        let current = host.realtime_model().await.map_err(rejected)?;
                        if !Arc::ptr_eq(&model.router, &current.router) || model.route != current.route || model.voice != current.voice {
                            return Ok(());
                        }
                        Vec::new()
                    }
                    ServerMessage::AgentEvent { record, .. } => {
                        if matches!(&record.event.msg, EventMsg::ModelChanged(current) if current.route != model.route) {
                            return Ok(());
                        }
                        let mut commands = conversation.observe(&record.event);
                        if let Some(update) = progress(&record.event.msg) { commands.insert(0, update); }
                        commands
                    }
                    ServerMessage::Error { fatal: true, message, .. } => return Err(Error::Protocol(message)),
                    _ => Vec::new(),
                }
            }
            resolved = resolving.join_next(), if !resolving.is_empty() => {
                let (id, result) = resolved
                    .ok_or_else(|| Error::Protocol("voice task extraction disappeared".into()))?
                    .map_err(|error| Error::Protocol(format!("voice task extraction stopped: {error}")))?;
                match result {
                    Ok((text, usage)) => {
                        host.observe_voice_usage(model.provider_instance.clone(), usage).await.map_err(rejected)?;
                        submit_handoff(host, conversation, id, text).await?
                    }
                    Err(error) => vec![reject_handoff(id, &error.to_string())],
                }
            }
            event = call.events.recv() => {
                let Some(event) = event else { return Ok(()) };
                match event? {
                    RealtimeVoiceEvent::Transcript { id, role, text, complete } => {
                        transcript.record(&id, role, &text, complete).await?;
                        Vec::new()
                    }
                    RealtimeVoiceEvent::Handoff { id, text, needs_context } => {
                        if !needs_context {
                            submit_handoff(host, conversation, id, text).await?
                        } else if pending.len() + resolving.len() >= 32 {
                            vec![reject_handoff(id, "Too many pending voice requests. Please wait for the Bot's results.")]
                        } else {
                            // Final speech prunes old deltas, so a journal cursor cannot freeze context.
                            pending.push_back(PendingVoiceTask {
                                id, text, context: transcript.task_context().await?,
                            });
                            Vec::new()
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

struct PendingVoiceTask {
    id: String,
    text: String,
    context: String,
}

fn start_next_task(
    resolving: &mut JoinSet<(String, Result<(String, TokenUsage)>)>,
    pending: &mut VecDeque<PendingVoiceTask>,
    model: &crate::host::RealtimeModel,
    parent_id: &str,
) {
    if !resolving.is_empty() {
        return;
    }
    let Some(task) = pending.pop_front() else {
        return;
    };
    let checkpoints = Arc::clone(&model.checkpoints);
    let router = Arc::clone(&model.router);
    let route = model.route.clone();
    let parent_id = parent_id.to_owned();
    resolving.spawn(async move {
        let result = async {
            let parent = checkpoints
                .load(&parent_id)
                .await?
                .ok_or_else(|| Error::Protocol("voice parent session disappeared".into()))?;
            Ok(resolve_task(&router, &route, &parent, &task.context, &task.text).await?)
        }
        .await;
        (task.id, result)
    });
}

async fn submit_handoff(
    host: &HostHandle,
    conversation: &mut VoiceConversation,
    id: String,
    text: String,
) -> Result<Vec<RealtimeVoiceCommand>> {
    let Some(submission) = conversation.handoff(id, text)? else {
        return Ok(Vec::new());
    };
    let submission_id = submission.id.clone();
    Ok(match host.submit(submission).await {
        Ok(()) => Vec::new(),
        Err(rejection) => conversation.reject(&submission_id, &rejection.message),
    })
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
