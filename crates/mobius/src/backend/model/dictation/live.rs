//! OpenAI-compatible realtime transcription. One recording is one manually committed turn.
use base64::engine::general_purpose::STANDARD;
use reqwest::Url;
use serde_json::Value;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::provider::MAX_TRANSCRIPT_BYTES;
use super::{DictationAudio, DictationProvider};
use crate::backend::model::authorization::OpenAiAuthorization;
use crate::backend::model::responses;

use crate::backend::model::transport::{self, ModelWebSocket, WebSocketConnectError};
use crate::{Error, Result};

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    delta: Option<String>,
    transcript: Option<String>,
}

impl DictationProvider {
    pub(super) async fn live(
        &self,
        model: &str,
        language: Option<&str>,
        mut audio: DictationAudio,
        updates: mpsc::Sender<String>,
    ) -> Result<String> {
        use futures_util::StreamExt as _;
        let path = self
            .api
            .and_then(|api| api.realtime.as_deref())
            .ok_or_else(|| Error::Invalid("provider has no live transcription endpoint".into()))?;
        let url = Url::parse(&self.base_url.operation_url(path))
            .map_err(|_invalid| Error::Invalid("invalid dictation endpoint".into()))?;
        let url = transport::socket_url(url)?;
        let headers: Vec<_> = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut socket = transport::connect_authorized_websocket(
            self.auth.as_ref().map(|a| a as &dyn OpenAiAuthorization),
            url.as_str(),
            MAX_TRANSCRIPT_BYTES * 2,
            Some(Duration::from_millis(
                self.settings.socket_connect_timeout_ms,
            )),
            |auth| auth.authorize_http(true, None),
            &headers,
        )
        .await
        .map_err(connect_error)?;
        let transcription = match language {
            Some(lang) => json!({ "model": model, "languages": [lang] }),
            None => json!({ "model": model }),
        };
        let write_timeout = Duration::from_millis(self.settings.socket_io_timeout_ms);
        let session = json!({
            "type": "session.update",
            "session": {
                "type": "transcription",
                "audio": {
                    "input": {
                        "format": { "type": "audio/pcm", "rate": 24000 },
                        "transcription": transcription,
                        "turn_detection": null
                    }
                }
            }
        });
        send(&mut socket, session, write_timeout).await?;
        let mut partial = String::new();
        let mut committed = false;
        let mut received_audio = false;
        let idle = Duration::from_millis(self.settings.socket_idle_timeout_ms);
        loop {
            tokio::select! {
                frame = socket.next() => {
                    let Some(Ok(frame)) = frame else {
                        return Err(Error::Provider("live dictation connection closed before final text".into()));
                    };
                    match frame {
                        Message::Text(frame) => {
                            let event: Event<'_> = serde_json::from_str(&frame)
                                .map_err(|_error| Error::Provider("invalid live transcription response".into()))?;
                            match event.kind {
                                "conversation.item.input_audio_transcription.delta" => {
                                    if let Some(delta) = event.delta {
                                        if partial.len() + delta.len() > MAX_TRANSCRIPT_BYTES {
                                            return Err(Error::Provider("live transcript is too long".into()));
                                        }
                                        partial.push_str(&delta);
                                        // A slow view can omit an intermediate snapshot. The next update is cumulative.
                                        if let Ok(slot) = updates.try_reserve() {
                                            slot.send(partial.clone()); // rules-allow: the partial snapshot is delivered while later deltas extend this buffer
                                        }
                                    }
                                }
                                "conversation.item.input_audio_transcription.completed" => {
                                    if !committed {
                                        return Err(Error::Provider("transcription ended before the audio was committed".into()));
                                    }
                                    return event.transcript
                                        .filter(|text| text.len() <= MAX_TRANSCRIPT_BYTES)
                                        .ok_or_else(|| Error::Provider("invalid final transcript".into()));
                                }
                                "error" | "conversation.item.input_audio_transcription.failed" => {
                                    let detail = serde_json::from_str(&frame)?;
                                    return Err(Error::Provider(responses::response_provider_error(&detail, None)));
                                }
                                _ => {}
                            }
                        }
                        Message::Close(_) => return Err(Error::Provider("live dictation session ended early".into())),
                        _ => {}
                    }
                }
                chunk = audio.0.recv(), if !committed => {
                    match chunk {
                        Some(bytes) => {
                            use base64::Engine as _;
                            received_audio = true;
                            let encoded = STANDARD.encode(bytes);
                            send(
                                &mut socket,
                                json!({ "type": "input_audio_buffer.append", "audio": encoded }),
                                write_timeout,
                            ).await?;
                        }
                        None if !received_audio => return Ok(String::new()),
                        None => {
                            send(
                                &mut socket,
                                json!({ "type": "input_audio_buffer.commit" }),
                                write_timeout,
                            ).await?;
                            committed = true;
                        }
                    }
                }
                () = tokio::time::sleep(idle) => return Err(Error::Provider("live dictation timed out".into()))
            }
        }
    }
}

async fn send(socket: &mut ModelWebSocket, event: Value, limit: Duration) -> Result<()> {
    use futures_util::SinkExt as _;
    tokio::time::timeout(limit, socket.send(Message::Text(event.to_string().into())))
        .await
        .map_err(|_error| Error::Provider("live dictation write timed out".into()))?
        .map_err(|_error| Error::Provider("live dictation write failed".into()))
}

fn connect_error(error: WebSocketConnectError) -> Error {
    match error {
        WebSocketConnectError::Unauthorized(reply) | WebSocketConnectError::Http(reply) => {
            Error::Provider(transport::handshake_error("dictation", &reply))
        }
        WebSocketConnectError::Other(error) => error,
        WebSocketConnectError::InvalidHeader(message) => Error::Invalid(message),
        WebSocketConnectError::TimedOut => {
            Error::Provider("live dictation connection timed out".into())
        }
        WebSocketConnectError::Transport(_) | WebSocketConnectError::Request(_) => {
            Error::Provider("unable to open live dictation connection".into())
        }
    }
}
