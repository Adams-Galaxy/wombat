use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "wombat", version, about = "A Lua-powered dotfiles compiler")]
struct Cli {
    /// Wombat source repository. Defaults to configured source or ~/.local/share/wombat.
    #[arg(short = 'S', long, global = true)]
    source: Option<PathBuf>,

    /// Terminal color policy for human-facing output.
    #[arg(long, global = true, value_enum, default_value = "auto")]
    color: ColorArg,

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
    },
    /// Add an existing home file to Wombat source state.
    Add {
        /// Absolute existing file beneath the target home.
        target: PathBuf,
    },
    /// Compare a completed build product with a target home.
    Diff {
        /// Build product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Home directory to inspect. Defaults to the current user's home.
        #[arg(long)]
        target_home: Option<PathBuf>,

        /// Include complete patch bodies for creates, removals, and adoptions.
        #[arg(long)]
        patch: bool,
    },
    /// Guardedly reconcile an exact completed build with a target home.
    Apply {
        /// Build product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Home directory to mutate. Defaults to the current user's home.
        #[arg(long)]
        target_home: Option<PathBuf>,

        /// Policy for unmanaged collisions and downstream modifications.
        #[arg(long)]
        conflict: Option<ConflictArg>,
    },
    /// Build once, then guardedly apply that exact build product.
    Deploy {
        /// Build workspace, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Home directory to mutate. Defaults to the current user's home.
        #[arg(long)]
        target_home: Option<PathBuf>,

        /// Policy for unmanaged collisions and downstream modifications.
        #[arg(long)]
        conflict: Option<ConflictArg>,

        /// Repository-defined build inputs. Values must follow `--`.
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

fn main() -> ExitCode {
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(u8::try_from(code).unwrap_or(2));
        }
    };
    let stdout = wombat::Presenter::new(cli.color.into(), io::stdout().is_terminal());
    let stderr = wombat::Presenter::new(cli.color.into(), io::stderr().is_terminal());

    let result = match cli.command {
        Command::Build {
            build_dir,
            project_arguments,
        } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
            if project_arguments == [OsString::from("--help")] {
                let help = wombat::project_help(&source_root, None)?;
                print!("{}", stdout.human_output(&help));
                Ok(())
            } else {
                wombat::build(
                    wombat::BuildOptions::new(source_root, build_dir)
                        .with_project_arguments(project_arguments),
                )
                .map(|outcome| print_build_outcome(&outcome, stdout))
            }
        }),
        Command::Add { target } => wombat::config::resolve_source(cli.source.as_deref())
            .and_then(|source_root| wombat::config::resolve_home().map(|home| (source_root, home)))
            .and_then(|(source_root, home)| wombat::add(&source_root, &home, &target))
            .map(|outcome| println!("{}", stdout.paint(wombat::Role::Success, outcome.display()))),
        Command::Diff {
            build_dir,
            target_home,
            patch,
        } => resolve_deployment_options(cli.source.as_deref(), build_dir, target_home)
            .map(|options| options.with_patch(patch))
            .and_then(|options| wombat::diff(&options))
            .map(|outcome| print!("{}", stdout.human_output(&outcome.output))),
        Command::Apply {
            build_dir,
            target_home,
            conflict,
        } => resolve_deployment_options(cli.source.as_deref(), build_dir, target_home).and_then(
            |options| apply_options(&options, effective_policy(conflict), stdout, stderr),
        ),
        Command::Deploy {
            build_dir,
            target_home,
            conflict,
            project_arguments,
        } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
            if project_arguments == [OsString::from("--help")] {
                let help = wombat::project_help(&source_root, None)?;
                print!("{}", stdout.human_output(&help));
                return Ok(());
            }
            let target_home_explicit = target_home.is_some();
            let target_home = target_home.map_or_else(wombat::config::resolve_home, Ok)?;
            let outcome = wombat::build(
                wombat::BuildOptions::new(&source_root, build_dir)
                    .with_project_arguments(project_arguments),
            )?;
            print_build_outcome(&outcome, stdout);
            let options = wombat::DeploymentOptions::new(&outcome.build_dir, target_home)
                .with_target_home_explicit(target_home_explicit);
            let prepared = wombat::prepare_apply(&options)?;
            if prepared.build_id() != outcome.build_id {
                return Err(wombat::WombatError::configuration(format!(
                    "deploy built `{}` but opened `{}`; refusing to apply a different product",
                    outcome.build_id,
                    prepared.build_id()
                )));
            }
            apply_prepared(prepared, effective_policy(conflict), stdout, stderr)
        }),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}",
                stderr.paint(wombat::Role::Error, format!("error: {error}"))
            );
            ExitCode::FAILURE
        }
    }
}

fn parse_cli() -> std::result::Result<Cli, clap::Error> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let policy = requested_color(&arguments);
    let help = arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h");
    let terminal = if help {
        io::stdout().is_terminal()
    } else {
        io::stderr().is_terminal()
    };
    let presenter = wombat::Presenter::new(policy.into(), terminal);
    let choice = if presenter.color_enabled() {
        clap::ColorChoice::Always
    } else {
        clap::ColorChoice::Never
    };
    let matches = Cli::command()
        .color(choice)
        .try_get_matches_from(arguments)?;
    Cli::from_arg_matches(&matches)
}

fn requested_color(arguments: &[OsString]) -> ColorArg {
    for (index, argument) in arguments.iter().enumerate() {
        if argument == "--" {
            break;
        }
        if let Some(argument) = argument.to_str() {
            if let Some(value) = argument.strip_prefix("--color=") {
                return parse_color(value);
            }
            if argument == "--color"
                && let Some(value) = arguments.get(index + 1).and_then(|value| value.to_str())
            {
                return parse_color(value);
            }
        }
    }
    ColorArg::Auto
}

fn parse_color(value: &str) -> ColorArg {
    match value {
        "always" => ColorArg::Always,
        "never" => ColorArg::Never,
        _ => ColorArg::Auto,
    }
}

fn print_build_outcome(outcome: &wombat::BuildOutcome, presenter: wombat::Presenter) {
    println!(
        "{} {} ({} artifacts) at {}",
        presenter.paint(wombat::Role::Success, outcome.status.to_string()),
        presenter.paint(wombat::Role::Identity, &outcome.build_id),
        outcome.artifact_count,
        presenter.paint(wombat::Role::Path, outcome.build_dir.to_string_lossy(),)
    );
}

fn resolve_deployment_options(
    source: Option<&std::path::Path>,
    build_dir: PathBuf,
    target_home: Option<PathBuf>,
) -> wombat::Result<wombat::DeploymentOptions> {
    let build_dir = if build_dir.is_absolute() {
        build_dir
    } else {
        wombat::config::resolve_source(source)?.join(build_dir)
    };
    let target_home_explicit = target_home.is_some();
    let target_home = target_home.map_or_else(wombat::config::resolve_home, Ok)?;
    Ok(wombat::DeploymentOptions::new(build_dir, target_home)
        .with_target_home_explicit(target_home_explicit))
}

fn effective_policy(explicit: Option<ConflictArg>) -> wombat::ConflictPolicy {
    explicit.map_or_else(
        || {
            if io::stdin().is_terminal() && io::stderr().is_terminal() {
                wombat::ConflictPolicy::Ask
            } else {
                wombat::ConflictPolicy::Fail
            }
        },
        Into::into,
    )
}

fn apply_options(
    options: &wombat::DeploymentOptions,
    policy: wombat::ConflictPolicy,
    stdout: wombat::Presenter,
    stderr: wombat::Presenter,
) -> wombat::Result<()> {
    if policy == wombat::ConflictPolicy::Ask {
        let prepared = wombat::prepare_apply(options)?;
        apply_prepared(prepared, policy, stdout, stderr)
    } else {
        wombat::apply(options, policy).map(|outcome| print_apply_outcome(outcome, stdout, stderr))
    }
}

fn apply_prepared(
    prepared: wombat::PreparedApply,
    policy: wombat::ConflictPolicy,
    stdout: wombat::Presenter,
    stderr: wombat::Presenter,
) -> wombat::Result<()> {
    for warning in prepared.warnings() {
        eprintln!(
            "{}",
            stderr.paint(wombat::Role::Warning, format!("warning: {warning}"))
        );
    }
    let conflicts = prepared
        .plan()
        .conflicts()
        .map(|item| (item.target.clone(), item.reason.clone()))
        .collect::<Vec<_>>();
    let mut resolutions = std::collections::BTreeMap::new();
    match policy {
        wombat::ConflictPolicy::Ask => {
            for (target, reason) in conflicts {
                loop {
                    eprint!(
                        "{}\n{} ",
                        stderr.paint(
                            wombat::Role::Error,
                            format!(
                                "conflict at {target}: {}",
                                reason.as_deref().unwrap_or("target conflict")
                            ),
                        ),
                        stderr.paint(
                            wombat::Role::Heading,
                            "[d]iff, [s]kip, [o]verwrite, [a]bort:"
                        ),
                    );
                    io::stderr()
                        .flush()
                        .map_err(|error| wombat::WombatError::io("<stderr>", error))?;
                    let mut answer = String::new();
                    io::stdin()
                        .read_line(&mut answer)
                        .map_err(|error| wombat::WombatError::io("<stdin>", error))?;
                    match answer.trim().to_ascii_lowercase().as_str() {
                        "d" | "diff" => {
                            let rendered = prepared.rendered_diff_for(&target)?;
                            eprint!("{}", stderr.human_output(&rendered));
                        }
                        "s" | "skip" => {
                            resolutions.insert(target.clone(), wombat::ConflictResolution::Skip);
                            break;
                        }
                        "o" | "overwrite" => {
                            resolutions
                                .insert(target.clone(), wombat::ConflictResolution::Overwrite);
                            break;
                        }
                        "a" | "abort" => {
                            return Err(wombat::WombatError::configuration(
                                "deployment aborted by user",
                            ));
                        }
                        _ => eprintln!(
                            "{}",
                            stderr.paint(
                                wombat::Role::Warning,
                                "enter diff, skip, overwrite, or abort"
                            )
                        ),
                    }
                }
            }
        }
        wombat::ConflictPolicy::Fail => {
            if prepared.plan().has_conflicts() {
                return Err(wombat::deploy::conflict_error(prepared.plan()));
            }
        }
        wombat::ConflictPolicy::Skip => {
            resolutions.extend(
                conflicts
                    .into_iter()
                    .map(|(target, _)| (target, wombat::ConflictResolution::Skip)),
            );
        }
        wombat::ConflictPolicy::Overwrite => {
            resolutions.extend(
                conflicts
                    .into_iter()
                    .map(|(target, _)| (target, wombat::ConflictResolution::Overwrite)),
            );
        }
    }
    prepared
        .apply(&resolutions)
        .map(|outcome| print_apply_outcome(outcome, stdout, stderr))
}

fn print_apply_outcome(
    outcome: wombat::ApplyOutcome,
    stdout: wombat::Presenter,
    stderr: wombat::Presenter,
) {
    for warning in &outcome.warnings {
        eprintln!(
            "{}",
            stderr.paint(wombat::Role::Warning, format!("warning: {warning}"))
        );
    }
    println!(
        "{} {} ({} created, {} updated, {} removed, {} state-only, {} skipped)",
        stdout.paint(wombat::Role::Success, outcome.status.to_string()),
        stdout.paint(wombat::Role::Identity, &outcome.build_id),
        outcome.created,
        outcome.updated,
        outcome.removed,
        outcome.state_advanced,
        outcome.skipped.len()
    );
    for target in outcome.skipped {
        println!(
            "{} {}",
            stdout.paint(wombat::Role::Warning, "skipped"),
            stdout.paint(wombat::Role::Path, target)
        );
    }
}
