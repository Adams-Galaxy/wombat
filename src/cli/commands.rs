//! Command dispatch and human interaction.

use super::*;

pub(crate) fn run() -> ExitCode {
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
    wombat::presentation::install_human_event_sink(stderr);
    let trace = cli.trace;
    let requested_log_level = cli.log_level.map(Into::into);
    let log_adjustment = cli.verbose as i8 - cli.quiet as i8;
    let check_command = matches!(&cli.command, Command::Check { .. });

    let mut requested_exit = 0u8;
    let result = match cli.command {
        Command::Build {
            build_dir,
            project_arguments,
            compile_only,
            clean,
            yes,
            rerun_scripts,
            allow_host_scripts,
        } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
            if project_arguments == [OsString::from("--help")] {
                let help = wombat::project_help_with_options(with_log_level(
                    configured_build_options(
                        &source_root,
                        build_dir,
                        std::iter::empty::<OsString>(),
                    )?,
                    requested_log_level,
                    log_adjustment,
                ))?;
                print!("{}", stdout.human_output(&help));
                Ok(())
            } else {
                wombat::build(with_log_level(
                    configured_build_options(source_root, build_dir, project_arguments)?,
                    requested_log_level,
                    log_adjustment,
                ).with_compile_only(compile_only).with_yes(yes).with_clean(clean).with_provider_reconciliation(true).with_rerun_scripts(rerun_scripts).with_allow_host_scripts(allow_host_scripts))
                .map(|outcome| print_build_outcome(&outcome, stdout, stderr))
            }
        }),
        Command::Plan { command } => match command {
            PlanCommand::Construct {
                build_dir,
                project_arguments,
            } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
                wombat::plan(with_log_level(
                    configured_build_options(source_root, build_dir, project_arguments)?,
                    requested_log_level,
                    log_adjustment,
                ))
                .map(|outcome| {
                    println!(
                        "{} plan {}",
                        stdout.paint(wombat::Role::Success, "constructed"),
                        stdout.paint(wombat::Role::Identity, outcome.plan.plan_id)
                    );
                })
            }),
            PlanCommand::Materialise {
                build_dir,
                compile_only,
                clean,
                yes,
                rerun_scripts,
                allow_host_scripts,
            } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
                wombat::materialise(configured_build_options(
                    source_root,
                    build_dir,
                    std::iter::empty::<OsString>(),
                )?.with_compile_only(compile_only).with_yes(yes).with_clean(clean).with_provider_reconciliation(true).with_rerun_scripts(rerun_scripts).with_allow_host_scripts(allow_host_scripts))
                .map(|outcome| print_build_outcome(&outcome, stdout, stderr))
            }),
            PlanCommand::Inspect { section, build_dir } => {
                wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
                    let build_dir = if build_dir.is_absolute() {
                        build_dir
                    } else {
                        source_root.join(build_dir)
                    };
                    wombat::plan::read(&build_dir).map(|plan| {
                        print!(
                            "{}",
                            stdout.human_output(&wombat::inspect_plan(&plan, section.into()))
                        );
                    })
                })
            }
            PlanCommand::Deploy {
                build_dir,
                target_root,
                conflict,
                yes,
                allow_plan_mismatch,
                allow_compile_only,
                rerun_scripts,
                allow_host_scripts,
            } => resolve_deployment_options(cli.source.as_deref(), build_dir, target_root)
                .and_then(|options| {
                    let opened = wombat::open_build(&options.build_dir)?;
                    let materialisation = wombat::ladder::read(&options.build_dir)?;
                    if materialisation.plan_id != opened.manifest.plan_id
                        || !materialisation.rungs.iter().any(|(rung, status)| {
                            rung.id()
                                == wombat::ladder::CoreRung::MaterialiseAfter.id()
                                && *status == wombat::ladder::ExecutionStatus::Succeeded
                        })
                    {
                        return Err(wombat::WombatError::configuration(
                            "refusing deployment because materialisation has not completed successfully",
                        ));
                    }
                    if opened.manifest.execution_mode == wombat::manifest::ExecutionMode::CompileOnly
                        && !allow_compile_only
                    {
                        return Err(wombat::WombatError::configuration(
                            "refusing to deploy a compile-only product without --allow-compile-only",
                        ));
                    }
                    if let Ok(pending) = wombat::plan::read(&options.build_dir)
                        && pending.plan_id != opened.manifest.plan_id
                        && !allow_plan_mismatch
                    {
                        confirm_plan_mismatch(&opened.manifest.plan_id, &pending.plan_id)?;
                    }
                    apply_options(
                        &options
                            .with_yes(yes)
                            .with_provider_reconciliation(true)
                            .with_rerun_scripts(rerun_scripts)
                            .with_allow_host_scripts(allow_host_scripts),
                        effective_policy(conflict),
                        stdout,
                        stderr,
                    )
                }),
        },
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
        Command::Config { command } => match command {
            ConfigCommand::Show => wombat::config::describe_source(cli.source.as_deref())
                .map(|resolution| {
                    let origin = match resolution.origin {
                        wombat::config::SourceOrigin::Explicit => "--source argument",
                        wombat::config::SourceOrigin::Configured => "configured repository",
                        wombat::config::SourceOrigin::Default => "built-in default",
                    };
                    println!(
                        "source: {}",
                        stdout.paint(wombat::Role::Path, resolution.source.display().to_string())
                    );
                    println!("  from: {origin}");
                    println!(
                        "config: {}{}",
                        resolution.config_path.display(),
                        if resolution.config_exists {
                            ""
                        } else {
                            " (not present)"
                        }
                    );
                    if !resolution.source.join("wombat.lua").exists() {
                        eprintln!(
                            "{}",
                            stderr.paint(
                                wombat::Role::Warning,
                                "warning: no `wombat.lua` there yet; run `wombat init` to create one"
                            )
                        );
                    }
                }),
            ConfigCommand::SetSource { path } => (|| -> wombat::Result<()> {
                let selected = wombat::config::resolve_source_candidate(path.as_deref())?;
                let recorded = wombat::config::set_configured_source(&selected)?;
                println!(
                    "{} {}",
                    stdout.paint(wombat::Role::Success, "source"),
                    stdout.paint(wombat::Role::Path, recorded.source.display().to_string())
                );
                println!("recorded in {}", recorded.config_path.display());
                if !recorded.source.join("wombat.lua").exists() {
                    eprintln!(
                        "{}",
                        stderr.paint(
                            wombat::Role::Warning,
                            "warning: no `wombat.lua` there yet; run `wombat init` to create one"
                        )
                    );
                }
                Ok(())
            })(),
        },
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
        Command::Inspect { section, build_dir } => {
            resolve_product_path(cli.source.as_deref(), build_dir)
                .and_then(|(build_dir, _)| wombat::inspect(&build_dir, section.into()))
                .map(|output| print!("{}", stdout.human_output(&output)))
        }
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
            project_arguments,
            clean,
            yes,
            rerun_scripts,
            allow_host_scripts,
        } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
            let target_root_explicit = target_root.is_some();
            let target_root = target_root.map_or_else(wombat::config::resolve_home, Ok)?;
            let mut outcome = wombat::build(with_log_level(
                configured_build_options(source_root, build_dir, project_arguments)?,
                requested_log_level,
                log_adjustment,
            ).with_yes(yes).with_clean(clean).with_provider_reconciliation(true).with_rerun_scripts(rerun_scripts).with_allow_host_scripts(allow_host_scripts)
                .with_requirement_boundary(wombat::ladder::CoreRung::DeployAfter))?;
            print_build_outcome(&outcome, stdout, stderr);
            let options = wombat::DeploymentOptions::new(&outcome.build_dir, target_root)
                .with_target_root_explicit(target_root_explicit)
                .with_yes(yes)
                .with_provider_reconciliation(true)
                .with_clean(clean)
                .with_rerun_scripts(rerun_scripts)
                .with_allow_host_scripts(allow_host_scripts)
                .with_requirement_authorization(outcome.requirement_authorization.take());
            apply_options(&options, effective_policy(conflict), stdout, stderr)
        }),
        Command::Setup {
            repository,
            ssh,
            build_dir,
            target_root,
            conflict,
            clean,
            yes,
            rerun_scripts,
            allow_host_scripts,
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
            let target_root_explicit = target_root.is_some();
            let target_root = target_root.map_or_else(wombat::config::resolve_home, Ok)?;
            let mut outcome = wombat::build(
                with_log_level(
                    configured_build_options(&acquired.destination, build_dir, project_arguments)?,
                    requested_log_level,
                    log_adjustment,
                )
                .with_clean(clean)
                .with_yes(yes)
                .with_provider_reconciliation(true)
                .with_rerun_scripts(rerun_scripts)
                .with_allow_host_scripts(allow_host_scripts)
                .with_requirement_boundary(wombat::ladder::CoreRung::DeployAfter),
            )?;
            print_build_outcome(&outcome, stdout, stderr);
            let options = wombat::DeploymentOptions::new(&outcome.build_dir, target_root)
                .with_target_root_explicit(target_root_explicit)
                .with_yes(yes)
                .with_provider_reconciliation(true)
                .with_clean(clean)
                .with_rerun_scripts(rerun_scripts)
                .with_allow_host_scripts(allow_host_scripts)
                .with_requirement_authorization(outcome.requirement_authorization.take());
            apply_options(&options, effective_policy(conflict), stdout, stderr)
        })(),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "wombat", &mut io::stdout());
            Ok(())
        }
        Command::Check { build_dir, compile_only, project_arguments } => wombat::config::resolve_source(cli.source.as_deref()).and_then(|source_root| {
            let planned = wombat::plan_or_reuse(with_log_level(
                configured_build_options(&source_root, build_dir, project_arguments)?,
                requested_log_level,
                log_adjustment,
            ))?;
            if compile_only {
                wombat::build::check_compile_only_plan(
                    &source_root,
                    &planned.build_dir,
                    &planned.plan,
                )?;
                println!("compile-only check: provider gates are disabled");
                return Ok(());
            }
            wombat::build::check_plan_execution(&planned.build_dir, &planned.plan)?;
            wombat::check_target_plan(&planned.build_dir, &planned.plan).map(|outcome| {
                print!("{}", stdout.human_output(&outcome.display()));
                requested_exit = if outcome.operational_failure() { 2 } else if outcome.satisfied() { 0 } else { 1 };
            })
        }),
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
        .with_task_interpreters(wombat::config::resolve_runners()?))
}

fn with_log_level(
    options: wombat::BuildOptions,
    level: Option<wombat::LogLevel>,
    adjustment: i8,
) -> wombat::BuildOptions {
    match level {
        Some(level) => options.with_log_level(level),
        None => options.with_log_adjustment(adjustment),
    }
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

fn confirm_plan_mismatch(product_plan: &str, pending_plan: &str) -> wombat::Result<()> {
    if !io::stdin().is_terminal() {
        return Err(wombat::WombatError::configuration(format!(
            "product plan `{product_plan}` differs from pending plan `{pending_plan}`; non-interactive deployment requires --allow-plan-mismatch"
        )));
    }
    eprintln!("warning: product plan `{product_plan}` differs from pending plan `{pending_plan}`");
    eprint!("deploy the older materialised product anyway? [y/N] ");
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
        Err(wombat::WombatError::configuration(
            "deployment cancelled because the pending plan differs from the materialised product",
        ))
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
