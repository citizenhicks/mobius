//! Development-only visual capture: `MOBIUS_SNAPSHOT=<dir> cargo run --features snapshot`.
//! Walks each package's scenarios against the live gateway and writes one PNG per screen.
//! `MOBIUS_SNAPSHOT_ONLY=a,b` limits scenarios; `MOBIUS_SNAPSHOT_SESSION=<id>` picks the chat;
//! `MOBIUS_SNAPSHOT_APPEARANCE=light|dark` forces the appearance without saving it;
//! `MOBIUS_SNAPSHOT_HOLD_MS=<ms>` keeps each screen up that long after its capture, for
//! `screencapture` of native views or for driving the in-app browser from outside.
//! `MOBIUS_SNAPSHOT_REDUCE_MOTION=1` disables motion for this capture process only.
//! Native QuickLook and terminal panels are not part of the GPUI scene and render blank.
//! Scenarios only change local UI state; they never send chat messages or edit gateway records.
#![expect(
    clippy::print_stderr,
    reason = "this development harness reports each capture and check on stderr"
)]

mod attachments;
mod background;
mod bots;
mod chat;
mod config;
mod content;
mod documents;
mod harness;
mod memory;
mod selection;
mod shell;
mod sidebar;

use super::*;
use gpui::{
    AsyncWindowContext, BackgroundExecutor, PlatformInput, ScrollDelta, ScrollEvent, ScrollSource,
    TouchPhase,
};
use mobius::SessionId;
use mobius_gateway::wire::SessionActivityState;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// Process-local accessibility override; never changes the OS or saved preferences.
pub(in crate::ui) fn reduce_motion() -> bool {
    std::env::var("MOBIUS_SNAPSHOT_REDUCE_MOTION").is_ok_and(|value| value == "1")
}

/// Renders of the workspace, the transcript and the chat catalog, logged by `perf-*` scenarios.
static RENDERS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(in crate::ui) fn count_render(slot: usize) {
    if let Some(count) = RENDERS.get(slot) {
        count.fetch_add(1, Ordering::Relaxed);
    }
}

/// The painted bounds of the element a `probe` watches, logged frame by frame.
static PROBE: Mutex<Option<Probe>> = Mutex::new(None);

/// The watched element's id and its last painted bounds.
type Probe = (String, Bounds<Pixels>);

/// The watched probe; after a panic while it was held the harness stops probing.
fn probe_state() -> Option<MutexGuard<'static, Option<Probe>>> {
    PROBE.lock().ok()
}

/// Stops watching the probed element between scenarios.
fn clear_probe() {
    if let Some(mut probe) = probe_state() {
        *probe = None;
    }
}

/// Zeroes the render counters a `perf-*` scenario reports.
fn reset_renders() {
    for count in &RENDERS {
        count.store(0, Ordering::Relaxed);
    }
}

/// Starts watching the element `track` reports under `id`.
pub(in crate::ui) fn watch(id: String) {
    if let Some(mut probe) = probe_state() {
        *probe = Some((id, Bounds::default()));
    }
}

/// Waits `ms` milliseconds on the background executor.
fn wait(executor: &BackgroundExecutor, ms: u64) -> Task<()> {
    executor.timer(Duration::from_millis(ms))
}

/// Reports a step that could not run because its window or workspace is gone.
fn report<E: std::fmt::Display>(step: &str, result: Result<(), E>) {
    if let Err(error) = result {
        eprintln!("snapshot {step}: {error}");
    }
}

/// Types a fixture catalog, reporting one that no longer matches the wire format.
fn catalog(name: &str, value: serde_json::Value) -> Option<mobius_gateway::wire::ReadyPayload> {
    serde_json::from_value(value)
        .inspect_err(|error| eprintln!("snapshot {name}: {error}"))
        .ok()
}

/// The open chat's checked identity, when a chat is selected.
fn session_id(this: &Workspace) -> Option<SessionId> {
    this.client
        .state
        .selected_session_id
        .as_ref()
        .map(SessionId::share)
}

/// An invisible child that records its parent's painted bounds while `id` is watched.
pub(in crate::ui) fn track(id: &str) -> impl IntoElement + use<> {
    let id = id.to_owned();
    canvas(
        move |bounds, _, _| {
            if let Some(mut probe) = probe_state()
                && let Some((watched, recorded)) = probe.as_mut()
                && *watched == id
            {
                *recorded = bounds;
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

pub(in crate::ui) fn probed() -> Option<Bounds<Pixels>> {
    probe_state().and_then(|probe| probe.as_ref().map(|(_, bounds)| *bounds))
}

thread_local! {
    /// Bounds of elements scenarios click, by probe name, from the last prepaint.
    static PROBES: std::cell::RefCell<BTreeMap<String, Bounds<Pixels>>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
    /// Activity shown in place of a chat's own, so no real state changes.
    static ACTIVITY: std::cell::RefCell<BTreeMap<SessionId, (SessionActivityState, bool)>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
    /// Local changes a scenario made, undone before the next one starts.
    static UNDO: std::cell::RefCell<Vec<fn(&mut Workspace)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// A scenario opened a menu, which the next scenario closes first.
    static MENU_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A change a scenario makes after its capture, recorded as a frame strip.
    static STRIP: std::cell::Cell<Option<Step>> = const { std::cell::Cell::new(None) };
}

/// After the capture, runs `change` and records the frames that follow, with every
/// transcript row's position, so motion can be checked for jumps.
pub(super) fn strip_after_capture(change: Step) {
    STRIP.with(|strip| strip.set(Some(change)));
}

/// Records `frames` frames after a scenario's `strip_after_capture` change.
#[expect(
    clippy::future_not_send,
    reason = "it drives the window on GPUI's foreground executor, whose context is not Send"
)]
async fn capture_strip(
    name: &str,
    dir: &std::path::Path,
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    let Some(change) = STRIP.with(std::cell::Cell::take) else {
        return;
    };
    let started = std::time::Instant::now();
    let mut captures = Vec::new();
    // Frame 0 is the settled screen before the change; each frame advances motion's clock
    // by exactly one display frame, however long its capture takes.
    for frame in 0..24 {
        crate::ui::primitives::motion::set_clock(Some(started + Duration::from_millis(16 * frame)));
        if frame == 1 {
            report("strip change", this.update_in(cx, change));
        }
        PROBES.with(|probes| {
            probes
                .borrow_mut()
                .retain(|id, _| !id.starts_with("row:") && !id.starts_with("pane:"));
        });
        report(
            "strip draw",
            cx.update(|window, cx| {
                // Retained paint skips the strip's probes unless explicitly refreshed.
                window.refresh();
                window.draw(cx).clear(cx);
                content::verify_pane_motion(name, frame, cx);
            }),
        );
        let rows = PROBES.with(|probes| {
            probes
                .borrow()
                .iter()
                .filter(|(id, _)| id.starts_with("row:"))
                .map(|(id, bounds)| {
                    format!(
                        "{id}={:.1}+{:.1}",
                        f32::from(bounds.top()),
                        f32::from(bounds.size.height)
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        });
        eprintln!("strip {name} {frame} {} {rows}", 16 * frame);
        if let Ok(Ok(image)) = this.update_in(cx, |_, window, _| window.render_to_image()) {
            captures.push((dir.join(format!("{name}-strip-{frame:02}.png")), image));
        }
        wait(cx.background_executor(), 16).await;
    }
    crate::ui::primitives::motion::set_clock(None);
    for (path, image) in captures {
        if let Err(error) = image.save(&path) {
            eprintln!("snapshot {}: {error}", path.display());
        }
    }
}

/// The (state, unread) a fixture shows for chat `id`.
pub(in crate::ui) fn activity(id: &str) -> Option<(SessionActivityState, bool)> {
    ACTIVITY.with(|fixture| fixture.borrow().get(id).copied())
}

pub(super) fn set_activity(fixture: BTreeMap<SessionId, (SessionActivityState, bool)>) {
    ACTIVITY.with(|activity| *activity.borrow_mut() = fixture);
}

/// Records the bounds of the element it fills under `name`, for `click_probe`.
pub(in crate::ui) fn probe(name: String) -> impl IntoElement {
    canvas(
        move |bounds, _, _| PROBES.with(|probes| probes.borrow_mut().insert(name, bounds)),
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Draws, then clicks `from_right` in from the right edge of probe `name`, vertically
/// centred; a right click only presses, as a context menu opens on the press. Scenarios
/// may only open menus this way, never pick an item.
pub(super) fn click_probe(
    name: String,
    from_right: Pixels,
    button: MouseButton,
    window: &Window,
    cx: &Context<Workspace>,
) {
    cx.spawn_in(window, async move |this, cx| {
        // Rows settle once the chat has opened, as the harness waits for.
        for _ in 0..50 {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            if this.read_with(cx, settled).unwrap_or(true) {
                break;
            }
        }
        cx.background_executor()
            .timer(Duration::from_millis(200))
            .await;
        let clicked = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let Some(bounds) = PROBES.with(|probes| probes.borrow().get(&name).copied()) else {
                eprintln!("snapshot: no probe {name}");
                return;
            };
            let position = point(
                bounds.right() - from_right,
                bounds.top() + bounds.size.height / 2.,
            );
            let modifiers = Modifiers::default();
            let moved = MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers,
            };
            window.dispatch_event(PlatformInput::MouseMove(moved), cx);
            // Hover reveals row actions; they take clicks only once drawn.
            window.draw(cx).clear(cx);
            let down = MouseDownEvent {
                button,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
            };
            window.dispatch_event(PlatformInput::MouseDown(down), cx);
            if button == MouseButton::Left {
                let up = MouseUpEvent {
                    button,
                    position,
                    modifiers,
                    click_count: 1,
                };
                window.dispatch_event(PlatformInput::MouseUp(up), cx);
            }
            MENU_OPEN.with(|open| open.set(true));
        });
        report("click", clicked);
    })
    .detach();
}

/// Undoes a scenario's local change once it has been captured.
pub(super) fn undo_after_capture(undo: fn(&mut Workspace)) {
    UNDO.with(|list| list.borrow_mut().push(undo));
}

/// Closes a menu the previous scenario opened; the open menu has focus, so Escape
/// reaches only it.
fn dismiss_menu(window: &mut Window, cx: &mut App) {
    if MENU_OPEN.with(|open| open.replace(false))
        && let Ok(escape) = Keystroke::parse("escape")
    {
        window.dispatch_keystroke(escape, cx);
    }
}

pub(super) type Step = fn(&mut Workspace, &mut Window, &mut Context<Workspace>);
pub(super) type Scenario = (&'static str, Step);

/// Scenarios that click their probed element after the first capture, twice, drawing
/// frame by frame so a transient layout jump shows in the probe log and the captures.
const CLICKS: [&[&str]; 1] = [chat::CLICKS];

/// A real pointer click through the window's dispatch.
fn click(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    let modifiers = Modifiers::default();
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
        }),
        cx,
    );
}

const PACKAGES: [&[Scenario]; 12] = [
    shell::SCENARIOS,
    sidebar::SCENARIOS,
    chat::SCENARIOS,
    attachments::SCENARIOS,
    content::SCENARIOS,
    bots::SCENARIOS,
    config::SCENARIOS,
    selection::SCENARIOS,
    background::SCENARIOS,
    documents::SCENARIOS,
    harness::SCENARIOS,
    memory::SCENARIOS,
];

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

/// A scenario's screen is ready: no chat page, or its chat has opened.
fn settled(this: &Workspace, _: &App) -> bool {
    !sidebar::title_scroll_pending()
        && !documents::images_pending()
        && !this.config.backdrop.is_loading()
        && (!matches!(this.page_tabs.first(), Some(PageTab::Chat(_)))
            || this.active_session_ready())
}

fn forced_appearance() -> Option<Appearance> {
    match std::env::var("MOBIUS_SNAPSHOT_APPEARANCE").ok()?.as_str() {
        "light" => Some(Appearance::Light),
        "dark" => Some(Appearance::Dark),
        _ => None,
    }
}

fn selected(name: &str, only: Option<&str>) -> bool {
    only.is_none_or(|only| only.split(',').any(|n| n == name))
}

/// Clicks each disclosure twice and records its changing bounds and painted frames.
#[expect(
    clippy::future_not_send,
    reason = "it drives the window on GPUI's foreground executor, whose context is not Send"
)]
async fn capture_clicks(
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

fn verify_scenario(name: &str, this: &Workspace, window: &mut Window, cx: &mut App) {
    content::verify_content_action(name, this, cx);
    if name == "routine-rejected-draft" {
        assert!(
            !window.has_active_dialog(cx),
            "routine reply self-check completed"
        );
    }
}

impl Workspace {
    pub(super) fn start_snapshots(window: &Window, cx: &Context<Self>) {
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
                let verified = this.update_in(cx, |this, window, cx| verify_scenario(name, this, window, cx));
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
