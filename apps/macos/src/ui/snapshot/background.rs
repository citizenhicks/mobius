//! Window background scenarios: each moon scene behind a chat, one in the light theme,
//! the transparent window and the Appearance page's Background group. Settings are set in
//! memory only and undone after each capture.

use super::Scenario;
use super::capture::{forced_appearance, open_chat};
use super::probes::undo_after_capture;
use crate::ui::backdrop::{Background, Source, Texture};

use crate::ui::Workspace;
use crate::ui::router::{PageTab, SettingsPage};
use crate::ui::theme::Appearance;

use gpui::{Context, Window};

/// Shows `source` behind both appearances until the capture is taken.
pub(super) fn show(this: &mut Workspace, source: Source, cx: &mut Context<Workspace>) {
    for background in [
        &mut this.config.backdrop.dark,
        &mut this.config.backdrop.light,
    ] {
        *background = Background {
            source,
            ..Background::default()
        };
    }
    this.refresh_appearance(cx);
    undo_after_capture(|this| {
        this.config.backdrop.dark = Background::default();
        this.config.backdrop.light = Background::default();
        this.config.backdrop.sliders = None;
    });
}

fn chat_over(
    source: Source,
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    show(this, source, cx);
    open_chat(this, window, cx);
}

pub(super) const SCENARIOS: &[Scenario] = &[
    ("bg-plain", |this, window, cx| {
        chat_over(Source::None, this, window, cx);
    }),
    ("bg-kanazawa", |this, window, cx| {
        chat_over(Source::Kanazawa, this, window, cx);
    }),
    ("bg-ishiyama", |this, window, cx| {
        chat_over(Source::Ishiyama, this, window, cx);
    }),
    ("bg-ryogoku", |this, window, cx| {
        chat_over(Source::Ryogoku, this, window, cx);
    }),
    ("bg-tama", |this, window, cx| {
        chat_over(Source::Tama, this, window, cx);
    }),
    ("bg-tama-light", |this, window, cx| {
        this.appearance = Appearance::Light;
        undo_after_capture(|this| {
            this.appearance = forced_appearance().unwrap_or_default();
        });
        chat_over(Source::Tama, this, window, cx);
    }),
    ("bg-frost-off", |this, window, cx| {
        chat_over(Source::Kanazawa, this, window, cx);
        this.config.backdrop.dark.frosted = false;
        this.config.backdrop.light.frosted = false;
        this.refresh_appearance(cx);
    }),
    ("bg-transparent", |this, window, cx| {
        chat_over(Source::Transparent, this, window, cx);
    }),
    ("bg-transparent-pixel-off", |this, window, cx| {
        chat_over(Source::Transparent, this, window, cx);
        this.config.backdrop.dark.texture = Texture::Off;
        this.config.backdrop.light.texture = Texture::Off;
        this.refresh_appearance(cx);
    }),
    ("bg-settings", |this, window, cx| {
        show(this, Source::Ishiyama, cx);
        this.open_page(PageTab::Settings(SettingsPage::Appearance), window, cx);
    }),
    ("bg-settings-transparent", |this, window, cx| {
        show(this, Source::Transparent, cx);
        this.open_page(PageTab::Settings(SettingsPage::Appearance), window, cx);
    }),
];
