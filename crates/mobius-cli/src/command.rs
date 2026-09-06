//! Command-line interface shared by the `mobius` binary and documentation tooling.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use mobius_gateway::client::Endpoint;

/// Parsed `mobius` command line.
#[derive(Debug, Parser)]
#[command(
    name = "mobius",
    version,
    propagate_version = true,
    about = "Terminal client for a möbius gateway"
)]
pub struct Cli {
    /// Command to run; omit it to open the terminal interface.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Commands supported by the `mobius` terminal client.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one task without opening the terminal interface.
    Run {
        /// Bot handle or stable identifier.
        bot: String,

        /// UTF-8 file containing the task prompt.
        task_file: PathBuf,
    },

    /// Pair this client with a gateway.
    Pair {
        /// Gateway endpoint, such as `wss://gateway.example.com`.
        endpoint: Endpoint,

        /// Single-use pairing code issued by the gateway.
        one_time_code: String,
    },

    /// Open the extension manager.
    Extensions,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_parses_a_bot_and_task_file() {
        let cli = Cli::try_parse_from(["mobius", "run", "@builder", "task.md"])
            .expect("parse run command");

        assert!(matches!(
            cli.command,
            Some(Command::Run { bot, task_file })
                if bot == "@builder" && task_file == std::path::Path::new("task.md")
        ));
    }
}
