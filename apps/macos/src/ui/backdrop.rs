//! Window backgrounds (Settings → Appearance → Background): one of Hiroshige's moon scenes,
//! the person's own picture or the desktop, behind translucent surfaces. GPUI has no
//! backdrop blur, so a blurred copy of the picture is made once as it loads, and frosted
//! surfaces paint their slice of it aligned to the window.

use super::*;
use crate::numeric::{Saturating, ToFloat};
use anyhow::Context as _;
use image::{ImageReader, RgbaImage, imageops};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use theme::Veil;

/// What fills the window behind the interface.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Transparent,
    Kanazawa,
    Ishiyama,
    Ryogoku,
    Tama,
    /// The person's picture at `Background::image`.
    Custom,
    /// The solid window; also any background a later version adds.
    #[default]
    #[serde(other)]
    None,
}

impl Source {
    /// The picker's choices before the person's own picture.
    pub const CHOICES: [Self; 6] = [
        Self::None,
        Self::Transparent,
        Self::Kanazawa,
        Self::Ishiyama,
        Self::Ryogoku,
        Self::Tama,
    ];

    /// A bundled scene's label, embedded file and title (`assets/NOTICE`).
    fn scene(self) -> Option<(&'static str, &'static str, &'static str)> {
        Some(match self {
            Self::Kanazawa => (
                "Kanazawa",
                "backgrounds/kanazawa-moon.jpg",
                "Panorama of the Eight Views of Kanasawa under a Full Moon, 1857",
            ),
            Self::Ishiyama => (
                "Ishiyama",
                "backgrounds/ishiyama-moon.jpg",
                "The Autumn Moon at Ishiyama on Lake Biwa, ca. 1835",
            ),
            Self::Ryogoku => (
                "Ryōgoku",
                "backgrounds/ryogoku-moon.jpg",
                "Twilight Moon at Ryōgoku Bridge",
            ),
            Self::Tama => (
                "Tama River",
                "backgrounds/tama-moon.jpg",
                "Autumn Moon on the Tama River, ca. 1838",
            ),
            Self::None | Self::Transparent | Self::Custom => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Transparent => "Transparent",
            Self::Custom => "Your picture",
            scene => scene.scene().map_or("None", |(label, ..)| label),
        }
    }

    /// The print a scene shows, credited to the artist.
    pub fn credit(self) -> Option<String> {
        self.scene()
            .map(|(_, _, title)| format!("Utagawa Hiroshige, {title}."))
    }

    fn is_picture(self) -> bool {
        !matches!(self, Self::None | Self::Transparent)
    }
}

/// The texture over the picture or desktop: none, a fine dot matrix, or scanlines over the
/// page that fade down the window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Texture {
    Off,
    Dots,
    #[default]
    Lines,
}

impl Texture {
    pub const CHOICES: [Self; 3] = [Self::Off, Self::Dots, Self::Lines];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Dots => "Dots",
            Self::Lines => "Lines",
        }
    }
}

/// Reads a texture, or the earlier `pixel` switch: on was the dot matrix.
impl<'de> Deserialize<'de> for Texture {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Saved {
            Pixel(bool),
            Texture(String),
        }
        Ok(match Saved::deserialize(deserializer)? {
            Saved::Pixel(true) => Self::Dots,
            Saved::Pixel(false) => Self::Off,
            Saved::Texture(name) => match name.as_str() {
                "off" => Self::Off,
                "dots" => Self::Dots,
                _ => Self::Lines,
            },
        })
    }
}

/// A picture's dim toward the page colour, in percent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(from = "u8")]
pub struct Dim(u8);

impl Dim {
    pub const MAX: u8 = 90;

    pub fn percent(self) -> u8 {
        self.0
    }
}

impl From<u8> for Dim {
    fn from(percent: u8) -> Self {
        Self(percent.min(Self::MAX))
    }
}

/// How much of the window covers the desktop, in percent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(from = "u8")]
pub struct Opacity(u8);

impl Opacity {
    pub const MIN: u8 = 50;
    pub const MAX: u8 = 100;

    pub fn percent(self) -> u8 {
        self.0
    }

    fn is_opaque(self) -> bool {
        self.0 == Self::MAX
    }
}

impl From<u8> for Opacity {
    fn from(percent: u8) -> Self {
        Self(percent.clamp(Self::MIN, Self::MAX))
    }
}

fn fraction(percent: u8) -> f32 {
    f32::from(percent) / 100.
}

/// One appearance's window background, saved with its colours.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct Background {
    pub source: Source,
    /// The person's own picture, offered again while another background shows.
    #[serde(serialize_with = "save_path", deserialize_with = "load_path")]
    pub image: Option<Arc<Path>>,
    /// `None` keeps the appearance's default.
    pub dim: Option<Dim>,
    /// The window over the desktop when Transparent.
    pub opacity: Opacity,
    /// The window over the desktop under a picture.
    pub picture_opacity: Opacity,
    /// Cards and the composer blur the picture behind them.
    pub frosted: bool,
    #[serde(alias = "pixel")]
    pub texture: Texture,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            source: Source::None,
            image: None,
            dim: None,
            opacity: Opacity(80),
            picture_opacity: Opacity(Opacity::MAX),
            frosted: true,
            texture: Texture::default(),
        }
    }
}

#[expect(
    clippy::ref_option,
    reason = "serde's serialize_with passes the field by reference"
)]
fn save_path<S: Serializer>(path: &Option<Arc<Path>>, serializer: S) -> Result<S::Ok, S::Error> {
    path.as_deref().serialize(serializer)
}

fn load_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Arc<Path>>, D::Error> {
    Ok(Option::<PathBuf>::deserialize(deserializer)?.map(Arc::from))
}

impl Background {
    /// Dark pictures sit deeper than light ones, which fade toward the page instead.
    pub fn dim(&self, is_dark: bool) -> Dim {
        self.dim.unwrap_or(Dim(if is_dark { 55 } else { 30 }))
    }

    /// The opacity setting of the current source.
    pub fn window_opacity(&self) -> Opacity {
        if self.source == Source::Transparent {
            self.opacity
        } else {
            self.picture_opacity
        }
    }

    pub fn set_window_opacity(&mut self, opacity: Opacity) {
        if self.source == Source::Transparent {
            self.opacity = opacity;
        } else {
            self.picture_opacity = opacity;
        }
    }

    pub fn shows_picture(&self) -> bool {
        self.source.is_picture()
    }

    fn picture(&self) -> Option<Picture> {
        match self.source {
            Source::Custom => self
                .image
                .as_ref()
                .map(|path| Picture::File(Arc::clone(path))),
            source => source.scene().map(|(_, file, _)| Picture::Bundled(file)),
        }
    }
}

/// Where a picture's bytes come from.
#[derive(Clone, Debug, PartialEq)]
enum Picture {
    Bundled(&'static str),
    File(Arc<Path>),
}

/// A loaded picture: graded from sharp at the top into frost lower down, its dot matrix,
/// and the frosted copy surfaces paint.
struct Pictures {
    graded: Arc<RenderImage>,
    dotted: Arc<RenderImage>,
    frosted: Arc<RenderImage>,
}

/// The longest side kept from a person's picture; the bundled ones already fit.
const LONGEST_SIDE: u32 = 2880;
/// The frosted copy is blurred at this width and stretched over the window, which keeps the
/// blur wide and cheap.
const FROST_WIDTH: u32 = 320;
const FROST_SIGMA: f32 = 6.;
/// The dot matrix: each cell of this many picture pixels becomes one dot of its average
/// colour, with the last row and column of the cell left as a darker gutter.
const DOT_CELL: u32 = 4;
const DOT_GUTTER: f32 = 0.4;
/// Down the picture its frost eases in between these heights and darkens by up to
/// `GRADE_SHADE`, so the composer sits on frost.
const GRADE_FROM: f32 = 0.35;
const GRADE_TO: f32 = 0.85;
const GRADE_SHADE: f32 = 0.2;
/// Scanlines: one `LINE_WIDTH` line every `LINE_PITCH`, at `LINE_ALPHA` (light, dark) at the
/// top of the window and gone by `LINE_FADE` of its height.
const LINE_PITCH: f32 = 2.;
const LINE_WIDTH: f32 = 0.5;
const LINE_ALPHA: [f32; 2] = [0.18, 0.4];
const LINE_FADE: f32 = 0.7;

fn smoothstep(from: f32, to: f32, t: f32) -> f32 {
    let t = ((t - from) / (to - from)).clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// The picture blended row by row into its frost, which darkens toward the bottom.
fn graded(image: &RgbaImage, frosted: &RgbaImage) -> RgbaImage {
    let (width, height) = image.dimensions();
    let mut graded = imageops::resize(frosted, width, height, imageops::FilterType::Triangle);
    for (y, (row, sharp)) in graded.rows_mut().zip(image.rows()).enumerate() {
        let mix = smoothstep(GRADE_FROM, GRADE_TO, y.to_f32() / height.to_f32());
        let shade = 1. - GRADE_SHADE * mix;
        for (pixel, sharp) in row.zip(sharp) {
            for (channel, sharp) in pixel.0.iter_mut().zip(sharp.0).take(3) {
                let blend = f32::from(sharp) * (1. - mix) + f32::from(*channel) * mix;
                *channel = (blend * shade).saturating_u8();
            }
        }
    }
    graded
}

/// The picture as a dot matrix: averaged cells with a dim gutter between the dots.
fn dot_matrix(image: &RgbaImage) -> RgbaImage {
    let (width, height) = image.dimensions();
    let mut dotted = RgbaImage::new(width, height);
    for cell_y in (0..height).step_by(DOT_CELL as usize) {
        for cell_x in (0..width).step_by(DOT_CELL as usize) {
            let (cell_w, cell_h) = (DOT_CELL.min(width - cell_x), DOT_CELL.min(height - cell_y));
            let mut sum = [0u32; 4];
            for y in cell_y..cell_y + cell_h {
                for x in cell_x..cell_x + cell_w {
                    for (total, channel) in sum.iter_mut().zip(image.get_pixel(x, y).0) {
                        *total += u32::from(channel);
                    }
                }
            }
            let count = cell_w * cell_h;
            let dot = sum.map(|total| u8::try_from(total / count).unwrap_or(u8::MAX));
            let [red, green, blue, alpha] = dot;
            let shade = |channel: u8| (f32::from(channel) * DOT_GUTTER).saturating_u8();
            let gutter = [shade(red), shade(green), shade(blue), alpha];
            for y in cell_y..cell_y + cell_h {
                for x in cell_x..cell_x + cell_w {
                    let edge = x - cell_x == DOT_CELL - 1 || y - cell_y == DOT_CELL - 1;
                    let pixel = if edge { gutter } else { dot };
                    dotted.put_pixel(x, y, image::Rgba(pixel));
                }
            }
        }
    }
    dotted
}

fn decode(picture: &Picture) -> anyhow::Result<Pictures> {
    let image = match picture {
        Picture::Bundled(file) => image::load_from_memory(
            &crate::assets::Backgrounds::get(file)
                .with_context(|| format!("{file} is not bundled"))?
                .data,
        )?,
        Picture::File(path) => ImageReader::open(path)?.with_guessed_format()?.decode()?,
    };
    let image = if image.width().max(image.height()) > LONGEST_SIDE {
        image.resize(LONGEST_SIDE, LONGEST_SIDE, imageops::FilterType::Triangle)
    } else {
        image
    };
    let image = image.into_rgba8();
    let height = (FROST_WIDTH * image.height() / image.width().max(1)).max(1);
    let frosted = imageops::fast_blur(
        &imageops::thumbnail(&image, FROST_WIDTH, height),
        FROST_SIGMA,
    );
    let dotted = dot_matrix(&image);
    Ok(Pictures {
        graded: texture(graded(&image, &frosted)),
        dotted: texture(dotted),
        frosted: texture(frosted),
    })
}

/// GPUI's image textures are BGRA.
fn texture(mut image: RgbaImage) -> Arc<RenderImage> {
    for pixel in image.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Arc::new(RenderImage::new([image::Frame::new(image)]))
}

fn release(pictures: &Pictures, cx: &mut App) {
    cx.drop_image(Arc::clone(&pictures.graded), None);
    cx.drop_image(Arc::clone(&pictures.dotted), None);
    cx.drop_image(Arc::clone(&pictures.frosted), None);
}

/// macOS Accessibility → Display → Reduce transparency.
fn reduce_transparency() -> bool {
    let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
    // SAFETY: a BOOL property of NSWorkspace (NSWorkspaceAccessibilityDisplay), read on the
    // main thread.
    unsafe { objc2::msg_send![&*workspace, accessibilityDisplayShouldReduceTransparency] }
}

/// What surfaces paint behind themselves.
#[derive(Default)]
struct Scenery {
    pictures: Option<Arc<Pictures>>,
    /// The dim over the picture: the page colour at the chosen strength.
    scrim: Hsla,
    /// The picture's opacity over the desktop.
    opacity: f32,
    frosted: bool,
    texture: Texture,
    desktop: bool,
    /// The scanlines at the top of the window.
    lines: Hsla,
}

impl Global for Scenery {}

/// Background package state; the settings rows live in `pages::background`.
#[derive(Default)]
pub(in crate::ui) struct BackdropUi {
    pub(in crate::ui) dark: Background,
    pub(in crate::ui) light: Background,
    /// The picture on screen; it stays while the next one loads.
    shown: Option<(Picture, Arc<Pictures>)>,
    loading: Option<Picture>,
    /// The window background the palette asked for, and the one the window has.
    window: WindowBackgroundAppearance,
    applied: WindowBackgroundAppearance,
    pub(in crate::ui) reduce_transparency: bool,
    /// The Dim and Opacity sliders of the Appearance page.
    pub(in crate::ui) sliders: Option<Sliders>,
}

/// The settings sliders, made for one appearance and one kind of source.
pub(in crate::ui) struct Sliders {
    /// Dark appearance, Transparent source.
    pub(in crate::ui) made_for: (bool, bool),
    pub(in crate::ui) dim: Entity<gpui_component::slider::SliderState>,
    pub(in crate::ui) opacity: Entity<gpui_component::slider::SliderState>,
    pub(in crate::ui) _subscriptions: [Subscription; 2],
}

impl BackdropUi {
    pub(in crate::ui) fn restored(preferences: &mut preferences::Preferences) -> Self {
        Self {
            dark: std::mem::take(&mut preferences.dark_background),
            light: std::mem::take(&mut preferences.light_background),
            ..Self::default()
        }
    }

    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn is_loading(&self) -> bool {
        self.loading.is_some()
    }
}

impl Workspace {
    /// The window background of the appearance on screen.
    pub(in crate::ui) fn background(&self) -> &Background {
        if self.palette.is_dark() {
            &self.config.backdrop.dark
        } else {
            &self.config.backdrop.light
        }
    }

    pub(in crate::ui) fn background_mut(&mut self) -> &mut Background {
        if self.palette.is_dark() {
            &mut self.config.backdrop.dark
        } else {
            &mut self.config.backdrop.light
        }
    }

    /// Veils `palette` for its appearance's background and starts loading its picture.
    pub(super) fn dress(&mut self, palette: Palette, cx: &mut Context<Self>) -> Palette {
        let is_dark = palette.is_dark();
        let reduce = reduce_transparency();
        self.config.backdrop.reduce_transparency = reduce;
        let background = if is_dark {
            &self.config.backdrop.dark
        } else {
            &self.config.backdrop.light
        };
        let (source, dim, opacity, frosted, texture) = (
            background.source,
            background.dim(is_dark),
            background.window_opacity(),
            background.frosted,
            background.texture,
        );
        let picture = background.picture().filter(|_| !reduce);
        match picture {
            None => {
                if let Some((_, pictures)) = self.config.backdrop.shown.take() {
                    release(&pictures, cx);
                }
            }
            Some(picture) => {
                let shown =
                    matches!(&self.config.backdrop.shown, Some((shown, _)) if *shown == picture);
                if !shown && self.config.backdrop.loading.as_ref() != Some(&picture) {
                    self.load_picture(picture, cx);
                }
            }
        }
        let pictures = self
            .config
            .backdrop
            .shown
            .as_ref()
            .map(|(_, pictures)| Arc::clone(pictures));
        let veil = match source {
            _ if reduce => None,
            Source::None => None,
            Source::Transparent => (!opacity.is_opaque()).then(|| Veil::Desktop {
                opacity: fraction(opacity.percent()),
            }),
            _ => pictures.is_some().then(|| Veil::Picture {
                dim: fraction(dim.percent()),
            }),
        };
        self.config.backdrop.window = match veil {
            Some(Veil::Desktop { .. }) => WindowBackgroundAppearance::Blurred,
            Some(Veil::Picture { .. }) if !opacity.is_opaque() => {
                WindowBackgroundAppearance::Blurred
            }
            _ => WindowBackgroundAppearance::Opaque,
        };
        let [light_lines, dark_lines] = LINE_ALPHA;
        cx.set_global(Scenery {
            pictures: pictures.filter(|_| veil.is_some()),
            scrim: palette.canvas.alpha(fraction(dim.percent())),
            opacity: fraction(opacity.percent()),
            frosted,
            texture,
            desktop: matches!(veil, Some(Veil::Desktop { .. })),
            lines: gpui::black().alpha(if is_dark { dark_lines } else { light_lines }),
        });
        veil.map_or(palette, |veil| palette.veiled(veil))
    }

    fn load_picture(&mut self, picture: Picture, cx: &Context<Self>) {
        let request = Picture::clone(&picture);
        self.config.backdrop.loading = Some(picture);
        let decoding = cx.background_spawn(async move {
            let decoded = decode(&request);
            (request, decoded)
        });
        cx.spawn(async move |this, cx| {
            let (picture, decoded) = decoding.await;
            if let Err(error) = this.update(cx, |this, cx| this.picture_loaded(picture, decoded, cx))
            {
                tracing::debug!(%error, "the workspace closed before its background picture loaded");
            }
        })
        .detach();
    }

    fn picture_loaded(
        &mut self,
        picture: Picture,
        decoded: anyhow::Result<Pictures>,
        cx: &mut Context<Self>,
    ) {
        if self.config.backdrop.loading.as_ref() == Some(&picture) {
            self.config.backdrop.loading = None;
        }
        match decoded {
            Ok(pictures) => {
                if self.background().picture().as_ref() == Some(&picture) {
                    let shown = self
                        .config
                        .backdrop
                        .shown
                        .replace((picture, Arc::new(pictures)));
                    if let Some((_, previous)) = shown {
                        release(&previous, cx);
                    }
                }
            }
            Err(error) => {
                // A missing or unreadable picture leaves a plain window, once.
                for background in [
                    &mut self.config.backdrop.dark,
                    &mut self.config.backdrop.light,
                ] {
                    if background.picture().as_ref() == Some(&picture) {
                        background.source = Source::None;
                        if matches!(picture, Picture::File(_)) {
                            background.image = None;
                        }
                    }
                }
                self.save_backgrounds(cx);
                self.announce(
                    Tone::Error,
                    format!("Couldn't show the background picture: {error:#}"),
                );
            }
        }
        self.refresh_appearance(cx);
    }

    pub(in crate::ui) fn save_backgrounds(&mut self, cx: &Context<Self>) {
        let (dark, light) = (
            Background::clone(&self.config.backdrop.dark),
            Background::clone(&self.config.backdrop.light),
        );
        self.save_preferences(
            "the background",
            move |preferences| {
                preferences.dark_background = dark;
                preferences.light_background = light;
            },
            cx,
        );
    }

    pub(in crate::ui) fn set_background_source(&mut self, source: Source, cx: &mut Context<Self>) {
        self.background_mut().source = source;
        self.save_backgrounds(cx);
        self.refresh_appearance(cx);
    }

    /// "Choose image…": the native open panel, then the picture as this appearance's
    /// background.
    pub(in crate::ui) fn choose_background_image(window: &Window, cx: &Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Use as Background".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
                && let Err(error) = this.update(cx, |this, cx| {
                    this.background_mut().image = Some(Arc::from(path));
                    this.set_background_source(Source::Custom, cx);
                })
            {
                tracing::debug!(%error, "the workspace closed before the chosen picture was applied");
            }
        })
        .detach();
    }

    /// Gives the window the background the palette was dressed for.
    pub(super) fn sync_window_background(&mut self, window: &Window) {
        if self.config.backdrop.applied != self.config.backdrop.window {
            self.config.backdrop.applied = self.config.backdrop.window;
            window.set_background_appearance(self.config.backdrop.window);
        }
    }
}

/// The window's bottom layer: the picture under its dim, then the frame colour.
pub(super) fn layer(p: &Palette, cx: &App) -> Div {
    div()
        .absolute()
        .inset_0()
        .children(picture(false, px(0.), cx))
        .children(desktop_texture(cx))
        .child(div().absolute().inset_0().bg(p.chrome))
}

/// Native desktop blur stays live; only its dot-matrix gutters are painted here.
fn desktop_texture(cx: &App) -> Option<Div> {
    let scenery = cx.try_global::<Scenery>()?;
    if !scenery.desktop || scenery.texture != Texture::Dots {
        return None;
    }
    Some(
        div().absolute().inset_0().child(
            canvas(
                |_, _, _| {},
                |bounds, (), window, _| {
                    let gutter = gpui::black().alpha(1. - DOT_GUTTER);
                    for x in (DOT_CELL - 1..f32::from(bounds.size.width).ceil().saturating_u32())
                        .step_by(DOT_CELL as usize)
                    {
                        let stripe = Bounds::new(
                            bounds.origin + point(px(x.to_f32()), px(0.)),
                            size(px(1.), bounds.size.height),
                        );
                        window.paint_quad(fill(stripe, gutter));
                    }
                    for y in (DOT_CELL - 1..f32::from(bounds.size.height).ceil().saturating_u32())
                        .step_by(DOT_CELL as usize)
                    {
                        let stripe = Bounds::new(
                            bounds.origin + point(px(0.), px(y.to_f32())),
                            size(bounds.size.width, px(1.)),
                        );
                        window.paint_quad(fill(stripe, gutter));
                    }
                },
            )
            .size_full(),
        ),
    )
}

/// Scanlines over the page, strongest at the top of the window and fading down it, over a
/// picture or the desktop. The parent must be positioned.
pub(super) fn scanlines(cx: &App) -> Option<Div> {
    let scenery = cx.try_global::<Scenery>()?;
    if scenery.texture != Texture::Lines || !(scenery.desktop || scenery.pictures.is_some()) {
        return None;
    }
    let lines = scenery.lines;
    Some(
        div().absolute().inset_0().child(
            canvas(
                |_, _, _| {},
                move |bounds, (), window, _| {
                    let window_height = f32::from(window.viewport_size().height).max(1.);
                    let top = f32::from(bounds.origin.y);
                    let bottom =
                        (top + f32::from(bounds.size.height)).min(window_height * LINE_FADE);
                    let mut y = (top / LINE_PITCH).ceil() * LINE_PITCH;
                    // ponytail: one quad per line, about 300 a frame; bake a tile if it shows.
                    while y < bottom {
                        let fade = 1. - smoothstep(0., LINE_FADE, y / window_height);
                        let line = Bounds::new(
                            point(bounds.origin.x, px(y)),
                            size(bounds.size.width, px(LINE_WIDTH)),
                        );
                        window.paint_quad(fill(line, lines.alpha(lines.a * fade)));
                        y += LINE_PITCH;
                    }
                },
            )
            .size_full(),
        ),
    )
}

/// A card, group or composer, with tile corners: frosted over a picture, otherwise its own
/// fill. The surface must be positioned, as the frost fills it.
pub(super) fn surface<E: Styled + ParentElement>(surface: E, fill: Hsla, cx: &App) -> E {
    let surface = surface.tile_corners();
    match frost(fill, cx) {
        Some(frost) => surface.child(frost),
        None => surface.bg(fill),
    }
}

/// A card floating over other content, with tile corners: frosted over a picture,
/// otherwise opaque.
pub(super) fn floating<E: Styled + ParentElement>(surface: E, fill: Hsla, cx: &App) -> E {
    let surface = surface.tile_corners();
    match frost(fill, cx) {
        Some(frost) => surface.child(frost),
        None => surface.bg(fill.alpha(1.)),
    }
}

/// Whether surfaces have something solid painted beneath their own fill.
pub(super) fn frosted(cx: &App) -> bool {
    cx.try_global::<Scenery>()
        .is_some_and(|scenery| scenery.frosted && scenery.pictures.is_some())
}

fn frost(fill: Hsla, cx: &App) -> Option<Div> {
    if !frosted(cx) {
        return None;
    }
    let slice = picture(true, radius::TILE, cx)?;
    Some(
        div()
            .absolute()
            .inset_0()
            .child(slice)
            .child(div().absolute().inset_0().tile_corners().bg(fill)),
    )
}

/// The window's picture under its dim, painted where this element lies.
fn picture(frosted: bool, radius: Pixels, cx: &App) -> Option<Div> {
    let scenery = cx.try_global::<Scenery>()?;
    let scrim = scenery.scrim;
    let pictures = scenery.pictures.as_ref()?;
    let image = Arc::clone(if frosted {
        &pictures.frosted
    } else if scenery.texture == Texture::Dots {
        &pictures.dotted
    } else {
        &pictures.graded
    });
    let image_size = image.size(0);
    Some(
        div().absolute().inset_0().opacity(scenery.opacity).child(
            canvas(
                |_, _, _| {},
                move |bounds, (), window, _| {
                    let cover = cover(window.viewport_size(), image_size);
                    let corners = Corners::all(radius);
                    // A texture that fails to upload leaves the dim alone over the frame.
                    if let Err(error) = window.paint_image(bounds, cover, corners, image, 0, false)
                    {
                        tracing::debug!(%error, "the background texture did not upload");
                    }
                    window.paint_quad(fill(bounds, scrim).corner_radii(corners));
                },
            )
            .size_full(),
        ),
    )
}

/// The picture's bounds when it covers the window, centred.
fn cover(window: Size<Pixels>, image: Size<DevicePixels>) -> Bounds<Pixels> {
    let (width, height) = (
        image.width.0.max(1).to_f32(),
        image.height.0.max(1).to_f32(),
    );
    let (window_width, window_height) = (f32::from(window.width), f32::from(window.height));
    let scale = (window_width / width).max(window_height / height);
    let size = size(px(width * scale), px(height * scale));
    Bounds::new(
        point(
            px((window_width - f32::from(size.width)) / 2.),
            px((window_height - f32::from(size.height)) / 2.),
        ),
        size,
    )
}

#[cfg(test)]
mod tests {
    use super::{Background, Dim, Opacity, Source, Texture};
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn the_earlier_pixel_switch_loads_as_a_texture() {
        let texture = |json: &str| serde_json::from_str::<Background>(json).unwrap().texture;
        assert_eq!(texture(r#"{"pixel":true}"#), Texture::Dots);
        assert_eq!(texture(r#"{"pixel":false}"#), Texture::Off);
        assert_eq!(texture(r#"{"texture":"lines"}"#), Texture::Lines);
        assert_eq!(texture("{}"), Texture::Lines);
    }

    #[test]
    fn backgrounds_round_trip_and_clamp_their_levels() {
        let background = Background {
            source: Source::Custom,
            image: Some(Arc::from(Path::new("/Pictures/moon.png"))),
            dim: Some(Dim::from(70)),
            opacity: Opacity::from(60),
            picture_opacity: Opacity::from(90),
            frosted: false,
            texture: Texture::Dots,
        };
        let restored: Background =
            serde_json::from_slice(&serde_json::to_vec(&background).unwrap()).unwrap();
        assert_eq!(restored, background);
        let clamped: Background = serde_json::from_str(
            r#"{"source":"tama","dim":200,"opacity":10,"picture_opacity":255}"#,
        )
        .unwrap();
        assert_eq!(clamped.source, Source::Tama);
        assert_eq!(clamped.dim(true).percent(), Dim::MAX);
        assert_eq!(clamped.opacity.percent(), Opacity::MIN);
        assert_eq!(clamped.picture_opacity.percent(), Opacity::MAX);
        assert!(clamped.frosted);
        let future: Background = serde_json::from_str(r#"{"source":"aurora"}"#).unwrap();
        assert_eq!(future, Background::default());
    }
}
