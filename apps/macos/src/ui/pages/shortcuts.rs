//! Settings › Keyboard shortcuts: every table action with its keystroke, recorded in place.

use super::*;
use crate::ui::navigation::shortcuts::{
    Area, Overrides, SHORTCUTS, Shortcut, assign, binding, conflict, find, is_customized,
    recordable,
};
use crate::ui::navigation::{ActiveShortcuts, apply_shortcuts};
use gpui::OperatingSystem;
use gpui_component::kbd::Kbd;

/// The row being edited, if any.
#[derive(Default)]
pub(in crate::ui) struct ShortcutsUi {
    editing: Option<Editing>,
    focus: Option<FocusHandle>,
    /// Holds every keystroke while recording, so bindings and menus never see it.
    intercept: Option<Subscription>,
}

enum Editing {
    Recording {
        id: &'static str,
        held: Modifiers,
        refused: bool,
    },
    Conflict {
        id: &'static str,
        keystroke: String,
        other: &'static Shortcut,
    },
}

impl Editing {
    fn id(&self) -> &'static str {
        match self {
            Self::Recording { id, .. } | Self::Conflict { id, .. } => id,
        }
    }
}

static NO_OVERRIDES: Overrides = Overrides::new();

fn overrides(cx: &App) -> &Overrides {
    cx.try_global::<ActiveShortcuts>()
        .map_or(&NO_OVERRIDES, |active| &active.0)
}

/// Takes the overrides in use out of the app; `apply_shortcuts` puts the new ones back.
fn take_overrides(cx: &mut App) -> Overrides {
    if cx.has_global::<ActiveShortcuts>() {
        cx.remove_global::<ActiveShortcuts>().0
    } else {
        Overrides::new()
    }
}

fn label(shortcut: &Shortcut) -> &'static str {
    shortcut.label.trim_end_matches('…')
}

/// The keystroke as one rounded key cap.
fn key_cap(keystroke: Keystroke, p: &Palette) -> Kbd {
    Kbd::new(keystroke)
        .h(height::COMPACT)
        .min_w(height::COMPACT)
        .px(space::S)
        .flex()
        .items_center()
        .justify_center()
        .control_corners()
        .border_1()
        .border_color(p.line)
        .bg(p.control)
        .text_color(p.text)
        .typography(Type::BodyMedium)
}

impl Workspace {
    pub(super) fn shortcut_actions(cx: &Context<Self>) -> Vec<AnyElement> {
        vec![
            toolbar_button(
                "shortcuts-reset-all",
                "Reset all to defaults",
                "hi.arrowClockwise",
                cx,
            )
            .disabled(overrides(cx).is_empty())
            .on_click(cx.listener(|this, _, window, cx| this.reset_all_shortcuts(window, cx)))
            .into_any_element(),
        ]
    }

    pub(super) fn shortcut_settings(&self, cx: &Context<Self>) -> Page {
        let overrides = overrides(cx);
        let customized = SHORTCUTS
            .iter()
            .filter(|shortcut| is_customized(overrides, shortcut))
            .count();
        let subtitle = match customized {
            0 => "All defaults".to_owned(),
            1 => "1 customized".to_owned(),
            count => format!("{count} customized"),
        };
        let header = PageHeader::new("Keyboard shortcuts").subtitle(subtitle);
        let mut page = page("settings-shortcuts", header);
        for area in Area::ALL {
            let rows = SHORTCUTS.iter().filter(|shortcut| shortcut.area == area);
            let group = rows.fold(Group::new(), |group, shortcut| {
                group.child(self.shortcut_row(shortcut, overrides, cx))
            });
            page = page.child(Section::new(area.label()).child(group));
        }
        page
    }

    fn shortcut_row(
        &self,
        shortcut: &'static Shortcut,
        overrides: &Overrides,
        cx: &Context<Self>,
    ) -> Row {
        let id = shortcut.id;
        let row = Row::new(
            SharedString::from(format!("shortcut-{id}")),
            label(shortcut),
        );
        let editing = self.config.shortcuts.editing.as_ref();
        match editing.filter(|editing| editing.id() == id) {
            Some(Editing::Recording { held, refused, .. }) => {
                self.recording_row(row, *held, *refused, cx)
            }
            Some(Editing::Conflict {
                keystroke, other, ..
            }) => self.conflict_row(row, keystroke, other, cx),
            None => self.shortcut_idle_row(row, shortcut, overrides, cx),
        }
    }

    fn shortcut_idle_row(
        &self,
        row: Row,
        shortcut: &'static Shortcut,
        overrides: &Overrides,
        cx: &Context<Self>,
    ) -> Row {
        let (id, p) = (shortcut.id, self.palette);
        let current = match binding(overrides, shortcut)
            .and_then(|keystroke| Keystroke::parse_with_os(&keystroke, OperatingSystem::Mac).ok())
        {
            Some(keystroke) => key_cap(keystroke, &p).into_any_element(),
            None => div()
                .typography(Type::Body)
                .text_color(p.muted)
                .child("Not set")
                .into_any_element(),
        };
        let current = div()
            .id(SharedString::from(format!("shortcut-caps-{id}")))
            .cursor_pointer()
            .child(current)
            .on_click(cx.listener(move |this, _, window, cx| this.record_shortcut(id, window, cx)));
        let row = row.control(current);
        let row = if is_customized(overrides, shortcut) {
            row.control(
                settings_button(SharedString::from(format!("shortcut-reset-{id}")), "Reset")
                    .ghost()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.reset_shortcut(id, window, cx)),
                    ),
            )
        } else {
            row
        };
        row.control(
            inline_button(
                SharedString::from(format!("shortcut-edit-{id}")),
                "Edit shortcut",
                "hi.edit02",
                cx,
            )
            .on_click(cx.listener(move |this, _, window, cx| this.record_shortcut(id, window, cx))),
        )
    }

    fn recording_row(&self, row: Row, held: Modifiers, refused: bool, cx: &Context<Self>) -> Row {
        let p = self.palette;
        let held = Modifiers {
            function: false,
            ..held
        };
        let prompt = if held.modified() {
            key_cap(
                Keystroke {
                    modifiers: held,
                    ..Keystroke::default()
                },
                &p,
            )
            .into_any_element()
        } else {
            div()
                .typography(Type::Body)
                .text_color(p.accent)
                .child("Press new shortcut…")
                .into_any_element()
        };
        let mut recorder = div()
            .id("shortcut-recorder")
            .h(height::REGULAR)
            .px(space::S)
            .flex()
            .items_center()
            .control_corners()
            .border_1()
            .border_color(p.accent)
            .bg(p.accent_soft)
            .child(prompt)
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                if let Some(Editing::Recording { held, .. }) = &mut this.config.shortcuts.editing {
                    *held = event.modifiers;
                    cx.notify();
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| this.cancel_shortcut(window, cx)));
        if let Some(focus) = &self.config.shortcuts.focus {
            recorder = recorder.track_focus(focus);
        }
        let hint = if refused {
            "Include ⌘, ⌃ or ⌥ so typing still works."
        } else {
            "Press Escape to cancel."
        };
        row.subtitle(hint).control(recorder)
    }

    fn conflict_row(
        &self,
        row: Row,
        keystroke: &str,
        other: &'static Shortcut,
        cx: &Context<Self>,
    ) -> Row {
        let keystroke =
            Keystroke::parse_with_os(keystroke, OperatingSystem::Mac).unwrap_or_default();
        let shown = Kbd::format(&keystroke);
        row.subtitle(format!("{shown} is used by {}. Replace it?", label(other)))
            .control(key_cap(keystroke, &self.palette))
            .control(
                settings_button("shortcut-cancel", "Cancel")
                    .on_click(cx.listener(|this, _, window, cx| this.cancel_shortcut(window, cx))),
            )
            .control(
                settings_button("shortcut-replace", "Replace")
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| this.replace_shortcut(window, cx))),
            )
    }
}

impl Workspace {
    /// Starts recording `id`: the next keystroke becomes its shortcut.
    pub(in crate::ui) fn record_shortcut(
        &mut self,
        id: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self
            .config
            .shortcuts
            .focus
            .get_or_insert_with(|| cx.focus_handle());
        let focus = FocusHandle::clone(focus);
        focus.focus(window, cx);
        self.config.shortcuts.editing = Some(Editing::Recording {
            id,
            held: window.modifiers(),
            refused: false,
        });
        let view = cx.entity().downgrade();
        self.config.shortcuts.intercept =
            Some(cx.intercept_keystrokes(move |event, window, cx| {
                if view
                    .update(cx, |this, cx| this.recorded(&event.keystroke, window, cx))
                    .is_ok()
                {
                    cx.stop_propagation();
                }
            }));
        cx.notify();
    }

    /// Handles one keystroke while recording.
    pub(in crate::ui) fn recorded(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Editing::Recording { id, .. }) = self.config.shortcuts.editing else {
            return;
        };
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            return self.cancel_shortcut(window, cx);
        }
        let Some(keystroke) = recordable(keystroke) else {
            if let Some(Editing::Recording { refused, .. }) = &mut self.config.shortcuts.editing {
                *refused = true;
            }
            return cx.notify();
        };
        self.propose_shortcut(id, keystroke, window, cx);
    }

    /// Binds `keystroke`, or asks first when another action already has it.
    fn propose_shortcut(
        &mut self,
        id: &'static str,
        keystroke: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match conflict(overrides(cx), id, &keystroke) {
            Some(other) => {
                self.stop_recording(window, cx);
                self.config.shortcuts.editing = Some(Editing::Conflict {
                    id,
                    keystroke,
                    other,
                });
                cx.notify();
            }
            None => self.save_shortcut(id, Some(&keystroke), window, cx),
        }
    }

    fn replace_shortcut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Editing::Conflict { id, keystroke, .. }) = self.config.shortcuts.editing.take()
        {
            self.save_shortcut(id, Some(&keystroke), window, cx);
        }
    }

    fn reset_shortcut(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        match find(id).and_then(Shortcut::default_keystroke) {
            Some(keystroke) => self.propose_shortcut(id, keystroke, window, cx),
            None => self.save_shortcut(id, None, window, cx),
        }
    }

    fn reset_all_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_shortcut(window, cx);
        self.store_shortcuts(Overrides::new(), cx);
    }

    fn save_shortcut(
        &mut self,
        id: &'static str,
        keystroke: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut overrides = take_overrides(cx);
        assign(&mut overrides, id, keystroke);
        self.cancel_shortcut(window, cx);
        self.store_shortcuts(overrides, cx);
    }

    fn store_shortcuts(&mut self, overrides: Overrides, cx: &mut Context<Self>) {
        let saved = overrides.clone(); // rules-allow: the saved preferences and the live keymap each own a copy
        self.save_preferences("shortcuts", |preferences| preferences.shortcuts = saved, cx);
        apply_shortcuts(overrides, cx);
        cx.notify();
    }

    fn cancel_shortcut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.shortcuts.editing.take().is_some() {
            self.stop_recording(window, cx);
            cx.notify();
        }
    }

    fn stop_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The interceptor may be running this; drop it once dispatch finishes.
        let intercept = self.config.shortcuts.intercept.take();
        cx.defer(move |_| drop(intercept));
        if self
            .config
            .shortcuts
            .focus
            .as_ref()
            .is_some_and(|focus| focus.is_focused(window))
        {
            self.focus.focus(window, cx);
        }
    }
}
