import CoreText
import Foundation
import Markdown
import SwiftStreamingMarkdown
import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// Equatable so an unchanged message is skipped entirely.
///
/// Comparing text, annotations, and streaming covers the view's whole input. Without this,
/// every row's body re-runs whenever anything in the transcript
/// changes: each one rescans its own text for `\dots` and rebuilds the markdown subtree,
/// which during streaming is a few hundred messages of work per frame to redraw one.
struct MobiusMarkdownText: View, Equatable {
    let text: String
    let streaming: Bool
    let annotations: [JSONValue]

    init(_ text: String, streaming: Bool, annotations: [JSONValue] = []) {
        self.text = text
        self.streaming = streaming
        self.annotations = annotations
    }

    var body: some View {
        MobiusMarkdownDocument(text: text, streaming: streaming, annotations: annotations)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Display exact annotated citation spans as links without changing the stored transcript.
func markdownWithCitations(_ text: String, annotations: [JSONValue]) -> String {
    guard !annotations.isEmpty else { return text }
    // Citation character offsets count Unicode scalars, not Swift graphemes or UTF-16 units.
    let indices = Array(text.unicodeScalars.indices) + [text.endIndex]
    let prose = markdownProseRanges(in: Markdown.Document(parsing: text))
    var citations: [Range<String.Index>: [URL]] = [:]
    for annotation in annotations {
        guard annotation["type"]?.stringValue == "url_citation",
            let start = annotation["startIndex"]?.intValue,
            let end = annotation["endIndex"]?.intValue,
            start >= 0, end > start, end < indices.count,
            let destination = annotation["url"]?.stringValue,
            let url = URL(string: destination),
            ["http", "https"].contains(url.scheme?.lowercased()),
            url.host?.isEmpty == false
        else { continue }
        let range = indices[start]..<indices[end]
        let marker = text[range]
        guard marker.wholeMatch(of: /cite[^\r\n]+/) != nil else { continue }
        let prefix = text[..<range.lowerBound]
            .replacingOccurrences(of: "\r\n", with: "\n")
            .replacingOccurrences(of: "\r", with: "\n")
            .split(separator: "\n", omittingEmptySubsequences: false)
        let startLocation = SourceLocation(
            line: prefix.count, column: (prefix.last?.utf8.count ?? 0) + 1, source: nil)
        let endLocation = SourceLocation(
            line: startLocation.line, column: startLocation.column + marker.utf8.count, source: nil)
        guard
            prose.contains(where: { $0.lowerBound <= startLocation && endLocation <= $0.upperBound }
            ),
            !citations[range, default: []].contains(url)
        else { continue }
        citations[range, default: []].append(url)
    }
    var rendered = ""
    var offset = text.startIndex
    for (range, urls) in citations.sorted(by: { $0.key.lowerBound < $1.key.lowerBound }) {
        rendered += text[offset..<range.lowerBound]
        rendered += urls.map { url in
            let label = (url.host ?? url.absoluteString)
                .replacingOccurrences(of: "[", with: #"\["#)
                .replacingOccurrences(of: "]", with: #"\]"#)
            return "[\(label)](<\(url.absoluteString)>)"
        }.joined(separator: " ")
        offset = range.upperBound
    }
    return rendered + text[offset...]
}

private func markdownProseRanges(in markup: Markup) -> [SourceRange] {
    guard !(markup is Markdown.Link), !(markup is Markdown.Image),
        !(markup is CodeBlock), !(markup is InlineCode)
    else { return [] }
    if markup is Markdown.Text { return markup.range.map { [$0] } ?? [] }
    return markup.children.flatMap { markdownProseRanges(in: $0) }
}

/// Add web links only to prose nodes; Markdown owns explicit links, images, and code.
func markdownWithDetectedLinks(_ document: Markdown.Document) -> Markdown.Document {
    guard let detector = try? NSDataDetector(types: NSTextCheckingResult.CheckingType.link.rawValue)
    else { return document }
    return detectingMarkdownLinks(in: document, using: detector) as? Markdown.Document ?? document
}

private func detectingMarkdownLinks(in markup: Markup, using detector: NSDataDetector) -> Markup {
    guard !markup.isEmpty, !(markup is Markdown.Link), !(markup is Markdown.Image),
        !(markup is CodeBlock), !(markup is InlineCode)
    else { return markup }
    return markup.withUncheckedChildren(
        markup.children.flatMap { child -> [Markup] in
            if let text = child as? Markdown.Text {
                return detectedMarkdownLinks(in: text, using: detector)
            }
            return [detectingMarkdownLinks(in: child, using: detector)]
        })
}

private func detectedMarkdownLinks(in text: Markdown.Text, using detector: NSDataDetector)
    -> [Markup]
{
    let source = text.string as NSString
    var result: [Markup] = []
    var offset = 0
    for match in detector.matches(
        in: text.string, range: NSRange(location: 0, length: source.length))
    {
        guard let url = match.url, ["http", "https"].contains(url.scheme?.lowercased()),
            url.host?.isEmpty == false
        else { continue }
        if match.range.location > offset {
            result.append(
                Markdown.Text(
                    source.substring(
                        with: NSRange(location: offset, length: match.range.location - offset))))
        }
        result.append(
            Markdown.Link(
                destination: url.absoluteString,
                Markdown.Text(source.substring(with: match.range))))
        offset = NSMaxRange(match.range)
    }
    guard offset > 0 else { return [text] }
    if offset < source.length { result.append(Markdown.Text(source.substring(from: offset))) }
    return result
}

/// Carries the renderer's selection and table actions into the app.
///
/// The renderer ships its own selection sheet, but a sheet can only be sized and backed from
/// inside its own content, so ours replaces it. Table actions are also owned here so they
/// stay local instead of touching the gateway connection.
@MainActor
@Observable
private final class MarkdownSelectionRequest: MarkdownListener {
    var isPresented = false
    var isDownloadingTable = false
    var tableDownloadContent: String?

    func onContextMenuTap(id: String, selectedContent: String) async { isPresented = true }

    func onRender(markdown: RenderableDocument) async {}
    func onTableCopyTap(content: String) async { copyToPasteboard(content) }

    func onTableDownloadTap(content: String) async {
        tableDownloadContent = content
        isDownloadingTable = true
    }
    func onContextMenuAppear(id: String, selectedContent: String) async {}
    func onImageTap(image: MarkdownImage) async {}
}

private let mobiusSelectTextMenu = TextContextMenu(menuGroups: [
    TextContextMenuGroup(
        title: nil,
        image: nil,
        displayInline: true,
        items: [TextContextMenuItem(id: "mobius.selectText", title: "Select text")]
    )
])

/// The renderer fades each newly arrived word in, which is the whole reason for this package:
/// the words settle in behind the stream instead of snapping in a line at a time.
///
/// It reads the palette itself rather than taking one from `MobiusMarkdownText`, so a theme
/// change still reaches the config even when the equatable parent skips its own body.
private struct MobiusMarkdownDocument: View {
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @Environment(\.fontResolutionContext) private var fontResolutionContext
    @State private var document = RenderableDocument.empty
    @State private var selection = MarkdownSelectionRequest()
    let text: String
    let streaming: Bool
    let annotations: [JSONValue]

    var body: some View {
        let request = MobiusMarkdownRenderRequest(
            text: text,
            annotations: annotations,
            config: config,
            colorScheme: colorScheme,
            dynamicTypeSize: dynamicTypeSize
        )
        DocumentView(renderableDocument: document, config: request.config, listener: selection)
            .textSelection(.enabled)
            .environment(
                \.openURL,
                OpenURLAction { url in
                    guard let file = model.workspaceFile(for: url) else { return .systemAction }
                    model.previewWorkspaceFile(file)
                    return .handled
                }
            )
            .task(id: request) {
                let cited = markdownWithCitations(request.text, annotations: request.annotations)
                let parsed = await MarkdownParserImpl().parse(
                    text: cited.replacingOccurrences(
                        of: #"\\dots\b"#, with: #"\\ldots"#, options: .regularExpression),
                    option: .init(
                        speculativeRewrite: false,
                        imageSupport: request.config.imageConfig.enabled)
                )
                let rendered = await RenderableDocument(
                    document: markdownWithDetectedLinks(parsed.document), config: request.config)
                guard !Task.isCancelled else { return }
                document = rendered
            }
            .sheet(isPresented: $selection.isPresented) {
                SelectableText(
                    content: selectableMarkdown(
                        text,
                        markerColor: UIColor(palette.accent),
                        quoteColor: UIColor(palette.muted)
                    )
                )
                .padding(.horizontal, MobiusSpace.l)
                .padding(.bottom, MobiusSpace.l)
                // Clears the drag indicator, which sits in the top of the sheet's own bounds.
                .padding(.top, MobiusSpace.xl)
                .mobiusSheet()
            }
            .fileExporter(
                isPresented: $selection.isDownloadingTable,
                item: selection.tableDownloadContent,
                contentTypes: [.plainText],
                defaultFilename: "table.md"
            ) { result in
                if case .failure(let error) = result {
                    model.showToast(verbatim: model.localizedErrorDescription(error), tone: .error)
                }
            }
    }

    private var config: MarkdownRenderConfig {
        let bodyFonts = TextFonts.mobius(MobiusStyle.bodyFont, context: fontResolutionContext)
        let captionFonts = TextFonts.mobius(MobiusStyle.captionFont, context: fontResolutionContext)
        let codeFonts = TextFonts.mobiusCode(context: fontResolutionContext)
        return MarkdownRenderConfig(
            shouldAnimateText: streaming && !reduceMotion,
            blockQuoteStyle: .init(textFonts: bodyFonts, textColor: palette.muted),
            headingStyle: .init(
                h1Font: .mobius(.title3.weight(.bold), context: fontResolutionContext),
                h2Font: .mobius(.headline, context: fontResolutionContext),
                h3Font: .mobius(.subheadline.weight(.bold), context: fontResolutionContext),
                h4Font: .mobius(.subheadline.weight(.bold), context: fontResolutionContext),
                h5Font: .mobius(.subheadline.weight(.bold), context: fontResolutionContext),
                h6Font: .mobius(.subheadline.weight(.bold), context: fontResolutionContext),
                textColor: .primary
            ),
            orderedListStyle: .init(textFonts: bodyFonts, textColor: palette.accent),
            paragraphStyle: .init(textFonts: bodyFonts, textColor: .primary),
            tableStyle: .init(
                textFonts: .mobius(.subheadline, context: fontResolutionContext),
                headerTextColor: .primary,
                regularTextColor: .primary,
                headerBackgroundColor: palette.raised,
                borderColor: palette.line,
                actionButtonColor: palette.accent
            ),
            inlineStyle: .init(
                boldTextColor: .primary,
                linkTextFont: bodyFonts.normal,
                linkTextColor: palette.accent,
                codeTextFont: codeFonts.normal,
                codeTextColor: .primary,
                codeBackgroundColor: palette.raised,
                codeUnderlineColor: palette.line
            ),
            // Each block is its own text view, so a drag stops at the paragraph it started in.
            // This item opens the whole message as one selectable document, which is the only
            // cross-block selection UIKit will give us.
            textContextMenu: mobiusSelectTextMenu,
            citationConfig: .init(
                font: captionFonts.normal,
                textColor: palette.accent,
                backgroundColor: palette.accentSoft
            ),
            codeBlockConfig: CodeBlockConfig(
                theme: .xcode,
                backgroundColor: palette.raised,
                foregroundColor: palette.muted,
                codeTextFonts: codeFonts,
                chromeTextFonts: captionFonts
            ),
            blockSpacing: MobiusSpace.m,
            textSelectionConfig: TextSelectionConfig(isEnabled: false),
            thematicBreakColor: palette.line
        )
    }
}

private struct MobiusMarkdownRenderRequest: Equatable {
    let text: String
    let annotations: [JSONValue]
    let config: MarkdownRenderConfig
    let colorScheme: ColorScheme
    let dynamicTypeSize: DynamicTypeSize
}

private extension TextFonts {
    static func mobiusCode(context: Font.Context) -> TextFonts {
        TextFonts(
            normal: Font.footnote.monospaced().resolve(in: context).ctFont as UIFont,
            italic: nil,
            bold: nil,
            boldItalic: nil,
            preferredLetterSpacing: nil,
            preferredLineHeight: nil
        )
    }

    /// Bold and italic variants are derived rather than listed: the transcript is system
    /// text, so the descriptor already knows how to slant and embolden every style.
    static func mobius(_ font: Font, context: Font.Context) -> TextFonts {
        let base = font.resolve(in: context).ctFont as UIFont
        return TextFonts(
            normal: base,
            italic: base.withTraits(.traitItalic),
            bold: base.withTraits(.traitBold),
            boldItalic: base.withTraits([.traitBold, .traitItalic]),
            preferredLetterSpacing: nil,
            preferredLineHeight: nil
        )
    }
}

/// Read-only and selectable: `Text` selects all of itself or nothing on iOS, so the one
/// control that drags a selection across a whole message is a text view.
private struct SelectableText: UIViewRepresentable {
    @Environment(\.mobiusPalette) private var palette
    let content: NSAttributedString

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.isEditable = false
        view.backgroundColor = .clear
        view.textContainerInset = .zero
        view.textContainer.lineFragmentPadding = 0
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        view.attributedText = content
        view.tintColor = UIColor(palette.accent)
        view.linkTextAttributes = [
            .foregroundColor: UIColor(palette.accent),
            .underlineStyle: NSUnderlineStyle.single.rawValue,
        ]
    }
}
