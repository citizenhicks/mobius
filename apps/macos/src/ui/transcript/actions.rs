//! A message's actions (TranscriptView.swift TranscriptRow): under an answer Copy, the
//! gateway's message actions, Reply, Pin and Speak, then the Bot that wrote it; the same actions
//! with the time it was sent and its author in the message's context menu.

use super::*;
use gpui_component::Disableable;
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use mobius::protocol::{FrontendSlot, MessageTarget, Op};
use mobius_gateway::wire::ClientMessage;
use std::rc::Rc;

enum Action {
    Copy {
        key: SharedString,
        text: SharedString,
    },
    Widget(Box<Op>),
    Reply {
        target: MessageTarget,
        text: SharedString,
        author: String,
    },
    Pin {
        target: MessageTarget,
        pinned: bool,
    },
    Speak(SharedString),
}

/// What the actions of one message act on.
pub(super) struct Message {
    pub(super) key: SharedString,
    pub(super) text: SharedString,
    pub(super) target: Option<MessageTarget>,
    /// Incoming provenance; generated assistant prose has no input author.
    pub(super) author: Option<MessageAuthor>,
    pub(super) recorded_at_ms: i64,
    /// The Bot that wrote an answer: its name, tint and shape.
    pub(super) bot: Option<(Arc<str>, Hsla, BotShape)>,
    pub(super) preview_author: Option<Arc<str>>,
}

impl Message {
    fn is_user(&self) -> bool {
        self.author.as_ref() == Some(&MessageAuthor::User)
    }

    pub(super) fn author_name(&self) -> String {
        if self.is_user() {
            return "you".to_owned();
        }
        self.bot.as_ref().map_or_else(
            || match &self.author {
                Some(MessageAuthor::Source { source, handle, .. }) => {
                    let handle = handle
                        .rsplit_once(" #")
                        .filter(|(_, id)| !id.is_empty() && source.id().starts_with(id))
                        .map_or_else(|| handle.as_ref(), |(name, _)| name);
                    if handle == "chat" || handle == source.id() {
                        "Bot".to_owned()
                    } else {
                        format!("@{handle}")
                    }
                }
                _ => self
                    .preview_author
                    .as_deref()
                    .unwrap_or("the Bot")
                    .to_owned(),
            },
            |(name, _, _)| name.to_string(),
        )
    }

    fn glyph(&self, p: &Palette, size: Pixels) -> AnyElement {
        self.bot.as_ref().map_or_else(
            || icon("bot.face", p).size(size).into_any_element(),
            |(_, tint, shape)| BotFace::new(*tint, *shape, size).into_any_element(),
        )
    }
}

/// "HH:MM · DD/MM/YY" in local time (MessageTimestamp).
fn timestamp(milliseconds: i64) -> Option<String> {
    use chrono::TimeZone;
    (milliseconds > 0)
        .then(|| chrono::Local.timestamp_millis_opt(milliseconds).single())
        .flatten()
        .map(|time| time.format("%H:%M · %d/%m/%y").to_string())
}

impl Workspace {
    /// The actions of `message`, in SwiftUI's order: label, glyph and whether it can run.
    fn actions(
        &self,
        message: &Message,
        chat: bool,
    ) -> Vec<(Action, SharedString, &'static str, bool)> {
        let key = &message.key;
        let copy = if self.chat_copied(key) {
            ("Copied", "hi.check")
        } else {
            ("Copy", "hi.copy")
        };
        let mut actions = Vec::new();
        if !message.text.is_empty() {
            actions.push((
                Action::Copy {
                    key: SharedString::clone(key),
                    text: SharedString::clone(&message.text),
                },
                copy.0.into(),
                copy.1,
                true,
            ));
        }
        let target = message.target.as_ref().filter(|_| chat);
        let ready = self.active_session_ready();
        let running = self.turn_running();
        if let Some(target) = target {
            for (widget, _) in self
                .slot_widget_items(FrontendSlot::MessageActions)
                .into_values()
            {
                let action = widget.action.clone(); // rules-allow: the action owns its operation while the widget keeps its own
                let Some(mut op) = action else {
                    continue;
                };
                if let Op::CapabilityCommand { target: slot, .. } = &mut op {
                    *slot = Some(*target);
                }
                let glyph = symbol_icon_or(widget.symbol.as_ref(), "hi.dotsThree");
                // A command that needs an idle chat, such as Fork, waits for the turn to end.
                let enabled = ready && !(running && self.requires_idle(&op));
                actions.push((
                    Action::Widget(Box::new(op)),
                    widget.text.clone().into(), // rules-allow: the action owns its label while the widget keeps its own
                    glyph,
                    enabled,
                ));
            }
        }
        let reply = target.map(|target| {
            (
                Action::Reply {
                    target: *target,
                    text: SharedString::clone(&message.text),
                    author: message.author_name(),
                },
                "Reply".into(),
                "hi.re",
                ready,
            )
        });
        // Only generated answers pin; the rail already marks the user's own messages.
        let pin = target.filter(|_| message.author.is_none()).map(|target| {
            let pinned = self
                .client
                .state
                .session
                .as_ref()
                .is_some_and(|session| session.pinned_messages.contains(target));
            let (label, glyph) = if pinned {
                ("Unpin", "hi.pinLocation03.fill")
            } else {
                ("Pin", "hi.pinLocation03")
            };
            (
                Action::Pin {
                    target: *target,
                    pinned: !pinned,
                },
                label.into(),
                glyph,
                ready,
            )
        });
        let speak = (!message.is_user() && !message.text.trim().is_empty()).then(|| {
            (
                Action::Speak(SharedString::clone(&message.text)),
                "Speak".into(),
                "hi.volumeHigh",
                true,
            )
        });
        actions.extend(reply.into_iter().chain(pin).chain(speak));
        actions
    }

    /// Whether the capability declares `op`'s command as needing an idle chat.
    fn requires_idle(&self, op: &Op) -> bool {
        let Op::CapabilityCommand {
            capability,
            command,
            ..
        } = op
        else {
            return false;
        };
        let session = self
            .client
            .state
            .session
            .iter()
            .flat_map(|s| &s.contributions);
        let gateway = self
            .client
            .state
            .gateway
            .iter()
            .flat_map(|g| &g.agent_catalog.contributions);
        session
            .chain(gateway)
            .filter(|contribution| contribution.capability == *capability)
            .flat_map(|contribution| &contribution.commands)
            .any(|declared| declared.name == *command && declared.requires_idle)
    }

    fn run_action(&mut self, action: &Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::Copy { key, text } => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                self.mark_copied(key.to_string(), cx);
            }
            Action::Widget(op) => {
                self.dispatch_widget_operation((**op).clone(), false, cx); // rules-allow: the action runs on every click and each dispatch owns its operation
            }
            Action::Reply {
                target,
                text,
                author,
            } => {
                self.begin_reply(*target, text.to_string(), author.clone(), window, cx); // rules-allow: the action runs on every click and each reply owns its author
            }
            Action::Pin { target, pinned } => {
                let message = self
                    .client
                    .state
                    .selected_session_id
                    .as_ref()
                    .map(|session_id| ClientMessage::SetMessagePinned {
                        request_id: crate::gateway::request_id(),
                        session_id: session_id.share(),
                        target: *target,
                        pinned: *pinned,
                    });
                if let Some(message) = message {
                    self.send(message, cx);
                }
            }
            Action::Speak(text) => self.speak_message(text),
        }
    }

    /// The row under an answer: its actions, then "· face Bot" in mono.
    pub(super) fn message_actions(
        &self,
        message: &Message,
        chat: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let buttons = self.actions(message, chat).into_iter().enumerate().map(
            |(index, (action, label, glyph, enabled))| {
                let id = SharedString::from(format!("{}:action:{index}", message.key));
                inline_button(id, label, glyph, cx)
                    .flex_shrink_0()
                    .disabled(!enabled)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_action(&action, window, cx);
                    }))
            },
        );
        div()
            .max_w_full()
            .min_w_0()
            .flex()
            .items_center()
            .children(buttons)
            .children(self.message_attribution(message))
            .into_any_element()
    }

    fn message_attribution(&self, message: &Message) -> Option<Stateful<Div>> {
        let source = match &message.author {
            Some(MessageAuthor::Source {
                source: mobius::protocol::MessageSource::Session { session_id },
                handle,
                ..
            }) => Some((session_id, handle)),
            _ => None,
        };
        if message.bot.is_none() && source.is_none() && message.preview_author.is_none() {
            return None;
        }
        let title = source.and_then(|(id, _)| {
            self.client
                .state
                .session_record(id)
                .and_then(|session| session.title.as_deref())
        });
        let conversation = source.is_some_and(|(id, _)| self.conversation_bot(id).is_some());
        let name = SharedString::from(message.author_name());
        let tooltip = source.map(|_| match title {
            Some(title) if !conversation => SharedString::from(format!("{name}\n{title}")),
            _ => SharedString::clone(&name),
        });
        let bot = message.bot.as_ref().map(|(_, tint, shape)| (*tint, *shape));
        let label = match title.filter(|_| !conversation) {
            Some(title) => SharedString::from(format!("{name} · {title}")),
            None => name,
        };
        Some(
            div()
                .id(SharedString::from(format!("{}:sender", message.key)))
                .flex()
                .min_w_0()
                .items_center()
                .gap(space::XS)
                .px(space::XS)
                .typography(Type::Caption)
                .text_color(self.palette.muted)
                .child(div().flex_shrink_0().child("·"))
                .child(
                    div()
                        .flex_shrink_0()
                        .child(message.glyph(&self.palette, glyph::MARK)),
                )
                .child(attribution::Label {
                    name: label,
                    recorded_at_ms: message.recorded_at_ms,
                })
                .when_some(tooltip, |view, tooltip| {
                    view.tooltip(move |window, cx| {
                        let tooltip = SharedString::clone(&tooltip);
                        Tooltip::element(move |_, cx| {
                            div()
                                .flex()
                                .items_center()
                                .gap(space::S)
                                .child(bot.map_or_else(
                                    || icon("bot.face", &palette(cx)).into_any_element(),
                                    |(tint, shape)| {
                                        BotFace::new(tint, shape, glyph::INLINE).into_any_element()
                                    },
                                ))
                                .child(SharedString::clone(&tooltip))
                        })
                        .build(window, cx)
                    })
                }),
        )
    }

    /// Right-click on `row`: its actions, then when it was sent and who wrote it.
    pub(super) fn with_message_menu(
        &self,
        row: Stateful<Div>,
        message: Message,
        chat: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let actions: Vec<_> = self
            .actions(&message, chat)
            .into_iter()
            .map(|(action, label, glyph, enabled)| (Rc::new(action), label, glyph, enabled))
            .collect();
        row.context_menu(move |menu, _, cx| {
            let mut menu = menu;
            for (action, label, glyph, enabled) in &actions {
                let action = Rc::clone(action);
                let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                    this.run_action(&action, window, cx);
                };
                let label = SharedString::clone(label);
                menu = menu.item(
                    workspace_item(label, glyph, &view, run, &palette(cx)).disabled(!enabled),
                );
            }
            message_information(menu, &message, cx)
        })
        .into_any_element()
    }
}

/// The disabled information rows closing a message's menu.
fn message_information(menu: PopupMenu, message: &Message, cx: &App) -> PopupMenu {
    let author = if message.is_user() {
        Some(PopupMenuItem::new("you").icon(icon("hi.userFocus", &palette(cx))))
    } else if let Some((name, tint, shape)) = &message.bot {
        Some(BotFace::new(*tint, *shape, glyph::INLINE).menu_item(Arc::clone(name)))
    } else {
        (message.author.is_some() || message.preview_author.is_some())
            .then(|| PopupMenuItem::new(message.author_name()).icon(icon("bot.face", &palette(cx))))
    };
    let time = timestamp(message.recorded_at_ms);
    if time.is_none() && author.is_none() {
        return menu;
    }
    let mut menu = menu.separator();
    if let Some(time) = time {
        menu = menu.item(
            PopupMenuItem::new(time)
                .icon(icon("hi.clock", &palette(cx)))
                .disabled(true),
        );
    }
    if let Some(author) = author {
        menu = menu.item(author.disabled(true));
    }
    menu
}
