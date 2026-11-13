use std::sync::Arc;

use mobius::SessionId;
use mobius::protocol::{
    FrontendBlock, FrontendPreviewKind, FrontendPreviewUpdate, MessageAuthor, MessageDelivery,
    MessageEvent,
};
use mobius_gateway::wire::{
    ActivityCounts, BlockDelta, RenderedPreview, ServerMessage, TranscriptChild, TranscriptItem,
    TranscriptItemContent, TranscriptItemState, TranscriptOp, TranscriptPage, TranscriptPosition,
    TranscriptRow, TranscriptRowKind,
};

use crate::{GatewayState, RowChild, TranscriptStore};

fn message(id: &str, text: &str) -> Arc<TranscriptItem> {
    Arc::new(TranscriptItem {
        id: id.into(),
        turn_id: Some("turn".into()),
        recorded_at_ms: 0,
        state: TranscriptItemState::Complete,
        terminal: false,
        starts_turn: false,
        resumed: false,
        inherited: false,
        target: None,
        content: TranscriptItemContent::Message {
            message: MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: text.into(),
                attachments: Vec::new(),
                reply: None,
                message_target: None,
            },
        },
    })
}

fn block(id: &str, text: &str) -> Arc<TranscriptItem> {
    Arc::new(TranscriptItem {
        state: TranscriptItemState::Pending,
        content: TranscriptItemContent::Block {
            capability: "tools".into(),
            block: FrontendBlock {
                id: Some(id.into()),
                text: text.into(),
                ..Default::default()
            },
            group_title: None,
        },
        ..(*message(&format!("block:tools:{id}"), "")).clone()
    })
}

fn row(id: &str, sequence: u64, items: &[&str], kind: TranscriptRowKind) -> Arc<TranscriptRow> {
    Arc::new(TranscriptRow {
        id: id.into(),
        position: TranscriptPosition {
            depth: 0,
            sequence,
            index: 0,
        },
        items: items
            .iter()
            .map(|&id| TranscriptChild::Item(id.into()))
            .collect(),
        kind,
    })
}

fn entry(id: &str, sequence: u64) -> Arc<TranscriptRow> {
    row(
        id,
        sequence,
        &[id],
        TranscriptRowKind::Entry { turn_changes: None },
    )
}

fn text(store: &TranscriptStore, id: &str) -> String {
    match &store.item(id).expect("item").content {
        TranscriptItemContent::Message { message } => message.text.clone(),
        TranscriptItemContent::Block { block, .. } => block.text.clone(),
        _ => String::new(),
    }
}

fn order(store: &TranscriptStore) -> Vec<&str> {
    store.rows().map(|row| &*row.id).collect()
}

#[test]
fn an_older_page_never_replaces_what_the_chat_holds() {
    let mut store = TranscriptStore::default();
    store.replace(TranscriptPage {
        items: vec![message("message:b", "live")],
        rows: vec![entry("message:b", 5)],
    });
    store.prepend(TranscriptPage {
        items: vec![message("message:a", "older"), message("message:b", "stale")],
        rows: vec![entry("message:a", 1), entry("message:b", 9)],
    });
    assert_eq!(order(&store), ["message:a", "message:b"]);
    assert_eq!(text(&store, "message:b"), "live");
    assert_eq!(store.row("message:b").unwrap().position.sequence, 5);
}

#[test]
fn ops_append_extend_and_remove_without_rules_of_their_own() {
    let mut store = TranscriptStore::default();
    store.apply(vec![
        TranscriptOp::AppendText {
            id: "message:missing".into(),
            text: "lost".into(),
        },
        TranscriptOp::Item {
            item: message("message:a", "hel"),
        },
        TranscriptOp::AppendText {
            id: "message:a".into(),
            text: "lo".into(),
        },
        TranscriptOp::Item {
            item: block("run", "out"),
        },
        TranscriptOp::Row {
            row: entry("message:a", 1),
        },
        TranscriptOp::Row {
            row: row(
                "activity:block:tools:run",
                2,
                &["block:tools:run"],
                TranscriptRowKind::Activity {
                    counts: ActivityCounts::default(),
                },
            ),
        },
    ]);
    assert!(store.item("message:missing").is_none());
    assert_eq!(text(&store, "message:a"), "hello");
    let touched = store.apply(vec![
        TranscriptOp::AppendBlock {
            id: "block:tools:run".into(),
            delta: BlockDelta {
                text: "\nput".into(),
                item_state: Some(TranscriptItemState::Complete),
                ..Default::default()
            },
        },
        TranscriptOp::ExtendRow {
            id: "activity:block:tools:run".into(),
            items: vec![TranscriptChild::Item("message:a".into())],
            kind: TranscriptRowKind::Activity {
                counts: ActivityCounts {
                    tool_calls: 1,
                    ..Default::default()
                },
            },
        },
        TranscriptOp::RemoveRow {
            id: "message:a".into(),
        },
    ]);
    assert_eq!(&*touched, [Arc::<str>::from("block:tools:run")]);
    assert_eq!(text(&store, "block:tools:run"), "out\nput");
    assert!(!store.item("block:tools:run").unwrap().is_pending());
    assert_eq!(order(&store), ["activity:block:tools:run"]);
    let activity = store.row("activity:block:tools:run").unwrap();
    assert_eq!(activity.items.len(), 2);
    assert!(
        store.item("message:a").is_some(),
        "a removed row keeps its items"
    );
    let (revision, structure, generation) = (
        store.revision(),
        store.structure_revision(),
        store.item("message:a").unwrap().generation(),
    );
    store.apply(vec![TranscriptOp::AppendText {
        id: "message:a".into(),
        text: "!".into(),
    }]);
    assert_ne!(store.revision(), revision, "appended text is a change");
    assert_eq!(store.structure_revision(), structure);
    assert_eq!(store.item("message:a").unwrap().generation(), generation);
    store.apply(vec![TranscriptOp::Item {
        item: message("message:a", "replaced"),
    }]);
    assert_ne!(store.structure_revision(), structure);
    assert_ne!(store.item("message:a").unwrap().generation(), generation);
}

#[test]
fn derived_data_is_cached_until_the_item_changes() {
    let mut store = TranscriptStore::default();
    store.apply(vec![TranscriptOp::Item {
        item: block("run", "before"),
    }]);
    let builds = std::cell::Cell::new(0);
    let derive = |entry: &crate::TranscriptEntry| {
        builds.set(builds.get() + 1);
        entry.block().unwrap().text.clone()
    };
    for _ in 0..2 {
        assert_eq!(
            store
                .item("block:tools:run")
                .unwrap()
                .derived(derive)
                .map(String::as_str),
            Some("before")
        );
    }
    assert_eq!(builds.get(), 1);
    store.apply(vec![TranscriptOp::AppendBlock {
        id: "block:tools:run".into(),
        delta: BlockDelta {
            text: " after".into(),
            ..Default::default()
        },
    }]);
    assert_eq!(
        store
            .item("block:tools:run")
            .unwrap()
            .derived(derive)
            .map(String::as_str),
        Some("before after")
    );
    assert_eq!(builds.get(), 2);
}

#[test]
fn working_becomes_worked_under_the_same_row() {
    let worked = |duration_ms| TranscriptRowKind::Worked {
        counts: ActivityCounts::default(),
        started_at_ms: 10,
        duration_ms,
    };
    let mut store = TranscriptStore::default();
    store.apply(vec![
        TranscriptOp::Item {
            item: block("run", ""),
        },
        TranscriptOp::Row {
            row: row("worked:turn", 2, &["block:tools:run"], worked(None)),
        },
    ]);
    store.apply(vec![TranscriptOp::Row {
        row: row("worked:turn", 2, &["block:tools:run"], worked(Some(5_000))),
    }]);
    assert_eq!(order(&store), ["worked:turn"]);
    assert_eq!(store.row("worked:turn").unwrap().kind, worked(Some(5_000)));
}

fn children(store: &TranscriptStore, id: &str) -> Vec<String> {
    let row = store.row(id).expect("row");
    store
        .row_children(row)
        .map(|child| match child {
            RowChild::Item(entry) => format!("item {}", entry.id),
            RowChild::Row(row) => format!("row {}", row.id),
        })
        .collect()
}

fn run(tool_calls: u32) -> TranscriptRowKind {
    TranscriptRowKind::Activity {
        counts: ActivityCounts {
            tool_calls,
            ..Default::default()
        },
    }
}

/// A question and a Worked row of commentary, run (2 tools), commentary, run (1 tool),
/// the second event extending the Worked row before its new run's `Row` op.
fn nested_store() -> TranscriptStore {
    let worked = || TranscriptRowKind::Worked {
        counts: ActivityCounts::default(),
        started_at_ms: 0,
        duration_ms: None,
    };
    let mut store = TranscriptStore::default();
    store.apply(vec![
        TranscriptOp::Item {
            item: message("message:q", "question"),
        },
        TranscriptOp::Item {
            item: message("assistant:s1", "looking"),
        },
        TranscriptOp::Item {
            item: block("a", "ls"),
        },
        TranscriptOp::Row {
            row: entry("message:q", 1),
        },
        TranscriptOp::Row {
            row: Arc::new(TranscriptRow {
                id: "worked:turn".into(),
                position: TranscriptPosition {
                    depth: 0,
                    sequence: 2,
                    index: 0,
                },
                items: vec![
                    TranscriptChild::Item("assistant:s1".into()),
                    TranscriptChild::Row("activity:block:tools:a".into()),
                ],
                kind: worked(),
            }),
        },
        TranscriptOp::Row {
            row: row("activity:block:tools:a", 3, &["block:tools:a"], run(1)),
        },
    ]);
    assert_eq!(order(&store), ["message:q", "worked:turn"]);
    store.apply(vec![
        TranscriptOp::Item {
            item: block("b", "cat"),
        },
        TranscriptOp::Item {
            item: message("assistant:s2", "found it"),
        },
        TranscriptOp::Item {
            item: block("c", "grep"),
        },
        TranscriptOp::ExtendRow {
            id: "activity:block:tools:a".into(),
            items: vec![TranscriptChild::Item("block:tools:b".into())],
            kind: run(2),
        },
        TranscriptOp::ExtendRow {
            id: "worked:turn".into(),
            items: vec![
                TranscriptChild::Item("assistant:s2".into()),
                TranscriptChild::Row("activity:block:tools:c".into()),
            ],
            kind: worked(),
        },
        TranscriptOp::Row {
            row: row("activity:block:tools:c", 5, &["block:tools:c"], run(1)),
        },
    ]);
    store
}

/// A Worked row reads commentary, run, commentary: each run is a nested row the parent
/// names, never a top-level row, and an `ExtendRow` naming a run before its `Row` op
/// resolves once the whole event is applied.
#[test]
fn nested_runs_resolve_in_place_and_stay_out_of_the_top_level() {
    let store = nested_store();
    assert_eq!(order(&store), ["message:q", "worked:turn"]);
    assert_eq!(
        children(&store, "worked:turn"),
        [
            "item assistant:s1",
            "row activity:block:tools:a",
            "item assistant:s2",
            "row activity:block:tools:c",
        ]
    );
    assert_eq!(
        children(&store, "activity:block:tools:a"),
        ["item block:tools:a", "item block:tools:b"]
    );
    assert_eq!(store.row("activity:block:tools:a").unwrap().kind, run(2));
    let worked_items = store.row("worked:turn").unwrap();
    let flat: Vec<_> = store
        .row_items(worked_items)
        .map(|entry| &*entry.id)
        .collect();
    assert_eq!(
        flat,
        [
            "assistant:s1",
            "block:tools:a",
            "block:tools:b",
            "assistant:s2",
            "block:tools:c",
        ]
    );
    assert_eq!(store.entries().count(), 6);
    assert_eq!(store.page().rows.len(), 4, "a page keeps the nested runs");
}

#[test]
fn removing_a_nested_run_drops_it_from_its_parent() {
    let mut store = nested_store();
    store.apply(vec![TranscriptOp::RemoveRow {
        id: "activity:block:tools:c".into(),
    }]);
    assert_eq!(
        children(&store, "worked:turn"),
        [
            "item assistant:s1",
            "row activity:block:tools:a",
            "item assistant:s2",
        ]
    );
    assert_eq!(order(&store), ["message:q", "worked:turn"]);

    store.apply(vec![TranscriptOp::RemoveRow {
        id: "worked:turn".into(),
    }]);
    assert_eq!(
        order(&store),
        ["message:q", "activity:block:tools:a"],
        "a run no held row lists is top-level"
    );
}

#[test]
fn a_moved_row_sorts_once_at_its_new_position() {
    let mut store = TranscriptStore::default();
    store.apply(vec![
        TranscriptOp::Item {
            item: message("message:a", "a"),
        },
        TranscriptOp::Item {
            item: message("message:b", "b"),
        },
        TranscriptOp::Row {
            row: entry("message:a", 1),
        },
        TranscriptOp::Row {
            row: entry("message:b", 2),
        },
        TranscriptOp::Row {
            row: entry("message:a", 3),
        },
    ]);
    assert_eq!(order(&store), ["message:b", "message:a"]);
}

#[test]
fn selected_chat_applies_replay_ops_but_only_live_committed_statistics() {
    let mut state = GatewayState::default();
    state.apply(&mut crate::fixture::opened("session", 0));
    let mut previous = Arc::clone(&state.session.as_ref().unwrap().run_stats);
    let mut stats = mobius_gateway::wire::RunStats::default();
    stats.completed.run_count = 7;
    stats.completed.elapsed_ms = 4_200;
    for (sequence, (session, replaying, metadata)) in [
        ("other", false, true),
        ("session", true, true),
        ("session", false, true),
        ("other", false, true),
        ("session", false, false),
    ]
    .into_iter()
    .enumerate()
    {
        state.replaying = replaying;
        stats.completed.run_count += 1;
        let frame = serde_json::json!({
            "type": "agent_event", "session_id": session,
            "run_stats": metadata.then_some(&stats),
            "record": {"sequence": sequence + 1, "recorded_at_ms": 0, "stream_metrics": [], "preview": null,
                "event": {"submission_id": null, "msg": {"type": "context_compacted"}}},
            "transcript": [
                {"op": "item", "item": {"id": format!("notice:{session}"), "turn_id": null,
                    "recorded_at_ms": 0, "state": "complete", "target": null,
                    "kind": "notice", "tone": "neutral", "text": "Compacted"}},
                {"op": "row", "row": {"id": format!("notice:{session}"),
                    "position": {"depth": 0, "sequence": 1, "index": 0},
                    "items": [{"item": format!("notice:{session}")}], "kind": "notice"}}
            ]
        });
        let mut message: ServerMessage = serde_json::from_value(frame).unwrap();
        state.apply(&mut message);
        let current = &state.session.as_ref().unwrap().run_stats;
        if session == "session" && !replaying && metadata {
            assert_eq!(**current, stats);
        } else {
            assert_eq!(
                **current, *previous,
                "replay, other chats and missing metadata preserve the latest snapshot"
            );
        }
        previous = Arc::clone(current);
    }
    assert_eq!(order(&state.transcript), ["notice:session"]);
}

fn preview(update: FrontendPreviewUpdate, page: TranscriptPage) -> RenderedPreview {
    RenderedPreview {
        kind: FrontendPreviewKind::Agent,
        state: None,
        symbol: None,
        duration_ms: None,
        started_at_ms: None,
        id: "preview".into(),
        title: "Agent".into(),
        subtitle: String::new().into(),
        page_id: "page".into(),
        update,
        transcript: page,
        next: None,
    }
}

#[test]
fn preview_pages_prepend_history_and_lay_live_pages_over_it() {
    let mut state = GatewayState {
        selected_session_id: Some(SessionId::new("session").unwrap()),
        ..Default::default()
    };
    let mut deliver = |preview: RenderedPreview| {
        let (msg, page) = preview.into_parts();
        let mut message = ServerMessage::AgentEvent {
            session_id: SessionId::new("session").unwrap(),
            record: serde_json::from_value(serde_json::json!({
                "sequence": 1, "recorded_at_ms": 0, "stream_metrics": [], "preview": page,
                "event": {"submission_id": null, "msg": msg}
            }))
            .unwrap(),
            transcript: Vec::new(),
            run_stats: None,
        };
        state.apply(&mut message);
    };
    deliver(preview(
        FrontendPreviewUpdate::Replace,
        TranscriptPage {
            items: vec![message("message:new", "new")],
            rows: vec![entry("message:new", 5)],
        },
    ));
    deliver(preview(
        FrontendPreviewUpdate::Prepend,
        TranscriptPage {
            items: vec![message("message:old", "old")],
            rows: vec![entry("message:old", 1)],
        },
    ));
    deliver(preview(
        FrontendPreviewUpdate::Replace,
        TranscriptPage {
            items: vec![message("message:new", "newer")],
            rows: vec![entry("message:new", 5)],
        },
    ));
    let shown = &state.previews["preview"].transcript;
    assert_eq!(order(shown), ["message:old", "message:new"]);
    assert_eq!(text(shown, "message:new"), "newer");
}

pub(crate) mod catalog {
    use mobius::SessionId;
    use mobius_gateway::wire::{
        HarnessConfig, ReadyPayload, ReadySection, ServerMessage, SessionRecord, SessionSlot,
        apply_session_changes,
    };
    use serde_json::json;
    use std::sync::Arc;

    use crate::GatewayState;

    fn session(id: &str) -> SessionRecord {
        SessionRecord {
            session_id: SessionId::new(id).unwrap(),
            session_context: Default::default(),
            parent_session_id: None,
            parent_sequence: None,
            sequence: 1,
            first_user_message: None,
            execution_stats: Default::default(),
            title: None,
            pinned: false,
            activity: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    pub(crate) fn ready(sessions: &[SessionRecord], bots: usize) -> ReadyPayload {
        let bots: Vec<_> = (0..bots)
            .map(|index| {
                json!({
                    "id": format!("bot-{index}"), "handle": format!("bot{index}"),
                    "conversation_session_id": format!("conversation-{index}"),
                    "name": "Bot", "description": "", "tint": "blue", "shape": "circle",
                    "harness": "mobius",
                    "config": {"revision": 1, "config": HarnessConfig::Mobius(Arc::default())},
                    "capabilities": {"attachments": false, "computer": false, "voice": {"state": "off"}, "memory": {"enabled": false, "global": false}},
                    "routine_interaction_policy": "unattended"
                })
            })
            .collect();
        serde_json::from_value(json!({
            "gateway_version": "test", "machine_name": "test", "computer_view": "unavailable", "bots": bots,
            "sessions": sessions, "background_approvals": [],
            "agent_catalog": {"bot_defaults": null, "default_capabilities": null,
                "middleware_features": [], "extensions": [], "contributions": [],
                "models": [], "model_providers": {}, "image_models": [], "voice_models": []},
            "harnesses": [], "max_active_sessions": 1,
            "revisions": {}, "omitted": [],
            "provider_instances": [], "providers": [],
            "session_file_limits": mobius::backend::session_files::session_file_limits()
        }))
        .unwrap()
    }

    fn ids(state: &GatewayState) -> Vec<&str> {
        state
            .sessions
            .iter()
            .map(|session| session.session_id.as_ref())
            .collect()
    }

    #[test]
    fn deltas_and_omitted_sections_rebuild_the_held_catalog() {
        let mut state = GatewayState::default();
        state.apply(&mut ServerMessage::Ready {
            payload: ready(&[session("a"), session("b")], 1),
        });
        apply_session_changes(
            &mut state.sessions,
            vec![
                SessionSlot::Changed(Box::new(session("c"))),
                SessionSlot::Unchanged(SessionId::new("b").unwrap()),
                SessionSlot::Unchanged(SessionId::new("a").unwrap()),
            ],
        );
        assert_eq!(ids(&state), ["c", "b", "a"]);

        let mut unchanged = ready(&[], 0);
        unchanged.omitted = [ReadySection::Sessions, ReadySection::Bots].into();
        unchanged.machine_name = "renamed".into();
        state.apply(&mut ServerMessage::Ready { payload: unchanged });
        let gateway = state.gateway.as_ref().unwrap();
        assert_eq!(gateway.machine_name, "renamed");
        assert_eq!(gateway.bots.len(), 1);
        assert!(gateway.omitted.is_empty());
        assert_eq!(ids(&state), ["c", "b", "a"]);

        state.apply(&mut ServerMessage::Ready {
            payload: ready(&[session("d")], 0),
        });
        assert_eq!(ids(&state), ["d"]);
        assert!(state.gateway.as_ref().unwrap().bots.is_empty());
        apply_session_changes(&mut state.sessions, Vec::new());
        assert!(
            state.sessions.is_empty(),
            "a delta lists every chat that remains"
        );
    }

    #[test]
    fn each_memories_answer_replaces_the_listed_memories() {
        let mut state = GatewayState::default();
        let mut listed = serde_json::from_value(serde_json::json!({
            "type": "memories", "request_id": "list", "memories": [{
                "id": "m1", "name": "package-manager", "description": "Package manager",
                "links": [], "harness": "mobius", "project": {"id": "ws-1", "path": "/work/one"},
                "bot_id": "bot-a", "global": false, "text": "uses pnpm",
                "updated_at": 1_761_000_000, "editable": true
            }]
        }))
        .unwrap();
        state.apply(&mut listed);
        let memories = state.memories.as_deref().unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(&*memories[0].text, "uses pnpm");

        state.apply(&mut ServerMessage::Memories {
            request_id: "forget".into(),
            memories: Vec::new(),
        });
        assert_eq!(state.memories.as_deref().map(<[_]>::len), Some(0));
    }
}
