//! Interface documents a Bot composes with `show_ui`: one hairline card headed by the block
//! title, holding sections, markdown, lists, tables, stat tiles, link previews, images and
//! charts. Neutral theme colours throughout; the accent marks data and links only.

mod chart;

use super::*;
use crate::numeric::ToFloat;
use gpui_component::table::{Table, TableBody, TableCell, TableHead, TableHeader, TableRow};
use mobius::protocol::{FrontendBlock, UiBlock, UiListItem, UiStat, UiTrend};

/// A list row's leading thumbnail.
const THUMBNAIL: Pixels = px(40.);
/// A link preview's wide image, and the placeholder an image keeps while it loads.
const PREVIEW: Pixels = px(160.);

/// A remote picture that fills its frame with `rounded` corners, neutral until it loads or if
/// it cannot. GPUI clips children to rectangles only, so the picture rounds itself.
fn picture(url: &str, p: &Palette, rounded: impl Fn(Div) -> Div + Copy + 'static) -> Img {
    let neutral = p.inset;
    let placeholder = move || rounded(div().size_full().bg(neutral)).into_any_element();
    let mut picture = img(SharedString::from(url.to_owned()))
        .object_fit(ObjectFit::Cover)
        .with_loading(placeholder)
        .with_fallback(placeholder);
    picture.style().corner_radii = std::mem::take(&mut rounded(div()).style().corner_radii);
    picture
}

fn all_corners(frame: Div) -> Div {
    frame.control_corners()
}

/// The host a link opens, "example.com" without "www.".
fn host(url: &str) -> Option<String> {
    let url = gpui::http_client::Url::parse(url).ok()?;
    let host = url.host_str()?;
    Some(host.strip_prefix("www.").unwrap_or(host).to_owned())
}

/// Cells that read as figures ("$1,999", "22 h", "−9 s") align right.
fn numeric(cell: &str) -> bool {
    cell.trim_start_matches(|c: char| "+-−$€£¥~≈".contains(c))
        .starts_with(|c: char| c.is_ascii_digit())
}

impl Workspace {
    /// The document card for a `Ui` block.
    pub(super) fn ui_document(
        &self,
        entry: &TranscriptEntry,
        block: &FrontendBlock,
        session_id: Option<&SessionId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        // A section heading joins the block after it; groups stand 16 apart.
        let mut groups: Vec<Div> = Vec::new();
        let mut heading: Option<&str> = None;
        for (index, part) in block.ui.iter().enumerate() {
            if let UiBlock::Section { title } = part {
                if let Some(title) = heading.replace(title) {
                    groups.push(self.ui_group(Some(title), None));
                }
                continue;
            }
            let id = format!("{}:ui:{index}", entry.id);
            let element = self.ui_block(&id, part, session_id, cx);
            groups.push(self.ui_group(heading.take(), Some(element)));
        }
        if let Some(title) = heading {
            groups.push(self.ui_group(Some(title), None));
        }
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(space::L)
            .p(inset::CARD)
            .control_corners()
            .border_1()
            .border_color(p.separator)
            .bg(p.card)
            .when(!block.title.is_empty(), |card| {
                card.child(
                    div()
                        .typography(Type::Headline)
                        .text_color(p.text)
                        .child(block.title.clone()), // rules-allow: the element owns its title while the block keeps it
                )
            })
            .children(groups)
            .into_any_element()
    }

    fn ui_group(&self, heading: Option<&str>, element: Option<AnyElement>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(space::S)
            .children(heading.map(|title| {
                div()
                    .typography(Type::Subhead)
                    .text_color(self.palette.text)
                    .child(title.to_owned())
            }))
            .children(element)
    }

    fn ui_block(
        &self,
        id: &str,
        part: &UiBlock,
        session_id: Option<&SessionId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = &self.palette;
        match part {
            UiBlock::Section { title } => div().child(title.to_owned()).into_any_element(),
            UiBlock::Text { markdown } => self
                .transcript_markdown(
                    SharedString::from(id.to_owned()),
                    markdown.to_owned(),
                    session_id,
                    cx,
                )
                .into_any_element(),
            UiBlock::List { items } => div()
                .flex()
                .flex_col()
                .children(items.iter().enumerate().flat_map(|(row, item)| {
                    // Hairlines between rows span the content; the hover fill reaches past it.
                    let line = (row > 0).then(|| hairline(p).into_any_element());
                    line.into_iter().chain([self.ui_list_row(
                        format!("{id}:{row}"),
                        item,
                        session_id,
                        cx,
                    )])
                }))
                .into_any_element(),
            UiBlock::Table { columns, rows } => self.ui_table(columns, rows),
            UiBlock::Stats { items } => div()
                .flex()
                .gap(space::S)
                .children(items.iter().map(|stat| self.ui_stat(stat)))
                .into_any_element(),
            UiBlock::Link {
                url,
                title,
                description,
                image_url,
            } => self.ui_link(
                id,
                url,
                title,
                description.as_deref(),
                image_url.as_deref(),
                session_id,
                cx,
            ),
            UiBlock::Image { url, alt, caption } => {
                // GPUI sizes a picture by its pixels unless both sides are set, so the frame
                // takes the loaded picture's shape at the card's width.
                let shape = cx
                    .fetch_asset::<ImgResourceLoader>(&Resource::Uri(url.to_owned().into()))
                    .and_then(Result::ok)
                    .map(|image| image.size(0))
                    .filter(|size| size.width.0 > 0 && size.height.0 > 0)
                    .map(|size| size.width.0.to_f32() / size.height.0.to_f32());
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S)
                    .child(
                        div()
                            .w_full()
                            .control_corners()
                            .overflow_hidden()
                            .bg(p.inset)
                            .map(|frame| match shape {
                                Some(shape) => frame.aspect_ratio(shape),
                                None => frame.h(PREVIEW),
                            })
                            .child(
                                picture(url, p, all_corners)
                                    .object_fit(ObjectFit::Contain)
                                    .size_full()
                                    .aria_label(alt.to_owned()),
                            ),
                    )
                    .children(caption.as_deref().map(|caption| {
                        div()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(caption.to_owned())
                    }))
                    .into_any_element()
            }
            UiBlock::Chart {
                kind,
                title,
                labels,
                series,
                unit,
            } => chart::chart(
                id,
                p,
                *kind,
                title.as_deref(),
                labels,
                series,
                unit.as_deref(),
            ),
        }
    }

    /// A row with an optional thumbnail, title over subtitle and a trailing figure; with a
    /// url it opens on click.
    fn ui_list_row(
        &self,
        id: String,
        item: &UiListItem,
        session_id: Option<&SessionId>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let title = SharedString::from(item.title.clone()); // rules-allow: the row owns its title while the block keeps it
        let row = div()
            .id(SharedString::from(id))
            .flex()
            .items_center()
            .gap(space::L)
            .mx(-space::S)
            .px(space::S)
            .py(space::S)
            .control_corners()
            .children(item.image_url.as_deref().map(|url| {
                div()
                    .size(THUMBNAIL)
                    .flex_shrink_0()
                    .control_corners()
                    .overflow_hidden()
                    .child(picture(url, &p, all_corners).size_full())
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(space::XXS)
                    .child(
                        div()
                            .truncate()
                            .typography(Type::BodyMedium)
                            .text_color(p.text)
                            .child(SharedString::clone(&title)),
                    )
                    .children(item.subtitle.as_deref().map(|subtitle| {
                        div()
                            .truncate()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(subtitle.to_owned())
                    })),
            )
            .children(item.trailing.as_deref().map(|trailing| {
                div()
                    .flex_shrink_0()
                    .typography(Type::Mono)
                    .text_color(p.text)
                    .child(trailing.to_owned())
            }));
        match item.url.as_deref() {
            Some(url) => {
                let (url, session) = (url.to_owned(), session_id.map(SessionId::share));
                row.cursor_pointer()
                    .role(gpui::accesskit::Role::Link)
                    .aria_label(title)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.bg(p.accent_soft))
                    .hover(move |row| row.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_transcript_link(&url, session.as_ref(), cx);
                    }))
                    .into_any_element()
            }
            None => row.into_any_element(),
        }
    }

    /// A hairline-framed table: a muted header row, dividers between rows, and columns of
    /// figures aligned right.
    fn ui_table(&self, columns: &[String], rows: &[Vec<String>]) -> AnyElement {
        fn cell<C: Styled + ParentElement + IntoElement>(cell: C, text: &str, right: bool) -> C {
            cell.min_w_0()
                .px(space::S)
                .py(inset::ROW_DENSE)
                .when(right, gpui::Styled::justify_end)
                .child(div().truncate().child(text.to_owned()))
        }
        let p = self.palette;
        let right: Vec<bool> = (0..columns.len())
            .map(|column| {
                !rows.is_empty()
                    && rows
                        .iter()
                        .all(|row| row.get(column).is_none_or(|cell| numeric(cell)))
            })
            .collect();
        Table::new()
            .control_corners()
            .border_1()
            .border_color(p.separator)
            .child(
                TableHeader::new().typography(Type::Subhead).child(
                    TableRow::new().px(space::S).children(
                        columns
                            .iter()
                            .zip(&right)
                            .map(|(text, right)| cell(TableHead::new(), text, *right)),
                    ),
                ),
            )
            .child(
                TableBody::new()
                    .typography(Type::Body)
                    .text_color(p.text)
                    .children(rows.iter().map(|row| {
                        TableRow::new()
                            .px(space::S)
                            .children(right.iter().enumerate().map(|(column, right)| {
                                let text = row.get(column).map_or("", String::as_str);
                                cell(TableCell::new(), text, *right)
                            }))
                    })),
            )
            .into_any_element()
    }

    fn ui_stat(&self, stat: &UiStat) -> Div {
        let p = self.palette;
        let tint = match stat.trend {
            Some(UiTrend::Up) => p.signal,
            Some(UiTrend::Down) => p.danger,
            Some(UiTrend::Flat) | None => p.muted,
        };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(space::XXS)
            .px(space::L)
            .py(space::S)
            .control_corners()
            .border_1()
            .border_color(p.separator)
            .child(
                div()
                    .truncate()
                    .typography(Type::Caption)
                    .text_color(p.muted)
                    .child(stat.label.clone()), // rules-allow: the element owns its label while the block keeps it
            )
            .child(
                div()
                    .truncate()
                    .typography(Type::Headline)
                    .text_color(p.ink)
                    .child(stat.value.clone()), // rules-allow: the element owns its value while the block keeps it
            )
            .children(stat.detail.as_deref().map(|detail| {
                div()
                    .truncate()
                    .typography(Type::Caption)
                    .text_color(tint)
                    .child(detail.to_owned())
            }))
    }

    /// A link preview: a wide picture over the title, two lines of description and the host.
    #[expect(
        clippy::too_many_arguments,
        reason = "a link preview's parts arrive as separate protocol fields"
    )]
    fn ui_link(
        &self,
        id: &str,
        url: &str,
        title: &str,
        description: Option<&str>,
        image_url: Option<&str>,
        session_id: Option<&SessionId>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let (href, session) = (url.to_owned(), session_id.map(SessionId::share));
        div()
            .id(SharedString::from(id.to_owned()))
            .flex()
            .flex_col()
            .control_corners()
            .border_1()
            .border_color(p.separator)
            .overflow_hidden()
            .cursor_pointer()
            .role(gpui::accesskit::Role::Link)
            .aria_label(title.to_owned())
            .focusable()
            .tab_stop(true)
            .focus_visible(|style| style.bg(p.accent_soft))
            .hover(move |card| card.bg(p.hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_transcript_link(&href, session.as_ref(), cx);
            }))
            .children(image_url.map(|image| {
                picture(image, &p, |frame| {
                    frame.top_corners(radius::inner(radius::CONTROL, inset::HAIRLINE))
                })
                .w_full()
                .h(PREVIEW)
                .flex_shrink_0()
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::XXS)
                    .px(space::L)
                    .py(space::S)
                    .child(
                        div()
                            .truncate()
                            .typography(Type::BodyMedium)
                            .text_color(p.text)
                            .child(title.to_owned()),
                    )
                    .children(description.map(|description| {
                        div()
                            .line_clamp(2)
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(description.to_owned())
                    }))
                    .children(host(url).map(|host| {
                        div()
                            .typography(Type::Caption)
                            .text_color(p.accent)
                            .child(host)
                    })),
            )
            .into_any_element()
    }
}
