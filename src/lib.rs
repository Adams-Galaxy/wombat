pub mod add;
pub mod build;
mod cache;
#[doc(hidden)]
pub mod config;
pub mod context;
pub mod deploy;
pub mod error;
pub mod frozen;
pub mod initialize;
mod inputs;
pub mod inspection;
pub mod ladder;
pub mod manifest;
mod path;
pub mod plan;
pub mod presentation;
mod project;
pub mod reconcile;
pub mod repository;
pub mod requirements;
pub mod runtime;
mod scripts;
mod selection;
mod source;
mod state;
mod tasks;

pub use add::{AddMethod, AddOutcome, AddStatus, add};
pub use build::{
    BuildOptions, BuildOutcome, BuildStatus, MaterialiseOutcome, OpenedBuild, PlanOutcome,
    VerifiedBuild, build, materialise, open_build, plan, plan_or_reuse, project_help,
    project_help_with_options, verify_build,
};
pub use context::{
    Architecture, Distribution, HostContext, Kernel, LooseVersion, OperatingSystem,
    OperatingSystemName, ResolvedTarget, TargetOrigin, TargetPlatform,
};
pub use deploy::{
    ApplyOutcome, ApplyStatus, ConflictPolicy, ConflictResolution, DeploymentOptions, DiffOutcome,
    PreparedApply, apply, diff, prepare_apply,
};
pub use error::{Diagnostic, ErrorKind, Result, WombatError};
pub use initialize::{InitOutcome, InitStatus, initialize};
pub use inspection::{InspectSection, PlanInspectSection, compare, explain, inspect, inspect_plan};
pub use manifest::Manifest;
pub use presentation::{ColorPolicy, LogLevel, Presenter, Role};
pub use reconcile::{ReconciliationAction, ReconciliationItem, ReconciliationPlan};
pub use repository::{
    AcquisitionOutcome, AcquisitionStatus, RepositoryIdentity, RepositoryLocator,
    acquire_repository,
};
pub use requirements::{
    BootstrapOutcome, CheckItem, CheckOutcome, CheckStatus, authorize_target_plan, check,
    check_plan, check_target_plan, prepare_target_plan_until_authorized,
};
