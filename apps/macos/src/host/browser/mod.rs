//! The in-app browser: Chromium, through CEF, drawn by native views in pane tabs. Every tab
//! shares one persistent profile. The gateway assigns a chat's rendered page.

mod devtools;
mod engine;
mod mac;
mod page;
mod router;

pub use engine::{available, ensure_started, shutdown};
pub use page::{Page, PageEvent, PageEvents, Popup, Shortcut, download_path};
