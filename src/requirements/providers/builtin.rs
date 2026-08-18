//! Authoritative registry and construction policy for built-in providers.
//!
//! Built-in names, contract versions, embedded Lua policy, and lifecycle
//! capabilities live here so adding a provider cannot silently omit one of the
//! validation or execution boundaries.

use super::*;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum BuiltinProvider {
    Apt,
    Brew,
    Dnf,
    Flatpak,
    Git,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Capabilities {
    pub(crate) prerequisites: bool,
    pub(crate) preparation: bool,
    pub(crate) deferred_preflight: bool,
    pub(crate) elevation: bool,
    pub(crate) serialized_checks: bool,
}

impl BuiltinProvider {
    pub(crate) const ALL: [Self; 5] = [Self::Apt, Self::Brew, Self::Dnf, Self::Flatpak, Self::Git];

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.name() == name)
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Brew => "brew",
            Self::Dnf => "dnf",
            Self::Flatpak => "flatpak",
            Self::Git => "git",
        }
    }

    pub(crate) const fn contract_version(self) -> u32 {
        match self {
            Self::Apt => 2,
            Self::Brew | Self::Dnf | Self::Flatpak | Self::Git => 1,
        }
    }

    pub(crate) const fn lua_source(self) -> &'static str {
        match self {
            Self::Apt => include_str!("../../../lua/wombat/providers/apt.lua"),
            Self::Brew => include_str!("../../../lua/wombat/providers/brew.lua"),
            Self::Dnf => include_str!("../../../lua/wombat/providers/dnf.lua"),
            Self::Flatpak => include_str!("../../../lua/wombat/providers/flatpak.lua"),
            Self::Git => include_str!("../../../lua/wombat/providers/git.lua"),
        }
    }

    pub(crate) const fn capabilities(self) -> Capabilities {
        match self {
            Self::Apt => Capabilities {
                prerequisites: true,
                preparation: true,
                deferred_preflight: true,
                elevation: true,
                serialized_checks: false,
            },
            Self::Brew | Self::Git => Capabilities {
                prerequisites: false,
                preparation: false,
                deferred_preflight: false,
                elevation: false,
                serialized_checks: false,
            },
            Self::Dnf => Capabilities {
                prerequisites: true,
                preparation: false,
                deferred_preflight: true,
                elevation: true,
                serialized_checks: true,
            },
            Self::Flatpak => Capabilities {
                prerequisites: true,
                preparation: false,
                deferred_preflight: true,
                elevation: true,
                serialized_checks: false,
            },
        }
    }
}

pub(crate) fn validate_builtin_contracts(
    requirements: &[Requirement],
    prerequisites: &[ProviderPrerequisite],
    preparations: &[ProviderPreparation],
) -> Result<()> {
    validate_apt_contract(requirements, prerequisites, preparations)?;
    validate_dnf_contract(requirements, prerequisites)?;
    validate_flatpak_contract(requirements, prerequisites)?;

    for prerequisite in prerequisites {
        if let Some(provider) = BuiltinProvider::from_name(&prerequisite.provider)
            && !provider.capabilities().prerequisites
        {
            return Err(WombatError::configuration(
                "this built-in provider does not support prerequisites",
            ));
        }
    }
    for operation in preparations {
        if let Some(provider) = BuiltinProvider::from_name(&operation.provider)
            && !provider.capabilities().preparation
        {
            return Err(WombatError::configuration(
                "this built-in provider does not support preparation operations",
            ));
        }
    }
    for requirement in requirements {
        let Some(provider) = BuiltinProvider::from_name(&requirement.binding.provider) else {
            continue;
        };
        if requirement.binding.elevated && !provider.capabilities().elevation {
            return Err(WombatError::configuration(format!(
                "{} package bindings must not declare elevation",
                provider.name()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn registry_names_versions_sources_and_capabilities_are_complete() {
        let mut names = BTreeSet::new();
        let expected = [
            (
                BuiltinProvider::Apt,
                "apt",
                2,
                Capabilities {
                    prerequisites: true,
                    preparation: true,
                    deferred_preflight: true,
                    elevation: true,
                    serialized_checks: false,
                },
            ),
            (
                BuiltinProvider::Brew,
                "brew",
                1,
                Capabilities {
                    prerequisites: false,
                    preparation: false,
                    deferred_preflight: false,
                    elevation: false,
                    serialized_checks: false,
                },
            ),
            (
                BuiltinProvider::Dnf,
                "dnf",
                1,
                Capabilities {
                    prerequisites: true,
                    preparation: false,
                    deferred_preflight: true,
                    elevation: true,
                    serialized_checks: true,
                },
            ),
            (
                BuiltinProvider::Flatpak,
                "flatpak",
                1,
                Capabilities {
                    prerequisites: true,
                    preparation: false,
                    deferred_preflight: true,
                    elevation: true,
                    serialized_checks: false,
                },
            ),
            (
                BuiltinProvider::Git,
                "git",
                1,
                Capabilities {
                    prerequisites: false,
                    preparation: false,
                    deferred_preflight: false,
                    elevation: false,
                    serialized_checks: false,
                },
            ),
        ];
        for (provider, name, contract_version, capabilities) in expected {
            assert!(names.insert(provider.name()));
            assert_eq!(provider.name(), name);
            assert_eq!(BuiltinProvider::from_name(provider.name()), Some(provider));
            assert!(provider.lua_source().contains("provider.define"));
            assert_eq!(provider.contract_version(), contract_version);
            assert_eq!(provider.capabilities(), capabilities);
        }
        assert_eq!(names.len(), BuiltinProvider::ALL.len());
        assert_eq!(BuiltinProvider::from_name("unknown"), None);
    }
}
