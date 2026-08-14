//! Authorized ladder execution and transactional target mutation.

use super::*;

pub(super) fn execute(
    prepared: PreparedApply,
    resolutions: &BTreeMap<String, ConflictResolution>,
) -> Result<ApplyOutcome> {
    let journal_path = prepared.state_guard.execution_journal_path();
    let plan_id = prepared.opened.manifest.plan_id.clone();
    let prepared_ladder = prepared.opened.manifest.ladder.clone();
    let result = execute_inner(prepared, resolutions);
    if let Err(error) = &result {
        let mut journal = crate::execution::ladder::read_at(&journal_path)
            .map(|journal| {
                journal.reopen_for_ladder(&plan_id, CoreRung::DeployAfter, &prepared_ladder)
            })
            .unwrap_or_else(|_| {
                ExecutionJournal::new_for_ladder(plan_id, CoreRung::DeployAfter, &prepared_ladder)
            });
        let failed_rung = if journal.rungs.iter().any(|record| {
            record.id == CoreRung::DeployApply && record.status == ExecutionStatus::Succeeded
        }) {
            CoreRung::DeployAfter
        } else {
            CoreRung::DeployApply
        };
        journal.fail(failed_rung, error);
        let _ = crate::execution::ladder::write_at(&journal_path, &journal);
    }
    result
}

fn execute_inner(
    prepared: PreparedApply,
    resolutions: &BTreeMap<String, ConflictResolution>,
) -> Result<ApplyOutcome> {
    let PreparedApply {
        opened,
        state_guard,
        previous,
        plan,
        warnings,
        mut requirement_authorization,
        state_root,
        target_root,
        clean,
        run_scripts,
        rerun_scripts,
        allow_host_scripts,
    } = prepared;
    let journal_path = state_guard.execution_journal_path();
    let mut journal = crate::execution::ladder::read_at(&journal_path)
        .map(|journal| {
            journal.reopen_for_ladder(
                &opened.manifest.plan_id,
                CoreRung::DeployAfter,
                &opened.manifest.ladder,
            )
        })
        .unwrap_or_else(|_| {
            ExecutionJournal::new_for_ladder(
                opened.manifest.plan_id.clone(),
                CoreRung::DeployAfter,
                &opened.manifest.ladder,
            )
        });
    journal.build_id = Some(opened.manifest.build_id.clone());
    journal.configure(
        opened.manifest.execution_mode,
        opened.manifest.skipped_requirement_gates.clone(),
    );
    let deploy_apply: crate::execution::ladder::RungId = CoreRung::DeployApply.into();
    let conflicts = plan
        .conflicts()
        .map(|item| item.target.clone())
        .collect::<BTreeSet<_>>();
    let provided = resolutions.keys().cloned().collect::<BTreeSet<_>>();
    if conflicts != provided {
        return Err(WombatError::conflict(
            "every conflict must have exactly one resolution before apply",
        ));
    }

    // Planning deliberately holds the target lock but performs no mutation. Even
    // operational cleanup and provider gates begin only after every conflict has
    // an explicit resolution.
    if clean {
        state_guard.reset_execution_journal()?;
    }

    let script_state_root = state_guard.scripts_directory();
    for rung in crate::execution::runner::ExecutionRange::between(
        &opened.manifest.ladder,
        CoreRung::DeployBefore,
        CoreRung::DeployApply,
        true,
        false,
    )? {
        if let Some(authorization) = &mut requirement_authorization {
            crate::requirements::prepare_product_deploy_at_authorized(
                &opened.requested_build_dir,
                &rung,
                authorization,
            )?;
        }
        // Recorded as Running before the work starts. If the process dies here,
        // reopening the journal sees Running and reports Interrupted rather than
        // silently treating unfinished work as never attempted.
        journal.set_id(&rung, ExecutionStatus::Running);
        crate::execution::ladder::write_at(&journal_path, &journal)?;
        crate::execution::script::check_runners(
            &opened
                .manifest
                .scripts
                .iter()
                .filter(|script| script.at == rung)
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        for outcome in crate::execution::script::execute_at(
            &opened.manifest.scripts,
            &rung,
            &crate::execution::script::ScriptExecutionOptions {
                state_root: &script_state_root,
                payload_root: &opened.product_dir,
                payload_kind: crate::execution::script::PayloadKind::Product,
                project_identity: &opened.manifest.project_identity,
                plan_id: &opened.manifest.plan_id,
                build_id: Some(&opened.manifest.build_id),
                execution_mode: opened.manifest.execution_mode,
                allow_host_scripts,
                rerun: rerun_scripts,
                run_scripts,
                target_root: Some(&target_root),
            },
        )? {
            journal.record_action(
                outcome.identity,
                &outcome.rung,
                match outcome.status {
                    crate::model::manifest::ScriptOutcomeStatus::Ran => ExecutionStatus::Succeeded,
                    _ => ExecutionStatus::Skipped,
                },
                outcome.reason,
            );
        }
        journal.set_id(&rung, ExecutionStatus::Succeeded);
        crate::execution::ladder::write_at(&journal_path, &journal)?;
    }
    journal.set(CoreRung::DeployApply, ExecutionStatus::Running);
    crate::execution::ladder::write_at(&journal_path, &journal)?;

    if let Some(authorization) = &mut requirement_authorization {
        crate::requirements::prepare_product_deploy_at_authorized(
            &opened.requested_build_dir,
            &deploy_apply,
            authorization,
        )?;
    }

    crate::execution::script::check_runners(
        &opened
            .manifest
            .scripts
            .iter()
            .filter(|script| script.at == deploy_apply)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    for outcome in crate::execution::script::execute_at(
        &opened.manifest.scripts,
        &deploy_apply,
        &crate::execution::script::ScriptExecutionOptions {
            state_root: &script_state_root,
            payload_root: &opened.product_dir,
            payload_kind: crate::execution::script::PayloadKind::Product,
            project_identity: &opened.manifest.project_identity,
            plan_id: &opened.manifest.plan_id,
            build_id: Some(&opened.manifest.build_id),
            execution_mode: opened.manifest.execution_mode,
            allow_host_scripts,
            rerun: rerun_scripts,
            run_scripts,
            target_root: Some(&target_root),
        },
    )? {
        journal.record_action(
            outcome.identity,
            &outcome.rung,
            match outcome.status {
                crate::model::manifest::ScriptOutcomeStatus::Ran => ExecutionStatus::Succeeded,
                _ => ExecutionStatus::Skipped,
            },
            outcome.reason,
        );
    }

    for item in &plan.items {
        let current = inspect_actual(&plan.target_root, &item.path)?;
        if current != item.actual {
            return Err(WombatError::configuration(format!(
                "target `{}` changed after deployment planning; no files were modified",
                item.target
            )));
        }
    }

    let mut state_by_target = previous
        .artifacts
        .into_iter()
        .map(|artifact| {
            let artifact = artifact.to_artifact();
            (target_key(&artifact), artifact)
        })
        .collect::<BTreeMap<_, _>>();
    let mut created = 0;
    let mut updated = 0;
    let mut removed = 0;
    let mut state_advanced = 0;
    let mut skipped = Vec::new();

    for item in &plan.items {
        let resolution = resolutions.get(&item.target).copied();
        if resolution == Some(ConflictResolution::Skip) {
            skipped.push(item.target.clone());
            continue;
        }
        let current = inspect_actual(&plan.target_root, &item.path)?;
        if current != item.actual {
            return Err(WombatError::configuration(format!(
                "target `{}` changed during deployment",
                item.target
            )));
        }

        let overwrite = resolution == Some(ConflictResolution::Overwrite);
        match item.action {
            ReconciliationAction::Unchanged => {}
            ReconciliationAction::Create => {
                write_desired(
                    &opened,
                    &plan.target_root,
                    desired_artifact(item)?,
                    &item.path,
                    false,
                )?;
                created += 1;
            }
            ReconciliationAction::Adopt
            | ReconciliationAction::AdvanceState
            | ReconciliationAction::Forget => {
                state_advanced += 1;
            }
            ReconciliationAction::Update => {
                write_desired(
                    &opened,
                    &plan.target_root,
                    desired_artifact(item)?,
                    &item.path,
                    true,
                )?;
                updated += 1;
            }
            ReconciliationAction::Remove => {
                remove_target(&item.path)?;
                removed += 1;
            }
            ReconciliationAction::Conflict if overwrite => {
                if let Some(desired) = &item.desired {
                    write_desired(&opened, &plan.target_root, desired, &item.path, true)?;
                    if matches!(item.actual, ActualArtifact::Absent) {
                        created += 1;
                    } else {
                        updated += 1;
                    }
                } else {
                    remove_target(&item.path)?;
                    removed += 1;
                }
            }
            ReconciliationAction::Conflict => {
                return Err(WombatError::invariant(format!(
                    "deployment conflict for `{}` reached execution without a resolution",
                    item.target
                )));
            }
        }

        let key_artifact = item
            .desired
            .as_ref()
            .or(item.previous.as_ref())
            .ok_or_else(|| {
                WombatError::invariant(format!(
                    "reconciliation item `{}` has neither desired nor prior state",
                    item.target
                ))
            })?;
        let key = target_key(key_artifact);
        if let Some(desired) = &item.desired {
            state_by_target.insert(key, desired.clone());
        } else {
            state_by_target.remove(&key);
        }
    }

    let artifacts = state_by_target
        .into_values()
        .map(AppliedArtifact::from_artifact)
        .collect::<Vec<_>>();
    let complete_build_id = skipped.is_empty().then(|| plan.build_id.clone());
    state_guard.write(&TargetState {
        format_version: crate::deploy::state::TARGET_STATE_FORMAT_VERSION,
        target_root: plan
            .target_root
            .to_str()
            .ok_or_else(|| WombatError::invariant("validated deployment root stopped being UTF-8"))?
            .to_string(),
        complete_build_id,
        artifacts,
    })?;
    journal.set(CoreRung::DeployApply, ExecutionStatus::Succeeded);
    crate::execution::ladder::write_at(&journal_path, &journal)?;
    drop(state_guard);
    for rung in crate::execution::runner::ExecutionRange::between(
        &opened.manifest.ladder,
        CoreRung::DeployApply,
        CoreRung::DeployAfter,
        false,
        true,
    )? {
        if let Some(authorization) = &mut requirement_authorization {
            crate::requirements::prepare_product_deploy_at_authorized(
                &opened.requested_build_dir,
                &rung,
                authorization,
            )?;
        }
        journal.set_id(&rung, ExecutionStatus::Running);
        crate::execution::ladder::write_at(&journal_path, &journal)?;
        crate::execution::script::check_runners(
            &opened
                .manifest
                .scripts
                .iter()
                .filter(|script| script.at == rung)
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        for outcome in crate::execution::script::execute_at(
            &opened.manifest.scripts,
            &rung,
            &crate::execution::script::ScriptExecutionOptions {
                state_root: &script_state_root,
                payload_root: &opened.product_dir,
                payload_kind: crate::execution::script::PayloadKind::Product,
                project_identity: &opened.manifest.project_identity,
                plan_id: &opened.manifest.plan_id,
                build_id: Some(&opened.manifest.build_id),
                execution_mode: opened.manifest.execution_mode,
                allow_host_scripts,
                rerun: rerun_scripts,
                run_scripts,
                target_root: Some(&target_root),
            },
        )? {
            journal.record_action(
                outcome.identity,
                &outcome.rung,
                match outcome.status {
                    crate::model::manifest::ScriptOutcomeStatus::Ran => ExecutionStatus::Succeeded,
                    _ => ExecutionStatus::Skipped,
                },
                outcome.reason,
            );
        }
        journal.set_id(&rung, ExecutionStatus::Succeeded);
        crate::execution::ladder::write_at(&journal_path, &journal)?;
    }
    let state_guard = TargetStateGuard::open(&state_root, &target_root, LockMode::Exclusive)?;
    crate::execution::ladder::write_at(&state_guard.execution_journal_path(), &journal)?;

    let changed = created + updated + removed + state_advanced;
    let status = if !skipped.is_empty() {
        ApplyStatus::AppliedWithSkips
    } else if changed == 0 {
        ApplyStatus::Unchanged
    } else {
        ApplyStatus::Applied
    };
    Ok(ApplyOutcome {
        status,
        build_id: plan.build_id,
        created,
        updated,
        removed,
        state_advanced,
        skipped,
        warnings,
    })
}

fn desired_artifact(item: &crate::deploy::reconcile::ReconciliationItem) -> Result<&Artifact> {
    item.desired.as_ref().ok_or_else(|| {
        WombatError::invariant(format!(
            "{:?} reconciliation for `{}` has no desired artifact",
            item.action, item.target
        ))
    })
}

fn write_desired(
    opened: &OpenedBuild,
    target_root: &Path,
    artifact: &Artifact,
    target: &Path,
    replace: bool,
) -> Result<()> {
    let parent = target.parent().expect("artifact targets have a parent");
    ensure_safe_parents(parent)?;
    let source = product_path(opened, artifact);
    let mut input = File::open(&source).map_err(|error| WombatError::io(&source, error))?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
    let metadata_before = input
        .metadata()
        .map_err(|error| WombatError::io(&source, error))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| WombatError::io(&source, error))?;
        if count == 0 {
            break;
        }
        temporary
            .write_all(&buffer[..count])
            .map_err(|error| WombatError::io(temporary.path(), error))?;
        hasher.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).expect("buffer length fits u64"))
            .ok_or_else(|| WombatError::configuration("build artifact exceeds u64"))?;
    }
    let metadata_after = input
        .metadata()
        .map_err(|error| WombatError::io(&source, error))?;
    if metadata_before.len() != metadata_after.len()
        || size != artifact.content.size
        || digest_string(hasher.finalize()) != artifact.content.digest
    {
        return Err(WombatError::configuration(format!(
            "verified build artifact `{}` changed while it was being applied",
            source.display()
        )));
    }
    set_mode(temporary.as_file(), temporary.path(), artifact)?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| WombatError::io(temporary.path(), error))?;
    if replace {
        temporary
            .persist(target)
            .map_err(|error| WombatError::io(target, error.error))?;
    } else {
        temporary.persist_noclobber(target).map_err(|error| {
            WombatError::configuration(format!(
                "target `{}` appeared during deployment: {}",
                target.display(),
                error.error
            ))
        })?;
    }
    sync_directory(parent)?;
    let actual = inspect_actual(target_root, target)?;
    if !crate::deploy::reconcile::actual_matches(&actual, artifact) {
        return Err(WombatError::configuration(format!(
            "deployed target `{}` did not verify",
            target.display()
        )));
    }
    Ok(())
}

fn remove_target(target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(target).map_err(|error| WombatError::io(target, error))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "refusing to remove non-regular target `{}`",
            target.display()
        )));
    }
    fs::remove_file(target).map_err(|error| WombatError::io(target, error))?;
    sync_directory(target.parent().expect("target files have parents"))
}

fn ensure_safe_parents(parent: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = parent;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(WombatError::configuration(format!(
                        "target parent `{}` must be a non-symlink directory",
                        current.display()
                    )));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current.parent().ok_or_else(|| {
                    WombatError::configuration("cannot create target parent above filesystem root")
                })?;
            }
            Err(error) => return Err(WombatError::io(current, error)),
        }
    }
    for directory in missing.iter().rev() {
        fs::create_dir(directory).map_err(|error| WombatError::io(directory, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
                .map_err(|error| WombatError::io(directory, error))?;
        }
        sync_directory(
            directory
                .parent()
                .expect("created directories have parents"),
        )?;
    }
    Ok(())
}

pub(super) fn product_path(opened: &OpenedBuild, artifact: &Artifact) -> PathBuf {
    opened.product_dir.join("tree").join(&artifact.target.path)
}

fn set_mode(file: &File, path: &Path, artifact: &Artifact) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = crate::deploy::reconcile::expected_mode(artifact);
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    crate::storage::atomic::sync_directory(path)
}
