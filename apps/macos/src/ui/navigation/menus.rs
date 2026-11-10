//! The menu bar, key bindings and the workspace's action handlers.

use super::shortcuts::{Area, Overrides, SHORTCUTS, binding};
use super::*;
use gpui_component::input;
use mobius::protocol::Op;
use std::rc::Rc;

actions!(
    workspace,
    [
        #[derive(Eq)]
        OpenSettings,
        #[derive(Eq)]
        NewTab,
        #[derive(Eq)]
        NewTerminal,
        #[derive(Eq)]
        CloseTab,
        #[derive(Eq)]
        ToggleSplit,
        #[derive(Eq)]
        ToggleSidePanel,
        #[derive(Eq)]
        StopTurn,
        #[derive(Eq)]
        Minimize,
        #[derive(Eq)]
        Zoom
    ]
);
actions!(
    mobius_desktop,
    [
        #[derive(Eq)]
        Quit,
        #[derive(Eq)]
        Hide,
        #[derive(Eq)]
        HideOthers,
        #[derive(Eq)]
        ShowAll
    ]
);

/// Installs the app menus and shortcuts (SHELL-25), with the user's overrides.
pub fn install_menus(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    let overrides = crate::ui::preferences::Preferences::load()
        .map(|preferences| preferences.shortcuts)
        .unwrap_or_default();
    apply_shortcuts(overrides, cx);
}

/// The overrides the keymap and menus currently use.
pub(in crate::ui) struct ActiveShortcuts(pub Overrides);

impl Global for ActiveShortcuts {}

/// Rebinds every table action with `overrides` applied, keeping gpui-component's own bindings,
/// and refreshes the shortcuts the menu bar shows.
pub(in crate::ui) fn apply_shortcuts(overrides: Overrides, cx: &mut App) {
    let ours: Vec<&str> = SHORTCUTS.iter().map(|s| (s.action)().name()).collect();
    let kept: Vec<KeyBinding> = cx
        .key_bindings()
        .borrow()
        .bindings()
        .filter(|binding| !ours.contains(&binding.action().name()))
        .cloned() // rules-allow: the keymap is cleared and rebuilt, so the kept bindings are copied out first
        .collect();
    let workspace: Option<Rc<KeyBindingContextPredicate>> =
        KeyBindingContextPredicate::parse("Workspace")
            .ok()
            .map(Into::into);
    let bindings = SHORTCUTS.iter().filter_map(|shortcut| {
        let keystroke = binding(&overrides, shortcut)?;
        let context = (!shortcut.app_wide)
            .then(|| workspace.as_ref().map(Rc::clone))
            .flatten();
        let action = (shortcut.action)();
        KeyBinding::load(
            &keystroke,
            action,
            context,
            false,
            None,
            &DummyKeyboardMapper,
        )
        .ok()
    });
    let bindings: Vec<KeyBinding> = bindings.collect();
    cx.clear_key_bindings();
    cx.bind_keys(kept);
    cx.bind_keys(bindings);
    cx.set_global(ActiveShortcuts(overrides));
    cx.set_menus(menus());
}

fn menus() -> Vec<Menu> {
    let mut menus: Vec<Menu> = Area::ALL
        .into_iter()
        .map(|area| {
            let mut items = Vec::new();
            for shortcut in SHORTCUTS.iter().filter(|shortcut| shortcut.area == area) {
                if shortcut.separated {
                    items.push(MenuItem::separator());
                }
                items.push(MenuItem::Action {
                    name: shortcut.label.into(),
                    action: (shortcut.action)(),
                    os_action: None,
                    checked: false,
                    disabled: false,
                });
            }
            Menu::new(area.menu()).items(items)
        })
        .collect();
    menus.insert(2, edit_menu());
    menus
}

fn edit_menu() -> Menu {
    Menu::new("Edit").items([
        MenuItem::os_action("Undo", input::Undo, OsAction::Undo),
        MenuItem::os_action("Redo", input::Redo, OsAction::Redo),
        MenuItem::separator(),
        MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
        MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
        MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
        MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
    ])
}

impl Workspace {
    /// Attaches every workspace action, and mouse back/forward, to the root.
    pub(in crate::ui) fn shell_actions(root: Div, cx: &Context<Self>) -> Div {
        root.on_action(cx.listener(|this, _: &NewChat, window, cx| this.new_chat(window, cx)))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| this.choose_file(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSide, _, cx| this.toggle_navigation(cx)))
            .on_action(cx.listener(|this, _: &Search, window, cx| this.command_palette(window, cx)))
            .on_action(
                cx.listener(|this, _: &Back, window, cx| this.navigate_history(false, window, cx)),
            )
            .on_action(cx.listener(|this, _: &Forward, window, cx| {
                this.navigate_history(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.select_section(Section::Settings, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.open_launcher(cx)))
            .on_action(
                cx.listener(|this, _: &NewTerminal, window, cx| this.open_terminal(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CloseTab, window, cx| this.close_document(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleSplit, window, cx| this.toggle_split(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleSidePanel, window, cx| {
                this.toggle_side_panel(window, cx);
            }))
            .on_action(cx.listener(|this, _: &StopTurn, _, cx| this.stop_turn(cx)))
            .on_action(|_: &Minimize, window, _| window.minimize_window())
            .on_action(|_: &Zoom, window, _| window.zoom_window())
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, window, cx| this.navigate_history(false, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, window, cx| this.navigate_history(true, window, cx)),
            )
    }

    fn close_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.page_tabs.get(self.active_page),
            Some(PageTab::Content(_))
        ) {
            self.close_page(self.active_page, window, cx);
        }
    }

    /// Shows the most recent document beside the page, or returns to the page.
    fn toggle_side_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = if self.active_content().is_some() {
            self.page_tabs
                .iter()
                .position(|tab| !matches!(tab, PageTab::Content(_)))
        } else {
            self.page_tabs
                .iter()
                .rposition(|tab| matches!(tab, PageTab::Content(_)))
        };
        if let Some(index) = target {
            self.select_page(index, window, cx);
        }
    }

    fn stop_turn(&mut self, cx: &mut Context<Self>) {
        if let Some(turn_id) = self
            .client
            .state
            .session
            .as_ref()
            .and_then(|session| session.active_turn_ids.first())
            .map(Arc::clone)
        {
            self.submit_op(Op::Interrupt { turn_id }, cx);
        }
    }
}
