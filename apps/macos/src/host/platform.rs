//! macOS capabilities live in the app process; gateway messages keep their original wire format.

use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::numeric::{Saturating, ToFloat};

fn wake_sender() -> &'static tokio::sync::watch::Sender<u64> {
    static WAKE: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> = std::sync::OnceLock::new();
    WAKE.get_or_init(|| tokio::sync::watch::channel(0).0)
}

/// Wakes the UI when native events or deferred file work are queued.
pub fn wake() {
    wake_sender().send_modify(|revision| *revision = revision.wrapping_add(1));
}

extern "C" fn native_wake() {
    wake();
}

/// Set native text defaults before GPUI initializes its glyph rasterizer.
/// Installs the menu bar item that keeps möbius reachable in the background.
pub fn install_status_item(icon: &'static [u8]) {
    // SAFETY: Swift copies the icon bytes before returning.
    unsafe { mobius_native_status_item(icon.as_ptr(), icon.len()) }
}

/// Read-only usage lines at the top of the menu bar item's menu.
pub fn set_status_usage(lines: &[String]) {
    let Ok(json) = serde_json::to_string(lines) else {
        return;
    };
    let Ok(json) = CString::new(json) else {
        return;
    };
    // SAFETY: Swift copies the NUL-terminated string on the main thread before returning.
    unsafe { mobius_native_status_usage(json.as_ptr()) }
}

/// Hides the app and its Dock icon; it keeps running from the menu bar item.
pub fn go_background() {
    // SAFETY: Invoked on the main thread.
    unsafe { mobius_native_background() }
}

/// Leaves menu-bar-only mode: the Dock icon and hidden window return. Main thread only.
pub fn restore() {
    // SAFETY: Invoked from GPUI's reopen handler on the main thread.
    unsafe { mobius_native_restore() }
}

/// Hands this launch to a möbius already running, which reopens its window; the caller then
/// exits. Call before the app starts, so a second copy never starts its own gateway.
pub fn hand_off_launch() -> bool {
    // SAFETY: A plain query plus an asynchronous LaunchServices request; no UI state.
    unsafe { mobius_native_hand_off_launch() }
}

/// Restores the existing app window after Chromium forwards a second launch. Main thread only.
pub fn reopen() {
    // SAFETY: CEF's relaunch callback runs on the same main thread as AppKit and GPUI.
    unsafe { mobius_native_reopen() }
}

/// Schedules macOS's Open With menu outside GPUI's event borrow.
pub fn open_with_menu(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: Swift copies this valid C string before scheduling the menu on the
    // main queue. No pointer is retained and opening errors use NativeEvent::Error.
    unsafe { mobius_native_open_with_menu(path.as_ptr()) };
}

/// The native view a browser tab's page draws into, added hidden the first time. Main
/// thread only, as are its callers: GPUI and Chromium's UI thread are the same thread.
pub fn browser_container(tab_id: &str) -> Option<NonNull<c_void>> {
    let tab_id = CString::new(tab_id).ok()?;
    // SAFETY: Swift copies the identifier; the view lives until the tab's panel closes.
    NonNull::new(unsafe { mobius_native_browser_container(tab_id.as_ptr()) })
}

pub fn configure_text() {
    // SAFETY: Registers a process-local Foundation default without retaining pointers.
    unsafe { mobius_native_configure_text() }
}

/// Obtains Cloud admission from Keychain only for the signed-in account's gateway.
/// Self-hosted endpoints are returned without any Cloud credential.
pub fn gateway_endpoint(
    endpoint: mobius_gateway::client::Endpoint,
) -> mobius_gateway::Result<mobius_gateway::client::Endpoint> {
    let address = CString::new(endpoint.to_string()).map_err(|error| {
        tracing::warn!(%error, "Gateway endpoint contains a NUL byte");
        mobius_gateway::Error::Config("Invalid gateway endpoint.".into())
    })?;
    let mut token = [0u8; 43];
    // SAFETY: Swift reads the valid C string and writes at most token.len() bytes synchronously.
    let status = unsafe {
        mobius_native_cloud_gateway_token(address.as_ptr(), token.as_mut_ptr(), token.len())
    };
    match status {
        0 => Ok(endpoint),
        1 => endpoint.with_websocket_bearer(std::str::from_utf8(&token).map_err(|error| {
            tracing::warn!(%error, "Cloud gateway token from Keychain is not UTF-8");
            mobius_gateway::Error::Config("Invalid Cloud sign-in credential.".into())
        })?),
        _ => Err(mobius_gateway::Error::Config(
            "Sign in to the Cloud gateway's account in Settings → Account before connecting."
                .into(),
        )),
    }
}
use serde_json::Value;

/// The system voice reading a text aloud.
pub type Speech = std::process::Child;

/// Starts reading `text` aloud with the system voice.
pub fn speak(text: &str) -> std::io::Result<Speech> {
    std::process::Command::new("/usr/bin/say").arg(text).spawn()
}

/// Draws `source` as a PNG thumbnail at `destination` and returns its size. A `downloaded`
/// source was fetched only for this and is removed; a failed drawing leaves no file.
pub fn draw_thumbnail(
    source: &std::path::Path,
    destination: &std::path::Path,
    max_pixels: u32,
    downloaded: bool,
) -> Option<(u32, u32)> {
    let size = thumbnail(source, destination, max_pixels);
    if downloaded {
        crate::gateway::local::discard(source);
    }
    if size.is_none()
        && let Err(error) = std::fs::remove_file(destination)
    {
        tracing::debug!(%error, path = %destination.display(), "no thumbnail was drawn to remove");
    }
    size
}

/// Writes a PNG thumbnail of the image or video at `source` to `destination`, at most
/// `max_pixels` on its longer side, and returns its pixel size. Blocking: call it from a
/// background thread. `None` for files ImageIO and AVFoundation cannot draw.
pub fn thumbnail(
    source: &std::path::Path,
    destination: &std::path::Path,
    max_pixels: u32,
) -> Option<(u32, u32)> {
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source.as_os_str().as_bytes()).ok()?;
    let destination = CString::new(destination.as_os_str().as_bytes()).ok()?;
    let mut size = [0u32; 2];
    // SAFETY: Swift reads both strings and writes two integers before returning.
    let made = unsafe {
        mobius_native_thumbnail(
            source.as_ptr(),
            destination.as_ptr(),
            max_pixels,
            size.as_mut_ptr(),
        )
    };
    (made && size[0] > 0 && size[1] > 0).then_some((size[0], size[1]))
}

/// A voice call's audio for the equalizer: the Bot's playback while it speaks, otherwise
/// the microphone, plus how far that level departs from its recent average.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceLevels {
    pub level: f32,
    pub flare: f32,
    pub playback: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeEvent {
    GatewayRequest {
        request: Value,
    },
    Dictation {
        text: String,
        is_final: bool,
    },
    DictationState {
        active: bool,
    },
    Title {
        request_id: String,
        title: String,
        /// Written for a new chat's first turn, so it never replaces a user's rename.
        #[serde(default)]
        automatic: bool,
    },
    /// A chat's "While you were away" recap; none when the model could not write one.
    Recap {
        request_id: String,
        recap: Option<ChatRecap>,
    },
    NotificationOpened {
        session_id: String,
    },
    /// The menu bar item's New Chat.
    StatusNewChat,
    VoiceState {
        active: bool,
        connected: bool,
        muted: bool,
    },
    /// A terminal tab's shell ended; `status` is its exit code, or 128 plus the signal.
    TerminalExited {
        tab_id: String,
        status: Option<i32>,
    },
    /// A native status changed without its own event; the UI renders again.
    StatusChanged,
    CloudStatus {
        status: CloudStatus,
    },
    /// A Cloud gateway pairing grant for the ordinary pairing flow.
    CloudPairing {
        endpoint: String,
        code: Redacted,
    },
    /// Sign-out removed the Cloud bearer; forget the gateway it paired.
    CloudForget {
        endpoint: String,
    },
    Error {
        message: String,
    },
}

/// What happened in a chat since it was last read, written on-device.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct ChatRecap {
    pub headline: String,
    #[serde(default)]
    pub done: Vec<String>,
    pub needs_you: Option<String>,
}

/// A one-time secret that never appears in debug output.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct Redacted(pub String);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The native Cloud account state; Swift owns the bearer and sends this after every change.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CloudStatus {
    pub signed_in: bool,
    pub name: Option<String>,
    pub email: Option<String>,
    pub subscribed: bool,
    pub plan: Option<String>,
    pub subscription: Option<String>,
    /// "Nov 3": when the AI allowance and Live minutes renew; none when the plan ends.
    pub allowance_renews_on: Option<String>,
    /// "Nov 3": when access ends for a plan that does not renew.
    pub access_ends_on: Option<String>,
    /// Some of this billing period's AI allowance is still unspent.
    pub allowance_left: Option<bool>,
    pub age: Option<CloudAge>,
    pub gateway: Option<String>,
    pub gateway_endpoint: Option<String>,
    /// The provider of the saved sign-in, e.g. "apple".
    pub provider: Option<String>,
    pub stage: Option<CloudStage>,
    pub error_kind: Option<CloudErrorKind>,
    pub error: Option<String>,
}

/// Cloud's age decision, or this Mac's attempt to share Apple's age range while none exists.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloudAge {
    Eligible,
    Underage,
    Checking,
    Declined,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloudStage {
    SigningIn,
    Checking,
    Provisioning,
    Connecting,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloudErrorKind {
    Session,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudAction {
    Refresh,
    /// Signs in with `CloudConfig::providers[n]`.
    SignIn(u8),
    Connect,
    SignOut,
    ConfirmAge,
}

impl CloudAction {
    /// The byte `mobius_native_cloud` reads.
    fn code(self) -> u8 {
        match self {
            Self::Refresh => 0,
            Self::Connect => 2,
            Self::SignOut => 3,
            Self::ConfirmAge => 4,
            Self::SignIn(provider) => 16 + provider,
        }
    }
}

/// `~/.mobius/cloud.json` as Swift read it: the parts the UI needs.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CloudConfig {
    pub site: String,
    pub providers: Vec<String>,
    pub age_gate: Option<u8>,
}

/// Read once from Swift, which owns the file and its Info.plist defaults; none when this
/// build has no Cloud.
pub fn cloud_config() -> Option<&'static CloudConfig> {
    static CONFIG: std::sync::OnceLock<Option<CloudConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            // SAFETY: Swift returns a strdup allocation (or null), released by its free function.
            owned_json(unsafe { mobius_native_cloud_config() }).and_then(Result::ok)
        })
        .as_ref()
}

/// Whether Swift's configured Cloud owns this exact gateway endpoint.
pub fn cloud_owns_gateway(endpoint: &str) -> bool {
    let Ok(endpoint) = CString::new(endpoint) else {
        return false;
    };
    // SAFETY: Swift reads the valid C string synchronously without retaining it.
    unsafe { mobius_native_cloud_owns_gateway(endpoint.as_ptr()) }
}

/// Sparkle's update state; Swift owns it and asks for a render after every change.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdateStatus {
    /// Sparkle can update this bundle; `reason` says why not otherwise.
    pub supported: bool,
    pub reason: Option<String>,
    pub stage: UpdateStage,
    pub version: Option<String>,
    pub notes_url: Option<String>,
    /// Download or extraction progress, 0 to 1.
    pub progress: Option<f64>,
    /// The last check found no newer version.
    pub up_to_date: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStage {
    #[default]
    Idle,
    Checking,
    Available,
    Downloading,
    Extracting,
    Installing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateAction {
    Check = 0,
    Install = 1,
}

/// Reads and frees a Swift `strdup` result, or null.
fn owned<T>(pointer: *mut c_char, read: impl FnOnce(&CStr) -> T) -> Option<T> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: A non-null result is a NUL-terminated string owned by the caller.
    let value = read(unsafe { CStr::from_ptr(pointer) });
    // SAFETY: This is the live allocation Swift returned.
    unsafe { mobius_native_free(pointer) };
    Some(value)
}

fn owned_text(pointer: *mut c_char) -> Option<String> {
    owned(pointer, |text| text.to_string_lossy().into_owned())
}

fn owned_json<T: serde::de::DeserializeOwned>(
    pointer: *mut c_char,
) -> Option<serde_json::Result<T>> {
    owned(pointer, |text| serde_json::from_slice(text.to_bytes()))
}

/// `bounds` as `[x, y, width, height]`, checked finite.
fn finite(bounds: [f64; 4], what: &str) -> Result<[f64; 4]> {
    anyhow::ensure!(
        bounds.iter().all(|value| value.is_finite()),
        "Invalid {what} bounds"
    );
    Ok(bounds)
}

/// `0xRRGGBB`, as the Swift side reads colours.
fn rgb(color: gpui::Rgba) -> u32 {
    (color.r * 255.).round().saturating_u32() << 16
        | (color.g * 255.).round().saturating_u32() << 8
        | (color.b * 255.).round().saturating_u32()
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DesktopStatus {
    pub enabled: bool,
    pub accessibility: bool,
    pub screen_recording: bool,
    pub registered: bool,
    pub accessibility_requested: bool,
    pub screen_recording_requested: bool,
    /// macOS 27 calls Accessibility "Device Control and Data Access".
    pub device_control: bool,
}

/// A macOS permission computer control needs (DesktopPermission in Swift).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopPermission {
    Accessibility = 0,
    ScreenRecording = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionStatus {
    NotRequested,
    Denied,
    Restricted,
    Granted,
}

impl PermissionStatus {
    fn from_tag(tag: u8) -> Self {
        match tag & 3 {
            1 => Self::Denied,
            2 => Self::Restricted,
            3 => Self::Granted,
            _ => Self::NotRequested,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NotRequested => "Requested when you start recording",
            Self::Denied => "Allow in System Settings → Privacy & Security",
            Self::Restricted => "Restricted by this Mac’s policy",
            Self::Granted => "Allowed",
        }
    }
}

pub struct AudioStatus {
    pub microphone: PermissionStatus,
    pub speech_recognition: PermissionStatus,
    pub dictation_model: DictationModel,
}

/// The on-device dictation model for the user's language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationModel {
    Unsupported,
    NotDownloaded,
    Downloading,
    Installed,
}

impl DictationModel {
    fn from_tag(tag: u8) -> Self {
        match tag & 3 {
            1 => Self::NotDownloaded,
            2 => Self::Downloading,
            3 => Self::Installed,
            _ => Self::Unsupported,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unsupported => "Unsupported",
            Self::NotDownloaded => "Downloads on first use",
            Self::Downloading => "Downloading",
            Self::Installed => "Available",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoundationStatus {
    Available,
    Disabled,
    Unsupported,
    Downloading,
    Unavailable,
}

impl FoundationStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "Available on this Mac",
            Self::Disabled => "Enable Apple Intelligence in System Settings",
            Self::Unsupported => "Apple Intelligence is unsupported on this Mac",
            Self::Downloading => "Apple's on-device model is downloading",
            Self::Unavailable => "Apple's on-device model is unavailable",
        }
    }
}

/// What a terminal tab runs and its colors, as sRGB components from 0 to 1.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TerminalLaunch {
    pub shell: String,
    pub folder: String,
    pub background: [f32; 4],
    pub foreground: [f32; 4],
    pub caret: [f32; 4],
    pub selection: [f32; 4],
    pub font_set: crate::ui::FontSet,
}

/// Constructed through GPUI's main-thread context; `!Send` keeps AppKit calls there.
pub struct Native {
    _main_thread: PhantomData<Rc<()>>,
}

#[expect(
    clippy::unused_self,
    reason = "`&self` is the main-thread capability: `Native` is `!Send`, so holding it proves AppKit calls stay on the main thread"
)]
impl Native {
    pub fn new(_cx: &gpui::App) -> Self {
        // SAFETY: The process-lifetime callback retains no GPUI pointer; it only wakes a channel.
        unsafe { mobius_native_set_wake(Some(native_wake)) };
        Self {
            _main_thread: PhantomData,
        }
    }

    pub fn wakeups(&self) -> tokio::sync::watch::Receiver<u64> {
        wake_sender().subscribe()
    }

    pub fn set_connected(&self, connected: bool) {
        // SAFETY: Native cannot leave the GPUI main thread.
        unsafe { mobius_native_connected(connected) }
    }

    pub fn receive_gateway(&self, envelope: &(impl Serialize + ?Sized)) -> Result<()> {
        let message = CString::new(serde_json::to_string(envelope)?)?;
        // SAFETY: Swift copies the live, NUL-terminated string before returning.
        unsafe { mobius_native_gateway(message.as_ptr()) }
        Ok(())
    }

    pub fn poll(&self) -> Result<Vec<NativeEvent>> {
        // SAFETY: Swift returns a strdup allocation, released using its paired free function.
        owned_json(unsafe { mobius_native_poll() })
            .context("The macOS event queue could not allocate memory")?
            .context("Read native macOS events")
    }

    pub fn set_desktop_control(&self, enabled: bool) {
        // SAFETY: Invoked on the main thread; turning on prompts for the first missing permission.
        unsafe { mobius_native_desktop(enabled) }
    }

    /// Clears möbius's stale entry for the permission and shows the system prompt again.
    pub fn reset_desktop_permission(&self, permission: DesktopPermission) {
        // SAFETY: Invoked on the main thread in response to the person's click.
        unsafe { mobius_native_desktop_reset(permission as u8) }
    }

    /// The system prompt the first time, System Settings after that.
    pub fn request_desktop_permission(&self, permission: DesktopPermission) {
        // SAFETY: Invoked on the main thread in response to the person's click.
        unsafe { mobius_native_desktop_request(permission as u8) }
    }

    pub fn desktop_status(&self) -> DesktopStatus {
        // SAFETY: Invoked on the main thread and returns only a permission bitset.
        let status = unsafe { mobius_native_desktop_status() };
        DesktopStatus {
            enabled: status & 1 != 0,
            accessibility: status & 2 != 0,
            screen_recording: status & 4 != 0,
            registered: status & 8 != 0,
            accessibility_requested: status & 16 != 0,
            screen_recording_requested: status & 32 != 0,
            device_control: status & 64 != 0,
        }
    }

    /// Why computer control is on but not working yet.
    pub fn desktop_reason(&self) -> Option<String> {
        // SAFETY: Swift returns null or a strdup allocation, released with its paired free.
        owned_text(unsafe { mobius_native_desktop_reason() })
    }

    /// The apps the person has always allowed or refused for Bots.
    pub fn desktop_apps(&self) -> Option<String> {
        // SAFETY: Swift returns null or a strdup allocation, released with its paired free.
        owned_text(unsafe { mobius_native_desktop_apps() })
    }

    pub fn forget_desktop_apps(&self) {
        // SAFETY: Invoked on the main thread in response to the person's click.
        unsafe { mobius_native_desktop_forget_apps() }
    }

    /// Colours the Bot's cursor with the app's accent.
    pub fn set_desktop_accent(&self, accent: gpui::Rgba) {
        // SAFETY: Invoked on the main thread with a plain value.
        unsafe { mobius_native_desktop_accent(rgb(accent)) }
    }

    /// Quits and opens möbius again, for a permission that only a new process sees.
    pub fn relaunch(&self) {
        // SAFETY: Invoked on the main thread; the app terminates afterwards.
        unsafe { mobius_native_relaunch() }
    }

    pub fn reduce_motion(&self) -> bool {
        // SAFETY: Invoked on the main thread and reads a system accessibility preference.
        unsafe { mobius_native_reduce_motion() }
    }

    pub fn foundation_status(&self) -> FoundationStatus {
        // SAFETY: Invoked on the main thread and returns a model availability tag.
        match unsafe { mobius_native_foundation_status() } {
            0 => FoundationStatus::Available,
            1 => FoundationStatus::Disabled,
            2 => FoundationStatus::Unsupported,
            3 => FoundationStatus::Downloading,
            _ => FoundationStatus::Unavailable,
        }
    }

    pub fn dictate(&self, enabled: bool) {
        // SAFETY: Invoked on the main thread; audio callbacks are retained by Swift.
        unsafe { mobius_native_dictation(enabled) }
    }

    /// Stops listening but lets the last words arrive; an inactive state follows.
    pub fn finish_dictation(&self) {
        // SAFETY: Invoked on the main thread; audio callbacks are retained by Swift.
        unsafe { mobius_native_dictation_finish() }
    }

    pub fn audio_status(&self) -> AudioStatus {
        // SAFETY: Reads native permission and recognizer status on the main thread without prompting.
        let status = unsafe { mobius_native_audio_status() };
        AudioStatus {
            microphone: PermissionStatus::from_tag(status),
            speech_recognition: PermissionStatus::from_tag(status >> 2),
            dictation_model: DictationModel::from_tag(status >> 4),
        }
    }

    /// Notification permission as last read; a background refresh reports changes.
    pub fn notification_status(&self) -> PermissionStatus {
        // SAFETY: Invoked on the main thread; returns a cached tag without prompting.
        PermissionStatus::from_tag(unsafe { mobius_native_notification_status() })
    }

    pub fn cloud(&self, action: CloudAction) {
        // SAFETY: Invoked on the main thread; results arrive as native events.
        unsafe { mobius_native_cloud(action.code()) }
    }

    pub fn update_status(&self) -> UpdateStatus {
        // SAFETY: Swift returns a strdup allocation, released using its paired free function.
        owned_json(unsafe { mobius_native_update_status() })
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    pub fn update(&self, action: UpdateAction) {
        // SAFETY: Invoked on the main thread; progress arrives as status changes.
        unsafe { mobius_native_update(action as u8) }
    }

    pub fn start_voice(&self, session_id: &str, wait_for_session_started: bool) -> Result<()> {
        let session = CString::new(session_id)?;
        // SAFETY: Swift copies the session ID before preparing the native audio transport.
        unsafe { mobius_native_voice_start(session.as_ptr(), wait_for_session_started) }
        Ok(())
    }

    pub fn end_voice(&self) {
        // SAFETY: Invoked on the main thread; Swift releases its media resources.
        unsafe { mobius_native_voice_stop() }
    }

    pub fn mute_voice(&self, muted: bool) {
        // SAFETY: Invoked on the main thread.
        unsafe { mobius_native_voice_mute(muted) }
    }

    /// The recording's loudness, one 0 to 1 sample per 100 ms, oldest first, and how
    /// many samples it has made in all.
    pub fn dictation_levels(&self) -> (Vec<f32>, usize) {
        let mut out = [0_f64; 128];
        let mut recorded = 0;
        // SAFETY: Swift writes at most `out.len()` doubles, then the total, and returns how
        // many doubles it wrote.
        let count = unsafe {
            mobius_native_dictation_levels(out.as_mut_ptr(), out.len(), &raw mut recorded)
        };
        let levels = out
            .iter()
            .take(count)
            .map(|level| level.clamp(0., 1.).to_f32())
            .collect();
        (levels, recorded)
    }

    /// The live call's displayed level and flare, 0 to 1, while it has audio.
    pub fn voice_levels(&self) -> VoiceLevels {
        let mut out = [0_f64; 2];
        // SAFETY: Invoked on the main thread; Swift writes exactly two doubles.
        let playback = unsafe { mobius_native_voice_levels(out.as_mut_ptr()) };
        let [level, flare] = out;
        VoiceLevels {
            level: level.clamp(0., 1.).to_f32(),
            flare: flare.clamp(0., 1.).to_f32(),
            playback,
        }
    }

    /// Writes a title from the first message and reply; Swift reports failures only when
    /// the title was asked for (`automatic` is false).
    pub fn generate_title(
        &self,
        request_id: &str,
        message: &str,
        reply: &str,
        automatic: bool,
    ) -> Result<()> {
        let request = CString::new(request_id)?;
        let message = CString::new(message)?;
        let reply = CString::new(reply)?;
        // SAFETY: Swift copies all three strings before creating its asynchronous task.
        unsafe {
            mobius_native_title(
                request.as_ptr(),
                message.as_ptr(),
                reply.as_ptr(),
                automatic,
            );
        }
        Ok(())
    }

    /// Recaps `transcript`, the chat's turns since it was last read; `waiting` names what
    /// the chat waits on, if anything. Swift answers with a `Recap` event either way.
    pub fn generate_recap(&self, request_id: &str, transcript: &str, waiting: &str) -> Result<()> {
        let request = CString::new(request_id)?;
        let transcript = CString::new(transcript)?;
        let waiting = CString::new(waiting)?;
        // SAFETY: Swift copies all three strings before creating its asynchronous task.
        unsafe { mobius_native_recap(request.as_ptr(), transcript.as_ptr(), waiting.as_ptr()) }
        Ok(())
    }

    /// Warms the on-device title model for `chat` (empty for a new chat), once per chat.
    pub fn titles_prewarm(&self, chat: &str) {
        let Ok(chat) = CString::new(chat) else {
            return;
        };
        // SAFETY: Invoked on the main thread; Swift copies the string before returning.
        unsafe { mobius_native_titles_prewarm(chat.as_ptr()) }
    }

    pub fn notify(&self, session_id: &str, title: &str, body: &str) -> Result<()> {
        let session = CString::new(session_id)?;
        let title = CString::new(title)?;
        let body = CString::new(body)?;
        // SAFETY: Swift copies the strings before requesting notification delivery.
        unsafe { mobius_native_notify(session.as_ptr(), title.as_ptr(), body.as_ptr()) }
        Ok(())
    }

    /// Shows the local file at `location` through QuickLook. Bounds are in GPUI window
    /// points from the top-left of its content view.
    pub fn show_panel(&self, tab_id: &str, location: &str, bounds: [f64; 4]) -> Result<()> {
        let tab_id = CString::new(tab_id)?;
        let location = CString::new(location)?;
        let [x, y, width, height] = finite(bounds, "preview")?;
        // SAFETY: Swift copies the string and attaches its view only on the main thread.
        unsafe { mobius_native_panel(tab_id.as_ptr(), location.as_ptr(), x, y, width, height) }
        Ok(())
    }

    /// Shows a terminal tab over `bounds`, starting its shell the first time; closing the
    /// panel ends the shell.
    pub fn show_terminal(
        &self,
        tab_id: &str,
        launch: &TerminalLaunch,
        bounds: [f64; 4],
    ) -> Result<()> {
        let [x, y, width, height] = finite(bounds, "terminal")?;
        let tab_id = CString::new(tab_id)?;
        let launch = CString::new(serde_json::to_string(launch)?)?;
        // SAFETY: Swift copies both strings and attaches its view only on the main thread.
        unsafe { mobius_native_terminal(tab_id.as_ptr(), launch.as_ptr(), x, y, width, height) }
        Ok(())
    }

    /// Shows a browser tab's page over `bounds`, in GPUI window points.
    pub fn show_browser(&self, tab_id: &str, bounds: [f64; 4]) -> Result<()> {
        let [x, y, width, height] = finite(bounds, "browser")?;
        let tab_id = CString::new(tab_id)?;
        // SAFETY: Swift copies the identifier and attaches its view only on the main thread.
        unsafe { mobius_native_browser(tab_id.as_ptr(), x, y, width, height) }
        Ok(())
    }

    /// The computer's ownership label, in native glass above its Chromium view.
    pub fn show_computer_owner(
        &self,
        text: &str,
        owner: bool,
        tint: gpui::Rgba,
        foreground: gpui::Rgba,
        bounds: [f64; 4],
    ) -> Result<()> {
        let [x, y, width, height] = finite(bounds, "computer ownership")?;
        let text = CString::new(text)?;
        // SAFETY: Swift copies the label and updates its owned view on the main thread.
        unsafe {
            mobius_native_computer_owner(
                text.as_ptr(),
                owner,
                rgb(tint),
                rgb(foreground),
                x,
                y,
                width,
                height,
            );
        }
        Ok(())
    }

    /// Hands the keyboard back to GPUI's view from a native one.
    pub fn focus_app(&self) {
        // SAFETY: Invoked on the main thread.
        unsafe { mobius_native_focus_app() }
    }

    /// Hands the keyboard to a shown browser tab's page.
    pub fn focus_browser(&self, tab_id: &str) {
        if let Ok(tab_id) = CString::new(tab_id) {
            // SAFETY: Swift copies the identifier; the page's view stays with its container.
            unsafe { mobius_native_focus_browser(tab_id.as_ptr()) }
        }
    }

    pub fn close_panel(&self, tab_id: &str) {
        if let Ok(tab_id) = CString::new(tab_id) {
            // SAFETY: Swift copies the identifier and releases its view on the main thread.
            unsafe { mobius_native_close_panel(tab_id.as_ptr()) }
        }
    }

    /// Loads `location` into the hidden QuickLook view ahead of `show_panel`.
    pub fn preload_panel(&self, location: &str) -> Result<()> {
        let location = CString::new(location)?;
        // SAFETY: Swift copies the string and touches its view only on the main thread.
        unsafe { mobius_native_preload_panel(location.as_ptr()) }
        Ok(())
    }

    pub fn hide_panel(&self) {
        // SAFETY: Invoked on the main thread.
        unsafe { mobius_native_hide_panel() }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: Native is main-thread-only; close any views retained by the bridge.
        unsafe { mobius_native_reset_panels() }
        self.hide_panel();
        self.end_voice();
        self.dictate(false);
        self.set_connected(false);
    }
}

unsafe extern "C" {
    fn mobius_native_open_with_menu(path: *const c_char);
    fn mobius_native_configure_text();
    fn mobius_native_browser_container(tab_id: *const c_char) -> *mut c_void;
    fn mobius_native_browser(tab_id: *const c_char, x: f64, y: f64, width: f64, height: f64);
    fn mobius_native_focus_browser(tab_id: *const c_char);
    fn mobius_native_focus_app();
    fn mobius_native_thumbnail(
        source: *const c_char,
        destination: *const c_char,
        max_pixels: u32,
        size: *mut u32,
    ) -> bool;
    fn mobius_native_poll() -> *mut c_char;
    fn mobius_native_set_wake(callback: Option<extern "C" fn()>);
    fn mobius_native_free(pointer: *mut c_char);
    fn mobius_native_connected(connected: bool);
    fn mobius_native_gateway(json: *const c_char);
    fn mobius_native_desktop(enabled: bool);
    fn mobius_native_desktop_request(permission: u8);
    fn mobius_native_desktop_reset(permission: u8);
    fn mobius_native_desktop_status() -> u8;
    fn mobius_native_desktop_reason() -> *mut c_char;
    fn mobius_native_desktop_apps() -> *mut c_char;
    fn mobius_native_desktop_forget_apps();
    fn mobius_native_desktop_accent(rgb: u32);
    fn mobius_native_relaunch();
    fn mobius_native_dictation(enabled: bool);
    fn mobius_native_dictation_finish();
    fn mobius_native_status_item(icon: *const u8, length: usize);
    fn mobius_native_status_usage(json: *const c_char);
    fn mobius_native_background();
    fn mobius_native_reopen();
    fn mobius_native_hand_off_launch() -> bool;
    fn mobius_native_restore();
    fn mobius_native_dictation_levels(
        out: *mut f64,
        capacity: usize,
        recorded: *mut usize,
    ) -> usize;
    fn mobius_native_audio_status() -> u8;
    fn mobius_native_notification_status() -> u8;
    fn mobius_native_cloud(action: u8);
    fn mobius_native_cloud_gateway_token(
        endpoint: *const c_char,
        output: *mut u8,
        capacity: usize,
    ) -> i32;
    fn mobius_native_foundation_status() -> u8;
    fn mobius_native_update_status() -> *mut c_char;
    fn mobius_native_cloud_config() -> *mut c_char;
    fn mobius_native_cloud_owns_gateway(endpoint: *const c_char) -> bool;
    fn mobius_native_update(action: u8);
    fn mobius_native_reduce_motion() -> bool;
    fn mobius_native_voice_start(session: *const c_char, wait_for_session_started: bool);
    fn mobius_native_voice_stop();
    fn mobius_native_voice_mute(muted: bool);
    fn mobius_native_voice_levels(out: *mut f64) -> bool;
    fn mobius_native_title(
        request: *const c_char,
        message: *const c_char,
        reply: *const c_char,
        automatic: bool,
    );
    fn mobius_native_titles_prewarm(chat: *const c_char);
    fn mobius_native_recap(
        request: *const c_char,
        transcript: *const c_char,
        waiting: *const c_char,
    );
    fn mobius_native_notify(session: *const c_char, title: *const c_char, body: *const c_char);
    fn mobius_native_preload_panel(location: *const c_char);
    fn mobius_native_panel(
        tab_id: *const c_char,
        location: *const c_char,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    );
    fn mobius_native_terminal(
        tab_id: *const c_char,
        launch: *const c_char,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    );
    fn mobius_native_hide_panel();
    fn mobius_native_computer_owner(
        text: *const c_char,
        owner: bool,
        tint: u32,
        foreground: u32,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    );
    fn mobius_native_close_panel(tab_id: *const c_char);
    fn mobius_native_reset_panels();
}

#[cfg(test)]
mod tests {
    use super::{CloudAge, CloudErrorKind, CloudStage, NativeEvent};

    #[test]
    fn cloud_events_decode_and_keep_the_pairing_code_out_of_debug_output() {
        let event: NativeEvent = serde_json::from_str(
            r#"{"type":"cloud_status","status":{"signed_in":true,"stage":"signing_in","error_kind":"other","age":"declined"}}"#,
        )
        .expect("cloud status");
        let NativeEvent::CloudStatus { status } = event else {
            panic!("expected cloud status");
        };
        assert!(status.signed_in && status.email.is_none());
        assert_eq!(status.stage, Some(CloudStage::SigningIn));
        assert_eq!(status.error_kind, Some(CloudErrorKind::Other));
        assert_eq!(status.age, Some(CloudAge::Declined));
        let pairing: NativeEvent = serde_json::from_str(
            r#"{"type":"cloud_pairing","endpoint":"wss://gw.example","code":"123456"}"#,
        )
        .expect("cloud pairing");
        assert!(!format!("{pairing:?}").contains("123456"));
    }
}
