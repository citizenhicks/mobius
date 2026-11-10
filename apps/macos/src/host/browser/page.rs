//! One browser tab's page: a Chromium browser drawn into the tab's native container. Its
//! handlers run on the main thread and report to the tab through a channel.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use anyhow::{Context, Result};
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

// cef's `wrap_*!` macros expand to unqualified handler, `Impl*` and `Wrap*` names, so
// they need the crate's glob import; it is the one glob outside parent re-exports.
use super::{devtools, engine};
use cef::*;

/// What a page tells its tab.
pub enum PageEvent {
    /// The browser exists; pages opened as popups arrive this way.
    Created(Browser),
    Address(String),
    Title(String),
    Loading {
        loading: bool,
        back: bool,
        forward: bool,
    },
    /// A link or script opened a new tab, already drawing into the container made for it.
    Opened(Popup),
    /// The page took keyboard focus from the rest of the window.
    Focused,
    Shortcut(Shortcut),
    Closed,
    /// The assigned page endpoint connected, sent a message, or left.
    DevTools(devtools::Command),
}

/// A page opened from another: its tab, and the channel its browser reports on.
pub struct Popup {
    pub tab: String,
    pub url: String,
    pub foreground: bool,
    pub events: PageEvents,
    pub receiver: UnboundedReceiver<PageEvent>,
}

/// Browser shortcuts typed while the page has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    FocusAddress,
    Reload,
    Back,
    Forward,
    Paste,
}

impl Shortcut {
    fn of(event: &KeyEvent) -> Option<Self> {
        let command = cef::sys::cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0;
        // Caps Lock and the like may accompany ⌘; any other modifier makes another shortcut.
        let others = cef::sys::cef_event_flags_t::EVENTFLAG_SHIFT_DOWN.0
            | cef::sys::cef_event_flags_t::EVENTFLAG_CONTROL_DOWN.0
            | cef::sys::cef_event_flags_t::EVENTFLAG_ALT_DOWN.0;
        if event.type_ != KeyEventType::RAWKEYDOWN
            || event.modifiers & command == 0
            || event.modifiers & others != 0
        {
            return None;
        }
        match event.windows_key_code {
            0x4C => Some(Self::FocusAddress),
            0x52 => Some(Self::Reload),
            0xDB => Some(Self::Back),
            0xDD => Some(Self::Forward),
            0x56 => Some(Self::Paste),
            _ => None,
        }
    }
}

pub type PageEvents = UnboundedSender<PageEvent>;

/// Tells the tab; a tab that already closed no longer listens.
fn report(events: &PageEvents, event: PageEvent) {
    if let Err(error) = events.send(event) {
        tracing::debug!(%error, "The browser tab closed before its page's event arrived");
    }
}

/// A tab's Chromium browser. Main thread only; dropping it closes the browser.
pub struct Page {
    browser: Browser,
    /// The scoped endpoint, created when the gateway assigns this page.
    grant: Option<devtools::Grant>,
    connections: Connections,
    /// Feeds the page's messages to the current connection.
    observer: Option<Registration>,
}

/// The agent connections that may reach a page: the newest, and those it replaced, which
/// no longer hear the page but may still detach the sessions they attached.
#[derive(Default)]
struct Connections {
    current: Option<u64>,
    retiring: Vec<u64>,
}

impl Connections {
    fn attach(&mut self, connection: u64) {
        if let Some(replaced) = self.current.replace(connection) {
            self.retiring.push(replaced);
        }
    }

    fn admits(&self, connection: u64) -> bool {
        self.current == Some(connection) || self.retiring.contains(&connection)
    }

    /// Forgets `connection`; true when it was the current one.
    fn detach(&mut self, connection: u64) -> bool {
        self.retiring.retain(|retiring| *retiring != connection);
        self.current
            .take_if(|current| *current == connection)
            .is_some()
    }
}

impl Page {
    /// Opens `url` in a new browser drawing into `container`, an NSView of `size` points.
    pub fn open(
        container: NonNull<c_void>,
        size: (i32, i32),
        url: &str,
        background: u32,
        events: PageEvents,
    ) -> Result<Self> {
        Self::open_page(container, size, url, background, events, false)
    }

    pub fn open_desktop(
        container: NonNull<c_void>,
        size: (i32, i32),
        url: &str,
        events: PageEvents,
    ) -> Result<Self> {
        Self::open_page(container, size, url, 0xFF_00_00_00, events, true)
    }

    fn open_page(
        container: NonNull<c_void>,
        size: (i32, i32),
        url: &str,
        background: u32,
        events: PageEvents,
        desktop: bool,
    ) -> Result<Self> {
        let (width, height) = size;
        let window = WindowInfo {
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        }
        .set_as_child(
            container.as_ptr(),
            &Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        );
        let settings = BrowserSettings {
            background_color: background,
            ..Default::default()
        };
        let mut client = TabClient::new(events, desktop);
        let browser = browser_host_create_browser_sync(
            Some(&window),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&settings),
            None,
            None,
        )
        .context("Chromium could not open the page")?;
        Ok(Self::adopt(browser))
    }

    /// Takes over a browser Chromium created itself, such as a popup's.
    pub fn adopt(browser: Browser) -> Self {
        Self {
            browser,
            grant: None,
            connections: Connections::default(),
            observer: None,
        }
    }

    /// Loads `url` in the main frame.
    pub fn load(&self, url: &str) {
        if let Some(frame) = self.browser.main_frame() {
            frame.load_url(Some(&CefString::from(url)));
        }
    }

    /// Invokes the bundled desktop viewer's public noVNC UI helpers, on the main thread.
    pub fn desktop_control(&self, enabled: bool, accent: &str) {
        self.desktop_call("setControl", &serde_json::json!([enabled, accent]));
    }

    pub fn desktop_paste(&self, text: &str) {
        self.desktop_call("paste", &serde_json::json!([text]));
    }

    pub fn desktop_fit(&self, enabled: bool) {
        self.desktop_call("setFit", &serde_json::json!([enabled]));
    }

    pub fn desktop_visible(&self, enabled: bool) {
        self.desktop_call("setVisible", &serde_json::json!([enabled]));
    }

    fn desktop_call(&self, method: &str, arguments: &serde_json::Value) {
        if let Some(frame) = self.browser.main_frame() {
            let script = format!("window.desktop?.{method}(...{arguments})");
            frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
        }
    }

    pub fn back(&self) {
        self.browser.go_back();
    }

    pub fn forward(&self) {
        self.browser.go_forward();
    }

    pub fn reload(&self) {
        self.browser.reload();
    }

    pub fn stop(&self) {
        self.browser.stop_load();
    }

    /// Gives the page keyboard focus within Chromium.
    pub fn focus(&self) {
        if let Some(host) = self.browser.host() {
            host.set_focus(1);
        }
    }
    /// The address the chat's agent connects to, valid while the page is open. Its
    /// connections report to the page's tab through `events`.
    pub fn endpoint(&mut self, events: &PageEvents) -> Result<&str> {
        let grant = match &mut self.grant {
            Some(grant) => grant,
            empty => empty.insert(devtools::grant(UnboundedSender::clone(events))?),
        };
        Ok(grant.endpoint())
    }

    /// Whether an agent is connected to the page.
    pub fn driven(&self) -> bool {
        self.connections.current.is_some()
    }

    /// Applies what the agent endpoint asked for. A newer connection replaces an older one,
    /// which stops hearing the page but may still detach the session it attached.
    pub fn devtools(&mut self, command: devtools::Command) {
        match command {
            devtools::Command::Attach {
                connection,
                messages,
            } => {
                let mut observer = Observer::new(RefCell::new(Some(messages)));
                let registration = self
                    .browser
                    .host()
                    .and_then(|host| host.add_dev_tools_message_observer(Some(&mut observer)));
                if registration.is_some() {
                    self.observer = registration;
                    self.connections.attach(connection);
                }
            }
            devtools::Command::Send {
                connection,
                message,
                processed,
            } => {
                if self.connections.current == Some(connection)
                    && let Some(host) = self.browser.host()
                {
                    host.send_dev_tools_message(Some(message.as_bytes()));
                }
                drop(processed);
            }
            devtools::Command::Detach {
                connection,
                message,
            } => {
                if self.connections.admits(connection)
                    && let Some(message) = message
                    && let Some(host) = self.browser.host()
                {
                    host.send_dev_tools_message(Some(message.as_bytes()));
                }
                if self.connections.detach(connection) {
                    self.observer = None;
                }
            }
        }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        if engine::running()
            && let Some(host) = self.browser.host()
        {
            host.close_browser(1);
        }
    }
}

wrap_dev_tools_message_observer! {
    struct Observer {
        messages: RefCell<Option<Sender<String>>>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_message(&self, _browser: Option<&mut Browser>, message: Option<&[u8]>) -> i32 {
            if let Some(message) = message.and_then(|bytes| std::str::from_utf8(bytes).ok()) {
                forward_message(&self.messages, message);
            }
            1
        }
    }
}

/// Never block Chromium's main thread or retain output for a stalled agent.
fn forward_message(messages: &RefCell<Option<Sender<String>>>, message: &str) {
    let mut messages = messages.borrow_mut();
    if let Some(sender) = messages.as_ref()
        && (message.len() > devtools::MAX_MESSAGE_BYTES
            || sender.try_send(message.to_owned()).is_err())
    {
        *messages = None;
    }
}

wrap_client! {
    struct TabClient {
        events: PageEvents,
        desktop: bool,
    }

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(Display::new(UnboundedSender::clone(&self.events)))
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(Loading::new(UnboundedSender::clone(&self.events)))
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(LifeSpan::new(UnboundedSender::clone(&self.events)))
        }

        fn focus_handler(&self) -> Option<FocusHandler> {
            Some(Focus::new(UnboundedSender::clone(&self.events)))
        }

        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(Keyboard::new(UnboundedSender::clone(&self.events), self.desktop))
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(Downloads::new())
        }
    }
}

fn text(value: Option<&CefString>) -> String {
    value.map(CefString::to_string).unwrap_or_default()
}

wrap_display_handler! {
    struct Display {
        events: PageEvents,
    }

    impl DisplayHandler {
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|frame| frame.is_main() == 1) {
                report(&self.events, PageEvent::Address(text(url)));
            }
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            report(&self.events, PageEvent::Title(text(title)));
        }
    }
}

wrap_load_handler! {
    struct Loading {
        events: PageEvents,
    }

    impl LoadHandler {
        fn on_loading_state_change(&self, _browser: Option<&mut Browser>, loading: i32, back: i32, forward: i32) {
            report(&self.events, PageEvent::Loading {
                loading: loading == 1,
                back: back == 1,
                forward: forward == 1,
            });
        }
    }
}

wrap_life_span_handler! {
    struct LifeSpan {
        events: PageEvents,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            // The popup keeps its opener, so sign-in windows can report back to the page.
            let tab = format!("browser:{}", uuid::Uuid::new_v4());
            let (Some(window_info), Some(client), Some(container)) =
                (window_info, client, crate::host::platform::browser_container(&tab))
            else {
                return 1;
            };
            // The container sizes the browser's view to itself.
            *window_info = WindowInfo {
                runtime_style: RuntimeStyle::ALLOY,
                ..Default::default()
            }
            .set_as_child(container.as_ptr(), &Rect::default());
            let (events, receiver) = unbounded_channel();
            *client = Some(TabClient::new(UnboundedSender::clone(&events), false));
            let popup = Popup {
                tab,
                url: text(target_url),
                foreground: disposition != WindowOpenDisposition::NEW_BACKGROUND_TAB,
                events,
                receiver,
            };
            report(&self.events, PageEvent::Opened(popup));
            0
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                engine::track(Browser::clone(browser));
                report(&self.events, PageEvent::Created(Browser::clone(browser)));
            }
        }

        /// A page lives in a tab, not its own window: left to CEF, closing it (or a page's
        /// `window.close()`) closes the whole möbius window. Closing the tab removes the page's
        /// view instead, which completes the close. While quitting, CEF's default is wanted.
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            if !engine::running() {
                return 0;
            }
            report(&self.events, PageEvent::Closed);
            1
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                engine::forget(browser);
            }
            report(&self.events, PageEvent::Closed);
        }
    }
}

wrap_focus_handler! {
    struct Focus {
        events: PageEvents,
    }

    impl FocusHandler {
        fn on_got_focus(&self, _browser: Option<&mut Browser>) {
            report(&self.events, PageEvent::Focused);
        }
    }
}

wrap_keyboard_handler! {
    struct Keyboard {
        events: PageEvents,
        desktop: bool,
    }

    impl KeyboardHandler {
        fn on_pre_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: *mut u8,
            _is_keyboard_shortcut: Option<&mut i32>,
        ) -> i32 {
            match event.and_then(Shortcut::of) {
                Some(Shortcut::Paste) if !self.desktop => 0,
                Some(shortcut) => {
                    report(&self.events, PageEvent::Shortcut(shortcut));
                    1
                }
                None => 0,
            }
        }
    }
}

wrap_download_handler! {
    struct Downloads;

    impl DownloadHandler {
        fn can_download(&self, _browser: Option<&mut Browser>, _url: Option<&CefString>, _method: Option<&CefString>) -> i32 {
            1
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            _item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> i32 {
            let (Some(callback), Some(folder)) = (callback, downloads_folder()) else {
                return 0;
            };
            let path = unused_path(&folder, &text(suggested_name));
            callback.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0);
            1
        }
    }
}

fn downloads_folder() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))
}

/// Where a download named `name` goes: Downloads, numbered rather than overwriting.
pub fn download_path(name: &str) -> Option<PathBuf> {
    Some(unused_path(&downloads_folder()?, name))
}

/// `name` in `folder`, numbered like Finder ("report 2.pdf") when that file exists.
fn unused_path(folder: &Path, name: &str) -> PathBuf {
    let name = Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("download");
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    (1..)
        .map(|copy| {
            let stem = if copy == 1 {
                stem.to_owned()
            } else {
                format!("{stem} {copy}")
            };
            match extension {
                Some(extension) => folder.join(format!("{stem}.{extension}")),
                None => folder.join(stem),
            }
        })
        .find(|path| !path.exists())
        .unwrap_or_else(|| folder.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_clients_and_oversized_events_close_the_observer_queue() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let messages = RefCell::new(Some(sender));
        forward_message(&messages, "first");
        forward_message(&messages, "overflow");
        assert!(messages.borrow().is_none());
        assert_eq!(receiver.try_recv().unwrap(), "first");
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let messages = RefCell::new(Some(sender));
        forward_message(&messages, &"x".repeat(devtools::MAX_MESSAGE_BYTES + 1));
        assert!(messages.borrow().is_none());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn downloads_never_overwrite() {
        let folder = tempfile::tempdir().expect("folder");
        let first = unused_path(folder.path(), "report.pdf");
        assert_eq!(first, folder.path().join("report.pdf"));
        std::fs::write(&first, b"").expect("write");
        assert_eq!(
            unused_path(folder.path(), "report.pdf"),
            folder.path().join("report 2.pdf")
        );
        assert_eq!(
            unused_path(folder.path(), "../escape"),
            folder.path().join("escape")
        );
        assert_eq!(
            unused_path(folder.path(), ""),
            folder.path().join("download")
        );
        assert_eq!(
            unused_path(folder.path(), ".env"),
            folder.path().join(".env")
        );
    }
}
