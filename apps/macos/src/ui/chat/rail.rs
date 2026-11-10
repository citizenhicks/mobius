//! The message rail: one tick per user message and pinned answer down the transcript's
//! leading edge, turns told apart by a subtle shade, pins by their Bot's tint and a longer
//! rest. Under the pointer the ticks swell like the Dock and a card shows the message with
//! the opening of its reply; clicking scrolls to it.

use super::*;
use crate::gateway::TranscriptEntry;
use crate::numeric::ToFloat;
use mobius::protocol::{MessageAuthor, ModelStepContentPhase};
use mobius_gateway::wire::{TranscriptItemContent, TranscriptRowKind};
use std::rc::Rc;

/// One user message or pinned answer on the rail.
pub(super) struct Mark {
    id: SharedString,
    item: usize,
    turn: usize,
    title: SharedString,
    reply: SharedString,
    pinned: bool,
    /// Its entry is still arriving, so the tick's accessible name may yet change.
    live: bool,
}

/// A tick at rest, how far the hovered one reaches, and the rail's tallest extent.
const REST: Pixels = px(6.);
const REACH: Pixels = px(28.);
const PITCH: Pixels = px(8.);
const TALLEST: Pixels = px(480.);
/// The card's text column; the card is this plus its padding.
const CARD_TEXT: Pixels = px(320.);

/// The Dock's falloff: the hovered tick at full reach, its neighbours less with distance.
fn tick_width(distance: Option<usize>, pinned: bool) -> Pixels {
    let extra = match distance {
        Some(0) => 1.,
        Some(1) => 0.45,
        Some(2) => 0.18,
        _ => 0.,
    };
    // A pin rests twice as long, so it reads apart from the user's ticks in any accent.
    let rest = if pinned { REST * 2. } else { REST };
    rest + (REACH - rest) * extra
}

/// Plain lines, without blank ones.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty())
}

fn answer<'a>(entries: &[&'a TranscriptEntry], source: &TranscriptEntry) -> Option<&'a str> {
    entries
        .iter()
        .take_while(|entry| !entry.starts_turn)
        .filter(|entry| entry.turn_id == source.turn_id)
        .find_map(|entry| match &entry.content {
            TranscriptItemContent::Assistant { phase, text, .. }
                if *phase != ModelStepContentPhase::Reasoning && !text.trim().is_empty() =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
}

fn reveal_entry(
    entry: AnyElement,
    composer_inset: Pixels,
    revealed: impl Fn(&mut App) + 'static,
) -> Div {
    div()
        .child(entry)
        .on_children_prepainted(move |bounds, window, cx| {
            if let Some(bounds) = bounds.first() {
                // Reveal the opening above the floating composer, even for a very tall answer.
                window.request_autoscroll(Bounds::new(
                    bounds.origin,
                    size(bounds.size.width, Type::Body.line_height() + composer_inset),
                ));
                revealed(cx);
            }
        })
}

impl Workspace {
    /// Opens the owning group, then lets its entry reveal itself after layout.
    fn reveal_rail_item(&mut self, scope: &str, index: usize, id: &str, cx: &mut Context<Self>) {
        self.chat_ui.rail_reveal = Some(Rc::new((scope.to_owned(), id.to_owned())));
        if let Some(ChatItem::Row(row)) = self.chat_ui.items.get(index)
            && let Some(row) = self
                .chat_ui
                .rows
                .get(*row)
                .and_then(|id| self.client.state.transcript.row(id))
            && matches!(
                row.kind,
                TranscriptRowKind::Worked { .. } | TranscriptRowKind::Activity { .. }
            )
        {
            self.expanded.insert(format!("{scope}:{}", row.id));
        }
        self.reveal_chat_item(scope, index, cx);
    }

    pub(in crate::ui) fn with_rail_reveal(
        &self,
        entry: AnyElement,
        scope: &str,
        id: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(reveal) = self
            .chat_ui
            .rail_reveal
            .as_ref()
            .filter(|reveal| reveal.0 == scope && reveal.1 == id)
        else {
            return entry;
        };
        let reveal = Rc::clone(reveal);
        let entity = cx.entity().downgrade();
        reveal_entry(entry, self.composer_inset(), move |cx| {
            let (entity, reveal) = (WeakEntity::clone(&entity), Rc::clone(&reveal));
            cx.defer(move |cx| {
                if let Err(error) = entity.update(cx, |this, cx| {
                    if this.chat_ui.rail_reveal.as_ref() == Some(&reveal) {
                        this.chat_ui.rail_reveal = None;
                        cx.notify();
                    }
                }) {
                    tracing::debug!(%error, "the workspace closed before the rail reveal settled");
                }
            });
        })
        .into_any_element()
    }

    pub(super) fn refresh_rail_marks(&mut self) {
        self.chat_ui.rail_marks = self.rail_marks();
        self.chat_ui.rail_revision = self.client.state.transcript.revision();
    }

    /// Whether text arrived since the marks were built and someone can read the difference:
    /// the card under the pointer, or a tick naming an entry still arriving. Anything else
    /// waits, so streaming does not parse the transcript's messages for every chunk.
    pub(super) fn rail_words_stale(&self) -> bool {
        self.chat_ui.rail_revision != self.client.state.transcript.revision()
            && (self.chat_ui.rail_hover.is_some()
                || self.chat_ui.rail_marks.iter().any(|mark| mark.live))
    }

    pub(super) fn rail_marks(&self) -> Vec<Mark> {
        let entries: Vec<_> = self.client.state.transcript.entries().collect();
        let pins = self
            .client
            .state
            .session
            .as_ref()
            .map(|session| &session.pinned_messages);
        let mut marks = Vec::new();
        let mut turn = 0;
        let mut index = 0;
        for (item, kind) in self.chat_ui.items.iter().enumerate() {
            let ChatItem::Row(row) = kind else { continue };
            let Some(row) = self
                .chat_ui
                .rows
                .get(*row)
                .and_then(|id| self.client.state.transcript.row(id))
            else {
                continue;
            };
            for entry in self.client.state.transcript.row_items(row) {
                index += 1;
                if let TranscriptItemContent::Assistant { text, .. } = &entry.content
                    && entry
                        .target
                        .is_some_and(|target| pins.is_some_and(|pins| pins.contains(&target)))
                {
                    // An answer's first line titles its card; the rest reads under it.
                    let plain = super::reply::plain_text(text);
                    let mut lines = lines(&plain);
                    marks.push(Mark {
                        id: Arc::clone(&entry.id).into(),
                        item,
                        turn,
                        title: lines.next().unwrap_or_default().to_owned().into(),
                        reply: lines.collect::<Vec<_>>().join(" ").into(),
                        pinned: true,
                        live: entry.is_pending(),
                    });
                    continue;
                }
                let TranscriptItemContent::Message { message } = &entry.content else {
                    continue;
                };
                if message.author != MessageAuthor::User || entry.session_sender().is_some() {
                    continue;
                }
                if entry.starts_turn && !marks.is_empty() {
                    turn += 1;
                }
                // The first answer text after the message, as the reference card shows.
                let reply = answer(entries.get(index..).unwrap_or_default(), entry)
                    .map(|text| {
                        lines(&super::reply::plain_text(text))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                marks.push(Mark {
                    id: Arc::clone(&entry.id).into(),
                    item,
                    turn,
                    title: lines(&super::reply::plain_text(&message.text))
                        .collect::<Vec<_>>()
                        .join(" ")
                        .into(),
                    reply: reply.into(),
                    pinned: false,
                    live: entry.is_pending(),
                });
            }
        }
        marks
    }

    /// The rail over the transcript's leading edge, or nothing for a single message.
    pub(in crate::ui) fn message_rail(
        &self,
        scope: &str,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let marks = &self.chat_ui.rail_marks;
        if marks.len() < 2 {
            return None;
        }
        let p = self.palette;
        let pin_tint = self
            .composer_bot()
            .map_or(p.accent, |bot| Accent::from(bot.tint).color());
        let visible = self
            .transcript_scrolls
            .get(scope)
            .map_or(0..0, |state| state.visible.start..state.visible.end);
        let hover = marks
            .iter()
            .position(|mark| self.chat_ui.rail_hover.as_ref() == Some(&mark.id));
        let ticks = marks.iter().enumerate().map(|(index, mark)| {
            let hovered = hover == Some(index);
            let in_view = visible.contains(&mark.item);
            // A pin keeps its Bot's tint under the pointer, so the card's tick stays readable.
            let ink = if mark.pinned {
                pin_tint
            } else if hovered {
                p.text
            } else if mark.turn % 2 == 0 {
                p.muted
            } else {
                p.muted.opacity(alpha::DIM)
            };
            let item = mark.item;
            let id = SharedString::clone(&mark.id);
            let reveal_id = SharedString::clone(&id);
            let scope = scope.to_owned();
            div()
                .id(SharedString::from(format!("rail-tick:{id}")))
                .relative()
                .flex_1()
                .min_h_0()
                .w(REACH + space::S)
                .flex()
                .items_center()
                .cursor_pointer()
                .focusable()
                .role(gpui::accesskit::Role::Button)
                .aria_label(if mark.pinned {
                    format!("Show pinned answer: {}", mark.title)
                } else {
                    format!("Show message: {}", mark.title)
                })
                .child(
                    div()
                        .h(px(2.))
                        .max_h_full()
                        .w(tick_width(
                            hover.map(|hovered| hovered.abs_diff(index)),
                            mark.pinned,
                        ))
                        .pill_corners()
                        .bg(ink)
                        .opacity(if hover.is_some() || in_view || mark.pinned {
                            1.
                        } else {
                            alpha::DIM
                        }),
                )
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered || this.chat_ui.rail_hover.as_ref() == Some(&id) {
                        this.chat_ui.rail_hover = hovered.then(|| SharedString::clone(&id));
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.reveal_rail_item(&scope, item, &reveal_id, cx);
                }))
                .when(hovered, |tick| tick.child(self.rail_card(mark, cx)))
        });
        Some(
            div()
                .absolute()
                .left(space::S)
                .top_0()
                .bottom_0()
                .flex()
                .items_center()
                .child(
                    div()
                        .id("message-rail")
                        .flex()
                        .flex_col()
                        .h((PITCH * marks.len().to_f32()).min(TALLEST))
                        .max_h_full()
                        .min_h_0()
                        .aria_label("Messages in this chat")
                        .children(ticks),
                )
                .into_any_element(),
        )
    }

    /// The hovered message as a card beside its tick: its first line as the title and the
    /// opening of the reply under it, wrapping inside the card.
    fn rail_card(&self, mark: &Mark, cx: &App) -> AnyElement {
        let p = self.palette;
        let card = div()
            .px(space::S)
            .py(space::S + space::XXS)
            .map(|card| backdrop::floating(card, p.card, cx))
            .border_1()
            .border_color(p.separator)
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(space::XS)
            .child(
                div()
                    .w(CARD_TEXT)
                    .typography(Type::Headline)
                    .text_color(p.text)
                    .truncate()
                    .child(SharedString::clone(&mark.title)),
            )
            .when(!mark.reply.is_empty(), |card| {
                card.child(
                    div()
                        .w(CARD_TEXT)
                        .typography(Type::Body)
                        .text_color(p.muted)
                        .whitespace_normal()
                        .line_clamp(3)
                        .text_ellipsis()
                        .child(SharedString::clone(&mark.reply)),
                )
            });
        gpui::anchored()
            .offset(point(REACH + space::S, px(-20.)))
            .snap_to_window_with_margin(space::S)
            .child(card)
            .into_any_element()
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    pub(in crate::ui) fn fixture_hover_rail(&mut self, index: usize, cx: &mut Context<Self>) {
        self.chat_ui.rail_hover = self
            .rail_marks()
            .get(index)
            .map(|mark| SharedString::clone(&mark.id));
        cx.notify();
    }

    /// Pins every third finished answer in local state only, hovers the first pin's tick and
    /// scrolls its answer into view, so its Unpin action shows.
    pub(in crate::ui) fn fixture_pin_answers(&mut self, cx: &mut Context<Self>) {
        let targets = self
            .client
            .state
            .transcript
            .entries()
            .filter(|entry| {
                matches!(&entry.content, TranscriptItemContent::Assistant { phase, text, .. }
                    if *phase != ModelStepContentPhase::Reasoning && !text.trim().is_empty())
            })
            .filter_map(|entry| entry.target)
            .step_by(3)
            .take(4)
            .collect::<Vec<_>>();
        if let Some(session) = &mut self.client.state.session {
            session.pinned_messages.extend(targets);
        }
        let marks = self.rail_marks();
        let first = marks.iter().find(|mark| mark.pinned);
        self.chat_ui.rail_hover = first.map(|mark| SharedString::clone(&mark.id));
        let scope = self
            .client
            .state
            .selected_session_id
            .as_deref()
            .map(|session_id| format!("chat:{session_id}"));
        if let (Some(mark), Some(scope)) = (first, scope) {
            self.reveal_rail_item(&scope, mark.item, &mark.id, cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::answer;
    use crate::gateway::fixture::Script;
    use mobius::protocol::{MessageAuthor, MessageDelivery, MessageEvent, ModelStepContentPhase};

    #[test]
    fn an_unanswered_message_does_not_preview_the_next_turn() {
        let mut script = Script::default();
        for turn in ["unanswered", "answered"] {
            script.turn(turn, true);
            script.message(
                turn,
                MessageEvent {
                    author: MessageAuthor::User,
                    delivery: MessageDelivery::Turn,
                    text: turn.into(),
                    attachments: Vec::new(),
                    reply: None,
                    message_target: None,
                },
            );
        }
        script.text(
            "answered",
            "step",
            ModelStepContentPhase::FinalAnswer,
            "the second answer",
        );
        let transcript = script.store();
        let entries: Vec<_> = transcript.entries().collect();
        assert_eq!(answer(&entries[1..], entries[0]), None);
        assert_eq!(answer(&entries[2..], entries[1]), Some("the second answer"));
    }

    #[cfg(feature = "snapshot")]
    #[gpui::test]
    fn a_pin_inside_a_tall_group_reveals_its_opening_above_the_composer(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::*;
        struct Group(gpui::ListState);
        impl Render for Group {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                gpui::list(self.0.clone(), |_, _, _| {
                    div()
                        .flex()
                        .flex_col()
                        .child(div().h(px(900.)).flex_shrink_0())
                        .child(reveal_entry(
                            div()
                                .h(px(600.))
                                .debug_selector(|| "pinned-answer".into())
                                .into_any_element(),
                            px(100.),
                            |_| {},
                        ))
                        .into_any_element()
                })
                .size_full()
            }
        }
        let list = gpui::ListState::new(1, gpui::ListAlignment::Top, px(0.));
        let cx = cx.add_empty_window();
        cx.draw(point(px(0.), px(0.)), size(px(400.), px(300.)), |_, cx| {
            cx.new(|_| Group(list.clone())).into_any_element()
        });
        let answer = cx.debug_bounds("pinned-answer").unwrap();
        let viewport = list.viewport_bounds();
        assert!(answer.top() >= viewport.top());
        assert!(answer.top() + Type::Body.line_height() <= viewport.bottom() - px(100.));
        assert!(list.logical_scroll_top().offset_in_item > px(0.));
    }
}
