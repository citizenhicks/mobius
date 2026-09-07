//! Command-line entrypoint shared by the gateway and CLI packages.

mod args;
mod connection;
mod init;
mod lifecycle;
mod provider;

use std::ffi::OsString;
#[cfg(any(unix, test))]
use std::fs::{self, File, OpenOptions, TryLockError};
#[cfg(any(unix, test))]
use std::io::Write;
#[cfg(any(unix, test))]
use std::io::{Read as _, Seek as _, SeekFrom};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use crate::auth::PairingStatus;
use crate::auth::{AuthStore, PairingGrant};
use crate::client::{Endpoint, GatewayClient, MAX_PENDING_FRAMES};
use crate::cloudflare::CloudflareTunnel;
use crate::config::{
    CloudflareConfig, ConfigStore, DEFAULT_LISTEN, GatewayConfig, TlsConfig, load_cloudflare_token,
    state_dir,
};
use crate::server::GatewayServer;
use crate::wire::{ClientKind, ClientMessage, ServerMessage};
use crate::{Error, Result};
#[cfg(unix)]
use nix::sys::signal::{Signal, kill};
#[cfg(unix)]
use nix::unistd::Pid;
#[cfg(any(unix, test))]
use serde::Deserialize;
use serde::Serialize;
#[cfg(unix)]
use tokio::process::{Child, Command as TokioCommand};
#[cfg(unix)]
use tokio::signal::unix::{Signal as TokioSignal, SignalKind, signal};
use uuid::Uuid;

#[cfg(test)]
use self::args::parse;
use self::args::{
    CloudflareInit, Command, ConnectOptions, InitOptions, RegisterProviderOptions, parse_cli,
};
pub use self::args::{FrontendCommand, GatewayCli};
use self::connection::*;
use self::init::*;
pub use self::init::{
    initialize_named_cloudflare, initialize_quick_cloudflare, reset_gateway_state,
};
pub use self::lifecycle::ensure_background_gateway;
use self::lifecycle::*;
use self::provider::*;

#[cfg(unix)]
/// Terminates a process group created by gateway autostart.
pub fn terminate_process_group(pid: u32) {
    if pid <= 1 {
        return;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let Some(group) = pid.checked_neg() else {
        return;
    };
    let _ = kill(Pid::from_raw(group), Signal::SIGKILL);
}

#[cfg(any(unix, test))]
const PROCESS_FILE: &str = "gateway-process.json";
#[cfg(unix)]
const STARTUP_FILE: &str = "gateway-start.lock";
#[cfg(unix)]
const STATE_MARKER_FILE: &str = "gateway.toml";
#[cfg(any(unix, test))]
const MAX_PROCESS_RECORD_BYTES: usize = 4 * 1024;
#[cfg(unix)]
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(unix)]
const BACKGROUND_START_TIMEOUT: Duration = Duration::from_secs(40);
#[cfg(unix)]
const BACKGROUND_START_POLL_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(unix)]
const MAX_BACKGROUND_ERROR_BYTES: u64 = 16 * 1024;
#[cfg(unix)]
const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Runs a gateway command with arguments excluding the executable name.
pub async fn run(
    arguments: Vec<OsString>,
    save_local_client: fn(&Endpoint, String) -> Result<()>,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let cli = match parse_cli(arguments) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(());
        }
        Err(error) => return Err(Error::Config(error.to_string())),
    };
    run_cli(cli, save_local_client, load_local_client).await
}

/// Runs an already parsed gateway command.
pub async fn run_cli(
    cli: GatewayCli,
    save_local_client: fn(&Endpoint, String) -> Result<()>,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    match cli.into_command()? {
        Command::Init(options) => initialize(options),
        Command::Bootstrap { state_dir } => initialize_bootstrap(state_dir, save_local_client),
        Command::ResetBotDefaults { state_dir } => reset_bot_defaults(state_dir),
        Command::PairingCode { state_dir } => pairing_code(state_dir, load_local_client).await,
        Command::RegisterProvider(options) => {
            register_provider_command(options, load_local_client).await
        }
        Command::Connect(options) => connect(options, load_local_client).await,
        Command::Serve {
            state_dir,
            background,
        } => {
            if background {
                serve_in_background(state_dir).await
            } else {
                serve(state_dir, true, save_local_client).await
            }
        }
        Command::ServeChild { state_dir } => serve(state_dir, false, save_local_client).await,
        Command::Exit { state_dir } => exit_gateway(state_dir),
    }
}

#[cfg(test)]
mod tests;
