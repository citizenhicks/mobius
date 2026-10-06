//! One gateway-owned browser/profile; Linux adds a private virtual desktop.

mod processes;
#[cfg(any(target_os = "linux", test))]
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
    held: AtomicBool,
    user: Mutex<Option<UserControl>>,
    last_session: Mutex<Option<String>>,
    changed: broadcast::Sender<()>,
}

struct UserControl {
    connection: Uuid,
    session_id: String,
    _lease: OwnedMutexGuard<()>,
    _execution: tokio::sync::OwnedRwLockWriteGuard<()>,
}

struct Runtime {
    config: Arc<ComputerConfig>,
    children: Vec<OwnedProcess>,
    browser: Option<OwnedProcess>,
    chromium: PathBuf,
    profile: PathBuf,
    endpoint: String,
    websocket: String,
    tabs: BTreeMap<String, String>,
    display: Option<String>,
    authority: PathBuf,
    socket: PathBuf,
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
            held: AtomicBool::new(false),
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

    #[cfg(test)]
    pub(crate) fn consumer_count(&self) -> usize {
        *mobius::sync::recover_lock(&self.consumers)
    }

    fn stop_if_unused(self: &Arc<Self>) {
        let remote = Arc::clone(self);
        tokio::spawn(async move {
            let _execution = Arc::clone(&remote.executions).write_owned().await;
            let mut runtime = remote.runtime.lock().await;
            if remote.held.load(Ordering::Acquire)
                || *mobius::sync::recover_lock(&remote.consumers) != 0
            {
                return;
            }
            if let Some(runtime) = runtime.take() {
                gateway_log!("gateway desktop stopping: no active consumers");
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

    pub(crate) fn control_state(&self, connection: Uuid) -> (bool, bool, Option<String>) {
        let user = mobius::sync::recover_lock(&self.user);
        match user.as_ref() {
            Some(user) => (
                true,
                user.connection == connection,
                Some(user.session_id.clone()),
            ),
            None => (
                self.held.load(Ordering::Acquire),
                false,
                mobius::sync::recover_lock(&self.last_session).clone(),
            ),
        }
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
        let _lease = Arc::clone(&self.control)
            .try_lock_owned()
            .map_err(|_| Error::Config("the desktop is busy".into()))?;
        self.check_execution()?;
        if cfg!(target_os = "macos") {
            self.binding(session_id, true).await?;
        } else {
            let mut runtime = self.runtime.lock().await;
            if let Some(runtime) = runtime.as_mut() {
                runtime.show(session_id).await?;
            }
            *mobius::sync::recover_lock(&self.last_session) = Some(session_id.to_owned());
        }
        let _ = self.changed.send(());
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
        let mut runtime = self.runtime.lock().await;
        if runtime.is_none() {
            *runtime = Some(Runtime::start(&self.state_dir, &self.config).await?);
        }
        let runtime = runtime.as_mut().ok_or_else(unavailable)?;
        for child in &mut runtime.children {
            child.check_running()?;
        }
        runtime.ensure_browser().await?;
        let page = runtime.page(session_id, activate).await?;
        if activate {
            *mobius::sync::recover_lock(&self.last_session) = Some(session_id.to_owned());
            let _ = self.changed.send(());
        }
        Ok(page)
    }

    pub(crate) fn connect(
        self: &Arc<Self>,
        session_id: &str,
        native: Arc<DesktopControl>,
    ) -> mobius::Result<DuplexStream> {
        self.check_execution()?;
        let lease = Arc::clone(&self.control).try_lock_owned().map_err(|_| {
            mobius::Error::Sandbox(
                "another agent or the user controls the desktop; inspect again after it finishes"
                    .into(),
            )
        })?;
        self.check_execution()?;
        let (worker, channel) = tokio::io::duplex(64 * 1024);
        let owner = Arc::clone(self);
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let _lease = lease;
            let _ = owner.relay(channel, &session_id, native).await;
        });
        Ok(worker)
    }

    async fn relay(
        &self,
        mut channel: DuplexStream,
        session_id: &str,
        native: Arc<DesktopControl>,
    ) -> Result<()> {
        let mut native_channel = None;
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
            };
            channel
                .write_u32(u32::try_from(reply.len()).map_err(|_| unavailable())?)
                .await?;
            channel.write_all(&reply).await?;
            channel.flush().await?;
        }
    }

    pub(crate) async fn stream(self: &Arc<Self>, connection: Uuid) -> Result<DesktopStream> {
        if !self.available() {
            return Err(unavailable());
        }
        let desktop_use = self.acquire_use().await;
        let mut runtime = self.runtime.lock().await;
        if runtime.is_none() {
            *runtime = Some(Runtime::start(&self.state_dir, &self.config).await?);
            let session_id = mobius::sync::recover_lock(&self.last_session).clone();
            if let Some(session_id) = session_id {
                runtime
                    .as_mut()
                    .ok_or_else(unavailable)?
                    .show(&session_id)
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
        let _ = self.changed.send(());
        Ok(())
    }

    pub(crate) async fn cancel_takeover(self: &Arc<Self>) {
        if mobius::sync::recover_lock(&self.user).is_none() {
            let _execution = Arc::clone(&self.executions).write_owned().await;
            let has_runtime = self.runtime.lock().await.is_some();
            if has_runtime && self.set_input(false).await.is_err() {
                self.shutdown().await;
            }
            self.held.store(false, Ordering::Release);
            let _ = self.changed.send(());
            self.stop_if_unused();
        }
    }

    pub(crate) async fn grant_control(&self, connection: Uuid, session_id: String) -> Result<()> {
        let execution = Arc::clone(&self.executions).write_owned().await;
        let lease = Arc::clone(&self.control).lock_owned().await;
        {
            let mut runtime = self.runtime.lock().await;
            runtime
                .as_mut()
                .ok_or_else(unavailable)?
                .show(&session_id)
                .await?;
        }
        *mobius::sync::recover_lock(&self.last_session) = Some(session_id.clone());
        if let Err(error) = self.set_input(true).await {
            if self.set_input(false).await.is_err() {
                self.shutdown().await;
            }
            return Err(error);
        }
        *mobius::sync::recover_lock(&self.user) = Some(UserControl {
            connection,
            session_id,
            _lease: lease,
            _execution: execution,
        });
        let _ = self.changed.send(());
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
            let _ = self.changed.send(());
            return Err(error);
        }
        drop(user);
        self.held.store(false, Ordering::Release);
        let _ = self.changed.send(());
        self.stop_if_unused();
        Ok(())
    }

    async fn set_input(&self, enabled: bool) -> Result<()> {
        let runtime = self.runtime.lock().await;
        let runtime = runtime.as_ref().ok_or_else(unavailable)?;
        runtime.set_input(enabled).await
    }

    pub(crate) async fn shutdown(&self) {
        let mut runtime = self.runtime.lock().await;
        if let Some(runtime) = runtime.take() {
            gateway_log!("gateway desktop stopping: shutdown");
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
            .map_err(|_| Error::Config("desktop viewer stopped accepting data".into()))??;
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
            let _ = remote.release_control(connection).await;
        });
    }
}

impl Runtime {
    async fn start(state_dir: &Path, config: &Arc<ComputerConfig>) -> Result<Self> {
        let directory = state_dir.join("desktop");
        private_directory(&directory)?;
        let profile = config
            .browser
            .profile_directory
            .clone()
            .unwrap_or_else(|| directory.join("profile"));
        private_directory(&profile)?;
        for name in ["panel", "status", "browser", "openbox", "xvnc"] {
            processes::clean_record(&directory.join(format!("{name}.json"))).await?;
        }
        let authority = directory.join("Xauthority");
        let socket = directory.join("rfb.sock");
        clean_socket(&socket).await?;
        let mut children = Vec::new();
        let mut browser = None;
        gateway_log!("gateway desktop starting");
        let started: Result<_> = async {
            let display =
                start_display(&directory, &authority, &socket, &mut children, config).await?;
            let runtime = super::prepare_desktop(state_dir, config).await?;
            let chromium = chromium_executable(&runtime, config).await?;
            let (child, endpoint, websocket) =
                start_browser(&chromium, &profile, display.as_deref(), &authority, config).await?;
            browser = Some(child);
            #[cfg(target_os = "linux")]
            if let Some(display) = &display {
                shell::start(
                    &directory,
                    display,
                    &authority,
                    &chromium,
                    &profile,
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
                gateway_log!("gateway desktop startup failed: {error}");
                if let Some(browser) = browser {
                    browser.stop().await;
                }
                stop_children(children, &socket).await;
                return Err(error);
            }
        };
        Ok(Self {
            config: Arc::clone(config),
            children,
            browser,
            chromium,
            profile,
            endpoint,
            websocket,
            tabs: BTreeMap::new(),
            display,
            authority,
            socket,
        })
    }

    async fn stop(mut self) {
        if let Some(browser) = self.browser.take() {
            browser.stop().await;
        }
        stop_children(self.children, &self.socket).await;
    }

    async fn refresh_browser(&mut self) -> Result<bool> {
        if let Some(browser) = self.browser.as_mut()
            && let Some(status) = browser.exit_status()?
        {
            gateway_log!(
                "gateway desktop browser exited ({status}); native desktop remains available"
            );
            self.browser.take().ok_or_else(unavailable)?.stop().await;
        }
        if let Some((endpoint, websocket)) = read_devtools(&self.profile.join("DevToolsActivePort"))
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

    async fn ensure_browser(&mut self) -> Result<()> {
        if self.refresh_browser().await? {
            return Ok(());
        }
        if self.browser.is_none() {
            let (browser, endpoint, websocket) = start_browser(
                &self.chromium,
                &self.profile,
                self.display.as_deref(),
                &self.authority,
                &self.config,
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

    async fn show(&mut self, session_id: &str) -> Result<()> {
        for child in &mut self.children {
            child.check_running()?;
        }
        if self.refresh_browser().await? {
            self.page(session_id, true).await?;
        }
        Ok(())
    }

    async fn page(&mut self, session_id: &str, activate: bool) -> Result<DesktopBrowserPage> {
        let targets = cdp(&self.websocket, "Target.getTargets", json!({})).await?;
        let present = self
            .tabs
            .get(session_id)
            .filter(|target| {
                targets["targetInfos"].as_array().is_some_and(|targets| {
                    targets
                        .iter()
                        .any(|entry| entry["targetId"].as_str() == Some(target.as_str()))
                })
            })
            .cloned();
        let target_id = match present {
            Some(target) => target,
            None => {
                if self.tabs.len() >= MAX_TABS && !self.tabs.contains_key(session_id) {
                    return Err(Error::Config("desktop tab limit reached".into()));
                }
                let target = cdp(
                    &self.websocket,
                    "Target.createTarget",
                    json!({"url":self.config.browser.start_page,"background":true}),
                )
                .await?;
                let target = target["targetId"]
                    .as_str()
                    .ok_or_else(unavailable)?
                    .to_owned();
                self.tabs.insert(session_id.to_owned(), target.clone());
                target
            }
        };
        if activate {
            cdp(
                &self.websocket,
                "Target.activateTarget",
                json!({"targetId":target_id}),
            )
            .await?;
        }
        Ok(DesktopBrowserPage {
            endpoint: self.endpoint.clone(),
            target_id: Some(target_id),
        })
    }

    async fn set_input(&self, enabled: bool) -> Result<()> {
        let display = self.display.as_ref().ok_or_else(unavailable)?;
        let executable = config::resolve_candidates(&self.config.desktop.tools.vncconfig)?;
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

async fn forward_native(channel: &mut DuplexStream, bytes: &[u8]) -> Result<Vec<u8>> {
    channel
        .write_u32(u32::try_from(bytes.len()).map_err(|_| unavailable())?)
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
    .map_err(|_| Error::Config("Chromium discovery timed out".into()))??;
    if !output.status.success() || output.stdout.len() > 4096 {
        return Err(Error::Config(
            "installed headed Chromium is unavailable".into(),
        ));
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).map_err(|_| unavailable())?);
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
            .map(|argument| argument.as_ref()),
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
    let _ = fs::remove_file(socket);
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
                let reply: Value = serde_json::from_str(&text)?;
                if reply["id"] == 1 {
                    return reply.get("result").cloned().ok_or_else(unavailable);
                }
            }
        }
        Err(unavailable())
    })
    .await
    .map_err(|_| Error::Config("desktop browser control timed out".into()))?
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

#[cfg(any(target_os = "linux", test))]
async fn run_short(command: &mut Command) -> Result<()> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(5), command.status())
        .await
        .map_err(|_| Error::Config("desktop setup timed out".into()))??;
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
        .map_err(|_| Error::Config("desktop configuration timed out".into()))??;
    if !output.status.success() || output.stdout.len() > 4096 || output.stderr.len() > 4096 {
        return Err(Error::Config("desktop configuration failed".into()));
    }
    Ok(output)
}

fn unavailable() -> Error {
    Error::Config("gateway desktop is unavailable".into())
}

#[cfg(test)]
mod tests;
