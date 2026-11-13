//! Local UI states for snapshots: a synthetic transcript and session details laid over an
//! open chat. Nothing here talks to the gateway.

use super::*;
use crate::gateway::{TranscriptStore, fixture};
use mobius::protocol::{
    ApprovalCall, ContentPart, EventMsg, ExecApprovalRequestEvent, FrontendAction,
    FrontendActionListItem, FrontendBlock, FrontendBlockFormat, FrontendBlockRole,
    FrontendBlockState, FrontendBlockUpdate, FrontendLink, FrontendListItemState,
    FrontendPickerOption, FrontendPriority, FrontendSymbol, FrontendTone, FrontendWidget,
    FrontendWidgetContent, MessageAuthor, MessageDelivery, MessageEvent, ModelStepContentPhase,
    RenderedBlock, SessionFileReference, TokenUsage, TokenUsageInfo, ToolCallBeginEvent,
    ToolCallEndEvent, ToolContent,
};
use mobius_gateway::wire::{RenderedPreview, SessionWidget};
use std::rc::Rc;

/// Which state a fixture scenario shows.
#[derive(Clone, Copy, PartialEq)]
pub(in crate::ui) enum Scene {
    Transcript,
    Running,
    Approval,
    Attention,
    Picker,
    Dictation,
    Voice,
    /// A running turn with an injected steer, one the gateway holds and one still sending.
    Steer,
    /// A turn whose patch tool call is open: a unified diff with a long hunk.
    Patch,
    /// A fork: a turn inherited from its parent, the divider, and the fork's own turn,
    /// with earlier inherited history still to load.
    Fork,
}

const DONE: &str = "fixture-done";
const LIVE: &str = "fixture-live";

/// The scene's events, grouped as the gateway groups them.
#[derive(Default)]
struct Script(fixture::Script);

struct ToolFixture {
    call: ToolCallBeginEvent,
    block: FrontendBlock,
}

impl Script {
    fn user(
        &mut self,
        id: &str,
        text: &str,
        delivery: MessageDelivery,
        files: Vec<SessionFileReference>,
    ) {
        self.0.message(
            id,
            MessageEvent {
                author: MessageAuthor::User,
                delivery,
                text: text.into(),
                attachments: files,
                reply: None,
                message_target: None,
            },
        );
    }

    fn text(&mut self, turn: &str, step: &str, phase: ModelStepContentPhase, text: &str) {
        self.0.text(turn, step, phase, text);
    }

    fn turn(&mut self, turn: &str, start: bool) {
        self.0.turn(turn, start);
    }

    fn tool(&mut self, tool: ToolFixture) {
        let ToolFixture { call, block } = tool;
        let event = if block.state == FrontendBlockState::Pending {
            EventMsg::ToolCallBegin(call)
        } else {
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: call.turn_id,
                call_id: call.call_id,
                name: call.name,
                output: ToolContent::default(),
                is_error: block.tone == FrontendTone::Error,
            })
        };
        self.0.apply(
            None,
            event,
            vec![RenderedBlock {
                capability: "tools".into(),
                block,
            }],
        );
    }

    fn store(self) -> TranscriptStore {
        self.0.store()
    }
}

fn file(name: &str, size: u64, media_type: &str) -> SessionFileReference {
    SessionFileReference {
        id: format!("fixture-{name}"),
        name: name.into(),
        size,
        media_type: media_type.into(),
    }
}

fn tool(id: &str, turn: &str, title: &str, text: &str, format: FrontendBlockFormat) -> ToolFixture {
    ToolFixture {
        call: ToolCallBeginEvent {
            turn_id: turn.into(),
            call_id: id.into(),
            name: "fixture_tool".into(),
            arguments: serde_json::json!({}),
        },
        block: FrontendBlock {
            id: Some(id.into()),
            group: Some(turn.into()),
            update: FrontendBlockUpdate::Replace,
            state: FrontendBlockState::Complete,
            role: FrontendBlockRole::Tool,
            title: title.into(),
            text: text.into(),
            symbol: None,
            links: Vec::new(),
            files: Vec::new(),
            content: ToolContent::default(),
            format,
            tone: FrontendTone::Neutral,
            ui: Vec::new(),
        },
    }
}

const DIFF: &str = "--- a/src/ui/chat/composer.rs\n+++ b/src/ui/chat/composer.rs\n\
@@ -12,4 +12,6 @@\n-const ROW_INSET: f32 = 4.;\n+/// ComposerSurface's icon row inset.\n\
+const ROW_INSET: Pixels = px(6.);\n+\n fn overlapped() {}\n";

/// A finished turn (collapsing into Worked for) and a live one with tools and a diff.
fn transcript(running: bool) -> TranscriptStore {
    let mut script = Script::default();
    done_turn(&mut script);
    script
        .0
        .error("The provider rate limit was reached; retrying in 20 seconds.");
    live_turn(&mut script, running);
    script.store()
}

pub(in crate::ui) const CODING_DIFF: &str = "--- a/src/ui/navigation/recaps.rs\n+++ b/src/ui/navigation/recaps.rs\n\
@@ -48,9 +48,12 @@ impl Recaps {\n     pub(super) fn button_state(&self, catalog: &CatalogState) -> Glyph {\n\
-        if catalog.unread.is_empty() {\n-            return Glyph::Idle;\n-        }\n\
+        // The catalog settles after the turn ends: wait for it before showing news.\n\
+        if !catalog.tracking || catalog.unread.is_empty() {\n+            return Glyph::Idle;\n+        }\n\
+        let waiting = catalog.unread.len();\n         Glyph::Ready {\n\
-            count: catalog.unread.len(),\n+            count: waiting,\n         }\n     }\n";

/// A change to the coding demo, recorded by the motion strips.
#[derive(Clone, Copy)]
pub(in crate::ui) enum CodingChange {
    /// The turn ends and its steps fold into Worked for.
    Finish,
    /// A new message and the first step of the reply arrive.
    Reply,
    /// The answer streams two more paragraphs.
    Stream,
}

pub(in crate::ui) fn apply_coding_change(transcript: &mut TranscriptStore, change: CodingChange) {
    let mut script = coding_demo();
    match change {
        CodingChange::Finish => script.turn(DONE, false),
        CodingChange::Reply => {
            script.user(
                "fixture-code-next",
                "Also add a regression test for a catalog that never settles.",
                MessageDelivery::Turn,
                Vec::new(),
            );
            script.text(
                DONE,
                "test",
                ModelStepContentPhase::Reasoning,
                "**Writing the regression test**\nA catalog stuck before tracking must keep the button idle.",
            );
        }
        CodingChange::Stream => script.text(
            DONE,
            "answer",
            ModelStepContentPhase::FinalAnswer,
            "\n\nThe fix keeps the count stable while chats finish one after another, because \
the catalog only starts tracking once every chat has reported in.\n\nIf you see it again, \
open the recap with a chat still running: the button should stay idle until it finishes.",
        ),
    }
    transcript.replace(script.0.page());
}

/// A coding turn for the site's background, caught just before it ends so its steps stay
/// open: it searches, reads, edits, tests and answers.
pub(in crate::ui) fn coding_demo_transcript() -> TranscriptStore {
    coding_demo().store()
}

fn coding_demo() -> Script {
    let mut script = Script::default();
    script.turn(DONE, true);
    script.user(
        "fixture-code-ask",
        "The recap button flickers when a chat finishes. Find out why and fix it.",
        MessageDelivery::Turn,
        Vec::new(),
    );
    script.text(
        DONE,
        "trace",
        ModelStepContentPhase::Reasoning,
        "**Tracing the recap state**\nThe button reads `unread` before the catalog has settled.",
    );
    script.tool(tool(
        "code-search",
        DONE,
        "rg \"unread\" src/ui/navigation",
        "recaps.rs:51:        if catalog.unread.is_empty() {\n\
catalog.rs:44:    pub(super) unread: BTreeSet<String>,\n\
chats.rs:212:            self.catalog.unread.insert(session_id);\n",
        FrontendBlockFormat::PlainText,
    ));
    let mut read = tool(
        "code-read",
        DONE,
        "Read recaps.rs",
        "src/ui/navigation/recaps.rs\n",
        FrontendBlockFormat::PlainText,
    );
    read.block.links.push(FrontendLink {
        label: "recaps.rs".into(),
        href: "src/ui/navigation/recaps.rs".into(),
    });
    script.tool(read);
    script.tool(tool(
        "code-edit",
        DONE,
        "Edit recaps.rs",
        CODING_DIFF,
        FrontendBlockFormat::UnifiedDiff,
    ));
    script.tool(tool(
        "code-test",
        DONE,
        "cargo test recaps",
        "running 4 tests\ntest navigation::recaps::waits_for_the_catalog ... ok\n\
test navigation::recaps::counts_unread_chats ... ok\n\
test navigation::recaps::keeps_a_recap_until_news ... ok\n\
test navigation::recaps::follows_the_chat_language ... ok\n\n\
test result: ok. 4 passed; 0 failed\n",
        FrontendBlockFormat::PlainText,
    ));
    script.text(
        DONE,
        "answer",
        ModelStepContentPhase::FinalAnswer,
        "The button checked `unread` while the catalog was still catching up after the turn, \
so it showed news for a frame and then cleared. It now waits for `tracking` before it \
counts, and all **4** recap tests pass.",
    );
    script.turn(DONE, false);
    script
}

/// A finished turn whose Bot answered with the three `show_ui` demo documents, each
/// as the gateway sends it once the middleware's empty end append is merged.
pub(in crate::ui) fn ui_demo_transcript() -> TranscriptStore {
    #[derive(serde::Deserialize)]
    struct Document {
        title: String,
        blocks: Vec<mobius::protocol::UiBlock>,
    }
    let documents: Vec<Document> =
        serde_json::from_str(include_str!("ui_demo.json")).unwrap_or_default();
    let mut script = Script::default();
    script.turn(DONE, true);
    script.user(
        "fixture-ui-ask",
        "Plan my Lisbon weekend, recap my running quarter, and compare 14-inch laptops.",
        MessageDelivery::Turn,
        Vec::new(),
    );
    for (index, Document { title, blocks }) in documents.into_iter().enumerate() {
        script.0.block(
            "intelligent_ui",
            FrontendBlock {
                id: Some(format!("{DONE}/ui-{index}")),
                group: Some(DONE.into()),
                role: FrontendBlockRole::Artifact,
                format: FrontendBlockFormat::Ui,
                title,
                ui: blocks,
                ..Default::default()
            },
        );
    }
    script.text(
        DONE,
        "answer",
        ModelStepContentPhase::FinalAnswer,
        "Here are all three. Book Ramiro early on Saturday; it fills by eight.",
    );
    script.turn(DONE, false);
    script.store()
}

/// A turn that read, was steered, answered and left a report file behind.
fn done_turn(script: &mut Script) {
    script.turn(DONE, true);
    script.user(
        "fixture-ask",
        "Tidy the composer spacing and run the chat tests.",
        MessageDelivery::Turn,
        vec![file("spacing-notes.pdf", 24_000, "application/pdf")],
    );
    script.text(
        DONE,
        "plan",
        ModelStepContentPhase::Reasoning,
        "**Reading the composer**\nIt uses a 4 point inset.",
    );
    let mut read = tool(
        "read",
        DONE,
        "Read composer.rs",
        "src/ui/chat/composer.rs\n",
        FrontendBlockFormat::PlainText,
    );
    read.block.links.push(FrontendLink {
        label: "composer.rs".into(),
        href: "src/ui/chat/composer.rs".into(),
    });
    script.tool(read);
    script.user(
        "fixture-steer-done",
        "Keep the iPad gap as it is.",
        MessageDelivery::Steer,
        Vec::new(),
    );
    script.text(
        DONE,
        "answer",
        ModelStepContentPhase::FinalAnswer,
        "The composer now uses SwiftUI's **6 point** row inset, and the chat tests pass.",
    );
    let mut report = tool(
        "report",
        DONE,
        "Wrote report",
        "",
        FrontendBlockFormat::PlainText,
    );
    report.block.content = ToolContent(vec![ContentPart::File {
        file: file("test-report.txt", 3_100, "text/plain"),
    }]);
    script.tool(report);
    script.turn(DONE, false);
}

/// A turn that tests, is steered and edits; finished unless `running`.
fn live_turn(script: &mut Script, running: bool) {
    script.turn(LIVE, true);
    script.user(
        "fixture-next",
        "Now tighten the pills row.",
        MessageDelivery::Turn,
        Vec::new(),
    );
    script.text(
        LIVE,
        "look",
        ModelStepContentPhase::Reasoning,
        "## Checking the pills\nThe row is centred with 8 point gaps.",
    );
    script.tool(tool(
        "tests",
        LIVE,
        "cargo test chat",
        "running 12 tests\n# chat::pills ... ok\n*all* 12 passed\n",
        FrontendBlockFormat::PlainText,
    ));
    script.user(
        "fixture-steer-live",
        "Skip the snapshot tests for now.",
        MessageDelivery::Steer,
        Vec::new(),
    );
    script.tool(tool(
        "edit",
        LIVE,
        "Edit composer.rs",
        DIFF,
        FrontendBlockFormat::UnifiedDiff,
    ));
    if running {
        // An image still being made: the pulsing square field.
        let mut image = tool(
            "image",
            LIVE,
            "Pills row mockup",
            "",
            FrontendBlockFormat::Image,
        );
        image.block.state = FrontendBlockState::Pending;
        script.tool(image);
    }
    if !running {
        script.text(
            LIVE,
            "done",
            ModelStepContentPhase::FinalAnswer,
            "The pills row is tightened.",
        );
        script.turn(LIVE, false);
    }
}

/// An `apply_patch` envelope as a tool sends it: one change with context, one added file.
const ENVELOPE: &str = "*** Begin Patch\n*** Update File: src/ui/chat/pills.rs\n\
@@ fn composer_pills(&self) {\n-        .gap(space::S)\n+        .gap(space::XS)\n\
         .overflow_x_scroll()\n*** Add File: docs/pills.md\n+# Pills\n\
+The row floats over the transcript.\n*** End Patch";

/// A unified diff of `root`'s files, as `apply_patch` reports it: absolute paths, a short
/// hunk and one long enough to fold.
fn patch_diff(root: &str) -> String {
    let path = format!("{root}/src/ui/transcript/mod.rs");
    let mut diff = format!(
        "--- {path}\n+++ {path}\n@@ -40,6 +40,7 @@\n     fn file_row(&self) {{\n\
-        div().flex().gap(space::S)\n+        div()\n+            .flex_wrap()\n\
             .children(files)\n     }}\n@@ -120,3 +121,52 @@\n impl Workspace {{\n"
    );
    diff.extend((0..48).map(|line| format!("+    // tile {line}: same height, same gap\n")));
    diff.push_str(" }\n");
    diff
}

/// A running turn that patched two files.
fn patch_transcript(root: &str) -> TranscriptStore {
    let mut script = Script::default();
    script.turn(LIVE, true);
    script.user(
        "fixture-patch",
        "Align the file tiles and the pills.",
        MessageDelivery::Turn,
        Vec::new(),
    );
    script.tool(tool(
        "patch-diff",
        LIVE,
        "Edit file",
        &patch_diff(root),
        FrontendBlockFormat::UnifiedDiff,
    ));
    script.store()
}

/// One finished turn: a question, a thought and an answer.
fn short_turn(script: &mut Script, turn: &str, ask: &str, answer: &str) {
    script.turn(turn, true);
    script.user(
        &format!("{turn}-ask"),
        ask,
        MessageDelivery::Turn,
        Vec::new(),
    );
    script.text(
        turn,
        &format!("{turn}-think"),
        ModelStepContentPhase::Reasoning,
        "**Weighing the options**",
    );
    script.text(
        turn,
        &format!("{turn}-answer"),
        ModelStepContentPhase::FinalAnswer,
        answer,
    );
    script.turn(turn, false);
}

/// A fork of `parent` whose inherited turn sits above the divider and its own below.
fn fork_transcript(parent: &SessionId) -> TranscriptStore {
    let mut inherited = Script(fixture::Script::inherited(1));
    short_turn(
        &mut inherited,
        "fixture-parent",
        "Sketch two layouts for the pills row.",
        "**A** floats the pills over the transcript; **B** docks them on the composer.",
    );
    let mut own = Script::default();
    short_turn(
        &mut own,
        "fixture-fork",
        "Go with A, and keep the Session pill last.",
        "Done: the pills float over the transcript and Session stays last.",
    );
    own.0.fork_point(parent.share());
    let mut transcript = own.store();
    transcript.prepend(inherited.0.page());
    transcript
}

/// Reasoning long enough to span several lines once expanded.
const REASONING: &str = "**Tracing the scroll offset**\n\nThe transcript scrolls a plain \
flex column, so its offset counts from the top and a block that grows below the fold \
should leave everything above it where it was.\n\nThe list re-renders when the \
expanded set changes, and the cached region keys on that set, so one click should cost \
exactly one render.\n\n- Check the header's painted bounds\n- Check the scroll offset\n\
- Check the fade does not change the height";

/// Twelve finished turns, each folding its reasoning and a tool into Worked for. Even
/// turns think at length: past 4 KB the markdown view parses off the main thread.
fn long_transcript() -> TranscriptStore {
    let mut script = Script::default();
    let long = format!("{REASONING}{}", format!("\n\n{REASONING}").repeat(8));
    for index in 0..12 {
        let turn = format!("fixture-turn-{index}");
        script.user(
            &format!("fixture-ask-{index}"),
            &format!("Step {index}: check the transcript scroll once more."),
            MessageDelivery::Turn,
            Vec::new(),
        );
        script.turn(&turn, true);
        script.text(
            &turn,
            &format!("think-{index}"),
            ModelStepContentPhase::Reasoning,
            if index % 2 == 0 { &long } else { REASONING },
        );
        script.tool(tool(
            &format!("run-{index}"),
            &turn,
            "cargo test transcript",
            "running 4 tests\nall 4 passed\n",
            FrontendBlockFormat::PlainText,
        ));
        script.text(
            &turn,
            &format!("answer-{index}"),
            ModelStepContentPhase::FinalAnswer,
            "The header stays where it was and only the rows below it move.",
        );
        script.turn(&turn, false);
    }
    script.store()
}

/// The footer widgets a busy chat shows: the task list and the subagents count.
fn footer_widgets() -> [SessionWidget; 2] {
    let widget = |capability: &str, text: &str, symbol: FrontendSymbol, content| SessionWidget {
        capability: capability.into(),
        item: Arc::new(FrontendWidget {
            id: "status".into(),
            slot: FrontendSlot::ComposerFooter,
            text: text.into(),
            tone: FrontendTone::Success,
            symbol: Some(symbol),
            icon_only: false,
            progress: None,
            content: Some(content),
            action: None,
            count: None,
            priority: FrontendPriority::Secondary,
        }),
    };
    let mut tasks = widget(
        "tasks",
        "2/5",
        FrontendSymbol::Task,
        FrontendWidgetContent::ActionList {
            title: "Tasks".into(),
            items: Vec::new(),
            actions: Vec::new(),
        },
    );
    let item = Arc::make_mut(&mut tasks.item);
    item.tone = FrontendTone::Neutral;
    item.progress = Some(mobius::protocol::FrontendProgress {
        completed: 2,
        total: 5,
    });
    let mut subagents = widget(
        "subagents",
        "1 subagent",
        FrontendSymbol::Agent,
        FrontendWidgetContent::ActionList {
            title: "Subagents".into(),
            items: vec![subagent_item(
                "fixture-subagent",
                "Review the pills",
                "running",
            )],
            actions: Vec::new(),
        },
    );
    Arc::make_mut(&mut subagents.item).count = Some(1);
    [tasks, subagents]
}

/// One agent in the Subagents list, as the gateway maps its `status`.
fn subagent_item(path: &str, name: &str, status: &str) -> FrontendActionListItem {
    let (state, tone, symbol) = match status {
        "pending_init" => (
            FrontendListItemState::Pending,
            FrontendTone::Neutral,
            FrontendSymbol::Progress,
        ),
        "running" => (
            FrontendListItemState::InProgress,
            FrontendTone::Neutral,
            FrontendSymbol::Progress,
        ),
        "completed" => (
            FrontendListItemState::Completed,
            FrontendTone::Neutral,
            FrontendSymbol::Agent,
        ),
        "interrupted" => (
            FrontendListItemState::Interrupted,
            FrontendTone::Warning,
            FrontendSymbol::Agent,
        ),
        _ => (
            FrontendListItemState::Failed,
            FrontendTone::Neutral,
            FrontendSymbol::Agent,
        ),
    };
    FrontendActionListItem {
        outcome: None,
        id: path.into(),
        text: name.into(),
        state,
        actions: vec![FrontendAction {
            id: "subagents".into(),
            label: "Open subagent".into(),
            symbol,
            tone,
            op: Op::CapabilityCommand {
                capability: "subagents".into(),
                command: "subagents".into(),
                arguments: path.into(),
                input: None,
                target: None,
            },
            input_from_label: false,
            editor: None,
        }],
    }
}

/// 32.4K of a 270K window: the Session pill reads "12%".
fn token_usage() -> TokenUsageInfo {
    use mobius::protocol::ContextPartKind::*;
    use mobius::protocol::{ContextBreakdown, ContextPart};
    let usage = TokenUsage {
        input_tokens: 30_000,
        output_tokens: 2_400,
        total_tokens: 32_400,
        ..TokenUsage::default()
    };
    let part = |kind, tokens, items| ContextPart {
        kind,
        tokens,
        items,
    };
    TokenUsageInfo {
        total_token_usage: usage,
        last_token_usage: usage,
        model_context_window: Some(270_000),
        context: Some(ContextBreakdown {
            parts: vec![
                part(Messages, 17_900, 42),
                part(ToolResults, 6_100, 18),
                part(Tools, 2_500, 21),
                part(Capabilities, 1_500, 11),
                part(Instructions, 900, 2),
                part(Skills, 600, 1),
                part(Memory, 300, 2),
                part(ToolCalls, 200, 18),
                part(DeferredTools, 4_100, 37),
            ],
            compaction_buffer: 33_750,
        }),
    }
}

/// A message the gateway holds until the turn reaches it, as the messages capability
/// shows it: queued for the next turn, or steering the running one.
fn held_widget(id: &str, text: &str, symbol: FrontendSymbol) -> SessionWidget {
    SessionWidget {
        capability: "messages".into(),
        item: Arc::new(FrontendWidget {
            id: id.into(),
            slot: FrontendSlot::TranscriptTail,
            text: text.into(),
            tone: FrontendTone::Neutral,
            symbol: Some(symbol),
            icon_only: false,
            progress: None,
            count: None,
            priority: FrontendPriority::Primary,
            content: None,
            action: Some(Op::CapabilityCommand {
                capability: "messages".into(),
                command: "edit".into(),
                arguments: id.into(),
                input: Some(text.into()),
                target: None,
            }),
        }),
    }
}

/// The chat's own widgets with the scene's held message and footer in place of the chat's.
fn scene_widgets(widgets: &[SessionWidget], scene: Scene) -> Vec<SessionWidget> {
    let mut widgets: Vec<_> = widgets
        .iter()
        .filter(|widget| {
            !matches!(
                widget.item.slot,
                FrontendSlot::TranscriptTail | FrontendSlot::ComposerFooter
            )
        })
        .cloned() // rules-allow: the scene builds its own widget list from the chat's
        .collect();
    if scene == Scene::Attention {
        match serde_json::from_value(serde_json::json!({
            "id":"attention", "slot":"attention", "text":"1", "tone":"warning", "symbol":"question",
            "icon_only":false, "progress":null, "action":null,
            "content":{"type":"action_list", "title":"Questions", "actions":[], "items":[
                {"id":"open", "text":"Which environment should I use?", "state":"pending", "actions":
                    (["Preview", "Staging", "Production"].into_iter().enumerate().map(|(i, label)| serde_json::json!({
                        "id":i.to_string(), "label":label, "symbol":"question", "tone":"neutral", "editor":null, "input_from_label":true,
                        "op":{"type":"capability_command", "capability":"fixture", "command":"answer", "arguments":"open", "input":null, "target":null}
                    })).chain(std::iter::once(serde_json::json!({
                        "id":"other", "label":"Write an answer", "symbol":"question", "tone":"neutral",
                        "editor":{"title":"Answer", "label":"Write an answer", "description":"", "submit_label":"Send"},
                        "op":{"type":"capability_command", "capability":"fixture", "command":"answer", "arguments":"open", "input":null, "target":null}
                    }))).chain(std::iter::once(serde_json::json!({
                        "id":"skip", "label":"Skip", "symbol":"user", "tone":"neutral", "editor":null,
                        "op":{"type":"capability_command", "capability":"fixture", "command":"skip", "arguments":"open", "input":null, "target":null}
                    }))).collect::<Vec<_>>())},
                {"id":"answered", "text":"Which style?", "outcome":"Answered: Minimal", "state":"completed", "actions":[]},
                {"id":"skipped", "text":"Add an animation?", "outcome":"Skipped", "state":"completed", "actions":[]}
            ]}
        })) {
            Ok(item) => widgets.push(SessionWidget {
                capability: "fixture".into(),
                item,
            }),
            Err(error) => tracing::warn!(%error, "the attention fixture widget didn't parse"),
        }
    }
    widgets.push(if scene == Scene::Steer {
        held_widget(
            "fixture-held",
            "Use the accent colour for the diff pill.",
            FrontendSymbol::Steer,
        )
    } else {
        held_widget(
            "fixture-queued",
            "Also check the light appearance.",
            FrontendSymbol::Queue,
        )
    });
    widgets.extend(footer_widgets());
    widgets
}

impl Workspace {
    /// Real message metadata over an open chat, with a catalog-owned source Bot.
    /// No operation is submitted and the typed events use the normal transcript reducer.
    pub(in crate::ui) fn fixture_session_messages(
        &mut self,
        steer: bool,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        self.session_messages(steer, expanded, None, cx);
    }

    /// The peer turn, its message sent from `project` or else the source Bot's own chat.
    fn session_messages(
        &mut self,
        steer: bool,
        expanded: bool,
        project: Option<SessionId>,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.client.state.gateway.as_ref().and_then(|gateway| {
            gateway.bots.iter().find(|bot| {
                self.composer_bot()
                    .is_none_or(|current| current.id != bot.id)
            })
        }) else {
            return;
        };
        let source_id = project.unwrap_or_else(|| source.conversation_session_id.share());
        let source_bot = Arc::clone(&source.id);
        let first = self.client.state.sessions.first().cloned(); // rules-allow: the fixture derives a new session record from an existing one
        if let Some(mut record) = first {
            record.session_id = source_id.share();
            record.session_context.owner_id = source_bot;
            record.session_context.workspace_label = None;
            record.title = Some("Source persistent conversation".into());
            self.client
                .state
                .sessions
                .retain(|session| session.session_id != source_id);
            self.client.state.sessions.push(record);
        }
        let mut script = Script::default();
        let turn = "fixture-peer";
        script.turn(turn, true);
        if steer {
            script.user(
                "fixture-peer-ask",
                "Please prepare today's job search update.",
                MessageDelivery::Turn,
                Vec::new(),
            );
            script.text(
                turn,
                "peer-before",
                ModelStepContentPhase::Reasoning,
                "I am checking the current job search.",
            );
        }
        script.0.message("fixture-peer-message", MessageEvent {
            author: MessageAuthor::Source {
                message_id: "fixture-peer-message".into(),
                source: mobius::protocol::MessageSource::Session { session_id: source_id.share() },
                handle: format!("chat #{source_id}").into(),
                symbol: Some(FrontendSymbol::Chat),
                cause_id: None,
                ancestry: Vec::new(),
            },
            delivery: if steer { MessageDelivery::Steer } else { MessageDelivery::Turn },
            text: "I found three new roles that match your search. Two are remote, and one is based in Singapore. The application deadline for the first role is Friday.".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        });
        script.text(
            turn,
            "peer-work",
            ModelStepContentPhase::Reasoning,
            "I am reviewing the job search results and deadlines.",
        );
        script.tool(tool(
            "peer-review",
            turn,
            "Review job search results",
            "Three roles and their application deadlines are ready.",
            FrontendBlockFormat::PlainText,
        ));
        script.text(turn, "peer-answer", ModelStepContentPhase::FinalAnswer, "Jobhunter found **three matching roles**. I will review the remote options first and help you prepare the application due Friday.");
        script.turn(turn, false);
        self.client.state.transcript = script.store();
        if expanded {
            let scope = format!("chat:{}", self.selected_chat().unwrap_or_default());
            self.expanded.insert(format!("{scope}:worked:{turn}"));
        }
        if let Some(session) = &mut self.client.state.session {
            session.active_turn_ids.clear();
            session.widgets.retain(|widget| {
                !matches!(
                    widget.item.slot,
                    FrontendSlot::TranscriptTail | FrontendSlot::ComposerFooter
                )
            });
        }
        self.gateway_revision += 1;
        cx.notify();
        cx.notify();
    }

    /// Ordinary source sessions retain their project title in the same attribution row.
    #[expect(
        clippy::expect_used,
        reason = "fixed snapshot source identity must be valid"
    )]
    pub(in crate::ui) fn fixture_project_session_message(&mut self, cx: &mut Context<Self>) {
        let project = SessionId::new("fixture-project-source").expect("fixture session");
        self.session_messages(false, false, Some(project.share()), cx);
        if let Some(record) = self
            .client
            .state
            .sessions
            .iter_mut()
            .find(|session| session.session_id == project)
        {
            record.session_context.workspace_label = Some("/tmp/job-search".into());
            record.title = Some("Remote engineering roles".into());
        }
        self.gateway_revision += 1;
        cx.notify();
    }

    /// Types into the composer without sending.
    pub(in crate::ui) fn fixture_type(&self, text: &str, window: &mut Window, cx: &mut App) {
        self.composer
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    /// Opens the composer's model popover, optionally on its model list.
    pub(in crate::ui) fn fixture_model_picker(&mut self, list: bool, cx: &mut Context<Self>) {
        let Some(bot) = self.composer_bot().map(|bot| Arc::clone(&bot.id)) else {
            return;
        };
        self.model_picker = Some(ModelPicker::new(
            super::model::COMPOSER,
            ModelTarget::Bot(bot),
        ));
        self.show_models(list, cx);
    }

    /// A rejected provider key, including its original text behind Details.
    pub(in crate::ui) fn fixture_provider_issue(&mut self, cx: &mut Context<Self>) {
        let mut script = Script::default();
        script.user("fixture-ask", "Hey", MessageDelivery::Turn, Vec::new());
        script.0.block(
            "tools",
            FrontendBlock {
                id: Some("provider-error".into()),
                role: FrontendBlockRole::Notice,
                text: "provider error: Responses HTTP 401 Unauthorized: API key rejected.".into(),
                tone: FrontendTone::Error,
                ..FrontendBlock::default()
            },
        );
        self.client.state.transcript = script.store();
        self.gateway_revision += 1;
        cx.notify();
    }

    /// Runs `apply` once the opened chat is ready.
    pub(in crate::ui) fn when_chat_ready(
        window: &Window,
        cx: &Context<Self>,
        mut apply: impl FnMut(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..100 {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let applied = this.update_in(cx, |this, window, cx| {
                    let ready = this.active_session_ready();
                    if ready {
                        apply(this, window, cx);
                    }
                    ready
                });
                if applied.unwrap_or(true) {
                    return;
                }
            }
        })
        .detach();
    }

    /// Replaces the chat's subagents list with one agent of each status, in the gateway's
    /// order: active, completed, interrupted, failed.
    pub(in crate::ui) fn snapshot_mixed_subagents(&mut self, cx: &mut Context<Self>) {
        let items = vec![
            subagent_item("root/pill-review", "pill-review", "running"),
            subagent_item("root/icon-sweep", "icon-sweep", "pending_init"),
            subagent_item("root/release-notes", "release-notes", "completed"),
            subagent_item(
                "root/changelog-diff-with-a-rather-long-name",
                "changelog-diff-with-a-rather-long-name",
                "completed",
            ),
            subagent_item("root/flaky-test-hunt", "flaky-test-hunt", "interrupted"),
            subagent_item("root/schema-audit", "schema-audit", "errored"),
        ];
        let Some(session) = &mut self.client.state.session else {
            return;
        };
        for widget in &mut session.widgets {
            if &*widget.capability != "subagents" {
                continue;
            }
            let item = Arc::make_mut(&mut widget.item);
            if let Some(FrontendWidgetContent::ActionList { items: shown, .. }) = &mut item.content
            {
                item.text = format!("{} subagents", items.len());
                item.count = Some(items.len());
                item.tone = FrontendTone::Error;
                *shown = items;
                break;
            }
        }
        cx.notify();
    }

    /// Runs a chat widget's command as its pill does: the voice call's transcript, or the
    /// first item's action of a list such as the subagents list. These gateway commands only read.
    pub(in crate::ui) fn fixture_widget_command(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some((widget, _)) = self.footer_widget_items().remove(key) else {
            return;
        };
        let (title, op) = match (&widget.content, &widget.action) {
            (Some(FrontendWidgetContent::ActionList { items, .. }), _) => {
                match items
                    .first()
                    .and_then(|item| Some((item, item.actions.first()?)))
                {
                    Some((item, action)) => (item.text.clone(), action.op.clone()), // rules-allow: the preview tab owns its title and command
                    None => return,
                }
            }
            (_, Some(op)) => (super::detail::widget_title(widget), op.clone()), // rules-allow: the preview tab owns its command
            _ => return,
        };
        self.open_preview_command(title.into(), op, cx);
    }

    /// Lays twelve finished turns over the open chat and reveals the `nth` turn's
    /// reasoning line (see `fixture_reveal_reasoning`).
    pub(in crate::ui) fn apply_long_fixture(
        &mut self,
        nth: Option<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.client.state.transcript = long_transcript();
        self.gateway_revision += 1;
        self.fixture_reveal_reasoning(nth, window, cx)
    }

    /// Opens the rows around the `nth` reasoning entry (the last one when `None`) the way
    /// their headers' clicks would, and watches its line. Picking one by index also
    /// scrolls it to the middle of the viewport; the last one stays where tail following
    /// leaves it. Returns the reasoning line's disclosure id.
    pub(in crate::ui) fn fixture_reveal_reasoning(
        &mut self,
        nth: Option<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let scope = format!(
            "chat:{}",
            self.client
                .state
                .selected_session_id
                .as_deref()
                .unwrap_or_default()
        );
        let transcript = &self.client.state.transcript;
        let mut reasoning = transcript.entries().filter(|entry| {
            matches!(
                entry.content,
                mobius_gateway::wire::TranscriptItemContent::Assistant {
                    phase: ModelStepContentPhase::Reasoning,
                    ..
                }
            )
        });
        let entry = match nth {
            Some(nth) => reasoning.nth(nth),
            None => reasoning.next_back(),
        }?;
        let target = format!("{scope}:{}:reasoning", entry.id);
        let holds = |row: &mobius_gateway::wire::TranscriptRow| {
            row.items
                .iter()
                .any(|child| child.item() == Some(&entry.id))
        };
        for row in transcript
            .rows()
            .filter(|row| transcript.row_items(row).any(|item| item.id == entry.id))
        {
            self.expanded.insert(format!("{scope}:{}", row.id));
            for child in transcript.row_children(row) {
                if let crate::gateway::RowChild::Row(run) = child
                    && holds(run)
                {
                    self.expanded.insert(format!("{scope}:{}", run.id));
                }
            }
        }
        crate::ui::snapshot::watch(target.clone()); // rules-allow: the probe and the caller each keep the disclosure id
        cx.notify();
        cx.notify();
        if nth.is_some() {
            cx.spawn_in(window, async move |this, cx| {
                // Two passes: the first lands close, the second corrects for late layout.
                for _ in 0..2 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                    if let Err(error) = cx.update(|window, cx| window.draw(cx).clear(cx)) {
                        tracing::debug!(%error, "the window closed before the fixture redrew");
                    }
                    if let Err(error) = this.update(cx, |this, cx| {
                        if let Some(top) = crate::ui::snapshot::probed().map(|b| b.top()) {
                            this.fixture_center_transcript(&scope, top);
                        }
                        cx.notify();
                    }) {
                        tracing::debug!(%error, "the workspace closed before the fixture centred");
                    }
                }
            })
            .detach();
        }
        Some(target)
    }

    /// The transcript `scene` shows, its patches inside the open chat's folder.
    fn scene_transcript(&self, scene: Scene) -> TranscriptStore {
        if scene == Scene::Fork {
            return fork_transcript(&self.fixture_fork_parent());
        }
        if scene != Scene::Patch {
            return transcript(scene != Scene::Transcript);
        }
        let root = self
            .client
            .state
            .session
            .as_ref()
            .and_then(|session| session.workspace.as_ref())
            .map_or_else(String::new, |workspace| {
                workspace.path.to_string_lossy().into_owned()
            });
        patch_transcript(&root)
    }

    /// Another chat in the catalog to name as the fixture fork's parent.
    #[expect(
        clippy::expect_used,
        reason = "fixed fallback fixture identity must be valid"
    )]
    fn fixture_fork_parent(&self) -> SessionId {
        let selected = self.client.state.selected_session_id.as_ref();
        self.client
            .state
            .sessions
            .iter()
            .find(|session| Some(&session.session_id) != selected)
            .map_or_else(
                || SessionId::new("fixture-parent").expect("fixture session"),
                |session| session.session_id.share(),
            )
    }

    /// Lays `scene` over the open chat.
    pub(in crate::ui) fn apply_fixture(&mut self, scene: Scene, cx: &mut Context<Self>) {
        let running = scene != Scene::Transcript;
        self.model_picker = None;
        self.client.state.transcript = self.scene_transcript(scene);
        let scope = format!(
            "chat:{}",
            self.client
                .state
                .selected_session_id
                .as_deref()
                .unwrap_or_default()
        );
        for id in [
            "worked:fixture-live",
            "block:tools:tests:block",
            "block:tools:edit:block",
            "assistant:done:final_answer:turn-diff",
            "block:tools:patch-diff:block",
        ] {
            self.expanded.insert(format!("{scope}:{id}"));
        }
        if let Some(session) = &mut self.client.state.session {
            session.active_turn_ids = if running {
                vec![LIVE.into()]
            } else {
                Vec::new()
            };
            session.widgets = scene_widgets(&session.widgets, scene);
            session.pending_approvals.clear();
            if matches!(scene, Scene::Approval | Scene::Attention) {
                session.pending_approvals = vec![approval_fixture()];
            }
        }
        self.client.state.picker = (scene == Scene::Picker).then(|| {
            let option = |label: &str, detail: &str| FrontendPickerOption {
                label: label.into(),
                description: detail.into(),
                detail: String::new(),
                symbol: None,
                op: Op::Interrupt {
                    turn_id: LIVE.into(),
                },
            };
            (
                "Which branch should I use?".to_owned(),
                vec![
                    option("wp-chat", "The chat work package"),
                    option("main", "The integration branch"),
                ],
            )
        });
        self.client.state.previews.insert(
            "fixture-subagent".into(),
            Rc::new(crate::gateway::Preview {
                source: RenderedPreview {
                    kind: mobius::protocol::FrontendPreviewKind::Agent,
                    state: Some(FrontendListItemState::InProgress),
                    symbol: None,
                    duration_ms: Some(42_000),
                    started_at_ms: None,
                    id: "fixture-subagent".into(),
                    title: "Review the pills".into(),
                    subtitle: "No context".into(),
                    page_id: "fixture".into(),
                    update: mobius::protocol::FrontendPreviewUpdate::Replace,
                    transcript: Default::default(),
                    next: None,
                },
                transcript: TranscriptStore::default(),
            }),
        );
        self.client.state.history =
            (scene == Scene::Fork).then(|| mobius_gateway::wire::HistoryCursor {
                session_id: self.fixture_fork_parent(),
                before_sequence: 1,
            });
        self.client.state.token_usage = Some(Arc::new(token_usage()));
        self.fixture_changes((42, 9));
        self.dictating = scene == Scene::Dictation;
        self.voice_active = scene == Scene::Voice;
        self.voice_connected = self.voice_active;
        self.chat_ui.fixture_voice =
            self.voice_active
                .then_some(crate::host::platform::VoiceLevels {
                    level: 0.36,
                    flare: 0.4,
                    playback: false,
                });
        self.chat_ui.waiting = (scene == Scene::Running).then_some(waiting::Waiting);
        if scene == Scene::Steer {
            self.fixture_pending("fixture-sending", "And keep the Session pill last.");
        }
        self.gateway_revision += 1;
        cx.notify();
        cx.notify();
    }
}

/// A shell command and a patch awaiting approval.
fn approval_fixture() -> ExecApprovalRequestEvent {
    ExecApprovalRequestEvent {
        id: "fixture-approval".into(),
        turn_id: LIVE.into(),
        calls: vec![
            ApprovalCall {
                call_id: "call".into(),
                name: "shell".into(),
                arguments: serde_json::json!({
                    "command": "git push origin wp-chat",
                    "timeout_ms": 60000,
                }),
                preview: Some(FrontendBlock {
                    role: FrontendBlockRole::Tool,
                    title: "git push origin wp-chat".into(),
                    format: FrontendBlockFormat::PlainText,
                    ..Default::default()
                }),
            },
            ApprovalCall {
                call_id: "patch".into(),
                name: "apply_patch".into(),
                arguments: serde_json::json!({ "input": ENVELOPE }),
                preview: Some(FrontendBlock {
                    role: FrontendBlockRole::Tool,
                    title: "Edit composer.rs".into(),
                    text: DIFF.into(),
                    format: FrontendBlockFormat::UnifiedDiff,
                    ..Default::default()
                }),
            },
        ],
        reason: "The Bot wants to push the branch to the remote.".into(),
    }
}
