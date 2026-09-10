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
        instructions: "Delegate requested work to the workspace.".into(),
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
    let mut calls_url = Url::parse(&if api == VoiceApi::Codex {
        format!("{base}/calls")
    } else {
        base.replace("/realtime", "/live/sessions")
    })
    .unwrap();
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
                base.replace("/realtime", "/live/sessions")
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
            assert_eq!(request.uri().path(), "/v1/live/sessions/live_test/attach");
            assert_eq!(request.uri().query(), None);
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

fn live_answer(id: &str, sdp: &str) -> String {
    json!({"session":{"id":id},"transport":{"type":"webrtc","sdp":sdp}}).to_string()
}

fn delegation(api: VoiceApi, id: &str) -> Value {
    match api {
        VoiceApi::OpenAi => {
            json!({"type":"session.delegation.created","offset_ms":1000,"delegation":{"id":id,"type":"delegation","target":"client"}})
        }
        VoiceApi::Codex => {
            json!({"type":"delegation.created","item":{"id":id,"type":"delegation","target":"client","content":[{"type":"input_text","text":"Fix the actual bug, please."}]}})
        }
    }
}

#[tokio::test]
async fn live_and_codex_wire_contracts_delegate_once_reply_and_close() {
    for api in [VoiceApi::OpenAi, VoiceApi::Codex] {
        let (transport, listener) = transport(api).await;
        let reply_text = format!("x{}", "🗣".repeat(300));
        let expected_reply = reply_text.clone();
        let server = tokio::spawn(async move {
            let (mut http, _) = listener.accept().await.unwrap();
            let (headers, body) = read_request(&mut http).await;
            assert!(headers.contains("authorization: bearer secret\r\n"));
            assert!(headers.contains("content-type: application/json\r\n"));
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["session"]["delegation"]["type"], "client");
            assert!(body["session"].get("tools").is_none());
            if api == VoiceApi::Codex {
                assert!(
                    headers.starts_with(
                        "post /v1/realtime/calls?intent=quicksilver&architecture=avas "
                    )
                );
                assert_eq!(body["sdp"], SDP);
                assert_eq!(body["session"]["model"], "gpt-live-1-codex");
                assert_eq!(body["session"]["audio"]["output"]["voice"], "maple");
                respond(
                    &mut http,
                    "201 Created",
                    "Location: /v1/realtime/calls/rtc_test\r\n",
                    SDP,
                )
                .await;
            } else {
                assert!(headers.starts_with("post /v1/live/sessions "));
                assert_eq!(body["transport"], json!({"type":"webrtc","sdp":SDP}));
                assert_eq!(body["session"]["model"], "gpt-live-1");
                assert_eq!(body["session"]["audio"]["output"]["voice"], "quartz");
                respond(
                    &mut http,
                    "201 Created",
                    "Content-Type: application/json\r\n",
                    &live_answer("live_test", SDP),
                )
                .await;
            }
            drop(http);
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(socket, InspectUpgrade(api))
                .await
                .unwrap();
            for id in ["h1", "h1", "h2"] {
                socket
                    .send(Message::text(delegation(api, id).to_string()))
                    .await
                    .unwrap();
            }
            let context: Value =
                serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(
                context,
                match api {
                    VoiceApi::Codex =>
                        json!({"type":"session.context.append","channel":"commentary","content":[{"type":"input_text","text":"Bot is running tests."}]}),
                    VoiceApi::OpenAi =>
                        json!({"type":"session.thinking.append","delegation_id":null,"content":"Bot is running tests."}),
                }
            );
            for id in ["h1", "h2"] {
                let mut full_reply = String::new();
                while full_reply.len() < expected_reply.len() {
                    let reply: Value = serde_json::from_str(
                        socket.next().await.unwrap().unwrap().to_text().unwrap(),
                    )
                    .unwrap();
                    let text = match api {
                        VoiceApi::Codex => {
                            assert_eq!(reply["type"], "delegation.context.append");
                            assert_eq!(reply["delegation_item_id"], id);
                            assert_eq!(reply["channel"], "speakable");
                            reply["content"][0]["text"].as_str().unwrap()
                        }
                        VoiceApi::OpenAi => {
                            assert_eq!(reply["type"], "session.commentary.append");
                            assert_eq!(reply["delegation_id"], id);
                            reply["content"].as_str().unwrap()
                        }
                    };
                    assert!(text.len() <= 500);
                    full_reply.push_str(text);
                }
                assert_eq!(full_reply, expected_reply);
            }
            let close: Value =
                serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(close, json!({"type":"session.close"}));
            if api == VoiceApi::OpenAi {
                for event in [
                    json!({"type":"session.output_transcript.delta","event_id":"last","delta":"Done.","start_ms":1200,"end_ms":1500}),
                    json!({"type":"session.closed"}),
                ] {
                    socket.send(Message::text(event.to_string())).await.unwrap();
                }
            }
            assert!(matches!(
                socket.next().await.unwrap().unwrap(),
                Message::Close(_)
            ));
            let (mut http, _) = listener.accept().await.unwrap();
            let (headers, _) = read_request(&mut http).await;
            assert!(headers.starts_with(match api {
                VoiceApi::Codex => "post /v1/realtime/calls/rtc_test/hangup ",
                VoiceApi::OpenAi => "post /v1/live/sessions/live_test/hangup ",
            }));
            assert!(headers.contains("authorization: bearer secret\r\n"));
            respond(&mut http, "200 OK", "", "").await;
        });
        let mut selected = request();
        selected.voice = Some(
            if api == VoiceApi::Codex {
                "maple"
            } else {
                "quartz"
            }
            .into(),
        );
        let mut call = transport.start(selected).await.unwrap();
        assert_eq!(call.answer_sdp, SDP);
        for id in ["h1", "h2"] {
            assert_eq!(
                timeout(Duration::from_secs(2), call.events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                RealtimeVoiceEvent::Handoff {
                    id: id.into(),
                    text: (api == VoiceApi::Codex).then(|| "Fix the actual bug, please.".into()),
                }
            );
        }
        call.commands
            .send(RealtimeVoiceCommand::Context {
                text: "Bot is running tests.".into(),
            })
            .await
            .unwrap();
        for id in ["h1", "h2"] {
            call.commands
                .send(RealtimeVoiceCommand::Reply {
                    handoff_id: id.into(),
                    text: reply_text.clone(),
                })
                .await
                .unwrap();
        }
        call.commands
            .send(RealtimeVoiceCommand::Close)
            .await
            .unwrap();
        let final_events = timeout(Duration::from_secs(2), async {
            let mut events = Vec::new();
            while let Some(event) = call.events.recv().await {
                events.push(event.unwrap());
            }
            events
        })
        .await
        .unwrap();
        if api == VoiceApi::OpenAi {
            assert_eq!(final_events.len(), 2);
            assert!(
                matches!(&final_events[1], RealtimeVoiceEvent::Transcript { text, complete: true, .. } if text == "Done.")
            );
        } else {
            assert!(final_events.is_empty());
        }
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[test]
fn live_captions_preserve_both_speakers_deduplicate_and_finalize_before_delegation() {
    use crate::protocol::ConversationRole::{Assistant, User};
    let mut turns = VoiceTurns::default();
    let entry = |id: &str, role, text: &str, complete| RealtimeVoiceEvent::Transcript {
        id: id.into(),
        role,
        text: text.into(),
        complete,
    };
    let input = json!({"type":"session.input_transcript.delta","event_id":"e1","delta":"Fix it","start_ms":0,"end_ms":600});
    assert_eq!(
        turns.observe(VoiceApi::OpenAi, &input).unwrap(),
        [entry("live-0-1", User, "Fix it", false)]
    );
    assert!(turns.observe(VoiceApi::OpenAi, &input).unwrap().is_empty());
    let output = json!({"type":"session.output_transcript.delta","event_id":"e2","delta":"On it.","start_ms":500,"end_ms":900});
    assert_eq!(
        turns.observe(VoiceApi::OpenAi, &output).unwrap(),
        [entry("live-1-2", Assistant, "On it.", false)]
    );
    assert_eq!(
        turns
            .observe(VoiceApi::OpenAi, &delegation(VoiceApi::OpenAi, "h1"))
            .unwrap(),
        [
            entry("live-0-1", User, "Fix it", true),
            entry("live-1-2", Assistant, "On it.", true),
            RealtimeVoiceEvent::Handoff {
                id: "h1".into(),
                text: None
            },
        ]
    );
    assert!(
        turns
            .observe(VoiceApi::OpenAi, &delegation(VoiceApi::OpenAi, "h1"))
            .unwrap()
            .is_empty()
    );
    // Seconds are cumulative Live billing units, never token counts.
    assert!(
        turns
            .observe(
                VoiceApi::OpenAi,
                &json!({"type":"session.usage.updated","usage":{"seconds":10}})
            )
            .unwrap()
            .is_empty()
    );
    for (start, end) in [(20, 10), (-1, 10)] {
        assert!(turns.observe(VoiceApi::OpenAi, &json!({"type":"session.input_transcript.delta","delta":"bad","start_ms":start,"end_ms":end})).is_err());
    }
}

#[test]
fn live_caption_gaps_and_late_fragments_keep_text_in_separate_groups() {
    let mut turns = VoiceTurns::default();
    let fragment = |start, end, text| json!({"type":"session.input_transcript.delta","delta":text,"start_ms":start,"end_ms":end});
    turns
        .observe(VoiceApi::OpenAi, &fragment(100, 200, "First"))
        .unwrap();
    let events = turns
        .observe(VoiceApi::OpenAi, &fragment(1300, 1400, "Second"))
        .unwrap();
    assert!(
        matches!(&events[..], [RealtimeVoiceEvent::Transcript { text, complete: true, .. }, RealtimeVoiceEvent::Transcript { complete: false, .. }] if text == "First")
    );
    let events = turns
        .observe(VoiceApi::OpenAi, &fragment(300, 400, "Late"))
        .unwrap();
    assert!(
        matches!(&events[0], RealtimeVoiceEvent::Transcript { text, complete: true, .. } if text == "Second")
    );
    let events = turns
        .observe(VoiceApi::OpenAi, &json!({"type":"session.closed"}))
        .unwrap();
    assert!(
        matches!(&events[0], RealtimeVoiceEvent::Transcript { text, complete: true, .. } if text == "Late")
    );
}

#[test]
fn codex_final_transcripts_replace_drafts_and_deduplicate() {
    let mut turns = VoiceTurns::default();
    for (kind, role, draft, final_text) in [
        ("input_transcript.added", "user", "noise", ""),
        ("output_transcript.added", "assistant", "Hi", "Hi there!"),
    ] {
        let draft = turns
            .observe(VoiceApi::Codex, &json!({"type":kind,"item":{"text":draft}}))
            .unwrap();
        let final_event =
            json!({"type":"turn.done","turn":{"id":role,"role":role,"transcript":final_text}});
        let final_events = turns.observe(VoiceApi::Codex, &final_event).unwrap();
        assert!(
            matches!((&draft[0], &final_events[0]), (RealtimeVoiceEvent::Transcript { id, complete: false, .. }, RealtimeVoiceEvent::Transcript { id: final_id, text, complete: true, .. }) if id == final_id && text == final_text)
        );
        assert!(
            turns
                .observe(VoiceApi::Codex, &final_event)
                .unwrap()
                .is_empty()
        );
    }
    let mut task = delegation(VoiceApi::Codex, "long");
    let text = "Preserve all toolbar actions. ".repeat(100);
    task["item"]["content"][0]["text"] = text.clone().into();
    assert_eq!(
        turns.observe(VoiceApi::Codex, &task).unwrap(),
        [RealtimeVoiceEvent::Handoff {
            id: "long".into(),
            text: Some(text)
        }]
    );
}

#[tokio::test]
async fn public_session_identity_is_validated_and_invalid_sdp_hangs_up() {
    for id in ["../stolen", "live_invalid_sdp"] {
        let (transport, listener) = transport(VoiceApi::OpenAi).await;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            respond(
                &mut socket,
                "201 Created",
                "",
                &live_answer(id, "invalid SDP"),
            )
            .await;
            drop(socket);
            if id == "live_invalid_sdp" {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (headers, _) = read_request(&mut socket).await;
                assert!(headers.starts_with("post /v1/live/sessions/live_invalid_sdp/hangup "));
                respond(&mut socket, "200 OK", "", "").await;
            } else {
                assert!(
                    timeout(Duration::from_millis(20), listener.accept())
                        .await
                        .is_err()
                );
            }
        });
        assert!(transport.start(request()).await.is_err());
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
            "Content-Type: application/json\r\n",
            &live_answer("live_cancel", SDP),
        )
        .await;
        drop(socket);
        let (socket, _) = listener.accept().await.unwrap();
        connecting.send(()).unwrap();
        let (mut hangup, _) = listener.accept().await.unwrap();
        let (headers, _) = read_request(&mut hangup).await;
        assert!(headers.starts_with("post /v1/live/sessions/live_cancel/hangup "));
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
    let (transport, listener) = transport(VoiceApi::Codex).await;
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
            "Content-Type: application/json\r\n",
            &live_answer("live_denied", SDP),
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
        assert!(headers.starts_with("post /v1/live/sessions/live_denied/hangup "));
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

#[tokio::test(start_paused = true)]
async fn credential_deadline_hangs_up_a_retained_voice_call() {
    let (commands, _commands) = tokio::sync::mpsc::channel(1);
    let (_events, events) = tokio::sync::mpsc::channel(1);
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let mut call = RealtimeVoiceCall::new(SDP.into(), commands, events, cancel).unwrap();
    call.limit_credential(crate::backend::model::ModelCredentialLifetime {
        expires_at: Some(std::time::SystemTime::now() + Duration::from_secs(60)),
        ..Default::default()
    });
    assert!(cancelled.await.is_err());
    assert_eq!(call.answer_sdp, SDP);
}

#[tokio::test]
async fn credential_revocation_hangs_up_a_retained_voice_call() {
    let (commands, _commands) = tokio::sync::mpsc::channel(1);
    let (_events, events) = tokio::sync::mpsc::channel(1);
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let (owner, revoked) = tokio::sync::watch::channel(());
    let mut call = RealtimeVoiceCall::new(SDP.into(), commands, events, cancel).unwrap();
    call.limit_credential(crate::backend::model::ModelCredentialLifetime {
        expires_at: None,
        revoked: Some(revoked),
    });
    drop(owner);
    assert!(cancelled.await.is_err());
    assert_eq!(call.answer_sdp, SDP);
}
#[tokio::test]
async fn sideband_event_backpressure_preserves_order_and_terminal_errors() {
    let (transport, listener) = transport(VoiceApi::Codex).await;
    let (sent, sent_signal) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut http, _) = listener.accept().await.unwrap();
        read_request(&mut http).await;
        respond(
            &mut http,
            "201 Created",
            "Location: /v1/realtime/calls/rtc_test\r\n",
            SDP,
        )
        .await;
        drop(http);
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket =
            tokio_tungstenite::accept_hdr_async(socket, InspectUpgrade(VoiceApi::Codex))
                .await
                .unwrap();
        for n in 0..17 {
            let event = json!({"type":"turn.done","turn":{"id":format!("i{n}"),"role":"user","transcript":"Fix the actual bug, please."}});
            socket.send(Message::text(event.to_string())).await.unwrap();
        }
        sent.send(()).unwrap();
        drop(socket);
        let (mut http, _) = listener.accept().await.unwrap();
        let (headers, body) = read_request(&mut http).await;
        assert!(headers.starts_with("post /v1/realtime/calls/rtc_test/hangup "));
        assert!(body.is_empty());
        respond(&mut http, "200 OK", "", "").await;
    });

    let mut call = transport.start(request()).await.unwrap();
    sent_signal.await.unwrap();
    timeout(Duration::from_secs(2), async {
        while call.events.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for n in 0..17 {
        assert_eq!(
            timeout(Duration::from_secs(2), call.events.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            RealtimeVoiceEvent::Transcript {
                id: format!("codex-user-{}", n + 1),
                role: crate::protocol::ConversationRole::User,
                text: "Fix the actual bug, please.".into(),
                complete: true,
            }
        );
    }
    let _error = timeout(Duration::from_secs(2), call.events.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    drop(call);
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}
