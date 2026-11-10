//! Chromium's life in this process: it starts the first time a page needs it, runs its work
//! on GPUI's main thread, and shuts down after every browser closed so the profile is flushed.
//! Until then the app carries none of its processes, threads or timers.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use cef::args::Args;
use cef::library_loader::LibraryLoader;
use cef::*;

use super::mac;

/// The helper bundles' executable; Chromium derives the GPU, renderer, plugin and alerts
/// variants from its name. It stays ASCII: codesign mistakes a non-ASCII executable name
/// for a nested resource.
const HELPER: &str = "Mobius Helper.app/Contents/MacOS/Mobius Helper";
const FRAMEWORK: &str = "Chromium Embedded Framework.framework";
/// Cookies, storage and history, relative to the home folder.
const PROFILE: &str = "Library/Application Support/Mobius Desktop/Browser";

static RUNNING: AtomicBool = AtomicBool::new(false);
/// Starting failed once; the browser stays unavailable for this run.
static FAILED: AtomicBool = AtomicBool::new(false);

/// Whether CEF may be called: it started and has not begun shutting down.
pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// Whether a page can be opened: Chromium runs, or ships with this app and has not failed.
pub fn available() -> bool {
    running() || !FAILED.load(Ordering::Relaxed) && frameworks().is_ok()
}

/// Starts Chromium the first time a page needs it. Returns whether it runs; a failure
/// leaves the browser unavailable instead of ending the app.
#[expect(
    clippy::print_stderr,
    reason = "the app installs no log subscriber outside `__serve`; stderr is where a launch failure is seen"
)]
pub fn ensure_started() -> bool {
    if running() {
        return true;
    }
    if FAILED.load(Ordering::Relaxed) {
        return false;
    }
    let failure = match start() {
        Ok(ControlFlow::Continue(())) => return true,
        Ok(ControlFlow::Break(code)) => format!("initialization exited with code {code}"),
        Err(error) => format!("{error:#}"),
    };
    eprintln!("The in-app browser is unavailable: {failure}");
    FAILED.store(true, Ordering::Relaxed);
    false
}

fn frameworks() -> Result<PathBuf> {
    let frameworks = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .map(|contents| contents.join("Frameworks"))
        .context("the app has no bundle")?;
    ensure!(
        frameworks.join(FRAMEWORK).is_dir(),
        "Chromium ships inside the packaged app only"
    );
    Ok(frameworks)
}

/// Starts Chromium, keeping its profile beside the app's preferences. Only a packaged app
/// bundles the framework and helpers; elsewhere this fails and the app runs without them.
/// A break means initialization failed, or CEF forwarded this launch to another instance.
fn start() -> Result<ControlFlow<i32>> {
    let profile = profile()?;
    let executable = std::env::current_exe()?;
    let frameworks = frameworks()?;
    let library = LibraryLoader::new(&executable, false);
    ensure!(library.load(), "Chromium could not be loaded");
    // Chromium stays loaded until the process exits: CEF objects may be released that late.
    let _library = ManuallyDrop::new(library);
    cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    mac::adopt_app_protocol()?;
    let profile = profile.to_str().context("the profile path is not UTF-8")?;
    let helper = frameworks.join(HELPER);
    let settings = Settings {
        browser_subprocess_path: CefString::from(helper.to_str().context("helper path")?),
        external_message_pump: 1,
        // Switches such as --remote-debugging-port on the app's command line are ignored:
        // agents reach a page only through its scoped endpoint.
        command_line_args_disabled: 1,
        root_cache_path: CefString::from(profile),
        cache_path: CefString::from(profile),
        persist_session_cookies: 1,
        log_severity: LogSeverity::ERROR,
        ..Default::default()
    };
    let args = Args::new();
    let mut app = BrowserApp::new();
    let started = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if started != 1 {
        // After failed initialization CEF permits only GetExitCode, then immediate exit.
        return Ok(ControlFlow::Break(initialization_exit_code(
            cef::get_exit_code(),
        )));
    }
    RUNNING.store(true, Ordering::Relaxed);
    Ok(ControlFlow::Continue(()))
}

fn initialization_exit_code(code: i32) -> i32 {
    if u32::try_from(code).ok() == Some(Resultcode::NORMAL_EXIT_PROCESS_NOTIFIED.get_raw()) {
        0
    } else {
        code
    }
}

/// Cookies, storage and history. Snapshot runs, which nobody watches, keep a throwaway
/// profile instead: its keychain prompt would have no one to answer it.
fn profile() -> Result<PathBuf> {
    if cfg!(feature = "snapshot") {
        return Ok(std::env::temp_dir().join("mobius-snapshot-browser"));
    }
    let home = std::env::var_os("HOME").context("no home folder")?;
    Ok(PathBuf::from(home).join(PROFILE))
}

thread_local! {
    /// Browsers not yet closed, so shutdown can close them first.
    static OPEN: RefCell<Vec<Browser>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn track(browser: Browser) {
    OPEN.with_borrow_mut(|open| open.push(browser));
}

pub(super) fn forget(browser: &mut Browser) {
    OPEN.with_borrow_mut(|open| open.retain(|known| known.is_same(Some(browser)) == 0));
}

/// Closes every browser, waits briefly for them to go, and stops Chromium, which flushes
/// cookies and storage to disk. Blocks the main thread; call it once while quitting.
pub fn shutdown() {
    if !RUNNING.swap(false, Ordering::Relaxed) {
        return;
    }
    let hosts: Vec<BrowserHost> =
        OPEN.with_borrow(|open| open.iter().filter_map(Browser::host).collect());
    for host in hosts {
        host.close_browser(1);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while OPEN.with_borrow(|open| !open.is_empty()) && Instant::now() < deadline {
        cef::do_message_loop_work();
        std::thread::sleep(Duration::from_millis(5));
    }
    cef::shutdown();
}

wrap_app! {
    struct BrowserApp;

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(Pump::new())
        }

        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            // Chromium keeps its cookie key in the keychain as "Chromium Safe Storage"; a
            // snapshot run's throwaway profile uses a fixed key rather than prompt for it.
            let browser_process = process_type.is_none_or(|kind| kind.to_string().is_empty());
            if cfg!(feature = "snapshot")
                && browser_process
                && let Some(command_line) = command_line
            {
                command_line.append_switch(Some(&CefString::from("use-mock-keychain")));
            }
        }
    }
}

wrap_browser_process_handler! {
    struct Pump;

    impl BrowserProcessHandler {
        fn on_already_running_app_relaunch(
            &self,
            _command_line: Option<&mut CommandLine>,
            _current_directory: Option<&CefString>,
        ) -> i32 {
            // CEF otherwise opens a standalone Chrome window for this shared profile.
            crate::host::platform::reopen();
            1
        }

        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            schedule(Duration::from_millis(delay_ms.max(0).unsigned_abs()));
        }
    }
}

/// The newest work request; a pending one that is no longer newest does nothing.
static REQUEST: AtomicUsize = AtomicUsize::new(0);
/// Between requests Chromium still expects its work to run about 30 times a second while a
/// page is open; with none open it asks for the work it needs, so a slow fallback is enough.
const IDLE_TICK: Duration = Duration::from_millis(33);
const IDLE_TICK_WITHOUT_PAGES: Duration = Duration::from_secs(1);

thread_local! {
    static WORKING: Cell<bool> = const { Cell::new(false) };
    static REENTERED: Cell<bool> = const { Cell::new(false) };
}

/// Schedules CEF's work on the main thread after `delay`, replacing any pending request.
fn schedule(delay: Duration) {
    let request = REQUEST.fetch_add(1, Ordering::Relaxed) + 1;
    mac::after_on_main(delay, request, work);
}

extern "C" fn work(context: *mut c_void) {
    let request = context as usize;
    if request != REQUEST.load(Ordering::Relaxed) || !running() {
        return;
    }
    // A nested AppKit loop inside CEF's work (a menu, a dialog) drains the main queue too;
    // the outer call reschedules once it returns.
    if WORKING.replace(true) {
        REENTERED.set(true);
        return;
    }
    cef::do_message_loop_work();
    WORKING.set(false);
    if REENTERED.replace(false) {
        schedule(Duration::ZERO);
    } else if request == REQUEST.load(Ordering::Relaxed) {
        let pages = OPEN.with_borrow(|open| !open.is_empty());
        schedule(if pages {
            IDLE_TICK
        } else {
            IDLE_TICK_WITHOUT_PAGES
        });
    }
}
