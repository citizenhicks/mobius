use super::*;

/// File owner for gateway configuration and aggregate usage.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    state_dir: PathBuf,
    path: PathBuf,
}

/// Owner-only API-key storage kept outside frontend-readable configuration.
pub struct CredentialStore {
    path: PathBuf,
    values: Mutex<BTreeMap<String, StoredCredential>>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCredential {
    provider: String,
    api_key: String,
    base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UsageHistory {
    pub(super) days: BTreeMap<u64, BTreeMap<String, TokenUsage>>,
}

impl ConfigStore {
    /// Initializes an owner-only state directory and new config file.
    pub fn initialize(
        state_dir: PathBuf,
        listen: SocketAddr,
        tls: Option<TlsConfig>,
    ) -> Result<(Self, GatewayConfig)> {
        let config = GatewayConfig::new(listen, tls)?;
        let state_dir = prepare_state_dir(state_dir)?;
        let store = Self::at(state_dir);
        store.save_with_mode(&config, true)?;
        Ok((store, config))
    }

    /// Initializes state for an account-free Cloudflare Quick Tunnel.
    pub fn initialize_quick_cloudflare(
        state_dir: PathBuf,
        listen: SocketAddr,
    ) -> Result<(Self, GatewayConfig)> {
        Self::initialize_cloudflare(state_dir, listen, CloudflareConfig::Quick, None)
    }

    /// Initializes state for one user-owned Cloudflare Tunnel.
    pub fn initialize_named_cloudflare(
        state_dir: PathBuf,
        listen: SocketAddr,
        hostname: &str,
        token: &str,
    ) -> Result<(Self, GatewayConfig)> {
        Self::initialize_cloudflare(
            state_dir,
            listen,
            CloudflareConfig::named(hostname)?,
            Some(validate_cloudflare_token(token)?),
        )
    }

    fn initialize_cloudflare(
        state_dir: PathBuf,
        listen: SocketAddr,
        cloudflare: CloudflareConfig,
        token: Option<&str>,
    ) -> Result<(Self, GatewayConfig)> {
        let config = GatewayConfig::new_cloudflare(listen, cloudflare)?;
        let state_dir = prepare_state_dir(state_dir)?;
        let store = Self::at(state_dir);
        let result = token
            .map_or(Ok(()), |token| store.save_cloudflare_token(token))
            .and_then(|()| store.save_with_mode(&config, true));
        if let Err(error) = result {
            fs::remove_dir_all(&store.state_dir).map_err(|cleanup| {
                Error::Config(format!(
                    "{error}; failed to remove incomplete gateway state at {}: {cleanup}",
                    store.state_dir.display()
                ))
            })?;
            return Err(error);
        }
        Ok((store, config))
    }

    /// Opens and validates persisted gateway configuration.
    pub fn open(state_dir: PathBuf) -> Result<(Self, GatewayConfig)> {
        let state_dir = fs::canonicalize(state_dir)?;
        validate_private_state_dir(&state_dir)?;
        let store = Self::at(state_dir);
        let mut file = fs::File::open(&store.path)?;
        let mut contents = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut contents)?;
        if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
            return Err(Error::Config("gateway configuration is too large".into()));
        }
        let config: GatewayConfig = toml::from_slice(&contents).map_err(|error| {
            Error::Config(format!(
                "gateway state at {} is incompatible with this release; remove that directory and run `mobius` again: {error}",
                store.state_dir.display()
            ))
        })?;
        store.validate_config(&config)?;
        Ok((store, config))
    }

    /// Atomically replaces validated persistent configuration.
    pub fn save(&self, config: &GatewayConfig) -> Result<()> {
        self.save_with_mode(config, false)
    }

    /// Returns the protected state directory.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns the owner-managed extension store outside sandbox-masked gateway state.
    #[must_use]
    pub(crate) fn extensions_path(&self) -> PathBuf {
        crate::extensions::extensions_path(&self.state_dir)
    }

    /// Returns the provider credential file path.
    #[must_use]
    pub fn credentials_path(&self) -> PathBuf {
        self.state_dir.join("credentials.json")
    }

    /// Returns the provider browser-auth file path.
    #[must_use]
    pub fn provider_auth_path(&self) -> PathBuf {
        self.state_dir.join("provider-auth.json")
    }

    /// Returns the checkpoint database path.
    #[must_use]
    pub fn checkpoints_path(&self) -> PathBuf {
        self.state_dir.join("checkpoints.sqlite3")
    }

    /// Returns the authentication state path.
    #[must_use]
    pub fn auth_path(&self) -> PathBuf {
        self.state_dir.join("auth.json")
    }

    /// Returns the owner-only Cloudflare connector-token path.
    #[must_use]
    pub fn cloudflare_token_path(&self) -> PathBuf {
        self.state_dir.join(CLOUDFLARE_TOKEN_FILE)
    }

    fn at(state_dir: PathBuf) -> Self {
        let path = state_dir.join(CONFIG_FILE);
        Self { state_dir, path }
    }

    fn save_with_mode(&self, config: &GatewayConfig, create_new: bool) -> Result<()> {
        self.validate_config(config)?;
        let config = toml::to_string_pretty(config).map_err(|error| {
            Error::Config(format!("cannot encode gateway configuration: {error}"))
        })?;
        let contents = config;
        if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
            return Err(Error::Config("gateway configuration is too large".into()));
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.state_dir)?;
        #[cfg(unix)]
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(contents.as_bytes())?;
        file.as_file().sync_all()?;
        if create_new {
            file.persist_noclobber(&self.path)
                .map_err(|error| error.error)?;
        } else {
            file.persist(&self.path).map_err(|error| error.error)?;
        }
        Ok(())
    }

    fn validate_config(&self, config: &GatewayConfig) -> Result<()> {
        config.validate()?;
        if matches!(
            config.cloudflare.as_ref(),
            Some(CloudflareConfig::Named { .. })
        ) {
            load_cloudflare_token(&self.cloudflare_token_path())?;
        }
        Ok(())
    }

    fn save_cloudflare_token(&self, token: &str) -> Result<()> {
        let token = validate_cloudflare_token(token)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.state_dir)?;
        #[cfg(unix)]
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(token.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist_noclobber(self.cloudflare_token_path())
            .map_err(|error| error.error)?;
        Ok(())
    }
}

impl CredentialStore {
    /// Opens credential state, treating a missing file as an empty store.
    pub fn open(path: PathBuf) -> Result<Self> {
        let values = match fs::read(&path) {
            Ok(contents) => {
                if contents.len() > MAX_CREDENTIAL_STATE_BYTES {
                    return Err(Error::Config(
                        "provider credential state is too large".into(),
                    ));
                }
                serde_json::from_slice(&contents)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        validate_credential_state(&values)?;
        Ok(Self {
            path,
            values: Mutex::new(values),
        })
    }

    /// Atomically replaces one instance's API key after provider and size validation.
    pub fn set(
        &self,
        instance: &str,
        provider_id: &str,
        api_key: &str,
        base_url: Option<&str>,
    ) -> Result<()> {
        let credential = StoredCredential {
            provider: provider_id.into(),
            api_key: api_key.into(),
            base_url: base_url.map(str::to_owned),
        };
        validate_stored_credential(instance, &credential)?;
        let mut values = self
            .values
            .lock()
            .map_err(|_| Error::Config("provider credential lock is poisoned".into()))?;
        if let Some(credential) = values.get(instance)
            && credential.provider != provider_id
        {
            return Err(Error::Config(format!(
                "provider instance `{instance}` already belongs to `{}`",
                credential.provider
            )));
        }
        let mut next = values.clone();
        next.insert(instance.into(), credential);
        save_private_map(&self.path, &next)?;
        *values = next;
        Ok(())
    }

    /// Resolves one instance's credential for model assembly without exposing it to clients.
    pub fn get(
        &self,
        instance: &str,
        provider_id: &str,
        base_url: Option<&str>,
    ) -> Result<Option<String>> {
        let values = self
            .values
            .lock()
            .map_err(|_| Error::Config("provider credential lock is poisoned".into()))?;
        Ok(values
            .get(instance)
            .filter(|credential| {
                credential.provider == provider_id && credential.base_url.as_deref() == base_url
            })
            .map(|credential| credential.api_key.clone()))
    }

    /// Atomically removes one instance-scoped API-key credential.
    pub fn remove(&self, instance: &str) -> Result<bool> {
        super::validation::validate_instance_id(instance)?;
        let mut values = self
            .values
            .lock()
            .map_err(|_| Error::Config("provider credential lock is poisoned".into()))?;
        if !values.contains_key(instance) {
            return Ok(false);
        }
        let mut next = values.clone();
        next.remove(instance);
        save_private_map(&self.path, &next)?;
        *values = next;
        Ok(true)
    }
}

/// Resolves the gateway state directory from the environment or home directory.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MOBIUS_GATEWAY_STATE_DIR") {
        if path.is_empty() {
            return Err(Error::Config("MOBIUS_GATEWAY_STATE_DIR is empty".into()));
        }
        return Ok(path.into());
    }
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join(".mobius").join("gateway"))
        .ok_or_else(|| {
            Error::Config(
                "cannot determine the home directory; set MOBIUS_GATEWAY_STATE_DIR".into(),
            )
        })
}

/// Loads a connector token from an owner-only regular file without exposing its contents.
pub fn load_cloudflare_token(path: &Path) -> Result<String> {
    #[cfg(unix)]
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| -> Error {
            if error.raw_os_error() == Some(nix::libc::ELOOP) {
                invalid_cloudflare_token_file()
            } else {
                error.into()
            }
        })?;
    #[cfg(not(unix))]
    let file = {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(invalid_cloudflare_token_file());
        }
        fs::File::open(path)?
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(invalid_cloudflare_token_file());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::Config(
            "Cloudflare tunnel token file must not be accessible by group or others (use mode 0600)"
                .into(),
        ));
    }
    if metadata.len() > MAX_CLOUDFLARE_TOKEN_BYTES as u64 {
        return Err(invalid_cloudflare_token());
    }
    let mut contents = String::new();
    file.take(MAX_CLOUDFLARE_TOKEN_BYTES as u64 + 1)
        .read_to_string(&mut contents)?;
    let token = validate_cloudflare_token(&contents)?;
    Ok(token.to_owned())
}

fn prepare_state_dir(path: PathBuf) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::Config("gateway state directory must have a name".into()))?
        .to_owned();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let path = fs::canonicalize(parent)?.join(name);
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::Config(
                "gateway state directory already exists".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

fn validate_private_state_dir(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(Error::Config(
            "gateway state path must be a directory".into(),
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::Config(
            "gateway state directory must not be accessible by group or others (use mode 0700)"
                .into(),
        ));
    }
    Ok(())
}

fn validate_stored_credential(instance: &str, credential: &StoredCredential) -> Result<()> {
    super::validation::validate_instance_id(instance)?;
    let definition = provider(&credential.provider)?;
    if !matches!(definition.auth(), ProviderAuth::ApiKey(_)) {
        return Err(Error::Config(format!(
            "provider `{}` does not accept an API key",
            credential.provider
        )));
    }
    if credential.api_key.trim().is_empty() || credential.api_key.len() > MAX_API_KEY_BYTES {
        return Err(Error::Config(format!(
            "API key must be 1–{MAX_API_KEY_BYTES} bytes"
        )));
    }
    definition.validate_base_url(credential.base_url.as_deref())?;
    Ok(())
}

fn validate_credential_state(values: &BTreeMap<String, StoredCredential>) -> Result<()> {
    for (instance, credential) in values {
        validate_stored_credential(instance, credential)?;
    }
    Ok(())
}

fn save_private_map(path: &Path, values: &BTreeMap<String, StoredCredential>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("provider credential path has no parent".into()))?;
    validate_credential_state(values)?;
    let contents = serde_json::to_vec(values)?;
    if contents.len() > MAX_CREDENTIAL_STATE_BYTES {
        return Err(Error::Config(
            "provider credential state is too large".into(),
        ));
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&contents)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

impl UsageHistory {
    pub(super) fn observe(
        &mut self,
        provider: &str,
        usage: &TokenUsage,
        now: SystemTime,
    ) -> Result<bool> {
        validate_usage_provider(provider)?;
        validate_usage(usage)?;
        if usage == &TokenUsage::default() {
            return Ok(false);
        }
        let day = unix_day(now)?;
        let mut bucket = self
            .days
            .get(&day)
            .and_then(|providers| providers.get(provider))
            .cloned()
            .unwrap_or_default();
        bucket
            .checked_add(usage)
            .ok_or_else(|| Error::Config("daily token usage overflow".into()))?;
        self.days
            .entry(day)
            .or_default()
            .insert(provider.into(), bucket);
        let first_day = day.saturating_sub(USAGE_HISTORY_DAYS - 1);
        self.days.retain(|stored, _| *stored >= first_day);
        Ok(true)
    }
}

pub(super) fn unix_day(now: SystemTime) -> Result<u64> {
    Ok(now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Config("system clock is before the Unix epoch".into()))?
        .as_secs()
        / SECONDS_PER_DAY)
}

pub(super) fn validate_usage(usage: &TokenUsage) -> Result<()> {
    if !usage_nonnegative(usage) {
        return Err(Error::Config("token usage cannot be negative".into()));
    }
    Ok(())
}

pub(super) fn validate_usage_provider(provider: &str) -> Result<()> {
    if provider.trim().is_empty()
        || provider != provider.trim()
        || provider.len() > 256
        || provider.chars().any(char::is_control)
    {
        return Err(Error::Config(
            "usage provider ID must be canonical and 1–256 bytes".into(),
        ));
    }
    Ok(())
}

fn usage_nonnegative(usage: &TokenUsage) -> bool {
    usage.input_tokens >= 0
        && usage.cached_input_tokens >= 0
        && usage.cache_write_input_tokens >= 0
        && usage.output_tokens >= 0
        && usage.reasoning_output_tokens >= 0
        && usage.total_tokens >= 0
}
