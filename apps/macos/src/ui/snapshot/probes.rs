//! Snapshot instrumentation, readiness, pointer probes and frame strips.

use super::{Step, content, documents, sidebar};

use crate::ui::Workspace;
use crate::ui::router::PageTab;

use gpui::{
    App, AsyncWindowContext, BackgroundExecutor, Bounds, Context, IntoElement, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput,
    Point, Result, Styled as _, Task, WeakEntity, Window, canvas, point,
};

use mobius::SessionId;
use mobius_gateway::wire::SessionActivityState;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
/// Renders of the workspace, the transcript and the chat catalog, logged by `perf-*` scenarios.
pub(super) static RENDERS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(in crate::ui) fn count_render(slot: usize) {
    if let Some(count) = RENDERS.get(slot) {
        count.fetch_add(1, Ordering::Relaxed);
    }
}

/// The painted bounds of the element a `probe` watches, logged frame by frame.
pub(super) static PROBE: Mutex<Option<Probe>> = Mutex::new(None);

/// The watched element's id and its last painted bounds.
pub(super) type Probe = (String, Bounds<Pixels>);

/// The watched probe; after a panic while it was held the harness stops probing.
pub(super) fn probe_state() -> Option<MutexGuard<'static, Option<Probe>>> {
    PROBE.lock().ok()
}

/// Stops watching the probed element between scenarios.
pub(super) fn clear_probe() {
    if let Some(mut probe) = probe_state() {
        *probe = None;
    }
}

/// Zeroes the render counters a `perf-*` scenario reports.
pub(super) fn reset_renders() {
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
pub(super) fn wait(executor: &BackgroundExecutor, ms: u64) -> Task<()> {
    executor.timer(Duration::from_millis(ms))
}

/// Reports a step that could not run because its window or workspace is gone.
pub(super) fn report<E: std::fmt::Display>(step: &str, result: Result<(), E>) {
    if let Err(error) = result {
        eprintln!("snapshot {step}: {error}");
    }
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
    pub(super) static PROBES: std::cell::RefCell<BTreeMap<String, Bounds<Pixels>>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
    /// Activity shown in place of a chat's own, so no real state changes.
    static ACTIVITY: std::cell::RefCell<BTreeMap<SessionId, (SessionActivityState, bool)>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
    /// Local changes a scenario made, undone before the next one starts.
    pub(super) static UNDO: std::cell::RefCell<Vec<fn(&mut Workspace)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// A scenario opened a menu, which the next scenario closes first.
    static MENU_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A change a scenario makes after its capture, recorded as a frame strip.
    static STRIP: std::cell::Cell<Option<Step>> = const { std::cell::Cell::new(None) };
}

/// After the capture, runs `change` and records the frames that follow, with every
/// transcript row's position, so motion can be checked for jumps.
pub(in crate::ui) fn strip_after_capture(change: Step) {
    STRIP.with(|strip| strip.set(Some(change)));
}

/// Records `frames` frames after a scenario's `strip_after_capture` change.
#[expect(
    clippy::future_not_send,
    reason = "it drives the window on GPUI's foreground executor, whose context is not Send"
)]
pub(super) async fn capture_strip(
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
pub(in crate::ui) fn undo_after_capture(undo: fn(&mut Workspace)) {
    UNDO.with(|list| list.borrow_mut().push(undo));
}

/// Closes a menu the previous scenario opened; the open menu has focus, so Escape
/// reaches only it.
pub(super) fn dismiss_menu(window: &mut Window, cx: &mut App) {
    if MENU_OPEN.with(|open| open.replace(false))
        && let Ok(escape) = Keystroke::parse("escape")
    {
        window.dispatch_keystroke(escape, cx);
    }
}

/// A real pointer click through the window's dispatch.
pub(super) fn click(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
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

/// A scenario's screen is ready: no chat page, or its chat has opened.
pub(super) fn settled(this: &Workspace, _: &App) -> bool {
    !sidebar::title_scroll_pending()
        && !documents::images_pending()
        && !this.config.backdrop.is_loading()
        && (!matches!(this.page_tabs.first(), Some(PageTab::Chat(_)))
            || this.active_session_ready())
}
