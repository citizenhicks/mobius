//! The chat menu: one builder for the titlebar trigger and the catalog rows' context menu.

use super::*;
use crate::ui::navigation::session_title;
use gpui_component::menu::PopupMenu;
use mobius::SessionId;
use mobius::protocol::{MessageAuthor, ModelStepContentPhase};
use mobius_gateway::wire::TranscriptItemContent;
use mobius_gateway::wire::{BotShape, ClientMessage, GitDiffScope};
use std::borrow::Cow;

/// Where a session menu opens: the open chat's header, or any catalog row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum MenuScope {
    Header,
    Row,
}

#[derive(Clone, Copy)]
enum ChatAction {
    Info,
    Files,
    Diff,
    Folder,
    CopyPath,
    Bot,
    Computer,
    Pin(bool),
    Unread(bool),
    Rename,
    NameWithAi,
    Delete,
}

/// What a menu shows about one session, read before the menu is built.
struct MenuFacts {
    pinned: bool,
    unread: bool,
    owner: Arc<str>,
    path: bool,
    bots: Vec<(Arc<str>, Arc<str>, Hsla, BotShape)>,
    branches: Option<Vec<Choice>>,
    conversation: bool,
    /// Whether any capability contributes to the chat menu; an empty slot would show
    /// as a blank row.
    menu_widgets: bool,
    computer: String,
    computer_enabled: bool,
}

impl Workspace {
    fn menu_facts(&self, session_id: &SessionId, scope: MenuScope) -> MenuFacts {
        let record = self.client.state.session_record(session_id);
        let bots = self
            .client
            .state
            .gateway
            .iter()
            .flat_map(|gateway| &gateway.bots)
            .map(|bot| {
                (
                    Arc::clone(&bot.id),
                    Arc::clone(&bot.name),
                    Accent::from(bot.tint).color(),
                    bot.shape,
                )
            })
            .collect();
        let branches = (scope == MenuScope::Header)
            .then(|| self.client.state.session.as_ref()?.git.as_ref())
            .flatten()
            .map(|git| branch_choices(session_id, &git.current_branch, &git.branches));
        MenuFacts {
            pinned: record.is_some_and(|session| session.pinned),
            unread: self.is_unread(session_id),
            owner: record.map_or_else(|| "".into(), |s| Arc::clone(&s.session_context.owner_id)),
            path: self.chat_path(session_id).is_some(),
            bots,
            branches,
            conversation: self.conversation_bot(session_id).is_some(),
            menu_widgets: !self
                .widget_items(Some(mobius::protocol::FrontendSlot::ChatMenu))
                .is_empty(),
            computer: self.computer_title(),
            computer_enabled: self.computer_available(),
        }
    }

    /// The chat's workspace folder: the open chat's own, else the catalog's label for it.
    fn chat_path(&self, session_id: &SessionId) -> Option<Cow<'_, str>> {
        match &self.client.state.session {
            Some(open) if &open.session.session_id == session_id => open
                .workspace
                .as_ref()
                .map(|workspace| Cow::Owned(workspace.path.display().to_string())),
            _ => self
                .client
                .state
                .session_record(session_id)?
                .session_context
                .workspace_label
                .as_deref()
                .map(Cow::Borrowed),
        }
    }

    pub(super) fn chat_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity().downgrade();
        let draft = self.draft_selected();
        let session_id = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share);
        let trigger = if self.active_conversation_bot().is_some() {
            toolbar_button("chat-options", "Chat options", "hi.dotsThree", cx)
        } else {
            chat_menu_trigger(
                "chat-options",
                if draft {
                    "Choose AI Bot"
                } else {
                    "Chat options"
                },
                self.chat_face(face::ROW, window, cx),
                cx,
            )
        };
        #[cfg(feature = "snapshot")]
        let trigger = trigger.child(crate::ui::snapshot::probe("chat-header-bot".into()));
        trigger
            .disabled(if draft {
                !self.client.link.is_ready() || self.queued_draft.is_some()
            } else {
                !self.active_session_ready()
            })
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                if draft {
                    let Some(workspace) = view.upgrade() else {
                        return menu;
                    };
                    let workspace = workspace.read(cx);
                    let Some(gateway) = &workspace.client.state.gateway else {
                        return menu;
                    };
                    return chat::bot_menu(
                        menu,
                        &gateway.bots,
                        workspace.draft_bot().map(|bot| bot.id.as_ref()),
                        &view,
                        cx,
                    );
                }
                match session_id.as_ref() {
                    Some(session_id) => {
                        session_menu(menu, &view, session_id, MenuScope::Header, window, cx)
                    }
                    None => menu,
                }
            })
            .on_open_change(cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)))
            .into_any_element()
    }

    fn chat_action(
        &mut self,
        session_id: SessionId,
        action: ChatAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ChatAction::Info => {
                self.open_chat_detail(crate::ui::chat::ChatDetail::Context(session_id), window, cx);
            }
            ChatAction::Files => self.open_workspace_files(cx),
            ChatAction::Diff => self.open_git_diff(GitDiffScope::Unstaged, cx),
            ChatAction::Folder => self.attach_chat_folder(session_id, window, cx),
            ChatAction::CopyPath => {
                if let Some(path) = self.chat_path(&session_id) {
                    cx.write_to_clipboard(ClipboardItem::new_string(path.into_owned()));
                }
            }
            ChatAction::Bot => {
                if let Some(session) = &self.client.state.session {
                    let bot = Arc::clone(&session.session.context.owner_id);
                    self.open_chat_detail(crate::ui::chat::ChatDetail::Bot(bot), window, cx);
                }
            }
            ChatAction::Computer => self.open_computer(window, cx),
            ChatAction::Pin(pinned) => {
                self.send(
                    ClientMessage::SetSessionPinned {
                        request_id: crate::gateway::request_id(),
                        session_id,
                        pinned,
                    },
                    cx,
                );
            }
            ChatAction::Unread(unread) => self.set_unread(&session_id, unread),
            ChatAction::Rename => self.rename_chat(session_id, window, cx),
            ChatAction::NameWithAi => self.name_chat_with_ai(&session_id),
            ChatAction::Delete => self.confirm_delete_chat(session_id, window, cx),
        }
        cx.notify();
    }

    fn rename_chat(&self, session_id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.page_title(&PageTab::Chat(session_id.share()));
        self.edit_text(
            TextEdit::new("Rename chat", title).ok_label("Rename"),
            window,
            cx,
            move |this, title, _, cx| {
                this.send(
                    ClientMessage::RenameSession {
                        request_id: crate::gateway::request_id(),
                        session_id: session_id.share(),
                        title,
                    },
                    cx,
                )
            },
        );
    }

    fn attach_chat_folder(
        &self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_text(
            TextEdit::new("Attach a folder on this gateway", "").ok_label("Attach"),
            window,
            cx,
            move |this, folder, _, cx| {
                this.send(
                    ClientMessage::AttachSessionFolder {
                        request_id: crate::gateway::request_id(),
                        session_id: session_id.share(),
                        folder: folder.trim().into(),
                    },
                    cx,
                )
            },
        );
    }

    fn confirm_delete_chat(&self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .client
            .state
            .session_record(&id)
            .map_or_else(|| "this chat".into(), session_title);
        self.confirm_destructive(
            format!("Delete “{title}”?"),
            "This removes the chat from the gateway history.",
            "Delete chat",
            window,
            cx,
            move |this, window, cx| this.delete_chats(std::slice::from_ref(&id), window, cx),
        );
    }
}

/// Builds the chat menu for `session_id`; the header adds workspace and Bot actions.
pub(in crate::ui) fn session_menu(
    mut menu: PopupMenu,
    view: &WeakEntity<Workspace>,
    session_id: &SessionId,
    scope: MenuScope,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(mut facts) = view
        .upgrade()
        .map(|view| view.read(cx).menu_facts(session_id, scope))
    else {
        return menu;
    };
    let p = palette(cx);
    let item = |label: &str, glyph: &str, action: ChatAction| {
        let id = session_id.share();
        let run = move |this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
            this.chat_action(id.share(), action, window, cx);
        };
        workspace_item(label.to_owned(), glyph, view, run, &p)
    };
    if scope == MenuScope::Header {
        menu = menu.item(item("Chat info", "hi.info", ChatAction::Info));
        if facts.path {
            menu = menu.separator().label("Workspace");
            if let Some(choices) = facts.branches.take() {
                let view = WeakEntity::clone(view);
                menu = menu.submenu("Branch", window, cx, move |menu, _, cx| {
                    request_menu(menu, &choices, &view, cx)
                });
            }
            menu = menu
                .item(item("Files", "hi.fileMagnifyingGlass", ChatAction::Files))
                .item(item("Git diff", "hi.plusMinus02", ChatAction::Diff))
                .item(item("Attach folder…", "hi.folderPlus", ChatAction::Folder))
                .item(item("Copy workspace path", "hi.copy", ChatAction::CopyPath));
        }
        menu = menu.separator().label("Actions").item(item(
            "Bot agent settings",
            "hi.slidersHorizontal",
            ChatAction::Bot,
        ));
        menu = menu.item(
            item(&facts.computer, "hi.laptopMinimal", ChatAction::Computer)
                .disabled(!facts.computer_enabled),
        );
    } else if facts.unread {
        menu = menu.item(item(
            "Mark as read",
            "hi.checkCircle",
            ChatAction::Unread(false),
        ));
    } else {
        menu = menu.item(item(
            "Mark as unread",
            "hi.circle",
            ChatAction::Unread(true),
        ));
    }
    if scope == MenuScope::Row && facts.path {
        menu = menu.item(item("Copy workspace path", "hi.copy", ChatAction::CopyPath));
    }
    if !facts.conversation {
        let choices = reassign_choices(session_id, &facts);
        let reassign = WeakEntity::clone(view);
        let bot = Some(icon("bot.face.line", &palette(cx)));
        menu = menu.submenu_with_icon(bot, "Reassign Bot", window, cx, move |menu, _, cx| {
            request_menu(menu, &choices, &reassign, cx)
        });
    }
    if scope == MenuScope::Header && facts.menu_widgets {
        let view = WeakEntity::clone(view);
        menu = menu.item(PopupMenuItem::element(move |_, cx| {
            view.update(cx, |this, cx| {
                div().children(this.slot_widgets(mobius::protocol::FrontendSlot::ChatMenu, cx))
            })
            .unwrap_or_else(|_| div())
        }));
    }
    if facts.conversation {
        return menu;
    }
    if scope == MenuScope::Header {
        menu = menu.separator().label("Manage");
    }
    let (pin_label, pin_glyph) = if facts.pinned {
        ("Unpin chat", "hi.pushPinSlash")
    } else {
        ("Pin chat", "hi.pushPin")
    };
    menu.item(item(pin_label, pin_glyph, ChatAction::Pin(!facts.pinned)))
        .item(item("Rename chat", "hi.pencilSimple", ChatAction::Rename))
        .item(item(
            "Name with Apple Intelligence",
            "hi.aiScan",
            ChatAction::NameWithAi,
        ))
        .separator()
        .item(item("Delete chat…", "hi.trash", ChatAction::Delete))
}

type Choice = (SharedString, Option<(Hsla, BotShape)>, bool, ClientMessage);

fn branch_choices(session_id: &SessionId, current: &str, branches: &[String]) -> Vec<Choice> {
    branches
        .iter()
        .map(|branch| {
            let request = ClientMessage::SwitchGitBranch {
                request_id: crate::gateway::request_id(),
                session_id: session_id.share(),
                branch: branch.clone(), // rules-allow: the request owns the branch name while the workspace keeps its list
            };
            (branch.as_str().into(), None, branch == current, request)
        })
        .collect()
}

fn reassign_choices(session_id: &SessionId, facts: &MenuFacts) -> Vec<Choice> {
    facts
        .bots
        .iter()
        .map(|(id, name, tint, shape)| {
            let request = ClientMessage::ReassignSession {
                request_id: crate::gateway::request_id(),
                session_id: session_id.share(),
                bot_id: Arc::clone(id),
            };
            (
                SharedString::from(name.as_ref()),
                Some((*tint, *shape)),
                *id == facts.owner,
                request,
            )
        })
        .collect()
}

fn request_menu(
    mut menu: PopupMenu,
    choices: &[Choice],
    view: &WeakEntity<Workspace>,
    _cx: &App,
) -> PopupMenu {
    for (label, glyph, selected, request) in choices {
        let view = WeakEntity::clone(view);
        let request = request.clone(); // rules-allow: the menu is rebuilt from the same choices, so each item owns its request
        let item = glyph
            .map_or_else(
                || PopupMenuItem::new(SharedString::clone(label)),
                |(tint, shape)| {
                    BotFace::new(tint, shape, glyph::INLINE).menu_item(SharedString::clone(label))
                },
            )
            .checked(*selected)
            .disabled(*selected);
        menu = menu.item(item.on_click(move |_, _, cx| {
            let sent = view.update(cx, |this, cx| this.send(request.clone(), cx)); // rules-allow: the click handler is Fn and each send owns its request
            if let Err(error) = sent {
                tracing::debug!(%error, "menu request on a released workspace");
            }
        }));
    }
    menu
}

impl Workspace {
    /// Asks the on-device model for a title from the chat's first message and reply. Swift
    /// reports when Apple Intelligence is unavailable.
    fn name_chat_with_ai(&mut self, session_id: &SessionId) {
        let prompt = self
            .client
            .state
            .session_record(session_id)
            .and_then(|session| session.first_user_message.as_deref());
        let Some(prompt) = prompt else {
            self.report("This chat has no message to name it from yet.");
            return;
        };
        let reply = if self.client.state.selected_session_id.as_deref() == Some(session_id) {
            self.first_reply()
        } else {
            String::new()
        };
        if let Err(error) = self
            .native
            .generate_title(session_id, prompt, &reply, false)
        {
            self.report(error.to_string());
        }
    }

    /// Titles the open chat once, when its first turn ends, unless it already has a title.
    /// Without Apple Intelligence the first-message preview stays, silently.
    pub(in crate::ui) fn title_new_chat(&self, session_id: &SessionId) {
        let untitled = self
            .client
            .state
            .sessions
            .iter()
            .any(|session| &session.session_id == session_id && session.title.is_none());
        let mut turns =
            self.client
                .state
                .transcript
                .entries()
                .filter_map(|entry| match &entry.content {
                    TranscriptItemContent::Message { message }
                        if entry.starts_turn && message.author == MessageAuthor::User =>
                    {
                        Some(message.text.as_str())
                    }
                    _ => None,
                });
        let (Some(message), None) = (turns.next(), turns.next()) else {
            return;
        };
        if untitled && !message.trim().is_empty() {
            // A message with a NUL cannot cross to Swift; that chat keeps its preview.
            if let Err(error) =
                self.native
                    .generate_title(session_id, message, &self.first_reply(), true)
            {
                tracing::debug!(%error, "chat title not requested");
            }
        }
    }

    /// The open chat's answer text so far, for titles to reflect what the chat became.
    fn first_reply(&self) -> String {
        let answers: Vec<&str> = self
            .client
            .state
            .transcript
            .entries()
            .filter_map(|entry| match &entry.content {
                TranscriptItemContent::Assistant {
                    phase: ModelStepContentPhase::FinalAnswer,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        answers.join("\n\n")
    }

    /// Warms the title model when the composer of a new or untitled chat is used.
    pub(in crate::ui) fn prewarm_title(&self) {
        let chat = self.client.state.selected_session_id.as_deref();
        let named = chat.is_some_and(|chat| {
            self.client.state.sessions.iter().any(|session| {
                *session.session_id == *chat
                    && (session.title.is_some() || session.first_user_message.is_some())
            })
        });
        if !named {
            self.native.titles_prewarm(chat.unwrap_or_default());
        }
    }
}
