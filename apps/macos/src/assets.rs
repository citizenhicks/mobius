use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

#[derive(rust_embed::RustEmbed)]
#[folder = "assets/icons/"]
#[prefix = "icons/"]
#[include = "*.svg"]
pub struct Assets;

/// The bundled window backgrounds (`assets/NOTICE`), served beside the icons.
#[derive(rust_embed::RustEmbed)]
#[folder = "assets/backgrounds/"]
#[prefix = "backgrounds/"]
#[include = "*.jpg"]
pub struct Backgrounds;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let path = COMPONENT_ICONS
            .iter()
            .find(|(component, _)| *component == path)
            .map_or(path, |(_, ours)| *ours);
        Ok(Self::get(path)
            .or_else(|| Backgrounds::get(path))
            .map(|file| file.data))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Self::iter()
            .chain(Backgrounds::iter())
            .filter(|name| name.starts_with(path))
            .map(Into::into)
            .collect())
    }
}

/// gpui-component's built-in glyphs drawn with the app's artwork instead.
static COMPONENT_ICONS: &[(&str, &str)] = &[
    ("icons/check.svg", "icons/hi.check.svg"),
    ("icons/chevron-right.svg", "icons/hi.caretRight.svg"),
    ("icons/chevron-down.svg", "icons/hi.caretDown.svg"),
    ("icons/chevron-up.svg", "icons/hi.caretUp.svg"),
    ("icons/chevrons-up-down.svg", "icons/hi.caretDown.svg"),
    ("icons/close.svg", "icons/hi.x.svg"),
    ("icons/window-close.svg", "icons/hi.x.svg"),
    ("icons/search.svg", "icons/hi.magnifyingGlass.svg"),
    ("icons/plus.svg", "icons/hi.plus.svg"),
    ("icons/minus.svg", "icons/hi.minus.svg"),
    ("icons/loader.svg", "icons/mobius.spinner.svg"),
    ("icons/loader-circle.svg", "icons/mobius.spinner.svg"),
    ("icons/info.svg", "icons/hi.info.svg"),
    ("icons/triangle-alert.svg", "icons/hi.warning.svg"),
    ("icons/circle-x.svg", "icons/hi.xCircle.svg"),
    ("icons/circle-check.svg", "icons/hi.checkCircle.svg"),
    ("icons/copy.svg", "icons/hi.copy.svg"),
    ("icons/ellipsis.svg", "icons/hi.dotsThree.svg"),
    ("icons/eye-off.svg", "icons/hi.eyeOff.svg"),
    ("icons/external-link.svg", "icons/hi.arrowUpRight01.svg"),
    ("icons/folder.svg", "icons/hi.folder.svg"),
    ("icons/folder-open.svg", "icons/hi.folderOpen.svg"),
    ("icons/globe.svg", "icons/hi.globe02.svg"),
    ("icons/bell.svg", "icons/hi.bell.svg"),
    ("icons/settings.svg", "icons/hi.gear.svg"),
    ("icons/menu.svg", "icons/hi.menu.svg"),
    ("icons/calendar.svg", "icons/hi.calendarDots.svg"),
    ("icons/file-text.svg", "icons/hi.fileText.svg"),
    ("icons/arrow-up.svg", "icons/hi.arrowUp.svg"),
    ("icons/arrow-down.svg", "icons/hi.arrowDown.svg"),
];
