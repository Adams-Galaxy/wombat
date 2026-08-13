use serde::{Deserialize, Serialize};

use crate::execution::ladder::{ExecutionLadder, RungId};
use crate::model::context::ResolvedTarget;
use crate::model::frozen::FrozenValue;

use crate::model::source::{DirectoryLeaf, SourceFingerprint};

pub const MANIFEST_FORMAT_VERSION: u32 = 16;
pub const BUILD_PLAN_FORMAT_VERSION: u32 = 7;

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
    pub plan_id: String,
    pub execution_mode: ExecutionMode,
    pub skipped_requirement_gates: Vec<String>,
    pub sources: Vec<SourceFile>,
    pub inputs: Vec<BuildInput>,
    pub target: ResolvedTarget,
    pub observations: Vec<Observation>,
    pub process_observations: Vec<ProcessObservation>,
    pub modules: Vec<ManifestModule>,
    pub dependencies: Vec<Dependency>,
    pub project_identity: String,
    pub ladder: ExecutionLadder,
    pub providers: Vec<Provider>,
    pub requirements: Vec<Requirement>,
    pub preparations: Vec<ProviderPreparation>,
    pub tasks: Vec<Task>,
    pub scripts: Vec<Script>,
    pub artifact_policy: ArtifactPolicy,
    pub artifact_notices: Vec<ArtifactNotice>,
    pub artifact_selections: Vec<ArtifactSelection>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Normal,
    CompileOnly,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildPlan {
    pub format_version: u32,
    pub wombat_version: String,
    pub plan_id: String,
    pub project_arguments: Vec<String>,
    pub sources: Vec<SourceFile>,
    pub inputs: Vec<BuildInput>,
    pub target: ResolvedTarget,
    pub observations: Vec<Observation>,
    pub process_observations: Vec<ProcessObservation>,
    pub modules: Vec<ManifestModule>,
    pub dependencies: Vec<Dependency>,
    pub project_identity: String,
    pub ladder: ExecutionLadder,
    pub providers: Vec<Provider>,
    pub requirements: Vec<Requirement>,
    pub preparations: Vec<ProviderPreparation>,
    pub tasks: Vec<Task>,
    pub scripts: Vec<Script>,
    pub artifact_policy: ArtifactPolicy,
    pub artifact_notices: Vec<ArtifactNotice>,
    pub artifact_selections: Vec<ArtifactSelection>,
    pub artifacts: Vec<PlannedArtifact>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnallocatedPolicy {
    Ignore,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPolicy {
    pub unallocated: UnallocatedPolicy,
}

impl Default for ArtifactPolicy {
    fn default() -> Self {
        Self {
            unallocated: UnallocatedPolicy::Warn,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactNotice {
    pub kind: ArtifactNoticeKind,
    pub owner: String,
    pub selector: String,
    pub skipped: Vec<String>,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactNoticeKind {
    UnallocatedSkipped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSelection {
    pub owner: String,
    pub declared: String,
    pub expanded: String,
    pub physical: String,
    pub source_base: String,
    pub source_base_logical: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_base_target: Option<String>,
    pub source_base_hidden: bool,
    pub hidden: bool,
    pub kind: ArtifactSelectionKind,
    pub static_root: String,
    pub exclusions: Vec<String>,
    pub allow_empty: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit_target: Option<String>,
    pub matches: Vec<String>,
    pub skipped_unallocated: Vec<String>,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSelectionKind {
    Exact,
    Directory,
    Glob,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub name: String,
    pub priority: u32,
    pub config: FrozenValue,
    pub origin: ProviderOrigin,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderOrigin {
    Builtin {
        contract_version: u32,
    },
    Custom {
        entrypoint: String,
        files: Vec<ProviderFile>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFile {
    pub source: String,
    pub payload: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub kind: RequirementKind,
    pub owner: String,
    pub declared_at: SourceTrace,
    pub candidates: Vec<RequirementCandidate>,
    pub attempts: Vec<ResolutionAttempt>,
    pub selected: u32,
    pub choice: RequirementChoice,
    pub binding: ProviderBinding,
    pub when: RungId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementKind {
    Command,
    Package,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequirementCandidate {
    Command {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimum: Option<String>,
    },
    Package {
        name: String,
        provider: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimum: Option<String>,
        publications: Publications,
        with: FrozenValue,
    },
}

impl RequirementCandidate {
    pub fn name(&self) -> &str {
        match self {
            Self::Command { name, .. } | Self::Package { name, .. } => name,
        }
    }

    pub fn minimum(&self) -> Option<&str> {
        match self {
            Self::Command { minimum, .. } | Self::Package { minimum, .. } => minimum.as_deref(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publications {
    pub commands: Vec<String>,
}

impl Publications {
    pub fn command(name: impl Into<String>) -> Self {
        Self {
            commands: vec![name.into()],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionAttempt {
    pub candidate: u32,
    pub provider: String,
    pub outcome: ResolutionOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolutionOutcome {
    Selected,
    Unsupported { reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementChoice {
    Required,
    Preferred,
    Accepted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderBinding {
    pub provider: String,
    pub identity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    pub publications: Publications,
    pub data: FrozenValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPreparation {
    pub provider: String,
    pub identity: String,
    pub description: String,
    pub elevated: bool,
    pub data: FrozenValue,
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

/// A construction-time process result. Raw streams are intentionally never
/// persisted: Lua has already consumed them during construction, while the
/// executable plan only needs an identity commitment and safe inspection data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessObservation {
    pub invocation: ProcessInvocation,
    pub cwd: String,
    pub environment: Vec<ProcessEnvironmentChange>,
    pub stdin_digest: Option<String>,
    pub timeout_ms: Option<u64>,
    pub max_output: u64,
    pub sensitive: bool,
    pub ok: bool,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_size: u64,
    pub stdout_digest: String,
    pub stderr_size: u64,
    pub stderr_digest: String,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessInvocation {
    Exec { argv: Vec<String> },
    Shell { command: String, shell: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEnvironmentChange {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestModule {
    pub name: String,
    pub source: String,
    pub config: FrozenValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_base: Option<ModuleSourceBase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleSourceBase {
    pub declared: String,
    pub expanded: String,
    pub physical: String,
    pub logical: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub hidden: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_projection: Option<SourceProjection>,
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
    GeneratedLua {
        contract_version: u32,
    },
    Task {
        contract_version: u32,
        identity: String,
        output: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedArtifact {
    pub source: String,
    pub source_origin: SourceOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_projection: Option<SourceProjection>,
    pub production: PlannedProduction,
    pub target: TargetPath,
    pub owner: String,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannedProduction {
    Static {
        source_digest: String,
        executable: bool,
    },
    Template {
        renderer: RendererIdentity,
        source_digest: String,
        context: FrozenValue,
        executable: bool,
    },
    GeneratedLua {
        contract_version: u32,
        content_digest: String,
        size: u64,
        executable: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub identity: String,
    pub declaration_order: u64,
    pub owner: String,
    pub entrypoint: String,
    pub entrypoint_digest: String,
    pub params: FrozenValue,
    pub runner: TaskRunner,
    pub python_helper: bool,
    pub logs: TaskLogPolicy,
    pub cache: TaskCachePolicy,
    pub at: RungId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_root: Option<TaskTargetRoot>,
    pub declared_at: SourceTrace,
    pub outputs: Vec<TaskOutput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub identity: String,
    pub declaration_order: u64,
    pub owner: String,
    pub entrypoint: String,
    pub params: FrozenValue,
    pub runner: TaskRunner,
    pub python_helper: bool,
    pub logs: TaskLogPolicy,
    pub at: RungId,
    pub schedule: ScriptSchedule,
    pub scope: ScriptScope,
    pub payloads: Vec<ScriptPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptSchedule {
    Always,
    Once,
    Onchange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptScope {
    Target,
    Host,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptPayload {
    pub source: String,
    pub relative: String,
    pub digest: String,
    pub size: u64,
    pub executable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptOutcome {
    pub identity: String,
    pub rung: RungId,
    pub status: ScriptOutcomeStatus,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptOutcomeStatus {
    Ran,
    ScheduledSkip,
    CompileOnlySkip,
    Refused,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskRunner {
    EmbeddedLua {
        contract_version: u32,
    },
    Direct {
        contract_version: u32,
    },
    Interpreter {
        contract_version: u32,
        family: InterpreterFamily,
        command: String,
        args: Vec<String>,
    },
}

impl TaskRunner {
    pub const fn contract_version(&self) -> u32 {
        match self {
            Self::EmbeddedLua { contract_version }
            | Self::Direct { contract_version }
            | Self::Interpreter {
                contract_version, ..
            } => *contract_version,
        }
    }

    pub const fn is_embedded_lua(&self) -> bool {
        matches!(self, Self::EmbeddedLua { .. })
    }

    pub const fn is_direct(&self) -> bool {
        matches!(self, Self::Direct { .. })
    }

    pub fn interpreter(&self) -> Option<(InterpreterFamily, &str, &[String])> {
        match self {
            Self::Interpreter {
                family,
                command,
                args,
                ..
            } => Some((*family, command, args)),
            Self::EmbeddedLua { .. } | Self::Direct { .. } => None,
        }
    }

    pub fn command(&self) -> Option<&str> {
        self.interpreter().map(|(_, command, _)| command)
    }

    pub fn args(&self) -> &[String] {
        self.interpreter()
            .map_or(&[], |(_, _, arguments)| arguments)
    }

    pub fn is_python(&self) -> bool {
        matches!(
            self,
            Self::Interpreter {
                family: InterpreterFamily::Python,
                ..
            }
        )
    }

    pub const fn family_name(&self) -> &'static str {
        match self {
            Self::EmbeddedLua { .. } => "embedded_lua",
            Self::Direct { .. } => "direct",
            Self::Interpreter { family, .. } => family.as_str(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterpreterFamily {
    Python,
    PosixShell,
    Bash,
    Custom,
}

impl InterpreterFamily {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::PosixShell => "posix_shell",
            Self::Bash => "bash",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLogPolicy {
    Failure,
    Always,
    Never,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCachePolicy {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTargetRoot {
    pub path: String,
    pub origin: EvaluatedTargetOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutput {
    pub relative: String,
    pub content: FileContent,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluatedManifest {
    pub plan_id: String,
    pub project_arguments: Vec<String>,
    pub sources: Vec<SourceFile>,
    pub inputs: Vec<BuildInput>,
    pub target: ResolvedTarget,
    pub observations: Vec<Observation>,
    pub process_observations: Vec<ProcessObservation>,
    pub modules: Vec<ManifestModule>,
    pub dependencies: Vec<Dependency>,
    pub project_identity: String,
    pub ladder: ExecutionLadder,
    pub providers: Vec<Provider>,
    pub requirements: Vec<Requirement>,
    pub preparations: Vec<ProviderPreparation>,
    pub tasks: Vec<EvaluatedTask>,
    pub scripts: Vec<Script>,
    pub artifact_policy: ArtifactPolicy,
    pub artifact_notices: Vec<ArtifactNotice>,
    pub artifact_selections: Vec<ArtifactSelection>,
    pub artifacts: Vec<EvaluatedArtifact>,
    pub directories: Vec<EvaluatedDirectory>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluatedArtifact {
    pub kind: ArtifactKind,
    pub source: String,
    pub source_origin: SourceOrigin,
    pub source_projection: Option<SourceProjection>,
    pub production: EvaluatedProduction,
    pub target: TargetPath,
    pub fingerprint: Option<SourceFingerprint>,
    pub owner: String,
    pub declared_at: SourceTrace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum EvaluatedProduction {
    Static,
    Template {
        context: FrozenValue,
    },
    GeneratedLua {
        content: Vec<u8>,
        executable: bool,
    },
    Task {
        identity: String,
        output: String,
        content: Vec<u8>,
        executable: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluatedTask {
    pub task: Task,
    pub fingerprint: SourceFingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluatedDirectory {
    pub declared_source: String,
    pub root: String,
    pub physical_selector: String,
    pub static_root: String,
    pub hidden: bool,
    pub glob: bool,
    pub exclusions: Vec<String>,
    pub target_root: Option<EvaluatedTargetRoot>,
    pub owner: String,
    pub declared_at: SourceTrace,
    pub snapshot: Vec<DirectoryLeaf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluatedTargetRoot {
    pub path: String,
    pub origin: EvaluatedTargetOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvaluatedTargetOrigin {
    Explicit { declared: String },
    Inferred { source: String },
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
        expanded: String,
    },
    Directory {
        declared: String,
        expanded: String,
        root: String,
        relative: String,
        exclusions: Vec<String>,
        allow_empty: bool,
    },
    Generated {
        name: String,
    },
    Task {
        identity: String,
        relative: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProjection {
    pub physical: String,
    pub logical: String,
    pub allocated: bool,
    pub hidden: bool,
    pub components: Vec<SourceComponent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceComponent {
    pub physical: String,
    pub logical: String,
    pub attributes: Vec<SourceAttribute>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAttribute {
    Dot,
    Unallocated,
    Literal,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetPath {
    pub path: String,
    pub origin: TargetOrigin,
}

impl TargetPath {
    pub(crate) fn key(&self) -> &str {
        &self.path
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetOrigin {
    Explicit { declared: String },
    Inferred { source: String },
    DirectoryExplicit { declared: String, relative: String },
}

#[cfg(test)]
mod tests {
    use super::TaskRunner;

    #[test]
    fn task_runner_wire_shape_cannot_represent_partial_or_mixed_runners() {
        let missing_command = serde_json::json!({
            "kind": "interpreter",
            "contract_version": 1,
            "family": "python",
            "args": []
        });
        assert!(serde_json::from_value::<TaskRunner>(missing_command).is_err());

        let mixed_embedded = serde_json::json!({
            "kind": "embedded_lua",
            "contract_version": 1,
            "command": "lua"
        });
        assert!(serde_json::from_value::<TaskRunner>(mixed_embedded).is_err());
    }
}
