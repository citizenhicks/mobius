use crate::ui::Workspace;
use crate::ui::primitives::{composer_button, height, motion, space};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, IntoElement, ParentElement as _, Pixels, SharedString, Styled as _,
    Window, div, px,
};
use gpui::{FollowMode, ListAlignment, ListOffset, ListState};

use std::collections::BTreeSet;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

struct GlideFrame {
    entity: gpui::WeakEntity<Workspace>,
    scope: Rc<str>,
    glide_id: u64,
    target: GlideTarget,
    still: bool,
    velocity: f32,
    last: std::time::Instant,
}

/// The list id of the row that pages in earlier history.
pub(in crate::ui) const HISTORY: &str = "history";
/// The fastest a sent message travels to the top of the view, in points a second.
const TRAVEL: f32 = 2400.;
/// How long a list goes undrawn before its glide gives up.
const UNSEEN: std::time::Duration = std::time::Duration::from_millis(250);
/// What a chat's transcript scope starts with; documents use their tab's id.
const CHAT_SCOPE: &str = "chat:";

/// One transcript's virtualized list: only rows near the viewport are laid out, so a
/// transcript can hold its whole history.
#[derive(Default)]
pub(in crate::ui) struct TranscriptScroll {
    pub(in crate::ui) rows: Vec<Arc<str>>,
    animated: BTreeSet<Arc<str>>,
    /// The row to bring back when a page requested from its Load more button arrives above
    /// it, with how far below the view's top it was.
    history_anchor: Option<(Arc<str>, usize, Pixels)>,
    /// The last step of a restore, waiting a frame: where the row was placed, and how far to
    /// move it. Any newer scroll cancels it, even one that has not moved the list yet.
    settling: Option<(ListOffset, Pixels)>,
    list: Option<ListState>,
    /// Monotonic token for the active vsync glide; clearing or replacing it stops the glide.
    glide: Option<u64>,
    next_glide_id: u64,
    /// The message a gliding turn rests at the top, and the room kept below the last row
    /// so it can: a screen of its own for the answer to fill.
    room: Option<(Arc<str>, Pixels)>,
    /// When the list was last drawn; a glide ends once it no longer is.
    seen: std::cell::Cell<Option<std::time::Instant>>,
    /// The list items in view, for the message rail.
    pub(in crate::ui) visible: Range<usize>,
}

impl TranscriptScroll {
    /// The reader chose a position, which outranks any restore still under way.
    fn cancel_history_restore(&mut self) {
        self.history_anchor = None;
        self.settling = None;
    }

    /// Finishes a restore that nothing cancelled, unless the list moved since it was placed.
    fn settle_history_restore(&mut self) {
        if let Some((placed, below)) = self.settling.take()
            && let Some(list) = &self.list
        {
            let top = list.logical_scroll_top();
            if top.item_ix == placed.item_ix && top.offset_in_item == placed.offset_in_item {
                list.scroll_by(-below);
            }
        }
    }
}

impl Workspace {
    /// The reader asked for an earlier page from a document's Load more row, which a page
    /// arrives below: remembers the row after it, for `prepare_chat_list` to hold in place.
    pub(in crate::ui) fn begin_history_restore(&mut self, scope: &str) {
        let Some(state) = self.transcript_scrolls.get_mut(scope) else {
            return;
        };
        let Some(list) = &state.list else {
            return;
        };
        let top = list.logical_scroll_top();
        let on_more = top.item_ix == 0 && state.rows.first().is_some_and(|id| &**id == HISTORY);
        let index = if on_more { 1 } else { top.item_ix };
        // A row further down keeps its place through the splice on its own.
        let below = list
            .bounds_for_item(0)
            .zip(list.bounds_for_item(1))
            .filter(|_| on_more)
            .map_or(px(0.), |(more, row)| {
                row.top() - more.top() - top.offset_in_item
            });
        state.history_anchor = state
            .rows
            .get(index)
            .map(|id| (Arc::clone(id), index, below));
        list.pause_following_tail();
    }

    pub(super) fn animate_transcript_row(&self, scope: &str, row_id: &str) -> bool {
        self.transcript_scrolls
            .get(scope)
            .is_some_and(|s| s.animated.contains(row_id))
    }

    /// Scrolls `scope` so a probed element's top sits at the middle of the viewport.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn fixture_center_transcript(&mut self, scope: &str, top: Pixels) {
        self.stop_following_transcript(scope);
        if let Some(list) = self.scope_list(scope) {
            let viewport = list.viewport_bounds();
            list.scroll_by(top - (viewport.top() + viewport.size.height / 2.));
        }
    }

    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn scope_list(&self, scope: &str) -> Option<&ListState> {
        self.transcript_scrolls.get(scope)?.list.as_ref()
    }

    pub(super) fn stop_following_transcript(&mut self, scope: &str) {
        if let Some(state) = self.transcript_scrolls.get_mut(scope)
            && let Some(list) = &state.list
        {
            list.pause_following_tail();
        }
    }

    /// Keeps the list in step with `ids`, preserving the row under the reader.
    /// New rows rise in, except history arriving above the old top.
    pub(in crate::ui) fn prepare_chat_list(
        &mut self,
        scope: &str,
        ids: Vec<Arc<str>>,
        animate: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> ListState {
        let entity = cx.entity().downgrade();
        let state = self.transcript_scrolls.entry(scope.to_owned()).or_default();
        let list = ListState::clone(state.list.get_or_insert_with(|| {
            let list = ListState::new(0, ListAlignment::Top, px(1000.));
            list.set_follow_mode(FollowMode::Tail);
            let scope = scope.to_owned();
            list.set_scroll_handler(move |event, _, cx| {
                let range = event.visible_range.start..event.visible_range.end;
                // Only the chat pages in by scrolling; documents have a Load more button.
                let load_history = scope.starts_with(CHAT_SCOPE)
                    && event.is_scrolled
                    && !event.is_following_tail
                    && range.start <= 1;
                let updated = entity.update(cx, |this, cx| {
                    if let Some(state) = this.transcript_scrolls.get_mut(&scope) {
                        // The reader moved on from the row a page was going to restore,
                        // and from wherever a glide was taking them.
                        state.cancel_history_restore();
                        state.glide = None;
                        if state.visible != range {
                            state.visible = range;
                            cx.notify();
                        }
                    }
                    if load_history {
                        this.continue_history(cx);
                    }
                });
                if let Err(error) = updated {
                    tracing::debug!(%error, "chat list scroll on a released workspace");
                }
            });
            list
        }));
        if ids != state.rows {
            let old = &state.rows;
            let (removed, inserted) = splice_chat_rows(&list, old, &ids);
            let next = ids.get(inserted.start);
            let removed = old.get(removed).unwrap_or_default();
            let inserted = ids.get(inserted).unwrap_or_default();
            hand_off_rows(scope, removed, inserted, next);
            if animate && !old.is_empty() {
                let previous: std::collections::HashSet<_> = old.iter().collect();
                // Earlier turns land above the reader at their full height: only rows
                // after the first one that was already here arrive in motion.
                let known = ids.iter().position(|id| previous.contains(id)).unwrap_or(0);
                state.animated.extend(
                    ids.iter()
                        .skip(known)
                        .filter(|id| &***id != HISTORY && !previous.contains(id))
                        .map(Arc::clone),
                );
            }
            let current: std::collections::HashSet<_> = ids.iter().collect();
            state.animated.retain(|id| current.contains(id));
            if let Some((anchor, old_index, below)) = &state.history_anchor
                && let Some(index) = history_destination(&ids, anchor, *old_index)
            {
                state.settling = place_history_anchor(&list, index, *below, scope, window, cx);
                state.history_anchor = None;
            }
            state.rows = ids;
        }
        list
    }

    /// Scrolls the chat list so item `index` starts at the top of the view.
    pub(in crate::ui) fn reveal_chat_item(
        &mut self,
        scope: &str,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.stop_following_transcript(scope);
        if let Some(list) = self
            .transcript_scrolls
            .get(scope)
            .and_then(|s| s.list.as_ref())
        {
            list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    /// A transcript: a virtualized list whose items `render` draws on demand, under the
    /// chat's floating composer, with the return-to-latest button when scrolled up.
    pub(in crate::ui) fn chat_list(
        &self,
        scope: &str,
        list: ListState,
        overlay: Option<AnyElement>,
        mut render: impl FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        // GPUI's list lays items out at its full width and ignores horizontal padding,
        // so each item carries the gutter.
        let render = move |index, window: &mut Window, cx: &mut App| {
            div()
                .px(space::L)
                .child(render(index, window, cx))
                .into_any_element()
        };
        // The chat's composer floats over its transcript's foot, under its Bot's header.
        let chat = scope.starts_with(CHAT_SCOPE);
        let inset = if chat { self.composer_inset() } else { px(0.) };
        let header = if chat && self.active_conversation_bot().is_some() {
            height::BOT_CHAT_HEADER
        } else {
            px(0.)
        };
        // A message resting at the top with its answer filling in below is at the latest.
        let following = list.is_following_tail() || self.glide_rests(scope, space::L + header);
        let has_items = list.item_count() > 0;
        if let Some(state) = self.transcript_scrolls.get(scope) {
            state.seen.set(Some(motion::now()));
        }
        let scope: Rc<str> = scope.into();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .relative()
            .child(
                gpui::list(list, render)
                    .text_color(self.palette.ink)
                    .flex_1()
                    .min_h_0()
                    .pt(space::L + header)
                    .pb(space::L + inset),
            )
            .children(overlay)
            .when(!following && has_items, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom(space::S + inset)
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            composer_button(
                                SharedString::from(format!("tail:{scope}")),
                                "Return to latest message",
                                "hi.arrowDown",
                                cx,
                            )
                            .bg(self.palette.card)
                            .border_1()
                            .border_color(self.palette.separator)
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.glide_to(
                                        Rc::clone(&scope),
                                        GlideTarget::Latest,
                                        window,
                                        cx,
                                    );
                                },
                            )),
                        ),
                )
            })
            .into_any_element()
    }

    /// The room `scope`'s gliding turn keeps below its last row.
    pub(in crate::ui) fn glide_room(&self, scope: &str) -> Pixels {
        self.transcript_scrolls
            .get(scope)
            .and_then(|state| state.room.as_ref())
            .map_or(px(0.), |(_, room)| *room)
    }

    /// The item `scope`'s gliding turn rests at the top, and the room kept for it.
    pub(in crate::ui) fn glide_turn(&self, scope: &str) -> Option<(usize, Pixels)> {
        let state = self.transcript_scrolls.get(scope)?;
        let (row, room) = state.room.as_ref()?;
        Some((state.rows.iter().position(|id| id == row)?, *room))
    }

    /// Keeps `room` below `scope`'s last row so `row` can rest at the top.
    pub(in crate::ui) fn keep_glide_room(&mut self, scope: &str, row: Arc<str>, room: Pixels) {
        if let Some(state) = self.transcript_scrolls.get_mut(scope) {
            state.room = Some((row, room));
        }
    }

    /// Gives back the room `scope`'s gliding turn kept; whether there was any.
    pub(in crate::ui) fn release_glide_room(&mut self, scope: &str) -> bool {
        self.transcript_scrolls
            .get_mut(scope)
            .is_some_and(|state| state.room.take().is_some())
    }

    /// Whether `scope`'s gliding turn is on its way to the top or resting there.
    fn glide_rests(&self, scope: &str, rest: Pixels) -> bool {
        let Some(state) = self.transcript_scrolls.get(scope) else {
            return false;
        };
        let (Some(_), Some(list)) = (&state.room, &state.list) else {
            return false;
        };
        state.glide.is_some()
            || self
                .glide_turn(scope)
                .is_some_and(|(index, _)| rests(list, index, rest))
    }

    /// Scrolls `scope`'s list to `target` on the shared spring. One glide runs at a time:
    /// a new one replaces it, and the reader's own scrolling ends it where it is. Reduce
    /// Motion goes there at once.
    pub(in crate::ui) fn glide_to(
        &mut self,
        scope: Rc<str>,
        target: GlideTarget,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.transcript_scrolls.get_mut(&*scope) else {
            return;
        };
        state.cancel_history_restore();
        state.glide = None;
        if matches!(target, GlideTarget::Latest) {
            // The end of the list is its last row, not the room a gliding turn kept below.
            state.room = None;
        }
        let Some(list) = state.list.as_ref() else {
            return;
        };
        let list = ListState::clone(list);
        if matches!(target, GlideTarget::Row(..)) {
            list.pause_following_tail();
        }
        // Reduce Motion covers the whole distance in one step of the same glide, so it
        // lands where the animated one does.
        let still = cx.reduce_motion();
        if still && matches!(target, GlideTarget::Latest) {
            list.set_follow_mode(FollowMode::Tail);
            cx.notify();
            return;
        }
        state.next_glide_id = state.next_glide_id.wrapping_add(1);
        let glide_id = state.next_glide_id;
        state.glide = Some(glide_id);
        let frame = GlideFrame {
            entity: cx.entity().downgrade(),
            scope,
            glide_id,
            target,
            still,
            velocity: 0.,
            last: motion::now(),
        };
        cx.notify();
        Self::schedule_glide_frame(frame, window);
    }

    fn schedule_glide_frame(mut frame: GlideFrame, window: &Window) {
        window.on_next_frame(move |window, cx| {
            let keep_going = frame
                .entity
                .update(cx, |this, cx| {
                    let Some(state) = this.transcript_scrolls.get_mut(&*frame.scope) else {
                        return false;
                    };
                    if state.glide != Some(frame.glide_id) {
                        return false;
                    }
                    let Some(list) = state.list.as_ref() else {
                        state.glide = None;
                        return false;
                    };
                    // Its chat left the screen: a list that is not drawn never arrives.
                    if state
                        .seen
                        .get()
                        .is_some_and(|seen| motion::now() - seen > UNSEEN)
                    {
                        state.glide = None;
                        cx.notify();
                        return false;
                    }
                    let left = match &frame.target {
                        GlideTarget::Latest => f32::from(
                            list.max_offset_for_scrollbar().y
                                + list.scroll_px_offset_for_scrollbar().y,
                        ),
                        GlideTarget::Row(id, rest) => {
                            // By id: rows may arrive above it while the glide runs.
                            let Some(item_ix) = state.rows.iter().position(|row| row == id) else {
                                state.glide = None;
                                cx.notify();
                                return false;
                            };
                            let Some(left) = left_to_rest(list, item_ix, *rest) else {
                                // Too far away to be laid out: the list goes to it, and
                                // the glide settles the rest once it is measured.
                                list.scroll_to(ListOffset {
                                    item_ix,
                                    offset_in_item: px(0.),
                                });
                                frame.last = motion::now();
                                cx.notify();
                                return true;
                            };
                            f32::from(left)
                        }
                    };
                    let now = motion::now();
                    let (mut next, mut speed) =
                        motion::spring::step(left, frame.velocity, 0., now - frame.last);
                    // Most of a screen is a long way for a spring: a row is brought up at
                    // a pace the eye follows, easing in only over the last stretch.
                    let reach = TRAVEL * (now - frame.last).as_secs_f32().min(0.05);
                    if matches!(frame.target, GlideTarget::Row(..)) && (left - next).abs() > reach {
                        (next, speed) = (left - reach.copysign(left), -TRAVEL.copysign(left));
                    }
                    if frame.still {
                        (next, speed) = (0., 0.);
                    }
                    frame.velocity = speed;
                    frame.last = now;
                    list.scroll_by(px(left - next));
                    cx.notify();
                    if next == 0. {
                        if matches!(frame.target, GlideTarget::Latest) {
                            list.set_follow_mode(FollowMode::Tail);
                        }
                        state.glide = None;
                        return false;
                    }
                    true
                })
                .unwrap_or(false);
            if keep_going {
                Self::schedule_glide_frame(frame, window);
            }
        });
    }

    /// Scrolls the chat list so the first row whose id ends with `suffix` starts `offset`
    /// above the top of the view.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn fixture_reveal_row(
        &mut self,
        scope: &str,
        suffix: &str,
        offset: Pixels,
        cx: &mut Context<Self>,
    ) -> bool {
        self.stop_following_transcript(scope);
        let Some(state) = self.transcript_scrolls.get(scope) else {
            return false;
        };
        let (Some(row), Some(list)) = (
            state.rows.iter().position(|id| id.ends_with(suffix)),
            state.list.as_ref(),
        ) else {
            return false;
        };
        list.scroll_to(ListOffset {
            item_ix: row,
            offset_in_item: offset,
        });
        cx.notify();
        true
    }

    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn fixture_scroll_transcript(&mut self, scope: &str, offset: Pixels) {
        self.stop_following_transcript(scope);
        if let Some(list) = self.scope_list(scope) {
            list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: -offset,
            });
        }
    }
}

/// How far the list still has to scroll for item `index` to rest `rest` below the top of
/// the view, kept to what the list can scroll: a row at the head of a short transcript
/// rests where the list's start leaves it. `None` while the item is not laid out.
fn left_to_rest(list: &ListState, index: usize, rest: Pixels) -> Option<Pixels> {
    let row = list.bounds_for_item(index)?;
    let back = list.scroll_px_offset_for_scrollbar().y;
    let ahead = list.max_offset_for_scrollbar().y + back;
    Some((row.top() - list.viewport_bounds().top() - rest).clamp(back, ahead.max(back)))
}

/// Whether item `index` rests where a glide leaves it.
pub(in crate::ui) fn rests(list: &ListState, index: usize, rest: Pixels) -> bool {
    left_to_rest(list, index, rest).is_some_and(|left| left.abs() < px(2.))
}

/// Where a glide of a transcript's list ends.
pub(in crate::ui) enum GlideTarget {
    /// The end of the list, which is then followed again.
    Latest,
    /// The row with this id, resting this far below the top of the view.
    Row(Arc<str>, Pixels),
}

/// How the transcript treats a turn the reader starts (Settings → Appearance).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::ui) enum TurnMotion {
    /// The message glides to the top at once and the answer fills in below it; the end
    /// is followed again once the screen is full.
    Glide,
    /// The end stays in view throughout.
    #[default]
    #[serde(other)]
    Follow,
}

impl TurnMotion {
    pub const ALL: [Self; 2] = [Self::Follow, Self::Glide];

    pub fn label(self) -> &'static str {
        match self {
            Self::Follow => "Follow",
            Self::Glide => "Glide to top",
        }
    }

    /// The recording harness picks the motion of each run.
    #[cfg(feature = "snapshot")]
    pub fn or_harness(self) -> Self {
        match std::env::var("MOBIUS_TRANSCRIPT_MOTION").as_deref() {
            Ok("glide") => Self::Glide,
            Ok("follow") => Self::Follow,
            _ => self,
        }
    }
}

fn splice_chat_rows(
    list: &ListState,
    old: &[Arc<str>],
    new: &[Arc<str>],
) -> (Range<usize>, Range<usize>) {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let tail = old
        .get(head..)
        .unwrap_or_default()
        .iter()
        .rev()
        .zip(new.get(head..).unwrap_or_default().iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = head..old.len() - tail;
    let inserted = head..new.len() - tail;
    let offset = list.logical_scroll_top();
    // A prepend and a changing tail can put a surviving row inside the replaced range.
    let anchor = if !list.is_following_tail() && removed.contains(&offset.item_ix) {
        old.get(offset.item_ix)
            .and_then(|anchor| new.iter().position(|id| id == anchor))
            .map(|item_ix| ListOffset { item_ix, ..offset })
    } else {
        None
    };
    list.splice(removed.start..removed.end, inserted.len());
    if let Some(anchor) = anchor {
        list.scroll_to(anchor);
    }
    (removed, inserted)
}

fn history_destination(rows: &[Arc<str>], anchor: &str, old_index: usize) -> Option<usize> {
    rows.iter()
        .position(|id| &**id == anchor)
        .filter(|index| *index != old_index)
}

/// Puts the row at `index` back where the reader left it, `below` under the view's top. The
/// rows above it have no heights until they are laid out, so the last step waits a frame.
fn place_history_anchor(
    list: &ListState,
    index: usize,
    below: Pixels,
    scope: &str,
    window: &Window,
    cx: &Context<Workspace>,
) -> Option<(ListOffset, Pixels)> {
    list.scroll_to(ListOffset {
        item_ix: index,
        offset_in_item: px(0.),
    });
    (below > px(0.)).then(|| {
        let scope = scope.to_owned();
        let entity = cx.entity().downgrade();
        window.on_next_frame(move |_, cx| {
            let settled = entity.update(cx, |this, cx| {
                if let Some(state) = this.transcript_scrolls.get_mut(&scope) {
                    state.settle_history_restore();
                }
                cx.notify();
            });
            if let Err(error) = settled {
                tracing::debug!(%error, "the workspace closed before a history page settled");
            }
        });
        (list.logical_scroll_top(), below)
    })
}

/// Passes the height of rows that left to what took their place, so the list never jumps:
/// a turn's steps fold into the Worked for body under its new header, a replaced row hands
/// its height to its replacement, and a removed row's space closes beneath the row after it.
fn hand_off_rows(
    scope: &str,
    removed: &[Arc<str>],
    inserted: &[Arc<str>],
    next: Option<&Arc<str>>,
) {
    if removed.is_empty() {
        return;
    }
    let from: Vec<String> = removed
        .iter()
        .map(|id| super::row_motion_id(scope, id))
        .collect();
    let from = || from.iter().map(String::as_str);
    match inserted {
        [worked] if worked.starts_with("worked:") => {
            // The steps close under the new header, inside an item that starts at their height.
            let header = f32::from(height::REGULAR + space::S);
            motion::hand_off(from(), format!("{scope}:{worked}:body").into(), header);
            motion::hand_off(from(), super::row_motion_id(scope, worked).into(), 0.);
        }
        [first, ..] => motion::hand_off(from(), super::row_motion_id(scope, first).into(), 0.),
        [] => {
            // Nothing follows a removed tail, so its space closes at the list's end.
            let to = next.map_or_else(
                || super::tail_slack_id(scope),
                |next| super::row_motion_id(scope, next),
            );
            motion::hand_off(from(), to.into(), 0.);
        }
    }
}

#[cfg(test)]
mod tests;
