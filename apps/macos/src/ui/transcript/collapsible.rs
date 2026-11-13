//! Native text layout decides whether the five-line preview needs an expansion button.

use crate::ui::primitives::{Handler, Type, Typography as _, height, palette, space};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, ElementId, IntoElement, ParentElement as _, RenderOnce,
    SharedString, Styled as _, Window, div,
};
use gpui_component::ElementExt;
use gpui_component::button::{Button, ButtonVariants as _};

const PREVIEW_LINES: f32 = 5.;

#[derive(IntoElement)]
pub(super) struct CollapsibleText {
    pub(super) id: SharedString,
    pub(super) text: SharedString,
    pub(super) expanded: bool,
    pub(super) expanded_content: Option<AnyElement>,
    pub(super) toggle: Handler<ClickEvent>,
}

impl RenderOnce for CollapsibleText {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let overflow = window.use_keyed_state(
            (ElementId::from(SharedString::clone(&self.id)), "overflow"),
            cx,
            |_, _| false,
        );
        let show_toggle = self.expanded || *overflow.read(cx);
        let cap = window.pixel_snap(window.text_style().line_height_in_pixels(window.rem_size()))
            * PREVIEW_LINES;
        let plain = div()
            .relative()
            .flex_shrink_0()
            .whitespace_normal()
            .child(self.text)
            .on_prepaint(move |bounds, _, cx| {
                let clipped = bounds.size.height > cap;
                if *overflow.read(cx) != clipped {
                    overflow.update(cx, |overflow, cx| {
                        *overflow = clipped;
                        cx.notify();
                    });
                }
            });
        let content = if self.expanded {
            self.expanded_content
                .unwrap_or_else(|| plain.into_any_element())
        } else {
            div()
                .flex()
                .flex_col()
                .max_h(cap)
                .overflow_hidden()
                .child(plain)
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap(space::XXS)
            .child(content)
            .when(show_toggle, |view| {
                view.child(
                    Button::new(self.id)
                        .ghost()
                        .px_0()
                        .h(height::COMPACT)
                        .text_color(palette(cx).accent)
                        .accessibility_label(if self.expanded {
                            "Show less"
                        } else {
                            "See more"
                        })
                        .tooltip(if self.expanded {
                            "Collapses the message"
                        } else {
                            "Expands the full message"
                        })
                        .child(div().typography(Type::Subhead).underline().child(
                            if self.expanded {
                                "Show less"
                            } else {
                                "See more"
                            },
                        ))
                        .on_click(move |event, window, cx| (self.toggle)(event, window, cx)),
                )
            })
    }
}
