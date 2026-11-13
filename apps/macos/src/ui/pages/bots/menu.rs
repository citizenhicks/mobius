//! Row and header "More" menus, and the Bot actions they offer.

use super::*;
use gpui_component::menu::{DropdownMenu, PopupMenu};
use std::rc::Rc;

type Run = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// One labelled item of a row or header "More" menu.
pub(in crate::ui) struct MenuAction {
    label: SharedString,
    glyph: &'static str,
    disabled: bool,
    run: Run,
}

impl MenuAction {
    pub(super) fn new(
        label: impl Into<SharedString>,
        glyph: &'static str,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            glyph,
            disabled: false,
            run: Rc::new(run),
        }
    }

    pub(super) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

pub(super) fn fill_menu(
    mut menu: PopupMenu,
    view: &WeakEntity<Workspace>,
    actions: &[MenuAction],
    cx: &App,
) -> PopupMenu {
    for action in actions {
        let run = Rc::clone(&action.run);
        let label = SharedString::clone(&action.label);
        menu = menu.item(
            workspace_item(
                label,
                action.glyph,
                view,
                move |this, window, cx| run(this, window, cx),
                &palette(cx),
            )
            .disabled(action.disabled),
        );
    }
    menu
}

/// A "More" button that opens `actions` as a menu under its trailing edge.
pub(super) fn more_menu(
    button: Button,
    actions: Vec<MenuAction>,
    cx: &Context<Workspace>,
) -> AnyElement {
    let view = cx.entity().downgrade();
    button
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, cx| {
            fill_menu(menu, &view, &actions, cx)
        })
        .on_open_change(cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)))
        .into_any_element()
}

impl Workspace {
    /// Rename… and, except for @mobius, Delete Bot….
    pub(super) fn bot_manage_actions(&self, bot: &BotRecord) -> Vec<MenuAction> {
        let ready = self.client.link.is_ready();
        let (rename, delete) = (Arc::clone(&bot.id), Arc::clone(&bot.id));
        let mut actions = vec![
            MenuAction::new("Rename…", "hi.pencilSimple", move |this, window, cx| {
                this.rename_bot(&rename, window, cx);
            })
            .disabled(!ready),
        ];
        if &*bot.handle != "mobius" {
            actions.push(
                MenuAction::new("Delete Bot…", "hi.trash", move |this, window, cx| {
                    this.delete_bot(&delete, window, cx);
                })
                .disabled(!ready),
            );
        }
        actions
    }

    pub(super) fn rename_bot(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.client.state.bot(id) else {
            return;
        };
        let (id, revision, description, tint, shape) = (
            Arc::clone(&bot.id),
            bot.config.revision,
            Arc::clone(&bot.description),
            bot.tint,
            bot.shape,
        );
        let config = bot.config.config.clone(); // rules-allow: the rename sends the configuration as it was when the dialog opened
        self.edit_text(
            TextEdit::new("Rename Bot", &*bot.name).ok_label("Rename"),
            window,
            cx,
            move |this, name, _, cx| {
                let request_id = crate::gateway::request_id();
                let message = ClientMessage::UpdateBot {
                    request_id,
                    id: Arc::clone(&id),
                    expected_revision: revision,
                    name: name.trim().into(),
                    description: Arc::clone(&description),
                    tint,
                    shape,
                    config: config.clone(), // rules-allow: each submit sends its own copy of the snapshot
                };
                this.send(message, cx)
            },
        );
    }

    pub(in crate::ui) fn delete_bot(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.client.state.bot(id) else {
            return;
        };
        let (id, revision) = (Arc::clone(&bot.id), bot.config.revision);
        self.confirm_destructive(
            "Delete this Bot and all its data?",
            format!(
                "This permanently deletes every conversation, routine, and run history owned by @{}. Active work must finish first.",
                bot.handle
            ),
            "Delete Bot and All Data",
            window,
            cx,
            move |this, window, cx| {
                this.send(
                    ClientMessage::DeleteBot {
                        request_id: crate::gateway::request_id(),
                        id: Arc::clone(&id),
                        expected_revision: revision,
                    },
                    cx,
                );
                this.open_bots_landing(window, cx);
            },
        );
    }
}
