use std::path::PathBuf;

use chrono::NaiveDate;
use clap::{CommandFactory as _, Parser};
use mobius_cli::command::Cli;
use mobius_gateway::command::GatewayCli;

#[derive(Parser)]
#[command(about = "Generate möbius manual pages from the production CLI schemas")]
struct Arguments {
    /// Directory for generated section 1 manual pages.
    output: PathBuf,

    /// Date of the manual-page release.
    #[arg(long, value_name = "YYYY-MM-DD")]
    date: NaiveDate,
}

fn main() -> std::io::Result<()> {
    let arguments = Arguments::parse();
    std::fs::create_dir_all(&arguments.output)?;
    generate(Cli::command(), &arguments.output, arguments.date)?;
    generate(GatewayCli::command(), &arguments.output, arguments.date)
}

fn generate(
    mut command: clap::Command,
    output: &std::path::Path,
    date: NaiveDate,
) -> std::io::Result<()> {
    command = command.disable_help_subcommand(true);
    command.build();
    generate_command(command, output, date)
}

fn generate_command(
    command: clap::Command,
    output: &std::path::Path,
    date: NaiveDate,
) -> std::io::Result<()> {
    for subcommand in command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .cloned()
    {
        generate_command(subcommand, output, date)?;
    }
    let title = command
        .get_display_name()
        .unwrap_or_else(|| command.get_name())
        .to_uppercase();
    clap_mangen::Man::new(command)
        .title(title)
        .date(date.to_string())
        .manual("Möbius Manual")
        .generate_to(output)?;
    Ok(())
}
