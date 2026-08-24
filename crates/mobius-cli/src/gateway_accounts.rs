use std::collections::BTreeMap;
use std::env;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use mobius_gateway::client::{Endpoint, token_from_env};
use mobius_gateway::config::{ConfigStore, GatewayConfig};
use mobius_gateway::{Error, Result};
use serde::{Deserialize, Serialize};

const MAX_STORE_BYTES: usize = 64 * 1024;
const MAX_ACCOUNTS: usize = 64;
const MAX_TOKEN_BYTES: usize = 512;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TokenStoreRecord {
    selected_endpoint: Option<String>,
    tokens: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct GatewayAccounts {
    path: PathBuf,
    record: TokenStoreRecord,
}

impl GatewayAccounts {
    pub fn load() -> Result<Self> {
        Self::load_from(token_path()?)
    }

    pub fn endpoints(&self) -> impl ExactSizeIterator<Item = &str> {
        self.record.tokens.keys().map(String::as_str)
    }

    pub fn selected(&self) -> Option<&str> {
        self.record.selected_endpoint.as_deref()
    }

    pub fn token(&self, endpoint: &Endpoint) -> Option<&str> {
        self.record
            .tokens
            .get(&endpoint.to_string())
            .map(String::as_str)
    }

    pub fn select(&mut self, endpoint: &str) -> Result<()> {
        if !self.record.tokens.contains_key(endpoint) {
            return Err(Error::Config(format!(
                "gateway endpoint `{endpoint}` is not saved"
            )));
        }
        self.record.selected_endpoint = Some(endpoint.into());
        Ok(())
    }

    pub fn add(&mut self, endpoint: &Endpoint, token: String) -> Result<()> {
        validate_token(&token)?;
        let endpoint = endpoint.to_string();
        if !self.record.tokens.contains_key(&endpoint) && self.record.tokens.len() >= MAX_ACCOUNTS {
            return Err(Error::Config(
                "gateway token file has too many endpoints".into(),
            ));
        }
        self.record.tokens.insert(endpoint.clone(), token);
        self.record.selected_endpoint = Some(endpoint);
        Ok(())
    }

    pub fn forget(&mut self, endpoint: &str) {
        self.record.tokens.remove(endpoint);
        if self.selected() == Some(endpoint) {
            self.record.selected_endpoint = None;
        }
    }

    pub fn prepare(&self) -> Result<()> {
        let parent = parent(&self.path)?;
        std::fs::create_dir_all(parent)?;
        let file = tempfile::NamedTempFile::new_in(parent)?;
        secure(&file)?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        validate_record(&self.record)?;
        let contents = serde_json::to_vec(&self.record)?;
        if contents.len() > MAX_STORE_BYTES {
            return Err(Error::Config("gateway token file is too large".into()));
        }
        let parent = parent(&self.path)?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        secure(&file)?;
        file.write_all(&contents)?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }

    fn load_from(path: PathBuf) -> Result<Self> {
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    record: TokenStoreRecord::default(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Err(Error::Config("gateway token path is not a file".into()));
        }
        #[cfg(unix)]
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::Config(
                "gateway token file must be readable only by its owner".into(),
            ));
        }
        if metadata.len() > MAX_STORE_BYTES as u64 {
            return Err(Error::Config("gateway token file is too large".into()));
        }
        let record = serde_json::from_slice(&std::fs::read(&path)?).map_err(|_| {
            Error::Config(format!(
                "gateway token file has an unsupported format; delete {} and pair again",
                path.display()
            ))
        })?;
        validate_record(&record)?;
        Ok(Self { path, record })
    }
}

pub fn configured_endpoint() -> Result<Endpoint> {
    if environment_override_message().is_some() {
        return Endpoint::from_env();
    }
    GatewayAccounts::load()?
        .selected()
        .map_or_else(Endpoint::from_env, str::parse)
}

pub fn configured_token(endpoint: &Endpoint) -> Result<Option<String>> {
    if env::var_os("MOBIUS_GATEWAY_TOKEN").is_some() {
        return token_from_env().map(Some);
    }
    Ok(GatewayAccounts::load()?.token(endpoint).map(str::to_owned))
}

pub fn dashboard_gateway_endpoint(state_dir: &Path) -> Result<Endpoint> {
    let (_, config) = ConfigStore::open(state_dir.to_path_buf())?;
    if config.tls.is_some() {
        if env::var_os("MOBIUS_GATEWAY_ENDPOINT").is_none() {
            return Err(Error::Config(
                "TLS dashboards require MOBIUS_GATEWAY_ENDPOINT with the certificate hostname"
                    .into(),
            ));
        }
        return Endpoint::from_env();
    }
    endpoint_from_config(&config)
}

pub fn validate_local_gateway_config(
    state_dir: &Path,
    endpoint: &Endpoint,
    has_saved_token: bool,
) -> Result<()> {
    let (_, config) = ConfigStore::open(state_dir.to_path_buf())?;
    let configured_endpoint = endpoint_from_config(&config)?;
    if endpoint != &configured_endpoint {
        return Err(Error::Config(format!(
            "saved endpoint {endpoint} is not the local gateway configured at {configured_endpoint}; start it separately or select the configured endpoint"
        )));
    }
    if !has_saved_token && config.cloudflare.is_none() {
        return Err(missing_local_token(endpoint));
    }
    Ok(())
}

pub fn missing_local_token(endpoint: &Endpoint) -> Error {
    Error::Config(format!(
        "local gateway state exists but mobius-cli is not paired; stop the gateway, run `mobius-gateway connect` in another terminal, then run `mobius pair {endpoint} <one-time-code>`"
    ))
}

pub fn environment_override_message() -> Option<&'static str> {
    match (
        env::var_os("MOBIUS_GATEWAY_ENDPOINT").is_some(),
        env::var_os("MOBIUS_GATEWAY_TOKEN").is_some(),
    ) {
        (true, true) => Some(
            "Gateway selection is controlled by MOBIUS_GATEWAY_ENDPOINT and MOBIUS_GATEWAY_TOKEN. Unset them to manage saved gateways.",
        ),
        (true, false) => Some(
            "Gateway selection is controlled by MOBIUS_GATEWAY_ENDPOINT. Unset it to manage saved gateways.",
        ),
        (false, true) => Some(
            "Gateway selection is controlled by MOBIUS_GATEWAY_TOKEN. Unset it to manage saved gateways.",
        ),
        (false, false) => None,
    }
}

fn validate_record(record: &TokenStoreRecord) -> Result<()> {
    if record.tokens.len() > MAX_ACCOUNTS {
        return Err(Error::Config(
            "gateway token file has too many endpoints".into(),
        ));
    }
    for (endpoint, token) in &record.tokens {
        let parsed = endpoint.parse::<Endpoint>()?;
        if parsed.to_string() != *endpoint {
            return Err(Error::Config(
                "saved gateway endpoint is not canonical".into(),
            ));
        }
        validate_token(token)?;
    }
    if record
        .selected_endpoint
        .as_ref()
        .is_some_and(|endpoint| !record.tokens.contains_key(endpoint))
    {
        return Err(Error::Config(
            "selected gateway endpoint is not saved".into(),
        ));
    }
    Ok(())
}

fn endpoint_from_config(config: &GatewayConfig) -> Result<Endpoint> {
    format!(
        "{}://{}",
        if config.tls.is_some() { "tls" } else { "tcp" },
        config.listen
    )
    .parse()
}

fn validate_token(token: &str) -> Result<()> {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES || token.trim() != token {
        return Err(Error::Config("saved gateway token is invalid".into()));
    }
    Ok(())
}

fn token_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MOBIUS_GATEWAY_TOKEN_FILE") {
        return Ok(path.into());
    }
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|path| path.join(".mobius").join("gateway-tokens.json"))
        .ok_or_else(|| {
            Error::Config("cannot determine token path; set MOBIUS_GATEWAY_TOKEN_FILE".into())
        })
}

fn parent(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| Error::Config("token path has no parent".into()))
}

fn secure(file: &tempfile::NamedTempFile) -> Result<()> {
    #[cfg(unix)]
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts(path: &Path) -> GatewayAccounts {
        GatewayAccounts::load_from(path.to_path_buf()).expect("load accounts")
    }

    fn endpoint(value: &str) -> Endpoint {
        value.parse().expect("valid endpoint")
    }

    #[test]
    fn selecting_a_saved_account_updates_the_selected_endpoint() {
        let directory = tempfile::tempdir().expect("token directory");
        let mut accounts = accounts(&directory.path().join("tokens.json"));
        accounts
            .add(&endpoint("tcp://127.0.0.1:8741"), "local-token".into())
            .expect("local account");
        accounts
            .add(
                &endpoint("tls://gateway.example:443"),
                "remote-token".into(),
            )
            .expect("remote account");

        accounts
            .select("tcp://127.0.0.1:8741")
            .expect("select account");

        assert_eq!(accounts.selected(), Some("tcp://127.0.0.1:8741"));
    }

    #[test]
    fn forgetting_the_selected_account_clears_selection() {
        let directory = tempfile::tempdir().expect("token directory");
        let mut accounts = accounts(&directory.path().join("tokens.json"));
        accounts
            .add(&endpoint("tcp://127.0.0.1:8741"), "local-token".into())
            .expect("local account");

        accounts.forget("tcp://127.0.0.1:8741");

        assert_eq!(accounts.selected(), None);
    }

    #[test]
    fn account_record_round_trips_selection_and_tokens() {
        let directory = tempfile::tempdir().expect("token directory");
        let path = directory.path().join("tokens.json");
        let mut accounts = accounts(&path);
        let endpoint = endpoint("tls://gateway.example:443");
        accounts
            .add(&endpoint, "remote-token".into())
            .expect("remote account");
        accounts.save().expect("save accounts");

        let loaded = GatewayAccounts::load_from(path).expect("reload accounts");

        assert_eq!(
            (loaded.selected(), loaded.token(&endpoint)),
            (Some("tls://gateway.example:443"), Some("remote-token"))
        );
    }

    #[test]
    fn old_token_maps_fail_with_repair_guidance() {
        let directory = tempfile::tempdir().expect("token directory");
        let path = directory.path().join("tokens.json");
        std::fs::write(&path, r#"{"tcp://127.0.0.1:8741":"token"}"#).expect("legacy token map");
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("private permissions");

        let error = GatewayAccounts::load_from(path).expect_err("old format must fail");

        assert!(error.to_string().contains("delete"));
        assert!(error.to_string().contains("pair again"));
    }

    #[test]
    fn local_gateway_config_rejects_a_different_saved_endpoint() {
        let directory = tempfile::tempdir().expect("gateway state parent");
        let state = directory.path().join("gateway");
        mobius_gateway::command::initialize_quick_cloudflare(state.clone())
            .expect("initialize gateway");
        let endpoint = endpoint("tcp://127.0.0.1:9999");

        let error = validate_local_gateway_config(&state, &endpoint, true)
            .expect_err("mismatched endpoint must fail");

        assert!(error.to_string().contains("127.0.0.1:8741"));
    }
}
