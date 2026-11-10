use super::*;
use crate::host::platform::NativeEvent;
use mobius_gateway::wire::ClientFrame;

#[derive(serde::Serialize)]
struct NativeFrame<'a> {
    version: u16,
    #[serde(flatten)]
    message: &'a ServerMessage,
}

impl<'a> NativeFrame<'a> {
    fn for_message(message: &'a ServerMessage) -> Option<Self> {
        matches!(
            message,
            ServerMessage::DesktopControlRequested { .. }
                | ServerMessage::DesktopControlEnded { .. }
                | ServerMessage::Accepted { .. }
                | ServerMessage::Rejected { .. }
                | ServerMessage::RealtimeVoiceStarted { .. }
                | ServerMessage::RealtimeVoiceFailed { .. }
                | ServerMessage::RealtimeVoiceEnded { .. }
        )
        .then_some(Self {
            version: mobius_gateway::wire::PROTOCOL_VERSION,
            message,
        })
    }
}

/// Owns only the current recording's edits, so speech cannot overwrite a typed correction.
#[derive(Default)]
pub(in crate::ui) struct DictationDraft {
    original: SharedString,
    applied: SharedString,
}

impl DictationDraft {
    fn new(original: impl Into<SharedString>) -> Self {
        let original = original.into();
        Self {
            applied: SharedString::clone(&original),
            original,
        }
    }

    fn update(&mut self, spoken: &str, current: &str) -> Option<SharedString> {
        if current != &*self.applied {
            return None;
        }
        let separator = if self.original.is_empty()
            || self.original.ends_with(char::is_whitespace)
            || spoken.is_empty()
        {
            ""
        } else {
            " "
        };
        self.applied = format!("{}{separator}{spoken}", self.original).into();
        Some(SharedString::clone(&self.applied))
    }
}

impl Workspace {
    pub(super) fn forward_native(&mut self, message: &ServerMessage) {
        if matches!(message, ServerMessage::DesktopControlRequested { .. }) {
            self.native.set_desktop_accent(self.palette.accent.into());
        }
        if let Some(frame) = NativeFrame::for_message(message)
            && let Err(error) = self.native.receive_gateway(&frame)
        {
            self.report(format!("{error:#}"));
        }
    }

    pub(in crate::ui) fn stop_dictation_if_edited(&mut self, cx: &Context<Self>) {
        if self.dictating
            && &*self.dictation_draft.applied != self.composer.read(cx).value().as_str()
        {
            self.stop_dictation();
        }
    }

    pub(super) fn native_event(
        &mut self,
        event: NativeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            NativeEvent::GatewayRequest { request } => {
                match serde_json::from_value::<ClientFrame>(request) {
                    Ok(frame) => {
                        let voice =
                            matches!(frame.message, ClientMessage::StartRealtimeVoice { .. });
                        if let Err(error) = self.enqueue(frame.message) {
                            if voice {
                                self.stop_voice();
                            }
                            self.report(error);
                        }
                    }
                    Err(error) => self.report(format!("Native gateway request: {error}")),
                }
            }
            NativeEvent::Dictation { text, is_final } if self.dictating => {
                let current = self.composer.read(cx).value();
                let Some(text) = self.dictation_draft.update(&text, current.as_str()) else {
                    self.stop_dictation();
                    cx.notify();
                    return;
                };
                self.composer
                    .update(cx, |input, cx| input.set_value(text, window, cx));
                if is_final {
                    self.dictating = false;
                }
            }
            NativeEvent::DictationState { active: false } => self.dictating = false,
            NativeEvent::VoiceState {
                active,
                connected,
                muted,
            } => {
                self.voice_active = active;
                self.voice_connected = connected;
                self.voice_muted = muted;
            }
            NativeEvent::Title {
                request_id,
                title,
                automatic,
            } if self.client.link.is_ready() && self.conversation_bot(&request_id).is_none() => {
                if let Some(session) = self.client.state.sessions.iter().find(|session| {
                    session.session_id.as_str() == request_id
                        && (!automatic || session.title.is_none())
                }) {
                    self.send(
                        ClientMessage::RenameSession {
                            request_id: gateway::request_id(),
                            session_id: session.session_id.share(),
                            title,
                        },
                        cx,
                    );
                }
            }
            NativeEvent::Recap { request_id, recap } => self.recap_written(&request_id, recap),
            NativeEvent::NotificationOpened { session_id } => {
                if let Some(session) = self
                    .client
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.session_id.as_str() == session_id)
                {
                    self.open_page(PageTab::Chat(session.session_id.share()), window, cx);
                }
            }
            NativeEvent::Dictation { .. }
            | NativeEvent::DictationState { active: true }
            | NativeEvent::StatusChanged
            | NativeEvent::Title { .. } => {}
            NativeEvent::StatusNewChat => self.new_chat(window, cx),
            NativeEvent::TerminalExited { tab_id, status } => self.terminal_exited(&tab_id, status),
            NativeEvent::CloudStatus { status } => self.set_cloud_status(status),
            NativeEvent::CloudPairing { endpoint, code } => {
                self.pair_cloud_gateway(&endpoint, code.0, cx);
            }
            NativeEvent::CloudForget { endpoint } => self.forget_cloud_gateway(&endpoint, cx),
            NativeEvent::Error { message } => self.report(message),
        }
        cx.notify();
    }

    pub(in crate::ui) fn stop_dictation(&mut self) {
        if self.dictating {
            self.native.dictate(false);
            self.dictating = false;
        }
    }

    /// Ends dictation and restores the text from before it started.
    pub(in crate::ui) fn cancel_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_dictation();
        let original = std::mem::take(&mut self.dictation_draft).original;
        self.composer
            .update(cx, |input, cx| input.set_value(original, window, cx));
        cx.notify();
    }

    pub(in crate::ui) fn toggle_desktop(&self, cx: &mut Context<Self>) {
        self.native
            .set_desktop_control(!self.native.desktop_status().enabled);
        cx.notify();
    }

    pub(in crate::ui) fn toggle_dictation(&mut self, cx: &mut Context<Self>) {
        if !self.dictating && !self.draft_selected() && !self.active_session_ready() {
            return;
        }
        if self.dictating {
            // Text keeps landing until the inactive state arrives with the last words.
            self.native.finish_dictation();
            return;
        }
        self.dictation_draft = DictationDraft::new(self.composer.read(cx).value());
        self.native.dictate(true);
        self.dictating = true;
        // The waveform rolls like tape while recording, at 30 frames a second.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(crate::ui::primitives::motion::FRAME)
                    .await;
                let recording = this.update(cx, |this, cx| {
                    cx.notify();
                    this.dictating
                });
                if !recording.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// The recording's loudness history for the dictation waveform, and how far the tape
    /// has rolled toward the next sample, 0 to 1, so it moves between samples.
    pub(in crate::ui) fn dictation_tape(&self) -> (Vec<f32>, f32) {
        let (levels, recorded) = self.native.dictation_levels();
        #[cfg(feature = "snapshot")]
        if levels.is_empty() {
            let levels = (0..128_u8)
                .map(|index| {
                    ((f32::from(index) * 0.37).sin().abs() * 0.8) * (f32::from(index) / 128.)
                })
                .collect();
            return (levels, 0.5);
        }
        let now = std::time::Instant::now();
        let since = match self.dictation_tape.get() {
            Some((seen, at)) if seen == recorded => at,
            _ => {
                self.dictation_tape.set(Some((recorded, now)));
                now
            }
        };
        let rolled = (since.elapsed().as_secs_f32() / 0.1).min(1.);
        (levels, if recorded == 0 { 0. } else { rolled })
    }
}

#[cfg(test)]
mod tests {
    use super::NativeFrame;
    use mobius_gateway::wire::{ServerFrame, ServerMessage};

    #[test]
    fn native_frames_preserve_the_wire_and_only_forward_native_messages() {
        for json in [
            r#"{"type":"accepted","request_id":"request"}"#,
            r#"{"type":"rejected","request_id":"request","code":"denied","message":"No","fatal":false}"#,
            r#"{"type":"desktop_control_requested","request_id":"request","execution_id":"execution","session_id":"session","request":{"action":"apps"}}"#,
            r#"{"type":"desktop_control_ended","execution_id":"execution"}"#,
            r#"{"type":"realtime_voice_started","request_id":"request","session_id":"session","voice_id":"voice","answer_sdp":"sdp"}"#,
            r#"{"type":"realtime_voice_failed","request_id":"request","session_id":"session","message":"No"}"#,
            r#"{"type":"realtime_voice_ended","session_id":"session","voice_id":"voice","reason":null}"#,
        ] {
            let message: ServerMessage = serde_json::from_str(json).unwrap();
            let borrowed =
                serde_json::to_value(NativeFrame::for_message(&message).unwrap()).unwrap();
            assert_eq!(
                borrowed,
                serde_json::to_value(ServerFrame::new(message)).unwrap()
            );
        }
        assert!(NativeFrame::for_message(&ServerMessage::Authenticated).is_none());
    }
}
