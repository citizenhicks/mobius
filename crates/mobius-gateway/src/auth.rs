//! One-time pairing and independent bearer-token authentication.

mod config;
pub use config::AuthConfig;

use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use uuid::Uuid;

use crate::{Error, Result};

/// Maximum UTF-8 byte length of one gateway client bearer credential.
pub const MAX_CLIENT_CREDENTIAL_BYTES: usize = 512;
const MAX_CLIENT_LABEL_BYTES: usize = 128;
const MAX_AUTH_STATE_BYTES: u64 = 2 * 1024 * 1024;
const REVOKED_PAIRING_EXPIRY: i64 = 0;
const LOCAL_CLIENT_ID: &str = "00000000-0000-0000-0000-000000000001";
const LOCAL_CLIENT_LABEL: &str = "Local möbius CLI";

pub(crate) fn is_local_operator(id: &str) -> bool {
    id == LOCAL_CLIENT_ID
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthState {
    pending_pairing: Option<PendingPairing>,
    clients: Vec<ClientToken>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingPairing {
    digest: [u8; 32],
    expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientToken {
    id: String,
    label: String,
    digest: [u8; 32],
    created_at: i64,
}

impl ClientToken {
    fn identity(&self) -> ClientIdentity {
        ClientIdentity {
            id: self.id.clone(),       // rules-allow: the identity outlives the state lock
            label: self.label.clone(), // rules-allow: the identity outlives the state lock
        }
    }
}

/// Identity returned after a successful authentication handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
}

/// A newly issued bearer token. Only the digest is persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedToken {
    /// The client identifier.
    pub client_id: String,
    /// The token.
    pub token: String,
}

/// One pending code that may be consumed by exactly one client pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingGrant {
    /// The code.
    pub code: String,
    /// The expires at.
    pub expires_at: i64,
}

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingStatus {
    Pending,
    Consumed,
    Replaced,
}

/// File-backed authentication state shared by all accepted connections.
pub struct AuthStore {
    path: PathBuf,
    state: Mutex<AuthState>,
    channel_identity: crate::channel::Identity,
    config: AuthConfig,
}

impl std::fmt::Debug for AuthStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthStore")
            .field("path", &self.path)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AuthStore {
    /// Initializes authentication with the required operator pairing policy.
    /// # Errors
    /// Returns an error for invalid policy or inaccessible authentication state.
    pub fn initialize(
        path: impl Into<PathBuf>,
        config: AuthConfig,
    ) -> Result<(Self, PairingGrant)> {
        config.validate()?;
        Self::initialize_validated(path, config)
    }

    /// Uses pairing policy already checked with its gateway configuration.
    pub(crate) fn initialize_validated(
        path: impl Into<PathBuf>,
        config: AuthConfig,
    ) -> Result<(Self, PairingGrant)> {
        let path = path.into();
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| Error::Config("authentication path has no parent".into()))?,
        )?;
        let channel_identity = crate::channel::Identity::open(&path)?;
        let grant = new_pairing_grant(&channel_identity, config.pairing_lifetime_seconds)?;
        let state = AuthState {
            pending_pairing: Some(PendingPairing {
                digest: digest(&grant.code),
                expires_at: grant.expires_at,
            }),
            clients: Vec::new(),
        };
        save_auth_state(&path, &state, true)?;
        Ok((
            Self {
                path,
                state: Mutex::new(state),
                channel_identity,
                config,
            },
            grant,
        ))
    }

    /// Opens authentication with the required operator pairing policy.
    /// Existing clients remain valid when the configured limit is reduced.
    /// # Errors
    /// Returns an error for invalid policy, corrupt state, or inaccessible files.
    pub fn open(path: impl Into<PathBuf>, config: AuthConfig) -> Result<Self> {
        config.validate()?;
        Self::open_validated(path, config)
    }

    /// Uses pairing policy already checked with its gateway configuration.
    pub(crate) fn open_validated(path: impl Into<PathBuf>, config: AuthConfig) -> Result<Self> {
        let path = path.into();
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
        let file = options.open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::Config(
                "authentication state must be a regular file".into(),
            ));
        }
        if metadata.len() > MAX_AUTH_STATE_BYTES {
            return Err(Error::Config("authentication state is too large".into()));
        }
        let mut contents = Vec::new();
        file.take(MAX_AUTH_STATE_BYTES + 1)
            .read_to_end(&mut contents)?;
        if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_AUTH_STATE_BYTES {
            return Err(Error::Config("authentication state is too large".into()));
        }
        let state: AuthState = serde_json::from_slice(&contents)?;
        validate_auth_state(&state)?;
        let channel_identity = crate::channel::Identity::open(&path)?;
        Ok(Self {
            path,
            state: Mutex::new(state),
            channel_identity,
            config,
        })
    }

    /// Consumes the pending code and appends an independently issued client token.
    /// # Errors
    ///
    /// Returns an error if authentication fails or its stored state is invalid.
    pub fn pair(&self, code: &str, client_label: &str) -> Result<IssuedToken> {
        self.pair_at(code, client_label, chrono::Utc::now().timestamp())
    }

    fn pair_at(&self, code: &str, client_label: &str, now: i64) -> Result<IssuedToken> {
        validate_client_label(client_label)?;
        let mut state = self.lock_state()?;
        let Some(pending) = &state.pending_pairing else {
            return Err(Error::Unauthorized);
        };
        if pending.expires_at <= now || !credential_matches(code, &pending.digest) {
            return Err(Error::Unauthorized);
        }
        if state.clients.len() >= self.config.paired_clients {
            return Err(Error::Config("paired client limit reached".into()));
        }

        let token = self.channel_identity.credential(&random_secret(2));
        let pending = state.pending_pairing.take();
        state.clients.push(ClientToken {
            id: pairing_client_id(code),
            label: client_label.into(),
            digest: digest(&token),
            created_at: now,
        });
        self.publish(&mut state, |state| {
            state.clients.pop();
            state.pending_pairing = pending;
        })?;
        drop(state);
        Ok(IssuedToken {
            client_id: pairing_client_id(code),
            token,
        })
    }

    /// Consumes the pending code and rotates one existing client token in place.
    /// # Errors
    ///
    /// Returns an error if authentication fails or its stored state is invalid.
    pub fn repair_pairing(
        &self,
        code: &str,
        replacing_token_digest: &[u8; 32],
        client_label: &str,
    ) -> Result<IssuedToken> {
        let now = chrono::Utc::now().timestamp();
        validate_client_label(client_label)?;
        let mut state = self.lock_state()?;
        let Some(pending) = &state.pending_pairing else {
            return Err(Error::Unauthorized);
        };
        if pending.expires_at <= now || !credential_matches(code, &pending.digest) {
            return Err(Error::Unauthorized);
        }
        let mut matched = None;
        for (index, client) in state.clients.iter().enumerate() {
            if bool::from(client.digest.ct_eq(replacing_token_digest)) {
                matched = Some(index);
            }
        }
        let Some(client) = matched.and_then(|index| state.clients.get_mut(index)) else {
            return Err(Error::Unauthorized);
        };

        let token = self.channel_identity.credential(&random_secret(2));
        let client_id = client.id.clone(); // rules-allow: the issued token outlives the state lock
        let previous = (
            std::mem::replace(&mut client.label, client_label.into()),
            std::mem::replace(&mut client.digest, digest(&token)),
            std::mem::replace(&mut client.created_at, now),
        );
        let pending = state.pending_pairing.take();
        self.publish(&mut state, |state| {
            state.pending_pairing = pending;
            if let Some(client) = state
                .clients
                .iter_mut()
                .find(|client| client.id == client_id)
            {
                (client.label, client.digest, client.created_at) = previous;
            }
        })?;
        drop(state);
        Ok(IssuedToken { client_id, token })
    }

    pub(crate) fn provision_local_client(&self) -> Result<IssuedToken> {
        let token = self.channel_identity.credential(&random_secret(2));
        let now = chrono::Utc::now().timestamp();
        let mut state = self.lock_state()?;
        if let Some(client) = state
            .clients
            .iter_mut()
            .find(|client| client.id == LOCAL_CLIENT_ID)
        {
            let previous = (
                std::mem::replace(&mut client.digest, digest(&token)),
                std::mem::replace(&mut client.created_at, now),
            );
            self.publish(&mut state, |state| {
                if let Some(client) = state
                    .clients
                    .iter_mut()
                    .find(|client| client.id == LOCAL_CLIENT_ID)
                {
                    (client.digest, client.created_at) = previous;
                }
            })?;
        } else {
            if state.clients.len() >= self.config.paired_clients {
                return Err(Error::Config("paired client limit reached".into()));
            }
            state.clients.push(ClientToken {
                id: LOCAL_CLIENT_ID.into(),
                label: LOCAL_CLIENT_LABEL.into(),
                digest: digest(&token),
                created_at: now,
            });
            self.publish(&mut state, |state| {
                state.clients.pop();
            })?;
        }
        drop(state);
        Ok(IssuedToken {
            client_id: LOCAL_CLIENT_ID.into(),
            token,
        })
    }

    /// Replaces any unused pairing code without invalidating paired clients.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn create_pairing_code(&self) -> Result<PairingGrant> {
        let mut state = self.lock_state()?;
        let grant =
            new_pairing_grant(&self.channel_identity, self.config.pairing_lifetime_seconds)?;
        let pending = state.pending_pairing.replace(PendingPairing {
            digest: digest(&grant.code),
            expires_at: grant.expires_at,
        });
        self.publish(&mut state, |state| state.pending_pairing = pending)?;
        drop(state);
        Ok(grant)
    }

    /// Verifies a bearer token against every paired client digest.
    /// # Errors
    ///
    /// Returns an error if authentication fails or its stored state is invalid.
    pub fn authenticate(&self, token: &str) -> Result<ClientIdentity> {
        if token.is_empty() || token.len() > MAX_CLIENT_CREDENTIAL_BYTES {
            return Err(Error::Unauthorized);
        }
        let candidate = digest(token);
        let state = self.lock_state()?;
        let mut matched = None;
        for client in &state.clients {
            if bool::from(candidate.ct_eq(&client.digest)) {
                matched = Some(client);
            }
        }
        let identity = matched.map(ClientToken::identity);
        drop(state);
        identity.ok_or(Error::Unauthorized)
    }

    pub(crate) fn clients(&self) -> Result<Vec<ClientIdentity>> {
        Ok(self
            .lock_state()?
            .clients
            .iter()
            .map(ClientToken::identity)
            .collect())
    }

    pub(crate) fn unpair_client(&self, actor_id: &str, client_id: &str) -> Result<bool> {
        if actor_id == client_id {
            return Ok(false);
        }
        let mut state = self.lock_state()?;
        if !state.clients.iter().any(|client| client.id == actor_id) {
            return Ok(false);
        }
        let Some(index) = state
            .clients
            .iter()
            .position(|client| client.id == client_id)
        else {
            return Ok(false);
        };
        let removed = state.clients.remove(index);
        self.publish(&mut state, |state| state.clients.insert(index, removed))?;
        drop(state);
        Ok(true)
    }

    #[cfg(any(unix, test))]
    pub(crate) fn pairing_status(&self, code: &str) -> Result<PairingStatus> {
        let state = self.lock_state()?;
        if state
            .clients
            .iter()
            .any(|client| client.id == pairing_client_id(code))
        {
            return Ok(PairingStatus::Consumed);
        }
        Ok(match &state.pending_pairing {
            Some(pending) if credential_matches(code, &pending.digest) => PairingStatus::Pending,
            Some(_) | None => PairingStatus::Replaced,
        })
    }

    #[cfg(any(unix, test))]
    pub(crate) fn revoke_pairing_code(&self, code: &str) -> Result<()> {
        let mut state = self.lock_state()?;
        let Some(pending) = &state.pending_pairing else {
            return Ok(());
        };
        if !credential_matches(code, &pending.digest) {
            return Ok(());
        }
        let pending = state.pending_pairing.replace(PendingPairing {
            digest: digest(&random_secret(1)),
            expires_at: REVOKED_PAIRING_EXPIRY,
        });
        self.publish(&mut state, |state| state.pending_pairing = pending)?;
        drop(state);
        Ok(())
    }

    /// Persists the already-updated state, undoing the update when nothing was written.
    fn publish(&self, state: &mut AuthState, undo: impl FnOnce(&mut AuthState)) -> Result<()> {
        match crate::publication::Outcome::applied(save_auth_state(&self.path, state, false)) {
            Ok(publication) => publication.confirm(),
            Err(error) => {
                undo(state);
                Err(error)
            }
        }
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, AuthState>> {
        self.state
            .lock()
            .map_err(crate::poisoned("authentication state"))
    }

    pub(crate) fn channel_handshake(&self) -> Result<snow::HandshakeState> {
        self.channel_identity.responder()
    }
}

fn validate_client_label(label: &str) -> Result<()> {
    if label.is_empty()
        || label != label.trim()
        || label.len() > MAX_CLIENT_LABEL_BYTES
        || label.chars().any(char::is_control)
    {
        return Err(Error::Config(format!(
            "client label must be canonical, control-free, and 1–{MAX_CLIENT_LABEL_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_auth_state(state: &AuthState) -> Result<()> {
    if state.clients.len() > crate::config::MAX_CAPACITY {
        return Err(Error::Config(
            "authentication state exceeds the client limit".into(),
        ));
    }
    if state.pending_pairing.is_none() && state.clients.is_empty() {
        return Err(Error::Config(
            "authentication state has neither pairing nor client access".into(),
        ));
    }
    let mut client_ids = BTreeSet::new();
    let mut token_digests = BTreeSet::new();
    for client in &state.clients {
        let id = Uuid::parse_str(&client.id).map_err(|error| {
            Error::Config(format!("authentication client ID is invalid: {error}"))
        })?;
        if id.to_string() != client.id {
            return Err(Error::Config(
                "authentication client ID must be canonical".into(),
            ));
        }
        if !client_ids.insert(client.id.as_str()) {
            return Err(Error::Config(
                "authentication state contains duplicate client IDs".into(),
            ));
        }
        if !token_digests.insert(client.digest) {
            return Err(Error::Config(
                "authentication state contains duplicate token digests".into(),
            ));
        }
        validate_client_label(&client.label)?;
    }
    Ok(())
}

fn credential_matches(candidate: &str, expected: &[u8; 32]) -> bool {
    if candidate.is_empty() || candidate.len() > MAX_CLIENT_CREDENTIAL_BYTES {
        return false;
    }
    bool::from(digest(candidate).ct_eq(expected))
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn pairing_client_id(code: &str) -> String {
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest(code)[..16]);
    Uuid::from_bytes(bytes).to_string()
}

fn new_pairing_grant(
    identity: &crate::channel::Identity,
    lifetime_seconds: u64,
) -> Result<PairingGrant> {
    Ok(PairingGrant {
        code: identity.credential(&random_secret(1)),
        expires_at: chrono::Utc::now()
            .timestamp()
            .checked_add(i64::try_from(lifetime_seconds).map_err(|error| {
                Error::Config(format!("pairing lifetime is too large: {error}"))
            })?)
            .ok_or_else(|| Error::Config("pairing expiry overflow".into()))?,
    })
}

fn random_secret(parts: usize) -> String {
    (0..parts)
        .map(|_| Uuid::new_v4().simple().to_string())
        .collect()
}

fn save_auth_state(path: &Path, state: &AuthState, create_new: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("authentication path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    validate_auth_state(state)?;
    let contents = serde_json::to_vec_pretty(state)?;
    crate::publication::publish(path, &contents, create_new)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn persisted_auth_rejects_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_AUTH_STATE_BYTES + 1)
            .unwrap();
        let Err(error) = AuthStore::open(path, AuthConfig::default()) else {
            panic!("oversized auth must fail");
        };
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn pairing_rejects_the_code_at_its_exact_expiry() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("auth.json");
        let (auth, grant) =
            AuthStore::initialize(path, AuthConfig::default()).expect("initialize auth");

        assert!(matches!(
            auth.pair_at(&grant.code, "Mac", grant.expires_at),
            Err(Error::Unauthorized)
        ));
        auth.pair_at(&grant.code, "Mac", grant.expires_at - 1)
            .expect("code is valid before expiry");
    }

    #[test]
    fn unpairing_revokes_the_token_and_blocks_the_stale_client() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("auth.json");
        let (auth, first_code) =
            AuthStore::initialize(&path, AuthConfig::default()).expect("initialize auth");
        let first = auth.pair(&first_code.code, "Mac").expect("pair Mac");
        let second_code = auth.create_pairing_code().expect("second code");
        let second = auth.pair(&second_code.code, "iPhone").expect("pair iPhone");
        let third_code = auth.create_pairing_code().expect("third code");
        let third = auth.pair(&third_code.code, "CLI").expect("pair CLI");

        let removed = auth
            .unpair_client(&first.client_id, &second.client_id)
            .expect("unpair iPhone");
        let stale_removal = auth
            .unpair_client(&second.client_id, &third.client_id)
            .expect("reject stale client");
        let reopened = AuthStore::open(path, AuthConfig::default()).expect("reopen auth");

        assert_eq!(
            (
                removed,
                stale_removal,
                reopened.authenticate(&second.token).is_err(),
                reopened.authenticate(&first.token).is_ok(),
                reopened.authenticate(&third.token).is_ok(),
            ),
            (true, false, true, true, true)
        );
    }

    #[test]
    fn repairing_at_the_client_limit_rotates_the_existing_client() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("auth.json");
        let (auth, mut grant) =
            AuthStore::initialize(path, AuthConfig::default()).expect("initialize auth");
        let capacity = AuthConfig::default().paired_clients;
        let mut first = None;
        for index in 0..capacity {
            let issued = auth
                .pair(&grant.code, &format!("client {index}"))
                .expect("pair client");
            first.get_or_insert(issued);
            if index + 1 < capacity {
                grant = auth.create_pairing_code().expect("next code");
            }
        }
        let first = first.expect("first client");
        let replacement = auth.create_pairing_code().expect("repair code");

        let error = auth
            .pair(&replacement.code, "new client")
            .expect_err("client limit must reject a new client");
        let repaired = auth
            .repair_pairing(&replacement.code, &digest(&first.token), "repaired client")
            .expect("repair existing client");

        assert!(error.to_string().contains("client limit"));
        assert_eq!(repaired.client_id, first.client_id);
        assert_eq!(auth.clients().expect("clients").len(), capacity);
        assert!(auth.authenticate(&first.token).is_err());
        assert!(auth.authenticate(&repaired.token).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn auth_state_is_owner_only() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("auth.json");
        let invalid = AuthConfig {
            paired_clients: 0,
            ..AuthConfig::default()
        };
        assert!(AuthStore::initialize(&path, invalid).is_err());
        assert!(!path.exists());
        AuthStore::initialize(&path, AuthConfig::default()).expect("initialize auth");
        assert!(AuthStore::open(&path, invalid).is_err());

        let mode = fs::metadata(path)
            .expect("auth metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600);
    }
}
