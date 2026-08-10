use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "wombat", version, about = "A Lua-powered dotfiles compiler")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Evaluate configuration and print its desired-state manifest.
    Build {
        /// Wombat repository root containing wombat.lua.
        #[arg(default_value = ".")]
        root: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Build { root } => wombat::build(&root).and_then(|manifest| {
            serde_json::to_string_pretty(&manifest)
                .map_err(wombat::WombatError::from)
                .map(|json| println!("{json}"))
        }),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
