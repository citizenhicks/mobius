//! ChatGPT OAuth login, credential persistence, and request authorization.

use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use uuid::Uuid;

use super::super::openai_auth::OpenAiAuthorization;
use super::super::openai_auth::ResolvedAuthorization;
use super::super::provider::BrowserAuth;
use super::super::provider::BrowserLogin;
use super::super::provider::DeviceLogin;
use super::super::provider::ProviderCredential;
use super::PROVIDER_ID;
use super::manifest;
use crate::BoxFuture;
use crate::Error;
use crate::Result;

// ChatGPT feature-gates Codex models and transports by this wire-client version.
// Audited against openai/codex rust-v0.153.4 (Astra requires at least 0.153.0).
const CODEX_COMPAT_VERSION: &str = "0.153.4";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const SCOPE: &str = "openid profile email offline_access";
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const CALLBACK_LIMIT: usize = 16 * 1024;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CALLBACK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_LEEWAY: Duration = Duration::from_secs(60);
const TOKEN_LIMIT: usize = 64 * 1024;

pub(super) struct ChatGptAuth {
    path: PathBuf,
    client: Client,
    credential: Mutex<OAuthCredential>,
}

struct ChatGptLogin {
    listener: TcpListener,
    verifier: String,
    state: String,
    url: String,
    client: Client,
}

struct ChatGptDeviceLogin {
    user_code: String,
    device_auth_id: String,
    interval: Duration,
    client: Client,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthCredential {
    access: String,
    refresh: String,
    expires: u64,
    account_id: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

#[derive(Serialize)]
struct DeviceUserCodeRequest {
    client_id: &'static str,
}

#[derive(Deserialize)]
struct DeviceUserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    interval: String,
}

#[derive(Serialize)]
struct DeviceTokenRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_challenge: String,
    code_verifier: String,
}

type AuthFile = BTreeMap<String, OAuthCredential>;

impl ChatGptAuth {
    fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let credential = read_credential(&path)?;
        Ok(Self {
            path,
            client: http_client()?,
            credential: Mutex::new(credential),
        })
    }

    fn configured(path: &Path) -> Result<bool> {
        match read_credential(path) {
            Ok(_) => Ok(true),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn authorization(&self) -> Result<(String, String)> {
        self.resolve_authorization(None).await
    }

    async fn resolve_authorization(
        &self,
        rejected_token: Option<&str>,
    ) -> Result<(String, String)> {
        let mut credential = self.credential.lock().await;
        if !refresh_required(&credential, rejected_token) {
            return Ok(resolved(&credential));
        }

        let path = self.path.clone();
        let (lock, saved) = tokio::task::spawn_blocking(move || {
            let lock = acquire_lock(&path)?;
            let saved = read_credential(&path)?;
            Ok::<_, Error>((lock, saved))
        })
        .await
        .map_err(|error| Error::Auth(format!("credential lock task failed: {error}")))??;
        if !refresh_required(&saved, rejected_token) {
            *credential = saved;
            return Ok(resolved(&credential));
        }

        let refreshed = refresh_token(&self.client, &saved.refresh).await?;
        let path = self.path.clone();
        let saved = refreshed.clone();
        tokio::task::spawn_blocking(move || write_credential(&path, &saved))
            .await
            .map_err(|error| Error::Auth(format!("credential save task failed: {error}")))??;
        *credential = refreshed;
        drop(lock);
        Ok(resolved(&credential))
    }
}

impl ChatGptLogin {
    async fn start() -> Result<Self> {
        let (verifier, challenge) = pkce();
        let state = Uuid::new_v4().simple().to_string();
        let listener = TcpListener::bind(("127.0.0.1", 1455))
            .await
            .map_err(|error| Error::Auth(format!("cannot listen on localhost:1455: {error}")))?;
        let mut url = reqwest::Url::parse(AUTHORIZE_URL)
            .map_err(|error| Error::Auth(format!("invalid authorization URL: {error}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", "mobius");
        Ok(Self {
            listener,
            verifier,
            state,
            url: url.into(),
            client: http_client()?,
        })
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn open_browser(&self) {
        let _ = open_browser(&self.url);
    }

    async fn finish(self, path: PathBuf) -> Result<()> {
        let code = timeout(
            CALLBACK_TIMEOUT,
            wait_for_callback(self.listener, &self.state),
        )
        .await
        .map_err(|_| Error::Auth("ChatGPT login timed out".into()))??;
        let credential = exchange_code(&self.client, &code, &self.verifier, REDIRECT_URI).await?;
        save_login_credential(path, credential).await
    }
}

impl ChatGptDeviceLogin {
    async fn start() -> Result<Self> {
        let client = http_client()?;
        let response = client
            .post(DEVICE_USER_CODE_URL)
            .json(&DeviceUserCodeRequest {
                client_id: CLIENT_ID,
            })
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Auth(format!(
                "ChatGPT device-code login is unavailable (HTTP {status})"
            )));
        }
        let response: DeviceUserCodeResponse = bounded_json(response, "device-code").await?;
        if response.device_auth_id.trim().is_empty() || response.user_code.trim().is_empty() {
            return Err(Error::Auth(
                "ChatGPT device-code response omitted its code".into(),
            ));
        }
        let interval = response
            .interval
            .trim()
            .parse::<u64>()
            .map_err(|_| Error::Auth("ChatGPT device-code interval was invalid".into()))?
            .clamp(1, 30);
        Ok(Self {
            user_code: response.user_code,
            device_auth_id: response.device_auth_id,
            interval: Duration::from_secs(interval),
            client,
        })
    }

    async fn finish(self, path: PathBuf) -> Result<()> {
        let started = Instant::now();
        let response = loop {
            let response = self
                .client
                .post(DEVICE_TOKEN_URL)
                .json(&DeviceTokenRequest {
                    device_auth_id: &self.device_auth_id,
                    user_code: &self.user_code,
                })
                .send()
                .await?;
            let status = response.status();
            if status.is_success() {
                break bounded_json::<DeviceTokenResponse>(response, "device-token").await?;
            }
            if !matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
                return Err(Error::Auth(format!(
                    "ChatGPT device-code login failed with HTTP {status}"
                )));
            }
            let remaining = CALLBACK_TIMEOUT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(Error::Auth("ChatGPT device-code login timed out".into()));
            }
            sleep(self.interval.min(remaining)).await;
        };
        if response.authorization_code.trim().is_empty()
            || response.code_challenge.trim().is_empty()
            || response.code_verifier.trim().is_empty()
        {
            return Err(Error::Auth(
                "ChatGPT device-token response omitted authorization data".into(),
            ));
        }
        let credential = exchange_code(
            &self.client,
            &response.authorization_code,
            &response.code_verifier,
            DEVICE_REDIRECT_URI,
        )
        .await?;
        save_login_credential(path, credential).await
    }
}

async fn bounded_json<T>(mut response: reqwest::Response, operation: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > TOKEN_LIMIT {
            return Err(Error::Auth(format!(
                "ChatGPT {operation} response was too large"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| Error::Auth(format!("ChatGPT {operation} response was invalid")))
}

async fn save_login_credential(path: PathBuf, credential: OAuthCredential) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let _lock = acquire_lock(&path)?;
        write_credential(&path, &credential)
    })
    .await
    .map_err(|error| Error::Auth(format!("credential lock task failed: {error}")))?
}

fn http_client() -> Result<Client> {
    Ok(Client::builder().timeout(REQUEST_TIMEOUT).build()?)
}

fn pkce() -> (String, String) {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let request =
            match read_callback_request_with_timeout(&mut stream, CALLBACK_REQUEST_TIMEOUT).await {
                Ok(request) => request,
                Err(error) => {
                    respond(&mut stream, "400 Bad Request", "Invalid callback request.").await?;
                    return Err(error);
                }
            };
        if request.path() != "/auth/callback" {
            respond(&mut stream, "404 Not Found", "Callback route not found.").await?;
            continue;
        }
        if request
            .query_pairs()
            .find(|(key, _)| key == "state")
            .is_none_or(|(_, value)| value != expected_state)
        {
            respond(&mut stream, "400 Bad Request", "State mismatch.").await?;
            continue;
        }
        if let Some(error) = request
            .query_pairs()
            .find_map(|(key, value)| (key == "error").then(|| value.into_owned()))
        {
            respond(
                &mut stream,
                "400 Bad Request",
                "Authorization was declined.",
            )
            .await?;
            return Err(Error::Auth(format!(
                "ChatGPT authorization failed: {error}"
            )));
        }
        let Some(code) = request
            .query_pairs()
            .find_map(|(key, value)| (key == "code").then(|| value.into_owned()))
            .filter(|value| !value.is_empty())
        else {
            respond(
                &mut stream,
                "400 Bad Request",
                "Missing authorization code.",
            )
            .await?;
            continue;
        };
        respond(
            &mut stream,
            "200 OK",
            "Authorization received. You can return to möbius.",
        )
        .await?;
        return Ok(code);
    }
}

async fn read_callback_request_with_timeout(
    stream: &mut TcpStream,
    duration: Duration,
) -> Result<reqwest::Url> {
    timeout(duration, read_callback_request(stream))
        .await
        .map_err(|_| Error::Auth("OAuth callback request timed out".into()))?
}

async fn read_callback_request(stream: &mut TcpStream) -> Result<reqwest::Url> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Err(Error::Auth("OAuth callback closed before a request".into()));
        }
        request.extend_from_slice(&buffer[..count]);
        if request.len() > CALLBACK_LIMIT {
            return Err(Error::Auth("OAuth callback request was too large".into()));
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&request)
        .map_err(|_| Error::Auth("OAuth callback was not valid UTF-8".into()))?;
    let mut parts = request
        .lines()
        .next()
        .ok_or_else(|| Error::Auth("OAuth callback omitted a request line".into()))?
        .split_whitespace();
    if parts.next() != Some("GET") {
        return Err(Error::Auth("OAuth callback must use GET".into()));
    }
    let target = parts
        .next()
        .ok_or_else(|| Error::Auth("OAuth callback omitted its target".into()))?;
    reqwest::Url::parse(&format!("http://localhost{target}"))
        .map_err(|_| Error::Auth("OAuth callback target was invalid".into()))
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) -> Result<()> {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>möbius</title>\
         <h1>möbius</h1><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Security-Policy: default-src 'none'\r\n\
         Cache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn exchange_code(
    client: &Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredential> {
    token_request(
        client,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ],
        "exchange",
    )
    .await
}

async fn refresh_token(client: &Client, refresh: &str) -> Result<OAuthCredential> {
    token_request(
        client,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT_ID),
        ],
        "refresh",
    )
    .await
}

async fn token_request(
    client: &Client,
    form: &[(&str, &str)],
    operation: &str,
) -> Result<OAuthCredential> {
    let mut response = client.post(TOKEN_URL).form(form).send().await?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > TOKEN_LIMIT {
            return Err(Error::Auth(format!(
                "ChatGPT token {operation} response was too large"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(Error::Auth(format!(
            "ChatGPT token {operation} failed with HTTP {status}"
        )));
    }
    let token: TokenResponse = serde_json::from_slice(&body)
        .map_err(|_| Error::Auth(format!("ChatGPT token {operation} response was invalid")))?;
    if token.access_token.is_empty() || token.refresh_token.is_empty() || token.expires_in == 0 {
        return Err(Error::Auth(format!(
            "ChatGPT token {operation} response omitted credentials"
        )));
    }
    let account_id = account_id(&token.access_token)?;
    Ok(OAuthCredential {
        access: token.access_token,
        refresh: token.refresh_token,
        expires: now().saturating_add(token.expires_in),
        account_id,
    })
}

fn account_id(access_token: &str) -> Result<String> {
    if access_token.len() > TOKEN_LIMIT {
        return Err(Error::Auth("ChatGPT access token was too large".into()));
    }
    let mut segments = access_token.split('.');
    let _header = segments.next();
    let payload = segments
        .next()
        .ok_or_else(|| Error::Auth("ChatGPT access token was not a JWT".into()))?;
    if segments.next().is_none() || segments.next().is_some() {
        return Err(Error::Auth("ChatGPT access token was not a JWT".into()));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| Error::Auth("ChatGPT access token payload was invalid".into()))?;
    let payload: Value = serde_json::from_slice(&payload)
        .map_err(|_| Error::Auth("ChatGPT access token payload was invalid".into()))?;
    payload
        .get(JWT_AUTH_CLAIM)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Auth("ChatGPT access token omitted its account ID".into()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn expired(credential: &OAuthCredential) -> bool {
    now().saturating_add(REFRESH_LEEWAY.as_secs()) >= credential.expires
}

fn refresh_required(credential: &OAuthCredential, rejected_token: Option<&str>) -> bool {
    rejected_token.map_or_else(|| expired(credential), |token| credential.access == token)
}

fn resolved(credential: &OAuthCredential) -> (String, String) {
    (credential.access.clone(), credential.account_id.clone())
}

fn read_credential(path: &Path) -> Result<OAuthCredential> {
    let contents = fs::read(path)?;
    let auth: AuthFile = serde_json::from_slice(&contents)
        .map_err(|_| Error::Auth(format!("{} is not a valid auth file", path.display())))?;
    let credential = auth
        .get(PROVIDER_ID)
        .cloned()
        .ok_or_else(|| Error::Auth("ChatGPT login is required".into()))?;
    validate_credential(credential)
}

fn validate_credential(credential: OAuthCredential) -> Result<OAuthCredential> {
    if credential.access.is_empty()
        || credential.refresh.is_empty()
        || credential.account_id.is_empty()
        || credential.expires == 0
    {
        return Err(Error::Auth(
            "saved ChatGPT credentials are incomplete; log in again".into(),
        ));
    }
    if account_id(&credential.access)? != credential.account_id {
        return Err(Error::Auth(
            "saved ChatGPT account ID does not match its access token; log in again".into(),
        ));
    }
    Ok(credential)
}

fn acquire_lock(path: &Path) -> Result<File> {
    secure_parent(path)?;
    let lock_path = path.with_file_name("auth.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    #[cfg(unix)]
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    lock.lock()?;
    Ok(lock)
}

fn write_credential(path: &Path, credential: &OAuthCredential) -> Result<()> {
    secure_parent(path)?;
    let mut auth = match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map_err(|_| Error::Auth(format!("{} is not a valid auth file", path.display())))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => AuthFile::new(),
        Err(error) => return Err(error.into()),
    };
    auth.insert(PROVIDER_ID.into(), credential.clone());
    let contents = serde_json::to_vec_pretty(&auth)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::Auth("auth path has no parent".into()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&contents)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn secure_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Auth("auth path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> io::Result<()> {
    Command::new("open").arg(url).spawn().map(|_| ())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> io::Result<()> {
    Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_browser(url: &str) -> io::Result<()> {
    Command::new("xdg-open").arg(url).spawn().map(|_| ())
}

#[cfg(not(any(unix, target_os = "windows")))]
fn open_browser(_url: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "automatic browser launch is unsupported",
    ))
}

impl OpenAiAuthorization for ChatGptAuth {
    fn authorize_http<'a>(
        &'a self,
        streaming: bool,
        session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization>> {
        Box::pin(async move {
            let (token, account_id) = self.authorization().await?;
            let mut headers = vec![
                ("chatgpt-account-id".into(), account_id),
                ("originator".into(), "mobius".into()),
                ("version".into(), CODEX_COMPAT_VERSION.into()),
                (
                    "user-agent".into(),
                    concat!("mobius/", env!("CARGO_PKG_VERSION")).into(),
                ),
            ];
            if let Some(session_id) = session_id {
                headers.push(("session-id".into(), session_id.into()));
                headers.push(("thread-id".into(), session_id.into()));
                if streaming {
                    headers.push(("x-client-request-id".into(), session_id.into()));
                }
            }
            Ok(ResolvedAuthorization { token, headers })
        })
    }

    fn authorize_websocket<'a>(
        &'a self,
        session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization>> {
        Box::pin(async move {
            let (token, account_id) = self.authorization().await?;
            Ok(ResolvedAuthorization {
                token,
                headers: vec![
                    ("chatgpt-account-id".into(), account_id),
                    ("originator".into(), "mobius".into()),
                    ("version".into(), CODEX_COMPAT_VERSION.into()),
                    (
                        "user-agent".into(),
                        concat!("mobius/", env!("CARGO_PKG_VERSION")).into(),
                    ),
                    (
                        "openai-beta".into(),
                        "responses_websockets=2026-02-06".into(),
                    ),
                    ("session-id".into(), session_id.into()),
                    ("thread-id".into(), session_id.into()),
                    ("x-client-request-id".into(), session_id.into()),
                ],
            })
        })
    }

    fn recover_unauthorized<'a>(&'a self, rejected_token: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.resolve_authorization(Some(rejected_token)).await?;
            Ok(true)
        })
    }
}

impl BrowserLogin for ChatGptLogin {
    fn url(&self) -> &str {
        self.url()
    }

    fn open_browser(&self) {
        self.open_browser();
    }

    fn complete(self: Box<Self>, path: PathBuf) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.finish(path).await })
    }
}

impl DeviceLogin for ChatGptDeviceLogin {
    fn verification_url(&self) -> &str {
        DEVICE_VERIFICATION_URL
    }

    fn user_code(&self) -> &str {
        &self.user_code
    }

    fn complete(self: Box<Self>, path: PathBuf) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.finish(path).await })
    }
}

pub(super) static BROWSER_AUTH: BrowserAuth = BrowserAuth::new(
    manifest::AUTH_LABEL,
    browser_configured,
    browser_credential,
    browser_login,
)
.with_device_login(device_login);

fn browser_configured(path: &Path) -> Result<bool> {
    ChatGptAuth::configured(path)
}

fn browser_credential(path: &Path) -> Result<ProviderCredential> {
    Ok(ProviderCredential::Browser(Arc::new(ChatGptAuth::load(
        path,
    )?)))
}

fn browser_login() -> BoxFuture<'static, Result<Box<dyn BrowserLogin>>> {
    Box::pin(async { Ok(Box::new(ChatGptLogin::start().await?) as Box<dyn BrowserLogin>) })
}

fn device_login() -> BoxFuture<'static, Result<Box<dyn DeviceLogin>>> {
    Box::pin(async { Ok(Box::new(ChatGptDeviceLogin::start().await?) as Box<dyn DeviceLogin>) })
}

#[cfg(test)]
#[path = "openai_codex_tests.rs"]
mod tests;
