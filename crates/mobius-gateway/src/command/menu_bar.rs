//! Launch the optional desktop frontend and hand it the existing local client identity.

use std::io::IsTerminal as _;

use super::*;

const BUNDLE_ID: &str = "app.mobius.gateway";

pub(super) fn open_if_installed(state_dir: &Path) {
    if std::env::var_os("SSH_CONNECTION").is_none()
        && std::env::var_os("MOBIUS_GATEWAY_NO_MENU_BAR").is_none()
    {
        // The companion is optional; a missing GUI must never stop the server.
        let _ = open(state_dir);
    }
}

pub(super) fn open(state_dir: &Path) -> Result<()> {
    let status = std::process::Command::new("/usr/bin/open")
        .args(["-g", "-b", BUNDLE_ID, "--args", "--gateway-executable"])
        .arg(std::env::current_exe()?)
        .arg("--state-dir")
        .arg(fs::canonicalize(state_dir)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(Error::Config(
            "install the macOS möbius Gateway app to use the menu bar".into(),
        ));
    }
    Ok(())
}

pub(super) async fn connect(
    state_dir: PathBuf,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    if std::io::stdout().is_terminal() {
        return Err(Error::Config(
            "the menu bar connection handoff requires a private output pipe".into(),
        ));
    }
    let (store, config) = ConfigStore::open(state_dir)?;
    if config.tls.is_some() || !config.listen.ip().is_loopback() {
        return Err(Error::Config(
            "menu bar voice requires the gateway's local TCP listener".into(),
        ));
    }
    {
        let _startup = StartupGuard::create(store.state_dir())?;
        if running_process_pid(&store.state_dir().join(PROCESS_FILE))?.is_none() {
            let mut interrupts = signal(SignalKind::interrupt())?;
            let mut terminations = signal(SignalKind::terminate())?;
            start_background_gateway(store.state_dir(), &mut interrupts, &mut terminations)
                .await?
                .ok_or_else(|| Error::Config("gateway startup cancelled".into()))?;
        }
    }
    let endpoint = loopback_endpoint(&config)?;
    let token = load_local_client(&endpoint)?.ok_or_else(|| {
        Error::Config("pair this installation with the local gateway before using voice".into())
    })?;
    let connection = serde_json::json!({
        "endpoint": endpoint.to_string(),
        "token": token,
        "protocol_version": crate::wire::PROTOCOL_VERSION,
    });
    serde_json::to_writer(std::io::stdout().lock(), &connection)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_bar_commands_keep_the_selected_state_directory() {
        for command in ["menu-bar", "__menu-bar-connect"] {
            let parsed = args::parse(vec![
                command.into(),
                "--state-dir".into(),
                "/tmp/voice-gateway".into(),
            ])
            .expect("desktop command");
            let (Command::MenuBar { state_dir } | Command::MenuBarConnect { state_dir }) = parsed
            else {
                panic!("expected menu bar command");
            };
            assert_eq!(state_dir, PathBuf::from("/tmp/voice-gateway"));
        }
    }
}
