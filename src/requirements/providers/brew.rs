//! Homebrew binding helpers and environment policy.

use super::*;

/// Batched Homebrew metadata for one check pass. Homebrew's Ruby startup makes
/// one query per package disproportionately expensive.
pub(crate) struct BrewSnapshot {
    brew: Option<PathBuf>,
    formulae: BTreeMap<String, serde_json::Value>,
    casks: BTreeMap<String, serde_json::Value>,
}

impl BrewSnapshot {
    pub(crate) fn fetch(requirements: &[Requirement]) -> Result<Self> {
        let Some(brew) = which("brew") else {
            return Ok(Self {
                brew: None,
                formulae: BTreeMap::new(),
                casks: BTreeMap::new(),
            });
        };
        let mut formula_names = BTreeSet::new();
        let mut cask_names = BTreeSet::new();
        for requirement in requirements {
            if requirement.binding.provider != BuiltinProvider::Brew.name() {
                continue;
            }
            if requirement.kind == RequirementKind::Command
                && selected_candidate(requirement)
                    .is_ok_and(|selected| which(selected.name()).is_some())
            {
                continue;
            }
            let Ok((kind, name)) = brew_identity(&requirement.binding) else {
                continue;
            };
            if kind == "cask" {
                cask_names.insert(name.to_string());
            } else {
                formula_names.insert(name.to_string());
            }
        }
        Ok(Self {
            formulae: Self::fetch_records(&brew, "--formula", "formulae", "name", &formula_names)?,
            casks: Self::fetch_records(&brew, "--cask", "casks", "token", &cask_names)?,
            brew: Some(brew),
        })
    }

    fn fetch_records(
        brew: &Path,
        flag: &str,
        array_key: &str,
        name_field: &str,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, serde_json::Value>> {
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut args = vec!["info", "--json=v2", flag];
        args.extend(names.iter().map(String::as_str));
        let output = run_bounded(brew, &args, &brew_environment())?;
        if !output.success {
            return Ok(BTreeMap::new());
        }
        let json: serde_json::Value = serde_json::from_slice(&output.stdout.bytes)?;
        let mut records = BTreeMap::new();
        if let Some(array) = json.get(array_key).and_then(serde_json::Value::as_array) {
            for entry in array {
                if let Some(name) = entry.get(name_field).and_then(serde_json::Value::as_str) {
                    records.insert(name.to_string(), entry.clone());
                }
            }
        }
        Ok(records)
    }
}

pub(crate) fn check_brew(
    binding: &ProviderBinding,
    minimum: Option<&str>,
    snapshot: &BrewSnapshot,
) -> Result<CheckItem> {
    let (kind, name) = brew_identity(binding)?;
    let Some(brew) = &snapshot.brew else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "Homebrew is not available on PATH",
        ));
    };
    let cached = if kind == "cask" {
        snapshot.casks.get(name)
    } else {
        snapshot.formulae.get(name)
    };
    let entry = match cached {
        Some(entry) => entry.clone(),
        None => {
            let output = run_bounded(
                brew,
                &["info", "--json=v2", brew_flag(kind), name],
                &brew_environment(),
            )?;
            if !output.success {
                return Ok(provider_item(
                    binding,
                    CheckStatus::Unavailable,
                    &format!("brew info failed: {}", output_detail(&output)),
                ));
            }
            let json: serde_json::Value = serde_json::from_slice(&output.stdout.bytes)?;
            let array_key = if kind == "cask" { "casks" } else { "formulae" };
            let Some(entry) = json
                .get(array_key)
                .and_then(serde_json::Value::as_array)
                .and_then(|values| values.first())
            else {
                return Err(WombatError::configuration(
                    "Homebrew returned no matching package record",
                ));
            };
            entry.clone()
        }
    };
    let installed = installed_brew_versions(&entry);
    let Some(observed) = installed.last() else {
        return Ok(provider_item(
            binding,
            CheckStatus::Missing,
            "not installed",
        ));
    };
    if let Some(minimum) = minimum
        && !version_at_least(observed, minimum)
    {
        return Ok(provider_item(
            binding,
            CheckStatus::Outdated,
            &format!("observed {observed}; needs at least {minimum}"),
        ));
    }
    Ok(provider_item(
        binding,
        CheckStatus::Satisfied,
        &format!("{kind} {name} {observed}"),
    ))
}

pub(crate) fn preflight_brew(requirement: &Requirement) -> Result<()> {
    let (kind, name) = brew_identity(&requirement.binding)?;
    let brew = which("brew").ok_or_else(|| {
        WombatError::configuration("cannot bootstrap because Homebrew is not available")
    })?;
    let operation = brew_operation(&requirement.binding)?;
    let output = run_bounded(
        &brew,
        &[operation, "--dry-run", brew_flag(kind), name],
        &brew_environment(),
    )?;
    if !output.success {
        return Err(WombatError::configuration(format!(
            "Homebrew preflight failed for `{name}`: {}",
            output_detail(&output)
        )));
    }
    Ok(())
}

pub(crate) fn reconcile_brew(requirement: &Requirement) -> Result<()> {
    let (kind, name) = brew_identity(&requirement.binding)?;
    let brew = which("brew")
        .ok_or_else(|| WombatError::configuration("Homebrew disappeared before bootstrap"))?;
    let operation = brew_operation(&requirement.binding)?;
    let mut command = Command::new(&brew);
    command
        .args([operation, brew_flag(kind), name])
        .envs(brew_environment());
    let child_status = crate::execution::process::run_inherited(&mut command, "Homebrew")?;
    if !child_status.success {
        return Err(WombatError::configuration(format!(
            "Homebrew {operation} failed for `{name}` with {}",
            child_status.status
        )));
    }
    Ok(())
}

pub(crate) fn brew_identity(binding: &ProviderBinding) -> Result<(&str, &str)> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration(
            "Homebrew binding data must be a map",
        ));
    };
    let kind = match data.get("kind") {
        Some(FrozenValue::String(value)) => value.as_str(),
        _ => return Err(WombatError::configuration("Homebrew binding lacks kind")),
    };
    let name = match data.get("name") {
        Some(FrozenValue::String(value)) => value.as_str(),
        _ => return Err(WombatError::configuration("Homebrew binding lacks name")),
    };
    Ok((kind, name))
}

pub(crate) fn brew_flag(kind: &str) -> &'static str {
    if kind == "cask" {
        "--cask"
    } else {
        "--formula"
    }
}

pub(crate) fn brew_operation(binding: &ProviderBinding) -> Result<&'static str> {
    match check_brew(binding, None, &BrewSnapshot::fetch(&[])?)?.status {
        CheckStatus::Satisfied | CheckStatus::Outdated => Ok("upgrade"),
        CheckStatus::Missing => Ok("install"),
        CheckStatus::Unavailable => Err(WombatError::configuration(
            "Homebrew package state became unavailable during bootstrap",
        )),
    }
}

pub(crate) fn installed_brew_versions(entry: &serde_json::Value) -> Vec<String> {
    match entry.get("installed") {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|value| {
                value
                    .get("version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect(),
        Some(serde_json::Value::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    }
}

pub(crate) fn brew_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOMEBREW_NO_AUTO_UPDATE".to_string(), "1".to_string()),
        ("HOMEBREW_NO_INSTALL_CLEANUP".to_string(), "1".to_string()),
    ])
}
