use super::*;

pub(super) fn initialize(options: InitOptions) -> Result<()> {
    let (store, config) = match options.cloudflare {
        Some(CloudflareInit::Quick) => {
            ConfigStore::initialize_quick_cloudflare(options.state_dir, options.listen)?
        }
        Some(CloudflareInit::Named { hostname, token }) => {
            ConfigStore::initialize_named_cloudflare(
                options.state_dir,
                options.listen,
                &hostname,
                &token,
            )?
        }
        None if options.tls.is_none() => {
            ConfigStore::initialize_quick_cloudflare(options.state_dir, options.listen)?
        }
        None => ConfigStore::initialize(options.state_dir, options.listen, options.tls)?,
    };
    initialize_auth(&store)?;
    println!("initialized möbius gateway");
    print_listener(&config, None);
    println!("run `mobius-gateway connect` to pair a client");
    Ok(())
}

pub(super) fn initialize_auth(store: &ConfigStore) -> Result<()> {
    if let Err(error) = AuthStore::initialize(store.auth_path()) {
        return cleanup_failed_initialization(store, error);
    }
    Ok(())
}

pub(super) fn initialize_bootstrap(
    state_dir: PathBuf,
    save_local_client: fn(&Endpoint, String) -> Result<()>,
) -> Result<()> {
    let (store, config) = ConfigStore::initialize(state_dir, DEFAULT_LISTEN, None)?;
    let initialized = AuthStore::initialize(store.auth_path()).and_then(|(auth, _)| {
        let endpoint = direct_loopback_endpoint(&config)?;
        let issued = auth.provision_local_client()?;
        save_local_client(&endpoint, issued.token)
    });
    if let Err(error) = initialized {
        return cleanup_failed_initialization(&store, error);
    }
    println!("initialized möbius gateway bootstrap");
    print_listener(&config, None);
    Ok(())
}

pub(super) fn reset_default_agent(state_dir: PathBuf) -> Result<()> {
    #[cfg(unix)]
    {
        let (store, _) = ConfigStore::open(state_dir)?;
        let _startup = StartupGuard::create(store.state_dir())?;
        stop_gateway(store.state_dir(), None)?;
        let (store, config) = ConfigStore::open(store.state_dir().to_path_buf())?;
        let current = config.default_agent.as_ref().ok_or_else(|| {
            Error::Config("configure a provider before resetting defaults".into())
        })?;
        let composition = crate::wire::AgentComposition {
            provider: current.config.provider.clone(),
            ..crate::wire::AgentComposition::default()
        };
        let config = config.replacing_default_agent(current.revision, composition)?;
        store.save(&config)?;
        println!("reset möbius gateway defaults for new chats");
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = state_dir;
        Err(unsupported_lifecycle())
    }
}

pub(super) fn direct_loopback_endpoint(config: &GatewayConfig) -> Result<Endpoint> {
    if !config.listen.ip().is_loopback() || config.tls.is_some() || config.cloudflare.is_some() {
        return Err(Error::Config(
            "bootstrap commands require a direct plaintext loopback gateway".into(),
        ));
    }
    loopback_endpoint(config)
}

fn cleanup_failed_initialization<T>(store: &ConfigStore, error: Error) -> Result<T> {
    std::fs::remove_dir_all(store.state_dir()).map_err(|cleanup| {
        Error::Config(format!(
            "{error}; failed to remove incomplete gateway state at {}: {cleanup}",
            store.state_dir().display()
        ))
    })?;
    Err(error)
}

pub(super) fn provision_cloudflare_local_client(
    auth: &AuthStore,
    config: &GatewayConfig,
) -> Result<Option<(Endpoint, String)>> {
    if config.cloudflare.is_none() {
        return Ok(None);
    }
    let endpoint = loopback_endpoint(config)?;
    let issued = auth.provision_local_client()?;
    Ok(Some((endpoint, issued.token)))
}

pub(super) fn loopback_endpoint(config: &GatewayConfig) -> Result<Endpoint> {
    format!("tcp://{}", config.listen).parse()
}

/// Initializes one gateway with an account-free Cloudflare Quick Tunnel.
pub fn initialize_quick_cloudflare(state_dir: PathBuf) -> Result<()> {
    initialize(InitOptions {
        state_dir,
        listen: DEFAULT_LISTEN,
        tls: None,
        cloudflare: Some(CloudflareInit::Quick),
    })
}

/// Initializes one gateway against a user-owned named Cloudflare Tunnel.
pub fn initialize_named_cloudflare(
    state_dir: PathBuf,
    hostname: String,
    token: String,
) -> Result<()> {
    initialize(InitOptions {
        state_dir,
        listen: DEFAULT_LISTEN,
        tls: None,
        cloudflare: Some(CloudflareInit::Named { hostname, token }),
    })
}

/// Permanently removes previously confirmed gateway state after stopping its process.
///
/// # Errors
///
/// Returns an error unless the target is an empty real directory or contains a regular
/// `gateway.toml` marker. Lifecycle or filesystem failures are also returned.
pub fn reset_gateway_state(state_dir: PathBuf) -> Result<()> {
    #[cfg(unix)]
    {
        let had_config = validate_reset_target(&state_dir, false)?;
        let state_dir = fs::canonicalize(state_dir)?;
        let _startup = StartupGuard::create(&state_dir)?;
        if validate_reset_target(&state_dir, true)? != had_config {
            return Err(invalid_reset_target(&state_dir));
        }
        stop_gateway(&state_dir, None)?;
        fs::remove_dir_all(state_dir)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = state_dir;
        Err(unsupported_lifecycle())
    }
}

#[cfg(unix)]
pub(super) fn validate_reset_target(path: &Path, ignore_startup_lock: bool) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_reset_target(path));
    }
    let mut empty = true;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if ignore_startup_lock && entry.file_name() == STARTUP_FILE {
            continue;
        }
        empty = false;
    }
    if empty {
        return Ok(false);
    }
    let marker = fs::symlink_metadata(path.join(STATE_MARKER_FILE))
        .map_err(|_| invalid_reset_target(path))?;
    if !marker.is_file() || marker.file_type().is_symlink() {
        return Err(invalid_reset_target(path));
    }
    Ok(true)
}

#[cfg(unix)]
pub(super) fn invalid_reset_target(path: &Path) -> Error {
    Error::Config(format!(
        "refusing to reset {}: expected an empty directory or möbius gateway state with a regular {STATE_MARKER_FILE}",
        path.display()
    ))
}
