//! Shell scenarios: rail, sidebar, catalog, titlebar, palette and connection states.
//! Forced connection states restore the real link and catalog shortly after capture.

use super::*;
use crate::host::platform::{UpdateStage, UpdateStatus};
use crate::ui::navigation::Organization;
use crate::ui::runtime::Link;

pub(super) const SCENARIOS: &[Scenario] = &[
    ("motion-sidebar", |this, window, cx| {
        this.new_chat(window, cx);
        watch("motion:sidebar".into());
    }),
    ("motion-session-card", |this, window, cx| {
        open_chat(this, window, cx);
        watch("motion:session-card".into());
    }),
    ("shell-chronological", |this, window, cx| {
        this.navigation_catalog.organization = Organization::Chronological;
        open_chat(this, window, cx);
        restore_later(this, cx, |this| {
            this.navigation_catalog.organization = Organization::ByProject;
        });
    }),
    ("shell-catalog-loading", |this, window, cx| {
        this.new_chat(window, cx);
        force_link(this, Link::Connecting(String::new()), true, cx);
    }),
    ("shell-reconnecting", |this, window, cx| {
        open_chat(this, window, cx);
        let message = "The gateway closed the connection.".into();
        force_link(
            this,
            Link::Reconnecting {
                endpoint: None,
                attempt: 2,
                message,
            },
            false,
            cx,
        );
    }),
    ("shell-draft-after-reconnect", |this, window, cx| {
        this.new_chat(window, cx);
    }),
    ("shell-failed", |this, window, cx| {
        this.new_chat(window, cx);
        let failed = Link::Failed("Connection refused (os error 61)".into());
        force_link(this, failed, false, cx);
    }),
    ("shell-palette", |this, window, cx| {
        this.command_palette(window, cx);
    }),
    ("shell-toast", |this, window, cx| {
        this.new_chat(window, cx);
        this.report("The gateway rejected this request.");
    }),
    ("perf-idle", |this, window, cx| this.new_chat(window, cx)),
    ("perf-chat", open_chat),
    ("perf-bot", |this, window, cx| {
        let bot = this
            .client
            .state
            .gateway
            .as_ref()
            .and_then(|g| g.bots.first())
            .map(|b| Arc::clone(&b.id));
        if let Some(bot) = bot {
            this.open_page(PageTab::Bot(bot), window, cx);
        }
    }),
    ("perf-settings", |this, window, cx| {
        this.open_page(PageTab::Settings(SettingsPage::Appearance), window, cx);
    }),
    ("shell-no-sidebar", |this, window, cx| {
        open_chat(this, window, cx);
        this.navigation_visible = false;
    }),
    ("update-available", |this, window, cx| {
        show_update(this, UpdateStage::Available, None, window, cx);
    }),
    ("update-downloading", |this, window, cx| {
        show_update(this, UpdateStage::Downloading, Some(0.42), window, cx);
    }),
    ("update-installing", |this, window, cx| {
        show_update(this, UpdateStage::Installing, None, window, cx);
    }),
    ("update-settings", |this, window, cx| {
        show_update(this, UpdateStage::Available, None, window, cx);
        this.open_page(PageTab::Settings(SettingsPage::Account), window, cx);
    }),
    ("shell-narrow", |this, window, cx| {
        window.resize(size(px(1000.), px(720.)));
        open_chat(this, window, cx);
    }),
];

/// Shows the rail's update button in `stage` from a fixture, never from Sparkle.
fn show_update(
    this: &mut Workspace,
    stage: UpdateStage,
    progress: Option<f64>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    this.new_chat(window, cx);
    this.shell.updates.fixture = Some(UpdateStatus {
        supported: true,
        stage,
        version: Some("0.2.0".into()),
        progress,
        ..UpdateStatus::default()
    });
    // Clear only this scenario's fixture, never the next update scenario's.
    cx.spawn(async move |this, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(1500))
            .await;
        let cleared = this.update(cx, |this, cx| {
            let fixture = &mut this.shell.updates.fixture;
            if fixture.as_ref().is_some_and(|status| status.stage == stage) {
                *fixture = None;
                cx.notify();
            }
        });
        report("update fixture", cleared);
    })
    .detach();
}

/// Undoes a scenario's local change after its capture.
fn restore_later(_: &mut Workspace, cx: &Context<Workspace>, undo: fn(&mut Workspace)) {
    cx.spawn(async move |this, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(1500))
            .await;
        let restored = this.update(cx, |this, cx| {
            undo(this);
            cx.notify();
        });
        report("restore", restored);
    })
    .detach();
}

/// The real link, and the catalog when the scenario emptied it.
type Stash = (Link, Option<Vec<mobius_gateway::wire::SessionRecord>>);

thread_local! {
    /// The real link and catalog while a scenario shows a forced connection state.
    static STASH: std::cell::RefCell<Option<Stash>> = const { std::cell::RefCell::new(None) };
}

fn unstash(this: &mut Workspace) {
    if let Some((link, sessions)) = STASH.with(|stash| stash.borrow_mut().take()) {
        this.client.link = link;
        if let Some(sessions) = sessions {
            this.client.state.sessions = sessions;
        }
        this.gateway_revision += 1;
    }
}

/// Shows `link` (and, with `empty`, no catalog) without touching the gateway, restoring
/// the real state after the capture or when the next forced scenario starts.
fn force_link(this: &mut Workspace, link: Link, empty: bool, cx: &Context<Workspace>) {
    unstash(this);
    let real = std::mem::replace(&mut this.client.link, link);
    let sessions = empty.then(|| std::mem::take(&mut this.client.state.sessions));
    STASH.with(|stash| *stash.borrow_mut() = Some((real, sessions)));
    this.gateway_revision += 1;
    cx.spawn(async move |this, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(1500))
            .await;
        let unstashed = this.update(cx, |this, cx| {
            unstash(this);
            cx.notify();
        });
        report("restore link", unstashed);
    })
    .detach();
}
