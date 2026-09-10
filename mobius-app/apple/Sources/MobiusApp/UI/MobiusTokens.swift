import SwiftUI

/// Every gap in the app — stack spacing and padding alike — is one of these six steps.
/// A gap that is not on the scale is drift: it reads as an accident beside the rows above
/// and below it. `0` stays 0 where a stack is deliberately flush.
enum MobiusSpace {
    /// Two lines that belong to one another: a name over its path.
    static let xxs: CGFloat = 2
    /// Inside one label or badge.
    static let xs: CGFloat = 4
    /// The default: a glyph and its text, one row and the next.
    static let s: CGFloat = 8
    /// Between blocks inside a card.
    static let m: CGFloat = 12
    /// Screen margins, and the gap between cards.
    static let l: CGFloat = 16
    /// Between sections of a page.
    static let xl: CGFloat = 24
}

enum MobiusStyle {
    static let bodyFont: Font = .body
    static let controlFont: Font = .body.weight(.medium)
    static let metadataFont: Font = .footnote.monospaced()
    /// A code someone reads off this screen and types on another device.
    static let codeFont: Font = .system(.title2, design: .monospaced, weight: .bold)
    static let badgeFont: Font = .footnote.weight(.medium)
    /// The title of a section or a card.
    static let titleFont: Font = .headline
    /// Prose one step under the body: a note under a control, a label over a figure.
    /// `metadataFont` is the monospaced twin, for values rather than sentences.
    static let captionFont: Font = .footnote
    static let cardRadius: CGFloat = 22
    static let controlRadius: CGFloat = 9
    /// Between a control and a card: the radius a card keeps when it shrinks to a tile.
    static let tileRadius: CGFloat = 14
    static let cardShape = RoundedRectangle(cornerRadius: cardRadius, style: .continuous)
    static let controlShape = RoundedRectangle(cornerRadius: controlRadius, style: .continuous)
    static let tileShape = RoundedRectangle(cornerRadius: tileRadius, style: .continuous)
    static let cardPadding: CGFloat = 14

    // MARK: Rows
    /// Minimum height of a row, by how much it has to carry. A row that can be tapped needs
    /// the full target; the two below it are for rows that only read.
    static let rowCompact: CGFloat = 26
    static let rowRegular: CGFloat = 30
    static let rowTouch: CGFloat = 44
    static let badgeHeight = rowCompact
    static let controlHeight = rowRegular
    static let iconButtonSize = rowTouch

    // MARK: Transcript
    /// The chat, a subagent preview, and a Bot routine draw the same transcript, so they
    /// read these rather than each carrying their own copy of the numbers.
    static let transcriptWidth: CGFloat = 880
    static let transcriptRowSpacing = MobiusSpace.m
    static let transcriptOrbSize: CGFloat = 144
    static let transcriptPadding = MobiusSpace.l

    // MARK: Glyphs
    /// Marks that qualify a row rather than name it: carets, disclosure, trailing hints.
    static let glyphMark: CGFloat = 11
    /// A glyph standing beside text as the subject of the row.
    static let glyphInline: CGFloat = 14
    /// The leading mark of a header, and the standalone controls in the composer.
    static let glyphLead: CGFloat = 18
    /// Glyphs sit inside a 44pt target, so 16 left them floating in air. This fills the
    /// button without changing it: the tap area, and every explicit size a call site asks
    /// for, are untouched.
    static let iconSize: CGFloat = 22
    /// The column every inline glyph is centred in, so the text beside it starts at the same
    /// x on every row whatever the glyph's own size. Anything larger keeps its own width.
    ///
    /// Above the scale there is no token: a hero glyph on a card or an empty state is sized
    /// to its container, not to the text beside it, so those stay literals at the call site.
    static let glyphGutter: CGFloat = 18

    static let borderWidth: CGFloat = 0.75
    /// Empty space an icon button keeps around its glyph to reach a full tap target.
    static let iconButtonInset = (iconButtonSize - iconSize) / 2
    /// Outer padding for a row of icon buttons: they carry `iconButtonInset` of their own,
    /// so matching the margin of neighbouring text means subtracting it here.
    static let iconRowPadding = cardPadding + MobiusSpace.xs - iconButtonInset
}
