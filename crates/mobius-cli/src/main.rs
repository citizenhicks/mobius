//! Command-line entry point for möbius.

use clap::Parser as _;
use mobius_cli::command::Cli;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    Cli::parse().run().await
}
