use serde::{Deserialize, Serialize};

use crate::context::ResolvedTarget;
use crate::frozen::FrozenValue;

use crate::source::{DirectoryLeaf, SourceFingerprint};

pub const MANIFEST_FORMAT_VERSION: u32 = 7;

pub const MAX_SOURCE_TRACE_FRAMES: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

impl std::fmt::Display for SourceLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.source)?;
        if let Some(line) = self.line {
            write!(formatter, ":{line}")?;
            if let Some(column) = self.column {
                write!(formatter, ":{column}")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTrace {
    pub primary: SourceLocation,
    pub callers: Vec<SourceLocation>,
}

impl std::fmt::Display for SourceTrace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.primary.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    pub wombat_version: String,
    pub build_id: String,
    pub sources: Vec<SourceFile>,
    pub inputs: Vec<BuildInput>,
    pub target: ResolvedTarget,
    pub observations: Vec<Observation>,
    pub modules: Vec<ManifestModule>,
    pub dependencies: Vec<Dependency>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildInputKind {
    Flag,
    Choice,
    String,
    Integer,
    Target,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildInputOrigin {
    Default,
    CommandLine,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInput {
    pub name: String,
    pub kind: BuildInputKind,
    pub value: FrozenValue,
    pub origin: BuildInputOrigin,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSubject {
    Host,
    Target,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub subject: ObservationSubject,
    pub path: String,
    pub value: FrozenValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestModule {
    pub name: String,
    pub source: String,
    pub config: FrozenValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub kind: DependencyKind,
    pub from: String,
    pub to: String,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyKind {
    Use,
    Using,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub source: String,
    pub source_origin: SourceOrigin,
    pub production: Production,
    pub target: TargetPath,
    pub content: FileContent,
    pub owner: String,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Production {
    Static,
    Template {
        renderer: RendererIdentity,
        source_digest: String,
        context: FrozenValue,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererIdentity {
    pub name: String,
    pub contract_version: u32,
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
    pub sources: Vec<SourceFile>,
    pub inputs: Vec<BuildInput>,
    pub target: ResolvedTarget,
    pub observations: Vec<Observation>,
    pub modules: Vec<ManifestModule>,
    pub dependencies: Vec<Dependency>,
    pub artifacts: Vec<EvaluatedArtifact>,
    pub directories: Vec<EvaluatedDirectory>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EvaluatedArtifact {
    pub kind: ArtifactKind,
    pub source: String,
    pub source_origin: SourceOrigin,
    pub production: EvaluatedProduction,
    pub target: TargetPath,
    pub fingerprint: SourceFingerprint,
    pub owner: String,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EvaluatedProduction {
    Static,
    Template { context: FrozenValue },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EvaluatedDirectory {
    pub declared_source: String,
    pub root: String,
    pub target_root: EvaluatedTargetRoot,
    pub owner: String,
    pub declared_at: SourceTrace,
    pub snapshot: Vec<DirectoryLeaf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EvaluatedTargetRoot {
    pub anchor: TargetAnchor,
    pub path: String,
    pub origin: EvaluatedTargetOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EvaluatedTargetOrigin {
    Explicit {
        declared: String,
    },
    Inferred {
        basis: InferenceBasis,
        source_anchor: SourceAnchor,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    File,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceOrigin {
    Direct {
        declared: String,
    },
    Directory {
        declared: String,
        root: String,
        relative: String,
    },
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
    DirectoryExplicit {
        declared: String,
        relative: String,
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
    DotLocal,
}
