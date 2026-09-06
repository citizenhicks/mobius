use std::path::PathBuf;

use clap::Parser as _;
use mobius_cli::gateway_accounts::GatewayAccounts;
use mobius_gateway::client::Endpoint;
use mobius_gateway::command::{FrontendCommand, GatewayCli};

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let cli = GatewayCli::parse();
    match cli.frontend_command()? {
        Some(FrontendCommand::Init(state_dir)) => initialize_cloudflare(state_dir).await?,
        Some(FrontendCommand::Dashboard(state_dir)) => {
            mobius_cli::frontend::run_gateway_dashboard(state_dir).await?
        }
        Some(FrontendCommand::Provider(state_dir)) => {
            mobius_cli::frontend::run_gateway_provider(state_dir).await?
        }
        None => mobius_gateway::command::run_cli(cli, save_local_client, load_local_client).await?,
    }
    Ok(())
}

async fn initialize_cloudflare(state_dir: PathBuf) -> mobius_gateway::Result<()> {
    let Some(setup) = mobius_cli::frontend::run_cloudflare_setup().await? else {
        return Ok(());
    };
    if state_dir.try_exists()? {
        if !mobius_cli::frontend::confirm_gateway_reinitialize(&state_dir).await? {
            return Ok(());
        }
        mobius_gateway::command::reset_gateway_state(state_dir.clone())?;
    }
    match setup {
        mobius_cli::frontend::CloudflareInit::Quick => {
            mobius_gateway::command::initialize_quick_cloudflare(state_dir.clone())?;
        }
        mobius_cli::frontend::CloudflareInit::Named { hostname, token } => {
            mobius_gateway::command::initialize_named_cloudflare(
                state_dir.clone(),
                hostname,
                token,
            )?;
        }
    }
    mobius_gateway::command::run(
        vec![
            "connect".into(),
            "--state-dir".into(),
            state_dir.into_os_string(),
        ],
        save_local_client,
        load_local_client,
    )
    .await
}

fn save_local_client(endpoint: &Endpoint, token: String) -> mobius_gateway::Result<()> {
    let mut accounts = GatewayAccounts::load()?;
    accounts.add(endpoint, token)?;
    accounts.save()
}

fn load_local_client(endpoint: &Endpoint) -> mobius_gateway::Result<Option<String>> {
    Ok(GatewayAccounts::load()?.token(endpoint).map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> GatewayCli {
        GatewayCli::try_parse_from(
            std::iter::once("mobius-gateway").chain(arguments.iter().copied()),
        )
        .expect("parse gateway command")
    }

    #[test]
    fn no_command_and_provider_select_gateway_frontends() {
        let dashboard = parse(&[]);
        assert!(matches!(
            dashboard.frontend_command().expect("dashboard command"),
            Some(FrontendCommand::Dashboard(_))
        ));
        let init = parse(&["init", "--state-dir", "/tmp/gateway"]);
        assert!(matches!(
            init.frontend_command().expect("init command"),
            Some(FrontendCommand::Init(path)) if path == std::path::Path::new("/tmp/gateway")
        ));
        let provider = parse(&["provider", "--state-dir", "/tmp/gateway"]);
        assert!(matches!(
            provider.frontend_command().expect("provider command"),
            Some(FrontendCommand::Provider(path)) if path == std::path::Path::new("/tmp/gateway")
        ));
    }
}
