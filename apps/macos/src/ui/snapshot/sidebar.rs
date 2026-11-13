//! Sidebar scenarios: chat and project row actions, their menus, activity indicators
//! and the rail's highlight per section. Menus are only opened, never used, and
//! activity is a display fixture; nothing reaches the gateway.

use super::*;
use crate::ui::navigation::{Section, project_id};
use mobius_gateway::wire::SessionActivityState;

/// Half an inline button: where a click lands on a row's ⋯ from its inset.
const HALF_BUTTON: Pixels = px(13.);

pub(super) const SCENARIOS: &[Scenario] = &[
    ("sidebar-scroll-edge", scroll_edge_fixture),
    ("sidebar-title-scroll", |this, window, cx| {
        TITLE_SCROLL_PENDING.with(|pending| pending.set(true));
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, title_scroll_fixture);
    }),
    ("sidebar-row-actions", |this, window, cx| {
        open_chat(this, window, cx);
        reveal(this, |id, _| format!("chat:{id}"));
    }),
    ("sidebar-project-actions", |this, window, cx| {
        open_chat(this, window, cx);
        reveal(this, |_, project| format!("project:{project}"));
    }),
    ("sidebar-row-menu", |this, window, cx| {
        open_chat(this, window, cx);
        if let Some((id, _)) = open_row(this) {
            let at = row_actions_inset(false) + HALF_BUTTON;
            click_probe(format!("chat-row:{id}"), at, MouseButton::Left, window, cx);
        }
    }),
    ("sidebar-row-context-menu", |this, window, cx| {
        open_chat(this, window, cx);
        if let Some((id, _)) = open_row(this) {
            click_probe(
                format!("chat-row:{id}"),
                px(120.),
                MouseButton::Right,
                window,
                cx,
            );
        }
    }),
    ("sidebar-project-menu", |this, window, cx| {
        open_chat(this, window, cx);
        if let Some((_, project)) = open_row(this) {
            let at = row_actions_inset(true) + HALF_BUTTON;
            let name = format!("project-header:{project}");
            click_probe(name, at, MouseButton::Left, window, cx);
        }
    }),
    ("sidebar-pinned-project", |this, window, cx| {
        open_chat(this, window, cx);
        pin_oldest_project(this);
    }),
    ("sidebar-activity", |this, window, cx| {
        open_chat(this, window, cx);
        show_activity(this);
    }),
    ("perf-sidebar-activity", |this, window, cx| {
        open_chat(this, window, cx);
        show_activity(this);
    }),
    ("rail-chats", |this, window, cx| {
        rail(this, Section::Chats, window, cx);
    }),
    ("rail-chat-detail", |this, window, cx| {
        open_chat(this, window, cx);
        if let Some((id, _)) = open_row(this) {
            let detail = crate::ui::chat::ChatDetail::Context(id);
            this.open_chat_detail(detail, window, cx);
        }
    }),
    ("rail-bots", |this, window, cx| {
        rail(this, Section::Bots, window, cx);
    }),
    ("rail-gateway", |this, window, cx| {
        rail(this, Section::Gateway, window, cx);
    }),
    ("rail-mobius-live", |this, window, cx| {
        rail(this, Section::Mobius, window, cx);
    }),
    ("rail-events", |this, window, cx| {
        rail(this, Section::Events, window, cx);
    }),
    ("rail-settings", |this, window, cx| {
        rail(this, Section::Settings, window, cx);
    }),
];

/// A long, offline catalog, with the Bot strip and New Chat in the scroll area.
fn scroll_edge_fixture(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    harness::fixture(this);
    this.client.state.sessions = (0..40)
        .map(|index| {
            documents::fixture_session(
                &format!("fixture-sidebar-edge-{index}"),
                &format!("Review project changes and chat history {index}"),
                1_790_000_000_000 - index * 1_000,
            )
        })
        .collect();
    this.navigation_catalog = Default::default();
    this.navigation_catalog.organization = navigation::Organization::Chronological;
    this.gateway_revision += 1;
    this.new_chat(window, cx);
    this.navigation_visible = true;
    this.shell.catalog_scroll.set_offset(point(px(0.), px(0.)));
    strip_after_capture(|this, _, cx| {
        this.shell.catalog_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    });
    let scroll = ScrollHandle::clone(&this.shell.catalog_scroll);
    cx.spawn_in(window, async move |_, cx| {
        wait(cx.background_executor(), 100).await;
        report(
            "sidebar scroll edge",
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(
                    scroll.max_offset().y >= px(117.),
                    "fixture catalog must overflow"
                );
                let bounds = scroll.bounds();
                for distance in [60., 57.] {
                    window.dispatch_event(
                        PlatformInput::Scroll(ScrollEvent {
                            position: point(bounds.center().x, bounds.top() + space::XS),
                            delta: ScrollDelta::Pixels(point(px(0.), -px(distance))),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Moved,
                            source: ScrollSource::Pointer,
                        }),
                        cx,
                    );
                    window.draw(cx).clear(cx);
                }
                assert!(
                    scroll.offset().y < -px(100.),
                    "the visible edge must pass wheel input to the catalog"
                );
                title_pointer(
                    point(bounds.right() + space::L, bounds.center().y),
                    window,
                    cx,
                );
                window.draw(cx).clear(cx);
                eprintln!("snapshot sidebar-scroll-edge: overflowing catalog scrolled");
            }),
        );
    })
    .detach();
}

fn rail(this: &mut Workspace, section: Section, window: &mut Window, cx: &mut Context<Workspace>) {
    this.select_section(section, window, cx);
}

/// The open chat and its project.
fn open_row(this: &Workspace) -> Option<(SessionId, String)> {
    let Some(PageTab::Chat(id)) = this.primary_page() else {
        return None;
    };
    let session = this.client.state.session_record(id)?;
    Some((id.share(), project_id(session)?.to_owned()))
}

/// Shows a row's actions as hovering does, then hides them again after the capture.
fn reveal(this: &mut Workspace, key: fn(&str, &str) -> String) {
    let Some((id, project)) = open_row(this) else {
        return;
    };
    this.navigation_catalog.menu_open = Some(key(&id, &project));
    undo_after_capture(|this| this.navigation_catalog.menu_open = None);
}

/// Pins the project with the oldest chat in memory only, so it moves to the top.
fn pin_oldest_project(this: &mut Workspace) {
    let Some(oldest) = this
        .client
        .state
        .sessions
        .iter()
        .min_by_key(|s| s.updated_at)
    else {
        return;
    };
    let Some(project) = project_id(oldest) else {
        return;
    };
    let endpoint = this.client.endpoint.as_str().to_owned();
    let pins = &mut this.navigation_catalog.pinned_projects;
    let before = pins.clone(); // rules-allow: the scenario shows changed pins, then restores the exact set
    pins.entry(endpoint).or_default().insert(project.to_owned());
    PINS.with(|stash| *stash.borrow_mut() = Some(before));
    undo_after_capture(|this| {
        if let Some(before) = PINS.with(|stash| stash.borrow_mut().take()) {
            this.navigation_catalog.pinned_projects = before;
        }
    });
}

thread_local! {
    /// The real pinned projects while a scenario shows another pin.
    static PINS: std::cell::RefCell<Option<BTreeMap<String, std::collections::BTreeSet<String>>>> =
        const { std::cell::RefCell::new(None) };
    static TITLE: std::cell::RefCell<Option<(SessionId, Option<String>)>> =
        const { std::cell::RefCell::new(None) };
    static TITLE_SCROLL_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn title_scroll_pending() -> bool {
    TITLE_SCROLL_PENDING.with(std::cell::Cell::get)
}

/// Tests local title changes and hover; no click or rename is sent to the gateway.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a self-checking scenario: a failed step or missing probe is the failure report, as in tests"
)]
fn title_scroll_fixture(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let (id, project) = open_row(this).expect("sidebar-title-scroll needs an existing chat");
    let session = this
        .client
        .state
        .sessions
        .iter_mut()
        .find(|s| s.session_id == id)
        .unwrap();
    TITLE.with(|saved| *saved.borrow_mut() = Some((id.share(), session.title.take())));
    session.title = Some("Short chat".into());
    this.gateway_revision += 1;
    cx.notify();
    undo_after_capture(|this| {
        if let Some((id, title)) = TITLE.with(|saved| saved.borrow_mut().take())
            && let Some(session) = this
                .client
                .state
                .sessions
                .iter_mut()
                .find(|s| s.session_id == id)
        {
            session.title = title;
            this.gateway_revision += 1;
        }
    });
    cx.spawn_in(window, async move |this, cx| {
        wait(cx.background_executor(), 100).await;
        let short = cx.update(|window, cx| title_frame("short", &id, &project, window, cx)).unwrap();
        this.update_in(cx, |this, _, cx| {
            let session = this.client.state.sessions.iter_mut().find(|s| s.session_id == id).unwrap();
            session.title = Some("Review chat title scrolling and consistent sidebar action alignment".into());
            this.gateway_revision += 1;
            cx.notify();
        }).unwrap();
        wait(cx.background_executor(), 32).await;
        let initial = cx.update(|window, cx| title_frame("initial", &id, &project, window, cx)).unwrap();
        assert!((short[0].size.width - initial[0].size.width).abs() < px(1.), "title changed row width");
        assert!((short[3].right() - initial[3].right()).abs() < px(1.), "title moved row actions");
        assert!(initial[1].right() <= initial[3].left(), "title overlaps row actions");
        cx.update(|window, cx| title_pointer(initial[1].center(), window, cx)).unwrap();
        for _ in 0..40 {
            wait(cx.background_executor(), 33).await;
            cx.update(|window, cx| window.draw(cx).clear(cx)).unwrap();
        }
        let middle = cx.update(|window, cx| title_frame("hover", &id, &project, window, cx)).unwrap();
        let still = this.read_with(cx, |_, cx| cx.reduce_motion()).unwrap();
        if still {
            assert!((middle[2].left() - initial[2].left()).abs() < px(1.), "Reduce Motion moved title");
        } else {
            assert!(middle[2].left() < initial[2].left() - px(1.), "hover did not scroll clipped title");
        }
        assert_eq!(middle[1], initial[1], "hover moved the title viewport");
        cx.update(|window, cx| title_pointer(point(px(1.), px(1.)), window, cx)).unwrap();
        wait(cx.background_executor(), 33).await;
        let reset = cx.update(|window, cx| title_frame("reset", &id, &project, window, cx)).unwrap();
        assert!((reset[2].left() - initial[2].left()).abs() < px(1.), "leaving did not reset title");
        assert!((reset[3].right() - initial[3].right()).abs() < px(1.), "hover moved row actions");
        eprintln!("sidebar-title-scroll: stable row/actions; hover movement and reset passed (reduced motion: {still})");
        TITLE_SCROLL_PENDING.with(|pending| pending.set(false));
    }).detach();
}

#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "a self-checking scenario: a failed step or missing probe is the failure report, as in tests"
)]
fn title_frame(
    phase: &str,
    id: &str,
    project: &str,
    window: &mut Window,
    cx: &mut App,
) -> [Bounds<Pixels>; 5] {
    window.draw(cx).clear(cx);
    let bounds = PROBES.with(|probes| {
        let probes = probes.borrow();
        [
            probes[&format!("chat-row:{id}")],
            probes[&format!("title-viewport:chat:{id}")],
            probes[&format!("title-text:chat:{id}")],
            probes[&format!("row-actions:chat:{id}")],
            probes[&format!("project-header:{project}")],
        ]
    });
    assert!(
        (bounds[0].size.width - bounds[4].size.width).abs() < px(1.),
        "chat row exceeds its project header"
    );
    assert!(
        (bounds[0].right() - bounds[3].right() - row_actions_inset(false)).abs() < px(1.),
        "row actions lost their trailing inset"
    );
    assert!(
        bounds[1].left() >= bounds[0].left() && bounds[1].right() <= bounds[0].right(),
        "title viewport exceeds row bounds"
    );
    let directory = PathBuf::from(std::env::var_os("MOBIUS_SNAPSHOT").unwrap());
    window
        .render_to_image()
        .unwrap()
        .save(directory.join(format!("sidebar-title-scroll-{phase}.png")))
        .unwrap();
    eprintln!("sidebar-title-scroll {phase}: {bounds:?}");
    bounds
}

fn title_pointer(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
        cx,
    );
}

/// Shows the open chat's project siblings running, awaiting approval and unread.
fn show_activity(this: &mut Workspace) {
    let Some((open, project)) = open_row(this) else {
        return;
    };
    let states = [
        (SessionActivityState::Running, false),
        (SessionActivityState::AwaitingApproval, false),
        (SessionActivityState::Idle, true),
    ];
    let siblings = this
        .client
        .state
        .sessions
        .iter()
        .filter(|s| s.session_id != open && !s.pinned && project_id(s) == Some(project.as_str()))
        .map(|s| s.session_id.share());
    let mut fixture: BTreeMap<_, _> = siblings.zip(states).collect();
    fixture.insert(open, (SessionActivityState::Running, false));
    set_activity(fixture);
    this.gateway_revision += 1;
    undo_after_capture(|this| {
        set_activity(BTreeMap::new());
        this.gateway_revision += 1;
    });
}
