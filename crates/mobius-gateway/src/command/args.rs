use super::*;
use mobius::backend::model::provider::HostedWebSearch;

#[derive(Debug)]
pub(super) enum Command {
    Init(InitOptions),
    Bootstrap {
        state_dir: PathBuf,
    },
    PairingCode {
        state_dir: PathBuf,
    },
    RegisterProvider(RegisterProviderOptions),
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
}

pub(super) fn parse(arguments: Vec<OsString>) -> Result<Command> {
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err(Error::Config(USAGE.into()));
    };
    if command == "init" {
        parse_init(arguments.collect()).map(Command::Init)
    } else if command == "bootstrap" {
        parse_state_dir(arguments.collect()).map(|state_dir| Command::Bootstrap { state_dir })
    } else if command == "pairing-code" {
        parse_pairing_code(arguments.collect()).map(|state_dir| Command::PairingCode { state_dir })
    } else if command == "register-provider" {
        parse_register_provider(arguments.collect()).map(Command::RegisterProvider)
    } else if command == "connect" {
        parse_connect(arguments.collect()).map(Command::Connect)
    } else if command == "serve" {
        parse_serve(arguments.collect())
    } else if command == "__serve" {
        parse_state_dir(arguments.collect()).map(|state_dir| Command::ServeChild { state_dir })
    } else if command == "exit" {
        parse_state_dir(arguments.collect()).map(|state_dir| Command::Exit { state_dir })
    } else {
        Err(Error::Config(USAGE.into()))
    }
}

pub(super) fn parse_register_provider(arguments: Vec<OsString>) -> Result<RegisterProviderOptions> {
    let mut configured_state_dir = None;
    let mut provider = None;
    let mut instance = None;
    let mut label = None;
    let mut model = None;
    let mut reasoning_efforts = None;
    let mut web_search = None;
    let mut base_url = None;
    let mut credentialless = false;
    let mut arguments = arguments.into_iter();
    while let Some(flag) = arguments.next() {
        if flag == "--credentialless" {
            if credentialless {
                return Err(Error::Config("--credentialless supplied twice".into()));
            }
            credentialless = true;
            continue;
        }
        let value = arguments
            .next()
            .ok_or_else(|| Error::Config(format!("{} requires a value", flag.to_string_lossy())))?;
        if flag == "--state-dir" {
            set_once(
                &mut configured_state_dir,
                PathBuf::from(value),
                "--state-dir",
            )?;
        } else if flag == "--provider" {
            set_once(
                &mut provider,
                value
                    .into_string()
                    .map_err(|_| Error::Config("--provider is not valid UTF-8".into()))?,
                "--provider",
            )?;
        } else if flag == "--instance" {
            set_once(
                &mut instance,
                value
                    .into_string()
                    .map_err(|_| Error::Config("--instance is not valid UTF-8".into()))?,
                "--instance",
            )?;
        } else if flag == "--label" {
            set_once(
                &mut label,
                value
                    .into_string()
                    .map_err(|_| Error::Config("--label is not valid UTF-8".into()))?,
                "--label",
            )?;
        } else if flag == "--model" {
            set_once(
                &mut model,
                value
                    .into_string()
                    .map_err(|_| Error::Config("--model is not valid UTF-8".into()))?,
                "--model",
            )?;
        } else if flag == "--reasoning-efforts" {
            let value = value
                .into_string()
                .map_err(|_| Error::Config("--reasoning-efforts is not valid UTF-8".into()))?;
            set_once(
                &mut reasoning_efforts,
                value.split(',').map(str::to_owned).collect(),
                "--reasoning-efforts",
            )?;
        } else if flag == "--web-search" {
            let value = value
                .into_string()
                .map_err(|_| Error::Config("--web-search is not valid UTF-8".into()))?;
            let value = value
                .parse::<HostedWebSearch>()
                .map_err(|_| Error::Config("--web-search must be off, cached, or live".into()))?;
            set_once(&mut web_search, value, "--web-search")?;
        } else if flag == "--base-url" {
            set_once(
                &mut base_url,
                value
                    .into_string()
                    .map_err(|_| Error::Config("--base-url is not valid UTF-8".into()))?,
                "--base-url",
            )?;
        } else {
            return Err(Error::Config(USAGE.into()));
        }
    }
    Ok(RegisterProviderOptions {
        state_dir: configured_state_dir.map_or_else(state_dir, Ok)?,
        provider: provider.ok_or_else(|| Error::Config("--provider is required".into()))?,
        instance,
        label,
        model: model.ok_or_else(|| Error::Config("--model is required".into()))?,
        reasoning_efforts: reasoning_efforts.unwrap_or_default(),
        web_search: web_search.unwrap_or_default(),
        base_url,
        credentialless,
    })
}

pub(super) fn parse_pairing_code(arguments: Vec<OsString>) -> Result<PathBuf> {
    match arguments.as_slice() {
        [json] if json == "--json" => state_dir(),
        [state_dir, path, json] if state_dir == "--state-dir" && json == "--json" => {
            Ok(PathBuf::from(path))
        }
        _ => Err(Error::Config(USAGE.into())),
    }
}

pub(super) fn parse_connect(arguments: Vec<OsString>) -> Result<ConnectOptions> {
    let mut configured_state_dir = None;
    let mut endpoint = None;
    let mut arguments = arguments.into_iter();
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| Error::Config(format!("{} requires a value", flag.to_string_lossy())))?;
        if flag == "--state-dir" {
            set_once(
                &mut configured_state_dir,
                PathBuf::from(value),
                "--state-dir",
            )?;
        } else if flag == "--endpoint" {
            let value = value
                .to_str()
                .ok_or_else(|| Error::Config("--endpoint is not valid UTF-8".into()))?
                .parse()?;
            set_once(&mut endpoint, value, "--endpoint")?;
        } else {
            return Err(Error::Config(USAGE.into()));
        }
    }
    Ok(ConnectOptions {
        state_dir: configured_state_dir.map_or_else(state_dir, Ok)?,
        endpoint,
    })
}

pub(super) fn parse_serve(arguments: Vec<OsString>) -> Result<Command> {
    let (configured_state_dir, background) = match arguments.as_slice() {
        [] => (None, false),
        [flag] if flag == "--background" => (None, true),
        [flag, path] if flag == "--state-dir" => (Some(path), false),
        [background, state_dir, path]
            if background == "--background" && state_dir == "--state-dir" =>
        {
            (Some(path), true)
        }
        [state_dir, path, background]
            if state_dir == "--state-dir" && background == "--background" =>
        {
            (Some(path), true)
        }
        _ => return Err(Error::Config(USAGE.into())),
    };
    let state_dir = configured_state_dir.map_or_else(state_dir, |path| Ok(PathBuf::from(path)))?;
    Ok(Command::Serve {
        state_dir,
        background,
    })
}

pub(super) fn parse_init(arguments: Vec<OsString>) -> Result<InitOptions> {
    let mut configured_state_dir = None;
    let mut listen = None;
    let mut certificate = None;
    let mut private_key = None;
    let mut cloudflare_hostname = None;
    let mut cloudflare_token_file = None;
    let mut arguments = arguments.into_iter();
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| Error::Config(format!("{} requires a value", flag.to_string_lossy())))?;
        if flag == "--state-dir" {
            set_once(
                &mut configured_state_dir,
                PathBuf::from(value),
                "--state-dir",
            )?;
        } else if flag == "--listen" {
            let value = value
                .to_str()
                .ok_or_else(|| Error::Config("--listen is not valid UTF-8".into()))?
                .parse()
                .map_err(|_| Error::Config("--listen is not a socket address".into()))?;
            set_once(&mut listen, value, "--listen")?;
        } else if flag == "--tls-cert" {
            set_once(&mut certificate, PathBuf::from(value), "--tls-cert")?;
        } else if flag == "--tls-key" {
            set_once(&mut private_key, PathBuf::from(value), "--tls-key")?;
        } else if flag == "--cloudflare-hostname" {
            let value = value
                .into_string()
                .map_err(|_| Error::Config("--cloudflare-hostname is not valid UTF-8".into()))?;
            set_once(&mut cloudflare_hostname, value, "--cloudflare-hostname")?;
        } else if flag == "--cloudflare-token-file" {
            set_once(
                &mut cloudflare_token_file,
                PathBuf::from(value),
                "--cloudflare-token-file",
            )?;
        } else {
            return Err(Error::Config(USAGE.into()));
        }
    }
    let state_dir = configured_state_dir.map_or_else(state_dir, Ok)?;
    let listen = listen.unwrap_or(DEFAULT_LISTEN);
    let tls = match (certificate, private_key) {
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
    let cloudflare = match (cloudflare_hostname, cloudflare_token_file) {
        (Some(hostname), Some(path)) => {
            if tls.is_some() {
                return Err(Error::Config(
                    "Cloudflare and direct TLS listener options cannot be combined".into(),
                ));
            }
            Some(CloudflareInit::Named {
                hostname,
                token: load_cloudflare_token(&path)?,
            })
        }
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
        listen,
        tls,
        cloudflare,
    })
}

pub(super) fn parse_state_dir(arguments: Vec<OsString>) -> Result<PathBuf> {
    let state_dir = match arguments.as_slice() {
        [] => state_dir()?,
        [flag, path] if flag == "--state-dir" => PathBuf::from(path),
        _ => return Err(Error::Config(USAGE.into())),
    };
    Ok(state_dir)
}

pub(super) fn set_once<T>(target: &mut Option<T>, value: T, flag: &str) -> Result<()> {
    if target.replace(value).is_some() {
        return Err(Error::Config(format!("{flag} was supplied more than once")));
    }
    Ok(())
}
