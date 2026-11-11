use clap::{ArgAction, Args, Parser, Subcommand};
use mobius::backend::model::provider::HostedWebSearch;

use super::{
    DEFAULT_LISTEN, Endpoint, Error, OsString, PathBuf, Result, SocketAddr, TlsConfig,
    load_secret_file, state_dir,
};

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
    pub(super) state_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub(super) command: Option<GatewaySubcommand>,
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
pub(super) enum GatewaySubcommand {
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
    /// Verify the running gateway answers authenticated local control; print readiness and collector status as JSON.
    Ready,
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
        /// Snapshot sections: activity, usage, runs, storage, resources, context.
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
pub(super) struct InitArgs {
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
pub(super) struct RegisterProviderArgs {
    /// Native Responses processing tier; omitted to use the endpoint default.
    #[arg(long, value_name = "TIER")]
    pub(super) service_tier: Option<String>,
    /// Provider identifier.
    #[arg(long, value_name = "ID")]
    pub(super) provider: String,

    /// Stable identifier for this configured provider instance.
    #[arg(long, value_name = "ID")]
    pub(super) instance: Option<String>,

    /// User-facing provider label.
    #[arg(long, value_name = "TEXT")]
    pub(super) label: Option<String>,

    /// Preserve existing model catalogs and selections when refreshing credentials/endpoints.
    /// Omit this flag when intentionally replacing an existing catalog.
    #[arg(long)]
    pub(super) preserve_selection: bool,

    /// Update only an already configured provider instance.
    #[arg(long)]
    pub(super) if_configured: bool,

    /// Default chat model; omit for a media-only provider.
    #[arg(long, value_name = "ID", default_value = "")]
    pub(super) model: String,

    /// Chat model ID; repeat to replace the list. Omit to preserve existing IDs.
    #[arg(long = "model-id", value_name = "ID")]
    pub(super) model_ids: Option<Vec<String>>,

    /// Replace models with JSON records: id, reasoning_efforts, default_reasoning; optional label, description, context_window.
    #[arg(long, value_name = "JSON", conflicts_with = "model_ids")]
    pub(super) models_json: Option<String>,

    /// Image model ID; repeat to replace the list. Omit to preserve existing IDs.
    #[arg(long = "image-model-id", value_name = "ID")]
    pub(super) image_model_ids: Option<Vec<String>>,

    /// Mark this setup's credential and usage limits as supplied by the gateway operator.
    #[arg(long)]
    pub(super) operator_managed: bool,

    /// Tool discovery mode: native or rebuild; omit to preserve the existing override.
    #[arg(long, value_name = "MODE", value_parser = ["native", "rebuild"], hide_possible_values = true)]
    pub(super) tool_discovery: Option<String>,

    /// Hosted web-search mode: off, cached, or live.
    #[arg(long, value_name = "MODE", default_value = "off")]
    pub(super) web_search: HostedWebSearch,

    /// Provider API base URL override.
    #[arg(long, value_name = "URL")]
    pub(super) base_url: Option<mobius::backend::model::provider::BaseUrl>,

    /// Configure an endpoint that does not require a credential.
    #[arg(long, conflicts_with = "credential_stdin")]
    pub(super) credentialless: bool,

    /// Read the provider credential from standard input.
    #[arg(long)]
    pub(super) credential_stdin: bool,

    /// Expire the piped credential at this Unix timestamp (seconds).
    #[arg(long, value_name = "TIMESTAMP", requires = "credential_stdin")]
    pub(super) credential_expires_at: Option<u64>,
}

#[derive(Debug, Args)]
pub(super) struct ConnectArgs {
    /// Public or local gateway endpoint.
    #[arg(long, value_name = "ENDPOINT")]
    pub(super) endpoint: Option<Endpoint>,
}

#[derive(Debug, Args)]
pub(super) struct ServeArgs {
    /// Start the gateway as a background process.
    #[arg(long)]
    pub(super) background: bool,
}

#[derive(Debug)]
pub(super) struct InitOptions {
    pub(super) state_dir: PathBuf,
    pub(super) listen: SocketAddr,
    pub(super) tls: Option<TlsConfig>,
    pub(super) cloudflare: Option<CloudflareInit>,
}

/// Cloudflare exposure selected during gateway initialization.
#[derive(Debug)]
pub enum CloudflareInit {
    /// Account-free temporary tunnel.
    Quick,
    /// Named tunnel whose credential is redacted in debug output.
    Named(crate::config::NamedTunnel),
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
        self.state_dir.clone().map_or_else(state_dir, Ok) // rules-allow: frontend_command borrows the parsed CLI and the frontend owns its path
    }
}

pub(super) fn parse_cli(arguments: Vec<OsString>) -> std::result::Result<GatewayCli, clap::Error> {
    GatewayCli::try_parse_from(std::iter::once(OsString::from("mobius-gateway")).chain(arguments))
}

pub(super) fn parse_init(state_dir: PathBuf, arguments: InitArgs) -> Result<InitOptions> {
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
        (Some(hostname), Some(path)) => Some(CloudflareInit::Named(
            crate::config::NamedTunnel::new(&hostname, &load_secret_file(&path)?)?,
        )),
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
