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

    /// Include filtered user frames and underlying diagnostic evidence.
    #[arg(long, global = true)]
    trace: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Acquire a dotfiles repository, build it, bootstrap it, and deploy it.
    Setup {
        /// Repository owner shorthand, owner/repository, URL, or explicit local path.
        repository: String,

        /// Expand GitHub shorthand using SSH instead of HTTPS.
        #[arg(long)]
        ssh: bool,

        /// Build workspace, relative to the acquired source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Deployment root to mutate. Defaults to the current user's home.
        #[arg(long)]
        target_root: Option<PathBuf>,

        /// Policy for unmanaged collisions and downstream modifications.
        #[arg(long)]
        conflict: Option<ConflictArg>,

        /// Confirm package bootstrap non-interactively.
        #[arg(long)]
        yes: bool,

        /// Require requirements to be satisfied without changing packages.
        #[arg(long)]
        no_bootstrap: bool,

        /// Require host construction tools to exist without preparing them.
        #[arg(long)]
        no_prepare: bool,

        /// Stop after requirements are satisfied.
        #[arg(long)]
        no_deploy: bool,

        /// Repository-defined build inputs. Values must follow `--`.
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
    },
    /// Evaluate and materialise a completed static build product.
    Build {
        /// Build workspace, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Repository-defined build inputs. Values must follow `--`.
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
    },
    /// Explicitly reconcile host tools required to construct the current plan.
    Prepare {
        /// Build workspace, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Confirm the complete host preparation plan non-interactively.
        #[arg(long)]
        yes: bool,

        /// Repository-defined build inputs. Values must follow `--`.
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
    },
    /// Create the smallest conventional Wombat source repository.
    Init {
        /// Repository path. Defaults to the selected configured/default source.
        path: Option<PathBuf>,
    },
    /// Add an existing home file to Wombat source state.
    Add {
        /// Absolute existing file beneath the target root.
        target: PathBuf,

        /// Root used to derive the target-relative source path. Defaults to the current user's home.
        #[arg(long)]
        target_root: Option<PathBuf>,
    },
    /// Inspect a pending construction plan or one exact completed product.
    Inspect {
        /// Focused product section. Defaults to the overview.
        #[arg(value_enum, default_value = "overview")]
        section: InspectArg,

        /// Focused pending-plan section when `section` is `plan`.
        #[arg(value_enum)]
        plan_section: Option<PlanInspectArg>,

        /// Build workspace or product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Repository-defined build inputs for pending-plan inspection.
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
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
    /// Guardedly reconcile an exact completed build with a target root.
    Apply {
        /// Build product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Deployment root to mutate. Defaults to the current user's home.
        #[arg(long)]
        target_root: Option<PathBuf>,

        /// Policy for unmanaged collisions and downstream modifications.
        #[arg(long)]
        conflict: Option<ConflictArg>,
    },
    /// Build once, then guardedly apply that exact build product.
    Deploy {
        /// Build workspace, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Deployment root to mutate. Defaults to the current user's home.
        #[arg(long)]
        target_root: Option<PathBuf>,

        /// Policy for unmanaged collisions and downstream modifications.
        #[arg(long)]
        conflict: Option<ConflictArg>,

        /// Repository-defined build inputs. Values must follow `--`.
        #[arg(last = true, allow_hyphen_values = true)]
        project_arguments: Vec<OsString>,
    },
    /// Check whether this local environment satisfies a completed build.
    Check {
        /// Build product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,
    },
    /// Explicitly reconcile requirements for a completed local build.
    Bootstrap {
        /// Build product, relative to the resolved source unless absolute.
        #[arg(short = 'B', long = "build-dir", default_value = "build")]
        build_dir: PathBuf,

        /// Confirm the complete bootstrap plan non-interactively.
        #[arg(long)]
        yes: bool,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum InspectArg {
    Plan,
    Overview,
    Inputs,
    Target,
    Modules,
    Dependencies,
    Providers,
    Requirements,
    Tasks,
    Artifacts,
    Sources,
}

impl From<InspectArg> for wombat::InspectSection {
    fn from(value: InspectArg) -> Self {
        match value {
            InspectArg::Plan => unreachable!("plan inspection has a distinct surface"),
            InspectArg::Overview => Self::Overview,
            InspectArg::Inputs => Self::Inputs,
            InspectArg::Target => Self::Target,
            InspectArg::Modules => Self::Modules,
            InspectArg::Dependencies => Self::Dependencies,
            InspectArg::Providers => Self::Providers,
            InspectArg::Requirements => Self::Requirements,
            InspectArg::Tasks => Self::Tasks,
            InspectArg::Artifacts => Self::Artifacts,
            InspectArg::Sources => Self::Sources,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum PlanInspectArg {
    Overview,
    Providers,
    Requirements,
    Tasks,
    Artifacts,
    Sources,
}

impl From<PlanInspectArg> for wombat::PlanInspectSection {
    fn from(value: PlanInspectArg) -> Self {
        match value {
            PlanInspectArg::Overview => Self::Overview,
            PlanInspectArg::Providers => Self::Providers,
            PlanInspectArg::Requirements => Self::Requirements,
            PlanInspectArg::Tasks => Self::Tasks,
            PlanInspectArg::Artifacts => Self::Artifacts,
            PlanInspectArg::Sources => Self::Sources,
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
    let trace = cli.trace;
    let check_command = matches!(&cli.command, Command::Check { .. });

    let mut requested_exit = 0u8;
    let result = match cli.command {
        Command::Setup {
            repository,
            ssh,
            build_dir,
            target_root,
            conflict,
            yes,
            no_bootstrap,
            no_prepare,
            no_deploy,
            project_arguments,
        } => (|| -> wombat::Result<()> {
            let destination = wombat::config::resolve_source_candidate(cli.source.as_deref())?;
            let locator = wombat::RepositoryLocator::parse(&repository, ssh)?;
            let acquired = wombat::acquire_repository(locator, &destination)?;
            println!(
                "{} repository {} at {}",
                stdout.paint(
                    wombat::Role::Success,
                    match acquired.status {
                        wombat::AcquisitionStatus::Cloned => "cloned",
                        wombat::AcquisitionStatus::Reused => "reused",
                    }
                ),
                stdout.paint(wombat::Role::Identity, &repository),
                stdout.paint(wombat::Role::Path, acquired.destination.to_string_lossy())
            );
            let source_root = wombat::config::resolve_source(Some(&acquired.destination))?;
            let target_root_explicit = target_root.is_some();
            let target_root = target_root.map_or_else(wombat::config::resolve_home, Ok)?;
            let build_options =
                configured_build_options(&source_root, build_dir, project_arguments)?;
            let planned = wombat::plan(build_options.clone())?;
            let host_requirements = wombat::check_plan(&planned.build_dir, &planned.plan)?;
            let target_requirements = wombat::check_target_plan(&planned.build_dir, &planned.plan)?;
            println!(
                "{}",
                stdout.paint(
                    wombat::Role::Heading,
                    format!("setup plan {}", planned.plan.plan_id)
                )
            );
            println!(
                "{}",
                stdout.paint(wombat::Role::Heading, "host construction requirements")
            );
            print!("{}", stdout.human_output(&host_requirements.display()));
            println!(
                "{}",
                stdout.paint(wombat::Role::Heading, "target environment requirements")
            );
            print!("{}", stdout.human_output(&target_requirements.display()));
            println!(
                "  deployment: {} ({})",
                stdout.paint(wombat::Role::Path, target_root.to_string_lossy()),
                if no_deploy { "disabled" } else { "guarded" }
            );
            if host_requirements.operational_failure() || target_requirements.operational_failure()
            {
                return Err(wombat::WombatError::configuration(
                    "setup cannot continue because requirement checking failed operationally",
                ));
            }
            if no_prepare && !host_requirements.satisfied() {
                return Err(wombat::WombatError::configuration(
                    "setup --no-prepare requires every host build requirement to be satisfied",
                ));
            }
            if no_bootstrap && !target_requirements.satisfied() {
                return Err(wombat::WombatError::configuration(
                    "setup --no-bootstrap requires every target requirement to be satisfied",
                ));
            }
            let will_mutate = (!no_prepare && !host_requirements.satisfied())
                || (!no_bootstrap && !target_requirements.satisfied());
            if will_mutate && !yes {
                confirm_setup()?;
            }
            if !no_prepare && !host_requirements.satisfied() {
                let prepared = wombat::prepare_plan(&planned.build_dir, &planned.plan, true)?;
                print!(
                    "{}",
                    stdout.human_output(&format!(
                        "prepare complete for {} ({} changed, {} already satisfied)\n",
                        prepared.build_id,
                        prepared.completed.len(),
                        prepared.already_satisfied.len()
                    ))
                );
                let current = wombat::plan(build_options.clone())?;
                if current.plan.plan_id != planned.plan.plan_id {
                    return Err(wombat::WombatError::configuration(format!(
                        "setup plan changed during host preparation: `{}` became `{}`; no rollback was attempted",
                        planned.plan.plan_id, current.plan.plan_id
                    )));
                }
            }
            let outcome = wombat::build(build_options)?;
            if outcome.manifest.plan_id != planned.plan.plan_id {
                return Err(wombat::WombatError::configuration(format!(
                    "setup planned `{}` but built `{}`; refusing further mutation",
                    planned.plan.plan_id, outcome.manifest.plan_id
                )));
            }
            print_build_outcome(&outcome, stdout, stderr);
            let initial = wombat::check(&outcome.build_dir)?;
            print!("{}", stdout.human_output(&initial.display()));
            if initial.operational_failure() {
                return Err(wombat::WombatError::configuration(
                    "setup cannot continue because requirement checking failed operationally",
                ));
            }
            if !no_bootstrap && !initial.satisfied() {
                let bootstrapped =
                    wombat::bootstrap_exact(&outcome.build_dir, true, &outcome.build_id)?;
                print!("{}", stdout.human_output(&bootstrapped.display()));
            }
            if no_deploy {
                Ok(())
            } else {
                let options = wombat::DeploymentOptions::new(&outcome.build_dir, target_root)
                    .with_target_root_explicit(target_root_explicit);
                let prepared = wombat::prepare_apply(&options)?;
                if prepared.build_id() != outcome.build_id {
                    Err(wombat::WombatError::configuration(
                        "setup built and prepared different products; refusing deployment",
                    ))
                } else {
                    apply_prepared(prepared, effective_policy(conflict), stdout, stderr)
                }
            }
        })(),
        Command::Build {
            build_dir,
            project_arguments,
        } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
            if project_arguments == [OsString::from("--help")] {
                let help = wombat::project_help_with_options(configured_build_options(
                    &source_root,
                    build_dir,
                    std::iter::empty::<OsString>(),
                )?)?;
                print!("{}", stdout.human_output(&help));
                Ok(())
            } else {
                wombat::build(configured_build_options(
                    source_root,
                    build_dir,
                    project_arguments,
                )?)
                .map(|outcome| print_build_outcome(&outcome, stdout, stderr))
            }
        }),
        Command::Prepare {
            build_dir,
            yes,
            project_arguments,
        } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
            wombat::prepare(
                configured_build_options(source_root, build_dir, project_arguments)?,
                yes,
            )
            .map(|outcome| print!("{}", stdout.human_output(&outcome.display())))
        }),
        Command::Init { path } => {
            let selected = match (cli.source.as_deref(), path.as_deref()) {
                (Some(_), Some(_)) => Err(wombat::WombatError::configuration(
                    "wombat init accepts either --source or PATH, not both",
                )),
                (source, path) => wombat::config::resolve_source_candidate(path.or(source)),
            };
            selected
                .and_then(|root| wombat::initialize(&root))
                .map(|outcome| {
                    println!("{}", stdout.paint(wombat::Role::Success, outcome.display()));
                    if let Some(warning) = outcome.warning {
                        eprintln!(
                            "{}",
                            stderr.paint(wombat::Role::Warning, format!("warning: {warning}"))
                        );
                    }
                })
        }
        Command::Add {
            target,
            target_root,
        } => wombat::config::resolve_source(cli.source.as_deref())
            .and_then(|source_root| {
                target_root
                    .map_or_else(wombat::config::resolve_home, Ok)
                    .map(|root| (source_root, root))
            })
            .and_then(|(source_root, target_root)| wombat::add(&source_root, &target_root, &target))
            .map(|outcome| println!("{}", stdout.paint(wombat::Role::Success, outcome.display()))),
        Command::Inspect {
            section,
            plan_section,
            build_dir,
            project_arguments,
        } => match section {
            InspectArg::Plan => wombat::config::resolve_source(cli.source.as_deref())
                .and_then(|source_root| {
                    wombat::plan(configured_build_options(
                        source_root,
                        build_dir,
                        project_arguments,
                    )?)
                })
                .map(|outcome| {
                    let output = wombat::inspect_plan(
                        &outcome.plan,
                        plan_section.unwrap_or(PlanInspectArg::Overview).into(),
                    );
                    print!("{}", stdout.human_output(&output));
                }),
            section => {
                if plan_section.is_some() || !project_arguments.is_empty() {
                    Err(wombat::WombatError::configuration(
                        "completed-product inspection does not accept a plan section or project arguments",
                    ))
                } else {
                    resolve_product_path(cli.source.as_deref(), build_dir)
                        .and_then(|(build_dir, _)| wombat::inspect(&build_dir, section.into()))
                        .map(|output| print!("{}", stdout.human_output(&output)))
                }
            }
        },
        Command::Explain {
            artifact,
            build_dir,
        } => resolve_product_path(cli.source.as_deref(), build_dir).and_then(
            |(build_dir, source_root)| {
                let selector = artifact.to_str().ok_or_else(|| {
                    wombat::WombatError::configuration("artifact selectors must be valid UTF-8")
                })?;
                let home = wombat::config::resolve_home().ok();
                let output = wombat::explain(
                    &build_dir,
                    selector,
                    source_root.as_deref(),
                    home.as_deref(),
                )?;
                print!("{}", stdout.human_output(&output));
                Ok(())
            },
        ),
        Command::Compare { products } => {
            let (left, right) = match products.as_slice() {
                [right] => (PathBuf::from("build"), right.clone()),
                [left, right] => (left.clone(), right.clone()),
                _ => unreachable!("clap constrains comparison operands"),
            };
            resolve_product_path(cli.source.as_deref(), left)
                .and_then(|(left, _)| {
                    resolve_product_path(cli.source.as_deref(), right)
                        .and_then(|(right, _)| wombat::compare(&left, &right))
                })
                .map(|output| print!("{}", stdout.human_output(&output)))
        }
        Command::Diff {
            build_dir,
            target_root,
            patch,
        } => resolve_deployment_options(cli.source.as_deref(), build_dir, target_root)
            .map(|options| options.with_patch(patch))
            .and_then(|options| wombat::diff(&options))
            .map(|outcome| print!("{}", stdout.human_output(&outcome.output))),
        Command::Apply {
            build_dir,
            target_root,
            conflict,
        } => resolve_deployment_options(cli.source.as_deref(), build_dir, target_root).and_then(
            |options| apply_options(&options, effective_policy(conflict), stdout, stderr),
        ),
        Command::Deploy {
            build_dir,
            target_root,
            conflict,
            project_arguments,
        } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
            if project_arguments == [OsString::from("--help")] {
                let help = wombat::project_help_with_options(configured_build_options(
                    &source_root,
                    build_dir,
                    std::iter::empty::<OsString>(),
                )?)?;
                print!("{}", stdout.human_output(&help));
                return Ok(());
            }
            let target_root_explicit = target_root.is_some();
            let target_root = target_root.map_or_else(wombat::config::resolve_home, Ok)?;
            let outcome = wombat::build(configured_build_options(
                &source_root,
                build_dir,
                project_arguments,
            )?)?;
            print_build_outcome(&outcome, stdout, stderr);
            let options = wombat::DeploymentOptions::new(&outcome.build_dir, target_root)
                .with_target_root_explicit(target_root_explicit);
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
        Command::Check { build_dir } => resolve_product_path(cli.source.as_deref(), build_dir)
            .and_then(|(build_dir, _)| wombat::check(&build_dir))
            .map(|outcome| {
                print!("{}", stdout.human_output(&outcome.display()));
                requested_exit = if outcome.operational_failure() {
                    2
                } else if outcome.satisfied() {
                    0
                } else {
                    1
                };
            }),
        Command::Bootstrap { build_dir, yes } => {
            resolve_product_path(cli.source.as_deref(), build_dir)
                .and_then(|(build_dir, _)| wombat::bootstrap(&build_dir, yes))
                .map(|outcome| print!("{}", stdout.human_output(&outcome.display())))
        }
    };

    match result {
        Ok(()) => ExitCode::from(requested_exit),
        Err(error) => {
            eprint!("{}", stderr.human_output(&error.render(trace)));
            ExitCode::from(if check_command { 2 } else { 1 })
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

fn print_build_outcome(
    outcome: &wombat::BuildOutcome,
    presenter: wombat::Presenter,
    warnings: wombat::Presenter,
) {
    println!(
        "{} {} ({} artifacts) at {}",
        presenter.paint(wombat::Role::Success, outcome.status.to_string()),
        presenter.paint(wombat::Role::Identity, &outcome.build_id),
        outcome.artifact_count,
        presenter.paint(wombat::Role::Path, outcome.build_dir.to_string_lossy(),)
    );
    for notice in &outcome.manifest.artifact_notices {
        let paths = notice
            .skipped
            .iter()
            .map(|path| format!("`{path}`"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "{}",
            warnings.paint(
                wombat::Role::Warning,
                format!(
                    "warning: artifact selector `{}` owned by `{}` skipped unallocated source{} {paths} at {}",
                    notice.selector,
                    notice.owner,
                    if notice.skipped.len() == 1 { "" } else { "s" },
                    notice.declared_at,
                ),
            )
        );
    }
}

fn configured_build_options(
    source_root: impl Into<PathBuf>,
    build_dir: impl Into<PathBuf>,
    project_arguments: impl IntoIterator<Item = impl Into<OsString>>,
) -> wombat::Result<wombat::BuildOptions> {
    Ok(wombat::BuildOptions::new(source_root, build_dir)
        .with_project_arguments(project_arguments)
        .with_task_interpreters(wombat::config::resolve_task_interpreters()?))
}

fn resolve_deployment_options(
    source: Option<&std::path::Path>,
    build_dir: PathBuf,
    target_root: Option<PathBuf>,
) -> wombat::Result<wombat::DeploymentOptions> {
    let build_dir = if build_dir.is_absolute() {
        build_dir
    } else {
        wombat::config::resolve_source(source)?.join(build_dir)
    };
    let target_root_explicit = target_root.is_some();
    let target_root = target_root.map_or_else(wombat::config::resolve_home, Ok)?;
    Ok(wombat::DeploymentOptions::new(build_dir, target_root)
        .with_target_root_explicit(target_root_explicit))
}

fn resolve_product_path(
    source: Option<&std::path::Path>,
    build_dir: PathBuf,
) -> wombat::Result<(PathBuf, Option<PathBuf>)> {
    if build_dir.is_absolute() {
        let source_root = source
            .map(|source| wombat::config::resolve_source(Some(source)))
            .transpose()?;
        return Ok((build_dir, source_root));
    }
    let source_root = wombat::config::resolve_source(source)?;
    Ok((source_root.join(build_dir), Some(source_root)))
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

fn confirm_setup() -> wombat::Result<()> {
    if !io::stdin().is_terminal() {
        return Err(wombat::WombatError::configuration(
            "setup requires --yes for pending host or target requirement changes when standard input is not a terminal",
        ));
    }
    eprint!("continue with this setup plan? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|error| wombat::WombatError::io("<stderr>", error))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| wombat::WombatError::io("<stdin>", error))?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(wombat::WombatError::configuration("setup cancelled"))
    }
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
