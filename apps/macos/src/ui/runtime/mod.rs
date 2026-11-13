mod attachments;
mod link;
mod native;
mod persist;
mod session;
mod thumbnails;
mod toast;
mod voice;

use super::*;
pub(in crate::ui) use crate::gateway::Link;
use crate::gateway::{self, Arrival, Connection, PendingMessage, Update};
pub(super) use attachments::DraftAttachment;
pub(super) use link::LinkTone;
use mobius::protocol::{EventMsg, Op};
use mobius_gateway::wire::{ClientMessage, ServerMessage};
pub(super) use native::DictationDraft;
pub(super) use session::DraftIntent;
#[cfg(feature = "snapshot")]
pub(in crate::ui) use thumbnails::ThumbnailKey;
pub(in crate::ui) use thumbnails::{Thumbnail, drawable, private_path};
pub(in crate::ui) use toast::Toast;

#[derive(Default)]
pub(super) struct RuntimeState {
    draft_owner: Option<String>,
    attachments: BTreeMap<String, Vec<DraftAttachment>>,
    pub(in crate::ui) thumbnails: thumbnails::Thumbnails,
    pub(super) desktop_held: bool,
}

impl RuntimeState {
    /// The composer no longer holds what a confirmed message carried.
    fn forget_sent(&mut self, sent: &PendingMessage) {
        if let Some(attachments) = self.attachments.get_mut(&sent.draft_key) {
            attachments.retain(|item| !sent.attachments.contains(&item.id));
        }
        for id in &sent.attachments {
            self.thumbnails.forget_draft(id);
        }
    }
}

impl Workspace {
    pub(in crate::ui) fn active_session_ready(&self) -> bool {
        let tab = self.page_tabs.get(self.active_page);
        let tab = if matches!(tab, Some(PageTab::Content(_))) {
            self.page_tabs
                .iter()
                .find(|tab| matches!(tab, PageTab::Chat(_)))
        } else {
            tab
        };
        let selected = self.client.state.selected_session_id.as_ref();
        let payload = self
            .client
            .state
            .session
            .as_ref()
            .map(|session| &session.session.session_id);
        !self.client.selecting()
            && matches!(tab, Some(PageTab::Chat(id)) if Some(id) == selected && selected == payload)
    }

    /// Whether `gateway` is the one this window is connected to, or connecting to.
    pub(in crate::ui) fn is_current(&self, gateway: &GatewayRef) -> bool {
        match gateway {
            GatewayRef::Local => self.client.local_gateway,
            GatewayRef::Saved(endpoint) => {
                !self.client.local_gateway && *endpoint == self.client.endpoint
            }
        }
    }

    /// The gateway this window is connected to, or connecting to.
    pub(in crate::ui) fn current_gateway(&self) -> GatewayRef {
        if self.client.local_gateway {
            GatewayRef::Local
        } else {
            GatewayRef::Saved(self.client.endpoint.clone()) // rules-allow: GatewayRef owns its endpoint beyond this borrow
        }
    }

    pub(super) fn send(&mut self, message: ClientMessage, cx: &mut Context<Self>) {
        if let Err(error) = self.enqueue(message) {
            self.report(error);
        }
        cx.notify();
    }

    pub(super) fn enqueue(&mut self, message: ClientMessage) -> Result<(), String> {
        let pending = self.pending_message(&message);
        let command = match &message {
            ClientMessage::Submit { submission, .. }
                if matches!(submission.op, Op::CapabilityCommand { .. }) =>
            {
                Some(Arc::clone(&submission.id))
            }
            _ => None,
        };
        let selecting = self.client.send(message, pending)?;
        if let Some(id) = command {
            self.content.track_command(id);
        }
        if selecting {
            self.chat_ui.open_failure = None;
        }
        Ok(())
    }

    /// What the composer must keep of a message it sends, until the gateway takes it up.
    fn pending_message(&self, message: &ClientMessage) -> Option<PendingMessage> {
        let ClientMessage::Submit {
            session_id,
            submission,
        } = message
        else {
            return None;
        };
        let Op::Message { message } = &submission.op else {
            return None;
        };
        Some(PendingMessage {
            draft_key: self.client.draft_key(session_id),
            text: message.text.clone(), // rules-allow: the message goes to the gateway; this copy restores the draft if refused
            attachments: self
                .draft_attachments()
                .iter()
                .filter(|item| {
                    item.file
                        .as_ref()
                        .is_some_and(|file| message.attachments.contains(file))
                })
                .map(|item| Arc::clone(&item.id))
                .collect(),
            files: message.attachments.clone(), // rules-allow: the message goes to the gateway; this copy restores the draft if refused
            reply: self
                .draft_reply(session_id)
                .filter(|(_, reply, _)| message.reply.as_ref() == Some(reply)),
        })
    }

    pub(super) fn submit_op(&mut self, op: Op, cx: &mut Context<Self>) {
        if self.client.link.is_ready()
            && self.active_session_ready()
            && let Some(id) = &self.client.state.selected_session_id
        {
            self.send(gateway::submit(id.share(), op), cx);
        }
    }

    pub(super) fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.native.poll() {
            Ok(events) => {
                for event in events {
                    self.native_event(event, window, cx);
                }
            }
            Err(error) => self.report(format!("{error:#}")),
        }
        self.request_thumbnails();
        self.reap_thumbnails(window, cx);
        let usage = self.menu_bar_usage();
        if usage != self.shell.menu_bar_usage {
            crate::host::platform::set_status_usage(&usage);
            self.shell.menu_bar_usage = usage;
        }
        self.next_toast(cx);
    }

    pub(super) fn gateway_update(
        &mut self,
        update: Update,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.storage_update(&update);
        self.regions
            .gateway_update(&update, self.client.state.selected_session_id.as_deref());
        match update {
            Update::Connecting {
                label,
                same_endpoint,
            } => self.connecting(label, same_endpoint, window, cx),
            Update::Reconnecting {
                endpoint,
                attempt,
                message,
            } => self.reconnecting(endpoint, attempt, message),
            Update::Connected(endpoint) => self.connected(endpoint, window, cx),
            Update::Resuming(request_id, session_id) => self.resuming(request_id, session_id),
            Update::Message(message) => self.receive(*message, window, cx),
            Update::Error(error) => {
                self.report(&error);
                self.set_link(Link::Failed(error.into()));
            }
            Update::SendFailed {
                request_id,
                delivery_uncertain,
                message,
            } => self.send_failed(
                request_id.as_deref(),
                delivery_uncertain,
                message,
                window,
                cx,
            ),
            Update::StorageExported {
                id,
                endpoint,
                result,
            } if endpoint == self.client.endpoint => self.storage_exported(&id, result, cx),
            Update::OperationError { endpoint, message } if endpoint == self.client.endpoint => {
                self.report(message);
            }
            // Previews download into the app's private folder; anything else was a download
            // the user asked for.
            Update::Downloaded { path, .. }
                if !self
                    .downloads
                    .as_ref()
                    .is_some_and(|folder| path.starts_with(folder.path())) =>
            {
                self.announce_saved(path);
            }
            Update::Downloaded {
                endpoint,
                session_id,
                file,
                path,
            } if self.client.link.is_ready() && endpoint == self.client.endpoint => {
                self.open_file_preview_for(
                    panel::TabOwner::Chat(session_id.share()),
                    format!("{session_id}:{}", file.id),
                    file.name,
                    path,
                    cx,
                );
            }
            Update::ThumbnailSource {
                endpoint,
                session_id,
                file_id,
                result,
            } => {
                self.thumbnail_result(&endpoint, session_id, file_id, result, cx);
            }
            Update::PreviewLoaded {
                endpoint,
                id,
                session_id,
                result,
            } if endpoint == self.client.endpoint => {
                self.preview_loaded(&id, &session_id, result, cx);
            }
            Update::Disconnected => self.set_link(Link::Failed("The gateway disconnected.".into())),
            Update::AttachmentDeleted {
                endpoint,
                session_id,
                id,
                result,
            } => self.attachment_deleted(&endpoint, &session_id, &id, result),
            Update::Uploaded {
                endpoint,
                session_id,
                id,
                result,
            } => self.uploaded(&endpoint, &session_id, &id, result),
            update @ Update::Recent { .. } => self.recent_loaded(update),
            Update::OperationError { .. }
            | Update::StorageExported { .. }
            | Update::PreviewLoaded { .. }
            | Update::Downloaded { .. } => {}
        }
    }

    fn receive(&mut self, mut message: ServerMessage, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace_changes_answer(&mut message);
        match self.client.arrive(&mut message) {
            Arrival::Ignore => return,
            Arrival::ResumeRejected => {
                self.select_primary_page(window, cx);
                return;
            }
            Arrival::Opened => {
                self.save_draft(cx);
                self.preserve_previews();
            }
            Arrival::Apply => {}
        }
        self.storage_message(&message, cx);
        self.forward_native(&message);
        self.provider_event(&message, cx);
        if let Some(error) = self.client.apply(&mut message) {
            self.report(error);
        }
        if matches!(message, ServerMessage::Ready { .. }) {
            self.set_link(Link::Ready);
        }
        self.dispatch_message(&message, window, cx);
        match message {
            ServerMessage::RoutineRunPreview {
                request_id,
                preview,
            } if self.client.state.routine_preview_request.as_ref() == Some(&request_id) => {
                self.client.state.routine_preview_request = None;
                self.open_routine_preview(preview, cx);
            }
            ServerMessage::SessionOpened {
                request_id,
                payload,
            } => {
                self.finish_session_open(&request_id, &payload.session.session_id, window, cx);
            }
            ServerMessage::AgentEvent { record, .. } => {
                self.confirm_message(&record.event, window, cx);
            }
            ServerMessage::DesktopHostRequested { .. } => self.notice_computer_setup(),
            ServerMessage::Rejected {
                request_id,
                fatal,
                message,
                ..
            } => self.request_rejected(&request_id, fatal, message),
            ServerMessage::Error {
                fatal: true,
                message,
                ..
            } => self.set_link(Link::Failed(message)),
            _ => {}
        }
    }

    /// The gateway refused `request_id`: drop what waited on it and say why.
    fn request_rejected(&mut self, request_id: &Arc<str>, fatal: bool, message: Arc<str>) {
        if self
            .queued_draft
            .as_ref()
            .is_some_and(|(id, _)| id == request_id)
        {
            self.queued_draft = None;
        }
        if self.client.rejected(request_id) {
            self.chat_ui.open_failure = Some(Arc::clone(&message));
        }
        if fatal {
            self.set_link(Link::Failed(message));
        }
    }

    /// Each package reacts to server messages in its own hook, after the shared state applied them.
    fn dispatch_message(
        &mut self,
        message: &ServerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shell_on_message(message, window, cx);
        self.chat_on_message(message, window, cx);
        self.content_on_message(message, window, cx);
        self.bots_on_message(message, window, cx);
        self.memory_on_message(message, cx);
        self.config_on_message(message, window, cx);
        if matches!(
            message,
            ServerMessage::Ready { .. } | ServerMessage::GatewayConfigured { .. }
        ) {
            self.remind_risky_defaults(cx);
        }
        if matches!(message, ServerMessage::Ready { .. }) {
            let page = self.primary_page().cloned(); // rules-allow: loading page data borrows the workspace mutably
            if let Some(page) = page {
                self.load_page_data(&page, cx);
            }
            self.resume_after_ready(window, cx);
        }
    }

    pub(super) fn refresh_appearance(&mut self, cx: &mut Context<Self>) {
        self.install_palette(FontSet::current(), cx);
        cx.notify();
    }

    /// Installs the palette of the appearance on screen, dressed for its window background.
    fn install_palette(&mut self, font_set: FontSet, cx: &mut Context<Self>) {
        let palette = (preferences::Preferences {
            appearance: self.appearance,
            accent: self.accent,
            font_set,
            dark_colors: self.dark_colors,
            light_colors: self.light_colors,
            ..Default::default()
        })
        .palette(cx);
        self.palette = self.dress(palette, cx);
        self.palette.install(cx);
    }

    pub(super) fn set_font_set(&mut self, font_set: FontSet, cx: &mut Context<Self>) {
        self.install_palette(font_set, cx);
        self.save_preferences(
            "the font",
            move |preferences| preferences.font_set = font_set,
            cx,
        );
        cx.notify();
    }

    pub(super) fn set_appearance(
        &mut self,
        appearance: Appearance,
        accent: Accent,
        cx: &mut Context<Self>,
    ) {
        self.appearance = appearance;
        self.accent = accent;
        self.save_appearance(cx);
    }

    /// Sets the background and foreground of the appearance on screen.
    pub(super) fn set_colors(&mut self, colors: theme::Colors, cx: &mut Context<Self>) {
        if self.palette.is_dark() {
            self.dark_colors = colors;
        } else {
            self.light_colors = colors;
        }
        self.save_appearance(cx);
    }

    fn save_appearance(&mut self, cx: &mut Context<Self>) {
        self.refresh_appearance(cx);
        let (appearance, accent) = (self.appearance, self.accent);
        let (dark_colors, light_colors) = (self.dark_colors, self.light_colors);
        self.save_preferences(
            "the appearance",
            move |preferences| {
                preferences.appearance = appearance;
                preferences.accent = accent;
                preferences.dark_colors = dark_colors;
                preferences.light_colors = light_colors;
            },
            cx,
        );
    }
}

#[cfg(test)]
mod tests;

impl Workspace {
    /// Queues a toast of `tone`, such as a success or a background chat finishing.
    pub(in crate::ui) fn announce(&mut self, tone: Tone, message: String) {
        if !self
            .shell
            .notices
            .iter()
            .any(|(_, queued, _)| *queued == message)
        {
            self.shell.notices.push((tone, message, None));
            crate::host::platform::wake();
        }
    }

    /// Says a file was saved; clicking the toast shows it in Finder.
    pub(in crate::ui) fn announce_saved(&mut self, file: std::path::PathBuf) {
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        let message = format!("Saved {name} to Downloads. Click to show it in Finder.");
        self.shell.notices.push((
            Tone::Success,
            message,
            Some(navigation::NoticeAction::Reveal(file)),
        ));
        crate::host::platform::wake();
    }

    /// The gateway needs this Mac to host computer control and it is not set up: say so
    /// once per launch and lead to the setup card.
    fn notice_computer_setup(&mut self) {
        let ready = self.native.desktop_status().enabled && self.native.desktop_reason().is_none();
        if ready || self.shell.computer_setup_noticed {
            return;
        }
        self.shell.computer_setup_noticed = true;
        self.shell.notices.push((
            Tone::Warning,
            "A Bot tried to use this Mac, but computer control isn’t set up. Click to finish setup."
                .to_owned(),
            Some(navigation::NoticeAction::ComputerSetup),
        ));
        crate::host::platform::wake();
    }

    /// Queues an error toast; `poll` shows it.
    pub(in crate::ui) fn report(&mut self, message: impl AsRef<str>) {
        let message = message.as_ref();
        // Voice failures arrive as the provider's raw text; say them plainly instead.
        if let Some(issue) = crate::ui::transcript::ProviderIssue::classify(message) {
            let explanation = self.explain(issue);
            if !self
                .shell
                .notices
                .iter()
                .any(|(_, queued, _)| *queued == explanation.text)
            {
                let action = explanation.action.map(navigation::NoticeAction::Issue);
                self.shell
                    .notices
                    .push((Tone::Error, explanation.text, action));
                crate::host::platform::wake();
            }
            return;
        }
        self.announce(Tone::Error, crate::gateway::friendly(message));
    }
}
