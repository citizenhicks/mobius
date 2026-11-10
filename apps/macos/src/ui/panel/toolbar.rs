//! The document toolbar and body frame shared by every pane kind.

use super::*;

/// A document: the 44 toolbar, when it has something to offer, over a `card` body.
/// The tab already names the document, so a toolbar never repeats the tab's title.
pub(super) fn document(
    toolbar: impl Into<Option<Div>>,
    body: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let p = palette(cx);
    div()
        .flex()
        .flex_col()
        .flex_1()
        .size_full()
        .min_h_0()
        .min_w_0()
        .bg(p.card)
        .children(
            toolbar
                .into()
                .map(|toolbar| toolbar.border_b_1().border_color(p.separator)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(body),
        )
        .into_any_element()
}

/// The toolbar band; leading content first, then `spacer()` and trailing actions.
pub(super) fn toolbar() -> Div {
    div()
        .flex()
        .items_center()
        .flex_shrink_0()
        .h(height::TOUCH)
        .px(space::S)
        .gap(space::S)
}

pub(super) fn spacer() -> Div {
    div().flex_1().min_w(space::S)
}

/// A flex shrink factor for toolbar content that gives way before everything else.
pub(super) const GIVE_WAY: f32 = 1000.;

/// Kind glyph, muted parent path and the file name, truncating from the path.
pub(super) fn path_label(path: &str, glyph: &str, cx: &App) -> Div {
    let p = palette(cx);
    let home = std::env::var("HOME").unwrap_or_default();
    let path = match path.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    };
    let (parent, name) = path
        .rsplit_once('/')
        .map_or(("", path.as_str()), |(parent, name)| (parent, name));
    div()
        .flex()
        .items_center()
        .gap(space::S)
        .min_w_0()
        .child(icon(glyph, &palette(cx)).size(glyph::INLINE))
        .when(!parent.is_empty(), |label| {
            label.child(
                div()
                    .flex_shrink(GIVE_WAY)
                    .min_w_0()
                    .truncate()
                    .typography(Type::Caption)
                    .text_color(p.muted)
                    .child(format!("{} ›", parent.replace('/', " › "))),
            )
        })
        .child(
            div()
                .min_w_0()
                .truncate()
                .typography(Type::BodyMedium)
                .child(name.to_owned()),
        )
}

/// A muted Mono counter such as "1,284 lines".
pub(super) fn meta(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .typography(Type::Mono)
        .text_color(palette(cx).muted)
        .child(text.into())
}

/// `+N −M` in the signal and danger tints.
pub(super) fn totals(added: usize, removed: usize, cx: &App) -> Div {
    let p = palette(cx);
    div()
        .flex()
        .flex_shrink_0()
        .gap(space::XS)
        .typography(Type::Mono)
        .child(div().text_color(p.signal).child(format!("+{added}")))
        .child(div().text_color(p.danger).child(format!("−{removed}")))
}

/// "1 line", "12,345 lines".
pub(super) fn line_count(lines: usize) -> String {
    let digits = lines.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out.push_str(if lines == 1 { " line" } else { " lines" });
    out
}
