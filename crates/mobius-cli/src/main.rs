//! Command-line entry point for möbius.

use clap::Parser as _;
use mobius_cli::command::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Cli::parse().run().await
}
