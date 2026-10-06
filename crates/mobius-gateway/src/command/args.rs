use clap::{ArgAction, Args, Parser, Subcommand};
use mobius::backend::model::provider::HostedWebSearch;

use super::*;

/// Parsed `mobius-gateway` command line.
#[derive(Debug, Parser)]
#[command(
    name = "mobius-gateway",
    version,
    propagate_version = true,
    about = "Run and configure a möbius gateway"
)]
pub struct GatewayCli {
    /// Directory containing gateway configuration and runtime state.
    #[arg(long, global = true, value_name = "PATH")]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<GatewaySubcommand>,
}

/// Frontend selected by a parsed gateway command line.
#[derive(Debug)]
pub enum FrontendCommand {
    /// Interactive Cloudflare initialization.
    Init(PathBuf),
    /// Gateway administration dashboard.
    Dashboard(PathBuf),
    /// Interactive provider setup.
    Provider(PathBuf),
}

#[derive(Debug, Subcommand)]
enum GatewaySubcommand {
    /// Print complete default gateway TOML without creating state.
    PrintDefaultConfig,
    /// Validate the current gateway TOML without starting services.
    CheckConfig,
    /// Export the matching computer worker and documentation for offline installs.
    ExportComputerResources {
        #[arg(long, value_name = "PATH")]
        directory: PathBuf,
    },
    /// Open the provider setup interface.
    Provider,
    /// Initialize gateway state.
    Init(InitArgs),
    /// Initialize a direct loopback gateway for machine use.
    Bootstrap,
    /// Restore the default Bot configuration.
    ResetBotDefaults,
    /// Enable or disable the gateway-owned desktop while the gateway is stopped.
    SetDesktop {
        /// Use true to enable the gateway-owned desktop or false to disable it.
        #[arg(long, action = ArgAction::Set, hide_possible_values = true)]
        enabled: bool,
    },
    /// Issue a one-time pairing code as JSON.
    PairingCode {
        /// Emit machine-readable JSON.
        #[arg(long, required = true)]
        json: bool,
    },
    /// Register a model provider non-interactively.
    RegisterProvider(RegisterProviderArgs),
    /// Revoke a stored credential and cancel operations that use it.
    ClearProviderCredential {
        #[arg(long)]
        instance: String,
    },
    /// Connect this installation to a running gateway.
    Connect(ConnectArgs),
    /// Run the gateway server.
    Serve(ServeArgs),
    #[command(name = "__serve", hide = true)]
    ServeChild,
    /// Manage outbound telemetry collectors.
    Telemetry {
        #[command(subcommand)]
        command: TelemetryCommand,
    },
    /// Set lifecycle policy while the gateway is stopped.
    SetRuntime {
        #[arg(long)]
        idle_exit_seconds: Option<u64>,
        /// Informational remote allowance, without local upload enforcement.
        #[arg(long)]
        storage_limit_bytes: Option<u64>,
        #[arg(long)]
        ingress: Option<SocketAddr>,
        #[arg(long, conflicts_with = "ingress")]
        clear_ingress: bool,
        #[arg(long, conflicts_with = "storage_limit_bytes")]
        clear_storage_limit: bool,
    },
    /// Stop a background gateway.
    Exit,
}

#[derive(Debug, Subcommand)]
pub(super) enum TelemetryCommand {
    /// Print delivery status and safe endpoint configuration as JSON.
    List,
    /// Upsert an endpoint by ID.
    Add {
        #[arg(long)]
        id: String,
        #[arg(long)]
        url: String,
        #[arg(long, default_value_t = 60)]
        every_seconds: u32,
        /// Require this collector's decision before accepting a user upload.
        #[arg(long)]
        upload_admission: bool,
        /// Snapshot sections: activity, usage, runs, storage, resources.
        #[arg(long, value_delimiter = ',')]
        sections: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        events: Vec<String>,
        #[arg(long)]
        bearer_env: Option<String>,
        #[arg(long)]
        bearer_file: Option<String>,
        #[arg(long = "field")]
        fields: Vec<String>,
    },
    /// Remove an endpoint by ID.
    Remove {
        #[arg(long)]
        id: String,
    },
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Address on which the gateway listens.
    #[arg(long, value_name = "ADDR")]
    listen: Option<SocketAddr>,

    /// PEM certificate for a direct TLS listener.
    #[arg(
        long = "tls-cert",
        value_name = "PATH",
        requires = "private_key",
        conflicts_with_all = ["cloudflare_hostname", "cloudflare_token_file"]
    )]
    certificate: Option<PathBuf>,

    /// PEM private key for a direct TLS listener.
    #[arg(
        long = "tls-key",
        value_name = "PATH",
        requires = "certificate",
        conflicts_with_all = ["cloudflare_hostname", "cloudflare_token_file"]
    )]
    private_key: Option<PathBuf>,

    /// Public hostname served by a named Cloudflare tunnel.
    #[arg(
        long,
        value_name = "HOST",
        requires = "cloudflare_token_file",
        conflicts_with_all = ["certificate", "private_key"]
    )]
    cloudflare_hostname: Option<String>,

    /// Owner-only file containing the named Cloudflare tunnel token.
    #[arg(
        long,
        value_name = "PATH",
        requires = "cloudflare_hostname",
        conflicts_with_all = ["certificate", "private_key"]
    )]
    cloudflare_token_file: Option<PathBuf>,
}

impl InitArgs {
    fn is_interactive(&self) -> bool {
        self.listen.is_none()
            && self.certificate.is_none()
            && self.private_key.is_none()
            && self.cloudflare_hostname.is_none()
            && self.cloudflare_token_file.is_none()
    }
}

#[derive(Debug, Args)]
struct RegisterProviderArgs {
    /// Native Responses processing tier; omitted to use the endpoint default.
    #[arg(long, value_name = "TIER")]
    service_tier: Option<String>,
    /// Provider identifier.
    #[arg(long, value_name = "ID")]
    provider: String,

    /// Stable identifier for this configured provider instance.
    #[arg(long, value_name = "ID")]
    instance: Option<String>,

    /// User-facing provider label.
    #[arg(long, value_name = "TEXT")]
    label: Option<String>,

    /// Provider model identifier.
    #[arg(long, value_name = "ID")]
    model: String,

    /// Comma-separated reasoning effort identifiers.
    #[arg(
        long,
        value_name = "CSV",
        value_delimiter = ',',
        action = ArgAction::Set
    )]
    reasoning_efforts: Vec<String>,

    /// Hosted web-search mode: off, cached, or live.
    #[arg(long, value_name = "MODE", default_value = "off")]
    web_search: HostedWebSearch,

    /// Provider API base URL override.
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// Configure an endpoint that does not require a credential.
    #[arg(long, conflicts_with = "credential_stdin")]
    credentialless: bool,

    /// Read the provider credential from standard input.
    #[arg(long)]
    credential_stdin: bool,

    /// Expire the piped credential at this Unix timestamp (seconds).
    #[arg(long, value_name = "TIMESTAMP", requires = "credential_stdin")]
    credential_expires_at: Option<u64>,
}

#[derive(Debug, Args)]
struct ConnectArgs {
    /// Public or local gateway endpoint.
    #[arg(long, value_name = "ENDPOINT")]
    endpoint: Option<Endpoint>,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Start the gateway as a background process.
    #[arg(long)]
    background: bool,
}

#[derive(Debug)]
pub(super) enum Command {
    PrintDefaultConfig,
    CheckConfig {
        state_dir: PathBuf,
    },
    ExportComputerResources {
        directory: PathBuf,
    },
    Telemetry {
        state_dir: PathBuf,
        command: TelemetryCommand,
    },
    SetRuntime {
        state_dir: PathBuf,
        idle_exit_seconds: Option<u64>,
        storage_limit_bytes: Option<u64>,
        ingress: Option<SocketAddr>,
        clear_ingress: bool,
        clear_storage_limit: bool,
    },
    Init(InitOptions),
    Bootstrap {
        state_dir: PathBuf,
    },
    ResetBotDefaults {
        state_dir: PathBuf,
    },
    SetDesktop {
        state_dir: PathBuf,
        enabled: bool,
    },
    PairingCode {
        state_dir: PathBuf,
    },
    RegisterProvider(RegisterProviderOptions),
    ClearProviderCredential {
        state_dir: PathBuf,
        instance: String,
    },
    Connect(ConnectOptions),
    Serve {
        state_dir: PathBuf,
        background: bool,
    },
    ServeChild {
        state_dir: PathBuf,
    },
    Exit {
        state_dir: PathBuf,
    },
}

#[derive(Debug)]
pub(super) struct InitOptions {
    pub(super) state_dir: PathBuf,
    pub(super) listen: SocketAddr,
    pub(super) tls: Option<TlsConfig>,
    pub(super) cloudflare: Option<CloudflareInit>,
}

/// Cloudflare exposure selected during gateway initialization.
pub enum CloudflareInit {
    /// Account-free temporary tunnel.
    Quick,
    /// Named tunnel with a credential that is redacted in debug output.
    Named {
        /// Public tunnel hostname.
        hostname: String,
        /// Cloudflare tunnel credential.
        token: String,
    },
}

impl std::fmt::Debug for CloudflareInit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quick => formatter.write_str("CloudflareInit::Quick"),
            Self::Named { hostname, .. } => formatter
                .debug_struct("CloudflareInit::Named")
                .field("hostname", hostname)
                .field("token", &"[redacted]")
                .finish(),
        }
    }
}

#[derive(Debug)]
pub(super) struct ConnectOptions {
    pub(super) state_dir: PathBuf,
    pub(super) endpoint: Option<Endpoint>,
}

#[derive(Debug)]
pub(super) struct RegisterProviderOptions {
    pub(super) service_tier: Option<String>,
    pub(super) state_dir: PathBuf,
    pub(super) provider: String,
    pub(super) instance: Option<String>,
    pub(super) label: Option<String>,
    pub(super) model: String,
    pub(super) reasoning_efforts: Vec<String>,
    pub(super) web_search: HostedWebSearch,
    pub(super) base_url: Option<String>,
    pub(super) credentialless: bool,
    pub(super) credential_stdin: bool,
    pub(super) credential_expires_at: Option<u64>,
}

impl GatewayCli {
    /// Returns the interactive frontend selected by this command line, if any.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn frontend_command(&self) -> Result<Option<FrontendCommand>> {
        let command = match &self.command {
            None => FrontendCommand::Dashboard(self.resolved_state_dir()?),
            Some(GatewaySubcommand::Provider) => {
                FrontendCommand::Provider(self.resolved_state_dir()?)
            }
            Some(GatewaySubcommand::Init(arguments)) if arguments.is_interactive() => {
                FrontendCommand::Init(self.resolved_state_dir()?)
            }
            _ => return Ok(None),
        };
        Ok(Some(command))
    }

    fn resolved_state_dir(&self) -> Result<PathBuf> {
        self.state_dir.clone().map_or_else(state_dir, Ok)
    }

    pub(super) fn into_command(self) -> Result<Command> {
        self.into_command_with_state(state_dir)
    }

    fn into_command_with_state(self, resolve: impl FnOnce() -> Result<PathBuf>) -> Result<Command> {
        let command = match self.command {
            Some(GatewaySubcommand::PrintDefaultConfig) => return Ok(Command::PrintDefaultConfig),
            Some(GatewaySubcommand::ExportComputerResources { directory }) => {
                return Ok(Command::ExportComputerResources { directory });
            }
            command => command,
        };
        let state_dir = self.state_dir.map_or_else(resolve, Ok)?;
        match command {
            Some(GatewaySubcommand::CheckConfig) => Ok(Command::CheckConfig { state_dir }),
            Some(GatewaySubcommand::Telemetry { command }) => {
                Ok(Command::Telemetry { state_dir, command })
            }
            Some(GatewaySubcommand::SetRuntime {
                idle_exit_seconds,
                storage_limit_bytes,
                ingress,
                clear_ingress,
                clear_storage_limit,
            }) => Ok(Command::SetRuntime {
                state_dir,
                idle_exit_seconds,
                storage_limit_bytes,
                ingress,
                clear_ingress,
                clear_storage_limit,
            }),
            Some(GatewaySubcommand::Init(arguments)) => {
                parse_init(state_dir, arguments).map(Command::Init)
            }
            Some(GatewaySubcommand::Bootstrap) => Ok(Command::Bootstrap { state_dir }),
            Some(GatewaySubcommand::ResetBotDefaults) => {
                Ok(Command::ResetBotDefaults { state_dir })
            }
            Some(GatewaySubcommand::SetDesktop { enabled }) => {
                Ok(Command::SetDesktop { state_dir, enabled })
            }
            Some(GatewaySubcommand::PairingCode { json: _ }) => {
                Ok(Command::PairingCode { state_dir })
            }
            Some(GatewaySubcommand::ClearProviderCredential { instance }) => {
                Ok(Command::ClearProviderCredential {
                    state_dir,
                    instance,
                })
            }
            Some(GatewaySubcommand::RegisterProvider(arguments)) => {
                Ok(Command::RegisterProvider(RegisterProviderOptions {
                    service_tier: arguments.service_tier,
                    state_dir,
                    provider: arguments.provider,
                    instance: arguments.instance,
                    label: arguments.label,
                    model: arguments.model,
                    reasoning_efforts: arguments.reasoning_efforts,
                    web_search: arguments.web_search,
                    base_url: arguments.base_url,
                    credentialless: arguments.credentialless,
                    credential_stdin: arguments.credential_stdin,
                    credential_expires_at: arguments.credential_expires_at,
                }))
            }
            Some(GatewaySubcommand::Connect(arguments)) => Ok(Command::Connect(ConnectOptions {
                state_dir,
                endpoint: arguments.endpoint,
            })),
            Some(GatewaySubcommand::Serve(arguments)) => Ok(Command::Serve {
                state_dir,
                background: arguments.background,
            }),
            Some(GatewaySubcommand::ServeChild) => Ok(Command::ServeChild { state_dir }),
            Some(GatewaySubcommand::Exit) => Ok(Command::Exit { state_dir }),
            None
            | Some(
                GatewaySubcommand::Provider
                | GatewaySubcommand::PrintDefaultConfig
                | GatewaySubcommand::ExportComputerResources { .. },
            ) => Err(Error::Config(
                "an executable gateway command is required".into(),
            )),
        }
    }
}

pub(super) fn parse_cli(arguments: Vec<OsString>) -> std::result::Result<GatewayCli, clap::Error> {
    GatewayCli::try_parse_from(std::iter::once(OsString::from("mobius-gateway")).chain(arguments))
}

#[cfg(test)]
pub(super) fn parse(arguments: Vec<OsString>) -> Result<Command> {
    parse_cli(arguments)
        .map_err(|error| Error::Config(error.to_string()))?
        .into_command()
}

fn parse_init(state_dir: PathBuf, arguments: InitArgs) -> Result<InitOptions> {
    let tls = match (arguments.certificate, arguments.private_key) {
        (Some(certificate), Some(private_key)) => Some(TlsConfig {
            certificate: std::fs::canonicalize(certificate)?,
            private_key: std::fs::canonicalize(private_key)?,
        }),
        (None, None) => None,
        _ => {
            return Err(Error::Config(
                "--tls-cert and --tls-key must be supplied together".into(),
            ));
        }
    };
    let cloudflare = match (
        arguments.cloudflare_hostname,
        arguments.cloudflare_token_file,
    ) {
        (Some(hostname), Some(path)) => Some(CloudflareInit::Named {
            hostname,
            token: load_secret_file(&path)?,
        }),
        (None, None) => None,
        _ => {
            return Err(Error::Config(
                "--cloudflare-hostname and --cloudflare-token-file must be supplied together"
                    .into(),
            ));
        }
    };
    Ok(InitOptions {
        state_dir,
        listen: arguments.listen.unwrap_or(DEFAULT_LISTEN),
        tls,
        cloudflare,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable_state() -> Result<PathBuf> {
        Err(Error::Config("no home directory configured".into()))
    }

    #[test]
    fn state_independent_commands_do_not_resolve_gateway_state() {
        let print = parse_cli(vec!["print-default-config".into()])
            .expect("print command")
            .into_command_with_state(unavailable_state)
            .expect("defaults do not require state");
        assert!(matches!(print, Command::PrintDefaultConfig));

        let export = parse_cli(vec![
            "export-computer-resources".into(),
            "--directory".into(),
            "computer-resources".into(),
        ])
        .expect("export command")
        .into_command_with_state(unavailable_state)
        .expect("resource export does not require state");
        assert!(matches!(export,
            Command::ExportComputerResources { directory }
                if directory == Path::new("computer-resources")
        ));

        let check = parse_cli(vec!["check-config".into()])
            .expect("check command")
            .into_command_with_state(unavailable_state)
            .expect_err("checking persisted configuration requires state");
        assert!(check.to_string().contains("no home directory configured"));
    }
}
