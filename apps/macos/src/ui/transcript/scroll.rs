use super::*;
use gpui::{FollowMode, ListAlignment, ListOffset, ListState};
use std::collections::BTreeSet;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

/// The list id of the row that pages in earlier history.
pub(in crate::ui) const HISTORY: &str = "history";
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
    fn scope_list(&self, scope: &str) -> Option<&ListState> {
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
                        // The reader moved on from the row a page was going to restore.
                        state.cancel_history_restore();
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
            if animate && !old.is_empty() && removed.is_empty() {
                let previous: std::collections::HashSet<_> = old.iter().collect();
                // Rows that arrive above the old first row are history, already read.
                let top = old
                    .iter()
                    .find(|id| &***id != HISTORY)
                    .and_then(|first| ids.iter().position(|id| id == first))
                    .unwrap_or(0);
                state.animated.extend(
                    ids.iter()
                        .enumerate()
                        .filter(|(index, id)| *index >= top && !previous.contains(id))
                        .map(|(_, id)| Arc::clone(id)),
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
        let following = list.is_following_tail();
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
                                move |this, _, _, cx| this.glide_to_latest(Rc::clone(&scope), cx),
                            )),
                        ),
                )
            })
            .into_any_element()
    }

    /// Scrolls the chat list down to the latest message on the shared spring, then follows
    /// the tail again; Reduce Motion goes there at once.
    fn glide_to_latest(&mut self, scope: Rc<str>, cx: &mut Context<Self>) {
        if let Some(state) = self.transcript_scrolls.get_mut(&*scope) {
            state.cancel_history_restore();
        }
        let list = move |this: &Self| {
            this.transcript_scrolls.get(&*scope).and_then(|state| {
                let list = state.list.as_ref()?;
                Some(ListState::clone(list))
            })
        };
        let Some(start) = list(self) else {
            return;
        };
        if cx.reduce_motion() {
            start.set_follow_mode(FollowMode::Tail);
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            let (mut velocity, mut last) = (0., motion::now());
            loop {
                cx.background_executor().timer(motion::FRAME_60).await;
                let done = this.update(cx, |this, cx| {
                    let Some(list) = list(this) else {
                        return true;
                    };
                    let left = f32::from(
                        list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y,
                    );
                    let now = motion::now();
                    let (next, speed) = motion::spring::step(left, velocity, 0., now - last);
                    (velocity, last) = (speed, now);
                    list.scroll_by(px(left - next));
                    cx.notify();
                    if next == 0. {
                        list.set_follow_mode(FollowMode::Tail);
                    }
                    next == 0.
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        })
        .detach();
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
    let from = from.iter().map(String::as_str);
    match inserted {
        [worked] if worked.starts_with("worked:") => {
            let header = f32::from(height::REGULAR + space::S);
            motion::hand_off(from, format!("{scope}:{worked}:body").into(), header);
        }
        [first, ..] => motion::hand_off(from, super::row_motion_id(scope, first).into(), 0.),
        [] => {
            if let Some(next) = next {
                motion::hand_off(from, super::row_motion_id(scope, next).into(), 0.);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TranscriptScroll, history_destination, splice_chat_rows};
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
