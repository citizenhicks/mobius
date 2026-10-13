use super::*;
use std::path::Path;
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::Arc;
use x11rb::wrapper::ConnectionExt as _;

#[test]
fn x11_transport_deadline_bounds_poll_and_cannot_be_reset_after_failure() {
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let (inner, _) = DefaultStream::from_unix_stream(socket).unwrap();
    let stream = BoundedStream {
        inner,
        deadline: Mutex::new(Instant::now() + Duration::from_millis(30)),
        failed: AtomicBool::new(false),
    };
    let started = Instant::now();
    assert_eq!(
        stream.poll(PollMode::Readable).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    *mobius::sync::recover_lock(&stream.deadline) = Instant::now() + X11_TIMEOUT;
    assert_eq!(
        stream.poll(PollMode::Writable).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}

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
fn requests_reject_unknown_fields_and_unscoped_capture() {
    for request in [
        json!({"action":"screenshot","displayId":1}),
        json!({"action":"screenshot_window","windowId":1,"pid":2}),
        json!({"action":"screenshot_window","windowId":-1}),
        json!({"action":"type_window_text","windowId":1,"text":null}),
    ] {
        assert!(serde_json::from_value::<Request>(request).is_err());
    }
    assert!(matches!(
        serde_json::from_value(json!({"action":"screenshot","afterAction":true})),
        Ok(Request::Screenshot { after_action: true })
    ));
    // A bare capture parses so it can fail with guidance instead of a schema error.
    assert!(matches!(
        serde_json::from_value(json!({"action":"screenshot"})),
        Ok(Request::Screenshot {
            after_action: false
        })
    ));
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

#[test]
fn keys_use_explicit_names_and_available_layout() {
    assert_eq!(keysym("return").unwrap(), 0xff0d);
    assert_eq!(keysym("f12").unwrap(), 0xffc9);
    assert!(keysym("f13").is_err());
    assert!(keysym("unknown").is_err());
    let keymap = Keymap(HashMap::from([(97, (38, false)), (65, (38, true))]));
    assert_eq!(keymap.key(65).unwrap(), (38, true));
    assert!(keymap.key(0x0100_4e00).is_err());
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

#[test]
#[ignore = "requires a disposable Xvfb display and xauth; never uses the user's desktop"]
fn x11_server_grab_and_paused_server_are_bounded() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    for paused in [false, true] {
        let mut fixture = Fixture::start();
        fixture.request(json!({"action":"windows"})).unwrap();
        if paused {
            kill(
                Pid::from_raw(i32::try_from(fixture.server.id()).unwrap()),
                Signal::SIGSTOP,
            )
            .unwrap();
        } else {
            fixture.client.grab_server().unwrap().check().unwrap();
        }
        let started = Instant::now();
        assert!(fixture.request(json!({"action":"windows"})).is_err());
        assert!(fixture.desktop.timed_out());
        assert!(!fixture.desktop.input_submitted());
        assert!(started.elapsed() < X11_TIMEOUT + Duration::from_secs(1));
        assert!(fixture.request(json!({"action":"windows"})).is_err());
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

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a disposable Xvfb display and xauth; never uses the user's desktop"]
async fn x11_cancelled_read_only_relay_preserves_desktop_and_releases_control() {
    use super::super::DesktopControl;
    use tokio::io::AsyncWriteExt as _;

    let mut fixture = Fixture::start();
    // A previous completed input must not make the next read-only request destructive.
    fixture.desktop.input(MOTION_NOTIFY_EVENT, 0).unwrap();
    let (remote, native) = fixture_remote(&mut fixture).await;
    fixture.client.grab_server().unwrap().check().unwrap();
    let mut worker = remote
        .connect("session", Arc::new(DesktopControl::default()))
        .unwrap();
    let request = br#"{"action":"windows"}"#;
    worker
        .write_u32(u32::try_from(request.len()).unwrap())
        .await
        .unwrap();
    worker.write_all(request).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        native.try_lock().is_err(),
        "X11 action must be blocked before cancellation"
    );
    remote.begin_takeover().unwrap();
    drop(worker);
    let lease = tokio::time::timeout(X11_TIMEOUT + Duration::from_secs(2), remote.control.lock())
        .await
        .expect("cancelled action retained desktop control");
    assert!(native.lock().unwrap().timed_out());
    assert!(!native.lock().unwrap().input_submitted());
    assert!(
        remote
            .runtime
            .lock()
            .await
            .as_ref()
            .unwrap()
            .native
            .is_none()
    );
    drop(lease);
    remote.cancel_takeover().await;
    assert!(!remote.held.load(Ordering::Acquire));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a disposable Xvfb display and xauth; never uses the user's desktop"]
async fn x11_old_timeout_cleanup_preserves_replacement_runtime() {
    let mut fixture = Fixture::start();
    let (remote, native) = fixture_remote(&mut fixture).await;
    let (replacement, replacement_native) = fixture_remote(&mut fixture).await;
    let replacement_runtime = replacement.runtime.lock().await.take().unwrap();
    fixture.client.grab_server().unwrap().check().unwrap();
    let owner = Arc::clone(&remote);
    let request = tokio::spawn(async move {
        owner
            .linux_request(
                json!({"action":"windows"}),
                "session",
                true,
                Arc::new(AtomicBool::new(false)),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while native.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut runtime = remote.runtime.lock().await;
    let original = Arc::clone(&native);
    assert!(
        tokio::task::spawn_blocking(move || original.lock().unwrap().timed_out())
            .await
            .unwrap()
    );
    assert!(
        !request.is_finished(),
        "cleanup must wait for the runtime lock"
    );
    drop(runtime.replace(replacement_runtime));
    drop(runtime);
    assert!(request.await.unwrap().is_err());
    assert!(Arc::ptr_eq(
        remote
            .runtime
            .lock()
            .await
            .as_ref()
            .unwrap()
            .native
            .as_ref()
            .unwrap(),
        &replacement_native,
    ));
    assert!(!replacement_native.lock().unwrap().timed_out());
    remote.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a disposable Xvfb display and xauth; never uses the user's desktop"]
async fn x11_cancelled_drag_retires_desktop_after_uncertain_input() {
    use super::super::DesktopControl;
    use tokio::io::AsyncWriteExt as _;

    let mut fixture = Fixture::start();
    fixture.desktop.begin("session");
    fixture.request(json!({"action":"windows"})).unwrap();
    let shot = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    let (remote, native) = fixture_remote(&mut fixture).await;
    let replacement = fixture_desktop(
        &fixture.display,
        &fixture._directory.path().join("Xauthority"),
    )
    .connection;
    let observer = Arc::new(std::mem::replace(&mut fixture.client, replacement));
    let (grabbed, held) = tokio::sync::oneshot::channel();
    let client = Arc::clone(&observer);
    let manager = std::thread::spawn(move || {
        loop {
            if matches!(client.wait_for_event().unwrap(), Event::ButtonPress(_)) {
                client.grab_server().unwrap().check().unwrap();
                grabbed.send(()).unwrap();
                break;
            }
        }
    });
    let mut worker = remote
        .connect("session", Arc::new(DesktopControl::default()))
        .unwrap();
    let request = serde_json::to_vec(&json!({
        "action":"drag", "screenshotId":shot["screenshotId"],
        "x":5, "y":5, "toX":20, "toY":20
    }))
    .unwrap();
    worker
        .write_u32(u32::try_from(request.len()).unwrap())
        .await
        .unwrap();
    worker.write_all(&request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), held)
        .await
        .unwrap()
        .unwrap();
    manager.join().unwrap();
    remote.begin_takeover().unwrap();
    drop(worker);
    let lease = tokio::time::timeout(X11_TIMEOUT + Duration::from_secs(2), remote.control.lock())
        .await
        .expect("cancelled drag retained desktop control");
    assert!(native.lock().unwrap().timed_out());
    assert!(native.lock().unwrap().input_submitted());
    assert!(remote.runtime.lock().await.is_none());
    drop(lease);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            remote.grant_control(uuid::Uuid::new_v4(), "session".into())
        )
        .await
        .unwrap()
        .is_err()
    );
    remote.cancel_takeover().await;
    assert!(!remote.held.load(Ordering::Acquire));
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
fn x11_input_waits_for_a_window_manager_to_apply_the_raise() {
    let mut fixture = Fixture::start();
    let root = fixture.client.setup().roots[0].root;
    // The fixture client stands in for a window manager that raises windows late.
    fixture
        .client
        .change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_REDIRECT),
        )
        .unwrap()
        .check()
        .unwrap();
    fixture.desktop.begin("session");
    fixture.request(json!({"action":"windows"})).unwrap();
    let shot = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    let held = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    let control = Control {
        held: &held,
        cancelled: &cancelled,
    };
    let client = &fixture.client;
    let (raised, clicked) = std::thread::scope(|scope| {
        let manager = scope.spawn(|| {
            loop {
                if let Event::ConfigureRequest(request) = client.wait_for_event().unwrap() {
                    std::thread::sleep(Duration::from_millis(80));
                    client
                        .configure_window(
                            request.window,
                            &ConfigureWindowAux::from_configure_request(&request),
                        )
                        .unwrap()
                        .check()
                        .unwrap();
                    return request.window;
                }
            }
        });
        // The blue window covers this point until the raise lands.
        let clicked = fixture.desktop.request(
            "session",
            false,
            json!({"action":"click","screenshotId":shot["screenshotId"],"x":60,"y":60,"button":"left","clicks":1}),
            &control,
        );
        (manager.join().unwrap(), clicked)
    });
    assert_eq!(raised, fixture.red);
    clicked.unwrap();
    let Event::ButtonPress(press) = fixture.input_event() else {
        panic!("expected button press");
    };
    assert_eq!(
        (press.event, press.event_x, press.event_y),
        (fixture.red, 60, 60)
    );
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
fn x11_rejects_other_clients_active_input_grabs() {
    let mut fixture = Fixture::start();
    fixture.request(json!({"action":"windows"})).unwrap();
    let shot = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    let pointer = fixture
        .client
        .grab_pointer(
            false,
            fixture.blue,
            EventMask::BUTTON_PRESS,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            CURRENT_TIME,
        )
        .unwrap()
        .reply()
        .unwrap();
    assert_eq!(pointer.status, GrabStatus::SUCCESS);
    let error = fixture.request(json!({"action":"click","screenshotId":shot["screenshotId"],"x":5,"y":5,"button":"left","clicks":1})).unwrap_err();
    assert!(error.to_string().contains("grabbed the pointer"));
    fixture
        .client
        .ungrab_pointer(CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();

    let shot = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    let shift = fixture
        .desktop
        .keyboard_mapping()
        .unwrap()
        .key(SHIFT)
        .unwrap()
        .0;
    fixture.desktop.input(KEY_PRESS_EVENT, shift).unwrap();
    let error = fixture
        .request(json!({"action":"move","screenshotId":shot["screenshotId"],"x":5,"y":5}))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("release mouse buttons and keyboard modifiers")
    );
    fixture.desktop.input(KEY_RELEASE_EVENT, shift).unwrap();

    let keyboard = fixture
        .client
        .grab_keyboard(
            false,
            fixture.blue,
            CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .unwrap()
        .reply()
        .unwrap();
    assert_eq!(keyboard.status, GrabStatus::SUCCESS);
    let error = fixture
        .request(
            json!({"action":"press_window_key","windowId":fixture.red,"key":"a","modifiers":[]}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("grabbed the keyboard"));
    fixture
        .client
        .ungrab_keyboard(CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    assert!(
        fixture
            .request(json!({"action":"screenshot","afterAction":true}))
            .is_err()
    );
}

#[test]
#[ignore = "requires a disposable Xvfb display and xauth; never uses the user's desktop"]
fn x11_decodes_sixteen_bit_capture_colors() {
    let mut fixture = Fixture::start_depth(16);
    fixture.request(json!({"action":"windows"})).unwrap();
    fixture
        .client
        .change_window_attributes(
            fixture.red,
            &ChangeWindowAttributesAux::new().background_pixel(0x4208),
        )
        .unwrap()
        .check()
        .unwrap();
    fixture
        .client
        .clear_area(false, fixture.red, 0, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    let shot = fixture
        .request(json!({"action":"screenshot_window","windowId":fixture.red}))
        .unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(shot["png"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        image::load_from_memory(&png)
            .unwrap()
            .to_rgb8()
            .get_pixel(5, 5)
            .0,
        [66, 65, 66]
    );
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
