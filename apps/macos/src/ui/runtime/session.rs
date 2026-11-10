use super::*;
use mobius::SessionId;
use mobius::protocol::{MessageAuthor, MessageSubmission};

pub(in crate::ui) enum DraftIntent {
    Message(String),
    Voice,
    Attachments(Vec<std::path::PathBuf>),
}

/// Preserve new typing when an earlier message returns unsent.
pub(super) fn recovered_draft(current: &str, unsent: &str) -> String {
    if current.is_empty() {
        unsent.into()
    } else if unsent.is_empty() || current == unsent {
        current.into()
    } else {
        format!("{current}\n\n{unsent}")
    }
}

impl RuntimeState {
    /// Binds the composer to the draft at `key`; returns the text to show unless it is
    /// already bound there.
    pub(super) fn bind_draft(
        &mut self,
        key: String,
        drafts: &mut BTreeMap<String, String>,
        current: &str,
    ) -> Option<String> {
        if self.draft_owner.as_ref() == Some(&key) {
            return None;
        }
        if self.draft_owner.is_none() && !current.is_empty() {
            drafts.insert(key.clone(), current.into()); // rules-allow: the drafts map and the draft owner each hold the chat's key
        }
        let value = drafts.get(&key).cloned().unwrap_or_default(); // rules-allow: the composer gets its own copy; the map keeps the draft
        self.draft_owner = Some(key);
        Some(value)
    }
}

/// Whether the pending open or create already leads to `page`.
pub(super) fn selection_follows(target: Option<Option<&SessionId>>, page: &PageTab) -> bool {
    match page {
        PageTab::Content(_) => true,
        PageTab::Draft => target.is_some_and(|session| session.is_none()),
        PageTab::Chat(id) => target.is_some_and(|session| session == Some(id)),
        _ => false,
    }
}

impl Workspace {
    pub(super) fn confirm_message(
        &mut self,
        event: &mobius::protocol::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let (Some(id), EventMsg::SubmissionRejected(rejected)) =
            (&event.submission_id, &event.msg)
        {
            self.restore_rejected(id, &rejected.message, window, cx);
        }
        let Some(sent) = self.client.confirmed_message(event) else {
            return;
        };
        self.runtime.forget_sent(&sent);
        if self.drafts.get(&sent.draft_key) == Some(&sent.text) {
            self.drafts.remove(&sent.draft_key);
        }
        if self.runtime.draft_owner.as_ref() == Some(&sent.draft_key)
            && self.composer.read(cx).value().as_str() == sent.text
        {
            self.composer
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    pub(in crate::ui) fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation_section = navigation::Section::Chats;
        self.save_draft(cx);
        self.stop_dictation();
        self.stop_voice();
        // The session resets below, so its workspace path moves to the draft.
        if let Some(session) = &mut self.client.state.session {
            if let Some(workspace) = &mut session.workspace {
                self.draft_workspace = std::mem::take(&mut workspace.path);
            }
            self.draft_bot = Some(Arc::clone(&session.session.context.owner_id));
        }
        self.cancel_session_selection();
        self.preserve_previews();
        self.client.state.reset_session();
        self.activate_tab(PageTab::Draft);
        self.load_draft("", window, cx);
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(in crate::ui) fn new_chat_for(
        &mut self,
        bot_id: Arc<str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_chat(window, cx);
        self.draft_bot = Some(bot_id);
    }

    pub(in crate::ui) fn draft_bot(&self) -> Option<&mobius_gateway::wire::BotRecord> {
        let bots = &self.client.state.gateway.as_ref()?.bots;
        self.draft_bot
            .as_ref()
            .and_then(|id| bots.iter().find(|bot| bot.id == *id))
            .or_else(|| bots.iter().find(|bot| &*bot.handle == "mobius"))
            .or_else(|| bots.first())
    }

    pub(in crate::ui) fn create_draft(&mut self, intent: DraftIntent, cx: &mut Context<Self>) {
        if !self.client.link.is_ready() || !self.draft_selected() || self.client.selecting() {
            return;
        }
        let Some(bot_id) = self.draft_bot().map(|bot| Arc::clone(&bot.id)) else {
            self.report("Configure a Bot in the connected gateway first.");
            cx.notify();
            return;
        };
        let request_id = gateway::request_id();
        let message = ClientMessage::CreateSession {
            request_id: Arc::clone(&request_id),
            workspace: self.draft_workspace.clone(), // rules-allow: the draft keeps its workspace until the chat opens
            bot_id,
        };
        match self.enqueue(message) {
            Ok(()) => self.queued_draft = Some((request_id, intent)),
            Err(error) => self.report(error),
        }
        cx.notify();
    }

    pub(in crate::ui) fn activate_tab(&mut self, tab: PageTab) {
        let previous = panel::TabOwner::of(&self.page_tabs);
        if self.split_content
            && matches!(&tab, PageTab::Content(id) if self.content.navigation.contains(&previous, id))
        {
            self.close_session_card();
        }
        self.active_page =
            self.content
                .navigation
                .activate(&mut self.page_tabs, self.active_page, tab);
        if previous != panel::TabOwner::of(&self.page_tabs) {
            self.motion.pane_shown = None;
            self.motion.pane = None;
            self.native.hide_panel();
        }
        self.refresh_computer_title();
        self.prune_thumbnails();
        self.history.record(&self.page_tabs, self.active_page);
    }

    pub(in crate::ui) fn open_page(
        &mut self,
        tab: PageTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(section) = tab.section() {
            self.navigation_section = section;
        }
        self.activate_tab(tab.clone()); // rules-allow: selecting the requested chat must still load it when its restored document is active
        self.select_route(tab, window, cx);
    }

    pub(super) fn send_failed(
        &mut self,
        request_id: Option<&str>,
        delivery_uncertain: bool,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Loading an existing chat cannot deliver a message or create another chat.
        let chat_send = request_id.is_some_and(|id| {
            self.pending_submission(id) || self.client.selection_for(id) == Some(None)
        });
        self.client.storage.send_failed(request_id);
        let restored = !delivery_uncertain
            && request_id.is_some_and(|id| self.restore_rejected(id, &message, window, cx));
        if request_id.is_some_and(|id| self.client.failed_request(id, delivery_uncertain)) {
            self.cancel_session_selection();
        }
        if !restored {
            self.report(if delivery_uncertain && chat_send {
                format!(
                    "Delivery is uncertain. Reconnect to check this chat before sending again. {message}"
                )
            } else {
                message
            });
        }
        self.set_link(Link::Failed(
            if chat_send {
                "Delivery failed."
            } else {
                "Connection failed."
            }
            .into(),
        ));
    }

    pub(in crate::ui) fn desktop_execution_changed(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let held = std::mem::replace(&mut self.runtime.desktop_held, enabled);
        if enabled {
            self.cancel_session_selection();
        } else if held
            && let Some(session_id) = self
                .client
                .state
                .selected_session_id
                .as_ref()
                .map(SessionId::share)
        {
            self.send(
                ClientMessage::OpenSession {
                    request_id: gateway::request_id(),
                    session_id,
                    last_sequence: None,
                },
                cx,
            );
        }
    }

    pub(in crate::ui) fn cancel_session_selection(&mut self) {
        self.client.cancel_selection();
        self.queued_draft = None;
    }

    pub(in crate::ui) fn select_page(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = self.page_tabs.get(index).cloned(); // rules-allow: selecting the page borrows the workspace mutably
        let Some(page) = page else {
            return;
        };
        self.active_page = index;
        self.content
            .navigation
            .remember(&self.page_tabs, self.active_page);
        self.select_route(page, window, cx);
    }

    fn select_route(&mut self, page: PageTab, window: &mut Window, cx: &mut Context<Self>) {
        self.save_draft(cx);
        self.stop_dictation();
        if voice::voice_session_changed(self.client.state.selected_session_id.as_ref(), &page) {
            self.stop_voice();
        }
        self.history.record(&self.page_tabs, self.active_page);
        let follows = selection_follows(self.client.selection_target(), &page);
        if !follows {
            self.cancel_session_selection();
        }
        self.load_page_data(&page, cx);
        if matches!(page, PageTab::Draft) {
            self.load_draft("", window, cx);
        }
        if let PageTab::Chat(id) = page {
            self.load_draft(&id, window, cx);
            if follows || self.active_session_ready() {
                cx.notify();
                return;
            }
            self.send(
                ClientMessage::OpenSession {
                    request_id: gateway::request_id(),
                    session_id: id,
                    last_sequence: None,
                },
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn save_draft(&mut self, cx: &Context<Self>) {
        if let Some(key) = &self.runtime.draft_owner {
            self.drafts
                .insert(key.clone(), self.composer.read(cx).value().to_string()); // rules-allow: the drafts map and the draft owner each hold the chat's key
        }
    }

    pub(super) fn preserve_previews(&mut self) {
        for tab in self.content.tabs.values_mut() {
            if let TabContent::Preview {
                session_id,
                preview,
            } = &mut tab.content
                && self.client.state.selected_session_id.as_ref() == Some(session_id)
                && let Some(current) = self.client.state.previews.get(&preview.source.id)
            {
                *preview = std::rc::Rc::clone(current);
            }
        }
    }

    pub(super) fn finish_session_open(
        &mut self,
        request_id: &str,
        session_id: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let queued = self
            .queued_draft
            .take()
            .filter(|(id, _)| **id == *request_id);
        self.active_page = self.content.navigation.session_opened(
            &mut self.page_tabs,
            self.active_page,
            session_id,
        );
        self.refresh_computer_title();
        self.prune_thumbnails();
        self.history.record(&self.page_tabs, self.active_page);
        if let Some((_, intent)) = queued {
            let key = self.client.draft_key(session_id);
            self.drafts.remove(&self.client.draft_key(""));
            self.drafts
                .insert(key.clone(), self.composer.read(cx).value().to_string()); // rules-allow: the drafts map, draft owner and attachments each hold the chat's key
            self.runtime.draft_owner = Some(key.clone()); // rules-allow: the drafts map, draft owner and attachments each hold the chat's key
            match intent {
                DraftIntent::Message(text) => {
                    self.submit_message(session_id, text, Vec::new(), None, cx);
                }
                DraftIntent::Voice => self.toggle_voice(cx),
                DraftIntent::Attachments(paths) => self.attach_files(key, paths, cx),
            }
        } else {
            self.restore_draft(window, cx);
        }
        self.focus_composer(window, cx);
    }

    pub(super) fn restore_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)
        {
            self.load_draft(&id, window, cx);
        }
    }

    pub(super) fn load_draft(
        &mut self,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.composer.read(cx).value();
        if let Some(value) = self.runtime.bind_draft(
            self.client.draft_key(session_id),
            &mut self.drafts,
            current.as_str(),
        ) {
            self.composer
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }

    pub(in crate::ui) fn send_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send_message_as(None, window, cx);
    }

    /// Sends the composer, asking for `delivery` when a turn is already running.
    pub(in crate::ui) fn send_message_as(
        &mut self,
        delivery: Option<mobius::protocol::ActiveMessageDelivery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composer_unavailable().is_some() {
            return;
        }
        let text = self.composer.read(cx).value().to_string();
        if self.draft_selected() && self.client.link.is_ready() && !text.trim().is_empty() {
            self.stop_dictation();
            self.create_draft(DraftIntent::Message(text), cx);
            return;
        }
        if self.draft_is_empty(cx)
            || !self.client.link.is_ready()
            || !self.active_session_ready()
            || !self.attachments_ready()
        {
            return;
        }
        let Some(session_id) = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)
        else {
            return;
        };
        let key = self.client.draft_key(&session_id);
        let attachments: Vec<_> = self
            .draft_attachments()
            .iter()
            .filter_map(|item| item.file.clone()) // rules-allow: the draft keeps its attachments until delivery is confirmed
            .collect();
        let attachment_ids: Vec<_> = self
            .draft_attachments()
            .iter()
            .map(|item| Arc::clone(&item.id))
            .collect();
        if self
            .client
            .pending_messages(&key)
            .any(|(_, message)| message.text == text && message.attachments == attachment_ids)
        {
            self.report("This message is awaiting confirmation. Reconnect to check whether it was delivered before sending it again.");
            cx.notify();
            return;
        }
        self.stop_dictation();
        if !self.submit_message(&session_id, text, attachments, delivery, cx) {
            return;
        }
        // The message shows as a pending bubble; the composer is free for the next one.
        self.drafts.remove(&key);
        self.composer
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// Messages sent from the current chat that the gateway has not taken up yet, by
    /// submission id.
    pub(in crate::ui) fn pending_messages(&self) -> Vec<(&str, &PendingMessage)> {
        let Some(owner) = &self.runtime.draft_owner else {
            return Vec::new();
        };
        self.client.pending_messages(owner).collect()
    }

    /// Shows `text` as sent from the selected chat and not yet taken up, until the next
    /// scenario starts.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn fixture_pending(&mut self, id: &str, text: &str) {
        let session = self
            .client
            .state
            .selected_session_id
            .as_deref()
            .unwrap_or_default();
        let message = PendingMessage {
            draft_key: self.client.draft_key(session),
            text: text.into(),
            attachments: Vec::new(),
            files: Vec::new(),
            reply: None,
        };
        self.client.insert_pending(id.into(), message);
        crate::ui::snapshot::undo_after_capture(|this| this.client.clear_pending());
    }

    pub(in crate::ui) fn pending_submission(&self, id: &str) -> bool {
        self.client.pending_submission(id)
    }

    /// Puts a rejected message back in the composer and says why.
    pub(in crate::ui) fn restore_rejected(
        &mut self,
        submission: &str,
        reason: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Every caller drops the refused message right after, so it moves out here.
        let Some(PendingMessage {
            draft_key: key,
            text,
            reply,
            ..
        }) = self.client.take_pending(submission)
        else {
            return false;
        };
        if self.runtime.draft_owner.as_ref() == Some(&key) {
            let current = self.composer.read(cx).value();
            let text = recovered_draft(current.as_str(), &text);
            self.composer
                .update(cx, |input, cx| input.set_value(text, window, cx));
        } else {
            self.drafts
                .entry(key)
                .and_modify(|current| *current = recovered_draft(current, &text))
                .or_insert(text);
        }
        self.restore_reply(reply);
        self.announce(
            Tone::Warning,
            format!("Your message was not sent. Its text was returned to the draft. {reason}"),
        );
        cx.notify();
        true
    }

    fn submit_message(
        &mut self,
        session_id: &SessionId,
        text: String,
        attachments: Vec<mobius::protocol::SessionFileReference>,
        requested_delivery: Option<mobius::protocol::ActiveMessageDelivery>,
        cx: &mut Context<Self>,
    ) -> bool {
        let message = MessageSubmission {
            author: MessageAuthor::User,
            text,
            attachments,
            reply: self.draft_reply(session_id).map(|(_, reply, _)| reply),
            requested_delivery,
            target_turn_id: None,
        };
        match self.enqueue(gateway::submit(session_id.share(), Op::Message { message })) {
            Ok(()) => {
                self.take_reply(session_id);
                cx.notify();
                true
            }
            Err(error) => {
                self.report(error);
                cx.notify();
                false
            }
        }
    }
}
