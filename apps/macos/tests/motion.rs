#![allow(
    clippy::unwrap_used,
    reason = "virtual window and path construction failures must fail the regression"
)]

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ListAlignment,
    ListState, Modifiers, ParentElement as _, PathBuilder, PlatformInput, Render, Scene,
    ScrollDelta, ScrollEvent, StatefulInteractiveElement as _, Styled as _, TestAppContext,
    TouchPhase, Window, WindowHandle, blue, canvas, div, fill, list, point, px, red, rgb,
};
use std::time::Instant;

#[test]
fn retained_motion_invariants() {
    gpui::assert_motion_invariants();
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Quad,
    Path,
}

fn moving(kind: Kind) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let motion = gpui::Motion::new(|_| gpui::MotionFrame {
                offset: point(px(7.), px(0.)),
                ..Default::default()
            });
            window.with_motion(motion, bounds.center(), |window| match kind {
                Kind::Quad => window.paint_quad(fill(bounds, red())),
                Kind::Path => {
                    let mut path = PathBuilder::fill();
                    path.add_polygon(
                        &[bounds.origin, bounds.top_right(), bounds.bottom_right()],
                        true,
                    );
                    window.paint_path(path.build().unwrap(), red());
                }
            });
        },
    )
    .size(px(20.))
}

struct Sibling;
impl Render for Sibling {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size(px(5.)).bg(blue())
    }
}

struct Mover;
impl Render for Mover {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use gpui::MotionExt as _;
        div()
            .size(px(10.))
            .bg(blue())
            .with_motion(gpui::Motion::new(|_| gpui::MotionFrame {
                offset: point(px(50.), px(0.)),
                ..Default::default()
            }))
    }
}

struct Page {
    state: Option<ListState>,
    kind: Kind,
    moving: bool,
    sibling: Entity<Sibling>,
    mover: Entity<Mover>,
}
impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (kind, active) = (self.kind, self.moving);
        let content = match &self.state {
            Some(state) => div()
                .w(px(200.))
                .h(px(100.))
                .child(
                    list(ListState::clone(state), move |row, _, _| {
                        div()
                            .h(px(20.))
                            .w_full()
                            .bg(rgb(0x0010_0000 + u32::try_from(row).unwrap()))
                            .when(active && row == 1, |row| row.child(moving(kind)))
                            .into_any_element()
                    })
                    .size_full(),
                )
                .into_any_element(),
            None => div()
                .id("scroller")
                .overflow_y_scroll()
                .w(px(200.))
                .h(px(100.))
                .when(active, |div| div.child(moving(kind)))
                .children((0..40).map(|_| div().h(px(20.)).bg(rgb(0x0010_1010))))
                .into_any_element(),
        };
        div()
            .size_full()
            .bg(rgb(0x00ff_ffff))
            .child(Entity::clone(&self.mover))
            .child(content)
            .child(Entity::clone(&self.sibling))
    }
}

fn draw(window: WindowHandle<Page>, cx: &mut TestAppContext) {
    window
        .update(cx, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
}

fn scroll(window: WindowHandle<Page>, distance: gpui::Pixels, cx: &mut TestAppContext) {
    let frames = |window: &Window| {
        let stats = window.layout_stats();
        stats.views_built + stats.views_reused
    };
    let before = window.update(cx, |_, window, _| frames(window)).unwrap();
    window
        .update(cx, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::Scroll(ScrollEvent {
                    position: point(px(150.), px(50.)),
                    delta: ScrollDelta::Pixels(point(px(0.), distance)),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Moved,
                    ..Default::default()
                }),
                cx,
            );
        })
        .unwrap();
    let after = window.update(cx, |_, window, _| frames(window)).unwrap();
    if before == after {
        draw(window, cx);
    }
}

fn check(scene: &Scene, scale: f32) -> usize {
    let table = scene.motion_transforms(Instant::now());
    let quads = scene
        .quads
        .iter()
        .map(|p| (p.fast_motion, p.background.as_solid() == Some(red())));
    let paths = scene
        .paths
        .iter()
        .map(|p| (p.fast_motion, p.color.as_solid() == Some(red())));
    let mut own = 0;
    let others = scene
        .shadows
        .iter()
        .map(|p| p.pad)
        .chain(scene.underlines.iter().map(|p| p.pad))
        .chain(scene.monochrome_sprites.iter().map(|p| p.pad))
        .chain(scene.polychrome_sprites.iter().map(|p| p.fast_motion));
    for (index, ours) in quads.chain(paths).chain(others.map(|index| (index, false))) {
        assert!(
            (index as usize) < table.len(),
            "primitive must index its own scene's motion table"
        );
        if ours {
            let offset = table
                .get(index as usize)
                .unwrap()
                .transformation
                .apply(point(px(0.), px(0.)));
            assert_eq!(
                f32::from(offset.x) / scale,
                7.,
                "content must keep its motion, not the sibling's 50px motion"
            );
            own += 1;
        }
    }
    for layer in &scene.layers.frames {
        if let Some(content) = layer.content.scene() {
            own += check(content, scale);
        }
    }
    own
}

#[gpui::test]
fn scrolling_and_retained_paint_keep_their_own_motion(cx: &mut TestAppContext) {
    for state in [
        None,
        Some(ListState::new(40, ListAlignment::Top, px(1000.))),
    ] {
        for kind in [Kind::Quad, Kind::Path] {
            let window = cx.add_window(|_, cx| Page {
                state: state.clone(),
                kind,
                moving: false,
                sibling: cx.new(|_| Sibling),
                mover: cx.new(|_| Mover),
            });
            draw(window, cx);
            draw(window, cx);
            for _ in 0..4 {
                scroll(window, -px(3.), cx);
            }
            window
                .update(cx, |_, window, _| {
                    assert!(
                        window.layout_stats().layer_frames_composited > 0,
                        "static {kind:?} fixture (list={}) must promote a layer before its content starts moving: {:?}", state.is_some(), window.layout_stats()
                    );
                })
                .unwrap();
            window
                .update(cx, |page, _, cx| {
                    page.moving = true;
                    cx.notify();
                })
                .unwrap();
            draw(window, cx);
            for step in 0..2 {
                scroll(window, if step == 0 { px(3.) } else { -px(3.) }, cx);
                window
                    .update(cx, |_, window, _| {
                        assert!(
                            check(window.rendered_scene(), window.scale_factor()) > 0,
                            "moving content must be painted"
                        );
                    })
                    .unwrap();
                window
                    .update(cx, |page, _, cx| {
                        page.sibling.update(cx, |_, cx| cx.notify());
                    })
                    .unwrap();
                draw(window, cx);
                window
                    .update(cx, |_, window, _| {
                        assert!(
                            check(window.rendered_scene(), window.scale_factor()) > 0,
                            "retained content must keep animating"
                        );
                    })
                    .unwrap();
            }
        }
    }
}
