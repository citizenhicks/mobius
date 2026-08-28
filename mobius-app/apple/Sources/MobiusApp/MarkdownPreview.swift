import Foundation

/// A lightweight Markdown rendering used while `CollapsibleText` measures its collapsed state.
/// Parsing each line independently keeps block markup from swallowing the line breaks that the
/// truncation measurement depends on.
func inlineMarkdownPreview(_ source: String) -> AttributedString {
    var preview = AttributedString()

    let lines = source.split(separator: "\n", omittingEmptySubsequences: false)
    for (index, line) in lines.enumerated() {
        if index > 0 { preview.append(AttributedString("\n")) }
        preview.append(
            (try? AttributedString(
                markdown: String(line),
                options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)
            )) ?? AttributedString(line)
        )
    }

    return preview
}
