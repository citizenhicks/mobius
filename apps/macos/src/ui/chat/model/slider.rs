//! SwiftUI's ReasoningSlider: a 50 point control whose 36 point capsule track fills with the
//! accent up to a white 40 point thumb, with a dot at every step. It commits when the
//! pointer is released.

use super::{
    AnyElement, BoxShadow, Context, Div, FluentBuilder, InteractiveElement, IntoElement, Levels,
    MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Shape,
    StatefulInteractiveElement, Styled, Window, Workspace, black, div, point, px, relative, white,
};
use crate::numeric::ToFloat;
use crate::ui::alpha;
use crate::ui::theme::Palette;

const HEIGHT: Pixels = px(50.);
/// The track sits 7 points inside the control, as `trackRect` insets it.
const INSET: Pixels = px(7.);
/// Slightly taller than the track, so it overhangs both edges by `-PAD`.
const THUMB: Pixels = px(40.);
const TICK: Pixels = px(6.);
/// Thumb margin inside the track; negative, so the thumb overhangs it.
const PAD: Pixels = px(-2.);

impl Workspace {
    pub(super) fn reasoning_slider(
        &self,
        levels: &Levels,
        index: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let count = levels.choices.len();
        let fraction = index.to_f32() / (count - 1).max(1).to_f32();
        let travel = THUMB + PAD * 2.;
        let track = HEIGHT - INSET * 2.;
        let ticks = (0..count).map(|step| {
            div()
                .absolute()
                .left(relative(step.to_f32() / (count - 1).max(1).to_f32()))
                .top((track - TICK) / 2.)
                .ml(-TICK / 2.)
                .size(TICK)
                .pill_corners()
                .bg(if step <= index {
                    white().opacity(alpha::DIM)
                } else {
                    p.muted
                })
        });
        let zones = (0..count).map(|step| {
            div()
                .id(("reasoning-step", step))
                .flex_1()
                .h_full()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.reasoning_preview(step, cx)),
                )
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.reasoning_preview(step, cx);
                    }
                }))
        });
        let commit = |this: &mut Self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>| {
            this.commit_reasoning(cx);
        };
        div()
            .h(HEIGHT)
            .py(INSET)
            .child(
                div()
                    .id("reasoning-slider")
                    .role(gpui::accesskit::Role::Slider)
                    .aria_label(format!(
                        "Reasoning effort, {}",
                        self.effort_label(
                            levels.choices.get(index).copied().unwrap_or(levels.current)
                        )
                    ))
                    .relative()
                    .h_full()
                    .pill_corners()
                    .bg(p.inset)
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right(travel)
                            .child(
                                div().relative().h_full().w(relative(fraction)).child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .bottom_0()
                                        .left_0()
                                        .right(-travel)
                                        .pill_corners()
                                        .bg(p.accent_fill),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(PAD + THUMB / 2.)
                            .right(PAD + THUMB / 2.)
                            .children(ticks),
                    )
                    .child(thumb(fraction, &p))
                    .child(div().absolute().size_full().flex().children(zones))
                    .on_mouse_up(MouseButton::Left, cx.listener(commit))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(commit)),
            )
            .into_any_element()
    }
}

/// The plain white thumb, travelling between the track's margins; a hairline keeps it
/// visible on a light track.
fn thumb(fraction: f32, p: &Palette) -> Div {
    div()
        .absolute()
        .top(PAD)
        .left(PAD)
        .right(PAD + THUMB)
        .h(THUMB)
        .child(
            div()
                .absolute()
                .left(relative(fraction))
                .size(THUMB)
                .pill_corners()
                .bg(white())
                .when(!p.is_dark(), |thumb| {
                    thumb.border_1().border_color(p.separator)
                })
                .shadow(vec![BoxShadow {
                    color: black().opacity(alpha::TINT),
                    offset: point(px(0.), px(1.)),
                    blur_radius: px(2.),
                    spread_radius: px(0.),
                    inset: false,
                }]),
        )
}
