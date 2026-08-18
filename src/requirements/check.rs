//! Read-only provider-prerequisite and requirement checks against the current
//! host environment.

use super::providers::builtin::BuiltinProvider;
use super::providers::*;
use super::*;

pub(super) fn check_context(context: &RequirementContext<'_>) -> Result<CheckOutcome> {
    let referenced = context
        .requirements
        .iter()
        .flat_map(|requirement| {
            requirement
                .binding
                .prerequisites
                .iter()
                .map(move |identity| (requirement.binding.provider.as_str(), identity.as_str()))
        })
        .collect::<BTreeSet<_>>();
    let mut items = context
        .prerequisites
        .iter()
        .filter(|prerequisite| {
            referenced.contains(&(
                prerequisite.provider.as_str(),
                prerequisite.identity.as_str(),
            ))
        })
        .map(|prerequisite| check_prerequisite(context, prerequisite))
        .collect::<Result<Vec<_>>>()?;
    // Independent checks are blocking subprocesses (brew, dpkg, git...), so
    // the dominant cost is wall-clock wait, not CPU. Provider backends that
    // cannot safely overlap are kept serial below.
    let brew = BrewSnapshot::fetch(context.requirements)?;
    let requirements = std::thread::scope(|scope| {
        enum Pending<'scope> {
            Concurrent(
                std::thread::ScopedJoinHandle<'scope, std::result::Result<CheckItem, String>>,
            ),
            Serial(std::result::Result<CheckItem, String>),
        }

        context
            .requirements
            .iter()
            .map(|requirement| {
                // DNF shares repository metadata and locks between processes;
                // concurrent repoquery calls can make an available package
                // appear absent and abort an otherwise valid bring-up.
                if BuiltinProvider::from_name(&requirement.binding.provider)
                    .is_some_and(|provider| provider.capabilities().serialized_checks)
                {
                    return Pending::Serial(
                        check_requirement(context, requirement, &brew)
                            .map_err(|error| error.to_string()),
                    );
                }
                // `WombatError` is not `Send` (it can wrap an `mlua::Error`,
                // which holds a non-Send `Arc<dyn Error>`), so cross the
                // thread boundary as a string and rebuild it after joining.
                Pending::Concurrent(scope.spawn(|| {
                    check_requirement(context, requirement, &brew)
                        .map_err(|error| error.to_string())
                }))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|pending| {
                match pending {
                    Pending::Concurrent(handle) => handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                    Pending::Serial(result) => result,
                }
                .map_err(WombatError::configuration)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    items.extend(requirements);
    Ok(CheckOutcome {
        build_id: context.id.to_string(),
        items,
    })
}

pub(super) fn check_prerequisite(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<CheckItem> {
    let started = std::time::Instant::now();
    let provider = provider_for(context.providers, &prerequisite.provider)?;
    let (status, detail) = match &provider.origin {
        ProviderOrigin::Builtin { .. } => match BuiltinProvider::from_name(&provider.name) {
            Some(BuiltinProvider::Apt) => check_apt_source(context, prerequisite)?,
            Some(BuiltinProvider::Dnf) => check_rpmfusion(context, prerequisite)?,
            Some(BuiltinProvider::Flatpak) => check_flathub(context, prerequisite)?,
            Some(provider) if !provider.capabilities().prerequisites => {
                return Err(WombatError::configuration(format!(
                    "built-in provider `{}` does not support prerequisites",
                    provider.name()
                )));
            }
            None | Some(_) => unreachable!("validated built-in prerequisite provider"),
        },
        ProviderOrigin::Custom { .. } => {
            check_custom_prerequisite(context, provider, prerequisite)?
        }
    };
    Ok(CheckItem {
        subject: CheckSubject::Prerequisite,
        identity: format!(
            "prerequisite:{}:{}",
            prerequisite.provider, prerequisite.identity
        ),
        provider: prerequisite.provider.clone(),
        status,
        detail,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

pub(super) fn check_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
    brew: &BrewSnapshot,
) -> Result<CheckItem> {
    let started = std::time::Instant::now();
    let mut item = check_requirement_uncounted(context, requirement, brew)?;
    item.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(item)
}

fn check_requirement_uncounted(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
    brew: &BrewSnapshot,
) -> Result<CheckItem> {
    let selected = selected_candidate(requirement)?;
    let label = requirement_label(requirement);
    if requirement.kind == RequirementKind::Command {
        let command = selected.name();
        if let Some(path) = which(command) {
            if let Some(minimum) = selected.minimum() {
                let observed = observe_command_version(&path)?;
                if version_at_least(&observed, minimum) {
                    return Ok(CheckItem {
                        subject: CheckSubject::Requirement,
                        identity: label,
                        provider: requirement.binding.provider.clone(),
                        status: CheckStatus::Satisfied,
                        detail: format!("{} at {}", path.display(), observed),
                        duration_ms: 0,
                    });
                }
                return Ok(CheckItem {
                    subject: CheckSubject::Requirement,
                    identity: label,
                    provider: requirement.binding.provider.clone(),
                    status: CheckStatus::Outdated,
                    detail: format!("observed {observed}; needs at least {minimum}"),
                    duration_ms: 0,
                });
            }
            return Ok(CheckItem {
                subject: CheckSubject::Requirement,
                identity: label,
                provider: requirement.binding.provider.clone(),
                status: CheckStatus::Satisfied,
                detail: path.display().to_string(),
                duration_ms: 0,
            });
        }
    }
    let provider = provider_for(context.providers, &requirement.binding.provider)?;
    let mut result = match &provider.origin {
        ProviderOrigin::Builtin { .. } => match BuiltinProvider::from_name(&provider.name) {
            Some(BuiltinProvider::Brew) => {
                check_brew(&requirement.binding, selected.minimum(), brew)?
            }
            Some(BuiltinProvider::Apt) => check_apt(&requirement.binding, selected.minimum())?,
            Some(BuiltinProvider::Dnf) => {
                check_dnf(context, &requirement.binding, selected.minimum())?
            }
            Some(BuiltinProvider::Flatpak) => check_flatpak(context, &requirement.binding)?,
            Some(BuiltinProvider::Git) => check_git(&requirement.binding)?,
            None => unreachable!("validated built-in requirement provider"),
        },
        ProviderOrigin::Custom { .. } => check_custom(context, provider, requirement)?,
    };
    result.subject = CheckSubject::Requirement;
    result.identity = label;
    if result.status == CheckStatus::Satisfied {
        for command in &requirement.binding.publications.commands {
            if which(command).is_none() {
                result.status = CheckStatus::Missing;
                result.detail = format!(
                    "package is present but published command `{command}` is absent from PATH"
                );
                break;
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[cfg(unix)]
    fn executable(path: &Path, source: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::write(path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn dnf_requirement(name: &str) -> Requirement {
        Requirement {
            kind: RequirementKind::Package,
            owner: "root".to_string(),
            declared_at: crate::model::manifest::SourceTrace {
                primary: crate::model::manifest::SourceLocation {
                    source: "wombat.lua".to_string(),
                    line: Some(1),
                    column: None,
                },
                callers: Vec::new(),
            },
            candidates: vec![RequirementCandidate::Package {
                name: name.to_string(),
                provider: Some("dnf".to_string()),
                minimum: None,
                publications: crate::model::manifest::Publications {
                    commands: Vec::new(),
                },
                with: FrozenValue::empty_map(),
            }],
            attempts: vec![crate::model::manifest::ResolutionAttempt {
                candidate: 0,
                provider: "dnf".to_string(),
                outcome: crate::model::manifest::ResolutionOutcome::Selected,
            }],
            selected: 0,
            choice: crate::model::manifest::RequirementChoice::Required,
            binding: ProviderBinding {
                provider: "dnf".to_string(),
                identity: format!("package:{name}"),
                elevated: true,
                package: Some(name.to_string()),
                publications: crate::model::manifest::Publications {
                    commands: Vec::new(),
                },
                prerequisites: Vec::new(),
                data: serde_json::from_value(serde_json::json!({ "name": name })).unwrap(),
            },
            when: crate::execution::ladder::CoreRung::MaterialiseBefore.into(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn dnf_repository_checks_never_overlap() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = temporary.path().join("bin");
        let active = temporary.path().join("dnf-active");
        fs::create_dir(&bin).unwrap();
        executable(&bin.join("rpm"), "#!/bin/sh\nexit 1\n");
        executable(
            &bin.join("dnf"),
            &format!(
                "#!/bin/sh\nif ! mkdir '{}'; then exit 70; fi\ntrap 'rmdir '\"'\"'{}'\"'\"'' EXIT\nsleep 0.2\nfor argument do name=$argument; done\nprintf '%s\\n' \"$name\"\n",
                active.display(),
                active.display(),
            ),
        );
        let requirements = [dnf_requirement("alpha"), dnf_requirement("beta")];
        let providers = [Provider {
            name: "dnf".to_string(),
            priority: 0,
            config: FrozenValue::empty_map(),
            origin: ProviderOrigin::Builtin {
                contract_version: 1,
            },
            declared_at: requirements[0].declared_at.clone(),
        }];
        let context = RequirementContext {
            id: "fixture",
            providers: &providers,
            requirements: &requirements,
            prerequisites: &[],
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin),
        };

        let outcome = check_context(&context).unwrap();
        assert!(
            outcome
                .items
                .iter()
                .all(|item| item.status == CheckStatus::Missing),
            "{:?}",
            outcome.items
        );
    }
}
