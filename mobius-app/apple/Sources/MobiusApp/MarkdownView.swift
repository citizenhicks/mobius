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
        Group {
            if !streaming, let prose = continuousProseMarkdown(normalizedText) {
                Text(prose)
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(.primary)
                    .lineSpacing(5)
                    .textSelection(.enabled)
            } else {
                MobiusMarkdownDocument(text: normalizedText, streaming: streaming)
            }
        }
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
    let text: String
    let streaming: Bool

    var body: some View {
        let request = MobiusMarkdownRenderRequest(
            text: text,
            config: config,
            colorScheme: colorScheme,
            dynamicTypeSize: dynamicTypeSize
        )
        DocumentView(renderableDocument: document, config: request.config)
            .task(id: request) {
                let parsed = await MarkdownParserImpl().parse(
                    text: request.text,
                    config: request.config
                )
                guard !Task.isCancelled else { return }
                document = parsed
            }
    }

    private var config: MarkdownRenderConfig {
        let bodyFonts = TextFonts.mobius(MobiusStyle.bodyFont, context: fontResolutionContext)
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
            orderedListStyle: .init(textFonts: bodyFonts, textColor: .primary),
            paragraphStyle: .init(textFonts: bodyFonts, textColor: .primary),
            tableStyle: .init(
                textFonts: .mobius(.subheadline, context: fontResolutionContext),
                headerTextColor: .primary,
                regularTextColor: .primary,
                headerBackgroundColor: palette.raised,
                borderColor: palette.muted.opacity(0.42),
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
            codeBlockConfig: CodeBlockConfig(
                theme: .xcode,
                backgroundColor: palette.raised,
                foregroundColor: palette.muted,
                codeTextFonts: codeFonts,
                chromeTextFonts: .mobius(.footnote, context: fontResolutionContext)
            ),
            blockSpacing: MobiusSpace.m,
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
            italic: base.mobiusWithTraits(.traitItalic),
            bold: base.mobiusWithTraits(.traitBold),
            boldItalic: base.mobiusWithTraits([.traitBold, .traitItalic]),
            preferredLetterSpacing: nil,
            preferredLineHeight: nil
        )
    }
}

private extension UIFont {
    func mobiusWithTraits(_ traits: UIFontDescriptor.SymbolicTraits) -> UIFont {
        guard let descriptor = fontDescriptor.withSymbolicTraits(
            fontDescriptor.symbolicTraits.union(traits)
        ) else { return self }
        return UIFont(descriptor: descriptor, size: 0)
    }
}
