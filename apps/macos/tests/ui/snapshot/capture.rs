//! Scenario execution and image capture, using the shared snapshot probes.

use super::probes::{
    PROBES, RENDERS, UNDO, capture_strip, clear_probe, click, dismiss_menu, probe_state, probed,
    report, reset_renders, settled, wait,
};
use super::{CLICKS, PACKAGES, content, sidebar};

use crate::ui::Workspace;
use crate::ui::router::PageTab;
use crate::ui::theme::Appearance;

use gpui::{AsyncWindowContext, Bounds, Context, WeakEntity, Window, point, px, size};

use gpui_component::WindowExt as _;
use mobius::SessionId;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;
/// Process-local accessibility override; never changes the OS or saved preferences.
pub(in crate::ui) fn reduce_motion() -> bool {
    std::env::var("MOBIUS_SNAPSHOT_REDUCE_MOTION").is_ok_and(|value| value == "1")
}

/// Types a fixture catalog, reporting one that no longer matches the wire format.
pub(super) fn catalog(
    name: &str,
    value: serde_json::Value,
) -> Option<mobius_gateway::wire::ReadyPayload> {
    serde_json::from_value(value)
        .inspect_err(|error| eprintln!("snapshot {name}: {error}"))
        .ok()
}

/// The open chat's checked identity, when a chat is selected.
pub(super) fn session_id(this: &Workspace) -> Option<SessionId> {
    this.client
        .state
        .selected_session_id
        .as_ref()
        .map(SessionId::share)
}

/// Opens the most active chat, or `MOBIUS_SNAPSHOT_SESSION`, falling back to a draft.
pub(super) fn open_chat(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let id = std::env::var("MOBIUS_SNAPSHOT_SESSION")
        .ok()
        .and_then(|id| {
            SessionId::new(id)
                .inspect_err(|error| tracing::warn!(%error, "invalid snapshot session ID"))
                .ok()
        })
        .or_else(|| {
            this.client
                .state
                .sessions
                .iter()
                .filter(|session| session.sequence > 20)
                .max_by_key(|session| session.updated_at)
                .map(|session| session.session_id.share())
        });
    match id {
        Some(id) => this.open_page(PageTab::Chat(id), window, cx),
        None => this.new_chat(window, cx),
    }
}

pub(super) fn forced_appearance() -> Option<Appearance> {
    match std::env::var("MOBIUS_SNAPSHOT_APPEARANCE").ok()?.as_str() {
        "light" => Some(Appearance::Light),
        "dark" => Some(Appearance::Dark),
        _ => None,
    }
}

pub(super) fn selected(name: &str, only: Option<&str>) -> bool {
    only.is_none_or(|only| only.split(',').any(|n| n == name))
}

/// Clicks each disclosure twice and records its changing bounds and painted frames.
#[expect(
    clippy::future_not_send,
    reason = "it drives the window on GPUI's foreground executor, whose context is not Send"
)]
pub(super) async fn capture_clicks(
    name: &str,
    dir: &std::path::Path,
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    let control = match name {
        "motion-sidebar" => Some("motion:sidebar-toggle"),
        "motion-session-card" => Some("motion:session-toggle"),
        "motion-files-rail" | "motion-changes-rail" => Some("motion:files-toggle"),
        "motion-folder" | "motion-changes-folder" => Some("motion:folder:src"),
        _ => None,
    };
    let clicks = control.is_some() || CLICKS.into_iter().flatten().any(|clicked| *clicked == name);
    let passes = if matches!(
        name,
        "motion-sidebar" | "motion-files-rail" | "motion-changes-rail"
    ) {
        ["close", "open"]
    } else {
        ["open", "close"]
    };
    // The stable button remains clickable after the watched geometry closes.
    for pass in passes.into_iter().filter(|_| clicks) {
        let bounds = control.map_or_else(probed, |name| {
            PROBES.with(|probes| probes.borrow().get(name).copied())
        });
        let Some(bounds) = bounds else {
            eprintln!("snapshot {name}: missing click probe {control:?}");
            break;
        };
        let position = bounds.origin
            + if control.is_some() {
                point(bounds.size.width / 2., bounds.size.height / 2.)
            } else {
                point(px(40.), px(10.))
            };
        let started = std::time::Instant::now();
        report("click", cx.update(|window, cx| click(position, window, cx)));
        let mut captures = Vec::new();
        // PNG encoding waits until the slide ends; it must not consume its frames.
        for frame in 1..=13 {
            if frame == 13 {
                wait(cx.background_executor(), 200).await;
            }
            let drawn = cx.update(|window, cx| {
                if control.is_some()
                    && let Some(mut probe) = probe_state()
                    && let Some((_, bounds)) = probe.as_mut()
                {
                    *bounds = Bounds::default();
                }
                window.draw(cx).clear(cx);
            });
            report("click frame", drawn);
            eprintln!(
                "probe {name}: {pass} frame {frame} elapsed_ms={} {:?}",
                started.elapsed().as_millis(),
                probed()
            );
            if [1, 12, 13].contains(&frame) || (control.is_some() && [3, 6].contains(&frame)) {
                let path = dir.join(format!("{name}-{pass}-{frame}.png"));
                if let Ok(Ok(image)) = this.update_in(cx, |_, window, _| window.render_to_image()) {
                    captures.push((path, image));
                }
            }
            wait(cx.background_executor(), 16).await;
        }
        for (path, image) in captures {
            if let Err(error) = image.save(&path) {
                eprintln!("snapshot {}: {error}", path.display());
            }
        }
    }
}

impl Workspace {
    pub(in crate::ui) fn start_snapshots(window: &Window, cx: &Context<Self>) {
        let Some(dir) = std::env::var_os("MOBIUS_SNAPSHOT").map(PathBuf::from) else {
            return;
        };
        let only = std::env::var("MOBIUS_SNAPSHOT_ONLY").ok();
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..200 {
                wait(cx.background_executor(), 100).await;
                let ready = this.read_with(cx, |this, _| {
                    this.client.link.is_ready() && this.client.state.gateway.is_some()
                });
                if ready.unwrap_or(true) {
                    break;
                }
            }
            if let Err(error) = std::fs::create_dir_all(&dir) {
                eprintln!("snapshot {}: {error}", dir.display());
            }
            let viewport = this
                .update_in(cx, |this, window, cx| {
                    if let Some(appearance) = forced_appearance() {
                        this.appearance = appearance;
                        this.refresh_appearance(cx);
                    }
                    window.viewport_size()
                })
                .unwrap_or(size(px(1600.), px(1000.)));
            let scenarios = PACKAGES.into_iter().flatten();
            for (name, step) in scenarios.filter(|(name, _)| selected(name, only.as_deref())) {
                // Menu close callbacks may update Workspace themselves.
                let dismissed = cx.update(|window, cx| {
                    dismiss_menu(window, cx);
                    window.close_all_dialogs(cx);
                    cx.stop_active_drag(window);
                });
                report("dismiss", dismissed);
                let stepped = this.update_in(cx, |this, window, cx| {
                    let undos = UNDO.with(|list| list.take());
                    if !undos.is_empty() {
                        for undo in undos {
                            undo(this);
                        }
                        // An undone colour change needs the palette rebuilt.
                        this.refresh_appearance(cx);
                    }
                    window.resize(viewport);
                    this.sidebar_resize.update(cx, |state, _| state.clear());
                    this.content_resize.update(cx, |state, _| state.clear());
                    this.navigation_visible = true;
                    step(this, window, cx);
                });
                report(name, stepped);
                for _ in 0..100 {
                    wait(cx.background_executor(), 100).await;
                    if this.read_with(cx, settled).unwrap_or(true) {
                        break;
                    }
                }
                assert!(!sidebar::title_scroll_pending(), "title scroll scenario timed out");
                wait(cx.background_executor(), 800).await;
                // Draw explicitly: a locked or sleeping display delivers no frames.
                report("draw", cx.update(|window, cx| window.draw(cx).clear(cx)));
                wait(cx.background_executor(), 300).await;
                report("draw", cx.update(|window, cx| window.draw(cx).clear(cx)));
                let verified = this.update_in(cx, |this, _, cx| content::verify_content_action(name, this, cx));
                report(name, verified);
                let path = dir.join(format!("{name}.png"));
                let saved = this.update_in(cx, |_, window, _| {
                    window.render_to_image().map(|image| image.save(&path))
                });
                eprintln!("snapshot {name}: {:?}", saved.map(|r| r.map(|r| r.is_ok())));
                if let Some(hold) = std::env::var("MOBIUS_SNAPSHOT_HOLD_MS")
                    .ok()
                    .and_then(|ms| ms.parse().ok())
                {
                    wait(cx.background_executor(), hold).await;
                }
                if let Some(bounds) = probed() {
                    eprintln!("probe {name}: before {bounds:?}");
                }
                capture_clicks(name, &dir, &this, cx).await;
                capture_strip(name, &dir, &this, cx).await;
                clear_probe();
                if name.starts_with("perf") {
                    reset_renders();
                    // Drive 30 frames a second, as the display link would for a visible window.
                    let mut drawing = Duration::ZERO;
                    for _ in 0..300 {
                        wait(cx.background_executor(), 33).await;
                        let started = std::time::Instant::now();
                        report("draw", cx.update(|window, cx| window.draw(cx).clear(cx)));
                        drawing += started.elapsed();
                    }
                    eprintln!("perf {name}: mean draw {:?}", drawing / 300);
                    let [workspace, transcript, catalog] =
                        RENDERS.each_ref().map(|count| count.load(Ordering::Relaxed));
                    eprintln!(
                        "perf {name}: 10s renders workspace={workspace} transcript={transcript} catalog={catalog}"
                    );
                }
            }
            report("quit", cx.update(|_, cx| cx.quit()));
        })
        .detach();
    }
}
