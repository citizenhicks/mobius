//! One gateway-owned browser/profile; Linux adds a private virtual desktop.

#[cfg(any(target_os = "linux", test))]
mod linux;
mod processes;
#[cfg(target_os = "linux")]
mod shell;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use mobius::backend::sandbox::DesktopBrowserPage;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{OwnedMutexGuard, broadcast};
use uuid::Uuid;

use super::desktop::DesktopControl;
use super::{ComputerConfig, config};
use crate::wire::MAX_DESKTOP_CHUNK_BYTES;
use crate::{Error, Result};
#[cfg(target_os = "linux")]
use processes::private_file;
use processes::{OwnedProcess, private_directory};

const MAX_HOST_REQUEST: usize = 1024 * 1024;
const MAX_TABS: usize = 128;

pub(crate) struct RemoteDesktop {
    pub(crate) browser: Arc<super::browser::BrowserHost>,
    enabled: bool,
    state_dir: PathBuf,
    config: Arc<ComputerConfig>,
    runtime: tokio::sync::Mutex<Option<Runtime>>,
    consumers: Mutex<usize>,
    control: Arc<tokio::sync::Mutex<()>>,
    executions: Arc<tokio::sync::RwLock<()>>,
    held: Arc<AtomicBool>,
    user: Mutex<Option<UserControl>>,
    last_session: Mutex<Option<Arc<str>>>,
    changed: broadcast::Sender<()>,
}

/// The controlled chat is `last_session`, which nothing else changes while the user holds control.
struct UserControl {
    connection: Uuid,
    _lease: OwnedMutexGuard<()>,
    _execution: tokio::sync::OwnedRwLockWriteGuard<()>,
}

struct Runtime {
    children: Vec<OwnedProcess>,
    browser: Option<OwnedProcess>,
    chromium: PathBuf,
    /// The profile under gateway state, used when the config names none.
    state_profile: PathBuf,
    endpoint: String,
    websocket: String,
    tabs: BTreeMap<String, String>,
    display: Option<String>,
    authority: PathBuf,
    socket: PathBuf,
    #[cfg(target_os = "linux")]
    native: Option<Arc<Mutex<linux::Desktop>>>,
}

pub(crate) struct DesktopStream {
    remote: Arc<RemoteDesktop>,
    pub(crate) connection: Uuid,
    socket: UnixStream,
    _use: DesktopUse,
}

pub(crate) struct DesktopUse {
    remote: Arc<RemoteDesktop>,
}

pub(super) fn prepare_configured_profile(config: &ComputerConfig) -> Result<()> {
    if let Some(path) = &config.browser.profile_directory {
        private_directory(path)?;
    }
    Ok(())
}

impl RemoteDesktop {
    pub(crate) fn new(
        state_dir: &Path,
        enabled: bool,
        config: impl Into<Arc<ComputerConfig>>,
    ) -> Self {
        let (changed, _) = broadcast::channel(16);
        Self {
            browser: Arc::default(),
            enabled,
            state_dir: state_dir.to_path_buf(),
            config: config.into(),
            runtime: tokio::sync::Mutex::new(None),
            consumers: Mutex::new(0),
            control: Arc::default(),
            executions: Arc::default(),
            held: Arc::new(AtomicBool::new(false)),
            user: Mutex::new(None),
            last_session: Mutex::new(None),
            changed,
        }
    }

    pub(crate) fn configuration(&self) -> &Arc<ComputerConfig> {
        &self.config
    }

    pub(crate) const fn available(&self) -> bool {
        self.enabled && cfg!(target_os = "linux")
    }

    pub(crate) const fn browser_available(&self) -> bool {
        self.enabled
    }

    pub(crate) async fn acquire_use(self: &Arc<Self>) -> DesktopUse {
        let _runtime = self.runtime.lock().await;
        *mobius::sync::recover_lock(&self.consumers) += 1;
        DesktopUse {
            remote: Arc::clone(self),
        }
    }

    fn stop_if_unused(self: &Arc<Self>) {
        let remote = Arc::clone(self);
        tokio::spawn(async move {
            let _execution = remote.executions.write().await;
            let mut runtime = remote.runtime.lock().await;
            if remote.held.load(Ordering::Acquire)
                || *mobius::sync::recover_lock(&remote.consumers) != 0
            {
                return;
            }
            if let Some(runtime) = runtime.take() {
                tracing::info!("gateway desktop stopping: no active consumers");
                runtime.stop().await;
            }
        });
    }

    pub(crate) fn check_execution(&self) -> mobius::Result<()> {
        if self.held.load(Ordering::Acquire) {
            return Err(mobius::Error::Sandbox(
                "execution is held while the user controls the desktop".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed.subscribe()
    }

    pub(crate) async fn execution_lease(
        &self,
    ) -> mobius::Result<tokio::sync::OwnedRwLockReadGuard<()>> {
        self.check_execution()?;
        let lease = Arc::clone(&self.executions).read_owned().await;
        self.check_execution()?;
        Ok(lease)
    }

    pub(crate) async fn execution_cancelled(&self) {
        let mut changed = self.subscribe();
        while self.check_execution().is_ok() {
            if changed
                .recv()
                .await
                .is_err_and(|error| matches!(error, broadcast::error::RecvError::Closed))
            {
                break;
            }
        }
    }

    pub(crate) fn control_state(&self, connection: Uuid) -> (bool, bool, Option<Arc<str>>) {
        let user = mobius::sync::recover_lock(&self.user);
        let (held, owner) = match user.as_ref() {
            Some(user) => (true, user.connection == connection),
            None => (self.held.load(Ordering::Acquire), false),
        };
        let session = mobius::sync::recover_lock(&self.last_session)
            .as_ref()
            .map(Arc::clone);
        drop(user);
        (held, owner, session)
    }

    pub(crate) async fn page(&self, session_id: &str) -> Result<Option<DesktopBrowserPage>> {
        self.check_execution()?;
        if !self.enabled {
            return Ok(None);
        }
        if cfg!(target_os = "macos") {
            return Ok(self.browser.page(session_id, false).await.map(|endpoint| {
                DesktopBrowserPage {
                    endpoint,
                    target_id: None,
                }
            }));
        }
        Ok(Some(self.binding(session_id, false).await?))
    }

    pub(crate) async fn show(&self, session_id: &str) -> Result<()> {
        self.check_execution()?;
        if !self.enabled {
            return Err(unavailable());
        }
        let _lease = self
            .control
            .try_lock()
            .map_err(|busy| Error::Config(format!("the desktop is busy: {busy}")))?;
        self.check_execution()?;
        if cfg!(target_os = "macos") {
            self.binding(session_id, true).await?;
        } else {
            let mut guard = self.runtime.lock().await;
            if let Some(runtime) = guard.as_mut() {
                runtime.show(session_id, &self.config).await?;
            }
            *mobius::sync::recover_lock(&self.last_session) = Some(session_id.into());
            drop(guard);
        }
        self.notify_changed();
        Ok(())
    }

    async fn binding(&self, session_id: &str, activate: bool) -> Result<DesktopBrowserPage> {
        if cfg!(target_os = "macos") {
            let endpoint = self
                .browser
                .page(session_id, activate)
                .await
                .ok_or_else(unavailable)?;
            return Ok(DesktopBrowserPage {
                endpoint,
                target_id: None,
            });
        }
        let mut guard = self.runtime.lock().await;
        if guard.is_none() {
            *guard = Some(Runtime::start(&self.state_dir, &self.config).await?);
        }
        let runtime = guard.as_mut().ok_or_else(unavailable)?;
        for child in &mut runtime.children {
            child.check_running()?;
        }
        runtime.ensure_browser(&self.config).await?;
        let page = runtime.page(session_id, activate, &self.config).await?;
        if activate {
            *mobius::sync::recover_lock(&self.last_session) = Some(session_id.into());
            self.notify_changed();
        }
        drop(guard);
        Ok(page)
    }

    pub(crate) fn connect(
        self: &Arc<Self>,
        session_id: &str,
        native: Arc<DesktopControl>,
    ) -> mobius::Result<DuplexStream> {
        self.check_execution()?;
        let lease = Arc::clone(&self.control)
            .try_lock_owned()
            .map_err(|busy| {
                mobius::Error::Sandbox(format!(
                    "another agent or the user controls the desktop ({busy}); inspect again after it finishes"
                ))
            })?;
        self.check_execution()?;
        let (worker, channel) = tokio::io::duplex(64 * 1024);
        let owner = Arc::clone(self);
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let _lease = lease;
            if let Err(error) = owner.relay(channel, &session_id, native).await {
                tracing::debug!(%error, "desktop host relay ended");
            }
        });
        Ok(worker)
    }

    async fn relay(
        &self,
        mut channel: DuplexStream,
        session_id: &str,
        // Linux gateways act on their own display; only a local Mac app hosts native control.
        #[cfg_attr(
            target_os = "linux",
            expect(unused_variables, reason = "Linux drives its own display instead")
        )]
        native: Arc<DesktopControl>,
    ) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        let mut native_channel = None;
        #[cfg(target_os = "linux")]
        let mut first_native = true;
        loop {
            let size = channel.read_u32().await? as usize;
            if size == 0 || size > MAX_HOST_REQUEST {
                return Err(Error::Protocol("invalid desktop host request".into()));
            }
            let mut bytes = vec![0; size];
            channel.read_exact(&mut bytes).await?;
            let request: Value = serde_json::from_slice(&bytes)?;
            let reply = if request == json!({"op":"begin_browser"}) {
                match self.binding(session_id, !cfg!(target_os = "macos")).await {
                    Ok(page) => serde_json::to_vec(&json!({"result":page}))?,
                    Err(error) => serde_json::to_vec(&json!({"error":error.to_string()}))?,
                }
            } else {
                #[cfg(target_os = "linux")]
                {
                    let cancelled = Arc::new(AtomicBool::new(false));
                    let action = self.linux_request(
                        request,
                        session_id,
                        first_native,
                        Arc::clone(&cancelled),
                    );
                    tokio::pin!(action);
                    let result = tokio::select! {
                        result = &mut action => result,
                        _ = channel.read_u8() => {
                            cancelled.store(true, Ordering::Release);
                            if let Err(error) = action.await {
                                tracing::debug!(%error, "cancelled desktop request failed");
                            }
                            return Ok(());
                        }
                    };
                    match result {
                        Ok(result) => {
                            first_native = false;
                            serde_json::to_vec(&json!({"result":result}))?
                        }
                        Err(error) => serde_json::to_vec(&json!({"error":error.to_string()}))?,
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    match native_channel
                        .take()
                        .map_or_else(|| native.connect(session_id), Ok)
                    {
                        Ok(mut channel) => {
                            // A failed reply after dispatch can hide a completed action; fail closed.
                            let reply = forward_native(&mut channel, &bytes).await?;
                            native_channel = Some(channel);
                            reply
                        }
                        Err(error) => serde_json::to_vec(&json!({"error":error.to_string()}))?,
                    }
                }
            };
            channel
                .write_u32(u32::try_from(reply.len()).map_err(unavailable_after)?)
                .await?;
            channel.write_all(&reply).await?;
            channel.flush().await?;
        }
    }

    #[cfg(target_os = "linux")]
    async fn linux_request(
        &self,
        request: Value,
        session: &str,
        begin: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Value> {
        self.check_execution()?;
        let native = {
            let mut guard = self.runtime.lock().await;
            if guard.is_none() {
                *guard = Some(Runtime::start(&self.state_dir, &self.config).await?);
            }
            let runtime = guard.as_mut().ok_or_else(unavailable)?;
            if runtime.native.is_none() {
                let display = runtime
                    .display
                    .as_deref()
                    .ok_or_else(unavailable)?
                    .to_owned();
                let xauth = config::resolve_executable(Path::new(
                    self.config.desktop.tools.xauth.as_ref(),
                ))?;
                let authentication = short_output(
                    Command::new(xauth)
                        .arg("-f")
                        .arg(&runtime.authority)
                        .args(["list", &display]),
                )
                .await?;
                let desktop = tokio::task::spawn_blocking(move || {
                    linux::Desktop::connect(&display, &authentication.stdout)
                })
                .await
                .map_err(|error| Error::Config(error.to_string()))??;
                runtime.native = Some(Arc::new(Mutex::new(desktop)));
            }
            let native = Arc::clone(runtime.native.as_ref().ok_or_else(unavailable)?);
            drop(guard);
            native
        };
        let session = session.to_owned();
        let held = Arc::clone(&self.held);
        let task_native = Arc::clone(&native);
        let (result, timed_out, input_submitted) =
            tokio::task::spawn_blocking(move || -> Result<_> {
                let mut desktop = task_native.lock().map_err(unavailable_after)?;
                let result = desktop.request(
                    &session,
                    begin,
                    request,
                    &linux::Control {
                        held: &held,
                        cancelled: &cancelled,
                    },
                );
                Ok((result, desktop.timed_out(), desktop.input_submitted()))
            })
            .await
            .map_err(|error| Error::Config(error.to_string()))??;
        if timed_out {
            let mut guard = self.runtime.lock().await;
            if !guard
                .as_ref()
                .and_then(|runtime| runtime.native.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, &native))
            {
                return result;
            }
            if input_submitted {
                // Timed-out input may still be queued, so retire its private display before reuse.
                if let Some(runtime) = guard.take() {
                    tracing::warn!("gateway desktop stopping: X11 input timed out");
                    runtime.stop().await;
                }
            } else if let Some(runtime) = guard.as_mut() {
                runtime.native = None;
            }
            drop(guard);
        }
        result
    }

    pub(crate) async fn stream(self: &Arc<Self>, connection: Uuid) -> Result<DesktopStream> {
        if !self.available() {
            return Err(unavailable());
        }
        let desktop_use = self.acquire_use().await;
        let mut runtime = self.runtime.lock().await;
        if runtime.is_none() {
            *runtime = Some(Runtime::start(&self.state_dir, &self.config).await?);
            // Browser activation awaits without holding the synchronous last-session mutex.
            let session_id = mobius::sync::recover_lock(&self.last_session)
                .as_ref()
                .map(Arc::clone);
            if let Some(session_id) = session_id {
                runtime
                    .as_mut()
                    .ok_or_else(unavailable)?
                    .show(&session_id, &self.config)
                    .await?;
            }
        }
        let runtime = runtime.as_mut().ok_or_else(unavailable)?;
        for child in &mut runtime.children {
            child.check_running()?;
        }
        let socket = UnixStream::connect(&runtime.socket).await?;
        Ok(DesktopStream {
            remote: Arc::clone(self),
            connection,
            socket,
            _use: desktop_use,
        })
    }

    pub(crate) fn begin_takeover(&self) -> Result<()> {
        if !self.available()
            || self
                .held
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(Error::Config(
                "desktop control is unavailable or already held".into(),
            ));
        }
        self.notify_changed();
        Ok(())
    }

    pub(crate) async fn cancel_takeover(self: &Arc<Self>) {
        if mobius::sync::recover_lock(&self.user).is_none() {
            let _execution = self.executions.write().await;
            let has_runtime = self.runtime.lock().await.is_some();
            if has_runtime && self.set_input(false).await.is_err() {
                self.shutdown().await;
            }
            self.held.store(false, Ordering::Release);
            self.notify_changed();
            self.stop_if_unused();
        }
    }

    pub(crate) async fn grant_control(&self, connection: Uuid, session_id: String) -> Result<()> {
        let execution = Arc::clone(&self.executions).write_owned().await;
        let lease = Arc::clone(&self.control).lock_owned().await;
        {
            let mut guard = self.runtime.lock().await;
            let runtime = guard.as_mut().ok_or_else(unavailable)?;
            #[cfg(target_os = "linux")]
            if let Some(native) = &runtime.native {
                native.lock().map_err(unavailable_after)?.invalidate();
            }
            runtime.show(&session_id, &self.config).await?;
            drop(guard);
        }
        *mobius::sync::recover_lock(&self.last_session) = Some(session_id.into());
        if let Err(error) = self.set_input(true).await {
            if self.set_input(false).await.is_err() {
                self.shutdown().await;
            }
            return Err(error);
        }
        *mobius::sync::recover_lock(&self.user) = Some(UserControl {
            connection,
            _lease: lease,
            _execution: execution,
        });
        self.notify_changed();
        Ok(())
    }

    pub(crate) async fn release_control(self: &Arc<Self>, connection: Uuid) -> Result<()> {
        let remote = Arc::clone(self);
        tokio::spawn(async move { remote.release_control_inner(connection).await })
            .await
            .map_err(|error| Error::Config(format!("desktop release failed: {error}")))?
    }

    async fn release_control_inner(self: &Arc<Self>, connection: Uuid) -> Result<()> {
        let user = {
            let mut user = mobius::sync::recover_lock(&self.user);
            if user
                .as_ref()
                .is_none_or(|user| user.connection != connection)
            {
                return Ok(());
            }
            user.take()
        };
        if let Err(error) = self.set_input(false).await {
            self.shutdown().await;
            self.held.store(false, Ordering::Release);
            self.notify_changed();
            return Err(error);
        }
        drop(user);
        self.held.store(false, Ordering::Release);
        self.notify_changed();
        self.stop_if_unused();
        Ok(())
    }

    async fn set_input(&self, enabled: bool) -> Result<()> {
        let guard = self.runtime.lock().await;
        let result = guard
            .as_ref()
            .ok_or_else(unavailable)?
            .set_input(enabled, &self.config)
            .await;
        drop(guard);
        result
    }

    fn notify_changed(&self) {
        if self.changed.send(()).is_err() {
            tracing::trace!("desktop control changed with no subscribers");
        }
    }

    pub(crate) async fn shutdown(&self) {
        let mut runtime = self.runtime.lock().await;
        if let Some(runtime) = runtime.take() {
            tracing::info!("gateway desktop stopping: shutdown");
            runtime.stop().await;
        }
    }
}

impl Drop for DesktopUse {
    fn drop(&mut self) {
        let unused = {
            let mut consumers = mobius::sync::recover_lock(&self.remote.consumers);
            *consumers -= 1;
            *consumers == 0
        };
        if unused {
            self.remote.stop_if_unused();
        }
    }
}

impl DesktopStream {
    pub(crate) async fn read(&mut self) -> Result<Option<Vec<u8>>> {
        let mut bytes = vec![0; MAX_DESKTOP_CHUNK_BYTES];
        let size = self.socket.read(&mut bytes).await?;
        bytes.truncate(size);
        Ok((size != 0).then_some(bytes))
    }

    pub(crate) async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_DESKTOP_CHUNK_BYTES {
            return Err(Error::Protocol("invalid desktop data chunk".into()));
        }
        // ponytail: same-user viewers honor viewOnly; Xvnc globally gates input during agent control.
        tokio::time::timeout(Duration::from_secs(5), self.socket.write_all(bytes))
            .await
            .map_err(|elapsed| {
                Error::Config(format!("desktop viewer stopped accepting data: {elapsed}"))
            })??;
        Ok(())
    }
}

impl Drop for DesktopStream {
    fn drop(&mut self) {
        if !self.remote.control_state(self.connection).1 {
            return;
        }
        let remote = Arc::clone(&self.remote);
        let connection = self.connection;
        tokio::spawn(async move {
            if let Err(error) = remote.release_control(connection).await {
                tracing::warn!(%error, "desktop control release failed");
            }
        });
    }
}

impl Runtime {
    async fn start(state_dir: &Path, config: &Arc<ComputerConfig>) -> Result<Self> {
        let directory = state_dir.join("desktop");
        private_directory(&directory)?;
        let state_profile = directory.join("profile");
        let profile = config
            .browser
            .profile_directory
            .as_deref()
            .unwrap_or(&state_profile);
        private_directory(profile)?;
        for name in ["panel", "status", "browser", "openbox", "xvnc"] {
            processes::clean_record(&directory.join(format!("{name}.json"))).await?;
        }
        let authority = directory.join("Xauthority");
        let socket = directory.join("rfb.sock");
        clean_socket(&socket).await?;
        let mut children = Vec::new();
        let mut browser = None;
        tracing::info!("gateway desktop starting");
        let started: Result<_> = async {
            let display =
                start_display(&directory, &authority, &socket, &mut children, config).await?;
            let runtime = super::prepare_desktop(state_dir, config).await?;
            let chromium = chromium_executable(&runtime, config).await?;
            let (child, endpoint, websocket) =
                start_browser(&chromium, profile, display.as_deref(), &authority, config).await?;
            browser = Some(child);
            #[cfg(target_os = "linux")]
            if let Some(display) = &display {
                shell::start(
                    &directory,
                    display,
                    &authority,
                    &chromium,
                    profile,
                    &mut children,
                    config,
                )
                .await?;
            }
            for child in &mut children {
                child.check_running()?;
            }
            browser.as_mut().ok_or_else(unavailable)?.check_running()?;
            Ok((display, chromium, endpoint, websocket))
        }
        .await;
        let (display, chromium, endpoint, websocket) = match started {
            Ok(started) => started,
            Err(error) => {
                tracing::warn!(%error, "gateway desktop startup failed");
                if let Some(browser) = browser {
                    browser.stop().await;
                }
                stop_children(children, &socket).await;
                return Err(error);
            }
        };
        Ok(Self {
            children,
            browser,
            chromium,
            state_profile,
            endpoint,
            websocket,
            tabs: BTreeMap::new(),
            display,
            authority,
            socket,
            #[cfg(target_os = "linux")]
            native: None,
        })
    }

    async fn stop(mut self) {
        if let Some(browser) = self.browser.take() {
            browser.stop().await;
        }
        stop_children(self.children, &self.socket).await;
    }

    fn profile<'config>(&'config self, config: &'config ComputerConfig) -> &'config Path {
        config
            .browser
            .profile_directory
            .as_deref()
            .unwrap_or(&self.state_profile)
    }

    async fn refresh_browser(&mut self, config: &ComputerConfig) -> Result<bool> {
        if let Some(browser) = self.browser.as_mut()
            && let Some(status) = browser.exit_status()?
        {
            tracing::warn!(
                %status, "gateway desktop browser exited; native desktop remains available"
            );
            self.browser.take().ok_or_else(unavailable)?.stop().await;
        }
        if let Some((endpoint, websocket)) =
            read_devtools(&self.profile(config).join("DevToolsActivePort"))
            && cdp(&websocket, "Browser.getVersion", json!({}))
                .await
                .is_ok()
        {
            if self.websocket != websocket {
                self.tabs.clear();
            }
            self.endpoint = endpoint;
            self.websocket = websocket;
            return Ok(true);
        }
        self.endpoint.clear();
        self.websocket.clear();
        self.tabs.clear();
        Ok(false)
    }

    async fn ensure_browser(&mut self, config: &ComputerConfig) -> Result<()> {
        if self.refresh_browser(config).await? {
            return Ok(());
        }
        if self.browser.is_none() {
            let (browser, endpoint, websocket) = start_browser(
                &self.chromium,
                self.profile(config),
                self.display.as_deref(),
                &self.authority,
                config,
            )
            .await?;
            self.browser = Some(browser);
            self.endpoint = endpoint;
            self.websocket = websocket;
            return Ok(());
        }
        Err(Error::Config(
            "desktop browser is not accepting connections".into(),
        ))
    }

    async fn show(&mut self, session_id: &str, config: &ComputerConfig) -> Result<()> {
        for child in &mut self.children {
            child.check_running()?;
        }
        if self.refresh_browser(config).await? {
            self.page(session_id, true, config).await?;
        }
        Ok(())
    }

    async fn page(
        &mut self,
        session_id: &str,
        activate: bool,
        config: &ComputerConfig,
    ) -> Result<DesktopBrowserPage> {
        let targets = cdp(&self.websocket, "Target.getTargets", json!({})).await?;
        let present = self.tabs.get(session_id).is_some_and(|target| {
            targets
                .get("targetInfos")
                .and_then(Value::as_array)
                .is_some_and(|targets| {
                    targets.iter().any(|entry| {
                        entry.get("targetId").and_then(Value::as_str) == Some(target.as_str())
                    })
                })
        });
        if !present {
            if self.tabs.len() >= MAX_TABS && !self.tabs.contains_key(session_id) {
                return Err(Error::Config("desktop tab limit reached".into()));
            }
            let mut target = cdp(
                &self.websocket,
                "Target.createTarget",
                json!({"url":config.browser.start_page,"background":true}),
            )
            .await?;
            let Some(Value::String(target)) = target.get_mut("targetId").map(Value::take) else {
                return Err(unavailable());
            };
            self.tabs.insert(session_id.to_owned(), target);
        }
        let target_id = self.tabs.get(session_id).ok_or_else(unavailable)?;
        if activate {
            cdp(
                &self.websocket,
                "Target.activateTarget",
                json!({"targetId":target_id}),
            )
            .await?;
        }
        Ok(DesktopBrowserPage {
            endpoint: self.endpoint.clone(), // rules-allow: the external page type owns its endpoint
            target_id: Some(target_id.clone()), // rules-allow: the external page type owns its target
        })
    }

    async fn set_input(&self, enabled: bool, config: &ComputerConfig) -> Result<()> {
        let display = self.display.as_ref().ok_or_else(unavailable)?;
        let executable = config::resolve_candidates(&config.desktop.tools.vncconfig)?;
        let value = if enabled { "1" } else { "0" };
        let parameters = ["AcceptKeyEvents", "AcceptPointerEvents", "AcceptCutText"];
        let mut command = Command::new(&executable);
        command
            .args(["-display", display])
            .env("XAUTHORITY", &self.authority);
        for parameter in parameters {
            command.arg("-set").arg(format!("{parameter}={value}"));
        }
        let output = short_output(&mut command).await?;
        if !output.stderr.is_empty() {
            return Err(Error::Config("desktop input configuration failed".into()));
        }
        for parameter in parameters {
            let output = short_output(
                Command::new(&executable)
                    .args(["-display", display, "-get", parameter])
                    .env("XAUTHORITY", &self.authority),
            )
            .await?;
            if !output.stderr.is_empty() || input_enabled(&output.stdout) != Some(enabled) {
                return Err(Error::Config(
                    "desktop input configuration was not applied".into(),
                ));
            }
        }
        Ok(())
    }
}

fn input_enabled(output: &[u8]) -> Option<bool> {
    match std::str::from_utf8(output).ok()?.trim() {
        "on" | "1" => Some(true),
        "off" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
async fn forward_native(channel: &mut DuplexStream, bytes: &[u8]) -> Result<Vec<u8>> {
    channel
        .write_u32(u32::try_from(bytes.len()).map_err(unavailable_after)?)
        .await?;
    channel.write_all(bytes).await?;
    channel.flush().await?;
    let size = channel.read_u32().await? as usize;
    if size == 0 || size > 64 * 1024 * 1024 {
        return Err(Error::Protocol("invalid native desktop response".into()));
    }
    let mut reply = vec![0; size];
    channel.read_exact(&mut reply).await?;
    Ok(reply)
}

async fn chromium_executable(runtime: &Path, config: &ComputerConfig) -> Result<PathBuf> {
    if let Some(executable) = &config.browser.executable {
        return config::resolve_executable(executable);
    }
    let mut command = Command::new(super::node_executable(runtime, config)?);
    command
        .args([
            "-e",
            "process.stdout.write(require(process.argv[1]).chromium.executablePath())",
        ])
        .arg(super::playwright_module(runtime, config).as_ref())
        .current_dir(runtime)
        .env(
            "PLAYWRIGHT_BROWSERS_PATH",
            super::browsers_directory(runtime, config).as_ref(),
        )
        .kill_on_drop(true);
    let output = tokio::time::timeout(
        Duration::from_secs(config.desktop.startup_timeout_seconds),
        command.output(),
    )
    .await
    .map_err(|elapsed| Error::Config(format!("Chromium discovery timed out: {elapsed}")))??;
    if !output.status.success() || output.stdout.len() > 4096 {
        return Err(Error::Config(
            "installed headed Chromium is unavailable".into(),
        ));
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).map_err(unavailable_after)?);
    config::resolve_executable(&path)
}

async fn start_browser(
    chromium: &Path,
    profile: &Path,
    display: Option<&str>,
    authority: &Path,
    config: &ComputerConfig,
) -> Result<(OwnedProcess, String, String)> {
    config
        .browser
        .validate_root_sandbox(nix::unistd::geteuid().is_root())?;
    let active_port = profile.join("DevToolsActivePort");
    if active_port.exists() {
        fs::remove_file(&active_port)?;
    }
    let mut command = Command::new(chromium);
    command.args(
        config
            .browser
            .headed_arguments(profile)
            .iter()
            .map(std::convert::AsRef::as_ref),
    );
    if let Some(display) = display {
        command.env("DISPLAY", display).env("XAUTHORITY", authority);
    }
    let directory = authority.parent().ok_or_else(unavailable)?;
    let mut browser = OwnedProcess::spawn(&mut command, directory.join("browser.json"))?;
    match wait_devtools(
        &active_port,
        &mut browser,
        config.desktop.startup_timeout_seconds,
    )
    .await
    {
        Ok((endpoint, websocket)) => Ok((browser, endpoint, websocket)),
        Err(error) => {
            browser.stop().await;
            Err(error)
        }
    }
}

fn read_devtools(path: &Path) -> Option<(String, String)> {
    let contents = fs::read_to_string(path).ok()?;
    if contents.len() > 4096 {
        return None;
    }
    let mut lines = contents.lines();
    let port = lines
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)?;
    let path = lines.next().filter(|path| {
        path.starts_with("/devtools/browser/") && !path.chars().any(char::is_whitespace)
    })?;
    Some((
        format!("http://127.0.0.1:{port}"),
        format!("ws://127.0.0.1:{port}{path}"),
    ))
}

async fn wait_devtools(
    path: &Path,
    browser: &mut OwnedProcess,
    timeout_seconds: u64,
) -> Result<(String, String)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        browser.check_running()?;
        if let Some(endpoint) = read_devtools(path) {
            return Ok(endpoint);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Config("headed Chromium did not become ready".into()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop_children(children: Vec<OwnedProcess>, socket: &Path) {
    for child in children.into_iter().rev() {
        child.stop().await;
    }
    if let Err(error) = fs::remove_file(socket) {
        tracing::debug!(%error, "desktop socket was not removed");
    }
}

async fn cdp(endpoint: &str, method: &str, params: Value) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_HOST_REQUEST))
            .max_frame_size(Some(MAX_HOST_REQUEST));
        let (mut socket, _) =
            tokio_tungstenite::connect_async_with_config(endpoint, Some(config), false)
                .await
                .map_err(crate::wire::websocket_error)?;
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"id":1,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(crate::wire::websocket_error)?;
        while let Some(message) = socket.next().await {
            let message = message.map_err(crate::wire::websocket_error)?;
            if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                let mut reply: Value = serde_json::from_str(&text)?;
                if reply.get("id").is_some_and(|id| *id == 1) {
                    return reply
                        .get_mut("result")
                        .map(Value::take)
                        .ok_or_else(unavailable);
                }
            }
        }
        Err(unavailable())
    })
    .await
    .map_err(|elapsed| Error::Config(format!("desktop browser control timed out: {elapsed}")))?
}

#[cfg(target_os = "linux")]
async fn start_display(
    directory: &Path,
    authority: &Path,
    socket: &Path,
    children: &mut Vec<OwnedProcess>,
    config: &ComputerConfig,
) -> Result<Option<String>> {
    let display = (config.desktop.display_start..config.desktop.display_end)
        .find(|display| {
            !Path::new(&format!("/tmp/.X{display}-lock")).exists()
                && !Path::new(&format!("/tmp/.X11-unix/X{display}")).exists()
        })
        .ok_or_else(|| Error::Config("no virtual display is available".into()))?;
    let display = format!(":{display}");
    run_short(
        Command::new(config::resolve_executable(Path::new(
            config.desktop.tools.xauth.as_ref(),
        ))?)
        .arg("-f")
        .arg(authority)
        .args(["add", &display, ".", &Uuid::new_v4().simple().to_string()]),
    )
    .await?;
    private_file(authority)?;
    let mut xvnc = Command::new(config::resolve_candidates(&config.desktop.tools.xvnc)?);
    xvnc.arg(&display)
        .arg("-auth")
        .arg(authority)
        .args([
            "-nolisten",
            "tcp",
            "-rfbport",
            "-1",
            "-rfbunixmode",
            "0600",
            "-SecurityTypes",
            "None",
            "-AlwaysShared",
            "-AcceptKeyEvents=0",
            "-AcceptPointerEvents=0",
            "-AcceptCutText=0",
            "-AcceptSetDesktopSize=0",
            "-AllowOverride",
            "AcceptKeyEvents,AcceptPointerEvents,AcceptCutText",
        ])
        .arg("-geometry")
        .arg(format!(
            "{}x{}",
            config.desktop.resolution[0], config.desktop.resolution[1]
        ))
        .arg("-depth")
        .arg(config.desktop.depth.to_string())
        .arg("-rfbunixpath")
        .arg(socket);
    children.push(OwnedProcess::spawn(&mut xvnc, directory.join("xvnc.json"))?);
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(config.desktop.startup_timeout_seconds);
    while UnixStream::connect(socket).await.is_err() {
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Config("virtual desktop did not become ready".into()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if let Some((executable, arguments)) = config::application(
        &config.desktop.window_manager,
        config::ApplicationRole::WindowManager,
    )? {
        let mut manager = Command::new(executable);
        manager
            .args(arguments)
            .env("DISPLAY", &display)
            .env("XAUTHORITY", authority);
        children.push(OwnedProcess::spawn(
            &mut manager,
            directory.join("openbox.json"),
        )?);
    }
    Ok(Some(display))
}

#[cfg(target_os = "macos")]
#[expect(
    clippy::unused_async,
    reason = "mirrors the Linux start_display, which awaits the display server"
)]
async fn start_display(
    _: &Path,
    _: &Path,
    _: &Path,
    _: &mut Vec<OwnedProcess>,
    _: &ComputerConfig,
) -> Result<Option<String>> {
    Ok(None)
}

async fn clean_socket(socket: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt as _;
    let metadata = match fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() {
        return Err(Error::Config(
            "desktop socket is not an owned stale socket".into(),
        ));
    }
    match UnixStream::connect(socket).await {
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
        _ => {
            return Err(Error::Config(
                "desktop socket is not an owned stale socket".into(),
            ));
        }
    }
    fs::remove_file(socket)?;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn run_short(command: &mut Command) -> Result<()> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(5), command.status())
        .await
        .map_err(|elapsed| Error::Config(format!("desktop setup timed out: {elapsed}")))??;
    if !status.success() {
        return Err(Error::Config("desktop setup command failed".into()));
    }
    Ok(())
}

async fn short_output(command: &mut Command) -> Result<std::process::Output> {
    command
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .map_err(|elapsed| {
            Error::Config(format!("desktop configuration timed out: {elapsed}"))
        })??;
    if !output.status.success() || output.stdout.len() > 4096 || output.stderr.len() > 4096 {
        return Err(Error::Config("desktop configuration failed".into()));
    }
    Ok(output)
}

fn unavailable() -> Error {
    Error::Config("gateway desktop is unavailable".into())
}

fn unavailable_after(error: impl std::fmt::Display) -> Error {
    Error::Config(format!("gateway desktop is unavailable: {error}"))
}

#[cfg(test)]
mod tests;
