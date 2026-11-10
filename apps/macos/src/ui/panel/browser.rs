//! Browser tabs: Chromium pages drawn by native views over the pane, under a GPUI toolbar.
//! Every tab shares the app's browser profile. The gateway assigns a chat's rendered page.

use super::native::NativeView;
use super::*;
use crate::host::browser::{Page, PageEvent, PageEvents, Popup, Shortcut};
use crate::numeric::Saturating;
use gpui_component::input::InputEvent;
use mobius_gateway::wire::{ClientMessage, ComputerView};
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

pub(in crate::ui) const GLYPH: &str = "hi.globe02";
const NEW_TAB: &str = "about:blank";
const SEARCH: &str = "https://duckduckgo.com/?q=";

/// A browser tab's page and what its toolbar shows.
pub(in crate::ui) struct BrowserTab {
    /// Chromium's page; a popup's arrives once Chromium created it.
    page: Option<Page>,
    events: PageEvents,
    address: Entity<InputState>,
    loading: bool,
    back: bool,
    forward: bool,
    _events: Task<()>,
    _address: Subscription,
}

/// What the address field's text opens: an address as typed, a bare host, or a search.
fn address_url(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.contains(char::is_whitespace) {
        return Some(search_url(text));
    }
    let (scheme, rest) = text.split_once(':').unwrap_or_default();
    let scheme = scheme.to_ascii_lowercase();
    if rest.starts_with("//") || matches!(scheme.as_str(), "about" | "data" | "mailto" | "blob") {
        return Some(text.to_owned());
    }
    let host = text.split(['/', '?', '#']).next().unwrap_or_default();
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    if matches!(name, "localhost" | "127.0.0.1" | "[::1]") {
        return Some(format!("http://{text}"));
    }
    let hostname = name.contains('.')
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-'));
    Some(if hostname {
        format!("https://{text}")
    } else {
        search_url(text)
    })
}

fn search_url(query: &str) -> String {
    let mut url = String::from(SEARCH);
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                url.push(char::from(byte));
            }
            b' ' => url.push('+'),
            _ => {
                url.push('%');
                url.extend(
                    [byte >> 4, byte & 0xF]
                        .into_iter()
                        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
                        .map(|digit| digit.to_ascii_uppercase()),
                );
            }
        }
    }
    url
}

/// The page's background while it loads: the pane's card color, as Chromium's ARGB.
fn argb(color: Hsla) -> u32 {
    let Rgba { r, g, b, .. } = color.into();
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round().saturating_u32();
    0xFF_00_00_00 | channel(r) << 16 | channel(g) << 8 | channel(b)
}

impl Workspace {
    /// Adds a browser tab whose page reports on `channel`, opening `url` in a new page;
    /// a popup's page already exists and arrives on the channel instead.
    fn add_browser(
        &mut self,
        (id, owner): (&str, TabOwner),
        url: Option<&str>,
        (events, receiver): (PageEvents, UnboundedReceiver<PageEvent>),
        foreground: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            crate::host::browser::ensure_started(),
            "The browser isn't available."
        );
        let page = match url {
            Some(url) => {
                let container = crate::host::platform::browser_container(id)
                    .ok_or_else(|| anyhow::anyhow!("The browser has no window to draw in."))?;
                let background = argb(self.palette.card.alpha(1.));
                Some(Page::open(
                    container,
                    (1024, 768),
                    url,
                    background,
                    PageEvents::clone(&events), // rules-allow: a channel sender is a shared handle, cloning it adds a sender
                )?)
            }
            None => None,
        };
        let tab: Rc<str> = Rc::from(id);
        let address =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search or enter an address"));
        let entered = Rc::clone(&tab);
        let subscription = cx.subscribe_in(&address, window, move |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.navigate_browser(&entered, window, cx);
            }
        });
        let reporting = Rc::clone(&tab);
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut receiver = receiver;
            while let Some(event) = receiver.recv().await {
                let delivered = this.update_in(cx, |this, window, cx| {
                    this.browser_event(&reporting, event, window, cx);
                });
                if delivered.is_err() {
                    break;
                }
            }
        });
        self.content.browsers.insert(
            id.to_owned(),
            BrowserTab {
                page,
                events,
                address,
                loading: false,
                back: false,
                forward: false,
                _events: task,
                _address: subscription,
            },
        );
        let id: Arc<str> = id.into();
        let visible = owner == TabOwner::of(&self.page_tabs);
        self.content.navigation.insert(owner, Arc::clone(&id));
        self.content.tabs.insert(
            Arc::clone(&id),
            ContentTab {
                id: Arc::clone(&id),
                title: "New tab".into(),
                content: TabContent::Browser,
            },
        );
        let page = PageTab::Content(id);
        if foreground && visible {
            self.activate_tab(page);
        } else if visible {
            activate_page(&mut self.page_tabs, page);
        }
        Ok(())
    }

    fn add_popup(
        &mut self,
        owner: TabOwner,
        popup: Popup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Popup {
            tab,
            url,
            foreground,
            events,
            receiver,
        } = popup;
        if self
            .add_browser(
                (&tab, owner),
                None,
                (events, receiver),
                foreground,
                window,
                cx,
            )
            .is_ok()
            && let Some(browser) = self.content.browsers.get(&tab)
        {
            browser
                .address
                .update(cx, |input, cx| input.set_value(url, window, cx));
        }
    }

    fn browser_event(
        &mut self,
        id: &str,
        event: PageEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.content.browsers.get_mut(id) else {
            return;
        };
        match event {
            PageEvent::Created(browser) => {
                if tab.page.is_none() {
                    tab.page = Some(Page::adopt(browser));
                }
            }
            PageEvent::Address(url) => {
                let editing = tab.address.focus_handle(cx).is_focused(window);
                if !editing {
                    tab.address
                        .update(cx, |input, cx| input.set_value(url, window, cx));
                }
            }
            PageEvent::Title(title) => {
                if let Some(content) = self.content.tabs.get_mut(id) {
                    content.title = if title.is_empty() {
                        "Untitled".into()
                    } else {
                        title.into()
                    };
                }
            }
            PageEvent::Loading {
                loading,
                back,
                forward,
            } => {
                tab.loading = loading;
                tab.back = back;
                tab.forward = forward;
            }
            PageEvent::Opened(popup) => {
                let owner = self.content.navigation.owner(id).cloned(); // rules-allow: opening a popup mutably borrows the registry
                if let Some(owner) = owner {
                    self.add_popup(owner, popup, window, cx);
                }
            }
            // The page took the keyboard, so GPUI's fields let go of it.
            PageEvent::Focused => window.focus(&self.focus, cx),
            PageEvent::Shortcut(shortcut) => self.browser_shortcut(id, shortcut, window, cx),
            PageEvent::Closed => self.close_content(id, window, cx),
            PageEvent::DevTools(command) => {
                if let Some(page) = &mut tab.page {
                    page.devtools(command);
                }
            }
        }
        cx.notify();
    }

    fn browser_shortcut(
        &self,
        id: &str,
        shortcut: Shortcut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self
            .content
            .browsers
            .get(id)
            .and_then(|tab| tab.page.as_ref())
        else {
            return;
        };
        match shortcut {
            Shortcut::FocusAddress => self.focus_address(id, window, cx),
            Shortcut::Reload => page.reload(),
            Shortcut::Back => page.back(),
            Shortcut::Forward => page.forward(),
            Shortcut::Paste => {}
        }
    }

    /// Moves the keyboard from the page to the tab's address field.
    fn focus_address(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.content.browsers.get(id) {
            self.native.focus_app();
            tab.address.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    fn navigate_browser(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.content.browsers.get(id) else {
            return;
        };
        let Some(url) = address_url(&tab.address.read(cx).value()) else {
            return;
        };
        if let Some(page) = &tab.page {
            page.load(&url);
            page.focus();
            self.native.focus_browser(id);
            window.focus(&self.focus, cx);
        }
    }

    /// Closes the tab `id` wherever it is in the strip.
    fn close_content(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let tab = PageTab::Content(id.into());
        if let Some(index) = self.page_tabs.iter().position(|page| *page == tab) {
            self.close_page(index, window, cx);
        } else {
            self.forget_content(id, cx);
        }
    }

    /// The page the browser tab `id` shows, unless it is a blank new tab.
    pub(in crate::ui) fn browser_address(&self, id: &str, cx: &App) -> Option<SharedString> {
        let address = self.content.browsers.get(id)?.address.read(cx).value();
        (!address.is_empty() && address != NEW_TAB).then_some(address)
    }

    /// Drops a closed tab's page, which closes it in Chromium.
    pub(in crate::ui) fn forget_browser(&mut self, id: &str) {
        self.content.browsers.remove(id);
    }

    pub(in crate::ui) fn forget_browsers(&mut self) {
        self.content.browsers.clear();
    }

    /// Registers the renderer and answers the gateway's assignment with one scoped page.
    pub(in crate::ui) fn browser_on_message(
        &mut self,
        message: &ServerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match message {
            ServerMessage::Ready { payload }
                if payload.computer_view == ComputerView::EmbeddedBrowser
                    && crate::host::browser::running() =>
            {
                self.send(
                    ClientMessage::SetBrowserRuntime {
                        request_id: crate::gateway::request_id(),
                        enabled: true,
                    },
                    cx,
                );
            }
            ServerMessage::BrowserPageRequested {
                request_id,
                session_id,
                foreground,
            } => {
                let endpoint = match self.assigned_browser(session_id, *foreground, window, cx) {
                    Ok(endpoint) => Some(endpoint),
                    Err(error) => {
                        self.report(format!("The computer couldn't open its browser: {error:#}"));
                        None
                    }
                };
                self.send(
                    ClientMessage::BrowserPageReply {
                        request_id: Arc::clone(request_id),
                        endpoint,
                    },
                    cx,
                );
            }
            _ => {}
        }
    }

    /// Human-open and worker attachment use the same chat page. Only a human-open brings
    /// a tab forward; a Bot's page opens behind whatever the person is looking at.
    fn assigned_browser(
        &mut self,
        session: &str,
        foreground: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<String> {
        let id = format!("browser:chat:{session}");
        if !self.content.browsers.contains_key(&id) {
            self.add_browser(
                (&id, TabOwner::Chat(SessionId::new(session)?)),
                Some(NEW_TAB),
                unbounded_channel(),
                foreground,
                window,
                cx,
            )?;
        } else if foreground {
            self.activate_tab(PageTab::Content(id.as_str().into()));
        }
        let tab = self
            .content
            .browsers
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("The chat's browser tab closed."))?;
        let page = tab
            .page
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("The chat's page is still opening."))?;
        let endpoint = page.endpoint(&tab.events)?.to_owned();
        if foreground
            && self
                .content
                .navigation
                .contains(&TabOwner::of(&self.page_tabs), &id)
        {
            if self.browser_address(&id, cx).is_none() {
                self.focus_address(&id, window, cx);
            } else {
                if let Some(page) = self
                    .content
                    .browsers
                    .get(&id)
                    .and_then(|tab| tab.page.as_ref())
                {
                    page.focus();
                }
                self.native.focus_browser(&id);
                window.focus(&self.focus, cx);
            }
        }
        cx.notify();
        Ok(endpoint)
    }

    pub(super) fn browser_document(&self, tab_id: &str, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let Some(tab) = self.content.browsers.get(tab_id) else {
            return document(None, EmptyState::new(GLYPH, "This page closed"), cx);
        };
        let id: Rc<str> = Rc::from(tab_id);
        let action = |this: &mut Self, id: &str, act: fn(&Page)| {
            if let Some(page) = this
                .content
                .browsers
                .get(id)
                .and_then(|tab| tab.page.as_ref())
            {
                act(page);
            }
        };
        let (back, forward, reload) = (Rc::clone(&id), Rc::clone(&id), Rc::clone(&id));
        let driven = tab.page.as_ref().is_some_and(Page::driven);
        let bar = toolbar()
            .child(
                toolbar_button("browser-back", "Back", "hi.arrowLeft", cx)
                    .disabled(!tab.back)
                    .on_click(cx.listener(move |this, _, _, _| action(this, &back, Page::back))),
            )
            .child(
                toolbar_button("browser-forward", "Forward", "hi.arrowRight", cx)
                    .disabled(!tab.forward)
                    .on_click(
                        cx.listener(move |this, _, _, _| action(this, &forward, Page::forward)),
                    ),
            )
            .child(if tab.loading {
                toolbar_button("browser-stop", "Stop", "hi.x", cx)
                    .on_click(cx.listener(move |this, _, _, _| action(this, &reload, Page::stop)))
            } else {
                toolbar_button("browser-reload", "Reload", "hi.reload", cx)
                    .on_click(cx.listener(move |this, _, _, _| action(this, &reload, Page::reload)))
            })
            .child(div().flex_1().min_w_0().child(text_field(&tab.address)))
            .when(driven, |bar| {
                bar.child(status_label(p.accent, "Bot is using this page", cx))
            });
        document(
            bar,
            Self::native_content(tab_id, NativeView::Browser, cx),
            cx,
        )
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Opens a browser tab on `url`, as a new tab would after typing it.
    #[expect(
        clippy::print_stderr,
        reason = "the snapshot harness reports its failures on stderr"
    )]
    pub(in crate::ui) fn snapshot_browser(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = format!("browser:{}", crate::gateway::request_id());
        if let Err(error) = self.add_browser(
            (&id, TabOwner::of(&self.page_tabs)),
            Some(url),
            unbounded_channel(),
            true,
            window,
            cx,
        ) {
            eprintln!("snapshot: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::address_url;

    #[test]
    fn the_address_field_opens_addresses_and_searches_the_rest() {
        let open = |text: &str| address_url(text).expect("an address");
        assert_eq!(open("https://example.com/a?b"), "https://example.com/a?b");
        assert_eq!(open("  example.com/docs "), "https://example.com/docs");
        assert_eq!(open("localhost:3000/app"), "http://localhost:3000/app");
        assert_eq!(open("about:blank"), "about:blank");
        assert_eq!(
            open("rust borrow checker"),
            "https://duckduckgo.com/?q=rust+borrow+checker"
        );
        assert_eq!(open("c++"), "https://duckduckgo.com/?q=c%2B%2B");
        assert_eq!(open("mobius"), "https://duckduckgo.com/?q=mobius");
        assert_eq!(open("192.168.1.1"), "https://192.168.1.1");
        assert!(address_url("   ").is_none());
    }
}
