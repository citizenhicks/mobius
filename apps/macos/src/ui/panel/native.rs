//! Native QuickLook, terminal and browser views placed over the pane.

use super::{AnyElement, Context, IntoElement, Styled, WindowExt, Workspace, canvas, px};
use crate::host::platform::TerminalLaunch;

/// What a native pane view shows.
pub(super) enum NativeView {
    File(String),
    Terminal(TerminalLaunch),
    /// The tab's Chromium page, drawn into its own container.
    Browser,
}

impl Workspace {
    pub(super) fn native_content(tab_id: &str, view: NativeView, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity().downgrade();
        let tab_id = tab_id.to_owned();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                if let Some(entity) = entity.upgrade() {
                    if window.has_active_dialog(cx)
                        || entity.read(cx).expanded.contains("native-overlay")
                        || entity.read(cx).pane_moving()
                    {
                        let native = &entity.read(cx).native;
                        native.hide_panel();
                        if let NativeView::File(path) = &view
                            && let Err(error) = native.preload_panel(path)
                        {
                            tracing::warn!(%error, tab = %tab_id, "the file preview could not be preloaded");
                        }
                        return;
                    }
                    // Native views paint over GPUI, so they stop above a floating composer.
                    let composer = entity
                        .read(cx)
                        .document_composer_inset(window, cx)
                        .unwrap_or_default();
                    let values = [
                        f64::from(bounds.origin.x),
                        f64::from(bounds.origin.y),
                        f64::from(bounds.size.width),
                        f64::from((bounds.size.height - composer).max(px(0.))),
                    ];
                    let native = &entity.read(cx).native;
                    let shown = match &view {
                        NativeView::File(path) => native.show_panel(&tab_id, path, values),
                        NativeView::Terminal(launch) => {
                            native.show_terminal(&tab_id, launch, values)
                        }
                        NativeView::Browser => native.show_browser(&tab_id, values),
                    };
                    if let Err(error) = shown {
                        tracing::warn!(%error, tab = %tab_id, "the native pane view could not be shown");
                    }
                }
            },
        )
        .flex_1()
        .size_full()
        .into_any_element()
    }
}
