use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "wombat", version, about = "A Lua-powered dotfiles compiler")]
struct Cli {
    /// Wombat source repository. Defaults to configured source or ~/.local/share/wombat.
    #[arg(short = 'S', long, global = true)]
    source: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Evaluate and materialise a completed static build product.
    Build {
        /// Build workspace, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
    },
    /// Add an existing home file to Wombat source state.
    Add {
        /// Absolute existing file beneath the target home.
        target: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let source = wombat::config::resolve_source(cli.source.as_deref());
    let result = match cli.command {
        Command::Build { build_dir } => source.and_then(|source_root| {
            wombat::build(wombat::BuildOptions::new(source_root, build_dir)).map(|outcome| {
                println!(
                    "{} {} ({} artifacts) at {}",
                    outcome.status,
                    outcome.build_id,
                    outcome.artifact_count,
                    outcome.build_dir.display()
                );
            })
        }),
        Command::Add { target } => source
            .and_then(|source_root| wombat::config::resolve_home().map(|home| (source_root, home)))
            .and_then(|(source_root, home)| wombat::add(&source_root, &home, &target))
            .map(|outcome| println!("{}", outcome.display())),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
