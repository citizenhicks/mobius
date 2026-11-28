//! Command-line entry point for möbius.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![deny(unreachable_pub)]

use clap::Parser as _;
use mobius_cli::command::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Cli::parse().run().await
}
