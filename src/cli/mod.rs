//! Clap argument types and the CLI's own vocabulary.
//!
//! The composition root: this is where global options, per-command arguments,
//! and their defaults live. Command behaviour is in [`commands`].
//!
//! Repository-defined inputs are kept in a separate namespace after `--`, so a
//! repository can name an option `--target` without colliding with Wombat's own.
use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::builder::styling;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

mod commands;
#[cfg(test)]
mod surface;

pub(crate) use commands::run;

/// Help styling, using the same colours as Wombat's own output so help and
/// command output read as one product.
///
/// Applied only when colour is enabled — [`parse_cli`] resolves `--color`,
/// `NO_COLOR`, and terminal detection before building the command, so these
/// styles are never emitted into a pipe.
const HELP_STYLES: styling::Styles = styling::Styles::styled()
    .header(
        styling::AnsiColor::Green
            .on_default()
            .effects(styling::Effects::BOLD.insert(styling::Effects::UNDERLINE)),
    )
    .usage(
        styling::AnsiColor::Green
            .on_default()
            .effects(styling::Effects::BOLD),
    )
    // Literals are what you type: flags, subcommands, and values. Cyan is the
    // colour Wombat already uses for paths in its output.
    .literal(
        styling::AnsiColor::Cyan
            .on_default()
            .effects(styling::Effects::BOLD),
    )
    .placeholder(styling::AnsiColor::Cyan.on_default())
    .valid(
        styling::AnsiColor::Cyan
            .on_default()
            .effects(styling::Effects::BOLD),
    )
    .invalid(
        styling::AnsiColor::Yellow
            .on_default()
            .effects(styling::Effects::BOLD),
    )
    .error(
        styling::AnsiColor::Red
            .on_default()
            .effects(styling::Effects::BOLD),
    );

#[derive(Debug, Parser)]
#[command(
    name = "wombat",
    version,
    about = "A Lua-powered dotfiles compiler",
    styles = HELP_STYLES
)]
struct Cli {
    /// Wombat source repository. Defaults to configured source or ~/.local/share/wombat.
    #[arg(short = 'S', long, global = true)]
    source: Option<PathBuf>,

    /// Terminal color policy for human-facing output.
    #[arg(long, global = true, value_enum, default_value = "auto")]
    color: ColorArg,

    /// Include filtered user frames and underlying diagnostic evidence.
    #[arg(long, global = true)]
    trace: bool,

    /// Override the repository minimum optional log level for this invocation.
    #[arg(long, global = true, value_enum, conflicts_with_all = ["verbose", "quiet"])]
    log_level: Option<LogLevelArg>,

    /// Show one more level of optional operational detail (repeatable).
    #[arg(short = 'v', global = true, action = clap::ArgAction::Count, conflicts_with_all = ["log_level", "quiet"])]
    verbose: u8,

    /// Show one fewer level of optional operational detail (repeatable).
    #[arg(short = 'q', global = true, action = clap::ArgAction::Count, conflicts_with_all = ["log_level", "verbose"])]
    quiet: u8,

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

        /// Repository-defined build inputs. Values must follow `--`.
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,

        /// Construct artifacts without provider reconciliation for a non-local target.
        #[arg(long)]
        compile_only: bool,
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        yes: bool,
        /// Skip requirement checking (packages, commands) for this run.
        #[arg(long)]
        skip_requirements: bool,
        /// Skip `w.script` entries; `w.build.task` entries are unaffected.
        #[arg(long, conflicts_with = "rerun_scripts")]
        skip_scripts: bool,
        #[arg(long)]
        rerun_scripts: bool,
        #[arg(long)]
        allow_host_scripts: bool,
    },
    /// Construct, materialise, or inspect an exact persisted executable plan.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Create the smallest conventional Wombat source repository.
    Init {
        /// Repository path. Defaults to the selected configured/default source.
        path: Option<PathBuf>,
    },
    /// Show or change Wombat's own configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Add an existing home file to Wombat source state.
    Add {
        /// Absolute existing file beneath the target root.
        target: PathBuf,

        /// Root used to derive the target-relative source path. Defaults to the current user's home.
        #[arg(long)]
        target_root: Option<PathBuf>,
    },
    /// Inspect one exact completed product.
    Inspect {
        /// Focused product section. Defaults to the overview.
        #[arg(value_enum, default_value = "overview")]
        section: InspectArg,

        /// Build workspace or product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
    },
    /// Explain one artifact in an exact completed build product.
    Explain {
        /// Artifact target, logical path, or anchored source path.
        artifact: PathBuf,

        /// Build product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
    },
    /// Compare one or two exact completed build products semantically.
    Compare {
        /// With one path, compare default `build` to it; with two, compare them directly.
        #[arg(num_args = 1..=2)]
        products: Vec<PathBuf>,
    },
    /// Compare a completed build product with a target root.
    Diff {
        /// Build product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Deployment root to inspect. Defaults to the current user's home.
        #[arg(long)]
        target_root: Option<PathBuf>,

        /// Include complete patch bodies for creates, removals, and adoptions.
        #[arg(long)]
        patch: bool,
    },
    /// Construct, materialise, then guardedly deploy one exact product.
    Apply {
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
        #[arg(long)]
        target_root: Option<PathBuf>,
        #[arg(long)]
        conflict: Option<ConflictArg>,
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        yes: bool,
        /// Skip requirement checking (packages, commands) for this run.
        #[arg(long)]
        skip_requirements: bool,
        /// Skip `w.script` entries; `w.build.task` entries are unaffected.
        #[arg(long, conflicts_with = "rerun_scripts")]
        skip_scripts: bool,
        #[arg(long)]
        rerun_scripts: bool,
        #[arg(long)]
        allow_host_scripts: bool,
    },
    /// Acquire a repository safely, then run the same guarded apply workflow.
    Setup {
        repository: String,
        #[arg(long)]
        ssh: bool,
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
        #[arg(long)]
        target_root: Option<PathBuf>,
        #[arg(long)]
        conflict: Option<ConflictArg>,
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        yes: bool,
        /// Skip requirement checking (packages, commands) for this run.
        #[arg(long)]
        skip_requirements: bool,
        /// Skip `w.script` entries; `w.build.task` entries are unaffected.
        #[arg(long, conflicts_with = "rerun_scripts")]
        skip_scripts: bool,
        #[arg(long)]
        rerun_scripts: bool,
        #[arg(long)]
        allow_host_scripts: bool,
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
    },
    /// Print a shell completion script to stdout.
    ///
    /// Source it directly, or write it to the completions directory your
    /// shell already scans (for zsh, any directory on `$fpath`).
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Check whether this local environment satisfies a completed build.
    Check {
        /// Build product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
        #[arg(long)]
        compile_only: bool,
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ConflictArg {
    Ask,
    Fail,
    Skip,
    Overwrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ColorArg {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show the resolved source repository and where that choice came from.
    Show,
    /// Record a default source repository, so `--source` is not needed.
    SetSource {
        /// Repository path. Defaults to the current directory.
        path: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum LogLevelArg {
    Debug,
    Info,
    Notice,
    Warn,
    Error,
}

impl From<LogLevelArg> for wombat::LogLevel {
    fn from(value: LogLevelArg) -> Self {
        match value {
            LogLevelArg::Debug => Self::Debug,
            LogLevelArg::Info => Self::Info,
            LogLevelArg::Notice => Self::Notice,
            LogLevelArg::Warn => Self::Warn,
            LogLevelArg::Error => Self::Error,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum InspectArg {
    Overview,
    Inputs,
    Target,
    Modules,
    Dependencies,
    Providers,
    Requirements,
    Ladder,
    Scripts,
    Tasks,
    Artifacts,
    Sources,
    Observations,
    Timeline,
}

#[derive(Debug, Subcommand)]
enum PlanCommand {
    /// Evaluate configuration and atomically persist an executable plan.
    Construct {
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
    },
    /// Execute the stored plan without evaluating configuration Lua.
    Materialise {
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
        #[arg(long)]
        compile_only: bool,
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        yes: bool,
        /// Skip requirement checking (packages, commands) for this run.
        #[arg(long)]
        skip_requirements: bool,
        /// Skip `w.script` entries; `w.build.task` entries are unaffected.
        #[arg(long, conflicts_with = "rerun_scripts")]
        skip_scripts: bool,
        #[arg(long)]
        rerun_scripts: bool,
        #[arg(long)]
        allow_host_scripts: bool,
    },
    /// Inspect an exact stored plan without evaluating configuration Lua.
    Inspect {
        #[arg(value_enum, default_value = "overview")]
        section: PlanInspectArg,
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
    },
    /// Guardedly deploy one exact completed product without reconstruction.
    Deploy {
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
        #[arg(long)]
        target_root: Option<PathBuf>,
        #[arg(long)]
        conflict: Option<ConflictArg>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        allow_plan_mismatch: bool,
        #[arg(long)]
        allow_compile_only: bool,
        /// Skip requirement checking (packages, commands) for this run.
        #[arg(long)]
        skip_requirements: bool,
        /// Skip `w.script` entries; `w.build.task` entries are unaffected.
        #[arg(long, conflicts_with = "rerun_scripts")]
        skip_scripts: bool,
        #[arg(long)]
        rerun_scripts: bool,
        #[arg(long)]
        allow_host_scripts: bool,
    },
}

impl From<InspectArg> for wombat::InspectSection {
    fn from(value: InspectArg) -> Self {
        match value {
            InspectArg::Overview => Self::Overview,
            InspectArg::Inputs => Self::Inputs,
            InspectArg::Target => Self::Target,
            InspectArg::Modules => Self::Modules,
            InspectArg::Dependencies => Self::Dependencies,
            InspectArg::Providers => Self::Providers,
            InspectArg::Requirements => Self::Requirements,
            InspectArg::Ladder => Self::Ladder,
            InspectArg::Scripts => Self::Scripts,
            InspectArg::Tasks => Self::Tasks,
            InspectArg::Artifacts => Self::Artifacts,
            InspectArg::Sources => Self::Sources,
            InspectArg::Observations => Self::Observations,
            InspectArg::Timeline => Self::Timeline,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum PlanInspectArg {
    Overview,
    Providers,
    Requirements,
    Ladder,
    Scripts,
    Tasks,
    Artifacts,
    Sources,
    Observations,
}

impl From<PlanInspectArg> for wombat::PlanInspectSection {
    fn from(value: PlanInspectArg) -> Self {
        match value {
            PlanInspectArg::Overview => Self::Overview,
            PlanInspectArg::Providers => Self::Providers,
            PlanInspectArg::Requirements => Self::Requirements,
            PlanInspectArg::Ladder => Self::Ladder,
            PlanInspectArg::Scripts => Self::Scripts,
            PlanInspectArg::Tasks => Self::Tasks,
            PlanInspectArg::Artifacts => Self::Artifacts,
            PlanInspectArg::Sources => Self::Sources,
            PlanInspectArg::Observations => Self::Observations,
        }
    }
}

impl From<ColorArg> for wombat::ColorPolicy {
    fn from(value: ColorArg) -> Self {
        match value {
            ColorArg::Auto => Self::Auto,
            ColorArg::Always => Self::Always,
            ColorArg::Never => Self::Never,
        }
    }
}

impl From<ConflictArg> for wombat::ConflictPolicy {
    fn from(value: ConflictArg) -> Self {
        match value {
            ConflictArg::Ask => Self::Ask,
            ConflictArg::Fail => Self::Fail,
            ConflictArg::Skip => Self::Skip,
            ConflictArg::Overwrite => Self::Overwrite,
        }
    }
}
