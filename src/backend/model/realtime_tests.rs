use super::*;
use crate::BoxFuture;
use crate::backend::model::Model;
use crate::backend::model::openai_auth::ResolvedAuthorization;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SDP: &str = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";

fn request() -> RealtimeVoiceRequest {
    RealtimeVoiceRequest {
        session_id: "session-1".into(),
        voice: None,
        offer_sdp: SDP.into(),
        instructions: "Delegate coding requests using ask_agent.".into(),
        handoff_tool: ToolDefinition {
            name: "ask_agent".into(),
            description: "Ask the coding agent.".into(),
            parameters: crate::middleware::messages::voice::handoff_tool().parameters,
        },
    }
}

#[derive(Default)]
struct Auth(AtomicBool);

impl OpenAiAuthorization for Auth {
    fn authorize_http<'a>(
        &'a self,
        streaming: bool,
        session: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization>> {
        assert!(!streaming);
        assert_eq!(session, Some("session-1"));
        Box::pin(async move {
            Ok(ResolvedAuthorization {
                token: if self.0.load(Ordering::SeqCst) {
                    "renewed"
                } else {
                    "secret"
                }
                .into(),
                headers: vec![("chatgpt-account-id".into(), "account-1".into())],
            })
        })
    }
    fn authorize_websocket<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization>> {
        panic!("voice must use call-create authorization, without Responses socket headers")
    }
    fn recover_unauthorized<'a>(&'a self, rejected: &'a str) -> BoxFuture<'a, Result<bool>> {
        assert_eq!(rejected, "secret");
        self.0.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(true) })
    }
}

async fn transport(api: VoiceApi) -> (RealtimeTransport, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1/realtime", listener.local_addr().unwrap());
    let mut calls_url = Url::parse(&format!("{base}/calls")).unwrap();
    if api == VoiceApi::Codex {
        calls_url.set_query(Some("intent=quicksilver&architecture=avas"));
    }
    (
        RealtimeTransport {
            api,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            auth: Arc::new(Auth::default()),
            calls_url,
            api_url: Url::parse(&if api == VoiceApi::Codex {
                base.replace("/realtime", "/live")
            } else {
                base
            })
            .unwrap(),
        },
        listener,
    )
}

struct InspectUpgrade(VoiceApi);

impl tokio_tungstenite::tungstenite::handshake::server::Callback for InspectUpgrade {
    fn on_request(
        self,
        request: &tokio_tungstenite::tungstenite::handshake::server::Request,
        response: tokio_tungstenite::tungstenite::handshake::server::Response,
    ) -> std::result::Result<
        tokio_tungstenite::tungstenite::handshake::server::Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        assert_eq!(request.headers()["authorization"], "Bearer secret");
        assert_eq!(request.headers()["chatgpt-account-id"], "account-1");
        assert!(!request.headers().contains_key("openai-beta"));
        if self.0 == VoiceApi::Codex {
            assert_eq!(request.uri().path(), "/v1/live/rtc_test");
            assert_eq!(request.uri().query(), None);
            assert_eq!(request.headers()["openai-alpha"], "quicksilver=v2");
            assert_eq!(request.headers()["x-session-id"], "session-1");
        } else {
            assert_eq!(request.uri().query(), Some("call_id=rtc_test"));
            assert!(!request.headers().contains_key("openai-alpha"));
        }
        Ok(response)
    }
}

async fn read_request(socket: &mut TcpStream) -> (String, String) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buf = [0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut buf = [0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&buf[..n]);
    }
    (
        headers.to_lowercase(),
        String::from_utf8(bytes[header_end..].to_vec()).unwrap(),
    )
}

async fn respond(socket: &mut TcpStream, status: &str, extra: &str, body: &str) {
    socket
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{extra}\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

fn completed() -> Value {
    json!({"type":"response.done","response":{"id":"r1","status":"completed","output":[
        {"type":"function_call","name":"ask_agent","call_id":"h1","arguments":"{\"text\":\"model paraphrase\"}"}
    ]}})
}

fn transcript() -> Value {
    json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"i1","transcript":"Fix the actual bug, please."})
}

#[test]
fn self_contained_handoffs_and_actual_asr_are_independent_and_deduplicated() {
    for asr_first in [true, false] {
        let mut turns = VoiceTurns::default();
        for event in [
            json!({"type":"input_audio_buffer.committed","item_id":"i1"}),
            json!({"type":"response.created","response":{"id":"r1"}}),
        ] {
            assert!(
                turns
                    .observe(VoiceApi::OpenAi, "ask_agent", &event)
                    .unwrap()
                    .is_empty()
            );
        }
        let pair = if asr_first {
            [transcript(), completed()]
        } else {
            [completed(), transcript()]
        };
        let mut observed = turns
            .observe(VoiceApi::OpenAi, "ask_agent", &pair[0])
            .unwrap();
        observed.extend(
            turns
                .observe(VoiceApi::OpenAi, "ask_agent", &pair[1])
                .unwrap(),
        );
        if !asr_first {
            observed.reverse();
        }
        assert_eq!(
            observed,
            vec![
                RealtimeVoiceEvent::Transcript {
                    id: "i1".into(),
                    role: crate::protocol::ConversationRole::User,
                    text: "Fix the actual bug, please.".into(),
                    complete: true
                },
                RealtimeVoiceEvent::Handoff {
                    id: "h1".into(),
                    text: "model paraphrase".into(),
                    needs_context: false,
                },
            ]
        );
        for event in pair {
            assert!(
                turns
                    .observe(VoiceApi::OpenAi, "ask_agent", &event)
                    .unwrap()
                    .is_empty()
            );
        }
    }
    let mut turns = VoiceTurns::default();
    let mut cancelled = completed();
    cancelled["response"]["status"] = "cancelled".into();
    assert!(
        turns
            .observe(VoiceApi::OpenAi, "ask_agent", &cancelled)
            .unwrap()
            .is_empty()
    );
    assert!(
        turns
            .observe(
                VoiceApi::OpenAi,
                "ask_agent",
                &json!({"type":"error","error":{"message":"voice access denied"}})
            )
            .unwrap_err()
            .to_string()
            .contains("voice access denied")
    );
}

#[tokio::test]
async fn public_and_codex_calls_use_authenticated_sideband_and_hang_up_on_drop() {
    for api in [VoiceApi::OpenAi, VoiceApi::Codex] {
        let (transport, listener) = transport(api).await;
        let handoff_count = if api == VoiceApi::OpenAi { 9 } else { 1 };
        let reply_text = if api == VoiceApi::Codex {
            format!("x{}", "🗣".repeat(300))
        } else {
            "Fixed it.".into()
        };
        let expected_reply = reply_text.clone();
        let (reply_ready, replied) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut http, _) = listener.accept().await.unwrap();
            let (headers, body) = read_request(&mut http).await;
            assert!(headers.contains("authorization: bearer secret\r\n"));
            assert!(headers.contains("chatgpt-account-id: account-1\r\n"));
            if api == VoiceApi::Codex {
                assert!(
                    headers.starts_with(
                        "post /v1/realtime/calls?intent=quicksilver&architecture=avas "
                    )
                );
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["sdp"], SDP);
                assert!(headers.contains("openai-alpha: quicksilver=v2\r\n"));
                assert_eq!(body["session"]["delegation"]["type"], "client");
                assert!(body["session"].get("type").is_none());
                assert_eq!(body["session"]["audio"]["output"]["voice"], "maple");
                assert!(body["session"].get("tools").is_none());
                assert_eq!(body["session"]["model"], "gpt-live-1-codex");
            } else {
                assert!(headers.contains("multipart/form-data; boundary=mobius-"));
                assert!(body.contains(SDP));
                assert!(body.contains("\"model\":\"gpt-realtime-2.1-mini\""));
                assert!(body.contains("\"transcription\":{\"model\":\"gpt-live-transcribe\"}"));
                assert!(body.contains("\"name\":\"ask_agent\""));
                assert!(body.contains("\"voice\":\"ash\""));
                assert!(body.contains("\"interrupt_response\":true"));
            }
            respond(
                &mut http,
                "201 Created",
                if api == VoiceApi::Codex {
                    "Location: /v1/realtime/calls/calls/rtc_test\r\n"
                } else {
                    "Location: /v1/realtime/calls/rtc_test\r\n"
                },
                SDP,
            )
            .await;
            drop(http);
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(socket, InspectUpgrade(api))
                .await
                .unwrap();
            if api == VoiceApi::Codex {
                for _ in 0..2 {
                    socket.send(Message::text(json!({"type":"delegation.created","offset_ms":1000,"item":{"id":"h1","type":"delegation","target":"client","content":[{"type":"input_text","text":"Fix the actual "},{"type":"input_text","text":"bug, please."}]}}).to_string())).await.unwrap();
                }
            } else {
                for n in 1..=handoff_count {
                    let mut done = completed();
                    done["response"]["id"] = format!("r{n}").into();
                    done["response"]["output"][0]["call_id"] = format!("h{n}").into();
                    let mut asr = transcript();
                    asr["item_id"] = format!("i{n}").into();
                    for event in [
                        json!({"type":"input_audio_buffer.committed","item_id":format!("i{n}")}),
                        json!({"type":"response.created","response":{"id":format!("r{n}")}}),
                        done.clone(),
                        asr,
                        done,
                    ] {
                        socket.send(Message::text(event.to_string())).await.unwrap();
                    }
                }
            }
            let context: Value =
                serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(
                context,
                if api == VoiceApi::Codex {
                    json!({"type":"session.context.append","channel":"commentary","content":[{"type":"input_text","text":"Bot is running tests."}]})
                } else {
                    json!({"type":"conversation.item.create","item":{"type":"message","role":"system","content":[{"type":"input_text","text":"Bot is running tests."}]}})
                }
            );
            let reply: Value =
                serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            if api == VoiceApi::Codex {
                let mut full_reply = String::new();
                let mut reply = reply;
                loop {
                    assert_eq!(reply["type"], "delegation.context.append");
                    assert_eq!(reply["delegation_item_id"], "h1");
                    assert_eq!(reply["channel"], "speakable");
                    assert_eq!(reply["content"][0]["type"], "input_text");
                    let text = reply["content"][0]["text"].as_str().unwrap();
                    assert!(text.len() <= 500);
                    full_reply.push_str(text);
                    if full_reply.len() >= expected_reply.len() {
                        break;
                    }
                    reply = serde_json::from_str(
                        socket.next().await.unwrap().unwrap().to_text().unwrap(),
                    )
                    .unwrap();
                }
                assert_eq!(full_reply, expected_reply);
            } else {
                assert_eq!(
                    reply,
                    json!({"type":"conversation.item.create","item":{"type":"function_call_output","call_id":"h1","output":"Fixed it."}})
                );
                for n in 2..=handoff_count {
                    let reply: Value = serde_json::from_str(
                        socket.next().await.unwrap().unwrap().to_text().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        reply,
                        json!({"type":"conversation.item.create","item":{"type":"function_call_output","call_id":format!("h{n}"),"output":"Fixed it."}})
                    );
                }
                let response: Value =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(
                    response,
                    json!({"type":"response.create","response":{"tool_choice":"none"}})
                );
            }
            reply_ready.send(()).unwrap();
            if api == VoiceApi::Codex {
                let close: Value =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(close, json!({"type":"session.close"}));
            }
            assert!(matches!(
                socket.next().await.unwrap().unwrap(),
                Message::Close(_)
            ));
            let (mut http, _) = listener.accept().await.unwrap();
            let (headers, body) = read_request(&mut http).await;
            assert!(headers.starts_with("post /v1/realtime/calls/rtc_test/hangup "));
            assert!(headers.contains("authorization: bearer secret\r\n"));
            assert!(body.is_empty());
            respond(&mut http, "200 OK", "", "").await;
        });
        let mut selected = request();
        selected.voice = Some(
            if api == VoiceApi::Codex {
                "maple"
            } else {
                "ash"
            }
            .into(),
        );
        let mut call = transport.start(selected).await.unwrap();
        assert_eq!(call.answer_sdp, SDP);
        for n in 1..=handoff_count {
            assert_eq!(
                timeout(Duration::from_secs(2), call.events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                RealtimeVoiceEvent::Handoff {
                    id: format!("h{n}"),
                    needs_context: api == VoiceApi::Codex,
                    text: if api == VoiceApi::Codex {
                        "Fix the actual bug, please."
                    } else {
                        "model paraphrase"
                    }
                    .into()
                }
            );
            if api == VoiceApi::OpenAi {
                assert_eq!(
                    timeout(Duration::from_secs(2), call.events.recv())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap(),
                    RealtimeVoiceEvent::Transcript {
                        id: format!("i{n}"),
                        role: crate::protocol::ConversationRole::User,
                        text: "Fix the actual bug, please.".into(),
                        complete: true
                    }
                );
            }
        }
        call.commands
            .send(RealtimeVoiceCommand::Context {
                text: "Bot is running tests.".into(),
            })
            .await
            .unwrap();
        for n in 1..=handoff_count {
            call.commands
                .send(RealtimeVoiceCommand::Reply {
                    handoff_id: format!("h{n}"),
                    text: reply_text.clone(),
                })
                .await
                .unwrap();
        }
        timeout(Duration::from_secs(2), replied)
            .await
            .unwrap()
            .unwrap();
        assert!(call.events.try_recv().is_err());
        drop(call);
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn unauthorized_refresh_is_reused_and_access_rejection_is_preserved() {
    let (transport, listener) = transport(VoiceApi::Codex).await;
    let server = tokio::spawn(async move {
        for (token, status) in [("secret", "401 Unauthorized"), ("renewed", "403 Forbidden")] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, _) = read_request(&mut socket).await;
            assert!(headers.contains(&format!("authorization: bearer {token}\r\n")));
            respond(
                &mut socket,
                status,
                "",
                "{\"error\":{\"message\":\"voice access denied\"}}",
            )
            .await;
        }
    });
    let Err(Error::Provider(error)) = transport.start(request()).await else {
        panic!("expected provider denial")
    };
    assert_eq!(error.status(), Some(403));
    assert!(error.to_string().contains("voice access denied"));
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn cancelling_sideband_setup_hangs_up_allocated_call() {
    let (transport, listener) = transport(VoiceApi::OpenAi).await;
    let (connecting, connected) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        respond(
            &mut socket,
            "201 Created",
            "Location: /v1/realtime/calls/rtc_cancel\r\n",
            SDP,
        )
        .await;
        drop(socket);
        let (socket, _) = listener.accept().await.unwrap();
        connecting.send(()).unwrap();
        let (mut hangup, _) = listener.accept().await.unwrap();
        let (headers, _) = read_request(&mut hangup).await;
        assert!(headers.starts_with("post /v1/realtime/calls/rtc_cancel/hangup "));
        respond(&mut hangup, "200 OK", "", "").await;
        drop(socket);
    });
    let setup = tokio::spawn(async move { transport.start(request()).await });
    timeout(Duration::from_secs(2), connected)
        .await
        .unwrap()
        .unwrap();
    setup.abort();
    assert!(matches!(setup.await, Err(error) if error.is_cancelled()));
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn foreign_call_locations_cannot_redirect_credentials() {
    let (transport, listener) = transport(VoiceApi::OpenAi).await;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        respond(
            &mut socket,
            "201 Created",
            "Location: https://attacker.example/v1/realtime/calls/rtc_stolen\r\n",
            SDP,
        )
        .await;
    });
    let result = transport.start(request()).await;
    assert!(matches!(result, Err(error) if error.to_string().contains("Location")));
    server.await.unwrap();
}

#[tokio::test]
async fn sideband_access_rejection_hangs_up_the_allocated_call() {
    let (transport, listener) = transport(VoiceApi::OpenAi).await;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        respond(
            &mut socket,
            "201 Created",
            "Location: /v1/realtime/calls/rtc_denied\r\n",
            SDP,
        )
        .await;
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        respond(
            &mut socket,
            "403 Forbidden",
            "",
            "{\"error\":{\"message\":\"voice access denied\"}}",
        )
        .await;
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, _) = read_request(&mut socket).await;
        assert!(headers.starts_with("post /v1/realtime/calls/rtc_denied/hangup "));
        respond(&mut socket, "200 OK", "", "").await;
    });
    let Err(Error::Provider(error)) = transport.start(request()).await else {
        panic!("expected sideband denial")
    };
    assert_eq!(error.status(), Some(403));
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn custom_endpoints_do_not_advertise_or_dispatch_realtime_voice() {
    let public = crate::backend::model::openai::OpenAi::new(
        "key",
        "https://api.openai.com/v1",
        "gpt-5.6-sol",
    )
    .unwrap();
    let custom =
        crate::backend::model::openai::OpenAi::new("key", "http://localhost:11434/v1", "local")
            .unwrap();
    assert!(public.supports_realtime_voice());
    assert!(!custom.supports_realtime_voice());
    let definition = crate::backend::model::provider::provider("responses").unwrap();
    assert!(
        !definition
            .realtime_voices(Some("https://api.openai.com/v1"))
            .is_empty()
    );
    assert!(
        definition
            .realtime_voices(Some("http://localhost:11434/v1"))
            .is_empty()
    );
}

#[test]
fn usage_is_validated_and_counted_once_for_each_response_and_transcription() {
    let mut turns = VoiceTurns::default();
    let response = json!({"type":"response.done","response":{"id":"r1","status":"cancelled","usage":{
        "input_tokens":12,"output_tokens":3,"total_tokens":15,"input_token_details":{"cached_tokens":4}
    }}});
    let events = turns
        .observe(VoiceApi::OpenAi, "ask_agent", &response)
        .unwrap();
    assert_eq!(
        events,
        vec![RealtimeVoiceEvent::Usage(TokenUsage {
            input_tokens: 12,
            output_tokens: 3,
            total_tokens: 15,
            cached_input_tokens: 4,
            ..TokenUsage::default()
        })]
    );
    assert!(
        turns
            .observe(VoiceApi::OpenAi, "ask_agent", &response)
            .unwrap()
            .is_empty()
    );
    let mut asr = transcript();
    asr["content_index"] = 0.into();
    asr["usage"] = json!({"type":"tokens","input_tokens":2,"output_tokens":3,"total_tokens":5});
    assert_eq!(
        turns.observe(VoiceApi::OpenAi, "ask_agent", &asr).unwrap(),
        vec![
            RealtimeVoiceEvent::Transcript {
                id: "i1".into(),
                role: crate::protocol::ConversationRole::User,
                text: "Fix the actual bug, please.".into(),
                complete: true
            },
            RealtimeVoiceEvent::Usage(TokenUsage {
                input_tokens: 2,
                output_tokens: 3,
                total_tokens: 5,
                ..TokenUsage::default()
            })
        ]
    );
    assert!(
        turns
            .observe(VoiceApi::OpenAi, "ask_agent", &asr)
            .unwrap()
            .is_empty()
    );
    for usage in [
        json!({"input_tokens":-1}),
        json!({"input_tokens":1,"output_tokens":0,"total_tokens":1,"input_token_details":{"cached_tokens":2}}),
        json!({"input_tokens":i64::MAX,"output_tokens":1,"total_tokens":i64::MAX}),
    ] {
        let mut invalid_response = response.clone();
        invalid_response["response"]["id"] = "invalid".into();
        invalid_response["response"]["usage"] = usage;
        assert!(
            turns
                .observe(VoiceApi::OpenAi, "ask_agent", &invalid_response)
                .is_err()
        );
    }
}

#[test]
fn replies_wait_for_the_active_response_and_coalesce_before_next_speech() {
    let mut turns = VoiceTurns::default();
    turns
        .observe(
            VoiceApi::OpenAi,
            "ask_agent",
            &json!({"type":"response.created","response":{"id":"r1"}}),
        )
        .unwrap();
    turns.reply_ready = true;
    assert!(!turns.take_reply_response());
    turns
        .observe(
            VoiceApi::OpenAi,
            "ask_agent",
            &json!({"type":"response.done","response":{"id":"r1","status":"cancelled"}}),
        )
        .unwrap();
    assert!(turns.take_reply_response());
    turns.reply_ready = true;
    assert!(!turns.take_reply_response());
    turns
        .observe(
            VoiceApi::OpenAi,
            "ask_agent",
            &json!({"type":"response.created","response":{"id":"r2"}}),
        )
        .unwrap();
    // A retransmitted older completion must not mark the new response idle.
    turns
        .observe(
            VoiceApi::OpenAi,
            "ask_agent",
            &json!({"type":"response.done","response":{"id":"r1","status":"cancelled"}}),
        )
        .unwrap();
    assert!(!turns.take_reply_response());
    turns
        .observe(
            VoiceApi::OpenAi,
            "ask_agent",
            &json!({"type":"response.done","response":{"id":"r2","status":"completed"}}),
        )
        .unwrap();
    assert!(turns.take_reply_response());
    assert!(!turns.take_reply_response());
}

#[test]
fn background_noise_with_empty_transcription_does_not_end_the_call() {
    let mut event = transcript();
    event["transcript"] = "".into();
    let mut turns = VoiceTurns::default();
    assert!(
        turns
            .observe(VoiceApi::OpenAi, "ask_agent", &event)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn direct_speech_and_spoken_handoff_results_share_the_same_transcript_path() {
    use crate::protocol::ConversationRole::{Assistant, User};
    for api in [VoiceApi::OpenAi, VoiceApi::Codex] {
        // Even a response requested after a Bot handoff remains visible in voice history.
        let mut turns = VoiceTurns {
            response: ResponseState::Requested,
            ..VoiceTurns::default()
        };
        let events = if api == VoiceApi::Codex {
            vec![
                json!({"type":"input_transcript.added","item":{"id":"input-1","text":"Hello"}}),
                json!({"type":"turn.done","turn":{"id":"user-turn","role":"user","transcript":"Hello!"}}),
                json!({"type":"output_transcript.added","item":{"id":"output-1","text":"Hi"}}),
                json!({"type":"output_transcript.added","item":{"id":"output-2","text":" there"}}),
                json!({"type":"turn.done","turn":{"id":"assistant-turn","role":"assistant","transcript":"Hi there!"}}),
            ]
        } else {
            vec![
                json!({"type":"conversation.item.input_audio_transcription.delta","item_id":"user","delta":"Hello"}),
                json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"user","transcript":"Hello!"}),
                json!({"type":"response.created","response":{"id":"response"}}),
                json!({"type":"response.output_audio_transcript.delta","response_id":"response","item_id":"assistant","content_index":0,"delta":"Hi"}),
                json!({"type":"response.output_audio_transcript.delta","response_id":"response","item_id":"assistant","content_index":0,"delta":" there"}),
                json!({"type":"response.output_audio_transcript.done","response_id":"response","item_id":"assistant","content_index":0,"transcript":"Hi there!"}),
            ]
        };
        let mut observed = Vec::new();
        for event in &events {
            observed.extend(turns.observe(api, "ask_agent", event).unwrap());
        }
        let user = if api == VoiceApi::Codex {
            "codex-user-1"
        } else {
            "user"
        };
        let assistant = if api == VoiceApi::Codex {
            "codex-assistant-2"
        } else {
            "assistant:0"
        };
        let entry = |id: &str, role, text: &str, complete| RealtimeVoiceEvent::Transcript {
            id: id.into(),
            role,
            text: text.into(),
            complete,
        };
        assert_eq!(
            observed,
            [
                entry(user, User, "Hello", false),
                entry(user, User, "Hello!", true),
                entry(assistant, Assistant, "Hi", false),
                entry(assistant, Assistant, " there", false),
                entry(assistant, Assistant, "Hi there!", true)
            ]
        );
        assert!(
            turns
                .observe(api, "ask_agent", events.last().unwrap())
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn voice_catalog_default_selection_and_invalid_ids_are_provider_owned() {
    for api in [VoiceApi::OpenAi, VoiceApi::Codex] {
        let (transport, listener) = transport(api).await;
        assert_eq!(
            transport.session(&request())["audio"]["output"]["voice"],
            if api == VoiceApi::Codex {
                "cove"
            } else {
                "marin"
            }
        );
        let mut selected = request();
        selected.voice = Some(
            if api == VoiceApi::Codex {
                "maple"
            } else {
                "cedar"
            }
            .into(),
        );
        assert_eq!(
            transport.session(&selected)["audio"]["output"]["voice"],
            if api == VoiceApi::Codex {
                "maple"
            } else {
                "cedar"
            }
        );
        selected.voice = Some("not-a-supported-voice".into());
        assert!(
            matches!(transport.start(selected).await, Err(error) if error.to_string().contains("not supported"))
        );
        assert!(
            timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }
}

#[test]
fn native_delegation_keeps_agreed_task_separate_from_actual_spoken_words() {
    let mut turns = VoiceTurns::default();
    let task = json!({"type":"delegation.created","item":{"id":"task-1","type":"delegation","target":"client","content":[{"type":"input_text","text":"Update the dark theme with the agreed blue accent; preserve all toolbar actions."}]}});
    assert_eq!(
        turns.observe(VoiceApi::Codex, "ask_agent", &task).unwrap(),
        [RealtimeVoiceEvent::Handoff {
            id: "task-1".into(),
            text:
                "Update the dark theme with the agreed blue accent; preserve all toolbar actions."
                    .into(),
            needs_context: true,
        }]
    );
    assert!(
        turns
            .observe(VoiceApi::Codex, "ask_agent", &task)
            .unwrap()
            .is_empty()
    );
    let spoken = json!({"type":"turn.done","turn":{"id":"speech-1","role":"user","transcript":"Yes, do it."}});
    assert!(
        matches!(&turns.observe(VoiceApi::Codex, "ask_agent", &spoken).unwrap()[0], RealtimeVoiceEvent::Transcript { text, .. } if text == "Yes, do it.")
    );
}

#[test]
fn explicit_task_arguments_preserve_requirements_beyond_identifier_length() {
    let mut turns = VoiceTurns::default();
    let text = format!(
        "Update the interface with these requirements: {}",
        "preserve actions; ".repeat(100)
    );
    let mut event = completed();
    event["response"]["output"][0]["arguments"] = json!({"text":text}).to_string().into();
    assert_eq!(
        turns
            .observe(VoiceApi::OpenAi, "ask_agent", &event)
            .unwrap(),
        [RealtimeVoiceEvent::Handoff {
            id: "h1".into(),
            text,
            needs_context: false,
        }]
    );
}
