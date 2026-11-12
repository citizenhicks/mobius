mod activity;
mod attention;
mod composer;
pub(in crate::ui) use composer::surface_shadow;
mod controls;
mod conversation;
mod detail;
#[cfg(feature = "snapshot")]
mod fixtures;
mod fork;
mod mention;
mod model;
mod page;
mod picker;
mod pills;
mod rail;
mod reply;
mod session_info;
mod voice_wave;
mod waiting;
pub(super) use controls::{bot_menu, file_card, file_detail, file_glyph, tile_size};
pub(super) use detail::ChatDetail;
#[cfg(feature = "snapshot")]
pub(super) use fixtures::{
    CODING_DIFF, CodingChange, Scene, apply_coding_change, coding_demo_transcript,
    ui_demo_transcript,
};
pub(super) use mention::Mention;
pub(super) use model::{ModelPicker, ModelTarget, configured_model, update_bot_message};
pub(super) use reply::reply_quote;
pub(in crate::ui) use session_info::now_ms;
pub(in crate::ui) use waiting::reasoning_title;

use super::*;
use gpui_component::Disableable;
use mobius::SessionId;
use mobius::protocol::{EventMsg, FrontendSlot, Op};
use mobius_gateway::wire::{BotShape, ServerMessage};

/// Package-owned UI state; new fields belong here rather than on `Workspace`.
#[derive(Default)]
pub(in crate::ui) struct ChatUi {
    pub(in crate::ui) markdown:
        std::cell::RefCell<std::collections::BTreeMap<SharedString, transcript::StreamingMarkdown>>,
    pub(in crate::ui) removable_files: std::collections::BTreeSet<String>,
    /// Disclosures the person opened, which grow open; ones already open when a chat loads
    /// simply show.
    pub(in crate::ui) opened: std::collections::BTreeSet<String>,
    /// Which session card sections are open.
    pub(in crate::ui) session_sections: session_info::CardSections,
    /// The gateway's reason for rejecting the selected chat.
    pub(in crate::ui) open_failure: Option<Arc<str>>,
    /// SwiftUI's brief reaction after a turn ends, for the chat it ended in.
    reaction: Option<(SessionId, BotMood)>,
    reaction_timer: Option<Task<()>>,
    /// The chat `open_failure` belongs to.
    failed_session: Option<SessionId>,
    attention: attention::AttentionIndex,
    decision_drafts:
        std::collections::BTreeMap<std::rc::Rc<attention::DecisionKey>, attention::DecisionDraft>,
    pub(in crate::ui) action_requests: std::collections::BTreeMap<String, Arc<str>>,
    /// The message whose Copy shows a check, and the timer that clears it.
    copied: Option<(String, Task<()>)>,
    /// The waiting line while a running turn is quiet, and the hold before it appears.
    waiting: Option<waiting::Waiting>,
    waiting_hold: Option<Task<()>>,
    /// The running turn's Worked row: its live "Working" header stands in for the waiting
    /// line, so the transcript keeps one row instead of trading a line for it.
    pub(in crate::ui) running_row: Option<Arc<str>>,
    /// Semantic projection; painting still registers text selection every frame.
    items: Vec<ChatItem>,
    rows: Vec<Arc<str>>,
    projection: Option<(Option<SessionId>, u64)>,
    list: Option<gpui::ListState>,
    tail_ids: Vec<Arc<str>>,
    history_visible: bool,
    removable_revision: Option<(Option<SessionId>, u64)>,
    rail_marks: Vec<rail::Mark>,
    /// The transcript revision `rail_marks` read their words from.
    rail_revision: u64,
    /// The pinned answers the rail marks were built from.
    rail_pins: Vec<mobius::protocol::MessageTarget>,
    /// The history page on its way, so each earlier turn is asked for once.
    history_request: Option<Arc<str>>,
    /// The message tick under the pointer on the rail.
    rail_hover: Option<SharedString>,
    /// The scoped entry to reveal after its containing row has expanded.
    rail_reveal: Option<std::rc::Rc<(String, String)>>,
    /// A composer notice, such as a send the gateway rejected.
    /// Redraws the Session pill once a second while a run is active.
    ticker: Option<Task<()>>,
    pub(in crate::ui) visible: bool,
    /// The message the next send replies to, by gateway-scoped draft key, with its author.
    reply: Option<(String, mobius::protocol::MessageReply, String)>,
    /// The system voice reading a message aloud.
    speech: Option<crate::host::platform::Speech>,
    /// The chat whose session card is pinned over the conversation.
    session_card: Option<SessionId>,
    /// Retains the card through its closing slide and supports reversing mid-motion.
    session_card_motion: motion::Reveals,
    /// Voice levels a snapshot fixture plays in place of the native call.
    fixture_voice: Option<crate::host::platform::VoiceLevels>,
    /// The workspace's unstaged Git changes the pill shows.
    changes: activity::WorkspaceChanges,
    /// The turn changes last opened in a chat's Changes tab, as that tab's "This turn".
    turn_changes: Option<(SessionId, Arc<str>)>,
    /// The height of the composer floating over the transcript, which the transcript
    /// leaves free below its last row.
    composer_inset: Pixels,
    /// The `@query` being typed in the composer.
    mention: Option<mention::MentionMenu>,
    /// Paths `@` put in the composer, drawn there as links while the text holds them.
    linked: std::collections::BTreeSet<String>,
    /// The open chat's folder and stored files, for `@` and the New tab's chat files.
    chat_sources: mention::ChatSources,
    /// Chat-folder files being read to save to Downloads.
    saving: Vec<Arc<str>>,
}

impl Workspace {
    pub(in crate::ui) fn chat_on_message(
        &mut self,
        message: &ServerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match message {
            ServerMessage::Accepted { request_id, .. } => {
                self.chat_ui
                    .action_requests
                    .retain(|key, request| request != request_id || key.starts_with("approval\0"));
            }
            ServerMessage::Rejected { request_id, .. } => {
                self.chat_ui
                    .action_requests
                    .retain(|_, request| request != request_id);
            }
            ServerMessage::AgentEvent { record, .. }
                if matches!(record.event.msg, EventMsg::SubmissionRejected(_)) =>
            {
                if let Some(id) = &record.event.submission_id {
                    self.chat_ui
                        .action_requests
                        .retain(|_, request| **request != **id);
                }
            }
            ServerMessage::Ready { .. } => self.chat_ui.action_requests.clear(),
            _ => {}
        }
        self.ensure_session_ticker(cx);
        self.update_waiting(cx);
        if matches!(message, ServerMessage::Ready { .. })
            && self.draft_workspace.as_os_str() == "."
            && let Some(latest) = self.recent_folders().into_iter().next()
        {
            self.draft_workspace = latest.into();
        }
        if let ServerMessage::Rejected {
            request_id,
            message,
            ..
        } = message
        {
            self.chat_rejected(request_id, message, window, cx);
        }
        self.workspace_changes_on_message(message);
        self.fork_on_message(message, window, cx);
        // Older pages are requested by scrolling or the history row.
        if let ServerMessage::SessionReplayComplete { session_id, .. }
        | ServerMessage::SessionHistory { session_id, .. } = message
            && self.client.state.selected_session_id.as_deref() == Some(session_id.as_str())
        {
            self.chat_ui.history_request = None;
        }
        // The chat's folder and files for `@` and the New tab, once the chat is open.
        if let ServerMessage::SessionReplayComplete { session_id, .. } = message
            && self.client.state.selected_session_id.as_deref() == Some(session_id.as_str())
        {
            self.load_chat_files();
        }
        let ServerMessage::AgentEvent {
            session_id, record, ..
        } = message
        else {
            return;
        };
        if self.client.state.replaying
            || self.client.state.selected_session_id.as_deref() != Some(session_id.as_ref())
        {
            return;
        }
        if let EventMsg::ExecApprovalDecided(decided) = &record.event.msg {
            self.chat_ui
                .action_requests
                .remove(&format!("approval\0{}", decided.id));
        }
        if mention::files_published(self.client.state.touched_entries()) {
            self.load_chat_files();
        }
        let mood = match &record.event.msg {
            EventMsg::TurnComplete(turn) if !turn.idle => {
                // A finished turn may have sent files back.
                self.load_chat_files();
                self.title_new_chat(session_id);
                BotMood::Happy
            }
            EventMsg::TurnAborted(turn) if !turn.idle => {
                self.load_chat_files();
                self.title_new_chat(session_id);
                BotMood::Dizzy
            }
            _ => return,
        };
        self.chat_ui.reaction = Some((session_id.share(), mood));
        self.chat_ui.reaction_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(motion::REACTION).await;
            if let Err(error) = this.update(cx, |this, cx| {
                this.chat_ui.reaction = None;
                cx.notify();
            }) {
                tracing::debug!(%error, "the workspace closed before the chat reaction ended");
            }
        }));
    }
}

impl Workspace {
    /// Opens a turn's changes (its card's "View all changes") in the chat's Changes tab.
    pub(in crate::ui) fn open_turn_changes(&mut self, diff: &str, cx: &mut Context<Self>) {
        let Some(session) = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)
        else {
            return;
        };
        self.chat_ui.turn_changes = Some((session.share(), diff.into()));
        self.show_turn_changes(session, cx);
    }

    /// The room the floating composer takes at the foot of the chat's transcript.
    pub(in crate::ui) fn composer_inset(&self) -> Pixels {
        self.chat_ui.composer_inset
    }

    /// The turn changes the chat's Changes tab can show again.
    pub(in crate::ui) fn turn_changes(&self, session: &str) -> Option<std::sync::Arc<str>> {
        self.chat_ui
            .turn_changes
            .as_ref()
            .filter(|(owner, _)| **owner == *session)
            .map(|(_, diff)| Arc::clone(diff))
    }

    /// Whether the selected chat has a turn running.
    pub(in crate::ui) fn turn_running(&self) -> bool {
        self.client
            .state
            .session
            .as_ref()
            .is_some_and(|session| !session.active_turn_ids.is_empty())
    }

    pub(in crate::ui) fn chat_copied(&self, key: &str) -> bool {
        self.chat_ui
            .copied
            .as_ref()
            .is_some_and(|(copied, _)| copied == key)
    }

    /// Shows Copy's check for 1.5 s (MessageActionButton).
    pub(in crate::ui) fn mark_copied(&mut self, key: String, cx: &mut Context<Self>) {
        let timer = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            if let Err(error) = this.update(cx, |this, cx| {
                this.chat_ui.copied = None;
                cx.notify();
            }) {
                tracing::debug!(%error, "the workspace closed before the copied check cleared");
            }
        });
        self.chat_ui.copied = Some((key, timer));
        cx.notify();
    }

    /// A rejected open or create belongs to the chat pane, not to a toast.
    fn chat_rejected(
        &mut self,
        request_id: &str,
        message: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending_submission(request_id) {
            self.restore_rejected(request_id, message, window, cx);
        } else if self.chat_ui.history_request.as_deref() == Some(request_id) {
            // Keep the cursor so a failed page can be retried.
            self.chat_ui.history_request = None;
        } else if let Some(session) = self.client.selection_for(request_id) {
            self.chat_ui.failed_session = session;
        } else if self
            .queued_draft
            .as_ref()
            .is_some_and(|(id, _)| **id == *request_id)
        {
            self.announce(Tone::Warning, message.to_owned());
        } else if self.chat_ui.changes.owns(request_id) {
            // The pill refreshes in the background; SwiftUI's pill stays quiet too.
        } else {
            return;
        }
        self.shell
            .notices
            .retain(|(_, queued, _)| queued != message);
    }

    /// Puts the caret in the composer unless a dialog holds focus.
    pub(in crate::ui) fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        if !window.has_active_dialog(cx) {
            let handle = self.composer.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    /// Ticks the Session pill at 1 Hz while the selected chat runs; stops on its own.
    pub(in crate::ui) fn ensure_session_ticker(&mut self, cx: &Context<Self>) {
        let running = |this: &Self| {
            this.chat_ui.visible
                && matches!(this.primary_page(), Some(PageTab::Chat(_)))
                && this
                    .client
                    .state
                    .session
                    .as_ref()
                    .is_some_and(|session| session.run_stats.active.is_some())
        };
        if !running(self) {
            self.chat_ui.ticker = None;
            return;
        }
        if self.chat_ui.ticker.is_some() {
            return;
        }
        self.chat_ui.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let Ok(again) = this.update(cx, |this, cx| {
                    let again = running(this);
                    if !again {
                        this.chat_ui.ticker = None;
                    }
                    cx.notify();
                    again
                }) else {
                    return;
                };
                if !again {
                    return;
                }
            }
        }));
    }

    /// BotFace.swift `BotMood.chat`: readiness, reaction, listening, draft, running.
    fn chat_mood(&self, cx: &App) -> BotMood {
        let reaction =
            self.chat_ui.reaction.as_ref().filter(|(session, _)| {
                self.client.state.selected_session_id.as_ref() == Some(session)
            });
        if !self.client.link.is_ready() {
            BotMood::Sleepy
        } else if let Some((_, mood)) = reaction {
            *mood
        } else if self.dictating || self.voice_active {
            BotMood::Listening
        } else if !self.composer.read(cx).value().is_empty() {
            BotMood::Watching
        } else if self.turn_running() {
            BotMood::Thinking
        } else {
            BotMood::Idle
        }
    }

    /// The selected chat's lively Bot face; one per size so hero and header move independently.
    pub(in crate::ui) fn chat_face(
        &self,
        size: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<BotFaceView> {
        self.chat_face_with(size, None, window, cx)
    }

    /// The chat face with an explicit mood for placeholder states that override the rule.
    fn chat_face_with(
        &self,
        size: Pixels,
        mood: Option<BotMood>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<BotFaceView> {
        let (tint, shape) = self
            .active_conversation_bot()
            .or_else(|| self.composer_bot())
            .map_or_else(
                || (self.accent.color(), BotShape::Circle),
                |bot| (Accent::from(bot.tint).color(), bot.shape),
            );
        let mood = mood.unwrap_or_else(|| self.chat_mood(cx));
        lively_face(
            format!("chat-face:{size}"),
            tint,
            shape,
            mood,
            size,
            window,
            cx,
        )
    }

    pub(super) fn render_chat(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let placeholder = self.chat_placeholder_kind();
        let floats = placeholder.is_none();
        let body = if let Some(kind) = placeholder {
            self.chat_placeholder(&kind, window, cx)
        } else {
            // TextView must register selection on every painted frame; cached
            // scene replay skips registration and clears the highlight.
            self.transcript_region(window, cx)
        };
        // The session card takes its own column, beside the transcript, so the
        // conversation narrows beside it instead of running underneath.
        let card = self.session_card(window, cx);
        let composer = div()
            .flex()
            .flex_col()
            .child(self.composer_view(false, window, cx));
        // The composer floats over the transcript; its margins stay clear as content scrolls.
        let (floating, docked) = if floats {
            (
                Some(
                    Self::floating_composer(composer, cx)
                        .left_0()
                        .right_0()
                        .into_any_element(),
                ),
                None,
            )
        } else {
            (None, Some(composer))
        };
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(body)
                            .children(floating),
                    )
                    .children(card),
            )
            .children(docked)
            .children(self.bot_conversation_header(window, cx))
            .map(|chat| Self::attach_dropped_files(chat, cx))
            .into_any_element()
    }

    /// Files dropped on `target` attach to the message being written.
    fn attach_dropped_files(target: Div, cx: &Context<Self>) -> Div {
        target.on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
            this.attach_paths(paths.paths().to_vec(), cx);
        }))
    }

    /// The composer floating at the full view's bottom-right corner over a document, so
    /// the document keeps the whole width while a message is written.
    pub(in crate::ui) fn document_composer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let composer = div()
            .flex()
            .flex_col()
            .child(self.composer_view(true, window, cx));
        Self::attach_dropped_files(Self::floating_composer(composer, cx), cx)
            .right_0()
            .w(width::COMPOSER_FLOAT)
            .max_w_full()
            .into_any_element()
    }

    /// The room the composer floating over a full-width document takes at its foot, while
    /// it floats there.
    pub(in crate::ui) fn document_composer_inset(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<Pixels> {
        (self.active_content().is_some()
            && matches!(self.primary_page(), Some(PageTab::Chat(_) | PageTab::Draft))
            && self.pane_split(window, cx).is_none())
        .then_some(self.chat_ui.composer_inset)
    }

    /// The composer laid over its page's foot, measuring itself so the transcript (or a
    /// native document view) can leave its height free; callers place it horizontally.
    fn floating_composer(composer: Div, cx: &Context<Self>) -> Div {
        let entity = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, _, cx| {
                let height = bounds.size.height;
                cx.defer(move |cx| {
                    if let Err(error) = entity.update(cx, |this, cx| {
                        if (this.chat_ui.composer_inset - height).abs() > px(0.5) {
                            this.chat_ui.composer_inset = height;
                            cx.notify();
                        }
                    }) {
                        tracing::debug!(%error, "the workspace closed before the composer was measured");
                    }
                });
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full();
        div().absolute().bottom_0().child(measure).child(composer)
    }

    /// The live transcript, including per-frame text selection registration.
    pub(in crate::ui) fn transcript_region(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        #[cfg(feature = "snapshot")]
        super::snapshot::count_render(1);
        self.refresh_decisions(cx);
        let state = &self.client.state;
        let selected = state.selected_session_id.as_ref();
        let scope = format!("chat:{}", selected.map_or("", |id| id.as_str()));
        let revision = state.transcript.structure_revision();
        let projection_changed =
            !self
                .chat_ui
                .projection
                .as_ref()
                .is_some_and(|(session, previous)| {
                    session.as_ref() == selected && *previous == revision
                });
        if projection_changed {
            self.chat_ui.rows = state
                .transcript
                .rows()
                .map(|row| Arc::clone(&row.id))
                .collect();
            self.chat_ui.projection = Some((selected.map(SessionId::share), revision));
        }
        let projected = &self.chat_ui.rows;
        self.chat_ui.running_row = self.live_worked_row();
        let standalone = self.chat_ui.running_row.is_none() && self.chat_ui.waiting.is_some();
        // The list's items, oldest first: the projected rows, the waiting line, held
        // capability widgets and messages still on their way.
        let mut tail_ids = Vec::new();
        let mut tail_items = Vec::new();
        if standalone {
            tail_items.push(ChatItem::Waiting);
            tail_ids.push(waiting::TAIL_ID.into());
        }
        for (index, key) in self.transcript_tail_keys().into_iter().enumerate() {
            tail_items.push(ChatItem::Tail(index));
            tail_ids.push(key.into());
        }
        if self.has_pending_decisions() {
            tail_items.push(ChatItem::Decisions);
            tail_ids.push("decisions:tail".into());
        }
        for (index, (id, _)) in self.pending_messages().into_iter().enumerate() {
            tail_items.push(ChatItem::Pending(index));
            tail_ids.push(format!("pending-message:{id}").into());
        }
        let history = self.history_pending();
        let pins = || {
            self.client
                .state
                .session
                .iter()
                .flat_map(|session| &session.pinned_messages)
        };
        let pins_changed = !pins().eq(&self.chat_ui.rail_pins);
        if pins_changed {
            self.chat_ui.rail_pins = pins().copied().collect();
        }
        if projection_changed
            || history != self.chat_ui.history_visible
            || tail_ids != self.chat_ui.tail_ids
            || self.chat_ui.list.is_none()
        {
            let mut items = Vec::new();
            let mut ids = Vec::new();
            if history {
                items.push(ChatItem::History);
                ids.push(transcript::HISTORY.into());
            }
            for (index, row) in projected.iter().enumerate() {
                items.push(ChatItem::Row(index));
                ids.push(Arc::clone(row));
            }
            ids.extend(tail_ids.iter().map(Arc::clone));
            items.extend(tail_items);
            self.chat_ui.list =
                Some(self.prepare_chat_list(&scope, ids, !self.client.state.replaying, window, cx));
            self.chat_ui.tail_ids = tail_ids;
            self.chat_ui.history_visible = history;
            self.chat_ui.items = items;
            self.refresh_rail_marks();
        } else if pins_changed || self.rail_words_stale() {
            self.refresh_rail_marks();
        }
        let Some(list) = self.chat_ui.list.as_ref() else {
            return div().into_any_element();
        };
        let list = gpui::ListState::clone(list);
        let rail = self.message_rail(&scope, cx);
        let entity = cx.entity().downgrade();
        let render = move |index: usize, window: &mut Window, cx: &mut App| {
            entity
                .update(cx, |this, cx| this.chat_item(index, window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        };
        self.chat_list(&scope, list, rail, render, cx)
    }

    /// A running turn's Worked row owns the waiting phrase from the moment it exists
    /// (TranscriptProjection.waitingSlot); otherwise one line follows the transcript.
    fn live_worked_row(&self) -> Option<Arc<str>> {
        self.client
            .state
            .transcript
            .rows()
            .rev()
            .find(|row| {
                matches!(
                    row.kind,
                    mobius_gateway::wire::TranscriptRowKind::Worked {
                        duration_ms: None,
                        ..
                    }
                )
            })
            .filter(|_| self.turn_running())
            .map(|row| Arc::clone(&row.id))
    }

    /// One item of the chat list, spaced from the next as the old column's gap was.
    fn chat_item(&self, index: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let session_id = self.client.state.selected_session_id.as_ref();
        let scope = format!("chat:{}", session_id.map_or("", SessionId::as_str));
        let last = index + 1 == self.chat_ui.items.len();
        let item = match self.chat_ui.items.get(index) {
            Some(ChatItem::History) => Some(self.earlier_history(cx)),
            Some(ChatItem::Row(row)) => self
                .chat_ui
                .rows
                .get(*row)
                .and_then(|id| self.client.state.transcript.row(id))
                .map(|row| {
                    // A question's call or an approval's block says only what its card says.
                    let decisions = self.row_decisions(row, cx);
                    let view = decisions.show_source.then(|| {
                        self.transcript_row_view(
                            &self.client.state.transcript,
                            row,
                            session_id,
                            &scope,
                            cx,
                        )
                    });
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::S)
                        .children(view)
                        .children(decisions.cards.into_iter().map(|card| {
                            div()
                                .mx_auto()
                                .w_full()
                                .max_w(width::TRANSCRIPT)
                                .child(card)
                        }))
                        .into_any_element()
                }),
            Some(ChatItem::Waiting) => self.waiting_tail(&scope, cx),
            Some(ChatItem::Tail(tail)) => self.transcript_tail(cx).into_iter().nth(*tail),
            Some(ChatItem::Decisions) => self.decision_deck(cx),
            Some(ChatItem::Pending(pending)) => self.pending_bubbles(cx).into_iter().nth(*pending),
            None => None,
        };
        div()
            .w_full()
            .when(!last, |item| item.pb(space::S))
            .children(item)
            .into_any_element()
    }

    /// Fetches one earlier turn: its own pages first, then what a fork inherits.
    /// One request at a time, never twice for
    /// the same cursor; the gateway answers each with exactly one turn.
    /// A quiet divider above the oldest loaded turn; it pages in earlier ones.
    fn earlier_history(&self, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let loading = self.chat_ui.history_request.is_some();
        let rule = || hairline(&p).flex_1();
        let label = if loading {
            div()
                .flex()
                .items_center()
                .gap(space::S)
                .child(Spinner::new(glyph::MARK).color(p.muted))
                .child(composer::status_text("Loading earlier messages…"))
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap(space::XS)
                .child(chevron(true, &p))
                .child("Earlier messages")
                .into_any_element()
        };
        div()
            .id("earlier-history")
            .flex()
            .items_center()
            .gap(space::S)
            .py(space::S)
            .typography(Type::Caption)
            .text_color(p.muted)
            .child(rule())
            .child(label)
            .child(rule())
            .when(!loading, |row| {
                row.cursor_pointer()
                    .hover(|style| style.text_color(p.text))
                    .on_click(cx.listener(|this, _, _, cx| this.continue_history(cx)))
            })
            .into_any_element()
    }

    pub(in crate::ui) fn continue_history(&mut self, cx: &mut Context<Self>) {
        if self.client.state.replaying
            || self.chat_ui.history_request.is_some()
            || !self.active_session_ready()
        {
            return;
        }
        let (Some(session), Some(before)) = (
            self.client.state.session.as_ref(),
            &self.client.state.history,
        ) else {
            return;
        };
        let request_id = crate::gateway::request_id();
        self.chat_ui.history_request = Some(Arc::clone(&request_id));
        let message = mobius_gateway::wire::ClientMessage::GetSessionHistory {
            request_id,
            session_id: session.session.session_id.share(),
            before: Some(before.clone()), // rules-allow: the state keeps the cursor until the page arrives
        };
        self.send(message, cx);
    }

    /// Earlier turns, of this chat or the one it was forked from, can be requested.
    pub(in crate::ui) fn history_pending(&self) -> bool {
        self.client.state.history.is_some()
    }
}

/// What one row of the chat list draws.
pub(in crate::ui) enum ChatItem {
    History,
    Row(usize),
    Waiting,
    Tail(usize),
    /// An open approval with no loaded transcript anchor.
    Decisions,
    Pending(usize),
}
