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
