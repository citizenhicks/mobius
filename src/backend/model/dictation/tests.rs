use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};

use super::*;
use crate::backend::model::endpoints::ProviderModelKind;
use crate::backend::model::provider::{BaseUrl, ProviderCredential, provider};
use crate::backend::model::test_support::loopback_listener;
use crate::backend::model::transport::ModelTransportSettings;
use std::time::Duration;
use tokio::sync::mpsc;

struct InspectEndpoint;

impl Callback for InspectEndpoint {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        assert_eq!(
            request.uri().path_and_query().unwrap().as_str(),
            "/v1/speech/live?intent=transcription"
        );
        Ok(response)
    }
}

fn adapter(id: &str, address: std::net::SocketAddr) -> DictationProvider {
    DictationProvider::new(
        provider(id).unwrap(),
        BaseUrl::new(format!("http://{address}/v1")).unwrap(),
        ProviderCredential::ApiKey("fixture-only".into()),
        &ModelTransportSettings::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn submits_recording_with_model_attribution_and_language() {
    let (listener, address) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut fragment = [0; 2048];
        loop {
            let count = conn.read(&mut fragment).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend(&fragment[..count]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|h| h.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let request = String::from_utf8_lossy(&bytes);
        assert!(request.starts_with("POST /v1/audio/transcriptions "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-only")
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("http-referer: https://github.com/citizenhicks/mobius")
        );
        for field in [
            "name=\"language\"\r\n\r\nfr",
            "name=\"model\"\r\n\r\nnvidia/nemotron-3.5-transcribe",
            "filename=\"dictation.wav\"",
        ] {
            assert!(request.contains(field));
        }
        let riff = bytes.windows(4).position(|w| w == b"RIFF").unwrap();
        let wav = &bytes[riff..riff + 50];
        assert_eq!(&wav[4..8], &42_u32.to_le_bytes());
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(&wav[24..28], &24_000_u32.to_le_bytes());
        assert_eq!(&wav[36..], b"data\x06\0\0\0\0\0\x01\0\x02\0");
        conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 18\r\n\r\n{\"text\":\"Bonjour\"}")
            .await
            .unwrap();
    });
    let (mut sender, audio) = DictationInput::channel();
    sender.try_send(vec![0, 0, 1, 0]).unwrap();
    sender.try_send(vec![2, 0]).unwrap();
    drop(sender);
    let result = adapter("openrouter", address)
        .transcribe(
            "nvidia/nemotron-3.5-transcribe",
            Some(&DictationLanguage::new("fr-FR".into()).unwrap()),
            audio,
            mpsc::channel(1).0,
        )
        .await;
    server.await.unwrap();
    assert_eq!(result.unwrap(), "Bonjour");
}

#[test]
fn explicit_native_recording_path_inherits_the_model_mode() {
    let definition = provider("responses").unwrap();
    for (path, expected) in [
        ("audio/transcriptions", DictationMode::Live),
        ("/audio/transcriptions", DictationMode::Live),
        ("/proxy/transcriptions", DictationMode::AfterStop),
    ] {
        let bound = definition
            .with_group_endpoint(
                ProviderModelKind::Dictation,
                definition.default_base_url(),
                "transcriptions",
                path,
            )
            .unwrap();
        assert_eq!(
            definition.dictation_mode("gpt-live-transcribe", &bound),
            Some(expected)
        );
        assert_eq!(
            definition.dictation_mode("whisper-1", &bound),
            Some(DictationMode::AfterStop)
        );
    }
}

#[tokio::test]
async fn explicit_realtime_adapter_applies_to_a_custom_model() {
    let (listener, address) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (conn, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(conn, InspectEndpoint)
            .await
            .unwrap();
        let session: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(
            session["session"]["audio"]["input"]["transcription"],
            json!({"model":"custom-transcription","languages":["de"]})
        );
        let append: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(
            append,
            json!({"type":"input_audio_buffer.append","audio":"AQACAA=="})
        );
        for delta in ["Gut", "en"] {
            ws.send(Message::text(
                json!({"type":"conversation.item.input_audio_transcription.delta","delta":delta})
                    .to_string(),
            ))
            .await
            .unwrap();
        }
        let commit: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(commit, json!({"type":"input_audio_buffer.commit"}));
        ws.send(Message::text(json!({"type":"conversation.item.input_audio_transcription.completed","transcript":"Guten Tag."}).to_string())).await.unwrap();
    });
    let (mut producer, audio) = DictationInput::channel();
    let (updates, mut visible) = mpsc::channel(2);
    let client = tokio::spawn(async move {
        let definition = provider("responses").unwrap();
        let base = BaseUrl::new(format!("http://{address}/v1")).unwrap();
        let operation = definition
            .with_group_endpoint(
                ProviderModelKind::Dictation,
                &base,
                "realtime_transcription",
                "speech/live",
            )
            .unwrap();
        assert_eq!(
            definition.dictation_mode("custom-transcription", &operation),
            Some(DictationMode::Live)
        );
        DictationProvider::new(
            definition,
            operation,
            ProviderCredential::ApiKey("fixture".into()),
            &ModelTransportSettings::default(),
        )
        .unwrap()
        .transcribe(
            "custom-transcription",
            Some(&DictationLanguage::new("de-DE".into()).unwrap()),
            audio,
            updates,
        )
        .await
    });
    producer.try_send(vec![1, 0, 2, 0]).unwrap();
    for expected in ["Gut", "Guten"] {
        let partial = tokio::time::timeout(Duration::from_secs(5), visible.recv())
            .await
            .unwrap();
        assert_eq!(partial.as_deref(), Some(expected));
    }
    drop(producer);
    server.await.unwrap();
    assert_eq!(client.await.unwrap().unwrap(), "Guten Tag.");
}

#[tokio::test]
async fn aborted_or_invalid_recording_never_contacts_provider() {
    let (listener, address) = loopback_listener().await;
    let provider = adapter("openrouter", address);
    let (mut sender, audio) = DictationInput::channel();
    for bad in [vec![], vec![0], vec![0; MAX_CHUNK_BYTES + 2]] {
        assert!(sender.try_send(bad).is_err());
    }
    drop(sender);
    assert_eq!(
        provider
            .transcribe("test", None, audio, mpsc::channel(1).0)
            .await
            .unwrap(),
        ""
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn live_provider_errors_keep_their_failure_class() {
    let (listener, address) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (conn, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(conn).await.unwrap();
        ws.next().await.unwrap().unwrap();
        ws.send(Message::text(json!({
            "type": "conversation.item.input_audio_transcription.failed",
            "error": {"type": "invalid_request_error", "code": "expired_secret_key", "message": "expired"}
        }).to_string())).await.unwrap();
    });
    let (_source, audio) = DictationInput::channel();
    let error = adapter("responses", address)
        .transcribe("gpt-live-transcribe", None, audio, mpsc::channel(1).0)
        .await
        .unwrap_err();
    assert_eq!(error.failure(), Some(crate::FailureClass::Authentication));
    server.await.unwrap();
}

#[test]
fn recording_budget_counts_only_accepted_frames() {
    let (mut source, mut audio) = DictationInput::channel();
    let mut accepted = 0;
    while source.try_send(vec![0, 0]).is_ok() {
        accepted += 2;
    }
    while audio.0.try_recv().is_ok() {}
    let remaining = MAX_RECORDING_BYTES - accepted;
    for _ in 0..remaining / MAX_CHUNK_BYTES {
        source.try_send(vec![0; MAX_CHUNK_BYTES]).unwrap();
        audio.0.try_recv().unwrap();
    }
    source
        .try_send(vec![0; remaining % MAX_CHUNK_BYTES])
        .unwrap();
    audio.0.try_recv().unwrap();
    assert!(source.try_send(vec![0, 0]).is_err());
}
