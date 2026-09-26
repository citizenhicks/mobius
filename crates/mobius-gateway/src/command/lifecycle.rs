use super::*;

#[cfg(any(unix, test))]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessRecord {
    pub(super) pid: u32,
    pub(super) endpoint: Option<String>,
}

pub(super) struct ProcessRecordGuard {
    #[cfg(unix)]
    pub(super) path: PathBuf,
    #[cfg(unix)]
    pub(super) file: File,
}

#[derive(Debug)]
pub(super) struct StartupGuard {
    #[cfg(unix)]
    pub(super) file: File,
}

pub(super) async fn serve(
    state_dir: PathBuf,
    lock_startup: bool,
    save_local_client: fn(&Endpoint, String) -> Result<()>,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let (store, config) = ConfigStore::open(state_dir)?;
    let state_dir = store.state_dir().to_path_buf();
    let startup = lock_startup
        .then(|| StartupGuard::create(&state_dir))
        .transpose()?;
    #[cfg(not(unix))]
    let _ = load_local_client;
    #[cfg(unix)]
    if reuse_current_gateway(&store, &config, None, load_local_client)
        .await?
        .is_some()
    {
        println!("gateway is already running at the same or a newer version");
        return Ok(());
    }
    #[cfg(unix)]
    ensure_gateway_stopped(&store, &config)?;
    let auth = AuthStore::open(store.auth_path())?;
    if let Some((endpoint, token)) = provision_cloudflare_local_client(&auth, &config)? {
        save_local_client(&endpoint, token)?;
    }
    let mut server = GatewayServer::open(state_dir.clone()).await?;
    let ready = server.notify_ready();
    let mut tunnel = CloudflareTunnel::start(&store, &config)?;
    let endpoint = match &mut tunnel {
        Some(tunnel) => Some(tunnel.endpoint().await?),
        None => None,
    };
    let serving = async {
        match &endpoint {
            Some(endpoint) => server.serve_cloudflare(endpoint.host().to_owned()).await,
            None => server.serve().await,
        }
    };
    tokio::pin!(serving);
    tokio::select! {
        biased;
        result = &mut serving => return result,
        result = ready => result.map_err(|_| Error::Config("gateway stopped before becoming ready".into()))?,
    }
    // The parent treats this locked record as readiness, so publish it only after
    // the serving loop has completed initialization and can accept connections.
    let _process_record = ProcessRecordGuard::create(&state_dir, endpoint.as_ref())?;
    drop(startup);
    println!("gateway serving in foreground");
    print_listener(&config, endpoint.as_ref());
    tokio::select! {
        result = &mut serving => result,
        result = async {
            match &mut tunnel {
                Some(tunnel) => tunnel.wait().await,
                None => std::future::pending().await,
            }
        } => result,
    }
}

#[cfg(unix)]
pub(super) async fn serve_in_background(
    state_dir: PathBuf,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let (store, config) = ConfigStore::open(state_dir)?;
    let _startup = StartupGuard::create(store.state_dir())?;
    let mut interrupts = signal(SignalKind::interrupt())?;
    let mut terminations = signal(SignalKind::terminate())?;
    let Some(process) = start_background_gateway(
        store.state_dir(),
        &mut interrupts,
        &mut terminations,
        load_local_client,
    )
    .await?
    else {
        println!("gateway start cancelled");
        return Ok(());
    };
    println!("gateway running (pid {})", process.pid);
    print_listener(&config, process.endpoint()?.as_ref());
    Ok(())
}

/// Starts the configured detached gateway, replacing an older running release.
#[cfg(unix)]
/// # Errors
///
/// Returns an error if the supplied value is invalid.
pub async fn ensure_background_gateway(
    state_dir: PathBuf,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    serve_in_background(state_dir, load_local_client).await
}

#[cfg(unix)]
pub(super) async fn start_background_gateway(
    state_dir: &Path,
    interrupts: &mut TokioSignal,
    terminations: &mut TokioSignal,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<Option<ProcessRecord>> {
    let state_dir = fs::canonicalize(state_dir)?;
    let process_path = state_dir.join(PROCESS_FILE);
    let (store, config) = ConfigStore::open(state_dir.clone())?;
    if let Some(process) = reuse_current_gateway(&store, &config, None, load_local_client).await? {
        return Ok(Some(process));
    }

    let log = tempfile::NamedTempFile::new_in(&state_dir)?;
    log.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut command = TokioCommand::new(std::env::current_exe()?);
    command
        .arg("__serve")
        .arg("--state-dir")
        .arg(&state_dir)
        .current_dir(&state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.reopen()?));
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
        tokio::select! {
            () = shutdown_signal(interrupts, terminations) => {
                stop_background_child(&mut child, &process_path).await;
                return Ok(None);
            }
            () = tokio::time::sleep(BACKGROUND_START_POLL_INTERVAL) => {}
        }
    }
}

#[cfg(unix)]
pub(super) async fn shutdown_signal(interrupts: &mut TokioSignal, terminations: &mut TokioSignal) {
    tokio::select! {
        _ = interrupts.recv() => {}
        _ = terminations.recv() => {}
    }
}

#[cfg(not(unix))]
pub(super) async fn serve_in_background(
    _state_dir: PathBuf,
    _load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    Err(unsupported_lifecycle())
}

#[cfg(not(unix))]
pub async fn ensure_background_gateway(
    _state_dir: PathBuf,
    _load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    Err(unsupported_lifecycle())
}

#[cfg(unix)]
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
        let endpoint =
            configured_endpoint.map_or_else(Endpoint::from_env, |endpoint| Ok(endpoint.clone()))?;
        if endpoint.is_plaintext() || endpoint.is_websocket() {
            return Err(Error::Config(
                "TLS gateway upgrades require MOBIUS_GATEWAY_ENDPOINT with the certificate hostname".into(),
            ));
        }
        endpoint
    } else {
        loopback_endpoint(config)?
    };
    let token = load_local_client(&endpoint)?.ok_or_else(|| {
        Error::Config("local gateway credential is unavailable; cannot check its version".into())
    })?;
    let version = tokio::time::timeout(
        Duration::from_secs(2),
        endpoint.local_gateway_version(config.listen, &token),
    )
    .await
    .map_err(|_| Error::Config("gateway version check timed out".into()))??;
    if !gateway_version_is_older(&version, env!("CARGO_PKG_VERSION"))? {
        return Ok(Some(process));
    }
    let state_dir = store.state_dir().to_path_buf();
    tokio::task::spawn_blocking(move || stop_gateway(&state_dir, Some(process.pid)))
        .await
        .map_err(|error| Error::Config(format!("gateway stop task failed: {error}")))??;
    Ok(None)
}

#[cfg(any(unix, test))]
pub(super) fn gateway_version_is_older(running: &str, starting: &str) -> Result<bool> {
    let parse = |version: &str| {
        semver::Version::parse(version)
            .map_err(|error| Error::Config(format!("invalid gateway version `{version}`: {error}")))
    };
    Ok(parse(running)?.cmp_precedence(&parse(starting)?).is_lt())
}

#[cfg(unix)]
pub(super) async fn stop_background_child(child: &mut Child, process_path: &Path) {
    if let Some(pid) = child.id() {
        terminate_process_group(pid);
    }
    let _ = child.wait().await;
    remove_unlocked_process_record(process_path);
}

#[cfg(unix)]
pub(super) fn remove_unlocked_process_record(path: &Path) {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return;
    };
    if file.try_lock().is_ok() {
        let _ = fs::remove_file(path);
    }
}

#[cfg(unix)]
pub(super) fn background_startup_error(
    message: impl std::fmt::Display,
    log: &tempfile::NamedTempFile,
) -> Error {
    let mut details = String::new();
    if let Ok(file) = File::open(log.path()) {
        let _ = file
            .take(MAX_BACKGROUND_ERROR_BYTES)
            .read_to_string(&mut details);
    }
    let details = details.trim();
    Error::Config(if details.is_empty() {
        message.to_string()
    } else {
        format!("{message}: {details}")
    })
}

pub(super) fn print_listener(config: &GatewayConfig, runtime_endpoint: Option<&Endpoint>) {
    if let Some(cloudflare) = &config.cloudflare {
        if let Some(endpoint) = runtime_endpoint
            .map(ToString::to_string)
            .or_else(|| cloudflare.endpoint())
        {
            println!("public endpoint: {endpoint}");
        } else {
            println!("public endpoint: assigned when the gateway starts");
        }
        println!("local endpoint: tcp://{}", config.listen);
        println!("tunnel origin: http://{}", config.listen);
        return;
    }
    let scheme = if config.tls.is_some() { "tls" } else { "tcp" };
    println!("listener: {scheme}://{}", config.listen);
}

#[cfg(unix)]
pub(super) fn exit_gateway(state_dir: PathBuf) -> Result<()> {
    let (store, _) = ConfigStore::open(state_dir)?;
    let _startup = StartupGuard::create(store.state_dir())?;
    stop_gateway(store.state_dir(), None)
}

#[cfg(unix)]
pub(super) fn stop_gateway(state_dir: &Path, expected_pid: Option<u32>) -> Result<()> {
    let path = state_dir.join(PROCESS_FILE);
    let Some((record, file)) = open_process_record(&path)? else {
        eprintln!("gateway is stopped");
        return Ok(());
    };
    if !process_is_running(&file)? {
        eprintln!("gateway is stopped");
        return Ok(());
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
        .map_err(|_| Error::Config("invalid gateway process record".into()))?;
    if let Err(error) = kill(pid, Signal::SIGINT) {
        if !process_is_running(&file)? {
            eprintln!("gateway is stopped");
            return Ok(());
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
    eprintln!("gateway stopped");
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn exit_gateway(_state_dir: PathBuf) -> Result<()> {
    Err(unsupported_lifecycle())
}

#[cfg(any(unix, test))]
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
    #[cfg(unix)]
    pub(super) fn create(state_dir: &Path, endpoint: Option<&Endpoint>) -> Result<Self> {
        let state_dir = fs::canonicalize(state_dir)?;
        let path = state_dir.join(PROCESS_FILE);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => Error::Config("gateway is already running".into()),
            TryLockError::Error(error) => error.into(),
        })?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let record = ProcessRecord {
            pid: std::process::id(),
            endpoint: endpoint.map(ToString::to_string),
        };
        serde_json::to_writer(&mut file, &record)?;
        file.flush()?;
        file.sync_all()?;
        Ok(Self { path, file })
    }

    #[cfg(not(unix))]
    pub(super) fn create(_state_dir: &Path, _endpoint: Option<&Endpoint>) -> Result<Self> {
        Ok(Self {})
    }
}

impl Drop for ProcessRecordGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = fs::remove_file(&self.path);
            let _ = self.file.unlock();
        }
    }
}

impl StartupGuard {
    #[cfg(unix)]
    pub(super) fn create(state_dir: &Path) -> Result<Self> {
        let path = fs::canonicalize(state_dir)?.join(STARTUP_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => {
                Error::Config("gateway startup is already in progress".into())
            }
            TryLockError::Error(error) => error.into(),
        })?;
        Ok(Self { file })
    }

    #[cfg(not(unix))]
    fn create(_state_dir: &Path) -> Result<Self> {
        Ok(Self {})
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = self.file.unlock();
    }
}

#[cfg(any(unix, test))]
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

#[cfg(any(unix, test))]
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

#[cfg(unix)]
pub(super) fn running_process_pid(path: &Path) -> Result<Option<u32>> {
    Ok(running_process_record(path)?.map(|record| record.pid))
}

#[cfg(unix)]
pub(super) fn running_process_record(path: &Path) -> Result<Option<ProcessRecord>> {
    let Some((record, file)) = open_process_record(path)? else {
        return Ok(None);
    };
    Ok(process_is_running(&file)?.then_some(record))
}

#[cfg(not(unix))]
pub(super) fn unsupported_lifecycle() -> Error {
    Error::Config("gateway process lifecycle commands require macOS or Linux".into())
}
