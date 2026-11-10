//! The agent's browser tab: the computer-control browser streamed from its loopback
//! DevTools port, shared by sending clicks, scrolling and typing to the same page. Only a
//! local gateway's browser can be watched.

use super::*;
use crate::host::agent_browser::{self, BrowserEvent, BrowserInput};
use gpui::{ImageSource, ObjectFit, RenderImage, ScrollDelta};
use mobius::SessionId;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;

pub(in crate::ui) const GLYPH: &str = super::desktop::GLYPH;

/// A watched agent browser, kept per tab; dropping it ends the connection.
pub(in crate::ui) struct AgentBrowserView {
    origin: String,
    input: Sender<BrowserInput>,
    // Replacing or closing the tab cancels its receiver before it can update another watch.
    _events: Task<()>,
    /// The newest frame and the page's size in CSS pixels.
    frame: Option<(Arc<RenderImage>, f32, f32)>,
    url: String,
    closed: Option<String>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
    clicks: u32,
    visible: bool,
}

/// Where a point in the tab lands on the page, given the frame drawn to fit `bounds`.
fn page_point(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    page: (f32, f32),
) -> Option<(f32, f32)> {
    let (width, height) = page;
    let (area_width, area_height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    if width <= 0. || height <= 0. || area_width <= 0. || area_height <= 0. {
        return None;
    }
    let scale = (area_width / width).min(area_height / height);
    let left = f32::from(bounds.origin.x) + (area_width - width * scale) / 2.;
    let top = f32::from(bounds.origin.y) + (area_height - height * scale) / 2.;
    let x = (f32::from(position.x) - left) / scale;
    let y = (f32::from(position.y) - top) / scale;
    ((0. ..=width).contains(&x) && (0. ..=height).contains(&y)).then_some((x, y))
}

impl Workspace {
    /// The open chat's agent browser. No gateway widget announces one since protocol 93.
    pub(in crate::ui) fn agent_browser_origin() -> Option<String> {
        None
    }

    /// Opens, or returns to, the chat's agent browser tab.
    pub(in crate::ui) fn open_agent_browser(&mut self, cx: &mut Context<Self>) {
        let (Some(origin), Some(session)) = (
            Self::agent_browser_origin(),
            self.client
                .state
                .selected_session_id
                .as_ref()
                .map(SessionId::share),
        ) else {
            self.report("The agent's browser is only available while it runs on this Mac.");
            cx.notify();
            return;
        };
        let id: Arc<str> = format!("agent-browser:{session}").into();
        if self.content.tabs.contains_key(&id) {
            // A restarted browser has a new port; a closed connection is retried.
            if self
                .content
                .agent_browsers
                .get(&*id)
                .is_none_or(|view| view.closed.is_some() || view.origin != origin)
            {
                self.watch_agent_browser(&id, origin, cx);
            }
            self.activate_tab(PageTab::Content(id));
            cx.notify();
            return;
        }
        self.watch_agent_browser(&id, origin, cx);
        self.open_content(
            ContentTab {
                id,
                title: self.computer_title().into(),
                content: TabContent::AgentBrowser,
            },
            cx,
        );
    }

    fn watch_agent_browser(&mut self, id: &str, origin: String, cx: &mut Context<Self>) {
        let (input, mut events) = agent_browser::watch(origin.clone()); // rules-allow: the watcher and the tab's view each own the origin
        let tab_id = id.to_owned();
        let receiver = cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                let applied =
                    this.update(cx, |this, cx| this.agent_browser_event(&tab_id, event, cx));
                if applied.is_err() {
                    break;
                }
            }
        });
        if let Some(old) = self.content.agent_browsers.insert(
            id.to_owned(),
            AgentBrowserView {
                origin,
                input,
                _events: receiver,
                frame: None,
                url: String::new(),
                closed: None,
                bounds: Rc::default(),
                focus: cx.focus_handle(),
                clicks: 0,
                visible: true,
            },
        ) && let Some((image, ..)) = old.frame
        {
            cx.drop_image(image, None);
        }
    }

    pub(in crate::ui) fn sync_agent_browser_visibility(&mut self, visible: Option<&str>) {
        for (id, view) in &mut self.content.agent_browsers {
            let enabled = visible == Some(id.as_str());
            if enabled != view.visible
                && view.input.try_send(BrowserInput::Visible(enabled)).is_ok()
            {
                view.visible = enabled;
            }
        }
    }

    fn agent_browser_event(&mut self, id: &str, event: BrowserEvent, cx: &mut Context<Self>) {
        let computer_title = self.computer_title();
        let Some(view) = self.content.agent_browsers.get_mut(id) else {
            return;
        };
        let frame = matches!(&event, BrowserEvent::Frame { .. });
        match event {
            BrowserEvent::Frame {
                image,
                width,
                height,
            } => {
                if let Some((old, ..)) = view.frame.replace((image, width, height)) {
                    cx.drop_image(old, None);
                }
            }
            BrowserEvent::Page { url, title } => {
                view.url = url;
                if let Some(tab) = self.content.tabs.get_mut(id) {
                    tab.title = if title.is_empty() {
                        computer_title.into()
                    } else {
                        title.into()
                    };
                }
            }
            BrowserEvent::Closed(reason) => view.closed = Some(reason),
        }
        if !frame || view.visible {
            cx.notify();
        }
    }

    /// The page the agent's browser in tab `id` shows.
    pub(in crate::ui) fn agent_browser_url(&self, id: &str) -> Option<&str> {
        Some(self.content.agent_browsers.get(id)?.url.as_str()).filter(|url| !url.is_empty())
    }

    /// Ends a closed tab's connection and frees its last frame.
    pub(in crate::ui) fn forget_agent_browser(&mut self, id: &str, cx: &mut App) {
        if let Some(view) = self.content.agent_browsers.remove(id)
            && let Some((image, ..)) = view.frame
        {
            cx.drop_image(image, None);
        }
    }

    pub(in crate::ui) fn forget_agent_browsers(&mut self, cx: &mut App) {
        for (_, view) in std::mem::take(&mut self.content.agent_browsers) {
            if let Some((image, ..)) = view.frame {
                cx.drop_image(image, None);
            }
        }
    }

    /// Whether the agent may be driving its browser: shared input waits while its turn runs.
    fn agent_using_browser(&self) -> bool {
        self.turn_running()
    }

    fn send_browser_input(&mut self, id: &str, input: BrowserInput) {
        if let Some(view) = self.content.agent_browsers.get(id)
            && !self.agent_using_browser()
            && let Err(error) = view.input.try_send(input)
        {
            self.report(match error {
                TrySendError::Full(_) => {
                    "The browser is busy; input was not sent. Try again shortly."
                }
                TrySendError::Closed(_) => "The browser connection has closed; input was not sent.",
            });
        }
    }

    fn browser_point(&self, id: &str, position: Point<Pixels>) -> Option<(f32, f32)> {
        let view = self.content.agent_browsers.get(id)?;
        let (_, width, height) = view.frame.as_ref()?;
        page_point(position, view.bounds.get(), (*width, *height))
    }

    pub(super) fn agent_browser_document(&self, tab_id: &str, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let Some(view) = self.content.agent_browsers.get(tab_id) else {
            return document(
                None,
                EmptyState::new(GLYPH, "The agent's browser isn't connected"),
                cx,
            );
        };
        let busy = self.agent_using_browser();
        let reconnect = Self::agent_browser_origin()
            .is_some_and(|origin| view.closed.is_some() || origin != view.origin);
        let bar = toolbar()
            .child(path_label(
                if view.url.is_empty() {
                    "Computer"
                } else {
                    &view.url
                },
                GLYPH,
                cx,
            ))
            .child(spacer())
            .when(busy, |bar| {
                bar.child(status_label(p.muted, "Agent is using the browser…", cx))
            })
            .when(reconnect, |bar| {
                bar.child(
                    button("agent-browser-reconnect", "Reconnect")
                        .on_click(cx.listener(|this, _, _, cx| this.open_agent_browser(cx))),
                )
            });
        let body = match (&view.frame, &view.closed) {
            (Some((image, ..)), _) => {
                self.agent_browser_frame(tab_id, view, Arc::clone(image), busy, cx)
            }
            (None, Some(_)) => EmptyState::new(GLYPH, "The agent's browser isn't running")
                .detail("It stopped or restarted. Reconnect once the agent uses its browser again.")
                .into_any_element(),
            (None, None) => EmptyState::new(GLYPH, "Connecting to the agent's browser")
                .loading()
                .into_any_element(),
        };
        document(bar, body, cx)
    }

    fn agent_browser_frame(
        &self,
        tab_id: &str,
        view: &AgentBrowserView,
        image: Arc<RenderImage>,
        busy: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let bounds = Rc::clone(&view.bounds);
        let focus = FocusHandle::clone(&view.focus);
        let id = |id: &str| id.to_owned();
        let (down, up, moved, wheel, key) =
            (id(tab_id), id(tab_id), id(tab_id), id(tab_id), id(tab_id));
        div()
            .id(SharedString::from(format!("{tab_id}:page")))
            .relative()
            .flex_1()
            .min_h_0()
            .bg(p.inset)
            .track_focus(&view.focus)
            .child(
                canvas(move |area, _, _| bounds.set(area), |_, (), _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                img(ImageSource::Render(image))
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&focus, cx);
                    let Some((x, y)) = this.browser_point(&down, event.position) else {
                        return;
                    };
                    let clicks = u32::try_from(event.click_count).unwrap_or(u32::MAX);
                    if let Some(view) = this.content.agent_browsers.get_mut(&down) {
                        view.clicks = clicks;
                    }
                    this.send_browser_input(&down, BrowserInput::Press { x, y, clicks });
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, _| {
                    let view = this.content.agent_browsers.get(&up);
                    let clicks = view.map_or(1, |view| view.clicks.max(1));
                    if let Some((x, y)) = this.browser_point(&up, event.position) {
                        this.send_browser_input(&up, BrowserInput::Release { x, y, clicks });
                    }
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, _| {
                if let Some((x, y)) = this.browser_point(&moved, event.position) {
                    this.send_browser_input(&moved, BrowserInput::Move { x, y });
                }
            }))
            .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, _| {
                let Some((x, y)) = this.browser_point(&wheel, event.position) else {
                    return;
                };
                let (dx, dy) = match event.delta {
                    ScrollDelta::Pixels(delta) => (f32::from(delta.x), f32::from(delta.y)),
                    ScrollDelta::Lines(delta) => (delta.x * 40., delta.y * 40.),
                };
                // DevTools scrolls down for a positive delta; GPUI reports the finger's motion.
                this.send_browser_input(
                    &wheel,
                    BrowserInput::Wheel {
                        x,
                        y,
                        dx: -dx,
                        dy: -dy,
                    },
                );
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                let keystroke = &event.keystroke;
                let input = if keystroke.modifiers.platform && keystroke.key == "v" {
                    cx.read_from_clipboard()
                        .and_then(|item| item.text())
                        .map(BrowserInput::Text)
                } else if keystroke.modifiers.platform || keystroke.modifiers.control {
                    None
                } else if let Some(special) = agent_browser::special_key(&keystroke.key) {
                    Some(BrowserInput::Key(special))
                } else {
                    keystroke.key_char.clone().map(BrowserInput::Text) // rules-allow: the event is borrowed and the page input owns its text
                };
                if let Some(input) = input {
                    this.send_browser_input(&key, input);
                    cx.stop_propagation();
                }
            }))
            .when(busy, |page| {
                page.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(p.card.opacity(0.18))
                        .cursor_not_allowed(),
                )
            })
            .into_any_element()
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Opens an agent browser tab on `origin` directly, without the gateway's widget; with
    /// `text`, focuses the page's first field with Tab and types it once frames arrive.
    pub(in crate::ui) fn snapshot_agent_browser(
        &mut self,
        origin: String,
        text: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        const ID: &str = "agent-browser:snapshot";
        self.watch_agent_browser(ID, origin, cx);
        self.open_content(
            ContentTab {
                id: ID.into(),
                title: "Agent browser".into(),
                content: TabContent::AgentBrowser,
            },
            cx,
        );
        let Some(text) = text else { return };
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let typed = this.update(cx, |this, _| {
                let tab = agent_browser::special_key("tab").map(BrowserInput::Key);
                for input in tab.into_iter().chain([BrowserInput::Text(text.into())]) {
                    this.send_browser_input(ID, input);
                }
            });
            if let Err(error) = typed {
                tracing::debug!(%error, "the workspace closed before the snapshot text was typed");
            }
        })
        .detach();
    }
}
