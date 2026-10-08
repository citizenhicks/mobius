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
fn renders_common_agent_markdown() {
    let lines = render(
        "# Result\n\n- **bold** and _emphasis_ and `code`\n\n| A | B |\n|---|---|\n| 1 | 2 |",
        crate::frontend::theme::current().style(crate::frontend::theme::Role::Text),
        80,
    );
    let text = plain_text(&lines);
    let modifiers = lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.style.add_modifier)
        .collect::<Vec<_>>();

    assert!(text.contains("Result") && text.contains("- bold and emphasis and code"));
    assert!(text.contains("A │ B") && text.contains("1 │ 2"));
    assert!(modifiers.iter().any(|value| value.contains(Modifier::BOLD)));
    assert!(
        modifiers
            .iter()
            .any(|value| value.contains(Modifier::ITALIC))
    );
}

#[test]
fn preserves_markdown_breaks_tasks_and_code_spacing() {
    let text = plain_text(&render(
        "first\nsecond  \nthird\n\n- [x] done\n- [ ] pending\n\n```unknown\n  one\n\n\n    two\n```\n\n<https://example.com>",
        Style::default(),
        80,
    ));
    assert_eq!(
        text,
        "first second\nthird\n\n- [x] done\n- [ ] pending\n\nunknown\n│   one\n│ \n│ \n│     two\n\nhttps://example.com"
    );
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
fn aligns_tables_and_stacks_rows_in_narrow_terminals() {
    let source = "| Name | Count |\n|:---|---:|\n| 界 | 2 |\n| longer | 100 |";
    let wide = render(source, Style::default(), 40);
    assert_eq!(
        plain_text(&wide),
        "Name   │ Count\n───────┼──────\n界     │     2\nlonger │   100"
    );
    let narrow = render(source, Style::default(), 12);
    assert_eq!(
        plain_text(&narrow),
        "Name: 界\nCount: 2\n\nName: longer\nCount: 100"
    );
    assert!(narrow.iter().all(|line| line.width() <= 12));
}

#[test]
fn highlights_fenced_code_and_renders_incomplete_streams() {
    let source = "```rust\nfn main() {\n\n    println!(\"hello\");\n}\n```";
    let complete = render(source, Style::default(), 80);
    let streaming = render(source.trim_end_matches('`'), Style::default(), 80);
    assert_eq!(complete, streaming);
    assert!(
        complete[1]
            .spans
            .iter()
            .any(|span| span.style.fg != complete[1].spans.last().unwrap().style.fg)
    );
    assert!(plain_text(&complete).contains("│ \n│     println!"));
}

#[test]
fn html_blocks_preserve_paragraph_boundaries_while_streaming() {
    for source in [
        "Before\n\n<div",
        "Before\n\n<div>html</div>\n\nAfter",
        "Before <span>inline</span> after",
    ] {
        assert_eq!(plain_text(&render(source, Style::default(), 80)), source);
        let mut cache = StreamCache::default();
        cache.render("Before", Style::default(), 80);
        assert_eq!(
            plain_text(cache.render(source, Style::default(), 80)),
            source
        );
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
    ] {
        let mut cache = StreamCache::default();
        for end in 1..=source.len() {
            if source.is_char_boundary(end) {
                let prefix = &source[..end];
                assert_eq!(
                    cache.render(prefix, Style::default(), 32),
                    render(prefix, Style::default(), 32),
                    "{prefix:?}"
                );
            }
        }
        assert_eq!(
            cache.render(source, Style::default(), 12),
            render(source, Style::default(), 12)
        );
        let style = Style::default().fg(ratatui::style::Color::Blue);
        assert_eq!(cache.render(source, style, 12), render(source, style, 12));
    }
}

#[test]
fn streaming_cache_reuses_finished_prose_and_clears_between_messages() {
    let mut cache = StreamCache::default();
    cache.render("First paragraph.\n\nT", Style::default(), 80);
    cache.render("First paragraph.\n\nTail", Style::default(), 80);
    assert!(cache.prefix_bytes > 0);
    let pointer = cache.lines[0].spans[0].content.as_ptr();
    cache.render("First paragraph.\n\nTail grows", Style::default(), 80);
    assert_eq!(cache.lines[0].spans[0].content.as_ptr(), pointer);
    cache.clear();
    assert_eq!(
        cache.render("New message", Style::default(), 80),
        render("New message", Style::default(), 80)
    );
}
