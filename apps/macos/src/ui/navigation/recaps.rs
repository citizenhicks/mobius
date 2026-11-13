//! "While you were away": an on-device recap of each chat that changed since it was last
//! read, written lazily when the dialog opens and kept until the chat changes again.

use super::catalog::session_title;
use super::*;
use crate::gateway::Update;
use crate::host::platform::{ChatRecap, FoundationStatus};
use crate::ui::primitives::Skeleton;
use mobius::SessionId;
use mobius_gateway::wire::{SessionActivityState, SessionRecord};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Recaps written at once; each holds a history read and then the on-device model.
const CONCURRENCY: usize = 2;

/// What a chat changed by: its catalog sequence and update time.
type Revision = (u64, i64);

#[derive(Default)]
pub(in crate::ui) struct RecapState {
    /// Each chat's catalog `updated_at` (gateway milliseconds) when it was last on screen.
    read_at: BTreeMap<SessionId, i64>,
    recaps: BTreeMap<SessionId, Recap>,
    queue: VecDeque<SessionId>,
    writing: BTreeSet<SessionId>,
}

pub(in crate::ui) struct Recap {
    revision: Revision,
    body: RecapBody,
}

pub(in crate::ui) enum RecapBody {
    /// The history read or the model is under way; `fallback` is the last message so far.
    Writing {
        fallback: Option<String>,
    },
    Written(ChatRecap),
    /// No recap could be written: the chat's last message stands in for it.
    Plain(String),
}

/// What a chat waits on, in words for the model, which writes "Needs you" in the chat's
/// language; the card's header carries the sidebar's language-neutral status either way.
fn waiting(session: &SessionRecord) -> Option<&'static str> {
    if session.activity.attention > 0 {
        Some("an answer to a question")
    } else if session.activity.state == SessionActivityState::AwaitingApproval {
        Some("an approval")
    } else {
        None
    }
}

fn revision(session: &SessionRecord) -> Revision {
    (session.sequence, session.updated_at)
}

impl RecapState {
    /// The chat `session` is on screen: everything it holds now has been read.
    pub(in crate::ui) fn read(&mut self, session: &SessionRecord) {
        self.read_at
            .insert(session.session_id.share(), session.updated_at);
    }

    /// A fixture recap for `session`; a writing one stays writing.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn set_fixture(&mut self, session: &SessionRecord, body: RecapBody) {
        let id = session.session_id.share();
        if matches!(body, RecapBody::Writing { .. }) {
            self.writing.insert(id.share());
        }
        let revision = revision(session);
        self.recaps.insert(id, Recap { revision, body });
    }
}

impl Workspace {
    /// Chats with something new since they were last read, newest first.
    fn changed_chats(&self) -> Vec<&SessionRecord> {
        let open = self.selected_chat();
        let mut chats: Vec<_> = self
            .client
            .state
            .sessions
            .iter()
            .filter(|session| {
                let id = session.session_id.as_ref();
                Some(id) != open
                    && self.conversation_bot(id).is_none()
                    && (self.is_unread(id) || waiting(session).is_some())
            })
            .collect();
        chats.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        chats
    }

    /// The header button shows only with something to recap and a model to recap it.
    pub(super) fn recaps_available(&self) -> bool {
        !self.changed_chats().is_empty()
            && self.native.foundation_status() == FoundationStatus::Available
    }

    pub(in crate::ui) fn open_recaps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stale: Vec<_> = self
            .changed_chats()
            .into_iter()
            .filter(|session| {
                let id = &session.session_id;
                !self.shell.recaps.writing.contains(id)
                    && self
                        .shell
                        .recaps
                        .recaps
                        .get(id)
                        .is_none_or(|recap| recap.revision != revision(session))
            })
            .map(|session| (session.session_id.share(), revision(session)))
            .collect();
        let recaps = &mut self.shell.recaps;
        for (id, revision) in stale {
            recaps.recaps.insert(
                id.share(),
                Recap {
                    revision,
                    body: RecapBody::Writing { fallback: None },
                },
            );
            recaps.queue.push_back(id);
        }
        self.write_recaps();
        let owner = cx.entity();
        let list = cx.new(|cx| {
            cx.observe(&owner, |_, _, cx| cx.notify()).detach();
            RecapDialog {
                workspace: owner.downgrade(),
            }
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(heading("While you were away"))
                .w(width::DIALOG)
                .child(Entity::clone(&list))
        });
    }

    /// Starts queued recaps while fewer than `CONCURRENCY` are being written.
    fn write_recaps(&mut self) {
        while self.shell.recaps.writing.len() < CONCURRENCY {
            let Some(id) = self.shell.recaps.queue.pop_front() else {
                return;
            };
            let since = self.shell.recaps.read_at.get(&id).copied();
            let started = self
                .client
                .gateway
                .as_ref()
                .is_some_and(|gateway| gateway.recent_history(id.share(), since).is_ok());
            if started {
                self.shell.recaps.writing.insert(id);
            } else {
                self.recap_written(&id, None);
            }
        }
    }

    /// The unread part of chat `session_id` arrived; the on-device model recaps it.
    pub(in crate::ui) fn recent_loaded(&mut self, update: Update) {
        let Update::Recent {
            endpoint,
            session_id,
            result,
        } = update
        else {
            return;
        };
        if endpoint != self.client.endpoint.as_str()
            || !self.shell.recaps.writing.contains(&session_id)
        {
            return;
        }
        let session_id = &*session_id;
        let chat = result.unwrap_or_default();
        let waits = self
            .client
            .state
            .session_record(session_id)
            .and_then(waiting)
            .unwrap_or_default();
        if let Some(Recap {
            body: RecapBody::Writing { fallback },
            ..
        }) = self.shell.recaps.recaps.get_mut(session_id)
        {
            *fallback = chat.last_message;
        }
        let asked = !chat.transcript.is_empty()
            && self
                .native
                .generate_recap(session_id, &chat.transcript, waits)
                .is_ok();
        if !asked {
            self.recap_written(session_id, None);
        }
    }

    /// The model answered for `session_id`, with a recap or without one; without one, the
    /// chat's last message stands in.
    pub(in crate::ui) fn recap_written(&mut self, session_id: &str, recap: Option<ChatRecap>) {
        self.shell.recaps.writing.remove(session_id);
        let session = self.client.state.session_record(session_id);
        if let Some(entry) = self.shell.recaps.recaps.get_mut(session_id) {
            let fallback = match &mut entry.body {
                RecapBody::Writing { fallback } => fallback.take(),
                _ => None,
            };
            entry.body = match recap {
                Some(mut recap) => {
                    recap.done.truncate(3);
                    // Only the model's own line, in the chat's language; a chat no longer
                    // waiting drops a stale one.
                    recap.needs_you = session.and_then(waiting).and(recap.needs_you);
                    RecapBody::Written(recap)
                }
                None => RecapBody::Plain(fallback.unwrap_or_else(|| {
                    session
                        .and_then(|session| session.activity.message.as_deref())
                        .or_else(|| session.and_then(|s| s.first_user_message.as_deref()))
                        .unwrap_or("Nothing new to show.")
                        .to_owned()
                })),
            };
        }
        self.write_recaps();
    }

    fn recap_list(&self, cx: &Context<Self>) -> AnyElement {
        let bots = self
            .client
            .state
            .gateway
            .as_ref()
            .map(|gateway| &gateway.bots);
        let cards = self.changed_chats().into_iter().map(|session| {
            let id = &session.session_id;
            let bot = bots.and_then(|bots| {
                bots.iter()
                    .find(|bot| bot.id == session.session_context.owner_id)
            });
            let face = bot.map(|bot| (Accent::from(bot.tint).color(), bot.shape));
            let body = self.shell.recaps.recaps.get(id).map(|recap| &recap.body);
            let status = self.chat_status(id);
            recap_card(
                id,
                session_title(session),
                face,
                status,
                body,
                &self.palette,
                cx,
            )
        });
        div()
            .id("recaps")
            .max_h(height::DIALOG_LIST_MAX)
            .overflow_y_scroll()
            .child(Group::new().children(cards))
            .into_any_element()
    }
}

/// One chat's card: who and which chat, then what happened and what waits on the reader.
fn recap_card(
    id: &SessionId,
    title: String,
    face: Option<(Hsla, mobius_gateway::wire::BotShape)>,
    status: Option<AnyElement>,
    body: Option<&RecapBody>,
    p: &Palette,
    cx: &Context<Workspace>,
) -> AnyElement {
    let mut spoken: Vec<Cow<'_, str>> = Vec::new();
    let lines: Vec<AnyElement> =
        match body {
            Some(RecapBody::Written(recap)) => {
                spoken.push(recap.headline.as_str().into());
                spoken.extend(recap.done.iter().map(|line| format!("Done: {line}").into()));
                let mut lines = vec![
                    div()
                        .typography(Type::Body)
                        .child(recap.headline.clone()) // rules-allow: the element owns its text while the recap keeps it
                        .into_any_element(),
                ];
                lines.extend(recap.done.iter().map(|line| {
                    recap_line("hi.check", p.muted, line, p.muted, p).into_any_element()
                }));
                if let Some(needs) = &recap.needs_you {
                    spoken.push(format!("Needs you: {needs}").into());
                    lines.push(
                        recap_line("hi.mousePointerClick", p.warning, needs, p.text, p)
                            .into_any_element(),
                    );
                }
                lines
            }
            Some(RecapBody::Plain(message)) => {
                spoken.push(message.as_str().into());
                vec![
                    div()
                        .typography(Type::Body)
                        .text_color(p.muted)
                        .line_clamp(2)
                        .child(message.to_owned())
                        .into_any_element(),
                ]
            }
            Some(RecapBody::Writing { .. }) | None => {
                spoken.push("Writing a recap".into());
                [0.9, 0.6]
                    .map(|width| {
                        div()
                            .h(Type::Body.line_height())
                            .flex()
                            .items_center()
                            .child(Skeleton::new().h(px(12.)).pill_corners().w(relative(width)))
                            .into_any_element()
                    })
                    .into()
            }
        };
    let leading = match face {
        Some((tint, shape)) => BotFace::new(tint, shape, face::ROW).into_any_element(),
        None => icon("bot.face", p).size(face::ROW).into_any_element(),
    };
    let open = id.share();
    div()
        .id(SharedString::from(format!("recap:{id}")))
        .role(gpui::accesskit::Role::Button)
        .aria_label(format!("{title}. {}", spoken.join(". ")))
        .cursor_pointer()
        .hover(|style| style.bg(p.hover))
        .flex()
        .gap(space::S)
        .px(space::L)
        .py(space::S)
        .child(leading)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .typography(Type::BodyMedium)
                                .truncate()
                                .child(title),
                        )
                        .children(status),
                )
                .children(lines),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            window.close_dialog(cx);
            this.open_page(PageTab::Chat(open.share()), window, cx);
        }))
        .into_any_element()
}

/// A recap line led by a small glyph.
fn recap_line(glyph_name: &str, tint: Hsla, text: &str, ink: Hsla, p: &Palette) -> Div {
    div()
        .flex()
        .items_start()
        .gap(space::S)
        .typography(Type::Caption)
        .text_color(ink)
        .child(
            div()
                .h(Type::Caption.line_height())
                .flex()
                .items_center()
                .child(icon(glyph_name, p).size(glyph::MARK).text_color(tint)),
        )
        .child(div().flex_1().min_w_0().child(text.to_owned()))
}

struct RecapDialog {
    workspace: WeakEntity<Workspace>,
}

impl Render for RecapDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace
            .update(cx, |this, cx| this.recap_list(cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}
