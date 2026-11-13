//! Settings (WP5): Account, Appearance, Security and Data, Keyboard shortcuts, Usage and
//! What's new.

use super::*;
use crate::host::platform::{
    DesktopPermission, DictationModel, FoundationStatus, PermissionStatus,
};
use crate::numeric::Saturating;
use gpui_component::button::ButtonCustomVariant;
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::text::TextView;
use std::sync::LazyLock;

/// Cloud's `site`: marketing and legal pages.
pub(super) fn site() -> Option<&'static str> {
    crate::host::platform::cloud_config().map(|cloud| cloud.site.as_str())
}
const PRIVACY: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_";
const LEGAL: [(&str, &str, &str); 5] = [
    ("Acceptable Use Policy", "hi.shieldCheck", "acceptable-use"),
    ("Terms of Service", "hi.doc", "terms"),
    ("Privacy Policy", "hi.shield02", "privacy"),
    ("Licenses", "hi.fileText", "licenses"),
    ("Help & Support", "hi.question", "support"),
];

/// Every desktop release's notes, as published with it.
#[derive(rust_embed::RustEmbed)]
#[folder = "../../.github/release-notes/"]
#[include = "mobius-desktop-v*.md"]
struct ReleaseNotes;

/// Small window drawings use the user's colours, so each choice previews its theme.
fn theme_preview(p: &Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .w_full()
        .h_full()
        .control_corners()
        .overflow_hidden()
        .border_1()
        .border_color(p.line)
        .bg(p.chrome)
        .child(
            div()
                .flex()
                .items_center()
                .h(space::S)
                .px(space::XS)
                .gap(space::XXS)
                .children(
                    (0..3).map(|_| div().w(space::XXS).h(space::XXS).pill_corners().bg(p.muted)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .w(space::L)
                        .p(space::XS)
                        .gap(space::XS)
                        .bg(p.inset)
                        .children(
                            (0..3).map(|_| div().w_full().h(space::XXS).pill_corners().bg(p.muted)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .p(space::XS)
                        .gap(space::XS)
                        .bg(p.canvas)
                        .child(
                            div()
                                .w(relative(0.75))
                                .h(space::XXS)
                                .pill_corners()
                                .bg(p.accent),
                        )
                        .child(div().w_full().h(space::XXS).pill_corners().bg(p.muted))
                        .child(
                            div()
                                .w(relative(0.75))
                                .h(space::XXS)
                                .pill_corners()
                                .bg(p.muted),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .h(space::S)
                                .cell_corners()
                                .border_1()
                                .border_color(p.line)
                                .bg(p.card),
                        ),
                ),
        )
}

/// `(version, notes)` for each release, newest first, without the notes' own title.
static RELEASE_NOTES: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
    let mut notes: Vec<_> = ReleaseNotes::iter()
        .filter_map(|name| {
            let version = name.strip_prefix("mobius-desktop-v")?.strip_suffix(".md")?;
            let text = String::from_utf8(ReleaseNotes::get(&name)?.data.into_owned()).ok()?;
            let body = text
                .split_once('\n')
                .filter(|(title, _)| title.starts_with("# "))
                .map_or(text.as_str(), |(_, body)| body);
            Some((version.to_owned(), body.trim().to_owned()))
        })
        .collect();
    notes.sort_by_cached_key(|(version, _)| {
        std::cmp::Reverse(
            version
                .split('.')
                .map(|part| part.parse::<u32>().unwrap_or_default())
                .collect::<Vec<_>>(),
        )
    });
    notes
});

impl Workspace {
    pub(in crate::ui) fn render_settings(
        &mut self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match page {
            SettingsPage::Account => self.account_settings(cx),
            SettingsPage::Appearance => self.appearance_settings(window, cx),
            SettingsPage::Security => self.security_settings(cx),
            SettingsPage::Data => self.data_settings(cx),
            SettingsPage::Shortcuts => self.shortcut_settings(cx),
            SettingsPage::Usage => self.usage_settings(cx),
            SettingsPage::WhatsNew => whats_new(),
        }
        .into_any_element()
    }

    fn account_settings(&self, cx: &Context<Self>) -> Page {
        let version = format!("möbius v{}", env!("CARGO_PKG_VERSION"));
        let mut about = Group::new().child(self.version_row(cx));
        if let Some(site) = site() {
            for (title, glyph, slug) in LEGAL {
                let url = format!("{}/legal/en/{slug}", site.trim_end_matches('/'));
                about = about.child(
                    Row::new(slug, title)
                        .leading(Leading::glyph(glyph))
                        .external()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
            }
        }
        let header =
            PageHeader::new("Account").subtitle(format!("{version} · {}", self.link_label()));
        page("settings-account", header)
            .child(self.cloud_section(cx))
            .child(Section::new("About").child(about))
    }

    fn appearance_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Page {
        let background = self.color_row(false, window, cx);
        let foreground = self.color_row(true, window, cx);
        let theme = self.theme_picker(cx);
        let accent = accent_tile_picker(
            "accent",
            self.accent,
            cx.listener(|this, accent: &Accent, _, cx| {
                this.set_appearance(this.appearance, *accent, cx);
            }),
        );
        let header = PageHeader::new("Appearance").subtitle(format!(
            "{} · {}",
            self.appearance.label(),
            self.accent.label()
        ));
        page("settings-appearance", header)
            .child(
                Group::new()
                    .child(
                        Row::new("theme-row", "Theme")
                            .subtitle("Follow macOS, or keep möbius dark or light.")
                            .control(theme),
                    )
                    .child(
                        Row::new("accent-row", "Accent")
                            .subtitle("Colours buttons, switches, links and selection.")
                            .control(accent),
                    )
                    .child(background)
                    .child(foreground)
                    .child(Self::font_row(cx)),
            )
            .child(self.background_section(cx))
    }

    fn theme_picker(&self, cx: &Context<Self>) -> Stateful<Div> {
        let p = self.palette;
        let light = Palette::new(false, self.accent, self.light_colors);
        let dark = Palette::new(true, self.accent, self.dark_colors);
        div()
            .id("theme")
            .flex()
            .w(width::FIELD)
            .gap(space::S)
            .p(space::XS)
            .tile_corners()
            .bg(p.inset)
            .children(Appearance::ALL.map(|appearance| {
                let preview = match appearance {
                    Appearance::System => div()
                        .flex()
                        .w_full()
                        .h_full()
                        .control_corners()
                        .overflow_hidden()
                        .child(theme_preview(&light).flex_1().min_w_0())
                        .child(theme_preview(&dark).flex_1().min_w_0()),
                    Appearance::Light => theme_preview(&light),
                    Appearance::Dark => theme_preview(&dark),
                };
                Button::new(appearance.label())
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .hover(p.hover)
                            .active(p.pressed),
                    )
                    .flex_1()
                    .h(height::REGULAR * 2.)
                    .p(space::XS)
                    .border_2()
                    .border_color(if appearance == self.appearance {
                        p.accent
                    } else {
                        transparent_black()
                    })
                    .toggled(appearance == self.appearance)
                    .accessibility_label(appearance.label())
                    .tooltip(appearance.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_appearance(appearance, this.accent, cx);
                    }))
                    .child(preview)
            }))
    }

    fn font_row(cx: &Context<Self>) -> Row {
        let selected = FontSet::current();
        let items = FontSet::ALL
            .into_iter()
            .map(|fonts| {
                MenuChoice::new(
                    fonts.label(),
                    fonts == selected,
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.set_font_set(fonts, cx)),
                )
            })
            .collect();
        Row::new("font-set", "Font")
            .subtitle("Interface and matching monospace text.")
            .control(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .child(menu_select(
                        "font-set-picker",
                        selected.label(),
                        None,
                        items,
                        cx,
                    ))
                    .child(
                        inline_button("reset-font-set", "Reset Font", "hi.setup02", cx)
                            .disabled(selected == FontSet::System)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_font_set(FontSet::System, cx);
                            })),
                    )
                    .when(cfg!(feature = "snapshot"), |control| {
                        #[cfg(feature = "snapshot")]
                        return control.child(crate::ui::snapshot::probe("font-set".into()));
                        #[cfg(not(feature = "snapshot"))]
                        control
                    }),
            )
    }

    /// One picker and editable HEX for the appearance currently on screen.
    fn color_row(&mut self, foreground: bool, window: &mut Window, cx: &mut Context<Self>) -> Row {
        self.ensure_color_edit(foreground, window, cx);
        let p = self.palette;
        let (title, part) = if foreground {
            ("Foreground colour", "Text")
        } else {
            ("Background colour", "Surfaces")
        };
        let theme = if p.is_dark() { "dark" } else { "light" };
        let row = Row::new(title, title).subtitle(format!("{part} in the {theme} theme."));
        let Some(edit) = self.config.color_edit(foreground) else {
            return row;
        };
        let colors = if p.is_dark() {
            self.dark_colors
        } else {
            self.light_colors
        };
        let overridden = if foreground {
            colors.foreground
        } else {
            colors.background
        }
        .is_some();
        let control = div()
            .flex()
            .items_center()
            .gap(space::S)
            .child(
                ColorPicker::new(&edit.picker)
                    .accessibility_label(format!("{title} colour"))
                    .anchor(Anchor::TopRight),
            )
            .child(
                text_field(&edit.input)
                    .aria_label(format!("{title} HEX"))
                    .w(width::COLOR)
                    .font_family(FontSet::current().mono()),
            )
            .child(
                inline_button(
                    SharedString::from(format!("reset-{title}")),
                    format!("Reset {title}"),
                    "hi.setup02",
                    cx,
                )
                .disabled(!overridden)
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.change_color(foreground, None, cx) {
                        this.sync_color_edit(foreground, window, cx);
                    }
                })),
            );
        row.control(control)
    }

    fn ensure_color_edit(&mut self, foreground: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .config
            .color_edit(foreground)
            .is_some_and(|edit| edit.is_dark == self.palette.is_dark())
        {
            return;
        }
        let color = if foreground {
            self.palette.ink
        } else {
            self.palette.canvas
        };
        let is_dark = self.palette.is_dark();
        let input = field(&hex(color), "#RRGGBB", window, cx);
        let picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(color));
        let input_events = cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &InputEvent, window, cx| {
                if this.palette.is_dark() != is_dark || !matches!(event, InputEvent::Change) {
                    return;
                }
                let text = input.read(cx).value();
                let text = text.trim();
                let value = if text.is_empty() {
                    None
                } else {
                    let Some(value) = parse_hex(text) else {
                        return;
                    };
                    Some(value)
                };
                if this.change_color(foreground, value, cx) {
                    let color = if foreground {
                        this.palette.ink
                    } else {
                        this.palette.canvas
                    };
                    if let Some(edit) = this.config.color_edit(foreground) {
                        edit.picker
                            .update(cx, |picker, cx| picker.set_value(color, window, cx));
                    }
                }
            },
        );
        let picker_events = cx.subscribe_in(
            &picker,
            window,
            move |this, _, event: &ColorPickerEvent, window, cx| {
                if this.palette.is_dark() != is_dark {
                    return;
                }
                let ColorPickerEvent::Change(color) = event;
                let value = color.and_then(|color| parse_hex(&hex(color)));
                this.change_color(foreground, value, cx);
                this.sync_color_edit(foreground, window, cx);
            },
        );
        *self.config.color_edit_slot(foreground) = Some(ColorEdit {
            is_dark: self.palette.is_dark(),
            input,
            picker,
            _events: [input_events, picker_events],
        });
    }

    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn edit_color(
        &mut self,
        foreground: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_color_edit(foreground, window, cx);
        if let Some(edit) = self.config.color_edit(foreground) {
            edit.input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        }
    }

    /// Exercises the real input/picker subscriptions without saving snapshot preferences.
    #[cfg(feature = "snapshot")]
    #[expect(
        clippy::print_stderr,
        reason = "the snapshot harness reports each fixture's result on stderr"
    )]
    pub(in crate::ui) fn fixture_color_controls(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_color_edit(false, window, cx);
        let before = (self.appearance, self.dark_colors, self.light_colors);
        let is_dark = self.palette.is_dark();
        let [typed, picked] = if is_dark {
            [0x20_20_20, 0x24_24_24]
        } else {
            [0xfa_fa_fa, 0xf8_f8_f8]
        };
        let opposite_appearance = if is_dark {
            Appearance::Light
        } else {
            Appearance::Dark
        };
        cx.spawn_in(window, async move |this, cx| {
            for step in 0..8 {
                cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                let updated = this.update_in(cx, |this, window, cx| {
                    this.ensure_color_edit(false, window, cx);
                    let Some(edit) = this.config.color_edit(false) else {
                        tracing::warn!("colour fixture: the background editor was not created");
                        return;
                    };
                    match step {
                        0 => edit.input.update(cx, |input, cx| { input.set_value("#12", window, cx); cx.emit(InputEvent::Change); }),
                        1 => {
                            assert_eq!((this.dark_colors, this.light_colors), (before.1, before.2));
                            assert_eq!(edit.input.read(cx).value().as_str(), "#12");
                            edit.input.update(cx, |input, cx| {
                                input.set_value(hex(rgb(typed).into()), window, cx);
                                cx.emit(InputEvent::Change);
                            });
                        }
                        2 => {
                            let saved = if is_dark { this.dark_colors } else { this.light_colors };
                            assert_eq!(saved.background, Some(typed));
                            edit.picker.update(cx, |picker, cx| picker.select_color(rgb(picked).into(), window, cx));
                        }
                        3 => {
                            assert_eq!(edit.input.read(cx).value().as_str(), hex(rgb(picked).into()));
                            edit.picker.update(cx, |picker, cx| picker.select_color(this.palette.ink, window, cx));
                        }
                        4 => {
                            assert_eq!(edit.input.read(cx).value().as_str(), hex(this.palette.canvas));
                            assert_eq!(edit.picker.read(cx).value().map(hex), Some(hex(this.palette.canvas)));
                            this.shell.notices.retain(|(_, text, _)| text != "Those colours are too close to read text on.");
                            assert!(this.change_color(false, None, cx));
                            this.sync_color_edit(false, window, cx);
                        }
                        5 => {
                            let saved = if is_dark { this.dark_colors } else { this.light_colors };
                            assert_eq!(saved.background, None, "reset must clear the override");
                            let opposite = if is_dark { this.light_colors } else { this.dark_colors };
                            assert_eq!(opposite, if is_dark { before.2 } else { before.1 });
                            this.set_appearance(opposite_appearance, this.accent, cx);
                        }
                        6 => {
                            assert_ne!(this.palette.is_dark(), is_dark);
                            assert_eq!(edit.input.read(cx).value().as_str(), hex(this.palette.canvas));
                            this.set_appearance(before.0, this.accent, cx);
                        }
                        _ => {
                            assert_eq!(this.palette.is_dark(), is_dark);
                            assert_eq!(edit.input.read(cx).value().as_str(), hex(this.palette.canvas));
                            eprintln!("appearance controls: partial HEX, valid HEX, picker sync/rejected contrast, reset and independent theme values passed");
                            this.dark_colors = before.1;
                            this.light_colors = before.2;
                            this.config.color_edits = [None, None];
                            this.refresh_appearance(cx);
                        }
                    }
                });
                if let Err(error) = updated {
                    tracing::debug!(%error, "colour fixture: the workspace was released");
                }
            }
        }).detach();
    }

    fn sync_color_edit(&self, foreground: bool, window: &mut Window, cx: &mut Context<Self>) {
        let color = if foreground {
            self.palette.ink
        } else {
            self.palette.canvas
        };
        if let Some(edit) = self.config.color_edit(foreground) {
            edit.input
                .update(cx, |input, cx| input.set_value(hex(color), window, cx));
            edit.picker
                .update(cx, |picker, cx| picker.set_value(color, window, cx));
        }
    }

    /// Applies one override through the existing persistence and contrast policy.
    fn change_color(
        &mut self,
        foreground: bool,
        value: Option<u32>,
        cx: &mut Context<Self>,
    ) -> bool {
        let is_dark = self.palette.is_dark();
        let mut colors = if is_dark {
            self.dark_colors
        } else {
            self.light_colors
        };
        if foreground {
            colors.foreground = value;
        } else {
            colors.background = value;
        }
        let preview = Palette::new(is_dark, self.accent, colors);
        if theme::contrast(preview.text, preview.sheet) < 4.5 {
            self.report("Those colours are too close to read text on.");
            return false;
        }
        self.set_colors(colors, cx);
        true
    }

    fn security_settings(&self, cx: &Context<Self>) -> Page {
        let desktop = self.native.desktop_status();
        let reason = self.native.desktop_reason();
        let toggle = toggle(
            "desktop-control",
            "Allow computer control",
            desktop.enabled,
            cx,
        )
        .on_click(cx.listener(|this, _: &bool, _, cx| this.toggle_desktop(cx)));
        let mut switch = Row::new("computer-control", "Allow computer control")
            .subtitle(reason.as_deref().unwrap_or(
                "Lets Bots on the gateway running on this Mac see the screen and use the mouse \
                 and keyboard.",
            ))
            .control(toggle);
        if desktop.enabled {
            let ready = reason.is_none();
            switch = switch.status(self.tone(ready), if ready { "Ready" } else { "Not ready" });
        }
        let mut control = Group::new()
            .child(switch)
            .child(self.desktop_permission_row(
                DesktopPermission::Accessibility,
                desktop.accessibility,
                desktop.accessibility_requested,
                cx,
            ))
            .child(self.desktop_permission_row(
                DesktopPermission::ScreenRecording,
                desktop.screen_recording,
                desktop.screen_recording_requested,
                cx,
            ));
        if let Some(apps) = self.native.desktop_apps() {
            control = control.child(Row::new("desktop-apps", "Apps").subtitle(apps).control(
                settings_button("desktop-apps-reset", "Reset").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.native.forget_desktop_apps();
                        cx.notify();
                    },
                )),
            ));
        }
        if desktop.screen_recording_requested && !desktop.screen_recording {
            control = control.child(
                Row::new("desktop-relaunch", "Reopen möbius")
                    .subtitle("macOS applies Screen Recording after möbius quits and opens again.")
                    .control(
                        settings_button("desktop-relaunch-button", "Quit & Reopen")
                            .on_click(cx.listener(|this, _, _, _| this.native.relaunch())),
                    ),
            );
        }
        let header = PageHeader::new("Security").subtitle(format!(
            "Computer control {}",
            match (desktop.enabled, desktop.registered) {
                (false, _) => "off",
                (true, true) => "ready",
                (true, false) => "not ready",
            }
        ));
        page("settings-security", header)
            .child(Section::new("Computer control").child(control))
            .child(Section::new("Apple Intelligence").child(self.foundation_group()))
            .child(Section::new("Dictation and voice").child(self.voice_group()))
            .child(Section::new("Notifications").child(self.notification_group()))
    }

    fn notification_group(&self) -> Group {
        let status = self.native.notification_status();
        let hint = match status {
            PermissionStatus::Granted => "Completed work and approvals use macOS notifications.",
            PermissionStatus::NotRequested => {
                "Requested when möbius first has something to tell you."
            }
            other => other.label(),
        };
        let allowed = status == PermissionStatus::Granted;
        let row = Row::new("notifications", "Notifications")
            .subtitle(hint)
            .status(self.tone(allowed), permission_label(status));
        Group::new().child(if allowed {
            row
        } else {
            row.control(open_settings(
                "notification-settings",
                "x-apple.systempreferences:com.apple.Notifications-Settings.extension",
            ))
        })
    }

    fn foundation_group(&self) -> Group {
        let status = self.native.foundation_status();
        let short = match status {
            FoundationStatus::Available => "Available",
            FoundationStatus::Disabled => "Off",
            FoundationStatus::Unsupported => "Unsupported",
            FoundationStatus::Downloading => "Downloading",
            FoundationStatus::Unavailable => "Unavailable",
        };
        let tone = match status {
            FoundationStatus::Available => self.palette.signal,
            FoundationStatus::Downloading => self.palette.warning,
            _ => self.palette.danger,
        };
        let row = Row::new("foundation", "On-device model").status(tone, short);
        Group::new().child(if status == FoundationStatus::Available {
            row
        } else {
            row.subtitle(status.label())
        })
    }

    fn voice_group(&self) -> Group {
        let audio = self.native.audio_status();
        Group::new()
            .child(self.permission_row(
                (
                    "microphone",
                    "Microphone",
                    "Used for dictation and voice chats.",
                ),
                audio.microphone,
                "Microphone",
            ))
            .child(self.permission_row(
                (
                    "speech-recognition",
                    "Speech recognition",
                    "Turns dictation into text.",
                ),
                audio.speech_recognition,
                "SpeechRecognition",
            ))
            .child(
                Row::new("on-device-dictation", "On-device dictation")
                    .subtitle("Dictation in your language without a network connection.")
                    .status(
                        match audio.dictation_model {
                            DictationModel::Installed => self.palette.signal,
                            DictationModel::Unsupported => self.palette.danger,
                            _ => self.palette.warning,
                        },
                        audio.dictation_model.label(),
                    ),
            )
    }

    /// One computer-control permission: the system prompt the first time, System Settings
    /// after that, since macOS asks only once.
    fn desktop_permission_row(
        &self,
        permission: DesktopPermission,
        allowed: bool,
        requested: bool,
        cx: &Context<Self>,
    ) -> Row {
        let (id, title, need) = match permission {
            DesktopPermission::Accessibility => (
                "accessibility",
                if self.native.desktop_status().device_control {
                    "Device Control and Data Access"
                } else {
                    "Accessibility"
                },
                "Needed to use the mouse and keyboard.",
            ),
            DesktopPermission::ScreenRecording => (
                "screen-recording",
                "Screen Recording",
                "Needed to see the screen.",
            ),
        };
        let (status, subtitle) = match (allowed, requested) {
            (true, _) => ("Allowed", need.to_owned()),
            (false, false) => ("Not requested", need.to_owned()),
            (false, true) => (
                "Waiting",
                format!(
                    "Turn on möbius under {title} in System Settings. Already on? Ask Again \
                     clears the old entry and asks once more."
                ),
            ),
        };
        let row = Row::new(id, title)
            .subtitle(subtitle)
            .status(self.tone(allowed), status);
        if allowed {
            return row;
        }
        row.control(
            settings_button(
                SharedString::from(format!("{id}-request")),
                if requested { "Ask Again" } else { "Allow…" },
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if requested {
                    this.native.reset_desktop_permission(permission);
                } else {
                    this.native.request_desktop_permission(permission);
                }
                cx.notify();
            })),
        )
    }

    /// A macOS permission: its status as a green or red dot, and System Settings when
    /// it is not allowed.
    fn permission_row(
        &self,
        (id, title, subtitle): (&'static str, &'static str, &'static str),
        status: PermissionStatus,
        pane: &str,
    ) -> Row {
        let allowed = status == PermissionStatus::Granted;
        let hint = if allowed { subtitle } else { status.label() };
        let row = Row::new(id, title)
            .subtitle(hint)
            .status(self.tone(allowed), permission_label(status));
        if allowed {
            row
        } else {
            row.control(open_settings(
                SharedString::from(format!("{id}-settings")),
                &format!("{PRIVACY}{pane}"),
            ))
        }
    }

    /// Green for an allowed or working state, red otherwise.
    fn tone(&self, on: bool) -> Hsla {
        if on {
            self.palette.signal
        } else {
            self.palette.danger
        }
    }
}

/// Picker and partial HEX text for one visible appearance colour.
pub(in crate::ui) struct ColorEdit {
    is_dark: bool,
    input: Entity<InputState>,
    picker: Entity<ColorPickerState>,
    _events: [Subscription; 2],
}

impl super::ConfigUi {
    fn color_edit(&self, foreground: bool) -> Option<&ColorEdit> {
        let [background, text] = &self.color_edits;
        if foreground { text } else { background }.as_ref()
    }

    fn color_edit_slot(&mut self, foreground: bool) -> &mut Option<ColorEdit> {
        let [background, text] = &mut self.color_edits;
        if foreground { text } else { background }
    }
}

fn hex(color: Hsla) -> String {
    let Rgba { r, g, b, .. } = color.to_rgb();
    let [r, g, b] = [r, g, b].map(|channel| (channel * 255.).round().saturating_u8());
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// `#RRGGBB`, with or without the `#`.
fn parse_hex(text: &str) -> Option<u32> {
    let digits = text.trim_prefix('#');
    (digits.len() == 6 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
}

fn open_settings(id: impl Into<ElementId>, url: &str) -> Button {
    let url = url.to_owned();
    settings_button(id, "Open Settings…").on_click(move |_, _, cx| cx.open_url(&url))
}

fn permission_label(status: PermissionStatus) -> &'static str {
    match status {
        PermissionStatus::Granted => "Allowed",
        PermissionStatus::NotRequested => "Not requested",
        PermissionStatus::Denied => "Denied",
        PermissionStatus::Restricted => "Restricted",
    }
}

impl Workspace {
    /// The Settings list (WP5): one row per settings page.
    pub(in crate::ui) fn settings_list(&self, cx: &Context<Self>) -> AnyElement {
        let mut list = crate::ui::navigation::sidebar_list();
        for page in SettingsPage::ALL {
            list = list.child(self.page_row(
                page.label(),
                page.label(),
                Leading::glyph(page.glyph()),
                PageTab::Settings(page),
                cx,
            ));
        }
        list.into_any_element()
    }
}

/// Each release's notes, newest first, in the app rather than on the website.
fn whats_new() -> Page {
    let header = PageHeader::new("What’s new").subtitle(format!(
        "You’re running möbius v{}",
        env!("CARGO_PKG_VERSION")
    ));
    RELEASE_NOTES.iter().fold(
        page("settings-whats-new", header),
        |page, (version, notes)| {
            let id = SharedString::from(format!("release-notes:{version}"));
            page.child(
                Section::new(format!("möbius {version}"))
                    .child(TextView::markdown(id, notes.as_str()).selectable(true)),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::RELEASE_NOTES;

    #[test]
    fn this_version_ships_with_its_release_notes_first() {
        let (newest, notes) = RELEASE_NOTES.first().expect("release notes");
        assert_eq!(newest, env!("CARGO_PKG_VERSION"));
        assert!(!notes.starts_with('#') && !notes.is_empty());
        assert!(RELEASE_NOTES.iter().any(|(version, _)| version == "0.1.0"));
    }
}
