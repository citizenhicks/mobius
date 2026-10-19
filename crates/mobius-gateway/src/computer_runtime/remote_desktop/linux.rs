//! Window-scoped observation and input on the gateway's private X11 display.

use std::cell::Cell;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io::{self, Cursor, IoSlice};
use std::os::fd::AsFd as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use serde::Deserialize;
use serde_json::{Value, json};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::image::{ColorComponent, Image};
use x11rb::protocol::Event;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ChangeWindowAttributesAux,
    ConfigureWindowAux, ConnectionExt as _, EventMask, GrabMode, GrabStatus, InputFocus,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, MapState, Pixmap, StackMode,
    VisualClass, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;

use crate::{Error, Result};

const X11_TIMEOUT: Duration = Duration::from_secs(5);

// DefaultStream polls indefinitely, including inside reply(), check(), and flush().
struct BoundedStream {
    inner: DefaultStream,
    deadline: Mutex<Instant>,
    failed: AtomicBool,
}

impl BoundedStream {
    fn remaining(&self) -> io::Result<Duration> {
        let remaining =
            mobius::sync::recover_lock(&self.deadline).saturating_duration_since(Instant::now());
        if self.failed.load(Ordering::Relaxed) || remaining.is_zero() {
            self.failed.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "X11 desktop timed out",
            ));
        }
        Ok(remaining)
    }
}

impl Stream for BoundedStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut flags = PollFlags::empty();
        if mode.readable() {
            flags |= PollFlags::POLLIN;
        }
        if mode.writable() {
            flags |= PollFlags::POLLOUT;
        }
        let mut fds = [PollFd::new(self.inner.as_fd(), flags)];
        loop {
            let timeout = PollTimeout::try_from(self.remaining()?).map_err(io::Error::other)?;
            match poll(&mut fds, timeout) {
                Ok(0) | Err(Errno::EINTR) => {}
                Ok(_) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn read(&self, bytes: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.remaining()?;
        self.inner.read(bytes, fds)
    }

    fn write(&self, bytes: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.remaining()?;
        self.inner.write(bytes, fds)
    }

    fn write_vectored(
        &self,
        bytes: &[IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.remaining()?;
        self.inner.write_vectored(bytes, fds)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Windows,
    ScreenshotWindow {
        #[serde(rename = "windowId")]
        window: Window,
    },
    Screenshot {
        #[serde(rename = "afterAction", default)]
        after_action: bool,
    },
    Click {
        #[serde(rename = "screenshotId")]
        screenshot: String,
        x: f64,
        y: f64,
        button: Button,
        clicks: u8,
    },
    Move {
        #[serde(rename = "screenshotId")]
        screenshot: String,
        x: f64,
        y: f64,
    },
    Drag {
        #[serde(rename = "screenshotId")]
        screenshot: String,
        x: f64,
        y: f64,
        #[serde(rename = "toX")]
        to_x: f64,
        #[serde(rename = "toY")]
        to_y: f64,
    },
    Scroll {
        #[serde(rename = "screenshotId")]
        screenshot: String,
        x: f64,
        y: f64,
        #[serde(rename = "deltaX")]
        delta_x: f64,
        #[serde(rename = "deltaY")]
        delta_y: f64,
    },
    TypeWindowText {
        #[serde(rename = "windowId")]
        window: Window,
        text: String,
    },
    PressWindowKey {
        #[serde(rename = "windowId")]
        window: Window,
        key: String,
        modifiers: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Button {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

#[derive(Clone, Copy)]
struct Screenshot {
    window: Window,
    geometry: Geometry,
    width: u32,
    height: u32,
    captured: Instant,
}

pub(super) struct Control<'a> {
    pub(super) held: &'a AtomicBool,
    pub(super) cancelled: &'a AtomicBool,
}

impl Control<'_> {
    fn check(&self) -> Result<()> {
        if self.held.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) {
            return Err(failure(
                "desktop execution was cancelled or taken over by the user",
            ));
        }
        Ok(())
    }
}

pub(super) struct Desktop {
    connection: RustConnection<BoundedStream>,
    root: Window,
    clients: Atom,
    title: Atom,
    utf8: Atom,
    pid: Atom,
    known: HashSet<Window>,
    redirected: HashSet<Window>,
    screenshots: HashMap<String, Screenshot>,
    session: String,
    last_action: Option<Window>,
    input_submitted: Cell<bool>,
}

impl Desktop {
    pub(super) fn connect(display: &str, authentication: &[u8]) -> Result<Self> {
        let number = display
            .strip_prefix(':')
            .filter(|value| !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()))
            .ok_or_else(|| failure("invalid private display"))?;
        let auth = std::str::from_utf8(authentication)
            .ok()
            .and_then(|value| value.split_whitespace().last())
            .filter(|value| value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| failure("private display authentication unavailable"))?;
        let cookie = (0..32)
            .step_by(2)
            .map(|offset| {
                let pair = auth
                    .get(offset..offset + 2)
                    .ok_or_else(|| failure("private display authentication unavailable"))?;
                u8::from_str_radix(pair, 16).map_err(xerror)
            })
            .collect::<Result<Vec<_>>>()?;
        let socket = std::os::unix::net::UnixStream::connect(format!("/tmp/.X11-unix/X{number}"))?;
        let (stream, _) = DefaultStream::from_unix_stream(socket).map_err(xerror)?;
        let stream = BoundedStream {
            inner: stream,
            deadline: Mutex::new(Instant::now() + X11_TIMEOUT),
            failed: AtomicBool::new(false),
        };
        let connection = RustConnection::connect_to_stream_with_auth_info(
            stream,
            0,
            b"MIT-MAGIC-COOKIE-1".to_vec(),
            cookie,
        )
        .map_err(xerror)?;
        connection
            .composite_query_version(0, 4)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?;
        connection
            .xtest_get_version(2, 2)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?;
        let root = connection
            .setup()
            .roots
            .first()
            .ok_or_else(|| failure("private display has no screen"))?
            .root;
        let clients = atom(&connection, b"_NET_CLIENT_LIST")?;
        let title = atom(&connection, b"_NET_WM_NAME")?;
        let utf8 = atom(&connection, b"UTF8_STRING")?;
        let pid = atom(&connection, b"_NET_WM_PID")?;
        Ok(Self {
            connection,
            root,
            clients,
            title,
            utf8,
            pid,
            known: HashSet::new(),
            redirected: HashSet::new(),
            screenshots: HashMap::new(),
            session: String::new(),
            last_action: None,
            input_submitted: Cell::new(false),
        })
    }

    pub(super) fn begin(&mut self, session: &str) {
        if self.session != session {
            session.clone_into(&mut self.session);
            self.invalidate();
        }
        self.last_action = None;
    }

    pub(super) fn invalidate(&mut self) {
        self.screenshots.clear();
        self.known.clear();
        self.last_action = None;
    }

    pub(super) fn timed_out(&self) -> bool {
        self.connection.stream().failed.load(Ordering::Relaxed)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn input_submitted(&self) -> bool {
        self.input_submitted.get()
    }

    pub(super) fn request(
        &mut self,
        session: &str,
        begin: bool,
        value: Value,
        control: &Control<'_>,
    ) -> Result<Value> {
        if self.timed_out() {
            return Err(failure("X11 desktop timed out"));
        }
        self.input_submitted.set(false);
        *mobius::sync::recover_lock(&self.connection.stream().deadline) =
            Instant::now() + X11_TIMEOUT;
        if begin {
            self.begin(session);
        }
        let request = serde_json::from_value(value).map_err(|error| {
            self.last_action = None;
            Error::Config(format!(
                "unsupported or invalid Linux desktop action: {error}"
            ))
        })?;
        self.perform(request, control)
    }

    fn perform(&mut self, request: Request, control: &Control<'_>) -> Result<Value> {
        if !matches!(
            request,
            Request::Windows | Request::ScreenshotWindow { .. } | Request::Screenshot { .. }
        ) {
            self.last_action = None;
        }
        control.check()?;
        self.events()?;
        match request {
            Request::Windows => self.windows(),
            Request::ScreenshotWindow { window } => self.capture(window, control),
            Request::Screenshot { after_action: true } => {
                let window = self
                    .last_action
                    .ok_or_else(|| failure("no window action to observe"))?;
                self.capture(window, control)
            }
            Request::Screenshot {
                after_action: false,
            } => Err(failure("Linux capture requires a selected window")),
            Request::TypeWindowText { window, text } => self.type_text(window, &text, control),
            Request::PressWindowKey {
                window,
                key,
                modifiers,
            } => self.press_key(window, &key, &modifiers, control),
            request => self.pointer(request, control),
        }
    }

    fn events(&mut self) -> Result<()> {
        while let Some(event) = self.connection.poll_for_event().map_err(xerror)? {
            let window = match event {
                Event::DestroyNotify(event) => {
                    self.known.remove(&event.window);
                    self.redirected.remove(&event.window);
                    Some(event.window)
                }
                Event::UnmapNotify(event) => Some(event.window),
                Event::ConfigureNotify(event) => {
                    self.screenshots
                        .retain(|_, shot| shot.window != event.window);
                    None
                }
                _ => None,
            };
            if let Some(window) = window {
                self.screenshots.retain(|_, shot| shot.window != window);
                if self.last_action == Some(window) {
                    self.last_action = None;
                }
            }
        }
        Ok(())
    }

    fn windows(&mut self) -> Result<Value> {
        let reply = self
            .connection
            .get_property(false, self.root, self.clients, AtomEnum::WINDOW, 0, 128)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?;
        let mut result = Vec::with_capacity(reply.value.len() / 4);
        self.known.clear();
        for window in reply.value32().into_iter().flatten() {
            let Ok(geometry) = self.geometry(window) else {
                continue;
            };
            self.connection
                .change_window_attributes(
                    window,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
                )
                .map_err(xerror)?
                .check()
                .map_err(xerror)?;
            self.known.insert(window);
            let title = self
                .connection
                .get_property(false, window, self.title, self.utf8, 0, 256)
                .map_err(xerror)?
                .reply()
                .map_err(xerror)?;
            let pid = self
                .connection
                .get_property(false, window, self.pid, AtomEnum::CARDINAL, 0, 1)
                .map_err(xerror)?
                .reply()
                .map_err(xerror)?;
            result.push(
                json!({"windowId":window, "title":String::from_utf8_lossy(&title.value),
                "pid":pid.value32().and_then(|mut values| values.next()),
                "width":geometry.width, "height":geometry.height}),
            );
        }
        Ok(Value::Array(result))
    }

    fn geometry(&self, window: Window) -> Result<Geometry> {
        if window == self.root || window == 0 {
            return Err(failure("choose a window from desktop.windows()"));
        }
        let attributes = self
            .connection
            .get_window_attributes(window)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?;
        if attributes.map_state != MapState::VIEWABLE
            || attributes.class != WindowClass::INPUT_OUTPUT
        {
            return Err(failure("the selected window is not visible"));
        }
        let dimensions = self
            .connection
            .get_geometry(window)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?;
        let position = self
            .connection
            .translate_coordinates(window, self.root, 0, 0)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?;
        if dimensions.width == 0
            || dimensions.height == 0
            || dimensions.width > 8192
            || dimensions.height > 8192
            || u32::from(dimensions.width) * u32::from(dimensions.height) > 20_000_000
        {
            return Err(failure("the selected window exceeds capture limits"));
        }
        Ok(Geometry {
            x: position.dst_x,
            y: position.dst_y,
            width: dimensions.width,
            height: dimensions.height,
        })
    }

    fn selected(&self, window: Window) -> Result<Geometry> {
        if !self.known.contains(&window) {
            return Err(failure("choose a current window from desktop.windows()"));
        }
        self.geometry(window)
    }

    #[expect(
        clippy::set_contains_or_insert,
        reason = "a window is recorded as redirected only after the redirect succeeds"
    )]
    fn capture(&mut self, window: Window, control: &Control<'_>) -> Result<Value> {
        let geometry = self.selected(window)?;
        if !self.redirected.contains(&window) {
            self.connection
                .composite_redirect_window(window, Redirect::AUTOMATIC)
                .map_err(xerror)?
                .check()
                .map_err(xerror)?;
            self.redirected.insert(window);
        }
        let pixmap = self.connection.generate_id().map_err(xerror)?;
        self.connection
            .composite_name_window_pixmap(window, pixmap)
            .map_err(xerror)?
            .check()
            .map_err(xerror)?;
        let pixels = self.pixels(pixmap, window, geometry);
        let released = self
            .connection
            .free_pixmap(pixmap)
            .map_err(xerror)?
            .check()
            .map_err(xerror);
        let pixels = pixels?;
        released?;
        control.check()?;
        if self.selected(window)? != geometry {
            return Err(failure("window changed during capture; capture again"));
        }
        let image = image::DynamicImage::ImageRgb8(pixels);
        let image = if image.width() > 1280 || image.height() > 1280 {
            image.thumbnail(1280, 1280)
        } else {
            image
        };
        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(xerror)?;
        let id = uuid::Uuid::new_v4().to_string();
        let shot = Screenshot {
            window,
            geometry,
            width: image.width(),
            height: image.height(),
            captured: Instant::now(),
        };
        let reply = json!({"screenshotId":id, "windowId":window, "width":shot.width,
            "height":shot.height,
            "coordinates":"Use image pixels from this screenshot; input focuses the selected window.",
            "png":base64::engine::general_purpose::STANDARD.encode(png.into_inner())});
        if self.screenshots.len() >= 8 {
            self.screenshots.clear();
        }
        self.screenshots.insert(id, shot);
        Ok(reply)
    }

    fn pixels(
        &self,
        pixmap: Pixmap,
        window: Window,
        geometry: Geometry,
    ) -> Result<image::RgbImage> {
        let visual = self
            .connection
            .get_window_attributes(window)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?
            .visual;
        let visual = self
            .connection
            .setup()
            .roots
            .first()
            .ok_or_else(|| failure("private display has no screen"))?
            .allowed_depths
            .iter()
            .flat_map(|depth| &depth.visuals)
            .find(|candidate| {
                candidate.visual_id == visual && candidate.class == VisualClass::TRUE_COLOR
            })
            .ok_or_else(|| failure("window requires a true-color visual"))?;
        let components =
            [visual.red_mask, visual.green_mask, visual.blue_mask].map(ColorComponent::from_mask);
        let [red, green, blue] = components;
        let [red, green, blue] = [
            red.map_err(xerror)?,
            green.map_err(xerror)?,
            blue.map_err(xerror)?,
        ];
        let (pixels, _) = Image::get(
            &self.connection,
            pixmap,
            0,
            0,
            geometry.width,
            geometry.height,
        )
        .map_err(xerror)?;
        Ok(image::RgbImage::from_fn(
            u32::from(geometry.width),
            u32::from(geometry.height),
            |x, y| {
                // Both coordinates lie inside the u16 geometry the image was built from.
                let pixel = pixels.get_pixel(
                    u16::try_from(x).unwrap_or(u16::MAX),
                    u16::try_from(y).unwrap_or(u16::MAX),
                );
                let [red, _] = red.decode(pixel).to_be_bytes();
                let [green, _] = green.decode(pixel).to_be_bytes();
                let [blue, _] = blue.decode(pixel).to_be_bytes();
                image::Rgb([red, green, blue])
            },
        ))
    }

    fn observed(&self, id: &str) -> Result<Screenshot> {
        let shot = self
            .screenshots
            .get(id)
            .copied()
            .ok_or_else(|| failure("screenshot is stale; capture the window again"))?;
        if shot.captured.elapsed() >= Duration::from_secs(60)
            || self.selected(shot.window)? != shot.geometry
        {
            return Err(failure(
                "window changed or screenshot expired; capture again",
            ));
        }
        Ok(shot)
    }

    fn focus(&self, window: Window, geometry: Geometry, control: &Control<'_>) -> Result<()> {
        control.check()?;
        if self.selected(window)? != geometry {
            return Err(failure("window changed before input"));
        }
        let mut frame = window;
        for _ in 0..32 {
            let tree = self
                .connection
                .query_tree(frame)
                .map_err(xerror)?
                .reply()
                .map_err(xerror)?;
            if tree.parent == self.root {
                break;
            }
            if tree.parent == 0 {
                return Err(failure("window left the private display"));
            }
            frame = tree.parent;
        }
        self.connection
            .configure_window(
                frame,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )
            .map_err(xerror)?
            .check()
            .map_err(xerror)?;
        self.connection
            .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)
            .map_err(xerror)?
            .check()
            .map_err(xerror)?;
        Ok(())
    }

    fn motion(&self, shot: Screenshot, point: (i16, i16), control: &Control<'_>) -> Result<()> {
        control.check()?;
        if self.selected(shot.window)? != shot.geometry {
            return Err(failure("window changed during input"));
        }
        let (x, y) = self.root_point(shot, point)?;
        if !self.uncovered(shot.window, x, y)? {
            return Err(failure("another window covers this point; capture again"));
        }
        self.input_submitted.set(true);
        self.connection
            .xtest_fake_input(MOTION_NOTIFY_EVENT, 0, CURRENT_TIME, self.root, x, y, 0)
            .map_err(xerror)?
            .check()
            .map_err(xerror)
    }

    /// A screenshot point on the root window, refused when it lies off the visible desktop.
    fn root_point(&self, shot: Screenshot, point: (i16, i16)) -> Result<(i16, i16)> {
        let x = i16::try_from(i32::from(shot.geometry.x) + i32::from(point.0)).map_err(xerror)?;
        let y = i16::try_from(i32::from(shot.geometry.y) + i32::from(point.1)).map_err(xerror)?;
        let screen = self
            .connection
            .setup()
            .roots
            .first()
            .ok_or_else(|| failure("private display has no screen"))?;
        if !u16::try_from(x).is_ok_and(|x| x < screen.width_in_pixels)
            || !u16::try_from(y).is_ok_and(|y| y < screen.height_in_pixels)
        {
            return Err(failure(
                "this point is outside the visible desktop; move the window and capture again",
            ));
        }
        Ok((x, y))
    }

    /// Whether `window` is the topmost window under a root point.
    fn uncovered(&self, window: Window, x: i16, y: i16) -> Result<bool> {
        let mut current = self.root;
        for _ in 0..32 {
            if current == window {
                return Ok(true);
            }
            let child = self
                .connection
                .translate_coordinates(self.root, current, x, y)
                .map_err(xerror)?
                .reply()
                .map_err(xerror)?
                .child;
            if child == 0 {
                break;
            }
            current = child;
        }
        Ok(false)
    }

    /// The window manager applies a raise asynchronously and cannot run while input holds the
    /// server, so wait outside the grab until the point reaches the window. The grabbed hit
    /// test in `motion` stays authoritative.
    fn settle(&self, shot: Screenshot, point: (i16, i16), control: &Control<'_>) -> Result<()> {
        let (x, y) = self.root_point(shot, point)?;
        let deadline = Instant::now() + Duration::from_millis(250);
        while !self.uncovered(shot.window, x, y)? && Instant::now() < deadline {
            control.check()?;
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn input(&self, kind: u8, code: u8) -> Result<()> {
        self.input_submitted.set(true);
        self.connection
            .xtest_fake_input(kind, code, CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(xerror)?
            .check()
            .map_err(xerror)
    }

    fn with_server<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        self.connection
            .grab_server()
            .map_err(xerror)?
            .check()
            .map_err(xerror)?;
        let _release = ReleaseServer(&self.connection);
        action()
    }

    fn button(
        &self,
        shot: Screenshot,
        point: (i16, i16),
        button: u8,
        control: &Control<'_>,
    ) -> Result<()> {
        self.with_server(|| {
            self.pointer_ready(shot.window)?;
            self.motion(shot, point, control)?;
            self.input(BUTTON_PRESS_EVENT, button)?;
            self.input(BUTTON_RELEASE_EVENT, button)
        })
    }

    // Called while the server is grabbed. Release our probe before sending input,
    // so the selected application's connection receives the normal events.
    fn pointer_ready(&self, window: Window) -> Result<()> {
        let mask = self
            .connection
            .query_pointer(self.root)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?
            .mask;
        if u16::from(mask) != 0 {
            return Err(failure(
                "release mouse buttons and keyboard modifiers before input",
            ));
        }
        let status = self
            .connection
            .grab_pointer(
                false,
                window,
                EventMask::NO_EVENT,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                CURRENT_TIME,
            )
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?
            .status;
        if status != GrabStatus::SUCCESS {
            return Err(failure(
                "another application has grabbed the pointer; finish its interaction first",
            ));
        }
        self.connection
            .ungrab_pointer(CURRENT_TIME)
            .map_err(xerror)?
            .check()
            .map_err(xerror)
    }

    fn pointer(&mut self, request: Request, control: &Control<'_>) -> Result<Value> {
        let (id, x, y) = match &request {
            Request::Click {
                screenshot, x, y, ..
            }
            | Request::Move { screenshot, x, y }
            | Request::Drag {
                screenshot, x, y, ..
            }
            | Request::Scroll {
                screenshot, x, y, ..
            } => (screenshot, *x, *y),
            _ => return Err(failure("unsupported Linux desktop action")),
        };
        let shot = self.observed(id)?;
        let point = shot.point(x, y)?;
        let destination = match &request {
            Request::Drag { to_x, to_y, .. } => Some(shot.point(*to_x, *to_y)?),
            _ => None,
        };
        let scroll = match &request {
            Request::Scroll {
                delta_x, delta_y, ..
            } => Some((scroll_steps(*delta_x)?, scroll_steps(*delta_y)?)),
            _ => None,
        };
        if matches!(&request, Request::Click { clicks, .. } if !(1..=2).contains(clicks)) {
            return Err(failure("click count must be one or two"));
        }
        self.focus(shot.window, shot.geometry, control)?;
        self.settle(shot, point, control)?;
        self.screenshots.clear();
        match request {
            Request::Click { button, clicks, .. } => {
                let button = match button {
                    Button::Left => 1,
                    Button::Right => 3,
                };
                for _ in 0..clicks {
                    self.button(shot, point, button, control)?;
                }
            }
            Request::Drag { .. } => self.drag(
                shot,
                point,
                destination.ok_or_else(|| failure("missing drag destination"))?,
                control,
            )?,
            Request::Scroll { .. } => {
                let (horizontal, vertical) =
                    scroll.ok_or_else(|| failure("missing scroll delta"))?;
                for (steps, positive, negative) in [(horizontal, 7, 6), (vertical, 5, 4)] {
                    for _ in 0..steps.unsigned_abs() {
                        let button = if steps > 0 { positive } else { negative };
                        self.button(shot, point, button, control)?;
                    }
                }
            }
            Request::Move { .. } => self.with_server(|| {
                self.pointer_ready(shot.window)?;
                self.motion(shot, point, control)
            })?,
            _ => unreachable!("pointer requests were validated"),
        }
        self.last_action = Some(shot.window);
        Ok(json!(true))
    }

    fn drag(
        &self,
        shot: Screenshot,
        from: (i16, i16),
        to: (i16, i16),
        control: &Control<'_>,
    ) -> Result<()> {
        self.with_server(|| {
            self.pointer_ready(shot.window)?;
            self.motion(shot, from, control)?;
            self.input(BUTTON_PRESS_EVENT, 1)
        })?;
        let result = (|| {
            for step in 1..=10 {
                std::thread::sleep(Duration::from_millis(20));
                let point = (
                    i16::try_from(
                        i32::from(from.0) + (i32::from(to.0) - i32::from(from.0)) * step / 10,
                    )
                    .map_err(xerror)?,
                    i16::try_from(
                        i32::from(from.1) + (i32::from(to.1) - i32::from(from.1)) * step / 10,
                    )
                    .map_err(xerror)?,
                );
                self.with_server(|| self.motion(shot, point, control))?;
            }
            Ok(())
        })();
        let released = self.input(BUTTON_RELEASE_EVENT, 1);
        result.and(released)
    }

    fn keyboard_mapping(&self) -> Result<Keymap> {
        let setup = self.connection.setup();
        let mapping = self
            .connection
            .get_keyboard_mapping(
                setup.min_keycode,
                setup.max_keycode.saturating_sub(setup.min_keycode) + 1,
            )
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?;
        if mapping.keysyms_per_keycode == 0 {
            return Err(failure("keyboard mapping unavailable"));
        }
        let mut keys = HashMap::with_capacity(mapping.keysyms.len());
        for (code, symbols) in (setup.min_keycode..=setup.max_keycode).zip(
            mapping
                .keysyms
                .chunks(usize::from(mapping.keysyms_per_keycode)),
        ) {
            // The first two levels are plain and Shift; the lowest keycode wins, as in Xlib.
            for (symbol, shifted) in symbols.iter().zip([false, true]) {
                if let Entry::Vacant(slot) = keys.entry(*symbol) {
                    slot.insert((code, shifted));
                }
            }
        }
        Ok(Keymap(keys))
    }

    fn type_text(&mut self, window: Window, text: &str, control: &Control<'_>) -> Result<Value> {
        if text.len() > 4096 || text.chars().any(char::is_control) {
            return Err(failure(
                "typeWindowText accepts up to 4096 bytes of plain text",
            ));
        }
        let mapping = self.keyboard_mapping()?;
        let keys = text
            .chars()
            .map(|character| {
                let symbol = u32::from(character);
                mapping.key(if symbol <= 0xff {
                    symbol
                } else {
                    0x0100_0000 | symbol
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let shift = if keys.iter().any(|(_, shifted)| *shifted) {
            Some(mapping.key(SHIFT)?.0)
        } else {
            None
        };
        let geometry = self.selected(window)?;
        self.focus(window, geometry, control)?;
        self.screenshots.clear();
        for (code, shifted) in keys {
            let modifiers = shift.filter(|_| shifted);
            self.key_event(window, geometry, code, modifiers.as_slice(), control)?;
        }
        self.last_action = Some(window);
        Ok(json!(true))
    }

    fn press_key(
        &mut self,
        window: Window,
        key: &str,
        modifiers: &[String],
        control: &Control<'_>,
    ) -> Result<Value> {
        let mapping = self.keyboard_mapping()?;
        let (code, shifted) = mapping.key(keysym(key)?)?;
        if modifiers.len() > 4 {
            return Err(failure("too many key modifiers"));
        }
        let mut codes = modifiers
            .iter()
            .map(|modifier| {
                let symbol = match modifier.as_str() {
                    "shift" => SHIFT,
                    "control" => 0xffe3,
                    "alt" => 0xffe9,
                    "super" => 0xffeb,
                    _ => return Err(failure("modifiers are shift, control, alt or super")),
                };
                Ok(mapping.key(symbol)?.0)
            })
            .collect::<Result<Vec<_>>>()?;
        if shifted {
            codes.push(mapping.key(SHIFT)?.0);
        }
        codes.sort_unstable();
        codes.dedup();
        let geometry = self.selected(window)?;
        self.focus(window, geometry, control)?;
        self.screenshots.clear();
        self.key_event(window, geometry, code, &codes, control)?;
        self.last_action = Some(window);
        Ok(json!(true))
    }

    fn key_event(
        &self,
        window: Window,
        geometry: Geometry,
        code: u8,
        modifiers: &[u8],
        control: &Control<'_>,
    ) -> Result<()> {
        self.with_server(|| self.key_event_locked(window, geometry, code, modifiers, control))
    }

    fn key_event_locked(
        &self,
        window: Window,
        geometry: Geometry,
        code: u8,
        modifiers: &[u8],
        control: &Control<'_>,
    ) -> Result<()> {
        control.check()?;
        if self.selected(window)? != geometry
            || self
                .connection
                .get_input_focus()
                .map_err(xerror)?
                .reply()
                .map_err(xerror)?
                .focus
                != window
        {
            return Err(failure("keyboard focus or window changed; capture again"));
        }
        let mask = self
            .connection
            .query_pointer(self.root)
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?
            .mask;
        if u16::from(mask) != 0 {
            return Err(failure(
                "release mouse buttons and keyboard modifiers, and turn off locked modifiers before typing",
            ));
        }
        let status = self
            .connection
            .grab_keyboard(
                false,
                window,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(xerror)?
            .reply()
            .map_err(xerror)?
            .status;
        if status != GrabStatus::SUCCESS {
            return Err(failure(
                "another application has grabbed the keyboard; finish its interaction first",
            ));
        }
        self.connection
            .ungrab_keyboard(CURRENT_TIME)
            .map_err(xerror)?
            .check()
            .map_err(xerror)?;
        let mut pressed = 0;
        let result = modifiers.iter().chain([&code]).try_for_each(|key| {
            self.input(KEY_PRESS_EVENT, *key)?;
            pressed += 1;
            Ok(())
        });
        let released = [code]
            .iter()
            .chain(modifiers.iter().rev())
            .skip(modifiers.len() + 1 - pressed)
            .map(|key| self.input(KEY_RELEASE_EVENT, *key))
            .fold(Ok(()), Result::and);
        result.and(released)
    }
}

struct ReleaseServer<'a>(&'a RustConnection<BoundedStream>);

impl Drop for ReleaseServer<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.ungrab_server() {
            tracing::debug!(%error, "private X11 server ungrab failed");
        }
        if let Err(error) = self.0.flush() {
            tracing::debug!(%error, "private X11 flush failed");
        }
    }
}

impl Screenshot {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the coordinates are checked to lie inside the u16 window geometry"
    )]
    fn point(self, x: f64, y: f64) -> Result<(i16, i16)> {
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= f64::from(self.width)
            || y >= f64::from(self.height)
        {
            return Err(failure("coordinates must be inside the window screenshot"));
        }
        Ok((
            (x * f64::from(self.geometry.width) / f64::from(self.width)) as i16,
            (y * f64::from(self.geometry.height) / f64::from(self.height)) as i16,
        ))
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the delta is bounded to 10,000, so at most 250 steps"
)]
fn scroll_steps(delta: f64) -> Result<i32> {
    if !delta.is_finite() || delta.abs() > 10_000.0 {
        return Err(failure("scroll delta exceeds its limit"));
    }
    Ok((delta.abs() / 40.0).ceil() as i32 * if delta < 0.0 { -1 } else { 1 })
}

const SHIFT: u32 = 0xffe1;

/// Keysym to its keycode and whether it needs Shift.
struct Keymap(HashMap<u32, (u8, bool)>);

impl Keymap {
    fn key(&self, symbol: u32) -> Result<(u8, bool)> {
        self.0
            .get(&symbol)
            .copied()
            .ok_or_else(|| failure("character is unavailable in this desktop's keyboard layout"))
    }
}

fn keysym(key: &str) -> Result<u32> {
    if let [byte] = key.as_bytes()
        && byte.is_ascii_graphic()
    {
        return Ok(u32::from(*byte));
    }
    if let Some(number) = key
        .strip_prefix('f')
        .and_then(|number| number.parse::<u32>().ok())
        .filter(|number| (1..=12).contains(number))
    {
        return Ok(0xffbd + number);
    }
    match key {
        "return" => Ok(0xff0d),
        "tab" => Ok(0xff09),
        "space" => Ok(0x20),
        "escape" => Ok(0xff1b),
        "delete" => Ok(0xff08),
        "forwarddelete" => Ok(0xffff),
        "left" => Ok(0xff51),
        "up" => Ok(0xff52),
        "right" => Ok(0xff53),
        "down" => Ok(0xff54),
        "home" => Ok(0xff50),
        "end" => Ok(0xff57),
        "pageup" => Ok(0xff55),
        "pagedown" => Ok(0xff56),
        _ => Err(failure("unsupported desktop key")),
    }
}

fn atom(connection: &impl Connection, name: &[u8]) -> Result<Atom> {
    Ok(connection
        .intern_atom(false, name)
        .map_err(xerror)?
        .reply()
        .map_err(xerror)?
        .atom)
}

fn failure(message: &str) -> Error {
    Error::Config(message.to_owned())
}
fn xerror(error: impl std::fmt::Display) -> Error {
    Error::Config(format!("private X11 desktop: {error}"))
}

#[cfg(test)]
mod tests;
