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
    /// Open the provider setup interface.
    Provider,
    /// Initialize gateway state.
    Init(InitArgs),
    /// Initialize a direct loopback gateway for machine use.
    Bootstrap,
    /// Restore the default Bot configuration.
    ResetBotDefaults,
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
    /// Stop a background gateway.
    Exit,
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
    Init(InitOptions),
    Bootstrap {
        state_dir: PathBuf,
    },
    ResetBotDefaults {
        state_dir: PathBuf,
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

pub(super) enum CloudflareInit {
    Quick,
    Named { hostname: String, token: String },
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
        let state_dir = self.state_dir.map_or_else(state_dir, Ok)?;
        match self.command {
            Some(GatewaySubcommand::Init(arguments)) => {
                parse_init(state_dir, arguments).map(Command::Init)
            }
            Some(GatewaySubcommand::Bootstrap) => Ok(Command::Bootstrap { state_dir }),
            Some(GatewaySubcommand::ResetBotDefaults) => {
                Ok(Command::ResetBotDefaults { state_dir })
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
            None | Some(GatewaySubcommand::Provider) => Err(Error::Config(
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
            token: load_cloudflare_token(&path)?,
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
