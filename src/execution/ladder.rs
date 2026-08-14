//! Typed core/custom execution ladders and their durable journal vocabulary.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use tempfile::NamedTempFile;

use crate::model::manifest::ExecutionMode;
use crate::{Result, WombatError};

pub const EXECUTION_JOURNAL_FORMAT_VERSION: u32 = 4;

fn elapsed_ms(instant: Instant) -> u64 {
    u64::try_from(instant.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RungId(String);

impl RungId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.split('.').any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
        {
            return Err(WombatError::configuration(format!(
                "invalid execution rung `{value}`; expected dot-separated ASCII names"
            )));
        }
        Ok(Self(value))
    }

    pub fn id(&self) -> &str {
        &self.0
    }

    pub fn core(&self) -> Option<CoreRung> {
        CoreRung::parse(&self.0).ok()
    }
}

impl From<CoreRung> for RungId {
    fn from(value: CoreRung) -> Self {
        Self(value.id().to_string())
    }
}

impl PartialEq<CoreRung> for RungId {
    fn eq(&self, other: &CoreRung) -> bool {
        self.0 == other.id()
    }
}

impl std::fmt::Display for RungId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LadderRung {
    pub id: RungId,
    pub children: Vec<LadderRung>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlatRung {
    pub id: RungId,
    pub parent: Option<RungId>,
    pub depth: u8,
    pub core: Option<CoreRung>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLadder {
    pub name: String,
    pub roots: Vec<LadderRung>,
    pub flattened: Vec<FlatRung>,
}

impl Default for ExecutionLadder {
    fn default() -> Self {
        let roots = CoreRung::ALL
            .into_iter()
            .map(|core| LadderRung {
                id: core.into(),
                children: Vec::new(),
            })
            .collect::<Vec<_>>();
        Self::new("default".to_string(), roots).expect("the fixed ladder is valid")
    }
}

impl ExecutionLadder {
    pub fn new(name: String, roots: Vec<LadderRung>) -> Result<Self> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(WombatError::configuration(format!(
                "invalid ladder name `{name}`; expected ASCII letters, numbers, `-`, or `_`"
            )));
        }
        let mut flattened = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for root in &roots {
            flatten_rung(root, None, 0, &mut seen, &mut flattened)?;
        }
        for core in CoreRung::ALL {
            let matches = flattened
                .iter()
                .filter(|rung| rung.core == Some(core))
                .collect::<Vec<_>>();
            if matches.len() != 1 || matches[0].depth != 0 {
                return Err(WombatError::configuration(format!(
                    "ladder `{name}` must contain core rung `{}` exactly once at the top level",
                    core.id()
                )));
            }
        }
        let core_order = flattened
            .iter()
            .filter_map(|rung| rung.core)
            .collect::<Vec<_>>();
        if core_order != CoreRung::ALL {
            return Err(WombatError::configuration(format!(
                "ladder `{name}` changes the required core rung order"
            )));
        }
        Ok(Self {
            name,
            roots,
            flattened,
        })
    }

    pub fn leaf_ids(&self) -> impl Iterator<Item = &RungId> {
        self.flattened
            .iter()
            .filter(|rung| !self.is_container(&rung.id))
            .map(|rung| &rung.id)
    }

    pub fn contains(&self, id: &RungId) -> bool {
        self.flattened.iter().any(|rung| &rung.id == id)
    }

    pub fn validate(&self) -> Result<()> {
        let rebuilt = Self::new(self.name.clone(), self.roots.clone())?;
        if &rebuilt != self {
            return Err(WombatError::configuration(
                "execution ladder flattened order does not match its tree",
            ));
        }
        Ok(())
    }

    pub fn is_container(&self, id: &RungId) -> bool {
        fn find<'a>(nodes: &'a [LadderRung], id: &RungId) -> Option<&'a LadderRung> {
            nodes.iter().find_map(|node| {
                if &node.id == id {
                    Some(node)
                } else {
                    find(&node.children, id)
                }
            })
        }
        find(&self.roots, id).is_some_and(|rung| !rung.children.is_empty())
    }

    pub fn position(&self, id: &RungId) -> Option<usize> {
        self.leaf_ids().position(|candidate| candidate == id)
    }

    pub fn before_or_at(&self, candidate: &RungId, boundary: CoreRung) -> bool {
        self.position(candidate)
            .zip(self.position(&boundary.into()))
            .is_some_and(|(candidate, boundary)| candidate <= boundary)
    }

    pub fn at_or_after(&self, candidate: &RungId, boundary: CoreRung) -> bool {
        self.position(candidate)
            .zip(self.position(&boundary.into()))
            .is_some_and(|(candidate, boundary)| candidate >= boundary)
    }
}

fn flatten_rung(
    rung: &LadderRung,
    parent: Option<&RungId>,
    depth: u8,
    seen: &mut std::collections::BTreeSet<RungId>,
    flattened: &mut Vec<FlatRung>,
) -> Result<()> {
    if depth > 8 {
        return Err(WombatError::configuration(
            "execution ladder nesting exceeds the maximum depth of 8",
        ));
    }
    if !seen.insert(rung.id.clone()) {
        return Err(WombatError::configuration(format!(
            "execution rung `{}` is duplicated or reused",
            rung.id
        )));
    }
    let core = rung.id.core();
    if core.is_some() && (depth != 0 || !rung.children.is_empty()) {
        return Err(WombatError::configuration(format!(
            "core rung `{}` must be a top-level leaf",
            rung.id
        )));
    }
    flattened.push(FlatRung {
        id: rung.id.clone(),
        parent: parent.cloned(),
        depth,
        core,
    });
    for child in &rung.children {
        flatten_rung(child, Some(&rung.id), depth + 1, seen, flattened)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum CoreRung {
    #[serde(rename = "materialise.before")]
    MaterialiseBefore,
    #[serde(rename = "materialise.tasks")]
    MaterialiseTasks,
    #[serde(rename = "materialise.artifacts")]
    MaterialiseArtifacts,
    #[serde(rename = "materialise.publish")]
    MaterialisePublish,
    #[serde(rename = "materialise.after")]
    MaterialiseAfter,
    #[serde(rename = "deploy.before")]
    DeployBefore,
    #[serde(rename = "deploy.apply")]
    DeployApply,
    #[serde(rename = "deploy.after")]
    DeployAfter,
}

impl CoreRung {
    pub const ALL: [Self; 8] = [
        Self::MaterialiseBefore,
        Self::MaterialiseTasks,
        Self::MaterialiseArtifacts,
        Self::MaterialisePublish,
        Self::MaterialiseAfter,
        Self::DeployBefore,
        Self::DeployApply,
        Self::DeployAfter,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::MaterialiseBefore => "materialise.before",
            Self::MaterialiseTasks => "materialise.tasks",
            Self::MaterialiseArtifacts => "materialise.artifacts",
            Self::MaterialisePublish => "materialise.publish",
            Self::MaterialiseAfter => "materialise.after",
            Self::DeployBefore => "deploy.before",
            Self::DeployApply => "deploy.apply",
            Self::DeployAfter => "deploy.after",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|rung| rung.id() == value)
            .ok_or_else(|| {
                WombatError::configuration(format!(
                    "unknown execution rung `{value}`; expected one of {}",
                    Self::ALL.map(Self::id).join(", ")
                ))
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// What happened to a rung or action, as recorded in the journal.
///
/// `Running` is written before the work starts, so a journal reopened with a
/// `Running` entry means the previous run died partway. That is what
/// distinguishes `Interrupted` from `Failed`: nobody recorded a failure, the
/// process simply stopped existing.
pub enum ExecutionStatus {
    /// Not reached yet.
    Pending,
    /// Started, and no outcome recorded. Reopening turns this into
    /// `Interrupted`.
    Running,
    Succeeded,
    /// Ran and reported failure. The reason is kept alongside.
    Failed,
    /// Started but never completed, discovered by reopening the journal.
    Interrupted,
    /// Deliberately not run — compile-only policy, or a schedule that was
    /// already satisfied.
    Skipped,
    /// Satisfied by a previous run's result rather than executed again.
    Reused,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The operational record of one ladder execution.
///
/// Kept outside the product on purpose. Manifests are sealed at publication, so
/// "what happened when this ran" lives here instead — which is why inspection
/// combines the two, and reports execution state as unavailable when no journal
/// exists rather than inventing an outcome.
pub struct ExecutionJournal {
    pub format_version: u32,
    pub plan_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    pub requested_boundary: CoreRung,
    pub execution_mode: ExecutionMode,
    pub skipped_requirement_gates: Vec<String>,
    pub reuse_decisions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    pub rungs: Vec<RungRecord>,
    pub actions: Vec<ActionJournal>,
    // Wall-clock starts for whatever is currently `Running`, kept only for
    // this process's lifetime so `set_id`/`record_action` can compute a
    // duration on the matching terminal call without every caller threading
    // an `Instant` through. Never persisted: a reopened journal has none, so
    // an entry left `Running` across a crash simply has no duration, which is
    // correct — nobody knows how long it actually ran.
    #[serde(skip)]
    rung_started: BTreeMap<RungId, Instant>,
    #[serde(skip)]
    action_started: BTreeMap<String, Instant>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RungRecord {
    pub id: RungId,
    pub status: ExecutionStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionJournal {
    pub identity: String,
    pub rung: RungId,
    pub status: ExecutionStatus,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<u64>,
}

impl ExecutionJournal {
    pub fn new(plan_id: String, requested_boundary: CoreRung) -> Self {
        Self::new_for_ladder(plan_id, requested_boundary, &ExecutionLadder::default())
    }

    pub fn new_for_ladder(
        plan_id: String,
        requested_boundary: CoreRung,
        ladder: &ExecutionLadder,
    ) -> Self {
        Self {
            format_version: EXECUTION_JOURNAL_FORMAT_VERSION,
            plan_id,
            build_id: None,
            requested_boundary,
            execution_mode: ExecutionMode::Normal,
            skipped_requirement_gates: Vec::new(),
            reuse_decisions: Vec::new(),
            last_failure: None,
            rungs: ladder
                .leaf_ids()
                .cloned()
                .map(|id| RungRecord {
                    id,
                    status: ExecutionStatus::Pending,
                    duration_ms: None,
                })
                .collect(),
            actions: Vec::new(),
            rung_started: BTreeMap::new(),
            action_started: BTreeMap::new(),
        }
    }

    pub fn reopen(self, plan_id: &str, requested_boundary: CoreRung) -> Self {
        self.reopen_for_ladder(plan_id, requested_boundary, &ExecutionLadder::default())
    }

    pub fn reopen_for_ladder(
        mut self,
        plan_id: &str,
        requested_boundary: CoreRung,
        ladder: &ExecutionLadder,
    ) -> Self {
        if self.plan_id != plan_id || self.requested_boundary != requested_boundary {
            return Self::new_for_ladder(plan_id.to_string(), requested_boundary, ladder);
        }
        for record in &mut self.rungs {
            if record.status == ExecutionStatus::Running {
                record.status = ExecutionStatus::Interrupted;
            }
        }
        self
    }

    pub fn configure(
        &mut self,
        execution_mode: ExecutionMode,
        skipped_requirement_gates: Vec<String>,
    ) {
        self.execution_mode = execution_mode;
        self.skipped_requirement_gates = skipped_requirement_gates;
        self.last_failure = None;
    }

    pub fn record_reuse(&mut self, decision: impl Into<String>) {
        let decision = decision.into();
        if !self.reuse_decisions.contains(&decision) {
            self.reuse_decisions.push(decision);
        }
    }

    pub fn fail(&mut self, rung: CoreRung, error: &crate::WombatError) {
        self.fail_id(&rung.into(), error);
    }

    pub fn fail_id(&mut self, rung: &RungId, error: &crate::WombatError) {
        self.set_id(rung, ExecutionStatus::Failed);
        self.last_failure = Some(error.to_string());
    }

    pub fn set(&mut self, rung: CoreRung, status: ExecutionStatus) {
        self.set_id(&rung.into(), status);
    }

    pub fn set_id(&mut self, rung: &RungId, status: ExecutionStatus) {
        let duration_ms = if status == ExecutionStatus::Running {
            self.rung_started.insert(rung.clone(), Instant::now());
            None
        } else {
            self.rung_started.remove(rung).map(elapsed_ms)
        };
        if let Some(record) = self.rungs.iter_mut().find(|record| record.id == *rung) {
            record.status = status;
            if duration_ms.is_some() {
                record.duration_ms = duration_ms;
            }
        }
    }

    pub fn record_action(
        &mut self,
        identity: impl Into<String>,
        rung: &RungId,
        status: ExecutionStatus,
        reason: impl Into<String>,
    ) {
        let identity = identity.into();
        let duration_ms = if status == ExecutionStatus::Running {
            self.action_started.insert(identity.clone(), Instant::now());
            None
        } else {
            self.action_started.remove(&identity).map(elapsed_ms)
        };
        let action = ActionJournal {
            identity: identity.clone(),
            rung: rung.clone(),
            status,
            reason: reason.into(),
            duration_ms,
        };
        if let Some(current) = self
            .actions
            .iter_mut()
            .find(|current| current.identity == identity)
        {
            *current = action;
        } else {
            self.actions.push(action);
        }
    }
}

pub fn write(build_dir: &Path, journal: &ExecutionJournal) -> Result<()> {
    let path = build_dir.join(".wombat/execution-journal.json");
    write_at(&path, journal)
}

pub fn write_at(path: &Path, journal: &ExecutionJournal) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(journal)?;
    let parent = path
        .parent()
        .ok_or_else(|| WombatError::configuration("execution journal has no parent directory"))?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| WombatError::io(parent, error))?;
    std::io::Write::write_all(&mut temporary, &bytes)
        .map_err(|error| WombatError::io(path, error))?;
    temporary
        .persist(path)
        .map_err(|error| WombatError::io(path, error.error))?;
    Ok(())
}

pub fn read(build_dir: &Path) -> Result<ExecutionJournal> {
    let path = build_dir.join(".wombat/execution-journal.json");
    read_at(&path)
}

pub fn read_at(path: &Path) -> Result<ExecutionJournal> {
    let bytes = fs::read(path).map_err(|error| WombatError::io(path, error))?;
    let journal: ExecutionJournal = serde_json::from_slice(&bytes)?;
    if journal.format_version != EXECUTION_JOURNAL_FORMAT_VERSION {
        return Err(WombatError::configuration(
            "unsupported execution journal format",
        ));
    }
    Ok(journal)
}
