//! Confirmation and text-entry dialogs guarded against gateway changes.

use super::controls::palette;
use super::tokens::{space, width};
use super::{Type, Typography, button, button_row, heading, text_field, textarea};
use crate::ui::Workspace;
use gpui::prelude::*;
use gpui::{AnyElement, App, AppContext, Div, Entity, Hsla, SharedString, WeakEntity, Window, div};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::Dialog;
use gpui_component::input::{InputState, TextareaState};
use gpui_component::{Disableable, WindowExt};
use std::cell::Cell;
use std::rc::Rc;

/// Runs a dialog's action; returns whether the dialog may close.
type Submit = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

pub enum EditKind {
    Line,
    Multiline { min: usize, max: usize },
}

/// What `Workspace::edit_text` asks for.
pub struct TextEdit {
    title: SharedString,
    value: String,
    kind: EditKind,
    caption: Option<SharedString>,
    ok_label: SharedString,
}

impl TextEdit {
    pub fn new(title: impl Into<SharedString>, value: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            value: value.into(),
            kind: EditKind::Line,
            caption: None,
            ok_label: "Save".into(),
        }
    }

    pub fn kind(mut self, kind: EditKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn caption(mut self, caption: impl Into<SharedString>) -> Self {
        self.caption = Some(caption.into());
        self
    }

    pub fn ok_label(mut self, label: impl Into<SharedString>) -> Self {
        self.ok_label = label.into();
        self
    }
}

#[derive(Clone)]
enum EditInput {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
}

impl EditInput {
    fn new(kind: &EditKind, value: String, window: &mut Window, cx: &mut App) -> Self {
        match *kind {
            EditKind::Line => {
                let input = cx.new(|cx| InputState::new(window, cx));
                input.update(cx, |input, cx| input.set_value(value, window, cx));
                Self::Line(input)
            }
            EditKind::Multiline { min, max } => {
                let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(min, max));
                input.update(cx, |input, cx| input.set_value(value, window, cx));
                Self::Area(input)
            }
        }
    }

    fn value(&self, cx: &App) -> String {
        match self {
            Self::Line(input) => input.read(cx).value().to_string(),
            Self::Area(input) => input.read(cx).value().to_string(),
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Line(input) => input.update(cx, |input, cx| input.focus(window, cx)),
            Self::Area(input) => input.update(cx, |input, cx| input.focus(window, cx)),
        }
    }

    fn element(&self, label: SharedString) -> AnyElement {
        match self {
            Self::Line(input) => text_field(input).aria_label(label).into_any_element(),
            Self::Area(input) => textarea(input).aria_label(label).into_any_element(),
        }
    }
}

impl Workspace {
    /// Runs `submit` only if the gateway this dialog was opened for is still connected.
    pub(in crate::ui) fn submit_gateway_dialog(
        &mut self,
        epoch: u64,
        cx: &mut Context<Self>,
        submit: impl FnOnce(&mut Self, &mut Context<Self>),
    ) -> bool {
        if epoch != self.client.epoch || !self.client.link.is_ready() {
            self.report("The gateway changed or disconnected. Close this dialog and try again.");
            cx.notify();
            return false;
        }
        submit(self, cx);
        true
    }

    /// Asks before an irreversible action; the confirm button is red.
    pub(in crate::ui) fn confirm_destructive(
        &self,
        title: impl Into<SharedString>,
        body: impl Into<SharedString>,
        action: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_confirm: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let (title, body, action) = (title.into(), body.into(), action.into());
        let epoch = self.client.epoch;
        let view = cx.entity().downgrade();
        let on_confirm = Rc::new(on_confirm);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, on_confirm) = (WeakEntity::clone(&view), Rc::clone(&on_confirm));
            let submit: Submit = Rc::new(move |window, cx| {
                view.update(cx, |this, cx| {
                    this.submit_gateway_dialog(epoch, cx, |this, cx| on_confirm(this, window, cx))
                })
                .unwrap_or(false)
            });
            let ok = Rc::clone(&submit);
            alert
                .title(heading(SharedString::clone(&title)))
                .description(SharedString::clone(&body))
                .width(width::DIALOG)
                .footer(actions(
                    button("dialog-ok", SharedString::clone(&action)).danger(),
                    true,
                    submit,
                ))
                .on_ok(move |_, window, cx| ok(window, cx))
        });
    }

    /// Asks for one value; an empty value is refused inline rather than silently.
    pub(in crate::ui) fn edit_text(
        &self,
        edit: TextEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
        submit: impl Fn(&mut Self, String, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let input = EditInput::new(&edit.kind, edit.value, window, cx);
        let view = cx.entity().downgrade();
        let epoch = self.client.epoch;
        let submit = Rc::new(submit);
        let empty = Rc::new(Cell::new(false));
        let danger = palette(cx).danger;
        let (title, caption, ok_label) = (edit.title, edit.caption, edit.ok_label);
        let focus = EditInput::clone(&input);
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = palette(cx).muted;
            let (field, view, submit, check) = (
                EditInput::clone(&input),
                WeakEntity::clone(&view),
                Rc::clone(&submit),
                Rc::clone(&empty),
            );
            let submit: Submit = Rc::new(move |window, cx| {
                let value = field.value(cx);
                if value.trim().is_empty() {
                    check.set(true);
                    window.refresh();
                    return false;
                }
                view.update(cx, |this, cx| {
                    this.submit_gateway_dialog(epoch, cx, |this, cx| {
                        submit(this, value, window, cx);
                    })
                })
                .unwrap_or(false)
            });
            let ok = Rc::clone(&submit);
            dialog
                .title(heading(SharedString::clone(&title)))
                .w(width::DIALOG)
                .child(input.element(SharedString::clone(&title)))
                .children(caption.as_ref().map(|caption| {
                    div()
                        .typography(Type::Caption)
                        .text_color(muted)
                        .child(SharedString::clone(caption))
                }))
                .when(empty.get(), |dialog| {
                    dialog.child(
                        div()
                            .typography(Type::Caption)
                            .text_color(danger)
                            .child("Enter a value to continue."),
                    )
                })
                .footer(actions(
                    button("dialog-ok", SharedString::clone(&ok_label)).primary(),
                    true,
                    submit,
                ))
                .on_ok(move |_, window, cx| ok(window, cx))
        });
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }
}

/// Pointer and Return share the same validation and submit operation.
pub fn dialog_actions(
    dialog: Dialog,
    ok: impl Into<SharedString>,
    enabled: bool,
    submit: impl Fn(&mut Window, &mut App) -> bool + 'static,
) -> Dialog {
    let submit: Submit = Rc::new(move |window, cx| enabled && submit(window, cx));
    let confirm = Rc::clone(&submit);
    dialog
        .footer(actions(button("dialog-ok", ok).primary(), enabled, submit))
        .on_ok(move |_, window, cx| confirm(window, cx))
}

fn actions(ok: Button, enabled: bool, submit: Submit) -> Div {
    button_row()
        .child(button("dialog-cancel", "Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
        .child(ok.disabled(!enabled).on_click(move |_, window, cx| {
            if submit(window, cx) {
                window.close_dialog(cx);
            }
        }))
}

/// The column every Bots dialog stacks its content in.
pub fn dialog_body() -> Div {
    div().flex().flex_col().gap(space::L)
}

/// A caption-labelled control stacked in a dialog.
pub fn labelled(label: &'static str, control: impl IntoElement, muted: Hsla) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .child(
            div()
                .typography(Type::Caption)
                .text_color(muted)
                .child(label),
        )
        .child(control)
}
