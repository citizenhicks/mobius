use gpui::{App, Global, Hsla, Rgba, rgb};
use gpui_component::{Theme, ThemeMode};
use mobius::protocol::FrontendTone;
use mobius_gateway::wire::ProviderTint;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    #[serde(alias = "lights_out")]
    Dark,
}

impl Appearance {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn is_dark(self, system: bool) -> bool {
        match self {
            Self::System => system,
            Self::Light => false,
            Self::Dark => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Accent {
    #[default]
    Blue,
    Teal,
    Green,
    Yellow,
    Orange,
    Red,
    Purple,
    White,
}

impl Accent {
    pub const ALL: [Self; 8] = [
        Self::Blue,
        Self::Teal,
        Self::Green,
        Self::Yellow,
        Self::Orange,
        Self::Red,
        Self::Purple,
        Self::White,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Blue => "Blue",
            Self::Teal => "Teal",
            Self::Green => "Green",
            Self::Yellow => "Yellow",
            Self::Orange => "Orange",
            Self::Red => "Red",
            Self::Purple => "Purple",
            Self::White => "White",
        }
    }

    pub fn color(self) -> Hsla {
        self.rgb().into()
    }

    pub(crate) fn rgb(self) -> Rgba {
        match self {
            Self::Blue => color(0.369, 0.506, 0.675),
            Self::Teal => color(0.561, 0.737, 0.733),
            Self::Green => color(0.639, 0.745, 0.549),
            Self::Yellow => color(0.922, 0.796, 0.545),
            Self::Orange => color(0.816, 0.529, 0.439),
            Self::Red => color(0.749, 0.380, 0.416),
            Self::Purple => color(0.706, 0.557, 0.678),
            // A soft off-white (nord4): pure white glares on faces and dark surfaces.
            Self::White => rgb(0xd8_de_e9),
        }
    }
}

impl From<ProviderTint> for Accent {
    fn from(tint: ProviderTint) -> Self {
        match tint {
            ProviderTint::Blue => Self::Blue,
            ProviderTint::Teal => Self::Teal,
            ProviderTint::Green => Self::Green,
            ProviderTint::Yellow => Self::Yellow,
            ProviderTint::Orange => Self::Orange,
            ProviderTint::Red => Self::Red,
            ProviderTint::Purple => Self::Purple,
            ProviderTint::White => Self::White,
        }
    }
}

impl From<Accent> for ProviderTint {
    fn from(accent: Accent) -> Self {
        match accent {
            Accent::Blue => Self::Blue,
            Accent::Teal => Self::Teal,
            Accent::Green => Self::Green,
            Accent::Yellow => Self::Yellow,
            Accent::Orange => Self::Orange,
            Accent::Red => Self::Red,
            Accent::Purple => Self::Purple,
            Accent::White => Self::White,
        }
    }
}

/// How a window background shows through the surfaces (`Palette::veiled`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Veil {
    /// A picture under a dim toward the page colour, 0 to 1.
    Picture { dim: f32 },
    /// The desktop, with the page at this opacity.
    Desktop { opacity: f32 },
}

/// Over the desktop the titlebar and rail show it a quarter more than the page does.
const DESKTOP_FRAME: f32 = 1.25;
/// The page's own layer over the frame, so together they reach the chosen opacity.
const DESKTOP_SHEET: f32 = 1. - 1. / DESKTOP_FRAME;
/// The least opacity of the frame, the page over it and cards over a picture: enough to
/// keep the picture calm behind the interface even where contrast would allow less.
const PICTURE_FLOOR: [f32; 3] = [0.5, 0.3, 0.55];

/// The background and foreground chosen for one appearance; `None` keeps the built-in one.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Colors {
    pub background: Option<u32>,
    pub foreground: Option<u32>,
}

/// The neutral ramp every surface, line and text colour comes from.
struct Neutrals {
    canvas: Rgba,
    /// The titlebar and rail.
    frame: Rgba,
    sidebar: Rgba,
    recessed: Rgba,
    panel: Rgba,
    raised: Rgba,
    line: Rgba,
    muted: Rgba,
    text: Rgba,
}

impl Neutrals {
    /// Drawn from the chosen background and foreground, or the defaults: surfaces step away
    /// from the background, and the frame, sidebar, lines and secondary text sit between it
    /// and the foreground.
    fn new(is_dark: bool, colors: Colors) -> Self {
        // Light cards step toward the ink; secondary text keeps contrast on those cards.
        let (background, foreground, frame, sidebar, muted) = if is_dark {
            (0x11_11_11, 0xfc_fc_fc, 12. / 235., 4. / 235., 141. / 235.)
        } else {
            (0xff_ff_ff, 0x1a_1c_1f, 9. / 229., 3. / 229., 149. / 229.)
        };
        let background = rgb(colors.background.unwrap_or(background));
        let foreground = rgb(colors.foreground.unwrap_or(foreground));
        let toward = |target: Rgba, amount: f32| background.blend(target.alpha(amount));
        let black = rgb(0);
        let (recessed, panel, raised, line) = if is_dark {
            (
                toward(black, 0.33),
                toward(foreground, 0.05),
                toward(foreground, 0.12),
                toward(foreground, 0.20),
            )
        } else {
            (
                toward(black, 0.03),
                toward(black, 0.03),
                toward(foreground, 0.06),
                toward(foreground, 0.08),
            )
        };
        Self {
            canvas: background,
            frame: toward(foreground, frame),
            sidebar: toward(foreground, sidebar),
            recessed,
            panel,
            raised,
            line,
            muted: toward(foreground, muted),
            text: foreground,
        }
    }
}

/// The Apple client's MobiusPalette, mixed in device RGB before GPUI conversion.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub panel: Hsla,
    pub raised: Hsla,
    pub line: Hsla,
    pub accent: Hsla,
    pub accent_fill: Hsla,
    pub accent_soft: Hsla,
    pub signal: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub muted: Hsla,
    /// Interface text, a step softer than `ink` like macOS's label colour.
    pub text: Hsla,
    /// The full-strength foreground, kept for transcript prose and the colour editor.
    pub ink: Hsla,
    pub on_accent: Hsla,
    /// Vivid fill of filled action buttons (send, dictation, primary actions). White
    /// follows the foreground, so it inverts to near-black on a light page.
    pub button_accent: Hsla,
    /// Titlebar and rail: one recessed L-frame.
    pub chrome: Hsla,
    /// The page: the inset sheet.
    pub sheet: Hsla,
    /// The sidebar floating on the sheet, a step between the frame and cards.
    pub float: Hsla,
    /// Groups, composer, pills and other content surfaces.
    pub card: Hsla,
    /// Secondary buttons, selected segments, popovers and menus.
    pub control: Hsla,
    /// Segmented tracks, progress tracks and code boxes.
    pub inset: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    /// Every 1 px hairline.
    pub separator: Hsla,
    /// The on fill of switches: the accent, or mid grey for White on dark (MobiusPalette).
    pub control_tint: Hsla,
    /// The page colour, always opaque: `sheet` turns translucent over a window background.
    pub canvas: Hsla,
    is_dark: bool,
}

impl Global for Palette {}

impl Palette {
    /// The accent colours controls only; surfaces, lines and text come from `colors`.
    pub fn new(is_dark: bool, tint: Accent, colors: Colors) -> Self {
        // White is ink on a light page; on a dark one it stays its soft off-white.
        let hue = if tint == Accent::White && !is_dark {
            rgb(0)
        } else {
            tint.rgb()
        };
        let (fill, soft, signal, warning, danger) = if is_dark {
            (
                color(0.298, 0.416, 0.557),
                color(0.227, 0.278, 0.349),
                Accent::Green.rgb(),
                Accent::Yellow.rgb(),
                Accent::Red.rgb(),
            )
        } else {
            (
                color(0.239, 0.353, 0.494),
                color(0.831, 0.871, 0.918),
                color(0.353, 0.482, 0.243),
                color(0.565, 0.435, 0.153),
                color(0.639, 0.263, 0.310),
            )
        };
        let Neutrals {
            canvas,
            frame,
            sidebar,
            recessed,
            panel,
            raised,
            line,
            muted,
            text,
        } = Neutrals::new(is_dark, colors);
        let panel: Hsla = panel.into();
        let (accent_fill, accent_soft) = if tint == Accent::Blue {
            (fill.into(), soft.into())
        } else {
            tinted_fills(hue, tint, is_dark, panel)
        };
        let muted: Hsla = muted.into();
        let line: Hsla = line.into();
        let interface_accent: Hsla = hue
            .blend(
                rgb(if is_dark { 0xff_ff_ff } else { 0 }).alpha(if tint == Accent::White {
                    0.
                } else {
                    0.55
                }),
            )
            .into();
        let chrome: Hsla = frame.into();
        let card = if is_dark { panel } else { raised.into() };
        let float: Hsla = sidebar.into();
        let control: Hsla = raised.into();
        let inset = if is_dark { recessed.into() } else { line };
        let label = soften_for_surfaces(
            canvas,
            text,
            [
                chrome,
                float,
                card,
                control,
                inset,
                mix(control, muted, 0.20),
            ],
        );
        Self {
            chrome,
            sheet: canvas.into(),
            float,
            card,
            control,
            inset,
            hover: muted.opacity(0.12),
            pressed: muted.opacity(0.20),
            separator: if is_dark { line.opacity(0.6) } else { line },
            panel,
            raised: raised.into(),
            line,
            control_tint: if tint == Accent::White && is_dark {
                gpui::hsla(0., 0., 0.55, 1.)
            } else {
                interface_accent
            },
            accent: interface_accent,
            accent_fill,
            accent_soft,
            signal: signal.into(),
            warning: warning.into(),
            danger: danger.into(),
            muted,
            text: label,
            ink: text.into(),
            on_accent: rgb(0xec_ef_f4).into(),
            button_accent: button_fill(tint, text),
            canvas: canvas.into(),
            is_dark,
        }
    }

    /// The palette over a window background: the frame, page and cards turn translucent.
    /// Over a picture each surface takes the least opacity (from a calm floor) that keeps
    /// text at 4.5:1 and secondary text at 3:1 over the picture's brightest and darkest
    /// pixels once dimmed; over the desktop the page shows at the chosen opacity.
    pub fn veiled(mut self, veil: Veil) -> Self {
        let (chrome, sheet, card) = match veil {
            Veil::Desktop { opacity } => (
                (1. - (1. - opacity) * DESKTOP_FRAME).max(0.),
                DESKTOP_SHEET,
                opacity,
            ),
            Veil::Picture { dim } => {
                let scrim = self.canvas.to_rgb().alpha(dim);
                let dimmed = [rgb(0xff_ff_ff), rgb(0)].map(|pixel| pixel.blend(scrim));
                let chrome = self.least_opacity(self.chrome, &dimmed, PICTURE_FLOOR[0]);
                let framed = dimmed.map(|pixel| pixel.blend(self.chrome.to_rgb().alpha(chrome)));
                (
                    chrome,
                    self.least_opacity(self.sheet, &framed, PICTURE_FLOOR[1]),
                    self.least_opacity(self.card, &dimmed, PICTURE_FLOOR[2]),
                )
            }
        };
        self.chrome = self.chrome.alpha(chrome);
        self.sheet = self.sheet.alpha(sheet);
        self.card = self.card.alpha(card);
        self.float = self.float.alpha(card);
        self
    }

    /// The least opacity from `floor` at which `surface` over every one of `backdrops`
    /// keeps body text at 4.5:1 and secondary text at 3:1.
    fn least_opacity(&self, surface: Hsla, backdrops: &[Rgba], floor: f32) -> f32 {
        let surface = surface.to_rgb();
        (0..=100_u8)
            .map(|step| f32::from(step) / 100.)
            .filter(|alpha| *alpha >= floor)
            .find(|alpha| {
                backdrops.iter().all(|backdrop| {
                    let composite: Hsla = backdrop.blend(surface.alpha(*alpha)).into();
                    contrast(self.text, composite) >= 4.5 && contrast(self.muted, composite) >= 3.
                })
            })
            .unwrap_or(1.)
    }

    /// Whether surfaces let a window background through.
    pub fn is_veiled(&self) -> bool {
        self.sheet.a < 1.
    }

    pub fn is_dark(&self) -> bool {
        self.is_dark
    }

    /// Unselected SVG glyphs: monochrome, about halfway from the page to the foreground
    /// (#8b8b8b and #898a8b on the default pages), keeping the 3:1 a glyph needs against the sheet.
    /// Selected filled glyphs use `ink`; filled buttons use `on_button_accent`.
    pub fn icon_ink(&self) -> Hsla {
        // Opaque: a veiled sheet's alpha must not carry into the glyphs drawn on it.
        let sheet = Rgba {
            a: 1.,
            ..self.sheet.to_rgb()
        };
        let step = if self.is_dark { 0.52 } else { 0.516 };
        let glyph: Hsla = sheet.blend(self.ink.to_rgb().alpha(step)).into();
        if contrast(glyph, sheet.into()) < 3. {
            self.ink
        } else {
            glyph
        }
    }

    /// Glyph and label on `button_accent`: the foreground, or the page where the
    /// foreground would not read on the fill.
    pub fn on_button_accent(&self) -> Hsla {
        if contrast(self.ink, self.button_accent) >= 3. {
            self.ink
        } else {
            self.canvas
        }
    }

    pub fn tone(&self, tone: FrontendTone) -> Hsla {
        match tone {
            FrontendTone::Neutral => self.text,
            FrontendTone::Success => self.signal,
            FrontendTone::Warning => self.warning,
            FrontendTone::Error => self.danger,
        }
    }

    /// Makes this the app's palette, returning it for the caller to keep.
    pub fn install(self, cx: &mut App) -> Self {
        cx.set_global(self);
        Theme::change(
            if self.is_dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            None,
            cx,
        );
        self.apply_component_theme(Theme::global_mut(cx));
        Theme::sync_base(cx);
        cx.refresh_windows();
        self
    }
    fn apply_component_theme(&self, theme: &mut Theme) {
        // Component text_sm is 0.875 rem, so this rem puts menus, inputs and tooltips on the
        // body size; app sizes use px tokens.
        theme.font_size = super::primitives::Type::Body.size() / 0.875;
        let fonts = super::primitives::FontSet::current();
        theme.font_family = fonts.regular().into();
        theme.mono_font_family = fonts.mono().into();
        theme.mono_font_size = super::primitives::Type::Code.size();
        // The library's only radius settings; its other tiers derive from `radius`, and
        // menus, inputs, tables and kbd use it, while dialogs and notifications use `radius_lg`.
        theme.radius = super::primitives::radius::CONTROL;
        theme.radius_lg = super::primitives::radius::TILE;
        let white: Hsla = rgb(0xff_ff_ff).into();
        let black: Hsla = rgb(0).into();
        let hover = mix(self.control, self.muted, 0.12);
        let pressed = mix(self.control, self.muted, 0.20);
        theme.colors = gpui_component::ThemeColor {
            background: self.canvas,
            foreground: self.text,
            border: self.separator,
            input: self.separator,
            accent: self.muted.opacity(if self.is_dark { 0.24 } else { 0.12 }),
            accent_foreground: self.text,
            primary: self.button_accent,
            primary_hover: mix(self.button_accent, self.on_button_accent(), 0.08),
            primary_active: mix(self.button_accent, black, 0.10),
            primary_foreground: self.on_button_accent(),
            button: self.control,
            button_hover: hover,
            button_active: pressed,
            button_foreground: self.text,
            button_primary: self.button_accent,
            button_primary_hover: mix(self.button_accent, self.on_button_accent(), 0.08),
            button_primary_active: mix(self.button_accent, black, 0.10),
            button_primary_foreground: self.on_button_accent(),
            button_secondary: self.control,
            button_secondary_hover: hover,
            button_secondary_active: pressed,
            button_secondary_foreground: self.text,
            button_danger: self.danger,
            button_danger_hover: mix(self.danger, white, 0.08),
            button_danger_active: mix(self.danger, black, 0.10),
            button_danger_foreground: white,
            danger: self.danger,
            danger_hover: mix(self.danger, white, 0.08),
            danger_active: mix(self.danger, black, 0.10),
            danger_foreground: white,
            secondary: self.control,
            secondary_hover: hover,
            secondary_active: pressed,
            secondary_foreground: self.text,
            muted: self.card,
            muted_foreground: self.muted,
            caret: self.accent,
            ring: self.accent,
            // Links and inline code share the vivid button fill rather than the softened accent.
            link: self.button_accent,
            link_hover: self.button_accent,
            link_active: self.button_accent,
            popover: self.control,
            popover_foreground: self.text,
            list: self.sheet,
            list_hover: self.hover,
            list_active: self.accent_soft,
            list_active_border: self.accent_soft.opacity(0.),
            sidebar: self.sheet,
            sidebar_foreground: self.text,
            sidebar_border: self.separator,
            sidebar_accent: self.accent_soft,
            sidebar_accent_foreground: self.accent,
            switch: self.line,
            switch_thumb: white,
            tab: self.chrome,
            tab_bar: self.chrome,
            tab_active: self.card,
            tab_foreground: self.muted,
            tab_active_foreground: self.text,
            title_bar: self.chrome,
            title_bar_border: self.separator,
            scrollbar: self.muted.opacity(0.),
            scrollbar_thumb: self.muted.opacity(0.35),
            scrollbar_thumb_hover: self.muted.opacity(0.55),
            skeleton: self.muted.opacity(0.18),
            success: self.signal,
            warning: self.warning,
            table: self.canvas.opacity(0.),
            table_head: self.hover,
            table_head_foreground: self.text,
            table_row_border: self.separator,
            ..theme.colors
        };
        theme.tokens = theme.colors.into();
        let style = &mut Arc::make_mut(&mut theme.highlight_theme).style;
        style.editor_background = Some(self.card);
        style.editor_gutter_background = Some(self.card);
        style.editor_active_line = Some(self.card);
        style.editor_foreground = Some(self.text);
        style.editor_line_number = Some(self.muted);
        style.editor_active_line_number = Some(self.muted);
    }
}

/// Mixes `a` toward `b` in device RGB, like the SwiftUI palette's blends.
pub fn mix(a: Hsla, b: Hsla, amount: f32) -> Hsla {
    a.to_rgb().blend(b.to_rgb().alpha(amount)).into()
}

/// `ink` at macOS's label strength (85%) where it remains readable.
fn soften(background: Rgba, ink: Rgba) -> Hsla {
    let label: Hsla = background.blend(ink.alpha(0.85)).into();
    if contrast(label, background.into()) < 4.5 {
        ink.into()
    } else {
        label
    }
}

/// Preserve the contrast of saved colours on surfaces that already met the text threshold.
/// A non-blue accent's filled control colour and its soft tint over `panel`.
/// Filled buttons take the vivid accent; White follows the foreground, so it inverts
/// with the scheme.
fn button_fill(tint: Accent, ink: Rgba) -> Hsla {
    if tint == Accent::White {
        ink
    } else {
        tint.rgb()
    }
    .into()
}

fn tinted_fills(hue: Rgba, tint: Accent, is_dark: bool, panel: Hsla) -> (Hsla, Hsla) {
    let shade = match (is_dark, tint == Accent::White) {
        (true, true) => 0.65,
        (true, false) => 0.5,
        (false, true) => 0.,
        (false, false) => 0.6,
    };
    (
        hue.blend(rgb(0).alpha(shade)).into(),
        panel
            .to_rgb()
            .blend(hue.alpha(if is_dark { 0.10 } else { 0.18 }))
            .into(),
    )
}

fn soften_for_surfaces(background: Rgba, ink: Rgba, surfaces: [Hsla; 6]) -> Hsla {
    let label = soften(background, ink);
    let ink: Hsla = ink.into();
    if surfaces
        .into_iter()
        .any(|surface| contrast(ink, surface) >= 4.5 && contrast(label, surface) < 4.5)
    {
        ink
    } else {
        label
    }
}

fn color(r: f32, g: f32, b: f32) -> Rgba {
    Rgba { r, g, b, a: 1.0 }
}

/// The WCAG contrast ratio between two opaque colours.
pub fn contrast(a: Hsla, b: Hsla) -> f32 {
    let luminance = |color: Hsla| {
        let Rgba { r, g, b, .. } = color.to_rgb();
        let linear = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::numeric::Saturating;

    #[test]
    fn default_neutrals_derive_the_frame_sidebar_and_muted_text() {
        let hex = |color: Rgba| {
            let byte = |c: f32| (c * 255.).round().saturating_u32();
            byte(color.r) << 16 | byte(color.g) << 8 | byte(color.b)
        };
        for (is_dark, expected) in [
            (
                true,
                [
                    0x11_11_11, 0x1d_1d_1d, 0x15_15_15, 0x9e_9e_9e, 0xfc_fc_fc, 0x2d_2d_2d,
                ],
            ),
            (
                false,
                [
                    0xff_ff_ff, 0xf6_f6_f6, 0xfc_fc_fc, 0x6a_6b_6d, 0x1a_1c_1f, 0xf1_f1_f2,
                ],
            ),
        ] {
            let n = Neutrals::new(is_dark, Colors::default());
            let got = [n.canvas, n.frame, n.sidebar, n.muted, n.text, n.raised].map(hex);
            assert_eq!(got, expected, "dark: {is_dark}");
        }
    }
}
