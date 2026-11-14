//! Command parsing and execution for the terminal client.

use std::env;
use std::fs::File;
use std::io::IsTerminal as _;
use std::ops::ControlFlow;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::frontend::{self, FrontendExit, request, wait_ready as wait_gateway_ready};
use crate::gateway_error;
use mobius::backend::session_files::SessionFileSelection;
use mobius::protocol::MAX_MESSAGE_BYTES;
use mobius::{Error, Result};
use mobius_gateway::client::{Endpoint, GatewayClient, GatewayEvents, GatewaySender};
use mobius_gateway::command::BackgroundGatewayOptions;
use mobius_gateway::config::state_dir;
use mobius_gateway::gateway_accounts::{
    GatewayAccounts, configured_endpoint, configured_token, missing_local_token,
    validate_local_gateway_config,
};
use mobius_gateway::wire::{
    BotRecord, ClientCapabilities, ClientMessage, ReadyPayload, ServerMessage,
    SessionActivityState, SessionReadyPayload, SessionRecord,
};
use tokio::io::AsyncReadExt as _;

/// Parsed `mobius` command line.
#[derive(Debug, Parser)]
#[command(
    name = "mobius",
    version,
    propagate_version = true,
    about = "Terminal client for a möbius gateway"
)]
pub struct Cli {
    /// Command to run; omit it to open the terminal interface.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Commands supported by the `mobius` terminal client.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one task without opening the terminal interface.
    Run {
        /// Bot handle or stable identifier.
        bot: String,

        /// UTF-8 file containing the task prompt.
        task_file: PathBuf,
    },

    /// Pair this client with a gateway.
    Pair {
        /// Gateway endpoint, such as `wss://gateway.example.com`.
        endpoint: Endpoint,

        /// Single-use pairing code issued by the gateway.
        one_time_code: String,
    },

    /// Open the extension manager.
    Extensions,

    /// Reopen a saved chat by its ID.
    Resume {
        /// The chat ID printed when the terminal interface closes.
        chat: mobius::SessionId,
    },
}

/// Characters of a chat's name the exit hint shows before eliding the rest.
const RESUME_HINT_NAME_CHARS: usize = 60;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_LOCAL_ENDPOINT: &str = "tcp://127.0.0.1:8741";

impl Cli {
    /// Executes the parsed command, including gateway startup and terminal lifecycle.
    ///
    /// # Errors
    /// Returns input, gateway connection, authentication, or frontend failures.
    pub async fn run(self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        match self.command {
            None => run_interactive(None).await?,
            Some(Command::Resume { chat }) => run_interactive(Some(chat)).await?,
            Some(Command::Run { bot, task_file }) => run_task(&bot, &task_file).await?,
            Some(Command::Pair {
                endpoint,
                one_time_code,
            }) => pair(endpoint, &one_time_code).await?,
            Some(Command::Extensions) => {
                let connected = connect_gateway().await?;
                frontend::run_extensions(connected.sender, connected.events, connected.gateway)
                    .await?;
            }
        }
        Ok(())
    }
}

async fn run_interactive(resume: Option<mobius::SessionId>) -> Result<()> {
    let Some(mut chat) = connect(None, None).await? else {
        return Ok(());
    };
    if let Some(session_id) = resume {
        let bootstrap = chat.session.session.session_id.share();
        chat.session = open_session(
            &chat.sender,
            &mut chat.events,
            &mut chat.gateway,
            session_id,
        )
        .await?;
        discard_pristine_session(
            &chat.sender,
            &mut chat.events,
            &mut chat.gateway,
            &bootstrap,
        )
        .await?;
        chat.disposable_session = None;
    }
    let mut draft = None;
    loop {
        let session_id = &chat.session.session.session_id;
        let choose_initial_bot = chat.disposable_session.as_ref() == Some(session_id);
        let (exit, sender, events) = frontend::run(
            chat.sender,
            chat.events,
            &mut chat.gateway,
            &mut chat.session,
            chat.endpoint.to_string(),
            frontend::Opening {
                choose_bot: choose_initial_bot,
                draft: draft.take(),
            },
        )
        .await?;
        chat.sender = sender;
        chat.events = events;
        match exit {
            FrontendExit::Exit => {
                let session_id = chat.session.session.session_id.share();
                refresh_sessions(&chat.sender, &mut chat.events, &mut chat.gateway).await?;
                if let Some(session) = chat
                    .gateway
                    .sessions
                    .iter()
                    .find(|session| session.session_id == session_id)
                    .filter(|session| !pristine_session(session))
                {
                    print_resume_hint(session);
                }
                return Ok(());
            }
            FrontendExit::Resume(session_id) => {
                if session_id != chat.session.session.session_id {
                    let current = chat.session.session.session_id.share();
                    discard_pristine_session(
                        &chat.sender,
                        &mut chat.events,
                        &mut chat.gateway,
                        &current,
                    )
                    .await?;
                }
                chat.session = open_session(
                    &chat.sender,
                    &mut chat.events,
                    &mut chat.gateway,
                    session_id,
                )
                .await?;
                chat.disposable_session = None;
            }
            FrontendExit::Reload => {
                if let Some(bootstrap) = chat.disposable_session.take()
                    && bootstrap != chat.session.session.session_id
                {
                    discard_pristine_session(
                        &chat.sender,
                        &mut chat.events,
                        &mut chat.gateway,
                        &bootstrap,
                    )
                    .await?;
                }
            }
            FrontendExit::Reconnect { draft: unsent } => {
                draft = unsent;
                let selected = Some((chat.endpoint, chat.session.session.session_id.share()));
                let Some(connection) = connect(selected, None).await? else {
                    return Ok(());
                };
                chat = connection;
            }
            FrontendExit::Fresh => {
                let Some(connection) = connect(None, None).await? else {
                    return Ok(());
                };
                chat = connection;
            }
        }
    }
}

async fn run_task(bot_handle: &str, task_file: &Path) -> Result<()> {
    let task = read_task_file(task_file).await?;
    let Some(Chat {
        sender,
        mut events,
        mut gateway,
        session,
        disposable_session,
        ..
    }) = connect(None, Some(bot_handle)).await?
    else {
        return Ok(());
    };
    if gateway.agent_catalog.models.is_empty() {
        if let Some(session_id) = disposable_session.as_ref() {
            discard_session(&sender, &mut events, &mut gateway, session_id).await?;
        }
        return Err(Error::Config(
            "configure the selected Bot's provider before using `mobius run`".into(),
        ));
    }
    if let Some(message) =
        frontend::run_headless(sender, events, session.session.session_id.share(), task).await?
    {
        print_output(&message);
    }
    Ok(())
}

async fn read_task_file(path: &Path) -> Result<String> {
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(Error::Config("task file exceeds message size limit".into()));
    }
    String::from_utf8(bytes)
        .map_err(|error| Error::Config(format!("task file must contain valid UTF-8: {error}")))
}

#[expect(
    clippy::print_stdout,
    reason = "the terminal interface has closed, so the hint goes to the shell it returns to"
)]
fn print_resume_hint(session: &SessionRecord) {
    let first_line = session
        .title
        .as_deref()
        .or(session.first_user_message.as_deref())
        .and_then(|name| name.lines().next())
        .unwrap_or("chat");
    let mut name: String = first_line.chars().take(RESUME_HINT_NAME_CHARS).collect();
    if name.len() < first_line.len() {
        name.push('…');
    }
    println!("{name} · reopen with: mobius resume {}", session.session_id);
}

#[expect(
    clippy::print_stdout,
    reason = "printing the task result is the command's output"
)]
fn print_output(value: &str) {
    println!("{}", output_text(value, std::io::stdout().is_terminal()));
}

fn output_text(value: &str, terminal: bool) -> String {
    if terminal {
        frontend::terminal_text(value)
    } else {
        value.into()
    }
}

#[expect(
    clippy::print_stdout,
    reason = "reporting the paired client is the command's output"
)]
async fn pair(endpoint: Endpoint, code: &str) -> std::result::Result<(), mobius_gateway::Error> {
    let endpoint = crate::authenticated_endpoint(endpoint)?;
    GatewayAccounts::load()?.prepare()?;
    let (_client, paired) = GatewayClient::pair(
        &endpoint,
        code,
        "mobius-cli",
        ClientCapabilities {
            presence: true,
            ..ClientCapabilities::default()
        },
    )
    .await?;
    let mut accounts = GatewayAccounts::load()?;
    accounts.add(&endpoint, paired.token)?;
    accounts.save()?;
    println!("paired {} · token saved", paired.client_id);
    Ok(())
}

/// An open chat on a connected gateway.
struct Chat {
    sender: GatewaySender,
    events: GatewayEvents,
    gateway: ReadyPayload,
    session: SessionReadyPayload,
    /// The chat this launch created, discarded again if it stays pristine.
    disposable_session: Option<mobius::SessionId>,
    endpoint: Endpoint,
}

/// A gateway connection whose initial catalog has arrived.
struct Connected {
    sender: GatewaySender,
    events: GatewayEvents,
    gateway: ReadyPayload,
    /// Whether the gateway is reached over plaintext on this machine.
    local: bool,
    endpoint: Endpoint,
}

async fn connect(
    selected: Option<(Endpoint, mobius::SessionId)>,
    preferred_bot: Option<&str>,
) -> Result<Option<Chat>> {
    let Connected {
        sender,
        mut events,
        mut gateway,
        local: local_gateway,
        endpoint,
    } = connect_gateway().await?;
    let (session, disposable_session) = match selected.filter(|(previous, _)| previous == &endpoint)
    {
        Some((_, session_id)) => {
            let session = open_session(&sender, &mut events, &mut gateway, session_id).await?;
            (session, None)
        }
        None if local_gateway => {
            if preferred_bot.is_none()
                && gateway.agent_catalog.bot_defaults.is_none()
                && !frontend::run_gateway_login(&sender, &mut events, &mut gateway).await?
            {
                return Ok(None);
            }
            let bot_id = preferred_bot.map_or_else(
                || resolve_bot(&gateway.bots, "@mobius"),
                |preferred| resolve_bot(&gateway.bots, preferred),
            )?;
            let session = create_session(
                &sender,
                &mut events,
                &mut gateway,
                env::current_dir()?,
                bot_id,
            )
            .await?;
            let disposable_session = Some(session.session.session_id.share());
            (session, disposable_session)
        }
        None if preferred_bot.is_some() => {
            return Err(Error::Config(
                "`mobius run` requires a local gateway workspace".into(),
            ));
        }
        None => {
            let first_session_id = gateway
                .sessions
                .first()
                .map(|session| session.session_id.share());
            if let Some(session_id) = first_session_id {
                let session = open_session(&sender, &mut events, &mut gateway, session_id).await?;
                (session, None)
            } else {
                let session_id = gateway
                    .bots
                    .iter()
                    .find(|bot| &*bot.handle == "mobius")
                    .or_else(|| gateway.bots.first())
                    .map(|bot| bot.conversation_session_id.share())
                    .ok_or_else(|| Error::Config("create a Bot before starting a chat".into()))?;
                let session = open_session(&sender, &mut events, &mut gateway, session_id).await?;
                (session, None)
            }
        }
    };
    Ok(Some(Chat {
        sender,
        events,
        gateway,
        session,
        disposable_session,
        endpoint,
    }))
}

async fn connect_gateway() -> Result<Connected> {
    let endpoint = crate::authenticated_endpoint(configured_endpoint().map_err(gateway_error)?)
        .map_err(gateway_error)?;
    // ponytail: TLS gateways skip local `@` scanning; use a gateway-backed inventory if needed.
    let local_gateway = endpoint.is_plaintext();
    let token = configured_token(&endpoint).map_err(gateway_error)?;
    if automatically_manage_local_gateway(&endpoint)
        && let Some(token) = token.as_deref()
    {
        match connect_local_once(&endpoint, token).await {
            Ok(client) => {
                let (sender, mut events) = client.into_parts();
                let gateway = wait_gateway_ready(&mut events).await?;
                if gateway.gateway_version == env!("CARGO_PKG_VERSION") {
                    return Ok(Connected {
                        sender,
                        events,
                        gateway,
                        local: local_gateway,
                        endpoint,
                    });
                }
                // The locked launcher already owns semantic version checks and upgrades.
            }
            Err(mobius_gateway::Error::Unauthorized) => {
                return Err(gateway_error(missing_local_token(&endpoint)));
            }
            Err(mobius_gateway::Error::Io(error))
                if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Err(mobius_gateway::Error::Protocol(_)) => {}
            Err(error) => return Err(gateway_error(error)),
        }
    }
    let connected = if automatically_manage_local_gateway(&endpoint) {
        start_local_gateway(&endpoint).await
    } else {
        match token {
            Some(token) => {
                GatewayClient::connect(
                    &endpoint,
                    &token,
                    ClientCapabilities {
                        presence: true,
                        ..ClientCapabilities::default()
                    },
                )
                .await
            }
            None => Err(missing_token(&endpoint)),
        }
    };
    let client = connected.map_err(gateway_error)?;
    let (sender, mut events) = client.into_parts();
    let gateway = wait_gateway_ready(&mut events).await?;
    Ok(Connected {
        sender,
        events,
        gateway,
        local: local_gateway,
        endpoint,
    })
}

fn automatically_manage_local_gateway(endpoint: &Endpoint) -> bool {
    endpoint.is_plaintext()
        && env::var_os("MOBIUS_GATEWAY_ENDPOINT").is_none()
        && env::var_os("MOBIUS_GATEWAY_TOKEN").is_none()
}

async fn connect_local_once(
    endpoint: &Endpoint,
    token: &str,
) -> mobius_gateway::Result<GatewayClient> {
    tokio::time::timeout(
        CONNECT_TIMEOUT,
        GatewayClient::connect(
            endpoint,
            token,
            ClientCapabilities {
                presence: true,
                ..ClientCapabilities::default()
            },
        ),
    )
    .await
    .map_err(|elapsed| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("gateway connection timed out: {elapsed}"),
        )
    })?
}

#[expect(
    clippy::print_stderr,
    reason = "the startup status line is shown on stderr before any terminal UI exists"
)]
async fn start_local_gateway(endpoint: &Endpoint) -> mobius_gateway::Result<GatewayClient> {
    let configured_state_dir = state_dir()?;
    let _startup_lock = lock_local_gateway_startup(&configured_state_dir)?;
    let saved_token = configured_token(endpoint)?;
    let binary = gateway_binary()?;
    let configured = configured_state_dir.try_exists()?;
    if configured {
        validate_local_gateway_config(&configured_state_dir, endpoint, saved_token.is_some())?;
    } else if endpoint.to_string() != DEFAULT_LOCAL_ENDPOINT {
        return Err(mobius_gateway::Error::Config(format!(
            "saved local gateway {endpoint} is stopped; start it separately before reconnecting"
        )));
    }
    let show_status = std::io::stderr().is_terminal();
    if show_status {
        eprint!("Starting local gateway at {endpoint}…");
    }
    let connected = async {
        if configured {
            start_gateway(endpoint, binary, &configured_state_dir).await
        } else {
            bootstrap_local_gateway(endpoint, binary, &configured_state_dir).await
        }
    }
    .await;
    if show_status {
        eprint!("\r\x1b[2K");
    }
    connected
}

fn lock_local_gateway_startup(state_dir: &Path) -> mobius_gateway::Result<File> {
    let mut name = state_dir
        .file_name()
        .ok_or_else(|| {
            mobius_gateway::Error::Config("gateway state directory has no file name".into())
        })?
        .to_os_string();
    name.push(".startup.lock");
    let path = state_dir.with_file_name(name);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = mobius::owner_only::open(&path)?;
    file.lock()?;
    Ok(file)
}

async fn bootstrap_local_gateway(
    endpoint: &Endpoint,
    binary: PathBuf,
    state_dir: &Path,
) -> mobius_gateway::Result<GatewayClient> {
    mobius_gateway::command::initialize_quick_cloudflare(state_dir.to_path_buf())?;
    match start_gateway(endpoint, binary, state_dir).await {
        Ok(client) => Ok(client),
        Err(error) => {
            if let Err(cleanup) = cleanup_failed_bootstrap(endpoint, state_dir) {
                return Err(mobius_gateway::Error::Config(format!(
                    "{error}; failed to clean up incomplete gateway state: {cleanup}"
                )));
            }
            Err(error)
        }
    }
}

fn cleanup_failed_bootstrap(endpoint: &Endpoint, state_dir: &Path) -> mobius_gateway::Result<()> {
    let state = mobius_gateway::command::reset_gateway_state(state_dir.to_path_buf());
    let client = (|| {
        let mut accounts = GatewayAccounts::load()?;
        if accounts.token(endpoint).is_some() {
            accounts.forget(&endpoint.to_string());
            accounts.save()?;
        }
        Ok(())
    })();
    match (state, client) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(state), Err(client)) => Err(mobius_gateway::Error::Config(format!(
            "{state}; failed to forget the local gateway credential: {client}"
        ))),
    }
}

fn gateway_binary() -> mobius_gateway::Result<PathBuf> {
    gateway_binary_beside(&env::current_exe()?)
}

fn gateway_binary_beside(current_executable: &Path) -> mobius_gateway::Result<PathBuf> {
    let name = if cfg!(windows) {
        "mobius-gateway.exe"
    } else {
        "mobius-gateway"
    };
    let candidate = current_executable.with_file_name(name);
    let metadata = std::fs::metadata(&candidate).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            mobius_gateway::Error::Config(
                "install mobius-cli to provide mobius-gateway beside mobius (`cargo install --locked mobius-cli`)"
                    .into(),
            )
        } else {
            error.into()
        }
    })?;
    if !metadata.is_file() {
        return Err(mobius_gateway::Error::Config(
            "the mobius-gateway path is not a file".into(),
        ));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(mobius_gateway::Error::Config(
            "the mobius-gateway binary is not executable".into(),
        ));
    }
    Ok(std::fs::canonicalize(candidate)?)
}

/// Starts the detached gateway in this process, then connects to it.
async fn start_gateway(
    endpoint: &Endpoint,
    binary: PathBuf,
    state_dir: &Path,
) -> mobius_gateway::Result<GatewayClient> {
    mobius_gateway::command::ensure_background_gateway_with(
        state_dir.to_path_buf(),
        configured_token,
        BackgroundGatewayOptions {
            binary: Some(binary),
            quiet: true,
        },
    )
    .await?;
    let token = configured_token(endpoint)?.ok_or_else(|| missing_local_token(endpoint))?;
    match connect_local_once(endpoint, &token).await {
        Err(mobius_gateway::Error::Unauthorized) => Err(missing_local_token(endpoint)),
        result => result,
    }
}

async fn open_session(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session_id: mobius::SessionId,
) -> Result<SessionReadyPayload> {
    request(
        sender,
        events,
        Some(gateway),
        |request_id| ClientMessage::OpenSession {
            request_id,
            session_id,
            last_sequence: None,
        },
        session_opened,
    )
    .await
}

async fn create_session(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    workspace: PathBuf,
    bot_id: Arc<str>,
) -> Result<SessionReadyPayload> {
    request(
        sender,
        events,
        Some(gateway),
        |request_id| ClientMessage::CreateSession {
            request_id,
            workspace,
            bot_id,
        },
        session_opened,
    )
    .await
}

fn session_opened(
    request_id: &str,
    message: ServerMessage,
) -> ControlFlow<SessionReadyPayload, ServerMessage> {
    match message {
        ServerMessage::SessionOpened {
            request_id: actual,
            payload,
        } if *actual == *request_id => ControlFlow::Break(payload),
        message => ControlFlow::Continue(message),
    }
}

fn resolve_bot(bots: &[BotRecord], preferred: &str) -> Result<Arc<str>> {
    let preferred = preferred.trim().trim_start_matches('@');
    bots.iter()
        .find(|bot| *bot.handle == *preferred || *bot.id == *preferred)
        .map(|bot| Arc::clone(&bot.id))
        .ok_or_else(|| Error::Config(format!("Bot `@{preferred}` was not found")))
}

async fn discard_session(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session_id: &mobius::SessionId,
) -> Result<()> {
    request(
        sender,
        events,
        Some(gateway),
        |request_id| ClientMessage::DeleteSessions {
            request_id,
            session_ids: vec![session_id.share()],
            selection: SessionFileSelection::All,
        },
        |request_id, message| match message {
            ServerMessage::Accepted { request_id: actual } if *actual == *request_id => {
                ControlFlow::Break(())
            }
            message => ControlFlow::Continue(message),
        },
    )
    .await
}

async fn discard_pristine_session(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session_id: &mobius::SessionId,
) -> Result<()> {
    refresh_sessions(sender, events, gateway).await?;
    if catalog_session_is_pristine(gateway, session_id) {
        discard_session(sender, events, gateway, session_id).await?;
    }
    Ok(())
}

async fn refresh_sessions(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    let sessions = request(
        sender,
        events,
        Some(gateway),
        |request_id| ClientMessage::ListSessions { request_id },
        |request_id, message| match message {
            ServerMessage::Sessions {
                request_id: Some(actual),
                sessions,
            } if *actual == *request_id => ControlFlow::Break(sessions),
            message => ControlFlow::Continue(message),
        },
    )
    .await?;
    gateway.sessions = sessions;
    Ok(())
}

fn catalog_session_is_pristine(gateway: &ReadyPayload, session_id: &mobius::SessionId) -> bool {
    gateway
        .sessions
        .iter()
        .find(|session| session.session_id == *session_id)
        .is_some_and(pristine_session)
}

fn pristine_session(session: &SessionRecord) -> bool {
    session.parent_session_id.is_none()
        && session.first_user_message.is_none()
        && session.execution_stats.run_count == 0
        && session.title.is_none()
        && !session.pinned
        && session.session_context.origin_label.as_deref() == Some("mobius-gateway")
        && session.activity.state == SessionActivityState::Idle
}

fn missing_token(endpoint: &Endpoint) -> mobius_gateway::Error {
    mobius_gateway::Error::Config(format!("pair mobius-cli with {endpoint} before connecting"))
}

#[cfg(test)]
mod tests {

    use super::*;

    #[tokio::test]
    async fn task_files_are_bounded_before_gateway_startup() {
        let directory = tempfile::tempdir().expect("task directory");
        let path = directory.path().join("task.md");
        std::fs::write(&path, vec![b'x'; MAX_MESSAGE_BYTES]).expect("write task");
        assert_eq!(
            read_task_file(&path)
                .await
                .expect("read bounded task")
                .len(),
            MAX_MESSAGE_BYTES
        );

        std::fs::write(&path, vec![b'x'; MAX_MESSAGE_BYTES + 1]).expect("write oversized task");
        assert!(read_task_file(&path).await.is_err());

        std::fs::write(&path, [0xff]).expect("write invalid UTF-8 task");
        assert!(read_task_file(&path).await.is_err());
    }

    #[test]
    fn local_gateway_startup_lock_allows_one_of_three_contenders() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let state_dir = directory.path().join("gateway");
        let lock_path = directory.path().join("gateway.startup.lock");
        let first = lock_local_gateway_startup(&state_dir).expect("first startup lock");
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("second contender");
        let third = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("third contender");

        assert!(
            matches!(second.try_lock(), Err(std::fs::TryLockError::WouldBlock))
                && matches!(third.try_lock(), Err(std::fs::TryLockError::WouldBlock))
        );

        drop(first);
        second.lock().expect("released startup lock");
        drop(second);
        drop(third);
        assert!(lock_path.exists(), "startup lock must remain persistent");
        assert_eq!(
            std::fs::metadata(lock_path)
                .expect("startup lock metadata")
                .permissions()
                .mode()
                & 0o077,
            0,
            "startup lock must be owner-only"
        );
    }

    #[test]
    fn gateway_autostart_ignores_path_and_requires_a_sibling_binary() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let error = gateway_binary_beside(&directory.path().join("mobius"))
            .expect_err("missing sibling gateway must fail");

        assert!(
            error
                .to_string()
                .contains("cargo install --locked mobius-cli")
        );
    }

    #[test]
    fn stdout_filters_terminal_controls_but_preserves_piped_output() {
        let routine_output = "task: reset\u{1b}[2J.md";

        assert_eq!(output_text(routine_output, true), "task: reset[2J.md");
        assert_eq!(output_text(routine_output, false), routine_output);
    }
}
