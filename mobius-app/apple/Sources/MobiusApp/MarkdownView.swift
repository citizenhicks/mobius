import CoreText
import Foundation
import SwiftStreamingMarkdown
import SwiftUI
import UIKit

/// Equatable so an unchanged message is skipped entirely.
///
/// Text and streaming are the view's whole input, so comparing them is complete rather than
/// a guess. Without this, every row's body re-runs whenever anything in the transcript
/// changes: each one rescans its own text for `\dots` and rebuilds the markdown subtree,
/// which during streaming is a few hundred messages of work per frame to redraw one.
struct MobiusMarkdownText: View, Equatable {
    let text: String
    let streaming: Bool

    init(_ text: String, streaming: Bool) {
        self.text = text
        self.streaming = streaming
    }

    var body: some View {
        MobiusMarkdownDocument(text: normalizedText, streaming: streaming)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var normalizedText: String {
        guard text.contains(#"\dots"#) else { return text }
        return text.replacingOccurrences(
            of: #"\\dots\b"#,
            with: #"\\ldots"#,
            options: .regularExpression
        )
    }
}

/// Carries the "Select text" tap out of the renderer's edit menu and into SwiftUI.
///
/// The renderer ships its own selection sheet, but a sheet can only be sized and backed from
/// inside its own content, so ours replaces it. Everything else the listener reports is
/// somebody else's feature.
@MainActor
@Observable
private final class MarkdownSelectionRequest: MarkdownListener {
    var isPresented = false

    func onContextMenuTap(id: String, selectedContent: String) async { isPresented = true }

    func onRender(markdown: RenderableDocument) async {}
    func onTableCopyTap(content: String) async {}
    func onTableDownloadTap(content: String) async {}
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
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @Environment(\.fontResolutionContext) private var fontResolutionContext
    @State private var document = RenderableDocument.empty
    @State private var selection = MarkdownSelectionRequest()
    let text: String
    let streaming: Bool

    var body: some View {
        let request = MobiusMarkdownRenderRequest(
            text: text,
            config: config,
            colorScheme: colorScheme,
            dynamicTypeSize: dynamicTypeSize
        )
        DocumentView(renderableDocument: document, config: request.config, listener: selection)
            .task(id: request) {
                let parsed = await MarkdownParserImpl().parse(
                    text: request.text,
                    config: request.config
                )
                guard !Task.isCancelled else { return }
                document = parsed
            }
            .sheet(isPresented: $selection.isPresented) {
                SelectableText(content: selectableMarkdown(
                    text,
                    markerColor: UIColor(palette.accent),
                    quoteColor: UIColor(palette.muted)
                ))
                    .padding(.horizontal, MobiusSpace.l)
                    .padding(.bottom, MobiusSpace.l)
                    // Clears the drag indicator, which sits in the top of the sheet's own bounds.
                    .padding(.top, MobiusSpace.xl)
                    .mobiusSheet()
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

private struct MobiusMarkdownRenderRequest: Hashable {
    let text: String
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
