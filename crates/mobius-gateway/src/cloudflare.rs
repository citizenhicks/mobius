//! Lifecycle for one Cloudflare Tunnel connector.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::time::timeout;
use url::Url;

use crate::client::Endpoint;
use crate::config::{CloudflareConfig, ConfigStore, GatewayConfig, load_secret_file};
use crate::{Error, Result};

const READY_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct CloudflareTunnel {
    child: Child,
    endpoint: Option<Endpoint>,
    quick_endpoint: Option<oneshot::Receiver<Result<Endpoint>>>,
}

impl CloudflareTunnel {
    pub(crate) fn start(store: &ConfigStore, config: &GatewayConfig) -> Result<Option<Self>> {
        let Some(cloudflare) = &config.cloudflare else {
            return Ok(None);
        };
        let mut command = match cloudflare {
            CloudflareConfig::Quick => {
                quick_tunnel_command(cloudflared_executable(), config.listen)
            }
            CloudflareConfig::Named { .. } => {
                let token_path = store.cloudflare_token_path();
                load_secret_file(&token_path)?;
                named_tunnel_command(cloudflared_executable(), &token_path)
            }
        };
        command.kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            Error::Config(format!(
                "failed to start bundled cloudflared; install it beside mobius-gateway or on PATH: {error}"
            ))
        })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Config("failed to capture cloudflared output".into()))?;
        let (endpoint, quick_endpoint) = match cloudflare {
            CloudflareConfig::Quick => {
                let (sender, receiver) = oneshot::channel();
                tokio::spawn(drain_quick_stderr(stderr, sender));
                (None, Some(receiver))
            }
            CloudflareConfig::Named { .. } => {
                tokio::spawn(drain_stderr(stderr));
                let endpoint = cloudflare
                    .endpoint()
                    .ok_or_else(|| Error::Config("named Cloudflare endpoint is missing".into()))?
                    .parse()?;
                (Some(endpoint), None)
            }
        };
        Ok(Some(Self {
            child,
            endpoint,
            quick_endpoint,
        }))
    }

    /// Hands over the public endpoint once the tunnel has one.
    pub(crate) async fn endpoint(&mut self) -> Result<Endpoint> {
        if let Some(status) = self.child.try_wait()? {
            return Err(Error::Config(format!(
                "cloudflared stopped during startup with {status}"
            )));
        }
        if let Some(endpoint) = self.endpoint.take() {
            return Ok(endpoint);
        }
        let receiver = self.quick_endpoint.take().ok_or_else(|| {
            Error::Config("Cloudflare Quick Tunnel address is no longer available".into())
        })?;
        let endpoint = timeout(READY_TIMEOUT, receiver)
            .await
            .map_err(|error| {
                Error::Config(format!(
                    "Cloudflare Quick Tunnel did not assign an address within {} seconds: {error}",
                    READY_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|error| {
                Error::Config(format!(
                    "cloudflared stopped before assigning an address: {error}"
                ))
            })??;
        if let Some(status) = self.child.try_wait()? {
            return Err(Error::Config(format!(
                "cloudflared stopped during startup with {status}"
            )));
        }
        Ok(endpoint)
    }

    pub(crate) async fn wait(&mut self) -> Result<()> {
        let status = self.child.wait().await?;
        Err(Error::Config(format!(
            "cloudflared stopped unexpectedly with {status}"
        )))
    }
}

async fn drain_quick_stderr(
    stderr: impl AsyncRead + Unpin,
    sender: oneshot::Sender<Result<Endpoint>>,
) {
    let mut lines = BufReader::new(stderr).lines();
    let mut sender = Some(sender);
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Some(endpoint) = line.split_ascii_whitespace().find_map(quick_endpoint)
                    && let Some(sender) = sender.take()
                {
                    report_quick_endpoint(sender, Ok(endpoint));
                }
            }
            Ok(None) => break,
            Err(error) => {
                if let Some(sender) = sender.take() {
                    report_quick_endpoint(sender, Err(error.into()));
                }
                return;
            }
        }
    }
    if let Some(sender) = sender {
        report_quick_endpoint(
            sender,
            Err(Error::Config(
                "cloudflared stopped before assigning an address".into(),
            )),
        );
    }
}

fn report_quick_endpoint(sender: oneshot::Sender<Result<Endpoint>>, endpoint: Result<Endpoint>) {
    if sender.send(endpoint).is_err() {
        tracing::debug!("Cloudflare Quick Tunnel startup stopped waiting for its address");
    }
}

async fn drain_stderr(mut stderr: impl AsyncRead + Unpin) {
    if let Err(error) = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await {
        tracing::debug!(%error, "cloudflared output stopped");
    }
}

fn quick_endpoint(candidate: &str) -> Option<Endpoint> {
    let url = Url::parse(candidate).ok()?;
    let hostname = url.host_str()?;
    let label = hostname.strip_suffix(".trycloudflare.com")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || label.is_empty()
        || label.contains('.')
        || !crate::hostnames::valid_label(label)
        || (candidate != format!("https://{hostname}")
            && candidate != format!("https://{hostname}/"))
    {
        return None;
    }
    format!("wss://{hostname}").parse().ok()
}

fn quick_tunnel_command(program: impl Into<OsString>, listen: SocketAddr) -> Command {
    let mut command = Command::new(program.into());
    command
        .arg("tunnel")
        .arg("--no-autoupdate")
        .arg("--config")
        .arg(empty_config_path())
        .arg("--url")
        .arg(format!("http://{listen}"))
        .env_remove("TUNNEL_TOKEN")
        .env_remove("TUNNEL_TOKEN_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn named_tunnel_command(program: impl Into<OsString>, token_path: &Path) -> Command {
    let mut command = Command::new(program.into());
    command
        .arg("tunnel")
        .arg("--no-autoupdate")
        .arg("run")
        .arg("--token-file")
        .arg(token_path)
        .env_remove("TUNNEL_TOKEN")
        .env_remove("TUNNEL_TOKEN_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn empty_config_path() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

fn cloudflared_executable() -> OsString {
    let name = if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    };
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(name)))
        .filter(|path| path.is_file())
        .map_or_else(|| OsString::from(name), PathBuf::into_os_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

    #[test]
    fn quick_endpoint_rejects_untrusted_url_shapes() {
        for candidate in [
            "http://bright-river.trycloudflare.com",
            "https://eviltrycloudflare.com",
            "https://user@bright-river.trycloudflare.com",
            "https://bright-river.trycloudflare.com:8443",
            "https://bright-river.trycloudflare.com?query",
            "https://bright-river.trycloudflare.com#fragment",
        ] {
            assert!(quick_endpoint(candidate).is_none(), "accepted {candidate}");
        }
    }

    #[tokio::test]
    async fn quick_stderr_keeps_draining_after_discovery() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let (sender, receiver) = oneshot::channel();
        let draining = tokio::spawn(drain_quick_stderr(reader, sender));
        writer
            .write_all(b"https://bright-river.trycloudflare.com\n")
            .await
            .expect("write endpoint");
        let endpoint = receiver.await.expect("endpoint sender").expect("endpoint");
        writer
            .write_all(&vec![b'x'; 1024])
            .await
            .expect("stderr remains drained");
        drop(writer);
        timeout(Duration::from_secs(1), draining)
            .await
            .expect("drainer completes")
            .expect("drainer task");

        assert_eq!(endpoint.to_string(), "wss://bright-river.trycloudflare.com");
    }

    #[test]
    fn named_tunnel_command_passes_only_the_token_file_path() {
        let command = named_tunnel_command("cloudflared", Path::new("/private/cloudflare-token"));
        let arguments = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "tunnel",
                "--no-autoupdate",
                "run",
                "--token-file",
                "/private/cloudflare-token"
            ]
        );
    }
}
