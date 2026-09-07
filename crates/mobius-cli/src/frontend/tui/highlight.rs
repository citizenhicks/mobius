use std::path::Path;
use std::sync::OnceLock;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::FontStyle;
use syntect::highlighting::Theme;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

const MAX_LINE_BYTES: usize = 4 * 1024;

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        two_face::theme::extra()
            .get(EmbeddedThemeName::CatppuccinMocha)
            .clone()
    })
}

pub(super) fn lines(code: &str, path: &str) -> Option<Vec<Vec<Span<'static>>>> {
    if code.lines().any(|line| line.len() > MAX_LINE_BYTES) {
        return None;
    }
    let extension = Path::new(path).extension()?.to_str()?;
    let syntax = syntaxes().find_syntax_by_extension(extension)?;
    let mut highlighter = HighlightLines::new(syntax, theme());
    LinesWithEndings::from(code)
        .map(|line| {
            highlighter
                .highlight_line(line, syntaxes())
                .ok()
                .map(|ranges| {
                    let mut spans = ranges
                        .into_iter()
                        .filter_map(|(style, text)| {
                            let text = text.trim_end_matches(['\n', '\r']);
                            (!text.is_empty()).then(|| {
                                let mut rendered = Style::default().fg(Color::Rgb(
                                    style.foreground.r,
                                    style.foreground.g,
                                    style.foreground.b,
                                ));
                                if style.font_style.contains(FontStyle::BOLD) {
                                    rendered = rendered.add_modifier(Modifier::BOLD);
                                }
                                Span::styled(text.to_string(), rendered)
                            })
                        })
                        .collect::<Vec<_>>();
                    if spans.is_empty() {
                        spans.push(Span::raw(String::new()));
                    }
                    spans
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_syntaxes_and_theme_highlight_without_loading_definitions() {
        for (path, code) in [
            ("main.rs", "fn main() {}"),
            ("config.yaml", "enabled: true"),
        ] {
            let rendered = lines(code, path).expect("bundled syntax and theme must load");
            let spans = &rendered[0];
            assert_eq!(
                spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>(),
                code
            );
            assert!(spans.iter().any(|span| span.style.fg != spans[0].style.fg));
        }
    }
}
