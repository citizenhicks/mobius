use super::{
    Arc, AuthStore, BACKGROUND_START_POLL_INTERVAL, BACKGROUND_START_TIMEOUT,
    CONNECTION_POLL_INTERVAL, Child, CloudflareConfig, CloudflareTunnel, ConfigStore, Deserialize,
    Duration, EXIT_POLL_INTERVAL, EXIT_TIMEOUT, Endpoint, Error, File, GatewayConfig,
    GatewayServer, Instant, MAX_BACKGROUND_ERROR_BYTES, MAX_PROCESS_RECORD_BYTES, OnceLock,
    OpenOptions, PROCESS_FILE, PUBLIC_ENDPOINT_TIMEOUT, Path, PathBuf, Pid, Result, STARTUP_FILE,
    SeekFrom, Serialize, Signal, SignalKind, Stdio, TokioCommand, TokioSignal, TryLockError,
    ensure_gateway_stopped, eprint_line, fs, kill, loopback_endpoint, print_line,
    provision_cloudflare_local_client, signal, terminate_process_group,
};

#[cfg(target_os = "macos")]
use std::ffi::OsString;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessRecord {
    pub(super) pid: u32,
    pub(super) endpoint: Option<String>,
}

pub(super) struct ProcessRecordGuard {
    pub(super) path: PathBuf,
    pub(super) file: File,
}

#[derive(Debug)]
pub(super) struct StartupGuard {
    pub(super) file: File,
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "the server is borrowed by the serving future, which runs until shutdown"
)]
pub(super) async fn serve(
    state_dir: PathBuf,
    lock_startup: bool,
    save_local_client: fn(&Endpoint, String) -> Result<()>,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let (store, config) = ConfigStore::open(state_dir)?;
    let startup = lock_startup
        .then(|| StartupGuard::create(store.state_dir()))
        .transpose()?;
    if reuse_current_gateway(&store, &config, None, load_local_client)
        .await?
        .is_some()
    {
        return Ok(print_line(format_args!(
            "gateway is already running at the same or a newer version"
        ))?);
    }
    ensure_gateway_stopped(&store, &config)?;
    let auth = AuthStore::open(store.auth_path(), config.auth)?;
    if let Some((endpoint, token)) = provision_cloudflare_local_client(&auth, &config)? {
        save_local_client(&endpoint, token)?;
    }
    let mut server = GatewayServer::open(store.state_dir().to_path_buf()).await?;
    let ready = server.notify_ready();
    let mut tunnel = CloudflareTunnel::start(&store, &config)?;
    // Loopback clients are served at once; the tunnel's hostname arrives when cloudflared
    // reports it, and WebSocket upgrades wait for it.
    let websocket_host = tunnel.as_ref().map(|_| Arc::new(OnceLock::new()));
    let serving = async {
        match &websocket_host {
            Some(hostname) => server.serve_cloudflare(Arc::clone(hostname)).await,
            None => server.serve().await,
        }
    };
    tokio::pin!(serving);
    tokio::select! {
        biased;
        result = &mut serving => return result,
        result = ready => result.map_err(|error| Error::Config(format!("gateway stopped before becoming ready: {error}")))?,
    }
    // The parent treats this locked record as readiness, so publish it only after
    // the serving loop has completed initialization and can accept connections.
    let mut process_record = ProcessRecordGuard::create(store.state_dir(), None)?;
    drop(startup);
    print_line(format_args!("gateway serving in foreground"))?;
    let (Some(tunnel), Some(hostname)) = (tunnel.as_mut(), websocket_host.as_deref()) else {
        print_listener(&config, None)?;
        return serving.await;
    };
    let endpoint = tokio::select! {
        result = &mut serving => return result,
        endpoint = tunnel.endpoint() => endpoint?,
    };
    if config
        .cloudflare
        .as_ref()
        .and_then(CloudflareConfig::hostname)
        .is_some_and(|configured| configured != endpoint.host())
    {
        return Err(Error::Config(
            "runtime Cloudflare hostname does not match gateway configuration".into(),
        ));
    }
    hostname.get_or_init(|| endpoint.host().to_owned());
    process_record.publish(&endpoint)?;
    print_listener(&config, Some(&endpoint))?;
    tokio::select! {
        result = &mut serving => result,
        result = tunnel.wait() => result,
    }
}

/// How [`ensure_background_gateway_with`] starts the detached gateway.
#[derive(Debug, Default)]
pub struct BackgroundGatewayOptions {
    /// Gateway executable started as `__serve`; the current executable when `None`.
    pub binary: Option<PathBuf>,
    /// Print nothing and install no signal handlers, for callers embedding the start in
    /// their own process; interrupting such a caller leaves the starting gateway alone.
    pub quiet: bool,
}

/// Starts the configured detached gateway, replacing an older running release.
/// # Errors
///
/// Returns an error if the supplied value is invalid.
pub async fn ensure_background_gateway(
    state_dir: PathBuf,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    ensure_background_gateway_with(
        state_dir,
        load_local_client,
        BackgroundGatewayOptions::default(),
    )
    .await
}

/// Starts the configured detached gateway as [`ensure_background_gateway`] does, with
/// the executable and output chosen by `options`.
/// # Errors
///
/// Returns an error if the configuration is invalid or the gateway does not start.
pub async fn ensure_background_gateway_with(
    state_dir: PathBuf,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
    options: BackgroundGatewayOptions,
) -> Result<()> {
    let (store, config) = ConfigStore::open(state_dir)?;
    let _startup = StartupGuard::create(store.state_dir())?;
    let binary = match options.binary {
        Some(binary) => binary,
        None => std::env::current_exe()?,
    };
    let mut signals = (!options.quiet)
        .then(ShutdownSignals::install)
        .transpose()?;
    let Some(process) = start_background_gateway(
        &store,
        &config,
        &binary,
        signals.as_mut(),
        load_local_client,
    )
    .await?
    else {
        return Ok(print_line(format_args!("gateway start cancelled"))?);
    };
    if options.quiet {
        return Ok(());
    }
    print_line(format_args!("gateway running (pid {})", process.pid))?;
    Ok(print_listener(&config, process.endpoint()?.as_ref())?)
}

/// Interrupt and termination signals that cancel a foreground gateway command.
pub(super) struct ShutdownSignals {
    interrupts: TokioSignal,
    terminations: TokioSignal,
}

impl ShutdownSignals {
    pub(super) fn install() -> Result<Self> {
        Ok(Self {
            interrupts: signal(SignalKind::interrupt())?,
            terminations: signal(SignalKind::terminate())?,
        })
    }

    pub(super) async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupts.recv() => {}
            _ = self.terminations.recv() => {}
        }
    }
}

pub(super) async fn start_background_gateway(
    store: &ConfigStore,
    config: &GatewayConfig,
    binary: &Path,
    mut signals: Option<&mut ShutdownSignals>,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<Option<ProcessRecord>> {
    let state_dir = store.state_dir();
    let process_path = state_dir.join(PROCESS_FILE);
    if let Some(process) = reuse_current_gateway(store, config, None, load_local_client).await? {
        return Ok(Some(process));
    }

    let log = tempfile::NamedTempFile::new_in(state_dir)?;
    log.as_file().set_permissions(mobius::owner_only::file())?;
    let mut command = TokioCommand::new(binary);
    command
        .arg("__serve")
        .arg("--state-dir")
        .arg(state_dir)
        .current_dir(state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.reopen()?));
    #[cfg(target_os = "macos")]
    command.env(
        "PATH",
        macos_gateway_path(std::env::var_os("PATH").as_deref()),
    );
    command.as_std_mut().process_group(0);

    let mut child = command.spawn()?;
    let Some(pid) = child.id() else {
        stop_background_child(&mut child, &process_path).await;
        return Err(Error::Config("background gateway has no process ID".into()));
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(background_startup_error(
                    format!("background gateway exited during startup with {status}"),
                    &log,
                ));
            }
            Ok(None) => {}
            Err(error) => {
                stop_background_child(&mut child, &process_path).await;
                return Err(error.into());
            }
        }

        let process_error = match running_process_record(&process_path) {
            Ok(Some(record)) if record.pid == pid => return Ok(Some(record)),
            Ok(Some(record)) => {
                stop_background_child(&mut child, &process_path).await;
                return Err(background_startup_error(
                    format!(
                        "gateway process {} claimed the process record during startup",
                        record.pid
                    ),
                    &log,
                ));
            }
            Ok(None) => None,
            Err(error) => Some(error),
        };

        if started.elapsed() >= BACKGROUND_START_TIMEOUT {
            stop_background_child(&mut child, &process_path).await;
            let message = process_error.map_or_else(
                || {
                    format!(
                        "background gateway did not start within {} seconds",
                        BACKGROUND_START_TIMEOUT.as_secs()
                    )
                },
                |error| format!("background gateway process record is invalid: {error}"),
            );
            return Err(background_startup_error(message, &log));
        }
        let cancelled = async {
            match signals.as_deref_mut() {
                Some(signals) => signals.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = cancelled => {
                stop_background_child(&mut child, &process_path).await;
                return Ok(None);
            }
            () = tokio::time::sleep(BACKGROUND_START_POLL_INTERVAL) => {}
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) fn macos_gateway_path(inherited: Option<&std::ffi::OsStr>) -> OsString {
    // Finder-launched apps omit Homebrew; retain the user's existing tool priority.
    let mut path = inherited
        .unwrap_or_else(|| std::ffi::OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin"))
        .to_os_string();
    path.push(":/opt/homebrew/bin:/usr/local/bin");
    path
}

pub(super) async fn reuse_current_gateway(
    store: &ConfigStore,
    config: &GatewayConfig,
    configured_endpoint: Option<&Endpoint>,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<Option<ProcessRecord>> {
    let Some(process) = running_process_record(&store.state_dir().join(PROCESS_FILE))? else {
        return Ok(None);
    };
    let endpoint = if config.tls.is_some() {
        let endpoint = match configured_endpoint {
            Some(endpoint) => std::borrow::Cow::Borrowed(endpoint),
            None => std::borrow::Cow::Owned(Endpoint::from_env()?),
        };
        if endpoint.is_plaintext() || endpoint.is_websocket() {
            return Err(Error::Config(
                "TLS gateway upgrades require MOBIUS_GATEWAY_ENDPOINT with the certificate hostname".into(),
            ));
        }
        endpoint
    } else {
        std::borrow::Cow::Owned(loopback_endpoint(config)?)
    };
    let token = load_local_client(&endpoint)?.ok_or_else(|| {
        Error::Config("local gateway credential is unavailable; cannot check its version".into())
    })?;
    let version = tokio::time::timeout(
        Duration::from_secs(2),
        endpoint.local_gateway_version(config.listen, &token),
    )
    .await
    .map_err(|elapsed| Error::Config(format!("gateway version check timed out: {elapsed}")))??;
    if !gateway_version_is_older(&version, env!("CARGO_PKG_VERSION"))? {
        return Ok(Some(process));
    }
    let state_dir = store.state_dir().to_path_buf();
    tokio::task::spawn_blocking(move || stop_gateway(&state_dir, Some(process.pid)))
        .await
        .map_err(|error| Error::Config(format!("gateway stop task failed: {error}")))??;
    Ok(None)
}

/// The running gateway's public endpoint. A Quick Tunnel assigns it after the gateway is
/// already serving locally, so this waits for `pid` to publish it.
pub(super) async fn runtime_endpoint(
    store: &ConfigStore,
    config: &GatewayConfig,
    process: &ProcessRecord,
) -> Result<Option<Endpoint>> {
    let quick = matches!(config.cloudflare, Some(CloudflareConfig::Quick));
    let started = Instant::now();
    let mut latest = None;
    // The record is rewritten in place when the address arrives, so one read can be torn.
    let mut unreadable = None;
    loop {
        let endpoint = latest.as_ref().unwrap_or(process).endpoint()?;
        if endpoint.is_some() || !quick {
            return Ok(endpoint);
        }
        if started.elapsed() >= PUBLIC_ENDPOINT_TIMEOUT {
            let detail = unreadable.map_or_else(String::new, |error| format!(" ({error})"));
            return Err(Error::Config(format!(
                "Cloudflare Quick Tunnel did not assign an address within {} seconds{detail}",
                PUBLIC_ENDPOINT_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(CONNECTION_POLL_INTERVAL).await;
        match running_process_record(&store.state_dir().join(PROCESS_FILE)) {
            Ok(Some(current)) if current.pid == process.pid => latest = Some(current),
            Ok(_) => {
                return Err(Error::Config(
                    "gateway stopped before its tunnel assigned an address".into(),
                ));
            }
            Err(error) => unreadable = Some(error),
        }
    }
}

pub(super) fn gateway_version_is_older(running: &str, starting: &str) -> Result<bool> {
    let parse = |version: &str| {
        semver::Version::parse(version)
            .map_err(|error| Error::Config(format!("invalid gateway version `{version}`: {error}")))
    };
    Ok(parse(running)?.cmp_precedence(&parse(starting)?).is_lt())
}

pub(super) async fn stop_background_child(child: &mut Child, process_path: &Path) {
    if let Some(pid) = child.id() {
        terminate_process_group(pid);
    }
    if let Err(error) = child.wait().await {
        tracing::warn!(%error, "failed to reap background gateway child");
    }
    remove_unlocked_process_record(process_path);
}

pub(super) fn remove_unlocked_process_record(path: &Path) {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return;
    };
    if file.try_lock().is_ok()
        && let Err(error) = fs::remove_file(path)
    {
        tracing::warn!(%error, path = %path.display(), "failed to remove stale process record");
    }
}

/// Adds a bounded diagnostic excerpt from the background gateway's startup log.
fn background_startup_error(
    message: impl std::fmt::Display,
    log: &tempfile::NamedTempFile,
) -> Error {
    let mut details = String::new();
    if let Ok(file) = File::open(log.path())
        && let Err(error) = file
            .take(MAX_BACKGROUND_ERROR_BYTES)
            .read_to_string(&mut details)
    {
        tracing::warn!(%error, "failed to read gateway startup log");
    }
    let details = details.trim();
    Error::Config(if details.is_empty() {
        message.to_string()
    } else {
        format!("{message}: {details}")
    })
}

pub(super) fn print_listener(
    config: &GatewayConfig,
    runtime_endpoint: Option<&Endpoint>,
) -> std::io::Result<()> {
    if let Some(cloudflare) = &config.cloudflare {
        if let Some(endpoint) = runtime_endpoint
            .map(ToString::to_string)
            .or_else(|| cloudflare.endpoint())
        {
            print_line(format_args!("public endpoint: {endpoint}"))?;
        } else {
            print_line(format_args!(
                "public endpoint: assigned when the gateway starts"
            ))?;
        }
        print_line(format_args!("local endpoint: tcp://{}", config.listen))?;
        return print_line(format_args!("tunnel origin: http://{}", config.listen));
    }
    let scheme = if config.tls.is_some() { "tls" } else { "tcp" };
    print_line(format_args!("listener: {scheme}://{}", config.listen))
}

pub(super) fn exit_gateway(state_dir: PathBuf) -> Result<()> {
    let (store, _) = ConfigStore::open(state_dir)?;
    let _startup = StartupGuard::create(store.state_dir())?;
    stop_gateway(store.state_dir(), None)
}

pub(super) fn stop_gateway(state_dir: &Path, expected_pid: Option<u32>) -> Result<()> {
    let path = state_dir.join(PROCESS_FILE);
    let Some((record, file)) = open_process_record(&path)? else {
        return Ok(eprint_line(format_args!("gateway is stopped"))?);
    };
    if !process_is_running(&file)? {
        return Ok(eprint_line(format_args!("gateway is stopped"))?);
    }
    if let Some(expected_pid) = expected_pid
        && record.pid != expected_pid
    {
        return Err(Error::Config(format!(
            "gateway process changed from {expected_pid} to {}",
            record.pid
        )));
    }
    let pid = i32::try_from(record.pid)
        .map(Pid::from_raw)
        .map_err(|error| Error::Config(format!("invalid gateway process record: {error}")))?;
    if let Err(error) = kill(pid, Signal::SIGINT) {
        if !process_is_running(&file)? {
            return Ok(eprint_line(format_args!("gateway is stopped"))?);
        }
        return Err(Error::Config(format!(
            "failed to interrupt gateway: {error}"
        )));
    }
    let started = Instant::now();
    while process_is_running(&file)? {
        if started.elapsed() >= EXIT_TIMEOUT {
            return Err(Error::Config(format!(
                "gateway process {} did not stop within {} seconds",
                record.pid,
                EXIT_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(EXIT_POLL_INTERVAL);
    }
    Ok(eprint_line(format_args!("gateway stopped"))?)
}

impl ProcessRecord {
    pub(super) fn validate(&self) -> Result<()> {
        if self.pid == 0 || i32::try_from(self.pid).is_err() {
            return Err(Error::Config("invalid gateway process record".into()));
        }
        if let Some(endpoint) = self.endpoint()?
            && !endpoint.is_websocket()
        {
            return Err(Error::Config(
                "gateway process endpoint must use wss://".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn endpoint(&self) -> Result<Option<Endpoint>> {
        self.endpoint.as_deref().map(str::parse).transpose()
    }
}

impl ProcessRecordGuard {
    pub(super) fn create(state_dir: &Path, endpoint: Option<&Endpoint>) -> Result<Self> {
        let state_dir = fs::canonicalize(state_dir)?;
        let path = state_dir.join(PROCESS_FILE);
        let file = mobius::owner_only::open(&path)?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => Error::Config("gateway is already running".into()),
            TryLockError::Error(error) => error.into(),
        })?;
        let mut guard = Self { path, file };
        guard.write(endpoint)?;
        Ok(guard)
    }

    /// Adds the tunnel's public endpoint once it is known; the record already stands for
    /// readiness, so clients that only need loopback never wait for it.
    pub(super) fn publish(&mut self, endpoint: &Endpoint) -> Result<()> {
        self.write(Some(endpoint))
    }

    fn write(&mut self, endpoint: Option<&Endpoint>) -> Result<()> {
        let record = ProcessRecord {
            pid: std::process::id(),
            endpoint: endpoint.map(ToString::to_string),
        };
        let contents = serde_json::to_vec(&record)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&contents)?;
        self.file.set_len(contents.len() as u64)?;
        self.file.flush()?;
        Ok(self.file.sync_all()?)
    }
}

impl Drop for ProcessRecordGuard {
    fn drop(&mut self) {
        {
            if let Err(error) = fs::remove_file(&self.path) {
                tracing::warn!(%error, path = %self.path.display(), "failed to remove process record");
            }
            if let Err(error) = self.file.unlock() {
                tracing::warn!(%error, "failed to unlock process record");
            }
        }
    }
}

impl StartupGuard {
    pub(super) fn create(state_dir: &Path) -> Result<Self> {
        let path = fs::canonicalize(state_dir)?.join(STARTUP_FILE);
        let file = mobius::owner_only::open(&path)?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => {
                Error::Config("gateway startup is already in progress".into())
            }
            TryLockError::Error(error) => error.into(),
        })?;
        Ok(Self { file })
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            tracing::warn!(%error, "failed to unlock gateway startup lock");
        }
    }
}

pub(super) fn open_process_record(path: &Path) -> Result<Option<(ProcessRecord, File)>> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() > MAX_PROCESS_RECORD_BYTES as u64 {
        return Err(Error::Config("gateway process record is too large".into()));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut contents = Vec::new();
    (&mut file)
        .take(MAX_PROCESS_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents.len() > MAX_PROCESS_RECORD_BYTES {
        return Err(Error::Config("gateway process record is too large".into()));
    }
    let record: ProcessRecord = serde_json::from_slice(&contents)?;
    record.validate()?;
    Ok(Some((record, file)))
}

pub(super) fn process_is_running(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

pub(super) fn running_process_pid(path: &Path) -> Result<Option<u32>> {
    Ok(running_process_record(path)?.map(|record| record.pid))
}

pub(super) fn running_process_record(path: &Path) -> Result<Option<ProcessRecord>> {
    let Some((record, file)) = open_process_record(path)? else {
        return Ok(None);
    };
    Ok(process_is_running(&file)?.then_some(record))
}
