//! A capability opening a chat it just made, such as a fork from a message: the gateway
//! asks with `FrontendEvent::OpenSession` and the chat opens.

use super::{Context, EventMsg, PageTab, ServerMessage, Window, Workspace};
use mobius::protocol::FrontendEvent;

impl Workspace {
    pub(super) fn fork_on_message(
        &mut self,
        message: &ServerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ServerMessage::AgentEvent {
            session_id, record, ..
        } = message
        else {
            return;
        };
        if let EventMsg::Frontend(FrontendEvent::OpenSession { session_id: opened }) =
            &record.event.msg
            && !self.client.state.replaying
            && self.client.state.selected_session_id.as_ref() == Some(session_id)
        {
            self.open_page(PageTab::Chat(opened.share()), window, cx);
        }
    }
}
