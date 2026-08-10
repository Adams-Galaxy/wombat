use serde::{Deserialize, Serialize};

use crate::frozen::FrozenValue;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub modules: Vec<ManifestModule>,
    pub dependencies: Vec<Dependency>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestModule {
    pub name: String,
    pub config: FrozenValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Dependency {
    pub kind: DependencyKind,
    pub from: String,
    pub to: String,
    pub declared_from: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyKind {
    Use,
    Using,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub source: String,
    pub target: String,
    pub owner: String,
    pub declared_from: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    File,
}
