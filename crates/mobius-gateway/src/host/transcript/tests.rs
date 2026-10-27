use std::collections::BTreeMap;
use std::sync::Arc;

use mobius::SessionId;
use mobius::protocol::{
    AssistantContentDeltaEvent, AssistantMessageEvent, ContentPart, ErrorEvent, ErrorKind, Event,
    EventMsg, FrontendBlock, FrontendBlockFormat, FrontendBlockRole, FrontendBlockState,
    FrontendBlockUpdate, FrontendLink, FrontendTone, MessageAuthor, MessageDelivery, MessageEvent,
    MessageTarget, ModelStepContent, ModelStepContentPhase, RenderedBlock, SessionFileReference,
    ToolContent, TurnAbortedEvent, TurnCompleteEvent, TurnStartedEvent, UiBlock,
};

use super::TranscriptReducer;
use crate::wire::{
    ActivityCounts, TranscriptChild, TranscriptItem, TranscriptItemContent, TranscriptItemState,
    TranscriptOp, TranscriptPage, TranscriptRow, TranscriptRowKind,
};

/// A client store: what a frontend holds after applying pages and ops.
#[derive(Debug, Default, PartialEq)]
struct Client {
    items: BTreeMap<Arc<str>, TranscriptItem>,
    rows: BTreeMap<Arc<str>, TranscriptRow>,
}

impl Client {
    fn from_page(page: &TranscriptPage) -> Self {
        Self {
            items: page
                .items
                .iter()
                .map(|item| (Arc::clone(&item.id), (**item).clone()))
                .collect(),
            rows: page
                .rows
                .iter()
                .map(|row| (Arc::clone(&row.id), (**row).clone()))
                .collect(),
        }
    }

    fn apply(&mut self, ops: &[TranscriptOp]) {
        for op in ops {
            match op {
                TranscriptOp::Item { item } => {
                    self.items.insert(Arc::clone(&item.id), (**item).clone());
                }
                TranscriptOp::AppendText { id, text } => {
                    if let Some(item) = self.items.get_mut(id) {
                        match &mut item.content {
                            TranscriptItemContent::Message { message } => {
                                message.text.push_str(text);
                            }
                            TranscriptItemContent::Assistant { text: current, .. } => {
                                current.push_str(text);
                            }
                            _ => panic!("append to a non-text item"),
                        }
                    }
                }
                TranscriptOp::AppendBlock { id, delta } => {
                    let item = self.items.get_mut(id).expect("append to a known item");
                    let TranscriptItemContent::Block {
                        block, group_title, ..
                    } = &mut item.content
                    else {
                        panic!("append to a non-block item");
                    };
                    block.text.push_str(&delta.text);
                    block.links.extend(delta.links.iter().cloned());
                    for file in &delta.files {
                        match block.files.iter_mut().find(|current| current.id == file.id) {
                            Some(current) => *current = file.clone(),
                            None => block.files.push(file.clone()),
                        }
                    }
                    block.content.0.extend(delta.content.iter().cloned());
                    block.ui.extend(delta.ui.iter().cloned());
                    if let Some(title) = &delta.title {
                        block.title.clone_from(title);
                    }
                    if let Some(state) = delta.state {
                        block.state = state;
                    }
                    if let Some(role) = delta.role {
                        block.role = role;
                    }
                    if let Some(tone) = delta.tone {
                        block.tone = tone;
                    }
                    if let Some(format) = delta.format {
                        block.format = format;
                    }
                    if delta.symbol.is_some() {
                        block.symbol.clone_from(&delta.symbol);
                    }
                    if delta.group.is_some() {
                        block.group.clone_from(&delta.group);
                    }
                    if delta.group_title.is_some() {
                        group_title.clone_from(&delta.group_title);
                    }
                    if let Some(state) = delta.item_state {
                        item.state = state;
                    }
                }
                TranscriptOp::Row { row } => {
                    self.rows.insert(Arc::clone(&row.id), (**row).clone());
                }
                TranscriptOp::ExtendRow { id, items, kind } => {
                    let row = self.rows.get_mut(id).expect("extend a known row");
                    assert_eq!(
                        std::mem::discriminant(&row.kind),
                        std::mem::discriminant(kind)
                    );
                    row.items.extend(items.iter().cloned());
                    row.kind = kind.clone();
                }
                TranscriptOp::RemoveRow { id } => {
                    self.rows.remove(id);
                }
            }
        }
        for row in self.rows.values() {
            for child in &row.items {
                if let TranscriptChild::Row(id) = child {
                    assert!(self.rows.contains_key(id), "{} nests unknown {id}", row.id);
                }
            }
        }
    }

    /// Top-level row ids in display order: every row no row lists as a child.
    fn order(&self) -> Vec<&str> {
        let nested = self
            .rows
            .values()
            .flat_map(|row| &row.items)
            .filter_map(|child| match child {
                TranscriptChild::Row(id) => Some(&**id),
                TranscriptChild::Item(_) => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let mut rows = self
            .rows
            .values()
            .filter(|row| !nested.contains(&*row.id))
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| row.position);
        rows.into_iter().map(|row| &*row.id).collect()
    }

    /// A row's item ids in display order, nested rows drawn in place.
    fn shown(&self, id: &str) -> Vec<&str> {
        self.row(id)
            .items
            .iter()
            .flat_map(|child| match child {
                TranscriptChild::Item(id) => vec![&**id],
                TranscriptChild::Row(id) => self.shown(id),
            })
            .collect()
    }

    /// A worked row's shown items and duration.
    fn worked(&self, id: &str) -> (Vec<&str>, Option<u64>) {
        let TranscriptRowKind::Worked { duration_ms, .. } = &self.row(id).kind else {
            panic!("{id} is not a worked row");
        };
        (self.shown(id), *duration_ms)
    }

    fn row(&self, id: &str) -> &TranscriptRow {
        self.rows.get(id).unwrap_or_else(|| panic!("no row {id}"))
    }

    fn item(&self, id: &str) -> &TranscriptItem {
        self.items.get(id).unwrap_or_else(|| panic!("no item {id}"))
    }
}

struct Chat {
    reducer: TranscriptReducer,
    sequence: u64,
    client: Client,
}

impl Chat {
    fn new(depth: u32) -> Self {
        Self {
            reducer: TranscriptReducer::new(depth),
            sequence: 0,
            client: Client::default(),
        }
    }

    fn send(&mut self, submission: Option<&str>, msg: EventMsg, blocks: Vec<RenderedBlock>) {
        let ops = self.event(submission, msg, blocks);
        self.client.apply(&ops);
    }

    fn event(
        &mut self,
        submission: Option<&str>,
        msg: EventMsg,
        blocks: Vec<RenderedBlock>,
    ) -> Vec<TranscriptOp> {
        self.sequence += 1;
        let event = Event {
            submission_id: submission.map(Into::into),
            msg,
        };
        self.reducer.apply(
            self.sequence,
            i64::try_from(self.sequence).unwrap() * 1000,
            &event,
            blocks,
        )
    }
}

fn started(turn: &str) -> EventMsg {
    EventMsg::TurnStarted(TurnStartedEvent {
        idle: false,
        turn_id: turn.into(),
        model_context_window: None,
    })
}

fn completed(turn: &str) -> EventMsg {
    EventMsg::TurnComplete(TurnCompleteEvent {
        idle: false,
        turn_id: turn.into(),
    })
}

fn user(text: &str, target: Option<u64>) -> EventMsg {
    EventMsg::Message(MessageEvent {
        author: MessageAuthor::User,
        delivery: MessageDelivery::Turn,
        text: text.into(),
        attachments: Vec::new(),
        reply: None,
        message_target: target.map(|checkpoint_sequence| MessageTarget {
            checkpoint_sequence,
            batch_item_count: std::num::NonZeroUsize::MIN,
        }),
    })
}

fn configured() -> EventMsg {
    EventMsg::SessionConfigured(Arc::new(mobius::protocol::SessionConfiguredEvent {
        session_id: SessionId::new("chat").unwrap(),
        context: Arc::default(),
        model: mobius::protocol::ModelChangedEvent {
            route: "route".into(),
            model: "model".into(),
            reasoning_effort: None,
            model_context_window: None,
        },
    }))
}

fn delta(turn: &str, step: &str, phase: ModelStepContentPhase, text: &str) -> EventMsg {
    EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
        session_id: SessionId::new("chat").unwrap(),
        turn_id: turn.into(),
        model_step_id: step.into(),
        delta: text.into(),
        phase,
    })
}

fn answer(turn: &str, step: &str, parts: &[(ModelStepContentPhase, &str)]) -> EventMsg {
    EventMsg::AssistantMessage(AssistantMessageEvent {
        session_id: SessionId::new("chat").unwrap(),
        turn_id: turn.into(),
        model_step_id: step.into(),
        content: parts
            .iter()
            .enumerate()
            .map(|(index, (phase, text))| ModelStepContent {
                output_index: index,
                part_index: 0,
                phase: *phase,
                text: (*text).into(),
                annotations: Vec::new(),
            })
            .collect(),
        message_target: Some(MessageTarget {
            checkpoint_sequence: 9,
            batch_item_count: std::num::NonZeroUsize::MIN,
        }),
    })
}

fn tool(id: &str, block: FrontendBlock) -> Vec<RenderedBlock> {
    vec![RenderedBlock {
        capability: "tools".into(),
        block: FrontendBlock {
            id: Some(id.into()),
            role: FrontendBlockRole::Tool,
            ..block
        },
    }]
}

fn notice(tone: FrontendTone, title: &str, text: &str) -> Vec<RenderedBlock> {
    vec![RenderedBlock {
        capability: "framework".into(),
        block: FrontendBlock {
            role: FrontendBlockRole::Notice,
            title: title.into(),
            text: text.into(),
            tone,
            ..FrontendBlock::default()
        },
    }]
}

fn diff(text: &str, state: FrontendBlockState) -> FrontendBlock {
    FrontendBlock {
        format: FrontendBlockFormat::UnifiedDiff,
        state,
        text: text.into(),
        ..FrontendBlock::default()
    }
}

fn error(message: &str) -> EventMsg {
    EventMsg::Error(ErrorEvent {
        kind: ErrorKind::Provider,
        message: message.into(),
        retryable: false,
        status: None,
        retry_after: None,
    })
}

fn aborted(turn: &str, reason: &str) -> EventMsg {
    EventMsg::TurnAborted(TurnAbortedEvent {
        idle: false,
        turn_id: turn.into(),
        reason: reason.into(),
    })
}

use FrontendBlockState::{Complete, Pending};
use ModelStepContentPhase::{Commentary, FinalAnswer, Reasoning};

/// Two turns with streaming, appended tool output, a diff, an error and an abort, plus a
/// notice outside any turn; the agent restarts before the second, resuming the chat.
fn session() -> Vec<(Option<&'static str>, EventMsg, Vec<RenderedBlock>)> {
    let begin = FrontendBlock {
        state: Pending,
        title: "Bash · cargo test".into(),
        text: "cargo test".into(),
        ..FrontendBlock::default()
    };
    let output = FrontendBlock {
        update: FrontendBlockUpdate::Append,
        state: Complete,
        group: Some("command".into()),
        text: "ok".into(),
        ..FrontendBlock::default()
    };
    vec![
        (None, configured(), Vec::new()),
        (None, started("a"), Vec::new()),
        (Some("ask-a"), user("fix it", Some(1)), Vec::new()),
        (None, delta("a", "s1", Reasoning, "think"), Vec::new()),
        (None, delta("a", "s1", Reasoning, "ing"), Vec::new()),
        (None, delta("a", "s1", Commentary, "looking"), Vec::new()),
        (
            None,
            answer(
                "a",
                "s1",
                &[(Reasoning, "thinking"), (Commentary, "looking")],
            ),
            Vec::new(),
        ),
        (None, EventMsg::ContextCompacted, tool("bash", begin)),
        (None, EventMsg::ContextCompacted, tool("bash", output)),
        (
            None,
            EventMsg::ContextCompacted,
            tool("patch", diff("+a\n-b\n+c", Complete)),
        ),
        (None, delta("a", "s2", FinalAnswer, "do"), Vec::new()),
        (None, delta("a", "s2", FinalAnswer, "ne"), Vec::new()),
        (
            None,
            answer("a", "s2", &[(FinalAnswer, "done")]),
            Vec::new(),
        ),
        (None, completed("a"), Vec::new()),
        (
            None,
            EventMsg::ContextCompacted,
            notice(FrontendTone::Neutral, "Compacted", "between turns"),
        ),
        (None, configured(), Vec::new()),
        (None, started("b"), Vec::new()),
        (Some("ask-b"), user("again", Some(2)), Vec::new()),
        (None, delta("b", "s3", FinalAnswer, "partial"), Vec::new()),
        (
            None,
            error("provider failed"),
            notice(FrontendTone::Error, "Error", "provider failed"),
        ),
        (
            None,
            aborted("b", "provider failed"),
            notice(FrontendTone::Warning, "Turn aborted", "provider failed"),
        ),
    ]
}

#[test]
fn ops_bring_a_client_from_any_snapshot_to_the_final_snapshot() {
    let mut chat = Chat::new(0);
    let mut snapshots = vec![chat.reducer.snapshot()];
    let mut ops = Vec::new();
    for (submission, msg, blocks) in session() {
        ops.push(chat.event(submission, msg, blocks));
        snapshots.push(chat.reducer.snapshot());
    }
    for op in ["append_text", "append_block", "row", "extend_row"] {
        assert!(
            serde_json::to_string(&ops)
                .unwrap()
                .contains(&format!("\"op\":\"{op}\"")),
            "the session covers {op}"
        );
    }
    let last = snapshots.last().unwrap();
    let expected = Client::from_page(last);
    for (prefix, snapshot) in snapshots.iter().enumerate() {
        let mut client = Client::from_page(snapshot);
        for event_ops in &ops[prefix..] {
            client.apply(event_ops);
        }
        assert_eq!(client, expected, "resuming after event {prefix}");
    }
    let mut ordered = last.rows.iter().map(|row| row.position).collect::<Vec<_>>();
    ordered.sort();
    assert_eq!(
        ordered,
        last.rows.iter().map(|row| row.position).collect::<Vec<_>>(),
        "snapshot rows are in position order"
    );
}

#[test]
fn only_the_first_turn_after_an_agent_restart_opens_resumed() {
    let mut chat = Chat::new(0);
    for (submission, msg, blocks) in session() {
        chat.send(submission, msg, blocks);
    }
    chat.send(None, started("c"), Vec::new());
    chat.send(Some("ask-c"), user("more", None), Vec::new());
    let resumed = |id: &str| chat.client.item(id).resumed;
    assert!(!resumed("message:ask-a"), "the chat's first turn");
    assert!(resumed("message:ask-b"), "the turn after the restart");
    assert!(!resumed("message:ask-c"), "a later turn of the same run");
}

#[test]
fn a_turn_works_under_one_row_that_keeps_its_id_when_it_ends() {
    let mut chat = Chat::new(0);
    let script = session();
    let running = script
        .iter()
        .position(|(_, msg, _)| matches!(msg, EventMsg::TurnComplete(_)));
    for (index, (submission, msg, blocks)) in script.into_iter().enumerate() {
        if Some(index) == running {
            let TranscriptRowKind::Worked { started_at_ms, .. } = chat.client.row("worked:a").kind
            else {
                unreachable!()
            };
            assert_eq!(started_at_ms, 2000);
            assert_eq!(
                chat.client.worked("worked:a"),
                (
                    vec![
                        "assistant:s1:reasoning",
                        "assistant:s1:commentary",
                        "block:tools:bash",
                        "block:tools:patch",
                    ],
                    None
                ),
                "while running, the streaming answer stays below the work"
            );
            assert_eq!(
                chat.client.order(),
                ["message:ask-a", "worked:a", "assistant:s2:final_answer"]
            );
        }
        chat.send(submission, msg, blocks);
    }
    assert_eq!(chat.client.worked("worked:a").1, Some(12_000));
    assert!(chat.client.item("assistant:s2:final_answer").terminal);
    assert_eq!(
        chat.client.order(),
        [
            "message:ask-a",
            "worked:a",
            "assistant:s2:final_answer",
            "block:15:3",
            "message:ask-b",
            "assistant:s3:final_answer",
            "block:20:3",
        ],
        "the error stays in view and the abort repeating it is dropped"
    );
    let partial = chat.client.item("assistant:s3:final_answer");
    assert!(partial.terminal, "an aborted turn ends on its last answer");
    assert_eq!(partial.state, TranscriptItemState::Complete);
}

#[test]
fn turn_changes_ride_only_the_last_terminal_answer_and_skip_pending_diffs() {
    let mut chat = Chat::new(0);
    chat.send(None, started("t"), Vec::new());
    chat.send(
        None,
        answer("t", "s1", &[(FinalAnswer, "first")]),
        Vec::new(),
    );
    chat.send(
        None,
        EventMsg::ContextCompacted,
        tool(
            "one",
            diff(
                "--- a\n+++ b\n@@ -1,2 +1,4 @@\n+x\n+++ added\n--- removed\n same\n+y\n",
                Complete,
            ),
        ),
    );
    chat.send(
        None,
        EventMsg::ContextCompacted,
        tool("two", diff("+pending", Pending)),
    );
    chat.send(
        None,
        answer("t", "s2", &[(FinalAnswer, "last")]),
        Vec::new(),
    );
    assert!(
        chat.client.rows.values().all(|row| !matches!(
            row.kind,
            TranscriptRowKind::Entry {
                turn_changes: Some(_)
            }
        )),
        "a running turn has no changes yet"
    );
    chat.send(None, completed("t"), Vec::new());
    let TranscriptRowKind::Entry {
        turn_changes: Some(changes),
    } = &chat.client.row("assistant:s2:final_answer").kind
    else {
        panic!("the last answer carries the turn's changes");
    };
    assert_eq!(changes.diffs, [Arc::from("block:tools:one")]);
    assert_eq!((changes.totals.additions, changes.totals.deletions), (3, 1));
    assert!(!chat.client.rows.contains_key("assistant:s1:final_answer"));
}

#[test]
fn a_streamed_answer_appends_text_then_the_final_message_replaces_it() {
    let mut chat = Chat::new(0);
    chat.send(None, started("t"), Vec::new());
    let first = chat.event(None, delta("t", "s", FinalAnswer, "Hel"), Vec::new());
    assert!(
        matches!(&first[..], [TranscriptOp::Item { item }, TranscriptOp::Row { .. }]
        if item.state == TranscriptItemState::Streaming)
    );
    chat.client.apply(&first);
    let next = chat.event(None, delta("t", "s", FinalAnswer, "lo"), Vec::new());
    assert_eq!(
        next,
        [TranscriptOp::AppendText {
            id: "assistant:s:final_answer".into(),
            text: "lo".into(),
        }]
    );
    let last = chat.event(
        None,
        answer("t", "s", &[(FinalAnswer, "Hello!")]),
        Vec::new(),
    );
    let [TranscriptOp::Item { item }] = &last[..] else {
        panic!("the final message only replaces the item: {last:?}");
    };
    assert_eq!(item.state, TranscriptItemState::Complete);
    assert!(item.target.is_some());
    assert!(
        matches!(&item.content, TranscriptItemContent::Assistant { text, .. } if text == "Hello!")
    );
}

#[test]
fn appended_blocks_merge_and_later_blocks_of_a_group_show_its_first_title() {
    let mut chat = Chat::new(0);
    let link = |label: &str| FrontendLink {
        label: label.into(),
        href: format!("file:///{label}"),
    };
    let file = |id: &str, size| SessionFileReference {
        id: id.into(),
        name: id.into(),
        size,
        media_type: "text/plain".into(),
    };
    chat.send(
        None,
        EventMsg::ContextCompacted,
        tool(
            "run",
            FrontendBlock {
                title: "Bash · make".into(),
                text: "make".into(),
                links: vec![link("a")],
                files: vec![file("f", 1)],
                content: ToolContent(vec![ContentPart::Text { text: "one".into() }]),
                ui: vec![UiBlock::Section { title: "u1".into() }],
                ..FrontendBlock::default()
            },
        ),
    );
    chat.send(
        None,
        EventMsg::ContextCompacted,
        tool(
            "run",
            FrontendBlock {
                update: FrontendBlockUpdate::Append,
                group: Some("cmd".into()),
                text: "built".into(),
                links: vec![link("a"), link("b")],
                files: vec![file("f", 2), file("g", 3)],
                content: ToolContent(vec![ContentPart::Text { text: "two".into() }]),
                ui: vec![UiBlock::Section { title: "u2".into() }],
                ..FrontendBlock::default()
            },
        ),
    );
    chat.send(
        None,
        EventMsg::ContextCompacted,
        tool(
            "poll",
            FrontendBlock {
                group: Some("cmd".into()),
                title: "Command · poll".into(),
                ..FrontendBlock::default()
            },
        ),
    );
    let TranscriptItemContent::Block {
        block, group_title, ..
    } = &chat.client.item("block:tools:run").content
    else {
        panic!("a block item");
    };
    assert_eq!(block.title, "Bash · make");
    assert_eq!(block.text, "make\nbuilt");
    assert_eq!(block.update, FrontendBlockUpdate::Replace);
    assert_eq!(block.group.as_deref(), Some("cmd"));
    assert_eq!(block.links, [link("a"), link("b")]);
    assert_eq!(block.files, [file("f", 2), file("g", 3)]);
    assert_eq!(block.content.0.len(), 2);
    assert_eq!(block.ui.len(), 2);
    assert_eq!(*group_title, None);
    let TranscriptItemContent::Block { group_title, .. } =
        &chat.client.item("block:tools:poll").content
    else {
        panic!("a block item");
    };
    assert_eq!(group_title.as_deref(), Some("Bash · make"));
    assert_eq!(
        chat.client.order(),
        ["activity:block:tools:run"],
        "loose activity of one group shares a row"
    );
}

#[test]
fn inherited_pages_prefix_ids_drop_targets_and_sort_above_the_fork_point() {
    let mut parent = Chat::new(1);
    parent.send(None, started("t"), Vec::new());
    parent.send(Some("ask"), user("hi", Some(4)), Vec::new());
    parent.send(
        None,
        answer("t", "s", &[(Reasoning, "hm"), (FinalAnswer, "hello")]),
        Vec::new(),
    );
    parent.reducer.finish_open_turns();
    let page = parent.reducer.page();
    let mut own = TranscriptReducer::new(0);
    own.fork_point(SessionId::new("parent").unwrap());
    let mut client = Client::from_page(&own.snapshot());
    for (id, item) in Client::from_page(&page).items {
        client.items.insert(id, item);
    }
    client.rows.extend(Client::from_page(&page).rows);
    assert_eq!(
        client.order(),
        [
            "fork:1:message:ask",
            "fork:1:worked:t",
            "fork:1:assistant:s:final_answer",
            "fork-point",
        ]
    );
    for item in &page.items {
        assert!(item.inherited && item.target.is_none(), "{}", item.id);
        if let TranscriptItemContent::Message { message } = &item.content {
            assert!(message.message_target.is_none());
        }
    }
    assert!(client.item("fork:1:assistant:s:final_answer").terminal);
    assert_eq!(client.row("fork-point").position.depth, 0);
}

#[test]
fn trimming_keeps_the_running_turn_and_history_continues_before_it() {
    let mut chat = Chat::new(0);
    for (submission, msg, blocks) in session() {
        chat.send(submission, msg, blocks);
    }
    chat.send(None, started("c"), Vec::new());
    chat.send(Some("ask-c"), user("more", None), Vec::new());
    assert_eq!(chat.reducer.history_before(), None);
    chat.reducer.trim_before(chat.sequence);
    let order = Client::from_page(&chat.reducer.snapshot());
    assert_eq!(order.order(), ["message:ask-c"]);
    assert_eq!(chat.reducer.history_before(), Some(chat.sequence - 1));
}

#[test]
fn a_long_turn_sends_ops_bounded_by_the_change_not_the_turn() {
    let mut chat = Chat::new(0);
    chat.send(None, started("t"), Vec::new());
    chat.send(Some("ask"), user("go", None), Vec::new());
    let chunk = "x".repeat(100);
    let mut events = Vec::new();
    for call in 0..500 {
        let begin = FrontendBlock {
            state: Pending,
            title: format!("Bash · step {call}"),
            ..FrontendBlock::default()
        };
        events.push(chat.event(
            None,
            EventMsg::ContextCompacted,
            tool(&format!("call-{call}"), begin),
        ));
        let output = FrontendBlock {
            update: FrontendBlockUpdate::Append,
            state: Pending,
            text: chunk.clone(),
            ..FrontendBlock::default()
        };
        events.push(chat.event(None, EventMsg::ContextCompacted, tool("run", output)));
    }
    for ops in &events {
        chat.client.apply(ops);
    }
    for ops in events.iter().skip(4) {
        let bytes = serde_json::to_vec(ops).unwrap().len();
        assert!(bytes < 768, "{bytes} bytes: {ops:?}");
    }
    assert!(matches!(
        &events.last().unwrap()[..],
        [TranscriptOp::AppendBlock { delta, .. }] if delta.text.len() == chunk.len() + 1
    ));
    assert_eq!(chat.client, Client::from_page(&chat.reducer.snapshot()));
    assert_eq!(chat.client.row("worked:t").items.len(), 1);
    assert_eq!(chat.client.shown("worked:t").len(), 501);
}

#[test]
fn a_worked_row_nests_each_run_of_work_between_its_commentary() {
    let mut chat = Chat::new(0);
    chat.send(None, started("t"), Vec::new());
    chat.send(Some("ask"), user("go", None), Vec::new());
    let call = |chat: &mut Chat, id: &str| {
        chat.event(
            None,
            EventMsg::ContextCompacted,
            tool(id, FrontendBlock::default()),
        )
    };
    chat.send(
        None,
        answer("t", "s1", &[(Commentary, "first")]),
        Vec::new(),
    );
    for id in ["a", "b"] {
        let ops = call(&mut chat, id);
        chat.client.apply(&ops);
    }
    let third = call(&mut chat, "c");
    chat.client.apply(&third);
    let tools = |tool_calls| ActivityCounts {
        tool_calls,
        ..ActivityCounts::default()
    };
    let [
        TranscriptOp::Item { .. },
        TranscriptOp::ExtendRow {
            id: worked_id,
            items: worked_items,
            kind: TranscriptRowKind::Worked { .. },
        },
        TranscriptOp::ExtendRow {
            id: run,
            items: run_items,
            kind: TranscriptRowKind::Activity { counts },
        },
    ] = &third[..]
    else {
        panic!("the third call extends its run and only re-counts the worked row: {third:?}");
    };
    assert_eq!(&**run, "activity:block:tools:a");
    assert_eq!(run_items, &[TranscriptChild::Item("block:tools:c".into())]);
    assert_eq!(*counts, tools(3));
    assert_eq!(&**worked_id, "worked:t");
    assert!(worked_items.is_empty());
    chat.send(
        None,
        answer("t", "s2", &[(Commentary, "second")]),
        Vec::new(),
    );
    for id in ["d", "e"] {
        let ops = call(&mut chat, id);
        chat.client.apply(&ops);
    }
    chat.send(
        None,
        answer("t", "s3", &[(FinalAnswer, "done")]),
        Vec::new(),
    );
    chat.send(None, completed("t"), Vec::new());
    assert_eq!(chat.client, Client::from_page(&chat.reducer.snapshot()));
    assert_eq!(
        chat.client.row("worked:t").items,
        [
            TranscriptChild::Item("assistant:s1:commentary".into()),
            TranscriptChild::Row("activity:block:tools:a".into()),
            TranscriptChild::Item("assistant:s2:commentary".into()),
            TranscriptChild::Row("activity:block:tools:d".into()),
        ]
    );
    for (run, count) in [("activity:block:tools:a", 3), ("activity:block:tools:d", 2)] {
        assert_eq!(
            chat.client.row(run).kind,
            TranscriptRowKind::Activity {
                counts: tools(count)
            }
        );
    }
    assert_eq!(
        chat.client.order(),
        ["message:ask", "worked:t", "assistant:s3:final_answer"],
        "nested runs are not top-level rows"
    );
}

#[test]
fn nested_preview_events_keep_their_order_within_one_sequence() {
    let mut reducer = TranscriptReducer::new(0);
    for (msg, blocks) in [
        (
            EventMsg::Warning(mobius::protocol::WarningEvent {
                message: "before".into(),
            }),
            Vec::new(),
        ),
        (
            EventMsg::ContextCompacted,
            tool("tool", FrontendBlock::default()),
        ),
        (
            EventMsg::Warning(mobius::protocol::WarningEvent {
                message: "after".into(),
            }),
            Vec::new(),
        ),
    ] {
        reducer.apply(
            1,
            1,
            &Event {
                submission_id: None,
                msg,
            },
            blocks,
        );
    }
    let page = reducer.page();
    assert_eq!(
        page.items.iter().map(|item| &*item.id).collect::<Vec<_>>(),
        ["notice:1:0", "block:tools:tool", "notice:1:7"]
    );
}
