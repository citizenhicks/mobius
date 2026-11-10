//! Automatic updates: the rail's update button, the Settings version row and the
//! Check for Updates action. Sparkle runs natively; this only renders its state.

use super::*;
use crate::host::platform::{UpdateAction, UpdateStage, UpdateStatus};
use crate::numeric::ToFloat;
use gpui_component::progress::ProgressCircle;
use std::borrow::Cow;
use std::time::Duration;

actions!(
    mobius_desktop,
    [
        #[derive(Eq)]
        CheckForUpdates
    ]
);

#[derive(Default)]
pub(in crate::ui) struct UpdateUi {
    /// A fixed state for snapshots, shown instead of Sparkle's.
    pub(in crate::ui) fixture: Option<UpdateStatus>,
    watch: Option<Task<()>>,
}

impl UpdateStage {
    fn busy(self) -> bool {
        matches!(
            self,
            Self::Downloading | Self::Extracting | Self::Installing
        )
    }
}

/// The Settings subtitle for an update state.
fn describe(status: &UpdateStatus) -> String {
    let version = status.version.as_deref().unwrap_or("");
    let percent = status
        .progress
        .map(|fraction| format!(" · {}", percent(fraction)))
        .unwrap_or_default();
    match status.stage {
        _ if !status.supported => status
            .reason
            .clone() // rules-allow: the subtitle and toast own their text while the status keeps its reason
            .unwrap_or_else(|| "Updates are unavailable.".into()),
        UpdateStage::Checking => "Checking for updates…".into(),
        UpdateStage::Available => format!("möbius {version} is available."),
        UpdateStage::Downloading => format!("Downloading möbius {version}{percent}"),
        UpdateStage::Extracting => format!("Preparing möbius {version}{percent}"),
        UpdateStage::Installing => format!("Installing möbius {version} and relaunching…"),
        UpdateStage::Idle if status.up_to_date => "möbius is up to date.".into(),
        UpdateStage::Idle => "Checks for updates automatically.".into(),
    }
}

/// The toast after a check the user asked for; errors arrive from Swift as their own toast.
fn check_result(status: &UpdateStatus) -> Option<(Tone, String)> {
    match status.stage {
        UpdateStage::Available => Some((Tone::Info, describe(status))),
        UpdateStage::Idle if status.up_to_date => Some((Tone::Success, describe(status))),
        _ => None,
    }
}

impl Workspace {
    fn update_status(&self) -> Cow<'_, UpdateStatus> {
        self.shell
            .updates
            .fixture
            .as_ref()
            .map_or_else(|| Cow::Owned(self.native.update_status()), Cow::Borrowed)
    }

    /// Checks now and reports the result as a toast.
    pub(in crate::ui) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let unsupported = {
            let status = self.update_status();
            (!status.supported).then(|| describe(&status))
        };
        if let Some(message) = unsupported {
            self.announce(Tone::Info, message);
            cx.notify();
            return;
        }
        self.native.update(UpdateAction::Check);
        self.shell.updates.watch = Some(cx.spawn(async move |this, cx| {
            for _ in 0..600 {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let done = this.update(cx, |this, cx| {
                    let result = {
                        let status = this.update_status();
                        if status.stage == UpdateStage::Checking {
                            return false;
                        }
                        check_result(&status)
                    };
                    if let Some((tone, message)) = result {
                        this.announce(tone, message);
                    }
                    cx.notify();
                    true
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Handles Check for Updates from the menu bar and the command palette.
    pub(in crate::ui) fn update_actions(root: Div, cx: &Context<Self>) -> Div {
        root.on_action(cx.listener(|this, _: &CheckForUpdates, _, cx| this.check_for_updates(cx)))
    }

    /// Downloads, installs and relaunches; progress shows in the rail button.
    pub(in crate::ui) fn install_update(&self, cx: &mut Context<Self>) {
        self.native.update(UpdateAction::Install);
        cx.notify();
    }

    /// The rail button above the Event Centre bell: the update while one is available,
    /// otherwise What's new.
    pub(super) fn update_rail_button(&self, cx: &Context<Self>) -> AnyElement {
        let status = self.update_status();
        if !status.supported || !(status.stage == UpdateStage::Available || status.stage.busy()) {
            let open = self.primary_page() == Some(&PageTab::Settings(SettingsPage::WhatsNew));
            return rail_button(
                "whats-new",
                "What’s new",
                icon("hi.info", &palette(cx)),
                open,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_page(PageTab::Settings(SettingsPage::WhatsNew), window, cx);
            }))
            .into_any_element();
        }
        let version = status.version.as_deref().unwrap_or_default();
        let label = match status.stage {
            UpdateStage::Available => format!("Update to möbius {version}"),
            _ => describe(&status),
        };
        // macOS system blue, stronger than the theme's accents.
        let blue = Hsla::from(gpui::rgb(0x0A_84_FF));
        let ring = match (status.stage, status.progress) {
            (UpdateStage::Downloading, Some(fraction)) => Some(
                ProgressCircle::new("update-progress")
                    .with_size(glyph::LEAD)
                    .color(blue)
                    .value((fraction * 100.).to_f32())
                    .into_any_element(),
            ),
            (stage, _) if stage.busy() => {
                Some(Spinner::new(glyph::LEAD).color(blue).into_any_element())
            }
            _ => None,
        };
        // While busy the ring replaces the glyph inside the same button.
        let glyph = if ring.is_some() {
            gpui_component::Icon::empty()
        } else {
            icon("hi.download01", &palette(cx))
        };
        // Always blue, whatever the chosen accent, so a waiting update stands out.
        let button = rail_button("update", label, glyph, false, cx)
            .bg(blue.opacity(0.18))
            .text_color(blue)
            .when(status.stage == UpdateStage::Available, |button| {
                button.on_click(cx.listener(|this, _, _, cx| this.install_update(cx)))
            });
        div()
            .relative()
            .flex_shrink_0()
            .child(button)
            .children(ring.map(|ring| {
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(ring)
            }))
            .into_any_element()
    }

    /// Settings → Account's Version row: this build and its update state.
    pub(in crate::ui) fn version_row(&self, cx: &Context<Self>) -> Row {
        let status = self.update_status();
        let row = Row::new("version", "Version").subtitle(format!(
            "v{} · {}",
            env!("CARGO_PKG_VERSION"),
            describe(&status)
        ));
        match status.stage {
            _ if !status.supported => row,
            UpdateStage::Available => row.control(
                settings_button("install-update", "Install and Relaunch")
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| this.install_update(cx))),
            ),
            UpdateStage::Idle => row.control(
                settings_button("check-updates", "Check for Updates")
                    .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
            ),
            _ => row,
        }
    }
}
