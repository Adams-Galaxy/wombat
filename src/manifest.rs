use serde::{Deserialize, Serialize};

use crate::frozen::FrozenValue;

pub const MANIFEST_FORMAT_VERSION: u32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    pub wombat_version: String,
    pub build_id: String,
    pub modules: Vec<ManifestModule>,
    pub dependencies: Vec<Dependency>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestModule {
    pub name: String,
    pub config: FrozenValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub source: String,
    pub target: TargetPath,
    pub content: FileContent,
    pub owner: String,
    pub declared_from: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileContent {
    pub digest: String,
    pub size: u64,
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EvaluatedManifest {
    pub modules: Vec<ManifestModule>,
    pub dependencies: Vec<Dependency>,
    pub artifacts: Vec<EvaluatedArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct EvaluatedArtifact {
    pub kind: ArtifactKind,
    pub source: String,
    pub target: TargetPath,
    pub owner: String,
    pub declared_from: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    File,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetPath {
    pub anchor: TargetAnchor,
    pub path: String,
    pub display: String,
    pub origin: TargetOrigin,
}

impl TargetPath {
    pub(crate) fn key(&self) -> (TargetAnchor, &str) {
        (self.anchor, &self.path)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetAnchor {
    Home,
    Config,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetOrigin {
    Explicit {
        declared: String,
    },
    Inferred {
        basis: InferenceBasis,
        source_anchor: SourceAnchor,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceBasis {
    ModuleAnchor,
    SourcePrefix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAnchor {
    Home,
    DotConfig,
}
