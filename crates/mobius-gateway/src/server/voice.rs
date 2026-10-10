//! Ephemeral voice calls belong to the authenticated connection that opened them.

use std::future::Future;

use mobius::middleware::voice::VoiceCall;
use mobius::protocol::EventMsg;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::*;

pub(super) struct ConnectionVoice {
    pub(super) session_id: String,
    pub(super) voice_id: String,
    updates: mpsc::Receiver<ServerMessage>,
    task: JoinHandle<()>,
    stop: Option<oneshot::Sender<()>>,
    shutdown_timeout: oneshot::Receiver<Duration>,
}

impl ConnectionVoice {
    pub(super) fn start(host: HostHandle, request_id: String, offer_sdp: String) -> Self {
        let session_id = host.session_id().to_owned();
        let (updates, receiver) = mpsc::channel(2);
        let id = request_id.clone();
        let session = session_id.clone();
        let (stop, mut stopped) = oneshot::channel();
        let (shutdown, shutdown_timeout) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut events = host.subscribe();
            let started = async {
                let lease = host.claim_realtime_voice().map_err(rejected)?;
                let model = host.realtime_model().await.map_err(rejected)?;
                let _ = shutdown.send(model.call.shutdown_timeout()?);
                let (call, answer_sdp) = VoiceCall::start(model.call, offer_sdp).await?;
                Ok::<_, Error>((lease, model.route, call, answer_sdp))
            };
            let Some(started) = startup(&mut stopped, started).await else {
                return;
            };
            match started {
                Ok((_lease, route, call, answer_sdp)) => {
                    let Ok(()) = updates
                        .send(ServerMessage::RealtimeVoiceStarted {
                            request_id: id.clone(),
                            session_id: session.clone(),
                            voice_id: id.clone(),
                            answer_sdp,
                        })
                        .await
                    else {
                        let _ = call.close(Ok::<_, Error>(())).await;
                        return;
                    };
                    let result = drive(&host, &route, call, &mut events, stopped).await;
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
            shutdown_timeout,
        }
    }

    pub(super) async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // The policy is sent before provider startup; an absent value means startup was cancelled.
        let deadline =
            self.shutdown_timeout.try_recv().unwrap_or_default() + Duration::from_secs(1);
        if tokio::time::timeout(deadline, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}

async fn startup<T>(
    stopped: &mut oneshot::Receiver<()>,
    started: impl Future<Output = Result<T>>,
) -> Option<Result<T>> {
    tokio::select! {
        biased;
        _ = &mut *stopped => None,
        started = started => Some(started),
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
        } => match require_selected(connection.selected, &session_id).cloned() {
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
        },
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
    route: &str,
    mut call: VoiceCall,
    events: &mut broadcast::Receiver<SharedFrame>,
    mut stopped: oneshot::Receiver<()>,
) -> Result<()> {
    let result = async {
        loop {
            tokio::select! {
                biased;
                () = host.wait_terminated() => return Ok(()),
                _ = &mut stopped => return Ok(()),
                event = events.recv() => {
                    let frame = event.map_err(|_| Error::Protocol("voice lost its conversation event stream".into()))?;
                    match &frame.message {
                        ServerMessage::SessionChanged { .. } => {
                            let current = host.realtime_model().await.map_err(rejected)?;
                            if !call.serves(&current.call) || route != current.route {
                                return Ok(());
                            }
                        }
                        ServerMessage::AgentEvent { record, .. } => {
                            if matches!(&record.event.msg, EventMsg::ModelChanged(current) if current.route != route) {
                                return Ok(());
                            }
                            call.observe(&record.event).await?;
                        }
                        ServerMessage::Error { fatal: true, message, .. } => return Err(Error::Protocol(message.clone())),
                        _ => {}
                    }
                }
                wake = call.wait() => {
                    let submit = async |submission| host.submit_validated(submission).await.map_err(|rejection| rejection.message);
                    if !call.handle(wake, submit).await? {
                        return Ok(());
                    }
                }
            }
        }
    }
    .await;
    call.close(result).await
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
