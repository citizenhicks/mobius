//! Contributed actions: editors, confirmations and the operations they send.

use super::*;
use mobius::SessionId;
use std::rc::Rc;

impl Workspace {
    pub(in crate::ui) fn widget_action(
        &self,
        action: &FrontendAction,
        global: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let glyph = if action.input_from_label {
            Some("hi.circle")
        } else {
            known_symbol_icon(Some(&action.symbol))
        };
        let shared = Rc::new(action.clone()); // rules-allow: the 'static listener owns its copy of the action from gateway state
        button(SharedString::from(&action.id), &action.label)
            .when_some(glyph, |button, glyph| {
                button.icon(icon(glyph, &palette(cx)))
            })
            .text_color(self.palette.tone(action.tone))
            .disabled(self.widget_action_blocked(action))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_widget_action(Rc::clone(&shared), global, window, cx);
            }))
            .into_any_element()
    }

    /// Offline, or this action's operation is already on its way.
    pub(in crate::ui) fn widget_action_blocked(&self, action: &FrontendAction) -> bool {
        !self.client.link.is_ready()
            || operation_key(&action.op)
                .is_some_and(|key| self.chat_ui.action_requests.contains_key(&key))
    }

    /// A contributed list action for a header: icon-only, its label as the tooltip, and
    /// the shared monochrome glyph.
    pub(in crate::ui) fn widget_header_action(
        &self,
        action: &FrontendAction,
        global: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let glyph = symbol_icon_or(Some(&action.symbol), "hi.sparkle");
        let id = SharedString::from(&action.id);
        let button = toolbar_button(id, &action.label, glyph, cx);
        let shared = Rc::new(action.clone()); // rules-allow: the 'static listener owns its copy of the action from gateway state
        button
            .disabled(self.widget_action_blocked(action))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_widget_action(Rc::clone(&shared), global, window, cx);
            }))
            .into_any_element()
    }

    pub(in crate::ui) fn activate_widget_action(
        &mut self,
        action: impl Into<Rc<FrontendAction>>,
        global: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let action: Rc<FrontendAction> = action.into();
        let session = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share);
        if action.editor.is_some() {
            let value = match &action.op {
                Op::CapabilityCommand {
                    input: Some(input), ..
                } => input.as_str(),
                _ => "",
            };
            let edit = match &action.editor {
                Some(editor) => TextEdit::new(&editor.title, value)
                    .caption(&editor.description)
                    .ok_label(&editor.submit_label),
                None => TextEdit::new(&action.label, value),
            }
            .kind(EditKind::Multiline { min: 3, max: 10 });
            self.edit_text(edit, window, cx, move |this, text, _, cx| {
                if !this.widget_session_current(global, session.as_deref(), cx) {
                    return;
                }
                let mut op = action.op.clone(); // rules-allow: each submission sends its own operation, the action stays shared
                if let Op::CapabilityCommand { input, .. } = &mut op {
                    *input = Some(text);
                }
                this.dispatch_widget_operation(op, global, cx);
            });
        } else if action.tone == FrontendTone::Error {
            let label = SharedString::from(&action.label);
            self.confirm_destructive(
                SharedString::clone(&label),
                "Confirm this action.",
                label,
                window,
                cx,
                move |this, _, cx| {
                    if !this.widget_session_current(global, session.as_deref(), cx) {
                        return;
                    }
                    let op = action.op.clone(); // rules-allow: each confirmation sends its own operation, the action stays shared
                    this.dispatch_widget_operation(op, global, cx);
                },
            );
        } else {
            let op = Rc::unwrap_or_clone(action).into_operation();
            self.dispatch_widget_operation(op, global, cx);
        }
    }

    /// Whether a session widget's chat is still the selected one; says so when it is not.
    fn widget_session_current(
        &mut self,
        global: bool,
        session: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        if global || self.client.state.selected_session_id.as_deref() == session {
            return true;
        }
        self.report("The chat changed. Open this action again in its original chat.");
        cx.notify();
        false
    }

    /// Sends a widget operation to the gateway or the selected chat; returns the id that
    /// answers to it, or `None` when nothing was sent.
    pub(in crate::ui) fn dispatch_widget_operation(
        &mut self,
        op: Op,
        global: bool,
        cx: &mut Context<Self>,
    ) -> Option<Arc<str>> {
        let key = operation_key(&op);
        if key
            .as_ref()
            .is_some_and(|key| self.chat_ui.action_requests.contains_key(key))
        {
            return None;
        }
        let (id, message) = if global {
            let id = crate::gateway::request_id();
            let message = ClientMessage::SubmitContribution {
                request_id: Arc::clone(&id),
                operation: op,
            };
            (id, message)
        } else {
            let session = self
                .client
                .state
                .selected_session_id
                .as_ref()
                .map(SessionId::share)?;
            if !self.client.link.is_ready() || !self.active_session_ready() {
                return None;
            }
            let message = crate::gateway::submit(session, op);
            let ClientMessage::Submit { submission, .. } = &message else {
                return None;
            };
            (Arc::clone(&submission.id), message)
        };
        if let Err(error) = self.enqueue(message) {
            self.report(error);
            cx.notify();
            return None;
        }
        if let Some(key) = key {
            self.chat_ui.action_requests.insert(key, Arc::clone(&id));
        }
        cx.notify();
        Some(id)
    }
}

fn operation_key(op: &Op) -> Option<String> {
    match op {
        Op::CapabilityCommand {
            capability,
            arguments,
            ..
        } => Some(format!("{capability}\0{arguments}")),
        Op::ExecApproval { id, .. } => Some(format!("approval\0{id}")),
        _ => None,
    }
}
