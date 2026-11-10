//! Split geometry for the companion pane (spec §1.10).

use super::{App, PageTab, Pixels, Window, Workspace, width};

/// The pane width beside the chat, or `None` when both cannot fit.
pub(super) fn split(available: Pixels) -> Option<Pixels> {
    (available >= width::CHAT_MIN + width::DOC_MIN).then(|| {
        (available * 0.5)
            .max(width::DOC_MIN)
            .min(width::DOC_MAX)
            .min(available - width::CHAT_MIN)
    })
}

impl Workspace {
    /// The pane width while a document shows beside the chat; not gated on session readiness.
    pub(in crate::ui) fn pane_split(&self, window: &Window, cx: &App) -> Option<Pixels> {
        if self.active_content().is_none()
            || !self.split_content
            || !matches!(self.primary_page(), Some(PageTab::Chat(_) | PageTab::Draft))
        {
            return None;
        }
        let available = window.viewport_size().width - width::RAIL - self.sidebar_width(cx);
        split(available).map(|initial| {
            self.content_resize
                .read(cx)
                .sizes()
                .get(1)
                .copied()
                .unwrap_or(initial)
                .max(width::DOC_MIN)
                .min(available - width::CHAT_MIN)
        })
    }
}
