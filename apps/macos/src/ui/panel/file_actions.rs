//! Native macOS opening and path actions shared by every file viewer.
use super::*;
use gpui_component::button::{ButtonCustomVariant, ButtonVariant};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSWorkspace};
use objc2_foundation::{NSDictionary, NSString, NSURL};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A native preview's local backing file and its original, user-facing path.
#[derive(Clone)]
pub(in crate::ui) struct FileDoc {
    /// Local file passed to macOS, including downloaded gateway copies.
    pub(super) path: PathBuf,
    /// Original local or gateway path used by the path label and Copy Path.
    pub(super) display_path: String,
    default_application: Option<PathBuf>,
    application_icon: Option<Arc<gpui::Image>>,
}

impl FileDoc {
    pub(super) fn local(path: PathBuf) -> Self {
        let display_path = path.to_string_lossy().into_owned();
        Self::new(path, display_path)
    }

    pub(super) fn new(path: PathBuf, display_path: String) -> Self {
        let default_application = NSURL::from_file_path(&path)
            .and_then(|url| NSWorkspace::sharedWorkspace().URLForApplicationToOpenURL(&url))
            .and_then(|url| url.to_file_path());
        let application_icon = default_application.as_deref().and_then(application_icon);
        Self {
            path,
            display_path,
            default_application,
            application_icon,
        }
    }
}

impl Workspace {
    pub(super) fn file_actions(file: &FileDoc, display_path: &str, cx: &Context<Self>) -> Div {
        let path = &file.path;
        let p = palette(cx);
        let segment = ButtonVariant::Custom(
            ButtonCustomVariant::new(cx)
                .color(gpui::transparent_black())
                .foreground(p.text)
                .hover(p.card.blend(p.hover))
                .active(p.card.blend(p.pressed)),
        );
        let label = file
            .default_application
            .as_ref()
            .and_then(|path| path.file_stem())
            .map_or_else(
                || "Open…".to_owned(),
                |name| format!("Open in {}", name.to_string_lossy()),
            );
        let copy = display_path.to_owned();
        let open = path.to_owned();
        let reveal = path.to_owned();
        let discover = path.to_owned();
        div()
            .flex()
            .items_center()
            .gap(space::XS)
            .flex_shrink_0()
            .child(
                toolbar_button("copy-file-path", "Copy path", "hi.copy", cx).on_click(
                    move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone())), // rules-allow: every click hands the clipboard its own String
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .pill_corners()
                    .bg(p.card)
                    .border_1()
                    .border_color(palette(cx).separator)
                    .overflow_hidden()
                    .child(
                        pill("open-file-system")
                            .label("Open")
                            .image(file.application_icon.as_ref().map(Arc::clone))
                            .button(cx)
                            .with_variant(segment)
                            .h(height::REGULAR)
                            .pill_corners()
                            .tooltip(label)
                            .on_click(move |_, _, cx| cx.open_with_system(&open)),
                    )
                    .child(
                        toolbar_button(
                            "open-file-with",
                            "Open with",
                            chevron(false, &palette(cx)),
                            cx,
                        )
                        .with_variant(segment)
                        .border_0()
                        .pill_corners()
                        .on_click(move |_, _, _| crate::host::platform::open_with_menu(&discover)),
                    ),
            )
            .child(
                toolbar_button("reveal-file", "Reveal in Finder", "hi.folder", cx)
                    .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
            )
    }
}

/// AppKit resolves bundle icons, including custom and system application icons.
fn application_icon(application: &Path) -> Option<Arc<gpui::Image>> {
    let path = NSString::from_str(application.to_str()?);
    let image = NSWorkspace::sharedWorkspace().iconForFile(&path);
    let data = image.TIFFRepresentation()?;
    let bitmap = NSBitmapImageRep::imageRepWithData(&data)?;
    // SAFETY: An empty dictionary contains no incorrectly typed property values.
    let png = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(Arc::new(gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        png.to_vec(),
    )))
}
