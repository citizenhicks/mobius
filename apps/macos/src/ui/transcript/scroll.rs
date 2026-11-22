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

/// The list id of the row that pages in earlier history.
pub(in crate::ui) const HISTORY: &str = "history";
/// The fastest a sent message travels to the top of the view, in points a second.
const TRAVEL: f32 = 2400.;
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
    /// The glide under way; dropping it stops it where it is.
    glide: Option<gpui::Task<()>>,
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
        let following = list.is_following_tail()
            || (chat
                && self
                    .glide_row()
                    .is_some_and(|row| self.row_rests(scope, row, space::L + header)));
        let has_items = list.item_count() > 0;
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
                                move |this, _, _, cx| {
                                    this.glide_to(Rc::clone(&scope), GlideTarget::Latest, cx);
                                },
                            )),
                        ),
                )
            })
            .into_any_element()
    }

    /// Whether `row` is gliding to, or resting at, `rest` below the top of `scope`'s list.
    fn row_rests(&self, scope: &str, row: &str, rest: Pixels) -> bool {
        let Some(state) = self.transcript_scrolls.get(scope) else {
            return false;
        };
        state.glide.is_some()
            || state.list.as_ref().is_some_and(|list| {
                state
                    .rows
                    .iter()
                    .position(|id| **id == *row)
                    .and_then(|index| list.bounds_for_item(index))
                    .is_some_and(|bounds| {
                        (bounds.top() - list.viewport_bounds().top() - rest).abs() < px(2.)
                    })
            })
    }

    /// Scrolls `scope`'s list to `target` on the shared spring. One glide runs at a time:
    /// a new one replaces it, and the reader's own scrolling ends it where it is. Reduce
    /// Motion goes there at once.
    pub(in crate::ui) fn glide_to(
        &mut self,
        scope: Rc<str>,
        target: GlideTarget,
        cx: &mut Context<Self>,
    ) {
        if matches!(target, GlideTarget::Latest) {
            // The end of the list is its last row, not the room a gliding turn kept below.
            self.release_glide_room();
        }
        let Some(state) = self.transcript_scrolls.get_mut(&*scope) else {
            return;
        };
        state.cancel_history_restore();
        state.glide = None;
        let Some(list) = state.list.as_ref() else {
            return;
        };
        let list = ListState::clone(list);
        if matches!(target, GlideTarget::Row(..)) {
            list.pause_following_tail();
        }
        if cx.reduce_motion() {
            match &target {
                GlideTarget::Latest => list.set_follow_mode(FollowMode::Tail),
                GlideTarget::Row(id, _) => {
                    if let Some(item_ix) = state.rows.iter().position(|row| row == id) {
                        list.scroll_to(ListOffset {
                            item_ix,
                            offset_in_item: px(0.),
                        });
                    }
                }
            }
            cx.notify();
            return;
        }
        state.glide = Some(cx.spawn(async move |this, cx| {
            let key = scope;
            let (mut velocity, mut last) = (0., motion::now());
            loop {
                cx.background_executor().timer(motion::FRAME_60).await;
                let done = this.update(cx, |this, cx| {
                    let Some(state) = this.transcript_scrolls.get(&*key) else {
                        return true;
                    };
                    let Some(list) = state.list.as_ref() else {
                        return true;
                    };
                    let left = match &target {
                        GlideTarget::Latest => f32::from(
                            list.max_offset_for_scrollbar().y
                                + list.scroll_px_offset_for_scrollbar().y,
                        ),
                        GlideTarget::Row(id, rest) => {
                            // By id: rows may arrive above it while the glide runs.
                            let Some(item_ix) = state.rows.iter().position(|row| row == id) else {
                                return true;
                            };
                            let Some(row) = list.bounds_for_item(item_ix) else {
                                // Too far away to be laid out: the list goes to it, and
                                // the glide settles the rest once it is measured.
                                list.scroll_to(ListOffset {
                                    item_ix,
                                    offset_in_item: px(0.),
                                });
                                cx.notify();
                                return false;
                            };
                            f32::from(row.top() - list.viewport_bounds().top() - *rest)
                        }
                    };
                    let now = motion::now();
                    let (mut next, mut speed) =
                        motion::spring::step(left, velocity, 0., now - last);
                    // Most of a screen is a long way for a spring: a row is brought up at
                    // a pace the eye follows, easing in only over the last stretch.
                    let reach = TRAVEL * (now - last).as_secs_f32().min(0.05);
                    if matches!(target, GlideTarget::Row(..)) && (left - next).abs() > reach {
                        (next, speed) = (left - reach.copysign(left), -TRAVEL.copysign(left));
                    }
                    (velocity, last) = (speed, now);
                    list.scroll_by(px(left - next));
                    cx.notify();
                    if next == 0. && matches!(target, GlideTarget::Latest) {
                        list.set_follow_mode(FollowMode::Tail);
                    }
                    next == 0.
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
            if let Err(error) = this.update(cx, |this, cx| {
                if let Some(state) = this.transcript_scrolls.get_mut(&*key) {
                    state.glide = None;
                }
                cx.notify();
            }) {
                tracing::debug!(%error, "the workspace closed before a glide ended");
            }
        }));
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
mod tests {
    use super::{TranscriptScroll, history_destination, splice_chat_rows};
    #[cfg(feature = "snapshot")]
    use gpui::{AppContext as _, Render, point, size};
    use gpui::{FollowMode, ListAlignment, ListOffset, ListState, px};
    use std::sync::Arc;

    #[test]
    fn history_and_tail_changes_preserve_the_current_row_and_pixel_offset() {
        let old = ["history", "a", "b", "c", "working"].map(Arc::from);
        let new = ["history", "older", "a", "b", "c", "answer", "waiting"].map(Arc::from);
        let list = ListState::new(old.len(), ListAlignment::Top, px(0.));
        list.set_follow_mode(FollowMode::Tail);
        list.scroll_to(ListOffset {
            item_ix: 2,
            offset_in_item: px(37.),
        });
        // The reader moves again while the older turn is on its way.
        list.scroll_to(ListOffset {
            item_ix: 3,
            offset_in_item: px(91.),
        });
        splice_chat_rows(&list, &old, &new);
        let top = list.logical_scroll_top();
        assert_eq!(&*new[top.item_ix], "c");
        assert_eq!(top.offset_in_item, px(91.));
        assert!(!list.is_following_tail());
    }

    #[test]
    fn history_insertion_keeps_tail_following_and_an_unchanged_top() {
        let old = ["history", "a", "b", "c"].map(Arc::from);
        let new = ["older", "a", "b", "c"].map(Arc::from);
        let list = ListState::new(old.len(), ListAlignment::Top, px(0.));
        list.set_follow_mode(FollowMode::Tail);
        splice_chat_rows(&list, &old, &new);
        assert!(list.is_following_tail());
        list.scroll_to(ListOffset {
            item_ix: 2,
            offset_in_item: px(22.),
        });
        splice_chat_rows(&list, &new, &new);
        assert_eq!(list.logical_scroll_top().item_ix, 2);
        assert_eq!(list.logical_scroll_top().offset_in_item, px(22.));
        assert!(!list.is_following_tail());
    }

    #[test]
    fn a_deferred_history_restore_yields_to_a_newer_scroll() {
        let placed = ListOffset {
            item_ix: 2,
            offset_in_item: px(30.),
        };
        let pending = || {
            let list = ListState::new(4, ListAlignment::Top, px(1000.));
            list.scroll_to(placed);
            TranscriptScroll {
                settling: Some((placed, px(40.))),
                list: Some(list),
                ..TranscriptScroll::default()
            }
        };
        let offset = |state: &TranscriptScroll| {
            state
                .list
                .as_ref()
                .unwrap()
                .logical_scroll_top()
                .offset_in_item
        };

        let mut untouched = pending();
        untouched.settle_history_restore();
        assert_ne!(
            offset(&untouched),
            px(30.),
            "nothing cancelled it, so it applies"
        );
        assert!(untouched.settling.is_none());

        // Return to latest waits before its first move, so the position has not changed yet.
        let mut gliding = pending();
        gliding.cancel_history_restore();
        gliding.settle_history_restore();
        assert_eq!(offset(&gliding), px(30.));

        // A move that bypasses the scroll handler, such as a rail jump, also wins.
        let mut moved = pending();
        let jump = ListOffset {
            item_ix: 1,
            offset_in_item: px(30.),
        };
        moved.list.as_ref().unwrap().scroll_to(jump);
        moved.settle_history_restore();
        assert_eq!(moved.list.as_ref().unwrap().logical_scroll_top().item_ix, 1);
        assert_eq!(offset(&moved), px(30.));
    }

    #[cfg(feature = "snapshot")]
    #[gpui::test]
    fn late_history_heights_preserve_the_readers_latest_position(cx: &mut gpui::TestAppContext) {
        use super::*;

        fn draw(cx: &mut gpui::VisualTestContext, list: &ListState, heights: &[Pixels]) {
            struct Rows {
                list: ListState,
                heights: Rc<[Pixels]>,
            }
            impl Render for Rows {
                fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                    let heights = Rc::clone(&self.heights);
                    gpui::list(ListState::clone(&self.list), move |index, _, _| {
                        div().h(heights[index]).w_full().into_any_element()
                    })
                    .size_full()
                }
            }
            cx.draw(point(px(0.), px(0.)), size(px(400.), px(300.)), |_, cx| {
                cx.new(|_| Rows {
                    list: ListState::clone(list),
                    heights: Rc::from(heights),
                })
                .into_any_element()
            });
        }

        let old = ["history", "a", "b", "c", "d", "working"].map(Arc::from);
        let new = ["history", "older", "a", "b", "c", "d", "answer"].map(Arc::from);
        let list = ListState::new(old.len(), ListAlignment::Top, px(1000.));
        list.set_follow_mode(FollowMode::Tail);
        list.scroll_to(ListOffset {
            item_ix: 2,
            offset_in_item: px(70.),
        });
        let cx = cx.add_empty_window();
        draw(cx, &list, &vec![px(400.); old.len()]);
        // A pending Markdown/image remeasure must not undo scrolling while history loads.
        list.remeasure_items(1..4);
        list.scroll_to(ListOffset {
            item_ix: 3,
            offset_in_item: px(95.),
        });
        splice_chat_rows(&list, &old, &new);
        let mut heights = vec![px(400.); new.len()];
        draw(cx, &list, &heights);
        let top = list.logical_scroll_top();
        assert_eq!(&*new[top.item_ix], "c");
        assert_eq!(top.offset_in_item, px(95.));
        let before = list.bounds_for_item(top.item_ix).unwrap().top();
        heights[1] = px(900.);
        heights[top.item_ix] = px(800.);
        list.remeasure_items(1..top.item_ix + 1);
        draw(cx, &list, &heights);
        assert_eq!(list.bounds_for_item(top.item_ix).unwrap().top(), before);
        assert_eq!(list.logical_scroll_top().offset_in_item, px(95.));
        assert!(!list.is_following_tail());
    }

    #[test]
    fn incoming_output_does_not_consume_a_pending_history_restore() {
        let appended = ["history", "first", "new"].map(Arc::from);
        assert_eq!(history_destination(&appended, "first", 1), None);
        let prepended = ["history", "older", "first", "new"].map(Arc::from);
        assert_eq!(history_destination(&prepended, "first", 1), Some(2));
        let exhausted = ["first", "new"].map(Arc::from);
        assert_eq!(history_destination(&exhausted, "first", 1), Some(0));
    }
}
