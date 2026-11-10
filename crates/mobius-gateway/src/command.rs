//! Command-line entrypoint shared by the gateway and CLI packages.

mod args;
mod connection;
mod init;
mod lifecycle;
mod provider;
mod telemetry;

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::SeekFrom;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::PairingStatus;
use crate::auth::{AuthStore, PairingGrant};
use crate::client::{Endpoint, GatewayClient};
use crate::cloudflare::{CloudflareTunnel, PUBLIC_ENDPOINT_TIMEOUT};
use crate::config::{
    CloudflareConfig, ConfigStore, DEFAULT_LISTEN, GatewayConfig, TlsConfig, load_secret_file,
    state_dir,
};
use crate::server::GatewayServer;
use crate::wire::{ClientCapabilities, ClientMessage, ServerMessage};
use crate::{Error, Result};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::Deserialize;
use serde::Serialize;
use tokio::process::{Child, Command as TokioCommand};
use tokio::signal::unix::{Signal as TokioSignal, SignalKind, signal};
use uuid::Uuid;

#[cfg(test)]
use self::args::parse;
pub use self::args::{CloudflareInit, FrontendCommand, GatewayCli};
use self::args::{Command, ConnectOptions, InitOptions, RegisterProviderOptions, parse_cli};
use self::connection::{connect, ensure_gateway_stopped, pairing_code};
use self::init::{
    direct_loopback_endpoint, initialize, initialize_bootstrap, loopback_endpoint,
    provision_cloudflare_local_client, reset_bot_defaults, set_desktop,
};
pub use self::init::{
    initialize_named_cloudflare, initialize_quick_cloudflare, reset_gateway_state,
};
use self::lifecycle::{
    StartupGuard, exit_gateway, print_listener, reuse_current_gateway, running_process_pid,
    running_process_record, runtime_endpoint, serve, serve_in_background, shutdown_signal,
    start_background_gateway, stop_gateway,
};
pub use self::lifecycle::{ensure_background_gateway, startup_error};
use self::provider::register_provider_command;

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
    match kill(Pid::from_raw(group), Signal::SIGKILL) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(error) => tracing::warn!(%error, pid, "failed to terminate gateway process group"),
    }
}

/// Writes one line of command output to standard output.
fn print_line(line: std::fmt::Arguments<'_>) -> std::io::Result<()> {
    use std::io::Write as _;
    writeln!(std::io::stdout().lock(), "{line}")
}

/// Writes one line of command status to standard error.
fn eprint_line(line: std::fmt::Arguments<'_>) -> std::io::Result<()> {
    use std::io::Write as _;
    writeln!(std::io::stderr().lock(), "{line}")
}

const PROCESS_FILE: &str = "gateway-process.json";
const STARTUP_FILE: &str = "gateway-start.lock";
const STATE_MARKER_FILE: &str = "gateway.toml";
const MAX_PROCESS_RECORD_BYTES: usize = 4 * 1024;
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const BACKGROUND_START_TIMEOUT: Duration = Duration::from_secs(40);
const BACKGROUND_START_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_BACKGROUND_ERROR_BYTES: u64 = 16 * 1024;
const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Runs a gateway command with arguments excluding the executable name.
/// # Errors
///
/// Returns an error if validation or an operation required to complete the request fails.
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
/// # Errors
///
/// Returns an error if validation or an operation required to complete the request fails.
pub async fn run_cli(
    cli: GatewayCli,
    save_local_client: fn(&Endpoint, String) -> Result<()>,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    match cli.into_command()? {
        Command::PrintDefaultConfig => {
            let config =
                toml::to_string_pretty(&GatewayConfig::new(crate::config::DEFAULT_LISTEN, None)?)
                    .map_err(|error| {
                    Error::Config(format!(
                        "cannot encode default gateway configuration: {error}"
                    ))
                })?;
            Ok(print_line(format_args!("{config}"))?)
        }
        Command::CheckConfig { state_dir } => {
            ConfigStore::open(state_dir)?;
            Ok(print_line(format_args!("gateway configuration is valid"))?)
        }
        Command::ExportComputerResources { directory } => {
            crate::computer_runtime::export_resources(&directory)
        }
        Command::Telemetry { state_dir, command } => {
            telemetry::run(state_dir, command, load_local_client).await
        }
        Command::SetRuntime {
            state_dir,
            idle_exit_seconds,
            storage_limit_bytes,
            ingress,
            clear_ingress,
            clear_storage_limit,
        } => {
            let (store, mut config) = ConfigStore::open(state_dir)?;
            ensure_gateway_stopped(&store, &config)?;
            if let Some(seconds) = idle_exit_seconds {
                config.runtime.idle_exit_seconds = seconds;
            }
            if let Some(bytes) = storage_limit_bytes {
                config.runtime.storage_limit_bytes = Some(bytes);
            }
            if let Some(address) = ingress {
                config.runtime.ingress = Some(address);
            }
            if clear_ingress {
                config.runtime.ingress = None;
            }
            if clear_storage_limit {
                config.runtime.storage_limit_bytes = None;
            }
            store.save(&config.validated()?)
        }
        Command::Init(options) => initialize(options),
        Command::Bootstrap { state_dir } => initialize_bootstrap(state_dir, save_local_client),
        Command::ResetBotDefaults { state_dir } => reset_bot_defaults(state_dir),
        Command::SetDesktop { state_dir, enabled } => set_desktop(state_dir, enabled),
        Command::PairingCode { state_dir } => pairing_code(state_dir, load_local_client).await,
        Command::ClearProviderCredential {
            state_dir,
            instance,
        } => provider::clear_provider_credential(state_dir, instance, load_local_client).await,
        Command::RegisterProvider(options) => {
            register_provider_command(options, load_local_client).await
        }
        Command::Connect(options) => connect(options, load_local_client).await,
        Command::Serve {
            state_dir,
            background,
        } => {
            if background {
                serve_in_background(state_dir, load_local_client).await
            } else {
                serve(state_dir, true, save_local_client, load_local_client).await
            }
        }
        Command::ServeChild { state_dir } => {
            serve(state_dir, false, save_local_client, load_local_client).await
        }
        Command::Exit { state_dir } => exit_gateway(state_dir),
    }
}

#[cfg(test)]
mod tests;
