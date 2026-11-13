//! The gateway's computer, beside the conversation; native chrome surrounds stock noVNC.

use super::*;
use crate::gateway::{DesktopEvent, DesktopViewer};
use crate::host::browser::{Page, PageEvent, Shortcut};
use crate::numeric::Saturating;
use mobius::SessionId;
use mobius_gateway::wire::{ClientMessage, ComputerView};

pub(in crate::ui) const GLYPH: &str = "hi.laptopMinimal";
const TAB: &str = "computer";
const CLIPBOARD_LIMIT: usize = 1024 * 1024;

pub(in crate::ui) struct DesktopTab {
    generation: Arc<str>,
    bot_name: Arc<str>,
    page: Option<Page>,
    viewer: Option<DesktopViewer>,
    connected: bool,
    visible: bool,
    owner: bool,
    occupied: bool,
    pending: bool,
    fit: bool,
    closed: Option<SharedString>,
    /// The opening, event and page tasks, alive as long as the tab.
    tasks: Vec<Task<()>>,
}

fn accent(color: Hsla) -> String {
    let color: Rgba = color.into();
    format!(
        "#{:02x}{:02x}{:02x}",
        (color.r * 255.).round().saturating_u8(),
        (color.g * 255.).round().saturating_u8(),
        (color.b * 255.).round().saturating_u8()
    )
}

impl Workspace {
    pub(in crate::ui) fn sync_desktop_visibility(&mut self, visible: bool) {
        if let Some(tab) = &mut self.content.desktop
            && tab.visible != visible
        {
            tab.visible = visible;
            if let Some(page) = &tab.page {
                page.desktop_visible(visible);
            }
        }
    }

    fn interactive_computer_view(&self) -> ComputerView {
        if crate::host::browser::available()
            && self
                .composer_bot()
                .is_some_and(|bot| bot.capabilities.computer)
        {
            self.client
                .state
                .gateway
                .as_ref()
                .map_or(ComputerView::Unavailable, |gateway| gateway.computer_view)
        } else {
            ComputerView::Unavailable
        }
    }

    pub(in crate::ui) fn computer_available(&self) -> bool {
        self.computer_unavailable_reason().is_none()
    }

    /// Why the Bot's computer cannot open now.
    pub(in crate::ui) fn computer_unavailable_reason(&self) -> Option<&'static str> {
        if !self.client.link.is_ready() {
            return Some("Connect to a gateway to see the Bot’s computer.");
        }
        if !self.active_session_ready() {
            return Some("Wait for the chat to open.");
        }
        (self.interactive_computer_view() == ComputerView::Unavailable
            && Self::agent_browser_origin().is_none())
        .then_some("This Bot has no computer to show yet.")
    }

    pub(in crate::ui) fn computer_title(&self) -> String {
        self.composer_bot().map_or_else(
            || "Bot’s computer".into(),
            |bot| format!("{}’s computer", bot.name),
        )
    }

    /// The shared viewer takes control for the chat being shown, after it has opened.
    pub(in crate::ui) fn computer_control_session(&self) -> Option<SessionId> {
        self.active_session_ready()
            .then(|| self.client.state.selected())
            .flatten()
    }

    pub(in crate::ui) fn refresh_computer_title(&mut self) {
        if self.computer_control_session().is_none()
            || !self
                .content
                .navigation
                .contains(&TabOwner::of(&self.page_tabs), TAB)
        {
            return;
        }
        let title = self.computer_title();
        if let Some(tab) = self.content.tabs.get_mut(TAB) {
            tab.title = title.into();
        }
    }

    pub(in crate::ui) fn open_computer(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.computer_available() {
            return;
        }
        match self.interactive_computer_view() {
            ComputerView::EmbeddedBrowser => {
                if let Some(session_id) = self.client.state.selected() {
                    self.send(
                        ClientMessage::OpenComputer {
                            request_id: crate::gateway::request_id(),
                            session_id,
                        },
                        cx,
                    );
                }
                return;
            }
            ComputerView::Unavailable => {
                self.open_agent_browser(cx);
                return;
            }
            ComputerView::RemoteDesktop => {}
        }
        let content = ContentTab {
            id: TAB.into(),
            title: self.computer_title().into(),
            content: TabContent::Desktop,
        };
        if let Some(tab) = &self.content.desktop
            && tab.closed.is_none()
        {
            self.open_content(content, cx);
            return;
        }
        self.forget_desktop(TAB);
        let Some(gateway) = &self.client.gateway else {
            return;
        };
        let receiver = match gateway.open_desktop() {
            Ok(receiver) => receiver,
            Err(error) => {
                self.report(crate::gateway::describe(&error));
                return;
            }
        };
        let bot_name = self
            .composer_bot()
            .map_or_else(|| "Bot".into(), |bot| Arc::clone(&bot.name));
        let generation = crate::gateway::request_id();
        self.content.desktop = Some(DesktopTab {
            generation: Arc::clone(&generation),
            bot_name,
            page: None,
            viewer: None,
            connected: false,
            visible: true,
            owner: false,
            occupied: false,
            pending: false,
            fit: true,
            closed: None,
            tasks: Vec::new(),
        });
        self.open_content(content, cx);
        let opening = cx.spawn_in(window, async move |this, cx| {
            let result = receiver
                .await
                .map_err(|error| {
                    tracing::debug!(%error, "the gateway dropped the computer request");
                    "The computer connection closed".to_owned()
                })
                .and_then(|result| result);
            let updated = this.update_in(cx, |this, window, cx| {
                if this
                    .content
                    .desktop
                    .as_ref()
                    .is_none_or(|tab| tab.generation != generation)
                {
                    return;
                }
                match result {
                    Ok(viewer) => {
                        if let Err(error) = this.start_computer(viewer, window, cx) {
                            this.computer_closed(error.to_string(), cx);
                        }
                    }
                    Err(message) => this.computer_closed(message, cx),
                }
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "the workspace closed before the computer opened");
            }
        });
        if let Some(tab) = &mut self.content.desktop {
            tab.tasks.push(opening);
        }
        cx.notify();
    }

    fn start_computer(
        &mut self,
        mut viewer: DesktopViewer,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.ensure_browser_started(cx),
            "The browser isn't available."
        );
        let container = crate::host::platform::browser_container(TAB)
            .ok_or_else(|| anyhow::anyhow!("The computer has no window to draw in"))?;
        let (page_events, mut pages) = tokio::sync::mpsc::unbounded_channel();
        let page = Page::open_desktop(container, (1024, 768), &viewer.url, page_events)?;
        let generation = self
            .content
            .desktop
            .as_ref()
            .map(|tab| Arc::clone(&tab.generation))
            .ok_or_else(|| anyhow::anyhow!("The computer closed"))?;
        let page_generation = Arc::clone(&generation);
        let page_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = pages.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        if this
                            .content
                            .desktop
                            .as_ref()
                            .is_some_and(|tab| tab.generation == page_generation)
                        {
                            this.computer_page_event(event, window, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut events = viewer
            .take_events()
            .ok_or_else(|| anyhow::anyhow!("The computer has no event stream"))?;
        let event_task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| {
                        if this
                            .content
                            .desktop
                            .as_ref()
                            .is_some_and(|tab| tab.generation == generation)
                        {
                            this.computer_event(event, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        if let Some(tab) = &mut self.content.desktop {
            tab.page = Some(page);
            tab.viewer = Some(viewer);
            tab.tasks.push(event_task);
            tab.tasks.push(page_task);
        }
        cx.notify();
        Ok(())
    }

    fn computer_page_event(
        &mut self,
        event: PageEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PageEvent::Title(title)
                if self
                    .content
                    .desktop
                    .as_ref()
                    .is_some_and(|tab| tab.closed.is_none()) =>
            {
                match title.as_str() {
                    "Computer · paused" => {
                        if let Some(tab) = &mut self.content.desktop {
                            tab.connected = false;
                            tab.owner = false;
                            tab.pending = false;
                        }
                    }
                    "Computer · connecting" => {
                        if let Some(tab) = &mut self.content.desktop {
                            tab.connected = false;
                        }
                    }
                    "Computer · connected" => {
                        if let Some(tab) = &mut self.content.desktop {
                            tab.connected = true;
                        }
                    }
                    "Computer · disconnected" => self.computer_closed(
                        "The computer viewer disconnected. Reopen Computer to reconnect.".into(),
                        cx,
                    ),
                    "Computer · failed" => self.computer_closed(
                        "The computer could not connect. Reopen Computer to retry.".into(),
                        cx,
                    ),
                    _ => {}
                }
            }
            PageEvent::Shortcut(Shortcut::Paste) => self.paste_to_computer(cx),
            PageEvent::Shortcut(Shortcut::Reload) => {
                self.computer_closed("Reopening computer".into(), cx);
                self.open_computer(window, cx);
            }
            PageEvent::Focused => window.focus(&self.focus, cx),
            PageEvent::Loading { loading: false, .. } => {
                if let Some(tab) = &self.content.desktop
                    && let Some(page) = &tab.page
                {
                    page.desktop_control(tab.owner, &accent(self.palette.accent));
                    page.desktop_fit(tab.fit);
                    page.desktop_visible(tab.visible);
                }
            }
            PageEvent::Closed => self.computer_closed("Computer closed".into(), cx),
            // This page is never offered to an agent or turned into a regular browser tab.
            _ => {}
        }
        cx.notify();
    }

    fn computer_event(&mut self, event: DesktopEvent, cx: &mut Context<Self>) {
        match event {
            DesktopEvent::Control {
                enabled,
                is_owner,
                session_id,
            } => {
                self.desktop_execution_changed(enabled, cx);
                let name = session_id
                    .as_deref()
                    .and_then(|id| self.client.state.session_record(id))
                    .and_then(|session| self.client.state.bot(&session.session_context.owner_id))
                    .map(|bot| Arc::clone(&bot.name));
                if let Some(tab) = &mut self.content.desktop {
                    tab.owner = enabled && is_owner;
                    tab.occupied = enabled && !is_owner;
                    if !enabled || is_owner {
                        tab.pending = false;
                    }
                    if let Some(name) = name {
                        tab.bot_name = name;
                    }
                    if let Some(page) = &tab.page {
                        page.desktop_control(tab.owner, &accent(self.palette.accent));
                    }
                }
            }
            DesktopEvent::Error(message) => {
                if let Some(tab) = &mut self.content.desktop {
                    tab.pending = false;
                    if let Some(page) = &tab.page {
                        page.desktop_control(tab.owner, &accent(self.palette.accent));
                    }
                }
                self.report(message);
            }
            DesktopEvent::Closed(message) => self.computer_closed(message, cx),
        }
        cx.notify();
    }

    fn computer_closed(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(tab) = &mut self.content.desktop {
            tab.connected = false;
            tab.owner = false;
            tab.pending = false;
            tab.closed = Some(message.into());
            tab.viewer = None;
            if let Some(page) = &tab.page {
                page.desktop_control(false, &accent(self.palette.accent));
            }
        }
        cx.notify();
    }

    fn toggle_computer_control(&mut self, cx: &mut Context<Self>) {
        let session_id = self.computer_control_session();
        let Some(tab) = &mut self.content.desktop else {
            return;
        };
        let Some(viewer) = &tab.viewer else {
            return;
        };
        if !tab.connected || tab.pending || tab.occupied || (!tab.owner && session_id.is_none()) {
            return;
        }
        match viewer.set_control(!tab.owner, if tab.owner { None } else { session_id }) {
            Ok(()) => {
                tab.pending = true;
                if tab.owner
                    && let Some(page) = &tab.page
                {
                    page.desktop_control(false, &accent(self.palette.accent));
                }
            }
            Err(error) => self.report(error.to_string()),
        }
        cx.notify();
    }

    fn paste_to_computer(&mut self, cx: &Context<Self>) {
        let Some(tab) = &self.content.desktop else {
            return;
        };
        if !tab.connected || !tab.owner || tab.pending || tab.closed.is_some() {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.len() > CLIPBOARD_LIMIT {
            self.report("Clipboard text is too large to paste");
            return;
        }
        if let Some(page) = &tab.page {
            page.desktop_paste(&text);
        }
    }

    pub(in crate::ui) fn forget_desktop(&mut self, id: &str) {
        if id == TAB {
            self.content.desktop = None;
        }
    }

    pub(in crate::ui) fn forget_desktops(&mut self) {
        self.content.desktop = None;
    }

    pub(super) fn desktop_document(&self, cx: &Context<Self>) -> AnyElement {
        let Some(tab) = &self.content.desktop else {
            return div().into_any_element();
        };
        let body = if let Some(message) = &tab.closed {
            self.native.hide_panel();
            EmptyState::new(GLYPH, "Computer disconnected")
                .detail(SharedString::clone(message))
                .into_any_element()
        } else if tab.connected && tab.page.is_some() {
            Self::native_content(TAB, NativeView::Browser, cx)
        } else {
            self.native.hide_panel();
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new(glyph::ICON))
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(rgb(0x00_00_00))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(body)
                    .when(tab.connected && tab.closed.is_none(), |body| {
                        body.child(Self::computer_owner(cx))
                    }),
            )
            .child(self.computer_footer(cx))
            .into_any_element()
    }

    fn computer_owner(cx: &Context<Self>) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                if window.has_active_dialog(cx) {
                    return;
                }
                let Some(entity) = entity.upgrade() else {
                    return;
                };
                let this = entity.read(cx);
                if this.expanded.contains("native-overlay") {
                    return;
                }
                let Some(tab) = &this.content.desktop else {
                    return;
                };
                let label = if tab.owner {
                    "You have control".into()
                } else if tab.occupied {
                    "Another device has control".into()
                } else {
                    format!("{} has control", tab.bot_name)
                };
                let shown = this.native.show_computer_owner(
                    &label,
                    tab.owner,
                    this.palette.accent_fill.into(),
                    if tab.owner {
                        this.palette.on_accent
                    } else {
                        this.palette.text
                    }
                    .into(),
                    [
                        f64::from(bounds.origin.x),
                        f64::from(bounds.origin.y),
                        f64::from(bounds.size.width),
                        f64::from(bounds.size.height),
                    ],
                );
                if let Err(error) = shown {
                    tracing::warn!(%error, "the computer's control badge could not be shown");
                }
            },
        )
        .absolute()
        .top(space::S)
        .left(space::S)
        .right(space::S)
        .h(height::REGULAR)
    }

    fn computer_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let Some(tab) = &self.content.desktop else {
            return div();
        };
        let owner = tab.owner;
        let available = tab.connected && tab.viewer.is_some() && tab.closed.is_none();
        let action = if tab.pending {
            "Please wait…"
        } else if tab.closed.is_some() {
            "Take control"
        } else if !tab.connected {
            if tab.page.is_some() {
                "Connecting…"
            } else {
                "Starting up…"
            }
        } else if owner {
            "Return control"
        } else {
            "Take control"
        };
        let control = button("computer-control", action)
            .primary()
            .disabled(
                !available
                    || tab.pending
                    || tab.occupied
                    || (!owner && self.computer_control_session().is_none()),
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle_computer_control(cx)));
        let controls = div()
            .flex()
            .items_center()
            .gap(space::S)
            .child(control)
            .child(
                toolbar_button("computer-fit", "Fit computer to pane", "hi.expand", cx)
                    .disabled(!available)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(tab) = &mut this.content.desktop {
                            tab.fit = !tab.fit;
                            if let Some(page) = &tab.page {
                                page.desktop_fit(tab.fit);
                            }
                        }
                        cx.notify();
                    })),
            )
            .when(owner, |row| {
                row.child(
                    toolbar_button(
                        "computer-paste",
                        "Paste to computer · ⌘V",
                        "hi.clipboard",
                        cx,
                    )
                    .disabled(tab.pending)
                    .on_click(cx.listener(|this, _, _, cx| this.paste_to_computer(cx))),
                )
            });
        div()
            .bg(p.card)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .items_center()
            .gap(space::S)
            .px(space::S)
            .py(space::S)
            .child(controls)
            .when(available, |footer| {
                footer.child(div().typography(Type::Caption).text_color(p.muted).child(
                    "Use passwords or verification codes. Passkeys and Touch ID aren’t available here.",
                ))
            })
    }
}
