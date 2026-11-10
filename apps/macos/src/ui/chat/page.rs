//! What the chat pane shows before there is a transcript: the draft hero, loading, an empty
//! chat, a chat the gateway would not open, and the connecting and offline states.
//! The composer stays mounted below every one of them.

use super::*;
use gpui_component::menu::DropdownMenu;

#[derive(Clone, Debug, Hash, PartialEq)]
pub(super) enum Placeholder {
    Draft,
    Loading,
    Empty,
    Failed(Arc<str>),
    Connecting(String),
    Offline(Arc<str>),
}

impl Workspace {
    /// The placeholder for the current chat pane, or `None` when a transcript shows.
    pub(super) fn chat_placeholder_kind(&self) -> Option<Placeholder> {
        let ready = self.active_session_ready();
        if ready
            && (!(self.client.state.transcript.is_empty() && self.pending_messages().is_empty())
                || self.history_pending())
        {
            return None;
        }
        match &self.client.link {
            runtime::Link::Connecting(_) | runtime::Link::Reconnecting { .. } => {
                return Some(Placeholder::Connecting(self.link_label()));
            }
            runtime::Link::Failed(message) => {
                return Some(Placeholder::Offline(Arc::clone(message)));
            }
            runtime::Link::Ready => {}
        }
        if self.draft_selected() {
            return Some(Placeholder::Draft);
        }
        if !ready {
            let page = self.primary_page();
            let failed = self.chat_ui.open_failure.as_ref().filter(|_| {
                matches!(page, Some(PageTab::Chat(id)) if self.chat_ui.failed_session.as_ref() == Some(id))
            });
            return Some(failed.map_or(Placeholder::Loading, |message| {
                Placeholder::Failed(Arc::clone(message))
            }));
        }
        Some(Placeholder::Empty)
    }

    /// `chat_placeholder(kind)`: the hero face and, per state, a line and actions.
    pub(super) fn chat_placeholder(
        &self,
        kind: &Placeholder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mood = match kind {
            Placeholder::Loading => Some(BotMood::Thinking),
            Placeholder::Failed(_) | Placeholder::Offline(_) => Some(BotMood::Sleepy),
            _ => None,
        };
        let face = self.chat_face_with(face::HERO, mood, window, cx);
        let body = match kind {
            Placeholder::Draft => Some(self.draft_hero(cx)),
            Placeholder::Loading | Placeholder::Empty => None,
            Placeholder::Failed(reason) => Some(self.failed_hero(reason, cx)),
            Placeholder::Connecting(label) => Some(self.connecting_hero(label)),
            Placeholder::Offline(message) => Some(self.offline_hero(message, cx)),
        };
        div()
            .id("chat-placeholder")
            .flex_1()
            .min_h_0()
            .px(space::L)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::L)
            .when(self.active_conversation_bot().is_none(), |view| {
                view.child(face)
            })
            .children(body)
            .into_any_element()
    }

    /// Codex img1: "What should we work on in `<folder>`?", the folder opening its menu.
    fn draft_hero(&self, cx: &Context<Self>) -> AnyElement {
        let path = SharedString::from(self.draft_workspace.display().to_string());
        let folders = self.recent_folders();
        let view = cx.entity().downgrade();
        let menu_path = SharedString::clone(&path);
        div()
            .flex()
            .flex_wrap()
            .justify_center()
            .items_baseline()
            .gap(space::S)
            .typography(Type::Headline)
            .child("What should we work on in")
            .child(
                Button::new("hero-folder")
                    .ghost()
                    .p_0()
                    .h_auto()
                    .tooltip(SharedString::clone(&path))
                    .accessibility_label(format!("Folder: {path}"))
                    .child(
                        div()
                            .typography(Type::Headline)
                            .underline()
                            .child(format!("{}?", super::controls::folder_name(&path))),
                    )
                    .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, cx| {
                        super::controls::folder_menu(menu, &folders, &menu_path, &view, cx)
                    })
                    .on_open_change(
                        cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)),
                    ),
            )
            .into_any_element()
    }

    fn failed_hero(&self, reason: &str, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        div()
            .max_w(width::NOTE)
            .flex()
            .flex_col()
            .items_center()
            .gap(space::S)
            .child(heading("Couldn't open this chat"))
            .child(
                div()
                    .id("open-failure")
                    .typography(Type::Body)
                    .text_color(p.muted)
                    .text_center()
                    .child(
                        gpui_component::text::TextView::markdown(
                            "open-failure-text",
                            reason.to_owned(),
                        )
                        .selectable(true),
                    ),
            )
            .child(
                button_row()
                    .mt(space::S)
                    .child(
                        button("chat-new", "New chat")
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
                    )
                    .child(button("chat-retry", "Try again").primary().on_click(
                        cx.listener(|this, _, window, cx| this.retry_chat_open(window, cx)),
                    )),
            )
            .into_any_element()
    }

    fn connecting_hero(&self, label: &str) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(space::S)
            .text_color(self.palette.muted)
            .typography(Type::Body)
            .child(Spinner::new(glyph::ICON))
            .child(if self.client.local_gateway {
                "Starting the local gateway…".to_owned()
            } else {
                label.to_owned()
            })
            .into_any_element()
    }

    fn offline_hero(&self, message: &str, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        div()
            .max_w(width::NOTE)
            .flex()
            .flex_col()
            .items_center()
            .gap(space::S)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .text_color(p.danger)
                    .child(icon("hi.warningOctagon", &palette(cx)).size(glyph::LEAD))
                    .child(heading("Not connected").text_color(p.text)),
            )
            .child(
                div()
                    .typography(Type::Body)
                    .text_color(p.muted)
                    .text_center()
                    .child(message.to_owned()),
            )
            .child(
                button_row()
                    .mt(space::S)
                    .child(
                        button("choose-gateway", "Choose gateway")
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                let gateway = this.current_gateway();
                                this.open_page(
                                    PageTab::Gateway(GatewayPage::Machine(gateway)),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        button("reconnect", "Try again")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let connection = match this.current_gateway() {
                                    GatewayRef::Local => crate::gateway::Connection::Local,
                                    GatewayRef::Saved(endpoint) => {
                                        crate::gateway::Connection::Saved(endpoint)
                                    }
                                };
                                this.connect(connection, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// Asks the gateway to open the failed chat again.
    fn retry_chat_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.chat_ui.open_failure = None;
        self.chat_ui.failed_session = None;
        if let Some(index) = self
            .page_tabs
            .iter()
            .position(|page| matches!(page, PageTab::Chat(_)))
        {
            self.select_page(index, window, cx);
        }
    }
}
