//! Built-in and Lua provider execution for checked prerequisites, shared
//! preparation, and requirement reconciliation.

use super::*;
use builtin::BuiltinProvider;
pub(super) use builtin::validate_builtin_contracts;

mod apt;
mod brew;
pub(crate) mod builtin;
mod custom;
mod dnf;
mod flatpak;
mod git;
mod support;

pub(super) use apt::*;
pub(super) use brew::*;
pub(super) use custom::*;
use dnf::*;
pub(super) use dnf::{check_dnf, check_rpmfusion};
use flatpak::*;
pub(super) use flatpak::{check_flathub, check_flatpak};
pub(super) use git::*;
pub(super) use support::*;

fn builtin_provider(provider: &Provider) -> Option<BuiltinProvider> {
    matches!(provider.origin, ProviderOrigin::Builtin { .. })
        .then(|| BuiltinProvider::from_name(&provider.name))
        .flatten()
}

pub(super) fn prepare_provider(
    context: &RequirementContext<'_>,
    operation: &ProviderPreparation,
    noninteractive: bool,
) -> Result<()> {
    let provider = provider_for(context.providers, &operation.provider)?;
    match (&provider.origin, builtin_provider(provider)) {
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Apt)) => {
            prepare_apt(operation, noninteractive)
        }
        (ProviderOrigin::Builtin { .. }, _) => Err(WombatError::configuration(format!(
            "built-in provider `{}` does not support preparation",
            provider.name
        ))),
        (ProviderOrigin::Custom { .. }, _) => {
            prepare_custom(context, provider, operation, noninteractive)
        }
    }
}

pub(super) fn reconcile_prerequisite(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
    observation: &CheckItem,
    noninteractive: bool,
) -> Result<()> {
    let provider = provider_for(context.providers, &prerequisite.provider)?;
    match (&provider.origin, builtin_provider(provider)) {
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Apt)) => {
            reconcile_apt_source(context, prerequisite, noninteractive)
        }
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Dnf)) => {
            reconcile_rpmfusion(context, prerequisite, noninteractive)
        }
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Flatpak)) => {
            reconcile_flathub(context, prerequisite, noninteractive)
        }
        (ProviderOrigin::Builtin { .. }, _) => Err(WombatError::configuration(format!(
            "built-in provider `{}` does not support prerequisites",
            provider.name
        ))),
        (ProviderOrigin::Custom { .. }, _) => reconcile_custom_prerequisite(
            context,
            provider,
            prerequisite,
            observation,
            noninteractive,
        ),
    }
}

pub(super) fn preflight(
    context: &RequirementContext<'_>,
    preparations: &[&ProviderPreparation],
    pending: &[&CheckItem],
) -> Result<()> {
    for item in pending
        .iter()
        .filter(|item| item.subject == CheckSubject::Prerequisite)
    {
        let prerequisite = prerequisite_for_item(context, item)?;
        let provider = provider_for(context.providers, &prerequisite.provider)?;
        match (&provider.origin, builtin_provider(provider)) {
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Apt)) => {
                preflight_apt_source(context, prerequisite)?;
            }
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Dnf)) => {
                preflight_rpmfusion(context, prerequisite)?;
            }
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Flatpak)) => {
                preflight_flathub(context, prerequisite)?;
            }
            (ProviderOrigin::Builtin { .. }, _) => {
                return Err(WombatError::configuration(format!(
                    "built-in provider `{}` does not support prerequisites",
                    provider.name
                )));
            }
            (ProviderOrigin::Custom { .. }, _) => {
                preflight_custom_prerequisite(context, provider, prerequisite)?;
            }
        }
    }
    for operation in preparations {
        let provider = provider_for(context.providers, &operation.provider)?;
        match (&provider.origin, builtin_provider(provider)) {
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Apt)) => {
                preflight_apt_preparation(operation)?;
            }
            (ProviderOrigin::Builtin { .. }, _) => {
                return Err(WombatError::configuration(format!(
                    "built-in provider `{}` does not support preparation",
                    provider.name
                )));
            }
            (ProviderOrigin::Custom { .. }, _) => {
                preflight_custom_preparation(context, provider, operation)?;
            }
        }
    }
    for item in pending
        .iter()
        .filter(|item| item.subject == CheckSubject::Requirement)
    {
        let requirement = requirement_for_item(context, item)?;
        let provider = provider_for(context.providers, &requirement.binding.provider)?;
        match (&provider.origin, builtin_provider(provider)) {
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Brew)) => {
                preflight_brew(requirement)?;
            }
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Apt)) => {
                if !requirement.binding.prerequisites.is_empty() {
                    preflight_elevation(requirement.binding.elevated)?;
                    continue;
                }
                preflight_apt_requirement(requirement)?;
            }
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Dnf)) => {
                if requirement.binding.prerequisites.is_empty() {
                    preflight_dnf_requirement(context, requirement)?;
                } else {
                    preflight_elevation(requirement.binding.elevated)?;
                }
            }
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Flatpak)) => {
                if requirement.binding.prerequisites.is_empty() {
                    preflight_flatpak_requirement(context, requirement)?;
                } else {
                    preflight_elevation(requirement.binding.elevated)?;
                }
            }
            (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Git)) => {
                preflight_git(requirement)?;
            }
            (ProviderOrigin::Builtin { .. }, _) => unreachable!(),
            (ProviderOrigin::Custom { .. }, _) => {
                preflight_custom_requirement(context, provider)?;
            }
        }
    }
    Ok(())
}

pub(super) fn preflight_deferred_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
) -> Result<()> {
    let provider = provider_for(context.providers, &requirement.binding.provider)?;
    if matches!(provider.origin, ProviderOrigin::Builtin { .. })
        && !builtin_provider(provider)
            .is_some_and(|provider| provider.capabilities().deferred_preflight)
    {
        return Ok(());
    }
    match (&provider.origin, builtin_provider(provider)) {
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Apt)) => {
            preflight_apt_requirement(requirement)
        }
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Dnf)) => {
            preflight_dnf_requirement(context, requirement)
        }
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Flatpak)) => {
            preflight_flatpak_requirement(context, requirement)
        }
        (ProviderOrigin::Builtin { .. }, _) | (ProviderOrigin::Custom { .. }, _) => Ok(()),
    }
}

pub(super) fn reconcile_requirement(
    context: &RequirementContext<'_>,
    requirement: &Requirement,
    status: CheckStatus,
    noninteractive: bool,
) -> Result<()> {
    let provider = provider_for(context.providers, &requirement.binding.provider)?;
    match (&provider.origin, builtin_provider(provider)) {
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Brew)) => {
            reconcile_brew(requirement)?;
        }
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Apt)) => {
            reconcile_apt_requirement(requirement, noninteractive)?;
        }
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Dnf)) => {
            reconcile_dnf_requirement(context, requirement, noninteractive)?;
        }
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Flatpak)) => {
            reconcile_flatpak_requirement(context, requirement, noninteractive)?;
        }
        (ProviderOrigin::Builtin { .. }, Some(BuiltinProvider::Git)) => {
            reconcile_git(requirement, noninteractive)?;
        }
        (ProviderOrigin::Builtin { .. }, _) => unreachable!(),
        (ProviderOrigin::Custom { .. }, _) => {
            reconcile_custom_requirement(context, provider, requirement, status, noninteractive)?;
        }
    }
    Ok(())
}
