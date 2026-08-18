//! Flatpak reference checks and conservative Flathub remote reconciliation.

use super::*;

const FLATHUB_DESCRIPTOR: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";
const FLATHUB_URL: &str = "https://dl.flathub.org/repo/";

#[derive(Clone, Debug, Eq, PartialEq)]
struct FlatpakRef {
    id: String,
    remote: String,
    kind: String,
    scope: String,
    arch: String,
    branch: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FlathubRemote {
    scope: String,
}

fn scope_flag(scope: &str) -> Result<&'static str> {
    match scope {
        "system" => Ok("--system"),
        "user" => Ok("--user"),
        _ => Err(WombatError::configuration(
            "Flatpak scope must be `system` or `user`",
        )),
    }
}

fn safe_flatpak_token(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn safe_flatpak_id(value: &str) -> bool {
    safe_flatpak_token(value) && value.contains('.')
}

fn flatpak_ref(binding: &ProviderBinding) -> Result<FlatpakRef> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration(
            "Flatpak binding data must be a map",
        ));
    };
    if data.keys().any(|field| {
        !matches!(
            field.as_str(),
            "id" | "remote" | "kind" | "scope" | "arch" | "branch"
        )
    }) {
        return Err(WombatError::configuration(
            "Flatpak binding contains an unsupported field",
        ));
    }
    let string = |field: &str| match data.get(field) {
        Some(FrozenValue::String(value)) => Ok(value.clone()),
        _ => Err(WombatError::configuration(format!(
            "Flatpak binding lacks `{field}`"
        ))),
    };
    let value = FlatpakRef {
        id: string("id")?,
        remote: string("remote")?,
        kind: string("kind")?,
        scope: string("scope")?,
        arch: string("arch")?,
        branch: match data.get("branch") {
            None => None,
            Some(FrozenValue::String(value)) => Some(value.clone()),
            Some(_) => {
                return Err(WombatError::configuration(
                    "Flatpak branch must be a string",
                ));
            }
        },
    };
    scope_flag(&value.scope)?;
    if value.remote != "flathub"
        || !matches!(value.kind.as_str(), "app" | "runtime")
        || !safe_flatpak_id(&value.id)
        || !safe_flatpak_token(&value.arch)
        || value
            .branch
            .as_deref()
            .is_some_and(|branch| !safe_flatpak_token(branch))
    {
        return Err(WombatError::configuration(
            "Flatpak binding contract is inconsistent",
        ));
    }
    let branch_identity = value.branch.as_deref().unwrap_or("current");
    if binding.identity
        != format!(
            "ref:{}:{}:{}:{}:{}",
            value.scope, value.kind, value.id, value.arch, branch_identity
        )
        || binding.elevated != (value.scope == "system")
        || binding.prerequisites != [format!("remote:{}:flathub", value.scope)]
    {
        return Err(WombatError::configuration(
            "Flatpak binding identity is inconsistent",
        ));
    }
    Ok(value)
}

fn flathub(prerequisite: &ProviderPrerequisite) -> Result<FlathubRemote> {
    let FrozenValue::Map(data) = &prerequisite.data else {
        return Err(WombatError::configuration(
            "Flathub prerequisite data must be a map",
        ));
    };
    if data
        .keys()
        .any(|field| !matches!(field.as_str(), "name" | "scope" | "descriptor" | "url"))
    {
        return Err(WombatError::configuration(
            "Flathub prerequisite contains an unsupported field",
        ));
    }
    let string = |field: &str| match data.get(field) {
        Some(FrozenValue::String(value)) => Ok(value.as_str()),
        _ => Err(WombatError::configuration(format!(
            "Flathub prerequisite lacks `{field}`"
        ))),
    };
    let scope = string("scope")?.to_string();
    scope_flag(&scope)?;
    if string("name")? != "flathub"
        || string("descriptor")? != FLATHUB_DESCRIPTOR
        || string("url")? != FLATHUB_URL
        || prerequisite.identity != format!("remote:{scope}:flathub")
        || prerequisite.elevated != (scope == "system")
    {
        return Err(WombatError::configuration(
            "Flathub prerequisite contract is inconsistent",
        ));
    }
    Ok(FlathubRemote { scope })
}

fn flatpak_command(context: &RequirementContext<'_>, purpose: &str) -> Result<PathBuf> {
    context.command("flatpak").ok_or_else(|| {
        WombatError::configuration(format!(
            "{purpose} requires `flatpak` to be available on PATH"
        ))
    })
}

fn flatpak_is_published_before(
    context: &RequirementContext<'_>,
    deadline: &crate::execution::ladder::RungId,
) -> bool {
    let Some(deadline) = context.ladder.position(deadline) else {
        return false;
    };
    context.requirements.iter().any(|requirement| {
        requirement.choice == crate::model::manifest::RequirementChoice::Required
            && requirement.binding.provider != BuiltinProvider::Flatpak.name()
            && requirement
                .binding
                .publications
                .commands
                .iter()
                .any(|command| command == "flatpak")
            && context
                .ladder
                .position(&requirement.when)
                .is_some_and(|position| position < deadline)
    })
}

fn flatpak_binding_deadline<'a>(
    context: &'a RequirementContext<'_>,
    binding: &ProviderBinding,
) -> Option<&'a crate::execution::ladder::RungId> {
    context
        .requirements
        .iter()
        .find(|requirement| {
            requirement.binding.provider == binding.provider
                && requirement.binding.identity == binding.identity
        })
        .map(|requirement| &requirement.when)
}

pub(crate) fn check_flathub(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<(CheckStatus, String)> {
    let remote = flathub(prerequisite)?;
    let Some(flatpak) = context.command("flatpak") else {
        if flatpak_is_published_before(context, &prerequisite.when) {
            return Ok((
                CheckStatus::Missing,
                "Flatpak will be provided by a required earlier-rung requirement".to_string(),
            ));
        }
        return Ok((
            CheckStatus::Unavailable,
            "Flatpak is not available on PATH; declare its DNF package at an earlier rung"
                .to_string(),
        ));
    };
    let output = run_bounded(
        &flatpak,
        &[
            scope_flag(&remote.scope)?,
            "remotes",
            "--show-disabled",
            "--columns=name,url,options,filter",
        ],
        &BTreeMap::new(),
    )?;
    if !output.success {
        return Ok((
            CheckStatus::Unavailable,
            format!("flatpak remotes failed: {}", output_detail(&output)),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout.bytes);
    let Some((url, options, filter)) = text.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let name = fields.next()?;
        (name == "flathub").then(|| {
            (
                fields.next().unwrap_or("").to_string(),
                fields.next().unwrap_or("").to_string(),
                fields.next().unwrap_or("").to_string(),
            )
        })
    }) else {
        return Ok((
            CheckStatus::Missing,
            format!("{} Flathub remote is absent", remote.scope),
        ));
    };
    if url != FLATHUB_URL {
        return Ok((
            CheckStatus::Unavailable,
            format!(
                "existing {} Flathub remote points to {url}, expected {FLATHUB_URL}; Wombat will not repoint it",
                remote.scope
            ),
        ));
    }
    let has_option = |wanted: &str| {
        options
            .split(',')
            .map(str::trim)
            .any(|option| option == wanted)
    };
    if has_option("no-gpg-verify") {
        return Ok((
            CheckStatus::Unavailable,
            format!(
                "existing {} Flathub remote disables GPG verification",
                remote.scope
            ),
        ));
    }
    let disabled = has_option("disabled");
    // Flatpak renders an absent filter as `-`; treating that display sentinel
    // as a path makes every successfully repaired remote permanently stale.
    let filter = filter.trim();
    let filtered = !filter.is_empty() && filter != "-";
    if disabled || filtered {
        return Ok((
            CheckStatus::Outdated,
            format!(
                "{} Flathub remote needs{}{} repair",
                remote.scope,
                if disabled { " enabling" } else { "" },
                if filtered { " filter removal" } else { "" }
            ),
        ));
    }
    Ok((
        CheckStatus::Satisfied,
        format!("{} Flathub remote is configured", remote.scope),
    ))
}

pub(super) fn preflight_flathub(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<()> {
    let remote = flathub(prerequisite)?;
    if context.command("flatpak").is_none()
        && flatpak_is_published_before(context, &prerequisite.when)
    {
        return preflight_elevation(
            remote.scope == "system" && context.system_root == Path::new("/"),
        );
    }
    flatpak_command(context, "Flathub reconciliation")?;
    preflight_elevation(remote.scope == "system" && context.system_root == Path::new("/"))
}

pub(super) fn reconcile_flathub(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
    noninteractive: bool,
) -> Result<()> {
    let remote = flathub(prerequisite)?;
    let flatpak = flatpak_command(context, "Flathub reconciliation")?;
    let (status, detail) = check_flathub(context, prerequisite)?;
    match status {
        CheckStatus::Satisfied => return Ok(()),
        CheckStatus::Unavailable => return Err(WombatError::configuration(detail)),
        CheckStatus::Missing => run_mutating(
            &flatpak,
            &[
                scope_flag(&remote.scope)?,
                "remote-add",
                "--if-not-exists",
                "flathub",
                FLATHUB_DESCRIPTOR,
            ],
            &BTreeMap::new(),
            prerequisite.elevated && context.system_root == Path::new("/"),
            noninteractive,
        )?,
        CheckStatus::Outdated => run_mutating(
            &flatpak,
            &[
                scope_flag(&remote.scope)?,
                "remote-modify",
                "--enable",
                "--no-filter",
                "--gpg-verify",
                "flathub",
            ],
            &BTreeMap::new(),
            prerequisite.elevated && context.system_root == Path::new("/"),
            noninteractive,
        )?,
    }
    Ok(())
}

pub(crate) fn check_flatpak(
    context: &RequirementContext<'_>,
    binding: &ProviderBinding,
) -> Result<CheckItem> {
    let reference = flatpak_ref(binding)?;
    let Some(flatpak) = context.command("flatpak") else {
        if flatpak_binding_deadline(context, binding)
            .is_some_and(|deadline| flatpak_is_published_before(context, deadline))
        {
            return Ok(provider_item(
                binding,
                CheckStatus::Missing,
                "Flatpak will be provided by a required earlier-rung requirement",
            ));
        }
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "Flatpak is not available on PATH; declare its DNF package at an earlier rung",
        ));
    };
    let output = run_bounded(
        &flatpak,
        &[
            scope_flag(&reference.scope)?,
            "list",
            if reference.kind == "app" {
                "--app"
            } else {
                "--runtime"
            },
            "--columns=application,arch,branch,origin",
        ],
        &BTreeMap::new(),
    )?;
    if !output.success {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            &format!("flatpak list failed: {}", output_detail(&output)),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout.bytes);
    let rows = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some((
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            ))
        })
        .filter(|(id, _, _, _)| *id == reference.id)
        .collect::<Vec<_>>();
    if rows.iter().any(|(_, arch, branch, origin)| {
        *arch == reference.arch
            && *origin == reference.remote
            && reference
                .branch
                .as_deref()
                .is_none_or(|wanted| wanted == *branch)
    }) {
        return Ok(provider_item(
            binding,
            CheckStatus::Satisfied,
            &format!(
                "{} {} is installed from {}",
                reference.kind, reference.id, reference.remote
            ),
        ));
    }
    if rows.is_empty() {
        Ok(provider_item(
            binding,
            CheckStatus::Missing,
            &format!(
                "{} is not installed in {} scope",
                reference.id, reference.scope
            ),
        ))
    } else {
        Ok(provider_item(
            binding,
            CheckStatus::Outdated,
            &format!(
                "{} is installed with a different architecture, branch, or origin",
                reference.id
            ),
        ))
    }
}

fn flatpak_ref_argument(reference: &FlatpakRef) -> String {
    match &reference.branch {
        Some(branch) => format!("{}/{}/{}", reference.id, reference.arch, branch),
        None => reference.id.clone(),
    }
}

pub(super) fn preflight_flatpak_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
) -> Result<()> {
    let reference = flatpak_ref(&requirement.binding)?;
    let flatpak = flatpak_command(context, "Flatpak preflight")?;
    let argument = flatpak_ref_argument(&reference);
    let output = run_bounded(
        &flatpak,
        &[
            scope_flag(&reference.scope)?,
            "remote-info",
            "--arch",
            &reference.arch,
            &reference.remote,
            &argument,
        ],
        &BTreeMap::new(),
    )?;
    if !output.success {
        return Err(WombatError::configuration(format!(
            "Flatpak preflight failed for `{}`: {}",
            reference.id,
            output_detail(&output)
        )));
    }
    preflight_elevation(requirement.binding.elevated && context.system_root == Path::new("/"))
}

pub(super) fn reconcile_flatpak_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
    noninteractive: bool,
) -> Result<()> {
    let reference = flatpak_ref(&requirement.binding)?;
    let flatpak = flatpak_command(context, "Flatpak reconciliation")?;
    let argument = flatpak_ref_argument(&reference);
    run_mutating(
        &flatpak,
        &[
            scope_flag(&reference.scope)?,
            "install",
            "--noninteractive",
            "--assumeyes",
            if reference.kind == "app" {
                "--app"
            } else {
                "--runtime"
            },
            "--arch",
            &reference.arch,
            &reference.remote,
            &argument,
        ],
        &BTreeMap::new(),
        requirement.binding.elevated && context.system_root == Path::new("/"),
        noninteractive,
    )
}

pub(super) fn validate_flatpak_contract(
    requirements: &[Requirement],
    prerequisites: &[ProviderPrerequisite],
) -> Result<()> {
    let remotes = prerequisites
        .iter()
        .filter(|value| value.provider == BuiltinProvider::Flatpak.name())
        .map(|value| Ok((value.identity.as_str(), flathub(value)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    for requirement in requirements
        .iter()
        .filter(|value| value.binding.provider == BuiltinProvider::Flatpak.name())
    {
        let reference = flatpak_ref(&requirement.binding)?;
        if requirement.kind != RequirementKind::Package
            || selected_candidate(requirement)?.minimum().is_some()
            || requirement.binding.package.is_some()
            || !remotes.contains_key(format!("remote:{}:flathub", reference.scope).as_str())
        {
            return Err(WombatError::configuration(
                "Flatpak bindings must be explicit package requirements with one valid Flathub prerequisite and no minimum version",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn executable(path: &Path, source: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let staged = path.with_extension("wombat-test-staged");
        fs::write(&staged, source).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(staged, path).unwrap();
    }

    fn binding() -> ProviderBinding {
        ProviderBinding {
            provider: "flatpak".to_string(),
            identity: "ref:system:app:org.gnome.Calculator:x86_64:current".to_string(),
            elevated: true,
            package: None,
            publications: crate::model::manifest::Publications {
                commands: Vec::new(),
            },
            prerequisites: vec!["remote:system:flathub".to_string()],
            data: serde_json::from_value(serde_json::json!({
                "id": "org.gnome.Calculator",
                "remote": "flathub",
                "kind": "app",
                "scope": "system",
                "arch": "x86_64",
            }))
            .unwrap(),
        }
    }

    #[test]
    fn verifier_rejects_unsafe_frozen_ref_tokens() {
        let mut unsafe_id = binding();
        let FrozenValue::Map(data) = &mut unsafe_id.data else {
            unreachable!()
        };
        data.insert(
            "id".to_string(),
            FrozenValue::String("-unsafe.Application".to_string()),
        );
        assert!(
            flatpak_ref(&unsafe_id)
                .unwrap_err()
                .to_string()
                .contains("contract is inconsistent")
        );

        let mut unsafe_branch = binding();
        let FrozenValue::Map(data) = &mut unsafe_branch.data else {
            unreachable!()
        };
        data.insert(
            "branch".to_string(),
            FrozenValue::String("--unsafe".to_string()),
        );
        assert!(
            flatpak_ref(&unsafe_branch)
                .unwrap_err()
                .to_string()
                .contains("contract is inconsistent")
        );
    }

    fn requirement(binding: ProviderBinding) -> Requirement {
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
                name: "org.gnome.Calculator".to_string(),
                provider: Some("flatpak".to_string()),
                minimum: None,
                publications: crate::model::manifest::Publications {
                    commands: Vec::new(),
                },
                with: FrozenValue::empty_map(),
            }],
            attempts: vec![crate::model::manifest::ResolutionAttempt {
                candidate: 0,
                provider: "flatpak".to_string(),
                outcome: crate::model::manifest::ResolutionOutcome::Selected,
            }],
            selected: 0,
            choice: crate::model::manifest::RequirementChoice::Required,
            binding,
            when: crate::execution::ladder::CoreRung::DeployBefore.into(),
        }
    }

    fn prerequisite() -> ProviderPrerequisite {
        ProviderPrerequisite {
            provider: "flatpak".to_string(),
            identity: "remote:system:flathub".to_string(),
            description: "Configure the system Flathub remote".to_string(),
            when: crate::execution::ladder::CoreRung::DeployBefore.into(),
            elevated: true,
            data: serde_json::from_value(serde_json::json!({
                "name": "flathub",
                "scope": "system",
                "descriptor": FLATHUB_DESCRIPTOR,
                "url": FLATHUB_URL,
            }))
            .unwrap(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn flathub_and_reference_reconcile_and_reuse_exact_state() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = temporary.path().join("bin");
        let remote = temporary.path().join("remote");
        let application = temporary.path().join("application");
        fs::create_dir(&bin).unwrap();
        executable(
            &bin.join("flatpak"),
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *' remotes '*) [ -f '{}' ] && cat '{}' ;;\n  *remote-add*) printf 'flathub\\t{}\\tsystem\\t-\\n' > '{}' ;;\n  *remote-modify*) printf 'flathub\\t{}\\tsystem\\t-\\n' > '{}' ;;\n  *' list '*) [ -f '{}' ] && printf 'org.gnome.Calculator\\tx86_64\\tstable\\tflathub\\n' ;;\n  *remote-info*) [ -f '{}' ] ;;\n  *install*) : > '{}' ;;\nesac\nexit 0\n",
                remote.display(),
                remote.display(),
                FLATHUB_URL,
                remote.display(),
                FLATHUB_URL,
                remote.display(),
                application.display(),
                remote.display(),
                application.display(),
            ),
        );
        let prerequisite = prerequisite();
        let requirement = requirement(binding());
        let context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: std::slice::from_ref(&requirement),
            prerequisites: std::slice::from_ref(&prerequisite),
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin),
        };

        assert_eq!(
            check_flathub(&context, &prerequisite).unwrap().0,
            CheckStatus::Missing
        );
        reconcile_flathub(&context, &prerequisite, true).unwrap();
        assert_eq!(
            check_flathub(&context, &prerequisite).unwrap().0,
            CheckStatus::Satisfied
        );
        assert_eq!(
            check_flatpak(&context, &requirement.binding)
                .unwrap()
                .status,
            CheckStatus::Missing
        );
        preflight_flatpak_requirement(&context, &requirement).unwrap();
        reconcile_flatpak_requirement(&context, &requirement, true).unwrap();
        assert_eq!(
            check_flatpak(&context, &requirement.binding)
                .unwrap()
                .status,
            CheckStatus::Satisfied
        );
    }

    #[test]
    #[cfg(unix)]
    fn flathub_repairs_filters_but_refuses_wrong_urls_and_unsafe_gpg() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = temporary.path().join("bin");
        let remote = temporary.path().join("remote");
        fs::create_dir(&bin).unwrap();
        executable(
            &bin.join("flatpak"),
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *' remotes '*) cat '{}' ;;\n  *remote-modify*) printf 'flathub\\t{}\\tsystem\\t-\\n' > '{}' ;;\nesac\n",
                remote.display(),
                FLATHUB_URL,
                remote.display(),
            ),
        );
        let prerequisite = prerequisite();
        let context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &[],
            prerequisites: std::slice::from_ref(&prerequisite),
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin),
        };

        fs::write(
            &remote,
            format!("flathub\t{FLATHUB_URL}\tsystem,disabled\t/usr/share/flatpak/fedora-flathub.filter\n"),
        )
        .unwrap();
        assert_eq!(
            check_flathub(&context, &prerequisite).unwrap().0,
            CheckStatus::Outdated
        );
        reconcile_flathub(&context, &prerequisite, true).unwrap();
        assert_eq!(
            check_flathub(&context, &prerequisite).unwrap().0,
            CheckStatus::Satisfied
        );

        fs::write(
            &remote,
            "flathub\thttps://example.invalid/repo/\tsystem\t\n",
        )
        .unwrap();
        assert_eq!(
            check_flathub(&context, &prerequisite).unwrap().0,
            CheckStatus::Unavailable
        );
        assert!(
            reconcile_flathub(&context, &prerequisite, true)
                .unwrap_err()
                .to_string()
                .contains("will not repoint")
        );

        fs::write(
            &remote,
            format!("flathub\t{FLATHUB_URL}\tsystem,no-gpg-verify\t\n"),
        )
        .unwrap();
        assert_eq!(
            check_flathub(&context, &prerequisite).unwrap().0,
            CheckStatus::Unavailable
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_missing_flatpak_executable_is_actionable_until_an_earlier_rung_provides_it() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = temporary.path().join("bin");
        let log = temporary.path().join("arguments");
        fs::create_dir(&bin).unwrap();
        let prerequisite = prerequisite();
        let requirement = requirement(binding());
        let mut publisher = requirement.clone();
        publisher.binding.provider = "dnf".to_string();
        publisher.binding.identity = "package:flatpak".to_string();
        publisher.binding.package = Some("flatpak".to_string());
        publisher.binding.prerequisites.clear();
        publisher.binding.publications.commands = vec!["flatpak".to_string()];
        publisher.when = crate::execution::ladder::CoreRung::MaterialiseBefore.into();
        let requirements = [publisher.clone(), requirement.clone()];
        let context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &requirements,
            prerequisites: std::slice::from_ref(&prerequisite),
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin.clone()),
        };

        let (status, detail) = check_flathub(&context, &prerequisite).unwrap();
        assert_eq!(status, CheckStatus::Missing);
        assert!(detail.contains("earlier-rung"), "{detail}");
        assert_eq!(
            check_flatpak(&context, &requirement.binding)
                .unwrap()
                .status,
            CheckStatus::Missing
        );
        preflight_flathub(&context, &prerequisite).unwrap();

        publisher.when = crate::execution::ladder::CoreRung::DeployBefore.into();
        let same_rung = [publisher, requirement.clone()];
        let same_rung_context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &same_rung,
            prerequisites: std::slice::from_ref(&prerequisite),
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin.clone()),
        };
        assert_eq!(
            check_flathub(&same_rung_context, &prerequisite).unwrap().0,
            CheckStatus::Unavailable
        );

        publisher = same_rung[0].clone();
        publisher.when = crate::execution::ladder::CoreRung::MaterialiseBefore.into();
        publisher.choice = crate::model::manifest::RequirementChoice::Preferred;
        let preferred = [publisher, requirement.clone()];
        let preferred_context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &preferred,
            prerequisites: std::slice::from_ref(&prerequisite),
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin.clone()),
        };
        assert_eq!(
            check_flathub(&preferred_context, &prerequisite).unwrap().0,
            CheckStatus::Unavailable
        );

        let mut circular = preferred[0].clone();
        circular.choice = crate::model::manifest::RequirementChoice::Required;
        circular.binding.provider = "flatpak".to_string();
        let circular = [circular, requirement.clone()];
        let circular_context = RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &circular,
            prerequisites: std::slice::from_ref(&prerequisite),
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: temporary.path().join("payloads"),
            system_root: temporary.path().join("root"),
            command_root: Some(bin.clone()),
        };
        assert_eq!(
            check_flathub(&circular_context, &prerequisite).unwrap().0,
            CheckStatus::Unavailable
        );

        executable(
            &bin.join("flatpak"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$*\" in *remote-info*) exit 0;; esac\nexit 0\n",
                log.display()
            ),
        );
        assert_eq!(
            check_flathub(&context, &prerequisite).unwrap().0,
            CheckStatus::Missing
        );
        assert_eq!(
            check_flatpak(&context, &requirement.binding)
                .unwrap()
                .status,
            CheckStatus::Missing
        );
        preflight_flatpak_requirement(&context, &requirement).unwrap();
        let arguments = fs::read_to_string(log).unwrap();
        assert!(
            arguments.contains(
                "--system\nremotes\n--show-disabled\n--columns=name,url,options,filter\n"
            ),
            "{arguments}"
        );
        assert!(
            arguments
                .contains("--system\nremote-info\n--arch\nx86_64\nflathub\norg.gnome.Calculator\n"),
            "{arguments}"
        );
    }
}
