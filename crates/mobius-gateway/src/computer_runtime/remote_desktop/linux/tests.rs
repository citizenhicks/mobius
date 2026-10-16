use super::*;
use std::path::Path;
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::Arc;
use x11rb::wrapper::ConnectionExt as _;

fn fixture_desktop(display: &str, authority: &Path) -> Desktop {
    let authentication = Command::new("xauth")
        .arg("-f")
        .arg(authority)
        .args(["list", display])
        .output()
        .unwrap();
    assert!(authentication.status.success());
    Desktop::connect(display, &authentication.stdout).unwrap()
}

#[test]
fn coordinates_and_scroll_are_bounded_in_window_pixels() {
    let shot = Screenshot {
        window: 1,
        geometry: Geometry {
            x: -40,
            y: 20,
            width: 2560,
            height: 1440,
        },
        width: 1280,
        height: 720,
        captured: Instant::now(),
    };
    assert_eq!(shot.point(640.0, 360.0).unwrap(), (1280, 720));
    for (x, y) in [(-1.0, 0.0), (1280.0, 0.0), (0.0, 720.0), (f64::NAN, 0.0)] {
        assert!(shot.point(x, y).is_err());
    }
    assert_eq!(scroll_steps(-1.0).unwrap(), -1);
    assert_eq!(scroll_steps(0.0).unwrap(), 0);
    assert_eq!(scroll_steps(10000.0).unwrap(), 250);
    assert!(scroll_steps(f64::INFINITY).is_err());
    assert!(scroll_steps(10001.0).is_err());
}

static FIXTURE_DISPLAY: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    server: Child,
    #[cfg(target_os = "linux")]
    display: String,
    _directory: tempfile::TempDir,
    desktop: Desktop,
    client: RustConnection<BoundedStream>,
    red: Window,
    blue: Window,
    _allocation: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

impl Fixture {
    fn start() -> Self {
        Self::start_depth(24)
    }

    fn start_depth(depth: u8) -> Self {
        let allocation = FIXTURE_DISPLAY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().unwrap();
        let authority = directory.path().join("Xauthority");
        let number = (100..200)
            .find(|number| {
                !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
                    && !Path::new(&format!("/tmp/.X{number}-lock")).exists()
            })
            .unwrap();
        let display = format!(":{number}");
        assert!(
            Command::new("xauth")
                .arg("-f")
                .arg(&authority)
                .args(["add", &display, ".", "0123456789abcdef0123456789abcdef"])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let server = Command::new("Xvfb")
            .args([
                &display,
                "-screen",
                "0",
                &format!("800x600x{depth}"),
                "-nolisten",
                "tcp",
                "-auth",
            ])
            .arg(&authority)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        let desktop = fixture_desktop(&display, &authority);
        let client = fixture_desktop(&display, &authority).connection;
        let root = client.setup().roots[0].root;
        let red = fixture_window(&client, root, 30, 30, 0xff0000, "Red selected window");
        let blue = fixture_window(&client, root, 50, 50, 0x0000ff, "Blue unrelated window");
        client
            .change_property32(
                PropMode::REPLACE,
                root,
                desktop.clients,
                AtomEnum::WINDOW,
                &[red, blue],
            )
            .unwrap()
            .check()
            .unwrap();
        Self {
            server,
            #[cfg(target_os = "linux")]
            display,
            _directory: directory,
            desktop,
            client,
            red,
            blue,
            _allocation: allocation,
        }
    }

    fn request(&mut self, value: Value) -> Result<Value> {
        let held = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        self.desktop.request(
            "session",
            false,
            value,
            &Control {
                held: &held,
                cancelled: &cancelled,
            },
        )
    }

    fn input_event(&self) -> Event {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(event @ (Event::ButtonPress(_) | Event::KeyPress(_))) =
                self.client.poll_for_event().unwrap()
            {
                return event;
            }
            assert!(Instant::now() < deadline, "fixture input event timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(target_os = "linux")]
async fn fixture_remote(
    fixture: &mut Fixture,
) -> (Arc<super::super::RemoteDesktop>, Arc<Mutex<Desktop>>) {
    use super::super::{RemoteDesktop, Runtime};

    let replacement = fixture_desktop(
        &fixture.display,
        &fixture._directory.path().join("Xauthority"),
    );
    let desktop = std::mem::replace(&mut fixture.desktop, replacement);
    let native = Arc::new(Mutex::new(desktop));
    let remote = Arc::new(RemoteDesktop::new(
        fixture._directory.path(),
        true,
        crate::computer_runtime::ComputerConfig::default(),
    ));
    *remote.runtime.lock().await = Some(Runtime {
        config: Arc::clone(remote.configuration()),
        children: vec![],
        browser: None,
        chromium: Default::default(),
        profile: Default::default(),
        endpoint: String::new(),
        websocket: String::new(),
        tabs: Default::default(),
        display: Some(fixture.display.clone()),
        authority: fixture._directory.path().join("Xauthority"),
        socket: fixture._directory.path().join("rfb.sock"),
        native: Some(Arc::clone(&native)),
    });
    (remote, native)
}

fn fixture_window(
    client: &impl Connection,
    root: Window,
    x: i16,
    y: i16,
    color: u32,
    title: &str,
) -> Window {
    let window = client.generate_id().unwrap();
    client
        .create_window(
            client.setup().roots[0].root_depth,
            window,
            root,
            x,
            y,
            200,
            160,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(color)
                .event_mask(EventMask::BUTTON_PRESS | EventMask::KEY_PRESS),
        )
        .unwrap()
        .check()
        .unwrap();
    client
        .change_property8(
            PropMode::REPLACE,
            window,
            atom(client, b"_NET_WM_NAME").unwrap(),
            atom(client, b"UTF8_STRING").unwrap(),
            title.as_bytes(),
        )
        .unwrap()
        .check()
        .unwrap();
    client.map_window(window).unwrap().check().unwrap();
    window
}

#[test]
#[ignore = "requires a disposable Xvfb display and xauth; never uses the user's desktop"]
fn x11_captures_only_selected_window_and_targets_input() {
    let mut fixture = Fixture::start();
    fixture.desktop.begin("session");
    let windows = fixture.request(json!({"action":"windows"})).unwrap();
    assert_eq!(windows.as_array().unwrap().len(), 2);
    assert!(
        fixture
            .request(json!({"action":"screenshot_window","windowId":fixture.desktop.root}))
            .is_err()
    );
    let shot = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(shot["png"].as_str().unwrap())
        .unwrap();
    let image = image::load_from_memory(&png).unwrap().to_rgb8();
    assert_eq!(
        image.get_pixel(60, 60).0,
        [255, 0, 0],
        "overlapping blue window must not leak into red capture"
    );
    assert_eq!(image.dimensions(), (200, 160));
    fixture.request(json!({"action":"click","screenshotId":shot["screenshotId"],"x":60,"y":60,"button":"left","clicks":1})).unwrap();
    let Event::ButtonPress(clicked) = fixture.input_event() else {
        panic!("expected button press");
    };
    assert_eq!(
        (clicked.event, clicked.event_x, clicked.event_y),
        (fixture.red, 60, 60)
    );
    let after = fixture
        .request(json!({"action":"screenshot","afterAction":true}))
        .unwrap();
    assert_eq!(after["windowId"], fixture.red);
    fixture
        .request(
            json!({"action":"press_window_key","windowId":fixture.red,"key":"a","modifiers":[]}),
        )
        .unwrap();
    let Event::KeyPress(keyed) = fixture.input_event() else {
        panic!("expected key press");
    };
    assert_eq!(keyed.event, fixture.red);
    assert!(fixture.request(json!({"action":"click","screenshotId":shot["screenshotId"],"x":60,"y":60,"button":"left","clicks":1})).is_err());
    fixture.desktop.begin("session");
    assert!(
        fixture
            .request(json!({"action":"screenshot","afterAction":true}))
            .is_err()
    );
    let held = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    assert!(
        fixture
            .desktop
            .request(
                "another-session",
                true,
                json!({"action":"apps"}),
                &Control {
                    held: &held,
                    cancelled: &cancelled
                }
            )
            .is_err()
    );
    assert!(fixture.desktop.known.is_empty() && fixture.desktop.screenshots.is_empty());
}

#[test]
#[ignore = "requires a disposable Xvfb display and xauth; never uses the user's desktop"]
fn x11_rejects_stale_closed_unknown_and_taken_over_windows() {
    let mut fixture = Fixture::start();
    fixture.desktop.begin("session");
    fixture.request(json!({"action":"windows"})).unwrap();
    assert!(
        fixture
            .request(json!({"action":"screenshot_window","windowId":9999}))
            .is_err()
    );
    let shot = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    fixture
        .client
        .configure_window(fixture.red, &ConfigureWindowAux::new().x(40))
        .unwrap()
        .check()
        .unwrap();
    assert!(
        fixture
            .request(json!({"action":"move","screenshotId":shot["screenshotId"],"x":5,"y":5}))
            .is_err()
    );
    fixture
        .client
        .configure_window(fixture.red, &ConfigureWindowAux::new().x(-30))
        .unwrap()
        .check()
        .unwrap();
    let offscreen = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    assert!(fixture.request(json!({"action":"click","screenshotId":offscreen["screenshotId"],"x":5,"y":5,"button":"left","clicks":1})).is_err());
    let caps = fixture
        .desktop
        .keyboard_mapping()
        .unwrap()
        .key(0xffe5)
        .unwrap()
        .0;
    fixture.desktop.input(KEY_PRESS_EVENT, caps).unwrap();
    fixture.desktop.input(KEY_RELEASE_EVENT, caps).unwrap();
    assert!(
        fixture
            .request(json!({"action":"type_window_text","windowId":fixture.red,"text":"abc"}))
            .is_err()
    );
    fixture.desktop.input(KEY_PRESS_EVENT, caps).unwrap();
    fixture.desktop.input(KEY_RELEASE_EVENT, caps).unwrap();
    fixture.desktop.invalidate();
    assert!(
        fixture.desktop.known.is_empty()
            && fixture.desktop.screenshots.is_empty()
            && fixture.desktop.last_action.is_none()
    );
    assert!(
        fixture
            .request(json!({"action":"screenshot_window","windowId":fixture.red}))
            .is_err()
    );
    fixture.request(json!({"action":"windows"})).unwrap();
    let shot = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    fixture
        .desktop
        .screenshots
        .get_mut(shot["screenshotId"].as_str().unwrap())
        .unwrap()
        .captured = Instant::now() - Duration::from_secs(61);
    assert!(
        fixture
            .request(json!({"action":"move","screenshotId":shot["screenshotId"],"x":5,"y":5}))
            .is_err()
    );
    fixture
        .client
        .destroy_window(fixture.red)
        .unwrap()
        .check()
        .unwrap();
    assert!(
        fixture
            .request(json!({"action":"screenshot_window","windowId":fixture.red}))
            .is_err()
    );
    let held = AtomicBool::new(true);
    let cancelled = AtomicBool::new(false);
    assert!(
        fixture
            .desktop
            .perform(
                Request::ScreenshotWindow {
                    window: fixture.blue
                },
                &Control {
                    held: &held,
                    cancelled: &cancelled
                }
            )
            .is_err()
    );
    held.store(false, Ordering::Release);
    cancelled.store(true, Ordering::Release);
    assert!(
        fixture
            .desktop
            .perform(
                Request::ScreenshotWindow {
                    window: fixture.blue
                },
                &Control {
                    held: &held,
                    cancelled: &cancelled
                }
            )
            .is_err()
    );
}
