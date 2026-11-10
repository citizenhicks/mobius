//! The one spacing, inset, radius, glyph, height and width scale (`docs/DESIGN.md`).
//! Values follow SwiftUI's MobiusSpace and MobiusStyle; a size that is not here is either
//! derived from these or a documented one-off at its call site.

/// Every gap and padding is one of these steps (MobiusSpace): 8 by default, 16 only at a page
/// edge, inside a card or popover, or around an outer container; 4 and 2 are optical nudges.
pub mod space {
    use gpui::{Pixels, px};
    /// Two lines that belong together: a title over its subtitle.
    pub const XXS: Pixels = px(2.);
    /// Inside one label or badge, and between titlebar buttons.
    pub const XS: Pixels = px(4.);
    /// The default: a glyph and its text, a control and the next, the sidebar inset.
    pub const S: Pixels = px(8.);
    /// Page margins, card, popover and composer insets, and the gap between page sections.
    pub const L: Pixels = px(16.);
}

/// Insets derived from component metrics, so contents centre in their frame.
pub mod inset {
    use gpui::{Pixels, px};
    /// Cards: approvals, pickers and usage tiles (MobiusStyle.cardPadding).
    pub const CARD: Pixels = px(16.);
    /// A 44 row's vertical padding, so a 30 control in it keeps the row at 44.
    pub const ROW: Pixels = px((44. - 30.) / 2.);
    /// A dense row's vertical padding: Body text in a 30 row.
    pub const ROW_DENSE: Pixels = px((30. - 20.) / 2.);
    /// An icon-only pill: a 14 glyph centred in the 26 capsule.
    pub const ICON_PILL: Pixels = px((26. - 14.) / 2.);
    /// The composer's button row: a 36 button centred in the 48 idle bar.
    pub const COMPOSER: Pixels = px((48. - 36.) / 2.);
    /// A hairline: a separator's thickness, or a border between a frame's edge and the fill
    /// or picture inside it.
    pub const HAIRLINE: Pixels = px(1.);
}

/// The few strengths at which one colour shows over another, so a shade is never picked at
/// the call site. An opacity that animates, or only hides and shows, is not a shade.
pub mod alpha {
    /// A colour washed over a surface: a tag's fill, a soft shadow.
    pub const TINT: f32 = 0.14;
    /// A quiet edge: the composer's hairline and focus outline.
    pub const EDGE: f32 = 0.35;
    /// Content stepped back: blocked options, the quieter ticks, a scrim, a stroke on a fill.
    pub const DIM: f32 = 0.5;
    /// A see-through surface text still reads on: reply quotes.
    pub const VEIL: f32 = 0.72;
}

pub mod radius {
    use gpui::{Pixels, px};
    /// Buttons, fields, rows, tabs and rail items.
    pub const CONTROL: Pixels = px(8.);
    /// Every container: groups, cards, banners, popovers, dialogs, the composer, bubbles
    /// and the sidebar.
    pub const TILE: Pixels = px(12.);
    /// Accent tiles behind hero glyphs.
    pub const HERO: Pixels = px(14.);
    /// The usage matrix's square cells.
    pub const CELL: Pixels = px(2.);
    /// Pills, segmented controls, swatches and dots.
    pub const FULL: Pixels = px(999.);

    /// The radius `inset` inside a frame with `parent` corners, so the two stay concentric.
    pub fn inner(parent: Pixels, inset: Pixels) -> Pixels {
        (parent - inset).max(Pixels::ZERO)
    }

    /// The radius `gap` outside a shape with `child` corners, so the two stay concentric.
    pub fn outer(child: Pixels, gap: Pixels) -> Pixels {
        child + gap
    }
}

/// SwiftUI glyph levels; nothing else is allowed (R4).
pub mod glyph {
    use gpui::{Pixels, px};
    /// Carets, disclosure marks, close marks and trailing hints.
    pub const MARK: Pixels = px(11.);
    /// A glyph beside Body or Caption text; toolbar, row and pill glyphs.
    pub const INLINE: Pixels = px(14.);
    /// Settings-row leading glyphs, banners, rail and composer buttons.
    pub const LEAD: Pixels = px(18.);
    /// The reasoning gauge and large placeholders.
    pub const ICON: Pixels = px(22.);
    /// The leading column every row centres its glyph in, so row text shares one x.
    pub const GUTTER: Pixels = px(18.);
    /// Empty states only.
    pub const HERO: Pixels = px(32.);
    /// A status dot, centred in an INLINE frame (MobiusStatusIndicator).
    pub const DOT: Pixels = px(8.);
    /// A chat's unread dot.
    pub const UNREAD: Pixels = px(7.);
    /// A progress ring inside a pill.
    pub const RING: Pixels = px(12.);
}

pub mod face {
    use gpui::{Pixels, px};
    pub const SIDEBAR: Pixels = px(18.);
    pub const ROW: Pixels = px(24.);
    pub const HERO: Pixels = px(48.);
    pub const FORM: Pixels = px(60.);
}

/// Control boxes and row heights: 20/26/30/36/44 (MobiusStyle rows and icon boxes).
pub mod height {
    use gpui::{Pixels, px};
    /// Close and dismiss marks, counters and the gateway switcher.
    pub const MINI: Pixels = px(20.);
    /// Pills, small buttons, segments and inline icon buttons.
    pub const COMPACT: Pixels = px(26.);
    /// Sidebar rows, tabs, text buttons, inputs and toolbar icon buttons.
    pub const REGULAR: Pixels = px(30.);
    /// Rail and composer buttons, and the sidebar title row beside the first rail button.
    pub const LARGE: Pixels = px(36.);
    /// List and settings rows, document toolbar, launcher tiles and pagination.
    pub const TOUCH: Pixels = px(44.);
    pub const TITLEBAR: Pixels = px(44.);
    /// Face, name pill and insets for the transparent persistent-chat header.
    pub const BOT_CHAT_HEADER: Pixels = px(48. + 26. + 2. + 4. + 8.);
    pub const COMPOSER_IDLE: Pixels = px(48.);
    /// Send and stop surfaces.
    pub const PRIMARY: Pixels = px(32.);
    /// A sidebar group label: 8 above a BodyMedium line.
    pub const GROUP_LABEL: Pixels = px(8. + 20.);
    /// Top padding 8, title row 36, switcher 20, bottom padding 8; pages start their
    /// title here so it lines up with the first sidebar row.
    pub const SIDEBAR_HEADER: Pixels = px(8. + 36. + 20. + 8.);
    /// A scrolled list inside a card or popover: eight touch rows (SwiftUI).
    pub const LIST_MAX: Pixels = px(44. * 8.);
    /// A scrolled list inside a dialog or the command palette.
    pub const DIALOG_LIST_MAX: Pixels = px(420.);
    /// The Memory page's page graph.
    pub const GRAPH: Pixels = px(520.);
}

pub mod width {
    use gpui::{Pixels, px};
    pub const RAIL: Pixels = px(52.);
    pub const SIDEBAR: Pixels = px(240.);
    pub const SIDEBAR_MIN: Pixels = px(200.);
    pub const SIDEBAR_MAX: Pixels = px(320.);
    /// The traffic lights' reserved leading titlebar inset.
    pub const TRAFFIC_LIGHTS: Pixels = px(87.);
    pub const PAGE: Pixels = px(720.);
    pub const TRANSCRIPT: Pixels = px(736.);
    /// The empty space a user bubble keeps on its leading side (SwiftUI Spacer).
    pub const BUBBLE_INDENT: Pixels = px(42.);
    /// A reply quote or fork label inside the transcript.
    pub const QUOTE: Pixels = px(420.);
    /// The composer floating over a full-width document.
    pub const COMPOSER_FLOAT: Pixels = px(480.);
    /// The toast card.
    pub const TOAST: Pixels = px(420.);
    /// Empty-state and hero prose.
    pub const NOTE: Pixels = px(360.);
    pub const POPOVER: Pixels = px(300.);
    pub const DIALOG: Pixels = px(480.);
    /// Dialogs laid out as a settings form, such as a routine.
    pub const DIALOG_FORM: Pixels = px(520.);
    pub const PALETTE: Pixels = px(600.);
    /// A text field or select in a settings row's trailing slot.
    pub const FIELD: Pixels = px(280.);
    /// A number stepper in a row's trailing slot.
    pub const STEPPER: Pixels = px(168.);
    /// A colour's swatch and hex in a row's trailing slot.
    pub const COLOR: Pixels = px(116.);
    pub const CHAT_MIN: Pixels = px(360.);
    pub const DOC_MIN: Pixels = px(300.);
    pub const DOC_MAX: Pixels = px(800.);
}
