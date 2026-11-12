//! Transcript rows as SwiftUI draws them (TranscriptView.swift, TranscriptEventViews.swift):
//! user bubbles, incoming session messages, assistant prose, notices, Worked for rows,
//! event groups and file cards. The chat, previews and routines share these rows.

mod actions;
mod attribution;
mod collapsible;
mod disclosure;
mod issue;
mod links;
mod scroll;
mod turn_diff;
mod ui_document;
pub(in crate::ui) use disclosure::BlockLine;
pub(in crate::ui) use issue::{IssueAction, ProviderIssue};
pub(in crate::ui) use links::StreamingMarkdown;
pub(in crate::ui) use scroll::HISTORY;
pub(super) use scroll::TranscriptScroll;

use super::*;
use crate::gateway::{TranscriptEntry, TranscriptStore};
use crate::numeric::ToFloat;
use gpui_component::menu::ContextMenuExt;
use mobius::SessionId;
use mobius::protocol::{
    FrontendAction, FrontendEditor, FrontendSymbol, FrontendTone, MessageAuthor, MessageDelivery,
    MessageEvent, ModelStepContentPhase, SessionFileReference,
};
use mobius_gateway::wire::{
    BotShape, TranscriptItemContent, TranscriptRow, TranscriptRowKind, TurnChanges,
};

/// The longest side of a generated image (TranscriptImageCard's widest card).
const IMAGE_CARD: f32 = 320.;

fn tone(tone: FrontendTone) -> Tone {
    match tone {
        FrontendTone::Error => Tone::Error,
        FrontendTone::Warning => Tone::Warning,
        FrontendTone::Success => Tone::Success,
        FrontendTone::Neutral => Tone::Info,
    }
}

impl Workspace {
    /// One projected row at the transcript's width, rising in if it just arrived.
    pub(super) fn transcript_row_view(
        &self,
        transcript: &TranscriptStore,
        row: &TranscriptRow,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = self.transcript_row(transcript, row, session_id, scope, cx);
        let view = div()
            .id(SharedString::from(format!("{scope}:{}", row.id)))
            .mx_auto()
            .w_full()
            .max_w(width::TRANSCRIPT)
            .flex_shrink_0()
            .typography(Type::Prose)
            .text_color(self.palette.ink)
            .child(content);
        #[cfg(feature = "snapshot")]
        let view = view
            .relative()
            .child(crate::ui::snapshot::probe(format!("row:{}", row.id)));
        // New rows grow in from nothing while fading, so neighbours slide rather than jump.
        let arriving = self.animate_transcript_row(scope, &row.id);
        let view = if arriving {
            let id = SharedString::from(format!("reveal:{scope}:{}", row.id));
            motion::fade_in(view, id, motion::REVEAL, cx)
        } else {
            view.into_any_element()
        };
        motion::smooth(row_motion_id(scope, &row.id), view)
            .grow_in(arriving)
            .into_any_element()
    }

    /// One row: an entry, a notice, an event group or a turn's work.
    fn transcript_row(
        &self,
        transcript: &TranscriptStore,
        row: &TranscriptRow,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = transcript.row_items(row).next();
        match &row.kind {
            TranscriptRowKind::Entry { turn_changes } => {
                let Some(entry) = first else {
                    return div().into_any_element();
                };
                let changes = turn_changes.as_ref().map(|changes| (transcript, changes));
                self.transcript_entry(entry, changes, session_id, scope, cx)
            }
            TranscriptRowKind::Notice => match first {
                Some(entry) if entry.block().is_some() => match issue::provider_issue(entry) {
                    Some((issue, message)) => {
                        self.provider_issue_row(entry, issue, message, scope, cx)
                    }
                    None => self.notice_row(entry),
                },
                Some(entry) => self.transcript_entry(entry, None, session_id, scope, cx),
                None => div().into_any_element(),
            },
            TranscriptRowKind::Activity { counts } => {
                self.event_group(transcript, row, *counts, session_id, scope, cx)
            }
            TranscriptRowKind::Worked {
                counts,
                started_at_ms,
                duration_ms,
            } => self.worked_row(
                transcript,
                row,
                (*counts, *started_at_ms, *duration_ms),
                session_id,
                scope,
                cx,
            ),
        }
    }

    /// One entry; `turn_diff` is what the turn changed, for its last answer.
    fn transcript_entry(
        &self,
        entry: &TranscriptEntry,
        turn_changes: Option<(&TranscriptStore, &TurnChanges)>,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match &entry.content {
            TranscriptItemContent::Message { message } if message.author == MessageAuthor::User => {
                self.user_bubble(entry, message, session_id, scope, cx)
            }
            TranscriptItemContent::Message { message } if entry.session_sender().is_some() => {
                self.session_message(entry, message, session_id, scope, cx)
            }
            TranscriptItemContent::Message { message } => {
                self.assistant_prose(entry, &message.text, None, session_id, scope, cx)
            }
            TranscriptItemContent::Assistant {
                phase: ModelStepContentPhase::Reasoning,
                text,
                ..
            } => self.reasoning_line(&entry.id, text, session_id, scope, cx),
            TranscriptItemContent::Assistant { text, .. } => {
                self.assistant_prose(entry, text, turn_changes, session_id, scope, cx)
            }
            TranscriptItemContent::Block { block, .. }
                if block.format == mobius::protocol::FrontendBlockFormat::Image =>
            {
                self.image_card(entry, block, session_id, cx)
            }
            TranscriptItemContent::Block { block, .. }
                if block.format == mobius::protocol::FrontendBlockFormat::Ui =>
            {
                self.ui_document(entry, block, session_id, cx)
            }
            TranscriptItemContent::Block { block, .. } => div()
                .flex()
                .flex_col()
                .gap(space::S)
                .children(self.file_row(
                    block.files.iter().chain(block.content.files()),
                    session_id,
                    false,
                    cx,
                ))
                .child(self.event_line(entry, session_id, scope, cx))
                .into_any_element(),
            TranscriptItemContent::Notice { tone: level, text }
                if *level == FrontendTone::Error =>
            {
                Notice::new(tone(*level), &**text).into_any_element()
            }
            TranscriptItemContent::Notice { text, .. } => div()
                .flex()
                .items_center()
                .typography(Type::Caption)
                .text_color(self.palette.muted)
                .child(SharedString::from(text))
                .into_any_element(),
            TranscriptItemContent::ForkPoint { parent } => self.fork_point(parent, cx),
        };
        self.with_rail_reveal(content, scope, &entry.id, cx)
    }

    /// The divider between what a fork inherited and its own conversation: "Forked from"
    /// and the parent chat, which it opens.
    fn fork_point(&self, parent: &SessionId, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let parent = parent.share();
        let title = self.page_title(&PageTab::Chat(parent.share()));
        let known = self
            .client
            .state
            .sessions
            .iter()
            .any(|s| s.session_id == parent);
        let rule = || hairline(&p).flex_1();
        let name = div()
            .id("fork-point-parent")
            .min_w_0()
            .truncate()
            .text_color(if known { p.accent } else { p.muted })
            .child(if known {
                SharedString::clone(&title)
            } else {
                "another chat".into()
            })
            .when(known, |name| {
                name.cursor_pointer()
                    .hover(gpui::Styled::underline)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_page(PageTab::Chat(parent.share()), window, cx);
                    }))
            });
        div()
            .id("fork-point")
            .role(gpui::accesskit::Role::Group)
            .aria_label(format!("Forked from {title}"))
            .min_h(height::REGULAR)
            .flex()
            .items_center()
            .gap(space::S)
            .typography(Type::Label)
            .text_color(p.muted)
            .child(rule())
            .child(
                div()
                    .min_w_0()
                    .max_w(width::QUOTE)
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .child(
                        icon("hi.gitBranch", &palette(cx))
                            .size(glyph::INLINE)
                            .flex_shrink_0(),
                    )
                    .child(div().flex_shrink_0().child("Forked from"))
                    .child(name),
            )
            .child(rule())
            .into_any_element()
    }

    /// TranscriptRow for the reader: files, a clamped accent bubble and "you".
    fn user_bubble(
        &self,
        entry: &TranscriptEntry,
        message: &MessageEvent,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let text = SharedString::from(message.text.clone()); // rules-allow: the bubble owns its text while the transcript keeps the message
        let bubble = div()
            .px(space::L)
            .py(space::S)
            .tile_corners()
            .bg(p.accent_soft)
            .flex()
            .flex_col()
            .gap(space::S)
            .child(self.collapsible_text(
                format!("{scope}:{}:more", entry.id),
                SharedString::clone(&text),
                None,
                cx,
            ));
        let (glyph, delivery) = match message.delivery {
            MessageDelivery::Turn => ("hi.userFocus", None),
            MessageDelivery::Steer => ("hi.arrowUpRight01", Some("steer")),
            MessageDelivery::Queue => ("hi.queue01", Some("queued")),
        };
        let row = div()
            .id(SharedString::from(format!("message-{}", entry.id)))
            .role(gpui::accesskit::Role::Article)
            .aria_label(format!("You: {}", message.text))
            .pl(width::BUBBLE_INDENT)
            .flex()
            .flex_col()
            .items_end()
            .gap(space::S)
            .children(
                entry
                    .resumed
                    .then(|| {
                        attribution::divider_timestamp(entry.recorded_at_ms, &chrono::Local::now())
                    })
                    .flatten()
                    .map(|heading| {
                        div()
                            .w_full()
                            .pb(space::L)
                            .flex()
                            .justify_center()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(heading)
                    }),
            )
            .children(
                message
                    .reply
                    .as_ref()
                    .map(|reply| chat::reply_quote(&p, &reply.text).max_w(width::QUOTE)),
            )
            .children(self.file_row(&message.attachments, session_id, true, cx))
            .when(!message.text.is_empty(), |row| row.child(bubble))
            .child(message_meta(&p, glyph, delivery, entry.recorded_at_ms));
        let reply_text = if message.text.is_empty() {
            let names = message.attachments.iter().map(|file| file.name.as_str());
            names.collect::<Vec<_>>().join(", ").into()
        } else {
            text
        };
        let actions = actions::Message {
            key: format!("{scope}:{}", entry.id).into(),
            text: reply_text,
            target: message.message_target,
            author: Some(message.author.clone()), // rules-allow: the 'static message menu owns its author
            recorded_at_ms: entry.recorded_at_ms,
            bot: None,
            preview_author: None,
        };
        self.with_message_menu(row, actions, scope.starts_with("chat:"), cx)
    }

    /// Human and peer messages share the measured five-line preview.
    fn collapsible_text(
        &self,
        key: String,
        text: SharedString,
        expanded_content: Option<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(key);
        let toggle = SharedString::clone(&id);
        collapsible::CollapsibleText {
            expanded: self.expanded.contains(id.as_ref()),
            id,
            text,
            expanded_content,
            toggle: std::rc::Rc::new(cx.listener(move |this, _, _, cx| {
                if !this.expanded.remove(toggle.as_ref()) {
                    this.expanded.insert(toggle.to_string()); // rules-allow: the toggle is Fn and the expanded set owns its keys
                }
                cx.notify();
            })),
        }
        .into_any_element()
    }

    /// A session's incoming message: trailing, right ruled, with its own sender below.
    fn session_message(
        &self,
        entry: &TranscriptEntry,
        message: &MessageEvent,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let actions = actions::Message {
            key: format!("{scope}:{}", entry.id).into(),
            text: message.text.clone().into(), // rules-allow: the 'static message menu owns its text
            target: entry.message_target().copied(),
            author: Some(message.author.clone()), // rules-allow: the 'static message menu owns its author
            recorded_at_ms: entry.recorded_at_ms,
            bot: self.entry_bot(entry),
            preview_author: None,
        };
        let row = div()
            .id(SharedString::from(format!("source-{}", entry.id)))
            .role(gpui::accesskit::Role::Article)
            .aria_label(format!("Message from {}", actions.author_name()))
            .ml(width::BUBBLE_INDENT)
            .border_r_2()
            .border_color(p.accent)
            .pr(space::L)
            .flex()
            .flex_col()
            .items_end()
            .gap(space::S)
            .children(
                message
                    .reply
                    .as_ref()
                    .map(|reply| chat::reply_quote(&p, &reply.text)),
            )
            .children(self.file_row(&message.attachments, session_id, true, cx))
            .when(!message.text.is_empty(), |row| {
                row.child(
                    self.collapsible_text(
                        format!("{scope}:{}:more", entry.id),
                        SharedString::clone(&actions.text),
                        Some(
                            self.transcript_markdown(
                                SharedString::clone(&actions.key),
                                SharedString::clone(&actions.text),
                                session_id,
                                cx,
                            )
                            .into_any_element(),
                        ),
                        cx,
                    ),
                )
            })
            .child(self.message_actions(&actions, scope.starts_with("chat:"), cx));
        self.with_message_menu(row, actions, scope.starts_with("chat:"), cx)
    }

    /// Unboxed markdown prose; a finished answer carries its actions and Bot below it.
    fn assistant_prose(
        &self,
        entry: &TranscriptEntry,
        text: &str,
        turn_changes: Option<(&TranscriptStore, &TurnChanges)>,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let live = entry.state == mobius_gateway::wire::TranscriptItemState::Streaming;
        let streaming = live && !cx.reduce_motion();
        let finished = !entry.is_pending() && !text.trim().is_empty();
        let (markdown, text) = self.streaming_transcript_markdown(
            SharedString::from(format!("{scope}:{}", entry.id)),
            entry.generation(),
            text,
            (live, finished),
            session_id,
            cx,
        );
        let markdown = markdown.stream_fade(streaming);
        let preview_author = match self.content.tabs.get(scope).map(|tab| &tab.content) {
            Some(TabContent::Preview { preview, .. }) => Some(Arc::clone(&preview.source.title)),
            _ => None,
        };
        let message = text.map(|text| actions::Message {
            key: format!("{scope}:{}", entry.id).into(),
            text,
            target: entry.message_target().copied(),
            author: match &entry.content {
                TranscriptItemContent::Message { message } => Some(message.author.clone()), // rules-allow: the 'static message menu owns its author
                _ => None,
            },
            recorded_at_ms: entry.recorded_at_ms,
            bot: preview_author
                .is_none()
                .then(|| self.entry_bot(entry))
                .flatten(),
            preview_author,
        });
        let chat = scope.starts_with("chat:");
        let prose = div()
            .id(SharedString::from(format!("assistant-{}", entry.id)))
            .role(gpui::accesskit::Role::Article)
            .aria_label("Assistant message")
            .w_full()
            .flex()
            .flex_col()
            .gap(space::XS)
            .child(markdown)
            .children(turn_changes.and_then(|(transcript, changes)| {
                self.turn_diff_card(
                    &format!("{scope}:{}", entry.id),
                    transcript,
                    changes,
                    scope,
                    cx,
                )
            }))
            .children(
                message
                    .as_ref()
                    .map(|message| self.message_actions(message, chat, cx)),
            );
        #[cfg(feature = "snapshot")]
        let prose = prose
            .relative()
            .child(snapshot::probe(format!("selection:{}", entry.id)));
        match message {
            Some(message) => self.with_message_menu(prose, message, chat, cx),
            None => prose.into_any_element(),
        }
    }

    /// The known sender's Bot by exact session ownership; ordinary answers use this chat.
    fn entry_bot(&self, entry: &TranscriptEntry) -> Option<(Arc<str>, Hsla, BotShape)> {
        if matches!(
            &entry.content,
            TranscriptItemContent::Message {
                message: MessageEvent {
                    author: MessageAuthor::Source {
                        source: mobius::protocol::MessageSource::Call { .. },
                        ..
                    },
                    ..
                }
            }
        ) {
            return None;
        }
        if let Some((_, session_id)) = entry.session_sender() {
            let bot = self.conversation_bot(session_id).or_else(|| {
                let owner = &self
                    .client
                    .state
                    .session_record(session_id)?
                    .session_context;
                self.client.state.bot(&owner.owner_id)
            })?;
            return Some((
                Arc::clone(&bot.name),
                Accent::from(bot.tint).color(),
                bot.shape,
            ));
        }
        let bot = match &entry.content {
            TranscriptItemContent::Message {
                message:
                    MessageEvent {
                        author: MessageAuthor::Source { .. },
                        ..
                    },
            } => return None,
            _ => self.composer_bot()?,
        };
        Some((
            Arc::clone(&bot.name),
            Accent::from(bot.tint).color(),
            bot.shape,
        ))
    }

    /// File cards for a message or event, deduplicated, in a wrapping row of equal
    /// heights against the column's edge: the reader's on the right, the Bot's on the left.
    /// `None` without files, so an empty row takes no gap.
    fn file_row<'a>(
        &self,
        files: impl IntoIterator<Item = &'a SessionFileReference>,
        session_id: Option<&SessionId>,
        trailing: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let cards = self.file_buttons(files, session_id, cx);
        (!cards.is_empty()).then(|| {
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .gap(space::S)
                .when(trailing, gpui::Styled::justify_end)
                .children(cards)
                .into_any_element()
        })
    }

    /// TranscriptImageCard: a generated image at up to 320 points on its long side, in its
    /// own shape, with no event line; while it generates, a square placeholder.
    fn image_card(
        &self,
        entry: &TranscriptEntry,
        block: &mobius::protocol::FrontendBlock,
        session_id: Option<&SessionId>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let file = block.files.iter().chain(block.content.files()).next();
        let thumbnail = session_id.and_then(|session_id| {
            file.filter(|file| !self.file_removed(session_id, &file.id))
                .and_then(|file| self.file_thumbnail(session_id, file))
        });
        let image = block.content.0.iter().find_map(|part| match part {
            mobius::protocol::ContentPart::Image { image } => Some((image.width, image.height)),
            _ => None,
        });
        let (width, height) = match (&thumbnail, image) {
            (Some(t), _) if t.width > 0 && t.height > 0 => (t.width.to_f32(), t.height.to_f32()),
            (_, Some((width, height))) if width > 0 && height > 0 => {
                (width.to_f32(), height.to_f32())
            }
            _ => (1., 1.),
        };
        let scale = IMAGE_CARD / width.max(height);
        let size = size(px(width * scale), px(height * scale));
        // The picture once it is here; until then the pulsing field at the picture's size,
        // whether it is still being made or only its thumbnail is on its way.
        let pending = entry.is_pending();
        let card = match file {
            Some(file)
                if session_id.is_some_and(|session| self.file_removed(session, &file.id)) =>
            {
                self.purged_image_card(size)
            }
            Some(file) if thumbnail.is_some() => self.file_button(file, session_id, Some(size), cx),
            Some(_) | None
                if (pending && file.is_none())
                    || session_id.is_some_and(|session| {
                        file.is_some_and(|file| self.thumbnail_pending(session, file))
                    }) =>
            {
                div()
                    .w(size.width)
                    .h(size.height)
                    .relative()
                    .overflow_hidden()
                    .tile_corners()
                    .bg(p.raised)
                    .child(organic::pulse(p.accent, false))
                    .when(pending, |card| {
                        card.child(
                            div()
                                .absolute()
                                .bottom(space::S)
                                .left_0()
                                .right_0()
                                .flex()
                                .justify_center()
                                .child(
                                    div()
                                        .px(space::S)
                                        .py(space::XS)
                                        .pill_corners()
                                        .bg(p.card)
                                        .typography(Type::Label)
                                        .text_color(p.muted)
                                        .child("Creating image…"),
                                ),
                        )
                    })
                    .into_any_element()
            }
            _ => {
                let label = if block.title.is_empty() {
                    "Image unavailable".to_owned()
                } else {
                    block.title.clone() // rules-allow: the card owns its label while the block keeps its title
                };
                div()
                    .w(size.width)
                    .h(size.height)
                    .tile_corners()
                    .bg(p.control)
                    .border_1()
                    .border_color(p.separator)
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(space::S)
                    .text_color(p.muted)
                    .child(icon("hi.image01", &palette(cx)).size(glyph::ICON))
                    .child(div().typography(Type::Label).child(label))
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap(space::S)
            .child(card)
            .when(!entry.is_pending() && !block.text.is_empty(), |column| {
                column.child(div().text_color(p.danger).child(block.text.clone())) // rules-allow: the element owns its text while the block keeps it
            })
            .into_any_element()
    }

    fn purged_image_card(&self, size: Size<Pixels>) -> AnyElement {
        let p = self.palette;
        div()
            .w(size.width)
            .h(size.height)
            .relative()
            .overflow_hidden()
            .tile_corners()
            .bg(p.raised)
            .child(organic::pulse(p.accent, true))
            .child(
                div()
                    .absolute()
                    .bottom(space::S)
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px(space::S)
                            .py(space::XS)
                            .pill_corners()
                            .bg(p.card)
                            .typography(Type::Label)
                            .text_color(p.muted)
                            .child("Image purged"),
                    ),
            )
            .into_any_element()
    }

    /// One file card per distinct file; clicking downloads and previews it.
    pub(super) fn file_buttons<'a>(
        &self,
        files: impl IntoIterator<Item = &'a SessionFileReference>,
        session_id: Option<&SessionId>,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let mut seen = std::collections::BTreeSet::new();
        files
            .into_iter()
            .filter(|file| seen.insert(file.id.as_str()))
            .map(|file| self.file_button(file, session_id, None, cx))
            .collect()
    }

    /// A file's card, `size` or a row tile's, that downloads and previews it on click.
    fn file_button(
        &self,
        file: &SessionFileReference,
        session_id: Option<&SessionId>,
        size: Option<Size<Pixels>>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let removed = session_id.is_some_and(|session| self.file_removed(session, &file.id));
        let enabled = self.client.link.is_ready() && !removed;
        let mention = chat::Mention::ChatFile(file.to_owned());
        let download = file.clone(); // rules-allow: the 'static click handler keeps its own file
        let thumbnail = session_id
            .filter(|_| !removed)
            .and_then(|session| self.file_thumbnail(session, file));
        let size = size.unwrap_or_else(|| chat::tile_size(thumbnail.as_ref()));
        let picture = thumbnail.is_some();
        let card = chat::file_card(
            SharedString::from(format!("file-{}", file.id)),
            chat::file_glyph(&file.media_type),
            file.name.clone(), // rules-allow: the card owns its name while the transcript keeps the file
            if removed {
                "File purged".into()
            } else {
                chat::file_detail(&file.name, file.size)
            },
            p.muted,
            thumbnail.as_ref(),
            cx,
        )
        .opacity(if removed { 0.5 } else { 1.0 })
        .w(size.width)
        .h(size.height)
        .aria_label(if removed {
            "File purged".into()
        } else {
            format!("Open file {}", file.name)
        })
        .when_some(session_id.filter(|_| enabled), |card, session_id| {
            let session_id = session_id.share();
            card.cursor_pointer()
                .when(!picture, |card| {
                    card.hover(move |card| card.bg(p.control.blend(p.hover)))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.download_file(session_id.share(), download.clone(), cx); // rules-allow: the click handler is Fn and each download owns its file
                }))
        });
        if removed || session_id.is_none() {
            card.into_any_element()
        } else {
            card.context_menu(Self::file_menu(mention, cx))
                .into_any_element()
        }
    }
}

impl Workspace {
    /// Messages on their way to the gateway: dimmed bubbles with a spinner, until the
    /// gateway records them, holds them as a steering or queued widget, or rejects them back
    /// into the composer. Exactly one bubble stands for a message at any moment.
    pub(in crate::ui) fn pending_bubbles(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let p = self.palette;
        let session_id = self.client.state.selected_session_id.as_ref();
        self.pending_messages()
            .into_iter()
            .map(|(id, message)| {
                div()
                    .id(SharedString::from(format!("pending-message:{id}")))
                    .mx_auto()
                    .w_full()
                    .max_w(width::TRANSCRIPT)
                    .flex_shrink_0()
                    .pl(width::BUBBLE_INDENT)
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(space::S)
                    .opacity(0.6)
                    .children(self.file_row(&message.files, session_id, true, cx))
                    .when(!message.text.is_empty(), |row| {
                        row.child(
                            div()
                                .px(space::L)
                                .py(space::S)
                                .tile_corners()
                                .bg(p.accent_soft)
                                .whitespace_normal()
                                .child(message.text.clone()), // rules-allow: the bubble owns its text while the pending message keeps it
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::XS)
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(Spinner::new(glyph::INLINE).color(p.muted))
                            .child("sending · you"),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// The list keys of `transcript_tail`'s items, in the same order.
    pub(in crate::ui) fn transcript_tail_keys(&self) -> Vec<String> {
        self.client
            .state
            .session
            .as_ref()
            .map_or_else(Vec::new, |session| {
                session
                    .widgets
                    .iter()
                    .filter(|widget| {
                        widget.item.slot == mobius::protocol::FrontendSlot::TranscriptTail
                    })
                    .map(|widget| format!("tail:{}:{}", widget.capability, widget.item.id))
                    .collect()
            })
    }

    /// Capability items after the live transcript: queued messages as dashed bubbles,
    /// anything else as an event line.
    pub(in crate::ui) fn transcript_tail(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let p = self.palette;
        let Some(session) = &self.client.state.session else {
            return Vec::new();
        };
        session
            .widgets
            .iter()
            .filter(|widget| widget.item.slot == mobius::protocol::FrontendSlot::TranscriptTail)
            .map(|widget| {
                let item = &widget.item;
                let queued = matches!(
                    &item.action,
                    Some(mobius::protocol::Op::CapabilityCommand { input: Some(_), .. })
                );
                let row = if queued {
                    let meta = match item.symbol.as_ref() {
                        Some(FrontendSymbol::Steer) => {
                            message_meta(&p, "hi.arrowUpRight01", Some("steer"), 0)
                        }
                        _ => message_meta(&p, "hi.queue01", Some("queued"), 0),
                    };
                    let edit = item.action.as_ref().map(|op| {
                        self.widget_action(
                            FrontendAction {
                                id: format!("edit:{}", item.id),
                                label: "Edit".into(),
                                symbol: FrontendSymbol::Edit,
                                tone: FrontendTone::Neutral,
                                op: op.clone(), // rules-allow: the editor owns its operation while the live widget keeps it
                                input_from_label: false,
                                editor: Some(FrontendEditor {
                                    title: "Edit message".into(),
                                    label: "Message".into(),
                                    description: "Update this message before it is delivered."
                                        .into(),
                                    submit_label: "Save".into(),
                                }),
                            },
                            false,
                            cx,
                        )
                    });
                    div()
                        .pl(width::BUBBLE_INDENT)
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap(space::S)
                        .child(
                            div()
                                .px(space::L)
                                .py(space::S)
                                .tile_corners()
                                .bg(p.accent_soft.opacity(0.24))
                                .border_1()
                                .border_dashed()
                                .border_color(p.accent.opacity(0.42))
                                .whitespace_normal()
                                .child(item.text.clone()), // rules-allow: the bubble owns its text while the widget keeps it
                        )
                        .child(meta.children(edit))
                } else {
                    div()
                        .min_h(height::COMPACT)
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .text_color(p.muted)
                        .child(
                            icon(
                                symbol_icon_or(item.symbol.as_ref(), "hi.queue01"),
                                &palette(cx),
                            )
                            .size(glyph::INLINE),
                        )
                        .child(item.text.clone()) // rules-allow: the element owns its text while the widget keeps it
                };
                let key = SharedString::from(format!("tail:{}:{}", widget.capability, item.id));
                let view = div()
                    .id(SharedString::clone(&key))
                    .mx_auto()
                    .w_full()
                    .max_w(width::TRANSCRIPT)
                    .flex_shrink_0()
                    .child(row);
                if !queued {
                    return view.into_any_element();
                }
                // A held message copies like a sent one (QueuedMessageView's context menu).
                let message = actions::Message {
                    key,
                    text: item.text.clone().into(), // rules-allow: the 'static message menu owns its text
                    target: None,
                    author: Some(MessageAuthor::User),
                    recorded_at_ms: 0,
                    bot: None,
                    preview_author: None,
                };
                self.with_message_menu(view, message, true, cx)
            })
            .collect()
    }
}

/// "you", "steer · you" or "queued · you" under a bubble.
fn message_meta(
    p: &Palette,
    glyph: &'static str,
    delivery: Option<&'static str>,
    recorded_at_ms: i64,
) -> Div {
    div()
        .max_w_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap(space::XS)
        .typography(Type::Caption)
        .text_color(p.muted)
        .child(icon(glyph, p).size(glyph::MARK).flex_shrink_0())
        .children(delivery.map(|label| div().flex_shrink_0().child(format!("{label} ·"))))
        .child(attribution::Label {
            name: "you".into(),
            recorded_at_ms,
        })
}

/// The id a transcript row's height follows under, for handing it on when rows change.
pub(in crate::ui) fn row_motion_id(scope: &str, row: &str) -> String {
    format!("row:{scope}:{row}")
}
