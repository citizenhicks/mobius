use super::*;

#[cfg(unix)]
pub(super) async fn connect(
    options: ConnectOptions,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let (store, config) = ConfigStore::open(options.state_dir)?;
    let configured_endpoint = connection_endpoint(&config, options.endpoint)?;
    let startup = StartupGuard::create(store.state_dir())?;
    if let Some((client_endpoint, pairing_endpoint)) =
        running_connection_endpoints(&store, &config, configured_endpoint.clone())?
    {
        let token = load_local_client(&client_endpoint)?.ok_or_else(|| {
            Error::Config(
                "this machine has no local gateway credential; restart the gateway once and retry"
                    .into(),
            )
        })?;
        drop(startup);
        let grant = request_running_pairing_code(&client_endpoint, &token).await?;
        print_connection(
            &pairing_endpoint,
            config.cloudflare.is_some().then_some(&client_endpoint),
            &grant.code,
        )?;
        println!("gateway remains running");
        return Ok(());
    }
    ensure_gateway_stopped(&store, &config)?;
    let mut interrupts = signal(SignalKind::interrupt())?;
    let mut terminations = signal(SignalKind::terminate())?;

    let auth = AuthStore::open(store.auth_path())?;
    let grant = auth.create_pairing_code()?;
    let deadline = pairing_deadline(grant.expires_at)?;
    let process =
        match start_background_gateway(store.state_dir(), &mut interrupts, &mut terminations).await
        {
            Ok(Some(process)) => process,
            Ok(None) => {
                AuthStore::open(store.auth_path())?.revoke_pairing_code(&grant.code)?;
                println!("connection cancelled");
                return Ok(());
            }
            Err(error) => {
                if let Err(revoke) =
                    AuthStore::open(store.auth_path())?.revoke_pairing_code(&grant.code)
                {
                    return Err(Error::Config(format!(
                        "{error}; failed to revoke one-time code: {revoke}"
                    )));
                }
                return Err(error);
            }
        };
    let pid = process.pid;
    let endpoint = match process.endpoint().and_then(|runtime| {
        runtime
            .or(configured_endpoint)
            .ok_or_else(|| Error::Config("gateway did not publish its runtime endpoint".into()))
    }) {
        Ok(endpoint) => endpoint,
        Err(error) => return stop_connect_gateway(&store, pid, &grant.code, error),
    };
    drop(startup);

    if let Some(hostname) = config
        .cloudflare
        .as_ref()
        .and_then(CloudflareConfig::hostname)
    {
        println!("Cloudflare connector is running.");
        println!(
            "If needed, publish {hostname} to http://{} now; möbius will keep waiting for pairing.",
            config.listen,
        );
    }
    let local_endpoint = config
        .cloudflare
        .as_ref()
        .map(|_| loopback_endpoint(&config))
        .transpose()?;
    if let Err(error) = print_connection(&endpoint, local_endpoint.as_ref(), &grant.code) {
        return stop_connect_gateway(&store, pid, &grant.code, error.into());
    }
    println!("waiting for a client…");

    let process_path = store.state_dir().join(PROCESS_FILE);
    loop {
        let running = match running_process_pid(&process_path) {
            Ok(running) => running,
            Err(error) => return stop_connect_gateway(&store, pid, &grant.code, error),
        };
        match running {
            Some(running) if running == pid => {}
            Some(running) => {
                return Err(Error::Config(format!(
                    "gateway process changed from {pid} to {running} while waiting for a client"
                )));
            }
            None => {
                return stop_connect_gateway(
                    &store,
                    pid,
                    &grant.code,
                    Error::Config("gateway stopped before a client paired".into()),
                );
            }
        }

        let pairing = match AuthStore::open(store.auth_path())
            .and_then(|auth| auth.pairing_status(&grant.code))
        {
            Ok(pairing) => pairing,
            Err(error) => return stop_connect_gateway(&store, pid, &grant.code, error),
        };
        match pairing {
            PairingStatus::Consumed => {
                println!("paired; gateway running in background (pid {pid})");
                return Ok(());
            }
            PairingStatus::Replaced => {
                println!("another pairing code was issued; gateway remains running");
                return Ok(());
            }
            PairingStatus::Pending => {}
        }

        if Instant::now() >= deadline {
            return stop_connect_gateway(
                &store,
                pid,
                &grant.code,
                Error::Config("one-time code expired before a client paired".into()),
            );
        }

        tokio::select! {
            () = shutdown_signal(&mut interrupts, &mut terminations) => {
                cleanup_connect(&store, pid, &grant.code)?;
                println!("connection cancelled");
                return Ok(());
            }
            () = tokio::time::sleep(CONNECTION_POLL_INTERVAL) => {}
        }
    }
}

#[cfg(not(unix))]
pub(super) async fn connect(
    _options: ConnectOptions,
    _load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    Err(unsupported_lifecycle())
}

#[cfg(unix)]
pub(super) fn running_connection_endpoints(
    store: &ConfigStore,
    config: &GatewayConfig,
    configured_endpoint: Option<Endpoint>,
) -> Result<Option<(Endpoint, Endpoint)>> {
    let Some(process) = running_process_record(&store.state_dir().join(PROCESS_FILE))? else {
        return Ok(None);
    };
    let pairing_endpoint = process
        .endpoint()?
        .or(configured_endpoint)
        .ok_or_else(|| Error::Config("gateway did not publish its runtime endpoint".into()))?;
    let client_endpoint = if config.cloudflare.is_some() {
        loopback_endpoint(config)?
    } else {
        pairing_endpoint.clone()
    };
    Ok(Some((client_endpoint, pairing_endpoint)))
}

pub(super) async fn request_running_pairing_code(
    client_endpoint: &Endpoint,
    token: &str,
) -> Result<PairingGrant> {
    let client =
        GatewayClient::connect(client_endpoint, token, ClientKind::GatewayDashboard).await?;
    let (sender, mut events) = client.into_parts();
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::CreatePairingCode {
            request_id: request_id.clone(),
        })
        .await?;

    for _ in 0..MAX_PENDING_FRAMES {
        let frame = events.next().await?.ok_or_else(|| {
            Error::Protocol("gateway disconnected before returning a pairing code".into())
        })?;
        match frame.message {
            ServerMessage::PairingCode {
                request_id: actual,
                code,
                expires_at,
            } if actual == request_id => return Ok(PairingGrant { code, expires_at }),
            ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } if actual == request_id => return Err(Error::Protocol(message)),
            ServerMessage::Error { message, .. } => return Err(Error::Protocol(message)),
            _ => {}
        }
    }
    Err(Error::Protocol(format!(
        "gateway sent {MAX_PENDING_FRAMES} unrelated frames before the pairing response"
    )))
}

pub(super) async fn pairing_code(
    state_dir: PathBuf,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let (_, config) = ConfigStore::open(state_dir)?;
    let endpoint = direct_loopback_endpoint(&config)?;
    let token = load_local_client(&endpoint)?
        .ok_or_else(|| Error::Config("gateway local control credential is unavailable".into()))?;
    let grant = request_running_pairing_code(&endpoint, &token).await?;
    println!("{}", pairing_code_json(&grant)?);
    Ok(())
}

#[derive(Serialize)]
struct PairingCodeOutput<'a> {
    code: &'a str,
    expires_at: i64,
}

pub(super) fn pairing_code_json(grant: &PairingGrant) -> Result<String> {
    Ok(serde_json::to_string(&PairingCodeOutput {
        code: &grant.code,
        expires_at: grant.expires_at,
    })?)
}

pub(super) fn connection_endpoint(
    config: &GatewayConfig,
    endpoint: Option<Endpoint>,
) -> Result<Option<Endpoint>> {
    if let Some(cloudflare) = &config.cloudflare {
        if endpoint.is_some() {
            return Err(Error::Config(
                "Cloudflare gateways determine their endpoint at startup; do not use --endpoint"
                    .into(),
            ));
        }
        return cloudflare.endpoint().as_deref().map(str::parse).transpose();
    }
    match (config.tls.is_some(), endpoint) {
        (true, None) => Err(Error::Config(
            "TLS gateways require --endpoint tls://HOST:PORT using the certificate hostname".into(),
        )),
        (true, Some(endpoint)) if endpoint.is_plaintext() || endpoint.is_websocket() => Err(
            Error::Config("a TLS gateway connection endpoint must use tls://".into()),
        ),
        (false, Some(endpoint)) if !endpoint.is_plaintext() => Err(Error::Config(
            "a plaintext gateway connection endpoint must use tcp://".into(),
        )),
        (_, Some(endpoint)) => Ok(Some(endpoint)),
        (false, None) => format!("tcp://{}", config.listen).parse().map(Some),
    }
}

#[cfg(unix)]
pub(super) fn ensure_gateway_stopped(store: &ConfigStore, config: &GatewayConfig) -> Result<()> {
    if running_process_pid(&store.state_dir().join(PROCESS_FILE))?.is_some() {
        return Err(Error::Config(
            "gateway is already running; create a code from a connected client or run `mobius-gateway exit` first"
                .into(),
        ));
    }
    let _listener = std::net::TcpListener::bind(config.listen).map_err(|error| {
        Error::Config(format!(
            "gateway listener {} is unavailable; stop it before connecting: {error}",
            config.listen
        ))
    })?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn pairing_deadline(expires_at: i64) -> Result<Instant> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Config("system clock is before the Unix epoch".into()))?
        .as_secs();
    let expires_at =
        u64::try_from(expires_at).map_err(|_| Error::Config("pairing expiry is invalid".into()))?;
    let remaining = expires_at
        .checked_sub(now)
        .ok_or_else(|| Error::Config("one-time code expired before startup".into()))?;
    Instant::now()
        .checked_add(Duration::from_secs(remaining))
        .ok_or_else(|| Error::Config("pairing deadline overflow".into()))
}

#[cfg(unix)]
pub(super) fn stop_connect_gateway<T>(
    store: &ConfigStore,
    pid: u32,
    code: &str,
    error: Error,
) -> Result<T> {
    match cleanup_connect(store, pid, code) {
        Ok(()) => Err(error),
        Err(stop) => Err(Error::Config(format!(
            "{error}; failed to clean up connection: {stop}"
        ))),
    }
}

#[cfg(unix)]
pub(super) fn cleanup_connect(store: &ConfigStore, pid: u32, code: &str) -> Result<()> {
    stop_gateway(store.state_dir(), Some(pid))?;
    AuthStore::open(store.auth_path())?.revoke_pairing_code(code)
}

#[cfg(unix)]
pub(super) fn print_connection(
    endpoint: &Endpoint,
    local_endpoint: Option<&Endpoint>,
    code: &str,
) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    write_connection(stdout.lock(), endpoint, local_endpoint, code)
}

#[cfg(any(unix, test))]
pub(super) fn write_connection(
    mut output: impl Write,
    endpoint: &Endpoint,
    local_endpoint: Option<&Endpoint>,
    code: &str,
) -> std::io::Result<()> {
    if let Some(local_endpoint) = local_endpoint {
        writeln!(output, "public endpoint: {endpoint}")?;
        writeln!(output, "local endpoint: {local_endpoint}")?;
    } else {
        writeln!(output, "endpoint: {endpoint}")?;
    }
    writeln!(output, "one-time code: {code}")?;
    writeln!(
        output,
        "setup code: {}",
        pairing_setup_payload(endpoint, code)
    )?;
    writeln!(output, "copy the setup code into möbius")?;
    writeln!(output, "another terminal: mobius pair {endpoint} {code}")?;
    if let Some(local_endpoint) = local_endpoint {
        writeln!(
            output,
            "local terminal: mobius pair {local_endpoint} {code}"
        )?;
    }
    output.flush()
}

#[cfg(any(unix, test))]
pub(super) fn pairing_setup_payload(endpoint: &Endpoint, code: &str) -> String {
    format!("mobius-pair:v1|{endpoint}|{code}")
}
