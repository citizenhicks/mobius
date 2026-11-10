use super::{Context, DraftIntent, PageTab, Workspace};
use mobius::SessionId;
use mobius_gateway::wire::VoiceCapability;

pub(super) fn voice_session_changed(selected: Option<&SessionId>, page: &PageTab) -> bool {
    match page {
        PageTab::Chat(id) => selected != Some(id),
        PageTab::Draft => true,
        _ => false,
    }
}

impl Workspace {
    pub(in crate::ui) fn stop_voice(&mut self) {
        self.native.end_voice();
        self.voice_active = false;
        self.voice_connected = false;
        self.voice_muted = false;
    }

    /// Whether and how the composer's Bot takes voice calls, as its gateway resolved it.
    fn voice_capability(&self) -> Option<&VoiceCapability> {
        self.composer_bot().map(|bot| &bot.capabilities.voice)
    }

    pub(in crate::ui) fn voice_unavailable_reason(&self) -> Option<&str> {
        if !self.client.link.is_ready() {
            return Some("Connect to a gateway to use voice.");
        }
        if self.queued_draft.is_some() || (!self.draft_selected() && !self.active_session_ready()) {
            return Some("Wait for the chat to open before starting voice.");
        }
        if self.client.state.gateway.is_none() {
            return Some("Wait for the gateway’s model information.");
        }
        match self.voice_capability() {
            Some(VoiceCapability::Ready { .. }) => None,
            Some(VoiceCapability::Unavailable { reason }) => Some(reason),
            Some(VoiceCapability::Off) | None => {
                Some("Turn on Voice for this Bot and connect a provider with voice in Settings.")
            }
        }
    }

    pub(in crate::ui) fn toggle_voice(&mut self, cx: &mut Context<Self>) {
        if self.voice_active {
            self.stop_voice();
        } else if let Some(reason) = self.voice_unavailable_reason() {
            let reason = reason.to_owned();
            self.report(reason);
        } else if self.draft_selected() {
            self.stop_dictation();
            self.create_draft(DraftIntent::Voice, cx);
        } else if let Some(session_id) = &self.client.state.selected_session_id {
            let wait_for_session_started = matches!(
                self.voice_capability(),
                Some(VoiceCapability::Ready {
                    waits_for_session_started: true,
                    ..
                })
            );
            match self
                .native
                .start_voice(session_id, wait_for_session_started)
            {
                Ok(()) => {
                    self.dictating = false;
                    self.voice_active = true;
                }
                Err(error) => self.report(error.to_string()),
            }
        }
        cx.notify();
    }
}
