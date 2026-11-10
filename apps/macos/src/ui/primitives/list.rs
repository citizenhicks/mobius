//! Page structure: scaffold, header, sections, grouped rows and stacked fields (spec §1.6).

use super::Shape;
use super::controls::palette;
use super::tokens::{face, glyph, height, inset, space, width};
use super::{BotFace, Handler, Type, Typography, heading, icon};
use crate::ui::theme::Palette;
use gpui::prelude::*;
use gpui::{
    AnyElement, AnyView, App, ClickEvent, Div, ElementId, Hsla, Pixels, Role, SharedString,
    StyledText, Window, div, px, relative, transparent_black,
};
use gpui_component::tooltip::Tooltip;
use mobius_gateway::wire::BotShape;
use std::rc::Rc;

/// A destination page: its own scroll position, the H1 aligned with the first sidebar row,
/// and a 720 column whose sections are 32 apart.
#[derive(IntoElement)]
pub struct Page {
    id: ElementId,
    header: PageHeader,
    fill: bool,
    children: Vec<AnyElement>,
}

pub fn page(key: impl Into<ElementId>, header: PageHeader) -> Page {
    Page {
        id: key.into(),
        header,
        fill: false,
        children: Vec::new(),
    }
}

impl Page {
    /// Fits the window instead of scrolling; a `flex_1` child takes the spare height.
    pub fn fill(mut self) -> Self {
        self.fill = true;
        self
    }
}

impl ParentElement for Page {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Page {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let fill = self.fill;
        let column = |element: Div| {
            element.when(fill, |element| element.flex().flex_col().flex_1().min_h_0())
        };
        div()
            .id(self.id)
            .flex_1()
            .min_h_0()
            .map(|page| {
                if fill {
                    page.flex().flex_col()
                } else {
                    page.overflow_y_scroll()
                }
            })
            .child(
                column(div())
                    .px(space::L)
                    .pt(height::SIDEBAR_HEADER)
                    .pb(if fill { space::L } else { space::L * 2. })
                    .child(
                        column(div())
                            .w_full()
                            .max_w(width::PAGE)
                            .mx_auto()
                            .flex()
                            .flex_col()
                            .child(self.header)
                            .child(
                                column(div())
                                    .flex()
                                    .flex_col()
                                    .gap(space::L)
                                    .children(self.children),
                            ),
                    ),
            )
    }
}

#[derive(IntoElement)]
pub struct PageHeader {
    title: SharedString,
    leading: Option<AnyElement>,
    subtitle: Option<SharedString>,
}

impl PageHeader {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            leading: None,
            subtitle: None,
        }
    }

    pub fn leading(mut self, element: impl IntoElement) -> Self {
        self.leading = Some(element.into_any_element());
        self
    }

    pub fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }
}

impl RenderOnce for PageHeader {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        div()
            .flex()
            .items_center()
            .gap(space::S)
            .mb(space::L)
            .children(self.leading)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(heading(self.title))
                    .children(self.subtitle.map(|subtitle| {
                        div()
                            .mt(space::XS)
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(subtitle)
                    })),
            )
    }
}

/// A titled block of groups.
#[derive(IntoElement)]
pub struct Section {
    title: SharedString,
    info: Option<SharedString>,
    description: Option<SharedString>,
    children: Vec<AnyElement>,
}

impl Section {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            info: None,
            description: None,
            children: Vec::new(),
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// An info glyph after the title whose tooltip explains the whole section.
    pub fn info(mut self, info: impl Into<SharedString>) -> Self {
        self.info = Some(info.into());
        self
    }
}

impl ParentElement for Section {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Section {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let info = self.info.map(|info| info_tip(&self.title, info, cx));
        div()
            .flex()
            .flex_col()
            .gap(space::S)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .px(space::XXS)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(space::XXS)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(space::XS)
                                    .child(heading(self.title))
                                    .children(info),
                            )
                            .children(self.description.map(|description| {
                                div()
                                    .typography(Type::Caption)
                                    .text_color(p.muted)
                                    .child(description)
                            })),
                    ),
            )
            .children(self.children)
    }
}

/// Rows on one card with inset hairlines between them.
#[derive(IntoElement, Default)]
pub struct Group {
    rows: Vec<AnyElement>,
    footer: Option<SharedString>,
    fill: bool,
}

impl Group {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn footer(mut self, footer: impl Into<SharedString>) -> Self {
        self.footer = Some(footer.into());
        self
    }

    /// Takes the spare height of a filling page; its last row should be `flex_1`.
    pub fn fill(mut self, fill: bool) -> Self {
        self.fill = fill;
        self
    }
}

impl ParentElement for Group {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.rows.extend(elements);
    }
}

impl RenderOnce for Group {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let mut card = crate::ui::backdrop::surface(div().relative(), p.card, cx)
            .flex()
            .flex_col()
            .border_1()
            .border_color(p.separator)
            .overflow_hidden()
            .when(self.fill, |card| card.flex_1().min_h_0());
        for (index, row) in self.rows.into_iter().enumerate() {
            if index > 0 {
                card = card.child(hairline(&p).mx(space::L));
            }
            card = card.child(row);
        }
        div()
            .flex()
            .flex_col()
            .when(self.fill, |group| group.flex_1().min_h_0())
            .child(card)
            .children(self.footer.map(|footer| {
                div()
                    .pt(space::S)
                    .px(space::XXS)
                    .typography(Type::Caption)
                    .text_color(p.muted)
                    .child(footer)
            }))
    }
}

/// The leading slot of list and sidebar rows.
pub enum Leading {
    Selection(bool),
    Glyph(SharedString),
    Face(Hsla, BotShape),
    /// A full-height face for a two-line content row.
    LargeFace(Hsla, BotShape),
    /// An empty gutter that keeps text aligned with glyph rows.
    Gutter,
}

impl Leading {
    pub fn glyph(name: impl Into<SharedString>) -> Self {
        Self::Glyph(name.into())
    }

    pub(super) fn element(self, glyph: Pixels, face: Pixels, cx: &App) -> AnyElement {
        let gutter = div()
            .w(glyph::GUTTER)
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center();
        match self {
            Self::Selection(selected) => gutter
                .w(glyph::ICON)
                .child(
                    icon(
                        if selected {
                            "hi.checkCircle"
                        } else {
                            "hi.circle"
                        },
                        &palette(cx),
                    )
                    .size(glyph::ICON),
                )
                .into_any_element(),
            Self::Glyph(name) => gutter
                .child(icon(&name, &palette(cx)).size(glyph))
                .into_any_element(),
            Self::Face(tint, shape) => gutter
                .child(BotFace::new(tint, shape, face))
                .into_any_element(),
            Self::LargeFace(tint, shape) => gutter
                .w(super::tokens::face::HERO)
                .child(BotFace::new(tint, shape, super::tokens::face::HERO))
                .into_any_element(),
            Self::Gutter => gutter.into_any_element(),
        }
    }
}

/// A settings or list row: 44 minimum, title and subtitle, then value, controls and caret.
#[derive(IntoElement)]
#[must_use]
pub struct Row {
    id: ElementId,
    title: SharedString,
    subtitle: Option<SharedString>,
    status: Option<(Hsla, SharedString)>,
    leading: Option<Leading>,
    value: Option<(SharedString, Type)>,
    controls: Vec<AnyElement>,
    disclosure: bool,
    external: bool,
    dense: bool,
    muted: bool,
    truncate: bool,
    placeholder: bool,
    on_click: Option<Handler<ClickEvent>>,
}

impl Row {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            status: None,
            leading: None,
            value: None,
            controls: Vec::new(),
            disclosure: false,
            external: false,
            dense: false,
            muted: false,
            truncate: false,
            placeholder: false,
            on_click: None,
        }
    }

    /// Stands in for this row while its values load (SettingsLoadingRows): the title and
    /// subtitle become bars of their own size, so the rows that arrive take the same
    /// space and nothing jumps.
    pub fn placeholder(mut self) -> Self {
        self.placeholder = true;
        self.status = None;
        self.controls.clear();
        self
    }

    /// The single muted row a Group shows when its list is empty.
    pub fn empty(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            dense: true,
            muted: true,
            ..Self::new(id, text)
        }
    }

    pub fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// A state such as Allowed or Denied: a dot in `tone` and a word leading the subtitle,
    /// so the trailing edge holds only the row's control.
    pub fn status(mut self, tone: Hsla, label: impl Into<SharedString>) -> Self {
        self.status = Some((tone, label.into()));
        self
    }

    pub fn leading(mut self, leading: Leading) -> Self {
        self.leading = Some(leading);
        self
    }

    pub fn value(mut self, value: impl Into<SharedString>) -> Self {
        self.value = Some((value.into(), Type::Body));
        self
    }

    pub fn mono(mut self, value: impl Into<SharedString>) -> Self {
        self.value = Some((value.into(), Type::Mono));
        self
    }

    pub fn control(mut self, control: impl IntoElement) -> Self {
        self.controls.push(control.into_any_element());
        self
    }

    /// A trailing caret for rows that open something.
    pub fn disclosure(mut self) -> Self {
        self.disclosure = true;
        self
    }

    /// A trailing arrow for rows that open a page in the browser or another app.
    pub fn external(mut self) -> Self {
        self.external = true;
        self
    }

    pub fn dense(mut self) -> Self {
        self.dense = true;
        self
    }

    /// Keeps a user-provided title on one line with an ellipsis.
    pub fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Row {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let label = SharedString::clone(&self.title);
        let role = if self.external {
            Role::Link
        } else {
            Role::Button
        };
        let leading = self
            .leading
            .map(|leading| leading.element(glyph::LEAD, face::ROW, cx));
        let title_color = if self.muted { Some(p.muted) } else { None };
        let bar = |line: Div| {
            line.pill_corners()
                .bg(p.muted.opacity(0.18))
                .text_color(gpui::transparent_black())
        };
        let text = div()
            .flex_1()
            .min_w_0()
            .flex()
            .when(self.placeholder, gpui::Styled::items_start)
            .flex_col()
            .gap(space::XXS)
            .child(
                div()
                    .typography(Type::Body)
                    .when_some(title_color, gpui::Styled::text_color)
                    .when(self.truncate, gpui::Styled::truncate)
                    .when(self.placeholder, bar)
                    .child(self.title),
            )
            .children(if self.placeholder {
                self.subtitle
                    .map(|subtitle| bar(div().typography(Type::Caption)).child(subtitle))
            } else {
                subtitle_line(self.status, self.subtitle, &p)
            });
        div()
            .id(self.id)
            .flex()
            .items_center()
            .gap(space::S)
            .px(space::L)
            .py(if self.dense {
                inset::ROW_DENSE
            } else {
                inset::ROW
            })
            .min_h(if self.dense {
                height::REGULAR
            } else {
                height::TOUCH
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .children(leading)
                    .child(text),
            )
            .children(self.value.map(|(value, style)| {
                div()
                    .max_w(relative(0.6))
                    .truncate()
                    .typography(style)
                    .text_color(p.muted)
                    .child(value)
            }))
            .when(!self.controls.is_empty(), |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .children(self.controls),
                )
            })
            .when(self.disclosure, |row| {
                row.child(icon("hi.caretRight", &palette(cx)).size(glyph::MARK))
            })
            .when(self.external, |row| {
                row.child(icon("hi.arrowUpRight01", &palette(cx)).size(glyph::MARK))
            })
            .when_some(self.on_click, |row, handler| {
                row.role(role)
                    .aria_label(label)
                    .focusable()
                    .tab_stop(true)
                    .border_1()
                    .border_color(transparent_black())
                    .px(space::L - px(1.))
                    .hover(|style| style.bg(p.hover))
                    .focus_visible(|style| style.bg(p.accent_soft).border_color(p.accent))
                    .when(self.external, gpui::Styled::cursor_pointer)
                    .on_click(move |event, window, cx| handler(event, window, cx))
            })
    }
}

/// The muted caption under a row's title, led by its status dot and word.
fn subtitle_line(
    status: Option<(Hsla, SharedString)>,
    subtitle: Option<SharedString>,
    p: &Palette,
) -> Option<Div> {
    let line = div()
        .typography(Type::Caption)
        .text_color(p.muted)
        .line_clamp(2);
    let Some((tone, label)) = status else {
        return subtitle.map(|subtitle| line.child(subtitle));
    };
    let dot = "●".len();
    let mut text = format!("● {label}");
    let label_end = text.len();
    if let Some(subtitle) = subtitle {
        text.push_str(" · ");
        text.push_str(&subtitle);
    }
    let highlights = [(0..dot, tone.into()), (dot + 1..label_end, p.text.into())];
    Some(line.child(StyledText::new(text).with_highlights(highlights)))
}

/// A labelled control that takes the full row width below its title.
#[derive(IntoElement)]
pub struct StackedField {
    title: SharedString,
    info: Option<SharedString>,
    help: Option<SharedString>,
    control: Option<AnyElement>,
}

impl StackedField {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            info: None,
            help: None,
            control: None,
        }
    }

    pub fn info(mut self, info: impl Into<SharedString>) -> Self {
        self.info = Some(info.into());
        self
    }

    pub fn help(mut self, help: impl Into<SharedString>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn control(mut self, control: impl IntoElement) -> Self {
        self.control = Some(control.into_any_element());
        self
    }
}

/// The muted info glyph whose tooltip explains `title`'s setting.
pub fn info_tip(title: &str, info: SharedString, cx: &App) -> impl IntoElement + use<> {
    let p = palette(cx);
    div()
        .id(SharedString::from(format!("{title}:info")))
        .text_color(p.muted)
        .child(icon("hi.info", &p).size(glyph::INLINE))
        .tooltip(text_tooltip(info))
}

/// A horizontal separator between rows or sections.
pub fn hairline(p: &Palette) -> Div {
    div().h(inset::HAIRLINE).bg(p.separator)
}

/// A plain text tooltip for any element's `.tooltip(..)`.
pub fn text_tooltip(text: SharedString) -> impl Fn(&mut Window, &mut App) -> AnyView {
    move |window, cx| Tooltip::new(text.as_str()).build(window, cx)
}

impl RenderOnce for StackedField {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let title = SharedString::clone(&self.title);
        div()
            .flex()
            .flex_col()
            .gap(space::S)
            .px(space::L)
            .py(inset::ROW)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::XS)
                    .typography(Type::Body)
                    .child(self.title)
                    .children(self.info.map(|info| info_tip(&title, info, cx))),
            )
            .children(self.control)
            .children(self.help.map(|help| {
                div()
                    .typography(Type::Caption)
                    .text_color(p.muted)
                    .child(help)
            }))
    }
}
