use super::*;

fn plain_text(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn wraps_lists_quotes_and_unicode_without_losing_styles() {
    let lines = render("> - **alpha beta gamma delta**", Style::default(), 16);
    assert_eq!(plain_text(&lines), "> - alpha beta\n>   gamma delta");
    assert!(
        lines[1]
            .spans
            .iter()
            .any(|span| span.content.contains("gamma")
                && span.style.add_modifier.contains(Modifier::BOLD))
    );
    let lines = render("- 界界界界界界", Style::default(), 8);
    assert_eq!(plain_text(&lines), "- 界界界\n  界界界");
    for width in [1, 2, 3, 4, 5, 8, 16, 80] {
        let lines = render("- **hello** e\u{301} 👨‍👩‍👧‍👦 world", Style::default(), width);
        assert!(plain_text(&lines).contains("e\u{301}"));
        assert!(plain_text(&lines).contains("👨‍👩‍👧‍👦"));
        assert!(lines.iter().all(|line| line.width() <= width.max(2)));
    }
}

#[test]
fn streaming_cache_matches_full_render_through_containers_references_and_resizing() {
    for source in [
        "First **paragraph**.\n\nSecond paragraph.\n\n# Heading\n\nTail",
        "Before\n\n```rust\nfn main() {\n\n}\n```\n\nAfter\n\nTail",
        "Before\n\n- first\n\n  continuation\n- second\n\nAfter\n\nTail",
        "Before\n\n> quote\n>\n> continued\n\nAfter\n\nTail",
        "Before\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\nAfter\n\nTail",
        "Before\n\nHeading\n---\n\nAfter\n\nTail",
        "Before\n\n---\n\nAfter\n\nTail",
        "界 **λ**\r\n\r\n    indented code\r\n\r\nAfter\r\n\r\nTail",
        "Before\n\n<div>\n\nraw HTML\n</div>\n\nAfter\n\nTail",
        "[text][id]\n\nLater\n\n[id]: https://example.com\n",
        "[id]\n\nLater\n\n[id]:\nhttps://example.com\n",
        "See [docs](https://example.com).\n\n[x]: listening on :8080\n\n```text\n[x]: https://example.com\n```\n\nTail",
    ] {
        let mut cache = StreamCache::default();
        let mut lines = Vec::new();
        for end in 1..=source.len() {
            if let Some(prefix) = source.get(..end) {
                cache.render_into(prefix, Style::default(), 32, None, &mut lines);
                assert_eq!(lines, render(prefix, Style::default(), 32), "{prefix:?}");
            }
        }
        cache.render_into(source, Style::default(), 12, None, &mut lines);
        assert_eq!(lines, render(source, Style::default(), 12));
        let style = Style::default().fg(ratatui::style::Color::Blue);
        cache.render_into(source, style, 12, None, &mut lines);
        assert_eq!(lines, render(source, style, 12));
    }
}

#[test]
fn streaming_cache_keeps_prefix_across_links_generics_and_fenced_code_blocks() {
    let source = "See [docs](https://example.com) and `Vec<u8>`.\n\n```rust\n// [x]: https://example.com\nfn demo(xs: &[u8]) -> Vec<usize> {\n    xs.iter().map(|&b| usize::from(b)).collect()\n}\n```\n\n[x]: listening on :8080\n\nClosing paragraph.";
    let mut cache = StreamCache::default();
    let mut lines = Vec::new();
    let mut peak_prefix = 0;
    for end in 1..=source.len() {
        if let Some(prefix) = source.get(..end) {
            cache.render_into(prefix, Style::default(), 40, None, &mut lines);
            assert_eq!(lines, render(prefix, Style::default(), 40), "{prefix:?}");
            assert!(
                cache.prefix_bytes() >= peak_prefix,
                "prefix cache unexpectedly reset at {prefix:?}"
            );
            peak_prefix = cache.prefix_bytes();
        }
    }
    assert!(
        cache.prefix_bytes() > source.find("[x]: listening").unwrap_or_default(),
        "expected closed code blocks and `[x]:` lines to stay incrementally cached"
    );
}
