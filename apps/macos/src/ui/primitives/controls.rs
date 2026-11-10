//! Buttons, fields and choice controls (spec §1.5, §1.6).

use super::Shape;
use super::tokens::{glyph, height, inset, radius, space, width};
use super::{IntoGlyph, Type, Typography, icon};
use crate::numeric::ToFloat;
use crate::ui::theme::{Accent, Palette};
use gpui::prelude::*;
use gpui::{
    Anchor, Animation, AnimationExt, AnyElement, App, AppContext, ClickEvent, Context, Div,
    ElementId, Entity, Hsla, Pixels, SharedString, WeakEntity, Window, div, ease_out_quint,
    linear_color_stop, linear_gradient, rgb, transparent_black, white,
};
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariant, ButtonVariants};
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::progress::ProgressCircle;
use gpui_component::{Icon, Selectable, Sizable, Size};
use mobius_gateway::wire::BotShape;
use std::rc::Rc;

/// Ink of a control that cannot act now, matching a disabled button's softened glyph.
const UNAVAILABLE: f32 = 0.5;

/// A control that cannot act now: greyed out, and clicking it only explains `reason`.
pub fn unavailable(
    id: impl Into<ElementId>,
    trigger: Button,
    reason: impl Into<SharedString>,
    cx: &App,
) -> AnyElement {
    let p = palette(cx);
    Popover::new(id)
        .anchor(Anchor::BottomRight)
        .trigger(trigger.opacity(UNAVAILABLE))
        .w(width::POPOVER)
        .p(space::L)
        .tile_corners()
        .bg(p.control)
        .child(
            div()
                .typography(Type::Body)
                .text_color(p.text)
                .child(reason.into()),
        )
        .into_any_element()
}

/// A shared event callback, as stored by the builders here.
pub(crate) type Handler<E> = Rc<dyn Fn(&E, &mut Window, &mut App)>;

pub(crate) fn palette(cx: &App) -> Palette {
    *cx.global::<Palette>()
}

/// Monochrome glyph at rest with a hover fill; disabled buttons dim.
fn quiet(cx: &App) -> ButtonVariant {
    let p = palette(cx);
    ButtonVariant::Custom(
        ButtonCustomVariant::new(cx)
            .color(transparent_black())
            .foreground(p.muted)
            .hover(p.hover)
            .active(p.pressed),
    )
}

/// A square quiet button labelled by its tooltip, for a glyph, face or mark child.
fn quiet_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    box_size: Pixels,
    cx: &App,
) -> Button {
    let label = label.into();
    Button::new(id)
        .with_variant(quiet(cx))
        .size(box_size)
        .p_0()
        .tooltip(&label)
        .accessibility_label(label)
}

/// The glyph is a child rather than `Button::icon`, which would rescale it to the box.
pub fn icon_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: impl IntoGlyph,
    box_size: Pixels,
    glyph_size: Pixels,
    cx: &App,
) -> Button {
    quiet_button(id, label, box_size, cx).child(glyph.into_glyph(cx).size(glyph_size))
}

/// Rail sections and rail widgets: 36 box, 18 glyph.
pub fn rail_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: impl IntoGlyph,
    selected: bool,
    cx: &App,
) -> Button {
    let p = palette(cx);
    icon_button(id, label, glyph, height::LARGE, glyph::LEAD, cx)
        .selected(selected)
        .when(selected, |button| {
            button.bg(p.accent_soft).text_color(p.accent)
        })
}

/// How much a selected rail glyph grows as it fills.
const RAIL_SELECTED_SCALE: f32 = 1.05;

/// A rail section: its outline glyph at rest, and selected, the filled glyph growing a
/// little into place; Reduce Motion swaps at once.
/// What a rail button draws: an outline glyph that fills when selected, or the two-layer
/// möbius logo (the ribbon in the accent, the fold in the foreground ink).
#[derive(Clone, Copy)]
pub enum RailGlyph {
    Pair(&'static str, &'static str),
    MobiusLogo,
}

pub fn rail_section_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: RailGlyph,
    selected: bool,
    cx: &App,
) -> Button {
    let p = palette(cx);
    let label: SharedString = label.into();
    let scale = if selected { RAIL_SELECTED_SCALE } else { 1. };
    let mark = match glyph {
        RailGlyph::Pair(outline, filled) => div().size(glyph::LEAD * scale).child(
            icon(if selected { filled } else { outline }, &p)
                .size_full()
                .when(selected, |mark| mark.text_color(p.ink)),
        ),
        RailGlyph::MobiusLogo => {
            let layer = |name: &str, tint: Hsla| {
                div()
                    .absolute()
                    .inset_0()
                    .child(icon(name, &p).text_color(tint).size_full())
            };
            div()
                .relative()
                .size(glyph::LEAD * scale)
                .child(layer("MobiusLogoRibbon", p.accent))
                .child(layer("MobiusLogoFold", p.ink))
        }
    };
    let mark = if cx.reduce_motion() {
        mark.into_any_element()
    } else {
        mark.with_animation(
            SharedString::from(format!("rail-glyph:{label}:{selected}")),
            Animation::new(super::motion::QUICK).with_easing(ease_out_quint()),
            move |mark, progress| {
                mark.size(glyph::LEAD * (1. + (scale - 1.) * progress))
                    .opacity(0.4 + 0.6 * progress)
            },
        )
        .into_any_element()
    };
    quiet_button(id, label, height::LARGE, cx)
        .child(mark)
        .selected(selected)
        .when(selected, |button| {
            button.bg(p.accent_soft).text_color(p.accent)
        })
}

/// Titlebar, sidebar-header, strip and document-toolbar actions: 30 box, 14 glyph.
pub fn toolbar_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: impl IntoGlyph,
    cx: &App,
) -> Button {
    icon_button(id, label, glyph, height::REGULAR, glyph::INLINE, cx)
}

/// Trailing row and message actions: 26 box, 14 glyph.
pub fn inline_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: impl IntoGlyph,
    cx: &App,
) -> Button {
    icon_button(id, label, glyph, height::COMPACT, glyph::INLINE, cx)
}

/// Actions inside a sidebar row, over the row's own hover or selected fill: glyph only,
/// so the row never shows a second highlight.
pub fn row_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: impl IntoGlyph,
    cx: &App,
) -> Button {
    let p = palette(cx);
    inline_button(id, label, glyph, cx).with_variant(ButtonVariant::Custom(
        ButtonCustomVariant::new(cx)
            .color(transparent_black())
            .foreground(p.muted)
            .hover(transparent_black())
            .active(transparent_black()),
    ))
}

/// Close and dismiss marks: 20 box, 11 glyph.
pub fn mini_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: impl IntoGlyph,
    cx: &App,
) -> Button {
    icon_button(id, label, glyph, height::MINI, glyph::MARK, cx)
}

/// Composer controls: 36 circle, 18 glyph.
pub fn composer_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: impl IntoGlyph,
    cx: &App,
) -> Button {
    icon_button(id, label, glyph, height::LARGE, glyph::LEAD, cx).pill_corners()
}

/// The model and reasoning trigger: a 22 gauge of `level` 0 through 5 in a 36 circle.
pub fn gauge_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    level: usize,
    cx: &App,
) -> Button {
    const GAUGES: [&str; 6] = [
        "composer.reasoning0",
        "composer.reasoning1",
        "composer.reasoning2",
        "composer.reasoning3",
        "composer.reasoning4",
        "composer.reasoning5",
    ];
    let [.., full] = GAUGES;
    icon_button(
        id,
        label,
        GAUGES.get(level).copied().unwrap_or(full),
        height::LARGE,
        glyph::ICON,
        cx,
    )
    .pill_corners()
}

/// Send, stop and dictate: 18 glyph on the accent fill in a 32 circle.
pub fn primary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: impl IntoGlyph,
    cx: &App,
) -> Button {
    let label = label.into();
    Button::new(id)
        .primary()
        .size(height::PRIMARY)
        .p_0()
        .pill_corners()
        .tooltip(SharedString::clone(&label))
        .accessibility_label(label)
        .child(
            glyph
                .into_glyph(cx)
                .size(glyph::LEAD)
                .text_color(palette(cx).on_button_accent()),
        )
}

/// The titlebar chat menu, drawn as the chat's Bot face.
pub fn chat_menu_trigger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    face: impl IntoElement,
    cx: &App,
) -> Button {
    quiet_button(id, label, height::REGULAR, cx).child(face)
}

pub trait ToggleExt {
    /// Marks a toggle button as on, announced as pressed.
    fn toggle(self, on: bool, cx: &App) -> Self;
}

impl ToggleExt for Button {
    fn toggle(self, on: bool, cx: &App) -> Self {
        let p = palette(cx);
        self.toggled(on)
            .when(on, |button| button.text_color(p.accent))
    }
}

/// The one text button: 30 high, 8 radius, BodyMedium label. Pick the look with
/// `ButtonVariants` (primary, default, ghost or danger); never restyle its shape.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    let label = label.into();
    Button::new(id)
        .with_size(Size::Small)
        .h(height::REGULAR)
        .px(space::L)
        .accessibility_label(SharedString::clone(&label))
        .child(div().typography(Type::BodyMedium).child(label))
}

/// Sign in with Apple as a titlebar action: Apple's logo-only button, the Apple logo
/// from the system font in the toolbar button's box.
pub fn apple_toolbar_button(id: impl Into<ElementId>, cx: &App) -> Button {
    quiet_button(id, "Sign in with Apple", height::REGULAR, cx).child(
        div()
            .text_size(glyph::LEAD)
            .line_height(glyph::LEAD)
            .text_color(palette(cx).text)
            .child("\u{F8FF}"),
    )
}

/// A push button in a settings row's trailing slot, at System Settings' compact height.
pub fn settings_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    button(id, label).h(height::COMPACT)
}

/// Standalone actions, trailing and right-aligned.
pub fn button_row() -> Div {
    div().flex().items_center().justify_end().gap(space::S)
}

pub fn field(
    value: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_owned()));
    input.update(cx, |input, cx| {
        input.set_value(value.to_owned(), window, cx);
    });
    input
}

pub fn text_field(state: &Entity<InputState>) -> Input {
    Input::new(state).h(height::REGULAR)
}

pub fn textarea(state: &Entity<TextareaState>) -> Textarea {
    Textarea::new(state)
}

/// Up to four mutually exclusive options.
#[derive(IntoElement)]
pub struct Segmented {
    id: ElementId,
    labels: Vec<SharedString>,
    selected: Option<usize>,
    full_width: bool,
    on_select: Handler<usize>,
}

pub fn segmented(
    id: impl Into<ElementId>,
    labels: impl IntoIterator<Item = impl Into<SharedString>>,
    selected: Option<usize>,
    on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> Segmented {
    Segmented {
        id: id.into(),
        labels: labels.into_iter().map(Into::into).collect(),
        selected,
        full_width: false,
        on_select: Rc::new(on_select),
    }
}

impl Segmented {
    /// Stretches to its container with equal segments.
    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }
}

impl RenderOnce for Segmented {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let full_width = self.full_width;
        div()
            .id(self.id)
            .flex()
            .h(height::REGULAR)
            .p(space::XXS)
            .pill_corners()
            .bg(p.inset)
            .when(full_width, gpui::Styled::w_full)
            .children(self.labels.into_iter().enumerate().map(|(index, label)| {
                let on_select = Rc::clone(&self.on_select);
                let selected = self.selected == Some(index);
                Button::new(index)
                    .with_variant(quiet(cx))
                    .h(height::COMPACT)
                    .p_0()
                    .px(space::S)
                    .pill_corners()
                    .accessibility_label(SharedString::clone(&label))
                    .toggled(selected)
                    .text_color(if selected { p.text } else { p.muted })
                    .when(full_width, gpui::Styled::flex_1)
                    .when(selected, |segment| {
                        segment.bg(p.control).border_1().border_color(p.separator)
                    })
                    .on_click(move |_, window, cx| on_select(&index, window, cx))
                    .child(div().typography(Type::Label).child(label))
            }))
    }
}

/// One checked item of a `menu_select`.
pub struct MenuChoice {
    label: SharedString,
    checked: bool,
    on_select: Handler<ClickEvent>,
}

impl MenuChoice {
    pub fn new(
        label: impl Into<SharedString>,
        checked: bool,
        on_select: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            checked,
            on_select: Rc::new(on_select),
        }
    }
}

/// The Hugeicons chevron on a menu trigger or disclosure: down while closed, up while open.
pub fn chevron(open: bool, p: &Palette) -> Icon {
    icon(if open { "hi.caretUp" } else { "hi.caretDown" }, p).size(glyph::MARK)
}

/// A secondary button with the current value and a down chevron; it opens a menu or
/// a popover.
pub fn select_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: Option<Icon>,
    cx: &App,
) -> Button {
    let label = label.into();
    Button::new(id)
        .with_size(Size::Small)
        .h(height::REGULAR)
        .px(space::L)
        .accessibility_label(SharedString::clone(&label))
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::S)
                .typography(Type::Body)
                .children(glyph.map(|glyph| glyph.size(glyph::INLINE)))
                .child(label)
                .child(chevron(false, &palette(cx))),
        )
}

/// More than four options: a `select_button` that opens a menu.
pub fn menu_select(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: Option<&str>,
    items: Vec<MenuChoice>,
    cx: &App,
) -> impl IntoElement {
    let items = Rc::new(items);
    let glyph = glyph.map(|name| icon(name, &palette(cx)));
    select_button(id, label, glyph, cx).dropdown_menu(move |mut menu, _, _| {
        for item in items.iter() {
            let on_select = Rc::clone(&item.on_select);
            menu = menu.item(
                PopupMenuItem::new(SharedString::clone(&item.label))
                    .checked(item.checked)
                    .on_click(move |event, window, cx| on_select(event, window, cx)),
            );
        }
        menu
    })
}

/// A 26 high capsule for composer and toolbar status (MobiusBadge).
#[must_use]
pub struct Pill {
    image: Option<std::sync::Arc<gpui::Image>>,
    id: ElementId,
    label: Option<SharedString>,
    glyph: Option<SharedString>,
    progress: Option<f32>,
    tint: Option<Hsla>,
}

pub fn pill(id: impl Into<ElementId>) -> Pill {
    Pill {
        image: None,
        id: id.into(),
        label: None,
        glyph: None,
        progress: None,
        tint: None,
    }
}

impl Pill {
    /// Applies the same semantic tint to the label and glyph.
    pub fn tint(mut self, tint: Hsla) -> Self {
        self.tint = Some(tint);
        self
    }

    pub fn image(mut self, image: Option<std::sync::Arc<gpui::Image>>) -> Self {
        self.image = image;
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn glyph(mut self, glyph: impl Into<SharedString>) -> Self {
        self.glyph = Some(glyph.into());
        self
    }

    /// A progress ring, `fraction` from 0 to 1.
    pub fn progress(mut self, fraction: f32) -> Self {
        self.progress = Some(fraction);
        self
    }

    pub fn button(self, cx: &App) -> Button {
        let p = palette(cx);
        let foreground = self.tint.unwrap_or(p.text);
        let icon_only = self.label.is_none() && self.progress.is_none();
        let content = div()
            .flex()
            .items_center()
            .gap(space::S)
            .typography(Type::Label)
            .children(self.progress.map(|fraction| {
                ProgressCircle::new("progress")
                    .with_size(glyph::RING)
                    .color(p.accent)
                    .value(fraction.clamp(0., 1.) * 100.)
            }))
            .children(self.glyph.map(|name| {
                icon(&name, &p)
                    .size(glyph::INLINE)
                    .text_color(self.tint.unwrap_or_else(|| p.icon_ink()))
            }))
            .children(self.image.map(|image| gpui::img(image).size(glyph::INLINE)))
            .when_some(self.label.as_ref(), |content, label| {
                content.child(SharedString::clone(label))
            });
        Button::new(self.id)
            .with_variant(ButtonVariant::Custom(
                ButtonCustomVariant::new(cx)
                    .color(p.card)
                    .foreground(foreground)
                    .hover(p.card.blend(p.hover))
                    .active(p.card.blend(p.pressed)),
            ))
            .h(height::COMPACT)
            .px(if icon_only {
                inset::ICON_PILL
            } else {
                space::S
            })
            .pill_corners()
            .when_some(self.label, |button, label| {
                button.accessibility_label(label)
            })
            .child(content)
    }
}

/// A status dot centred in an INLINE frame.
pub fn status_dot(color: Hsla) -> Div {
    div()
        .size(glyph::INLINE)
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .child(div().size(glyph::DOT).pill_corners().bg(color))
}

/// A status dot before its muted label: how rows show a state.
pub fn status_label(color: Hsla, label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .gap(space::XS)
        .child(status_dot(color))
        .child(
            div()
                .typography(Type::Body)
                .text_color(palette(cx).muted)
                .child(label.into()),
        )
}

/// The accent as a row of app icons: each tile is the icon in that accent, as on iOS where the
/// accent sets the Home Screen icon too. The icon's dark gradient sits under the two-tone ribbon.
#[derive(IntoElement)]
pub struct AccentTilePicker {
    id: ElementId,
    selected: Accent,
    on_select: Handler<Accent>,
}

pub fn accent_tile_picker(
    id: impl Into<ElementId>,
    selected: Accent,
    on_select: impl Fn(&Accent, &mut Window, &mut App) + 'static,
) -> AccentTilePicker {
    AccentTilePicker {
        id: id.into(),
        selected,
        on_select: Rc::new(on_select),
    }
}

/// The ribbon's folded face: the accent lifted toward white, as `AccentTint.artworkTint`.
fn fold_tint(accent: Accent) -> Hsla {
    if accent == Accent::White {
        return accent.color();
    }
    crate::ui::theme::mix(accent.color(), white(), 0.35)
}

impl RenderOnce for AccentTilePicker {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let side = height::REGULAR + space::S;
        div()
            .id(self.id)
            .flex()
            .flex_wrap()
            .gap(space::S)
            .children(Accent::ALL.map(|accent| {
                let on_select = Rc::clone(&self.on_select);
                let selected = accent == self.selected;
                // Both layers fill the same inset box and are centred in it, so the mark sits in
                // the middle of the tile however the SVG's own bounds fall.
                let mark = side - space::S * 2.;
                let layer = |name: &str, tint: Hsla| {
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(name, &p).text_color(tint).size(mark))
                };
                let tile = div()
                    .relative()
                    .size(side)
                    .control_corners()
                    .overflow_hidden()
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(rgb(0x3b_42_52), 0.),
                        linear_color_stop(rgb(0x2e_34_40), 0.7),
                    ))
                    .child(layer("MobiusLogoRibbon", accent.color()))
                    .child(layer("MobiusLogoFold", fold_tint(accent)));
                Button::new(accent.label())
                    .with_variant(quiet(cx))
                    .size(side + space::XS * 2.)
                    .p(space::XS)
                    .outer_corners(radius::CONTROL, space::XS)
                    .border_2()
                    .border_color(if selected {
                        p.accent
                    } else {
                        transparent_black()
                    })
                    .toggled(selected)
                    .accessibility_label(accent.label())
                    .tooltip(accent.label())
                    .on_click(move |_, window, cx| on_select(&accent, window, cx))
                    .child(tile)
            }))
    }
}

/// The accent colours as round swatches (AccentTintPicker).
#[derive(IntoElement)]
pub struct SwatchPicker {
    id: ElementId,
    selected: Accent,
    on_select: Handler<Accent>,
}

pub fn swatch_picker(
    id: impl Into<ElementId>,
    selected: Accent,
    on_select: impl Fn(&Accent, &mut Window, &mut App) + 'static,
) -> SwatchPicker {
    SwatchPicker {
        id: id.into(),
        selected,
        on_select: Rc::new(on_select),
    }
}

impl RenderOnce for SwatchPicker {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        div()
            .id(self.id)
            .flex()
            .flex_wrap()
            .gap(space::XXS)
            .children(Accent::ALL.map(|accent| {
                let on_select = Rc::clone(&self.on_select);
                Button::new(accent.label())
                    .with_variant(quiet(cx))
                    .size(height::REGULAR)
                    .p_0()
                    .pill_corners()
                    .accessibility_label(accent.label())
                    .toggled(accent == self.selected)
                    .when(accent == self.selected, |target| {
                        target.border_2().border_color(p.icon_ink())
                    })
                    .tooltip(accent.label())
                    .on_click(move |_, window, cx| on_select(&accent, window, cx))
                    .child(
                        div()
                            .size(glyph::LEAD)
                            .pill_corners()
                            .bg(accent.color())
                            .when(accent == Accent::White, |swatch| {
                                swatch.border_1().border_color(p.line)
                            }),
                    )
            }))
    }
}

/// The face shapes as small faces in the Bot's tint, beside its colour swatches.
#[derive(IntoElement)]
pub struct ShapePicker {
    id: ElementId,
    selected: BotShape,
    tint: Hsla,
    on_select: Handler<BotShape>,
}

pub fn shape_picker(
    id: impl Into<ElementId>,
    selected: BotShape,
    tint: Hsla,
    on_select: impl Fn(&BotShape, &mut Window, &mut App) + 'static,
) -> ShapePicker {
    ShapePicker {
        id: id.into(),
        selected,
        tint,
        on_select: Rc::new(on_select),
    }
}

impl RenderOnce for ShapePicker {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let colors = Accent::ALL.len().to_f32();
        div()
            .id(self.id)
            .w(height::REGULAR * colors + space::XXS * (colors - 1.))
            .flex()
            .flex_wrap()
            .justify_between()
            .gap(space::XXS)
            .children(BotShape::ALL.map(|shape| {
                let on_select = Rc::clone(&self.on_select);
                let label = super::bot_face::shape_label(shape);
                Button::new(label)
                    .with_variant(quiet(cx))
                    .size(height::REGULAR)
                    .p_0()
                    .pill_corners()
                    .accessibility_label(label)
                    .toggled(shape == self.selected)
                    .when(shape == self.selected, |target| {
                        target.bg(p.accent_soft).border_1().border_color(p.accent)
                    })
                    .tooltip(label)
                    .on_click(move |_, window, cx| on_select(&shape, window, cx))
                    .child(super::BotFace::new(self.tint, shape, glyph::LEAD))
            }))
    }
}

/// The one on/off control: a Switch in the row's trailing slot, tinted like SwiftUI.
pub fn toggle(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    cx: &App,
) -> gpui_component::switch::Switch {
    gpui_component::switch::Switch::new(id)
        .checked(on)
        .color(palette(cx).control_tint)
        .accessibility_label(label)
}

/// A menu row whose leading glyph or face sits in one fixed column, so labels align across
/// rows that mix icons and Bot faces. Pair with a right-side check (`Side::Right`).
/// A menu item with `glyph` that runs `run` on the workspace, unless it was released.
pub(crate) fn workspace_item(
    label: impl Into<SharedString>,
    glyph: &str,
    view: &WeakEntity<crate::ui::Workspace>,
    run: impl Fn(&mut crate::ui::Workspace, &mut Window, &mut Context<crate::ui::Workspace>) + 'static,
    palette: &Palette,
) -> PopupMenuItem {
    let view = WeakEntity::clone(view);
    PopupMenuItem::new(label)
        .icon(icon(glyph, palette))
        .on_click(move |_, window, cx| {
            if let Err(error) = view.update(cx, |this, cx| run(this, window, cx)) {
                tracing::debug!(%error, "menu item skipped: the workspace was released");
            }
        })
}

pub(crate) fn leading_item(
    label: impl Into<SharedString>,
    leading: impl Fn(&App) -> AnyElement + 'static,
) -> PopupMenuItem {
    let label = label.into();
    PopupMenuItem::element(move |_, cx| {
        div()
            .flex()
            .items_center()
            .gap(space::S)
            .child(
                div()
                    .size(glyph::INLINE)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(leading(cx)),
            )
            .child(gpui::text!(SharedString::clone(&label)))
    })
}
