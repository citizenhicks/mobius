//! The card grid a page of sections opens onto, and the zoom between a card and its page
//! (`docs/DESIGN.md`, Cards). Settings and a provider share it.

use super::controls::{palette, status_dot};
use super::motion::{SMOOTH_FPS, now, sampled, spring};
use super::tokens::{alpha, glyph, inset, radius, space};
use super::{Handler, Type, Typography, heading, icon};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, BorderStyle, Bounds, ClickEvent, Div, Hsla, Motion, MotionExt as _,
    MotionFrame, Pixels, Role, SharedString, Window, canvas, div, px, quad, size,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Cards across the page column.
const COLUMNS: usize = 3;
/// The spring rests within half a unit, so a flight's progress is solved over a thousand.
const TRAVEL: f32 = 1000.;

/// Where each card was last painted, by card id, so its page can zoom out of it.
pub type CardRects = Rc<RefCell<HashMap<SharedString, Bounds<Pixels>>>>;

/// What leads a card's summary.
pub enum CardMark {
    /// A status dot in a tone.
    Dot(Hsla),
    /// The section's last test.
    Tested { passed: bool },
}

/// One section of a page: its glyph in the accent, its title, and a two-line summary.
#[derive(IntoElement)]
#[must_use]
pub struct Card {
    id: SharedString,
    title: SharedString,
    glyph: SharedString,
    summary: SharedString,
    mark: Option<CardMark>,
    disabled: bool,
    rects: Option<CardRects>,
    on_click: Option<Handler<ClickEvent>>,
}

impl Card {
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        glyph: impl Into<SharedString>,
        summary: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            glyph: glyph.into(),
            summary: summary.into(),
            mark: None,
            disabled: false,
            rects: None,
            on_click: None,
        }
    }

    pub fn mark(mut self, mark: Option<CardMark>) -> Self {
        self.mark = mark;
        self
    }

    /// Stepped back and not clickable: a section this page does not have.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Records where the card paints, for the zoom into its page.
    pub fn rects(mut self, rects: &CardRects) -> Self {
        self.rects = Some(Rc::clone(rects));
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Card {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let mark = self.mark.map(|mark| match mark {
            CardMark::Dot(tone) => status_dot(tone).into_any_element(),
            CardMark::Tested { passed } => {
                let (name, tone) = if passed {
                    ("hi.checkCircle", p.signal)
                } else {
                    ("hi.warning", p.danger)
                };
                icon(name, &p)
                    .size(glyph::INLINE)
                    .flex_shrink_0()
                    .text_color(tone)
                    .into_any_element()
            }
        });
        let label = SharedString::from(format!("{} · {}", self.title, self.summary));
        let id = SharedString::clone(&self.id);
        let card = crate::ui::backdrop::surface(div().id(self.id).relative(), p.card, cx)
            .size_full()
            .flex()
            .flex_col()
            .gap(space::L)
            .p(inset::CARD)
            .border_1()
            .border_color(p.separator)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::S)
                    .child(
                        icon(&self.glyph, &p)
                            .size(glyph::HERO)
                            .flex_shrink_0()
                            .text_color(p.accent),
                    )
                    .child(heading(self.title).min_w_0().truncate()),
            )
            .child(
                div()
                    .h(Type::Caption.line_height() * 2.)
                    .flex()
                    .items_start()
                    .gap(space::XS)
                    .typography(Type::Caption)
                    .text_color(p.muted)
                    .children(mark)
                    .child(div().flex_1().min_w_0().line_clamp(2).child(self.summary)),
            )
            .when_some(self.rects, |card, rects| {
                let id = SharedString::clone(&id);
                card.child(
                    canvas(
                        move |bounds, _, _| {
                            rects.borrow_mut().insert(id, bounds);
                        },
                        |_, (), _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
            });
        #[cfg(feature = "snapshot")]
        let card = card.child(crate::ui::snapshot::probe(format!("card:{id}")));
        if self.disabled {
            return card.opacity(alpha::DIM);
        }
        card.when_some(self.on_click, |card, handler| {
            card.role(Role::Button)
                .aria_label(label)
                .focusable()
                .tab_stop(true)
                .hover(|style| style.bg(p.hover))
                .focus_visible(|style| style.bg(p.accent_soft).border_color(p.accent))
                .on_click(move |event, window, cx| handler(event, window, cx))
        })
    }
}

/// Cards in rows of three, every card the same size.
#[derive(IntoElement, Default)]
pub struct CardGrid {
    cards: Vec<Card>,
}

impl CardGrid {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn card(mut self, card: Card) -> Self {
        self.cards.push(card);
        self
    }
}

impl RenderOnce for CardGrid {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let mut grid = div().flex().flex_col().gap(space::S);
        let mut cards = self.cards.into_iter();
        while cards.len() > 0 {
            let mut row = div().flex().gap(space::S);
            for _ in 0..COLUMNS {
                row = row.child(div().flex_1().min_w_0().children(cards.next()));
            }
            grid = grid.child(row);
        }
        grid
    }
}

/// A page growing out of its card, or shrinking back into it. The card's surface travels
/// between the card's rect and the pane's while it dissolves, the page fades in at rest
/// and the grid fades out beneath them, all on the one spring. The page itself is not
/// scaled: clipping stays where layout put it, so scaled content would be cut.
pub struct CardZoom {
    /// The card's id in the grid.
    pub card: SharedString,
    pub opening: bool,
    pub since: Instant,
}

impl CardZoom {
    pub fn duration() -> Duration {
        spring::settling_time(0., 0., TRAVEL)
    }

    pub fn landed(&self) -> bool {
        now() >= self.since + Self::duration()
    }

    /// How much of the page shows at `at`: 0 at the card, 1 at the page.
    fn shown(opening: bool, since: Instant, at: Instant) -> f32 {
        let elapsed = sampled(at).saturating_duration_since(since);
        let (value, _) = spring::at(0., 0., TRAVEL, elapsed);
        let shown = value / TRAVEL;
        if opening { shown } else { 1. - shown }
    }

    /// The page's opacity in flight, or the grid's beneath it.
    fn opacity(&self, page: bool) -> Motion {
        let (opening, since) = (self.opening, self.since);
        Motion::opacity(move |at| {
            let shown = Self::shown(opening, since, at);
            if page { shown } else { 1. - shown }
        })
        .until(since + Self::duration())
        .max_fps(SMOOTH_FPS)
    }

    /// `grid` beneath `page` while the card's surface travels between them. Neither takes
    /// clicks in flight; both are laid out as they will rest.
    pub fn layers(&self, grid: AnyElement, page: AnyElement, rects: &CardRects, cx: &App) -> Div {
        let p = palette(cx);
        let (opening, since) = (self.opening, self.since);
        let end = since + Self::duration();
        let (card, rects) = (SharedString::clone(&self.card), Rc::clone(rects));
        // Painted after the grid, so the card's rect is this frame's.
        let surface = canvas(
            |_, _, _| {},
            move |pane, (), window, _| {
                let Some(card) = rects.borrow().get(&card).copied() else {
                    return;
                };
                if card.size.width <= px(0.) || card.size.height <= px(0.) {
                    return;
                }
                let grow = size(
                    pane.size.width / card.size.width - 1.,
                    pane.size.height / card.size.height - 1.,
                );
                let travel = pane.origin - card.origin;
                let motion = Motion::new(move |at| {
                    let shown = Self::shown(opening, since, at);
                    MotionFrame {
                        offset: travel * shown,
                        scale: size(1. + grow.width * shown, 1. + grow.height * shown),
                        opacity: 1. - shown,
                        ..MotionFrame::default()
                    }
                })
                .until(end)
                .max_fps(SMOOTH_FPS);
                window.with_motion(motion, card.origin, |window| {
                    window.paint_quad(quad(
                        card,
                        radius::TILE,
                        p.card,
                        inset::HAIRLINE,
                        p.separator,
                        BorderStyle::Solid,
                    ));
                });
            },
        );
        let layer = || div().absolute().inset_0().flex().flex_col();
        div()
            .relative()
            .size_full()
            .min_h_0()
            .child(layer().child(grid).with_motion(self.opacity(false)))
            .child(surface.absolute().inset_0())
            .child(layer().child(page).with_motion(self.opacity(true)))
            .child(div().absolute().inset_0().occlude())
    }
}
