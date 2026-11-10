//! Reveal a clipped sidebar title while hovered, using GPUI's measured scroll bounds.

use super::{Type, Typography};
use gpui::prelude::*;
use gpui::{App, ElementId, ScrollHandle, SharedString, Window, canvas, div, point, px};
use std::sync::Arc;
use std::time::Instant;

pub(super) fn scrolling_title(
    id: ElementId,
    title: SharedString,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let id = Arc::new(id);
    let state = window.use_keyed_state(
        ElementId::NamedChild(Arc::clone(&id), "title-scroll".into()),
        cx,
        |_, _| (ScrollHandle::new(), None::<Instant>),
    );
    let (scroll, started) = {
        let (scroll, started) = state.read(cx);
        (ScrollHandle::clone(scroll), *started)
    };
    let started = started.filter(|_| !cx.reduce_motion());
    let distance = started.map_or(0., |start| {
        (start.elapsed().as_secs_f32() - 0.7).max(0.) * 32.
    });
    scroll.set_offset(point(-px(distance).min(scroll.max_offset().x), px(0.)));
    let text = div()
        .relative()
        .map(|text| {
            if started.is_some() {
                text.flex_shrink_0().whitespace_nowrap()
            } else {
                text.w_full().min_w_0().truncate()
            }
        })
        .child(title);
    #[cfg(feature = "snapshot")]
    let text = text.child(crate::ui::snapshot::probe(format!("title-text:{id}")));
    #[cfg(feature = "snapshot")]
    let viewport_probe = crate::ui::snapshot::probe(format!("title-viewport:{id}"));
    div()
        .id(ElementId::NamedChild(
            Arc::clone(&id),
            "title-viewport".into(),
        ))
        .relative()
        .w_full()
        .min_w_0()
        .h(Type::Body.line_height())
        .typography(Type::Body)
        .on_hover(move |hovered, _, cx| {
            state.update(cx, |(_, started), cx| {
                *started = hovered.then(Instant::now);
                cx.notify();
            });
        })
        .child(
            div()
                .id(ElementId::NamedChild(id, "title-content".into()))
                .size_full()
                .overflow_hidden()
                .flex()
                .track_scroll(&scroll)
                .child(text),
        )
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, (), window, _| {
                    if started.is_some()
                        && -scroll.offset().x < scroll.max_offset().x
                        && !bounds.intersect(&window.content_mask().bounds).is_empty()
                    {
                        window.request_animation_frame();
                    }
                },
            )
            .absolute()
            .size_full(),
        )
        .map(|view| {
            #[cfg(feature = "snapshot")]
            let view = view.child(viewport_probe);
            view
        })
}
