//! Sidebar anatomy: header, rows, group labels (spec §1.7).

use super::controls::{chevron, palette};
use super::tokens::{face, glyph, height, space};
use super::{Handler, Leading, Type, Typography};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, FontWeight, Pixels, SharedString, Window, div, px,
    transparent_black,
};
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariant, ButtonVariants};
use gpui_component::{Disableable, Selectable};
use std::rc::Rc;

/// The header band: 8 above, then a title row as tall as a rail button, with trailing
/// toolbar buttons, so the title centres on the first rail item.
pub fn sidebar_header(
    title: impl Into<SharedString>,
    actions: impl IntoIterator<Item = AnyElement>,
) -> Div {
    div().pt(space::S).child(
        div()
            .h(height::LARGE)
            .px(space::S)
            .flex()
            .items_center()
            .gap(space::XS)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .typography(Type::Headline)
                    .child(title.into()),
            )
            .children(actions),
    )
}

/// A 30 high sidebar row: 18 gutter, title, meta, pin, status cell and caret.
#[derive(IntoElement)]
pub struct SidebarRow {
    id: ElementId,
    title: SharedString,
    leading: Leading,
    meta: Option<SharedString>,
    subtitle: Option<SharedString>,
    flush: bool,
    status: Option<AnyElement>,
    selected: bool,
    muted: bool,
    disabled: bool,
    caret: Option<bool>,
    actions: Option<(SharedString, Pixels, bool)>,
    tooltip: Option<SharedString>,
    scroll_title: bool,
    accessibility_label: Option<SharedString>,
    on_click: Option<Handler<ClickEvent>>,
}

impl SidebarRow {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            leading: Leading::Gutter,
            meta: None,
            subtitle: None,
            flush: false,
            status: None,
            selected: false,
            muted: false,
            disabled: false,
            caret: None,
            actions: None,
            tooltip: None,
            scroll_title: false,
            accessibility_label: None,
            on_click: None,
        }
    }

    pub fn leading(mut self, leading: Leading) -> Self {
        self.leading = leading;
        self
    }

    /// Trailing Caption detail, such as a handle or service.
    pub fn meta(mut self, meta: impl Into<SharedString>) -> Self {
        self.meta = Some(meta.into());
        self
    }

    /// A caption line under the title; the row grows to fit it.
    pub fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// No leading gutter: for lists with no glyphs or folders to align with.
    pub fn flush(mut self) -> Self {
        self.flush = true;
        self
    }

    /// A trailing status cell as wide as a status dot's frame.
    pub fn status(mut self, status: impl IntoElement) -> Self {
        self.status = Some(status.into_any_element());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn muted(mut self) -> Self {
        self.muted = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// A disclosure chevron, up while open.
    pub fn caret(mut self, open: bool) -> Self {
        self.caret = Some(open);
        self
    }

    /// Reserves a trailing slot `width` wide, before the caret, for actions the caller
    /// overlays there (see `row_actions_inset`). The status sits at its end and hides
    /// while `group` is hovered or `revealed`, so showing the actions never moves the row.
    pub fn actions_slot(
        mut self,
        group: impl Into<SharedString>,
        width: Pixels,
        revealed: bool,
    ) -> Self {
        self.actions = Some((group.into(), width, revealed));
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// Reveals clipped titles on hover without moving the row's trailing controls.
    pub fn scroll_title(mut self) -> Self {
        self.scroll_title = true;
        self
    }

    pub fn accessibility_label(mut self, label: impl Into<SharedString>) -> Self {
        self.accessibility_label = Some(label.into());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    fn trailing(&mut self, p: &crate::ui::theme::Palette) -> Div {
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(space::XS)
            .children(self.meta.take().map(|meta| {
                div()
                    .max_w(px(96.))
                    .truncate()
                    .typography(Type::Caption)
                    .text_color(p.muted)
                    .child(meta)
            }))
            .map(|cell| match self.actions.take() {
                Some((group, width, revealed)) => cell.child(
                    div()
                        .min_w(width)
                        .flex()
                        .justify_end()
                        .child(
                            div()
                                .min_w(height::COMPACT)
                                .flex()
                                .items_center()
                                .justify_center()
                                .children(self.status.take()),
                        )
                        .map(|slot| {
                            if revealed {
                                slot.invisible()
                            } else {
                                slot.group_hover(group, gpui::Styled::invisible)
                            }
                        }),
                ),
                None => cell.children(
                    self.status
                        .take()
                        .map(|status| div().w(glyph::INLINE).flex().justify_center().child(status)),
                ),
            })
            .children(self.caret.map(|open| chevron(open, p)))
    }
}

impl RenderOnce for SidebarRow {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let trailing = self.trailing(&p);
        let leading = self.leading.element(glyph::INLINE, face::SIDEBAR, cx);
        let label = self
            .accessibility_label
            .unwrap_or_else(|| SharedString::clone(&self.title));
        let fg = if self.muted && !self.selected {
            p.muted
        } else {
            p.text
        };
        let title = if self.scroll_title {
            super::scrolling_title::scrolling_title(self.id.clone(), self.title, window, cx) // rules-allow: Button and the scrolling title each need their own ElementId, a refcounted name
                .into_any_element()
        } else {
            div()
                .min_w_0()
                .truncate()
                .child(self.title)
                .into_any_element()
        };
        Button::new(self.id)
            .with_variant(ButtonVariant::Custom(
                ButtonCustomVariant::new(cx)
                    .color(transparent_black())
                    .foreground(fg)
                    .hover(p.hover)
                    .active(p.pressed),
            ))
            .w_full()
            .min_w_0()
            .map(|row| match self.subtitle {
                Some(_) => row.h_auto().py(space::XS),
                None => row.h(height::REGULAR),
            })
            .px(space::S)
            .accessibility_label(label)
            .selected(self.selected)
            .disabled(self.disabled)
            .when(self.selected, |row| {
                row.bg(p.accent_soft).font_weight(FontWeight::MEDIUM)
            })
            .when_some(self.tooltip, gpui_component::button::Button::tooltip)
            .when_some(self.on_click, |row, handler| {
                row.on_click(move |event, window, cx| handler(event, window, cx))
            })
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .typography(Type::Body)
                    .when(!self.flush, |row| row.child(leading))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(title)
                            .children(self.subtitle.map(|subtitle| {
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .typography(Type::Caption)
                                    .text_color(p.muted)
                                    .child(subtitle)
                            })),
                    )
                    .child(trailing),
            )
    }
}

/// How far a row's overlaid actions sit from its right edge: the row's inset, plus the
/// caret and its gap when the row has one.
pub fn row_actions_inset(caret: bool) -> Pixels {
    if caret {
        space::S + glyph::MARK + space::XS
    } else {
        space::S
    }
}

/// A group label: a Label line with 12 above it, and a disclosure caret when the group
/// folds.
#[derive(IntoElement)]
pub struct SidebarLabel {
    text: SharedString,
    caret: Option<bool>,
}

pub fn sidebar_label(text: impl Into<SharedString>) -> SidebarLabel {
    SidebarLabel {
        text: text.into(),
        caret: None,
    }
}

impl SidebarLabel {
    /// A disclosure chevron after the label, up while open.
    pub fn caret(mut self, open: bool) -> Self {
        self.caret = Some(open);
        self
    }
}

impl RenderOnce for SidebarLabel {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        div()
            .h(height::GROUP_LABEL)
            .pt(space::S)
            .px(space::S)
            .flex()
            .items_center()
            .gap(space::XS)
            .typography(Type::BodyMedium)
            .text_color(p.muted)
            .child(self.text)
            .children(self.caret.map(|open| chevron(open, &p)))
    }
}

pub fn show_more_row(id: impl Into<ElementId>) -> SidebarRow {
    SidebarRow::new(id, "Show more").muted()
}
