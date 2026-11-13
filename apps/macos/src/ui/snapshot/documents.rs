//! Interface-document scenarios: the `show_ui` demo transcript in a local chat that needs
//! no gateway, scrolled to each document in turn.

use super::Scenario;
use super::capture::{catalog, forced_appearance, session_id};
use super::probes::{report, set_activity, undo_after_capture};

use crate::ui::Workspace;
use crate::ui::router::PageTab;
use crate::ui::theme::Appearance;

use gpui::{Context, Pixels, Window, px, size};

use mobius::SessionId;
use mobius_gateway::wire::SessionActivityState;
use mobius_gateway::wire::{HarnessConfig, ReadyPayload, SessionReadyPayload, SessionRecord};
use std::collections::BTreeMap;
use std::sync::Arc;

const CHAT: &str = "fixture-intelligent-ui";

thread_local! {
    /// Remote pictures load over the network; the capture waits until this passes.
    static IMAGES_BY: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

/// The scenario's remote pictures may still be loading.
pub(super) fn images_pending() -> bool {
    IMAGES_BY.with(|by| by.get().is_some_and(|by| std::time::Instant::now() < by))
}

pub(super) const SCENARIOS: &[Scenario] = &[
    ("intelligent-ui-lisbon", |this, window, cx| {
        show(this, window, cx, 0, px(0.));
    }),
    ("intelligent-ui-running", |this, window, cx| {
        show(this, window, cx, 1, px(0.));
    }),
    ("intelligent-ui-running-end", |this, window, cx| {
        show(this, window, cx, 1, px(400.));
    }),
    ("intelligent-ui-laptops", |this, window, cx| {
        show(this, window, cx, 2, px(0.));
    }),
    ("intelligent-ui-laptops-end", |this, window, cx| {
        show(this, window, cx, 2, px(480.));
    }),
    ("coding-demo", coding),
    ("readme-hero", readme_hero),
    ("motion-transcript-fold", |this, window, cx| {
        coding_motion(this, window, cx, true, |this, _, cx| {
            coding_change(this, crate::ui::chat::CodingChange::Finish, cx);
        });
    }),
    ("motion-transcript-reply", |this, window, cx| {
        coding_motion(this, window, cx, true, |this, _, cx| {
            coding_change(this, crate::ui::chat::CodingChange::Reply, cx);
        });
    }),
    ("motion-transcript-stream", |this, window, cx| {
        coding_motion(this, window, cx, true, |this, _, cx| {
            coding_change(this, crate::ui::chat::CodingChange::Stream, cx);
        });
    }),
    ("motion-transcript-open", |this, window, cx| {
        coding_motion(this, window, cx, false, |this, _, cx| {
            let edit = this
                .client
                .state
                .transcript
                .entries()
                .find(|entry| entry.id.contains("code-edit"))
                .map(|entry| format!("chat:fixture-coding:{}:block", entry.id));
            if let Some(id) = edit {
                this.toggle_disclosure(&id, "chat:fixture-coding", cx);
            }
        });
    }),
    ("title-status-running", |this, window, cx| {
        status(this, window, cx, SessionActivityState::Running, false);
    }),
    ("title-status-approval", |this, window, cx| {
        status(
            this,
            window,
            cx,
            SessionActivityState::AwaitingApproval,
            false,
        );
    }),
    ("title-status-unread", |this, window, cx| {
        status(this, window, cx, SessionActivityState::Idle, true);
    }),
    ("recaps-dialog", recaps),
];

/// Chats a "While you were away" fixture recaps: id, title, waits on the reader, recap.
const RECAPS: [(&str, &str, bool); 4] = [
    ("fixture-recap-release", "Ship the 0.9 release", true),
    ("fixture-recap-berlin", "Wochenende in Berlin", false),
    ("fixture-recap-flaky", "Flaky gateway test", false),
    ("fixture-recap-writing", "Quarterly numbers", false),
];

/// The recap dialog over gateway-free fixture chats: written recaps (one in German, one
/// waiting on the reader), a chat whose recap failed, and one still being written.
fn recaps(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    use crate::host::platform::ChatRecap;
    use crate::ui::navigation::RecapBody;
    for (index, (id, title, waits)) in RECAPS.into_iter().enumerate() {
        let mut session = fixture_session(
            id,
            title,
            1_790_000_000_000 - i64::try_from(index).unwrap_or(i64::MAX) * 60_000,
        );
        session.activity.attention = u32::from(waits);
        let body = match index {
            0 => RecapBody::Written(ChatRecap {
                headline: "Tagged v0.9.0 and drafted the release notes.".into(),
                done: vec![
                    "Bumped the version in Cargo.toml and the iOS project".into(),
                    "Ran the full test suite: 412 passed".into(),
                    "Wrote release notes from the merged pull requests".into(),
                ],
                needs_you: Some("Approve pushing the v0.9.0 tag to origin.".into()),
            }),
            1 => RecapBody::Written(ChatRecap {
                headline: "Drei Restaurants in Kreuzberg für Samstag gefunden.".into(),
                done: vec![
                    "Tisch für vier um 19:30 Uhr im Café Nord vorgemerkt".into(),
                    "Route vom Hotel mit der U1 berechnet".into(),
                ],
                needs_you: None,
            }),
            2 => RecapBody::Plain(
                "The retry test passes now; the timeout was too short on CI.".into(),
            ),
            _ => RecapBody::Writing { fallback: None },
        };
        this.shell.recaps.set_fixture(&session, body);
        this.set_unread(&session.session_id, !waits);
        this.client.state.sessions.push(session);
    }
    undo_after_capture(|this| {
        let removed = this
            .client
            .state
            .sessions
            .extract_if(.., |session| {
                RECAPS.iter().any(|(id, ..)| *id == &*session.session_id)
            })
            .collect::<Vec<_>>();
        for session in removed {
            this.set_unread(&session.session_id, false);
        }
        this.shell.recaps = Default::default();
        this.gateway_revision += 1;
    });
    this.gateway_revision += 1;
    this.open_recaps(window, cx);
}

/// A gateway-free chat record for fixtures.
/// Opens a local demo chat, with fixture Bots when no gateway is connected.
pub(super) fn open_demo(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    id: &str,
    title: &str,
    transcript: crate::gateway::TranscriptStore,
) {
    if this.client.state.gateway.is_none() {
        this.client.state.gateway = fixture_gateway();
        undo_after_capture(|this| this.client.state.gateway = None);
    }
    let id = if let Some(session) = this
        .client
        .state
        .sessions
        .iter()
        .find(|s| s.session_id.as_str() == id)
    {
        session.session_id.share()
    } else {
        let session = fixture_session(id, title, 1_790_000_000_000);
        let id = session.session_id.share();
        this.client.state.sessions.push(session);
        id
    };
    this.client.state.selected_session_id = Some(id.share());
    this.client.state.session = Some(SessionReadyPayload {
        memory: None,
        active_turn_ids: Vec::new(),
        pending_approvals: Vec::new(),
        latest_sequence: 0,
        workspace: None,
        attached_folders: Vec::new(),
        git: None,
        session: Arc::new(mobius::protocol::SessionConfiguredEvent {
            session_id: id.share(),
            context: mobius::protocol::SessionContext {
                owner_id: "fixture-mobius".into(),
                ..Default::default()
            }
            .into(),
            model: mobius::protocol::ModelChangedEvent {
                route: "fixture".into(),
                model: "fixture".into(),
                reasoning_effort: None,
                model_context_window: None,
            },
        }),
        contributions: Vec::new(),
        widgets: Vec::new(),
        context_limit_tokens: None,
        token_usage: None,
        active_message_delivery: mobius::protocol::ActiveMessageDelivery::Steer,
        run_stats: Default::default(),
        pinned_messages: Default::default(),
    });
    this.client.state.transcript = transcript;
    this.open_page(PageTab::Chat(id), window, cx);
}

/// A coding chat beside its changes, explorer closed, at a full-screen Mac size: the
/// site's background.
fn coding(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    window.resize(size(px(1728.), px(1117.)));
    for (id, title, at) in [
        (
            "fixture-code-1",
            "Derive the palette from background",
            1_789_990_000_000,
        ),
        (
            "fixture-code-2",
            "Gateway reconnect backoff",
            1_789_980_000_000,
        ),
        (
            "fixture-code-3",
            "Release notes for 0.16",
            1_789_970_000_000,
        ),
        ("fixture-code-4", "Flaky voice call test", 1_789_960_000_000),
    ] {
        this.client
            .state
            .sessions
            .push(fixture_session(id, title, at));
    }
    undo_after_capture(|this| {
        this.client
            .state
            .sessions
            .retain(|session| !session.session_id.starts_with("fixture-cod"));
    });
    coding_chat(this, window, cx, true);
    this.split_content = true;
    this.open_diff_text(&super::content::last_commit_diff(), true, cx);
}

/// The README picture: a fake gateway with a few chats, the coding demo beside a diff tab
/// of its own change, over the Kanazawa moon in the dark theme. No live data appears.
fn readme_hero(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    window.resize(size(px(1600.), px(1000.)));
    super::harness::fixture(this);
    this.client.state.sessions = [
        (
            "fixture-hero-1",
            "Gateway reconnect backoff",
            1_789_990_000_000,
        ),
        (
            "fixture-hero-2",
            "Release notes for 0.17",
            1_789_980_000_000,
        ),
        ("fixture-hero-3", "Flaky voice call test", 1_789_970_000_000),
        ("fixture-hero-4", "Sidebar drag and drop", 1_789_960_000_000),
        (
            "fixture-hero-5",
            "Profile the transcript scroll",
            1_789_950_000_000,
        ),
        ("fixture-hero-6", "Weekend in Lisbon", 1_789_940_000_000),
    ]
    .into_iter()
    .map(|(id, title, at)| fixture_session(id, title, at))
    .collect();
    this.appearance = Appearance::Dark;
    undo_after_capture(|this| {
        this.appearance = forced_appearance().unwrap_or_default();
    });
    super::background::show(this, crate::ui::backdrop::Source::Kanazawa, cx);
    coding_chat(this, window, cx, false);
    this.split_content = true;
    this.open_diff_text(crate::ui::chat::CODING_DIFF, true, cx);
}

/// The coding demo chat, every step open when `expand`.
fn coding_chat(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    expand: bool,
) {
    open_demo(
        this,
        window,
        cx,
        "fixture-coding",
        "Fix the recap button flicker",
        crate::ui::chat::coding_demo_transcript(),
    );
    if let Some(session) = this.client.state.session.as_mut() {
        Arc::make_mut(&mut session.run_stats).completed.elapsed_ms = 252_000;
    }
    // Every step open: the search, the diff and the test run read as code.
    if !expand {
        cx.notify();
        return;
    }
    for row in this.client.state.transcript.rows() {
        this.expanded
            .insert(format!("chat:fixture-coding:{}", row.id));
    }
    for entry in this.client.state.transcript.entries() {
        if matches!(
            entry.content,
            mobius_gateway::wire::TranscriptItemContent::Block { .. }
        ) {
            this.expanded
                .insert(format!("chat:fixture-coding:{}:block", entry.id));
        }
    }
    cx.notify();
}

/// The coding demo with `change` recorded as a frame strip after the capture.
fn coding_motion(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    expand: bool,
    change: super::Step,
) {
    coding_chat(this, window, cx, expand);
    super::strip_after_capture(change);
}

fn coding_change(
    this: &mut Workspace,
    change: crate::ui::chat::CodingChange,
    cx: &mut Context<Workspace>,
) {
    crate::ui::chat::apply_coding_change(&mut this.client.state.transcript, change);
    cx.notify();
    cx.notify();
}

/// Demo Bots for runs without a gateway, so the sidebar and transcript show real faces.
fn fixture_gateway() -> Option<ReadyPayload> {
    let bots: Vec<_> = [
        ("mobius", "möbius", "purple", "circle"),
        ("scout", "Scout", "teal", "hexagon"),
        ("atlas", "Atlas", "orange", "star"),
        ("juno", "Juno", "green", "flower"),
        ("pixel", "Pixel", "blue", "diamond"),
    ]
    .into_iter()
    .map(|(handle, name, tint, shape)| {
        serde_json::json!({
            "id": format!("fixture-{handle}"), "handle": handle,
            "conversation_session_id": format!("fixture-conversation-{handle}"),
            "name": name, "description": "", "tint": tint, "shape": shape, "harness": "mobius",
            "config": {"revision": 1, "config": HarnessConfig::Mobius(Default::default())},
            "capabilities": {"attachments": false, "computer": false, "voice": {"state": "off"},
                "memory": {"enabled": false, "global": false}},
            "routine_interaction_policy": "unattended"
        })
    })
    .collect();
    catalog(
        "fixture ready payload",
        serde_json::json!({
            "gateway_version": "fixture", "machine_name": "This Mac",
            "computer_view": "unavailable", "bots": bots,
            "sessions": [], "background_approvals": [],
            "agent_catalog": {
                "bot_defaults": null, "default_capabilities": null, "models": [],
                "model_providers": {}, "image_models": [], "voice_models": [],
                "middleware_features": [], "extensions": [], "contributions": []
            },
            "harnesses": [], "max_active_sessions": 1,
            "revisions": {}, "omitted": [], "provider_instances": [], "providers": [],
            "session_file_limits": mobius::backend::session_files::session_file_limits()
        }),
    )
}

#[expect(
    clippy::expect_used,
    reason = "internal snapshot fixture identities must be valid"
)]
pub(super) fn fixture_session(id: &str, title: &str, updated_at: i64) -> SessionRecord {
    SessionRecord {
        session_id: SessionId::new(id).expect("fixture session"),
        session_context: mobius::protocol::SessionContext {
            owner_id: "fixture-mobius".into(),
            ..Default::default()
        },
        parent_session_id: None,
        parent_sequence: None,
        sequence: 0,
        first_user_message: None,
        execution_stats: Default::default(),
        title: Some(title.into()),
        pinned: false,
        activity: Default::default(),
        created_at: updated_at,
        updated_at,
    }
}

/// The demo chat whose titlebar status shows `state`, read while `unread`.
fn status(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    state: SessionActivityState,
    unread: bool,
) {
    show(this, window, cx, 0, px(0.));
    let Some(session) = session_id(this) else {
        return;
    };
    set_activity(BTreeMap::from([(session, (state, unread))]));
    undo_after_capture(|this| {
        set_activity(BTreeMap::new());
        this.gateway_revision += 1;
    });
}

/// Opens the local demo chat with document `index` at the top of the view, `offset` above it.
fn show(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    index: usize,
    offset: Pixels,
) {
    open_demo(
        this,
        window,
        cx,
        CHAT,
        "Weekend, running and laptops",
        crate::ui::chat::ui_demo_transcript(),
    );
    IMAGES_BY.with(|by| {
        by.set(Some(
            std::time::Instant::now() + std::time::Duration::from_secs(6),
        ));
    });
    let scope = format!("chat:{CHAT}");
    // The list learns its rows as it draws; then the document scrolls into view.
    cx.spawn_in(window, async move |this, cx| {
        let suffix = format!("/ui-{index}");
        // Reveal until the capture: the list may still be learning row heights.
        let mut revealed = false;
        while images_pending() {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            // A sleeping display delivers no frames, and the list lays rows out as it draws.
            report("draw", cx.update(|window, cx| window.draw(cx).clear(cx)));
            revealed |= this
                .update(cx, |this, cx| {
                    this.fixture_reveal_row(&scope, &suffix, offset, cx)
                })
                .unwrap_or(false);
        }
        if revealed {
            return;
        }
        eprintln!("snapshot: no document {index}");
    })
    .detach();
}

#[cfg(test)]
mod tests {
    #[test]
    fn offline_demo_catalog_matches_the_gateway_contract() {
        let catalog = super::fixture_gateway().expect("valid demo catalog");
        assert_eq!(catalog.bots.len(), 5);
        assert_eq!(&*catalog.bots[0].handle, "mobius");
    }
}
