//! Wombat's pre-1.0 orchestration surface.
//!
//! The product is CLI-first. These exports exist for integration tests and
//! embedding experiments and do not carry a stability promise before 1.0.
#![warn(missing_docs)]

#[allow(
    missing_docs,
    reason = "pre-1.0 CLI authoring model is not a stable Rust API"
)]
mod authoring;
#[doc(hidden)]
#[allow(
    missing_docs,
    reason = "pre-1.0 build model is intentionally doc-hidden"
)]
pub mod build;
#[doc(hidden)]
#[allow(
    missing_docs,
    reason = "pre-1.0 deployment model is intentionally doc-hidden"
)]
pub mod deploy;
#[doc(hidden)]
#[allow(
    missing_docs,
    reason = "diagnostic internals are intentionally doc-hidden"
)]
pub mod error;
#[allow(missing_docs, reason = "execution wire types are pre-1.0 internals")]
mod execution;
#[doc(hidden)]
#[allow(
    missing_docs,
    reason = "inspection internals are intentionally doc-hidden"
)]
pub mod inspection;
#[doc(hidden)]
#[allow(
    missing_docs,
    reason = "Lua implementation details are intentionally doc-hidden"
)]
pub mod lua;
#[allow(
    missing_docs,
    reason = "persisted schemas are documented as formats, not Rust API"
)]
mod model;
#[doc(hidden)]
#[allow(
    missing_docs,
    reason = "presentation internals are intentionally doc-hidden"
)]
pub mod presentation;
#[allow(
    missing_docs,
    reason = "project configuration internals are intentionally doc-hidden"
)]
mod project;
#[doc(hidden)]
#[allow(
    missing_docs,
    reason = "provider internals are intentionally doc-hidden"
)]
pub mod requirements;
mod storage;

#[doc(hidden)]
pub use deploy::reconcile;
#[doc(hidden)]
pub use execution::ladder;
#[doc(hidden)]
pub use model::plan;
#[doc(hidden)]
pub use model::{context, frozen, manifest};
#[doc(hidden)]
pub use project::config;

pub use authoring::add::{AddMethod, AddOutcome, AddStatus, add};
pub use authoring::initialize::{InitOutcome, InitStatus, initialize};
pub use authoring::repository::{
    AcquisitionOutcome, AcquisitionStatus, RepositoryIdentity, RepositoryLocator,
    acquire_repository,
};
pub use build::{
    BuildOptions, BuildOutcome, BuildStatus, MaterialiseOutcome, OpenedBuild, PlanOutcome,
    VerifiedBuild, build, materialise, open_build, plan, plan_or_reuse, project_help,
    project_help_with_options, verify_build,
};
pub use deploy::reconcile::{ReconciliationAction, ReconciliationItem, ReconciliationPlan};
pub use deploy::{
    ApplyOutcome, ApplyStatus, ConflictPolicy, ConflictResolution, DeploymentOptions, DiffOutcome,
    PreparedApply, apply, diff, prepare_apply,
};
pub use error::{Diagnostic, ErrorKind, Result, WombatError};
pub use inspection::{InspectSection, PlanInspectSection, compare, explain, inspect, inspect_plan};
pub use model::context::{
    Architecture, Distribution, HostContext, HostPaths, Kernel, LooseVersion, OperatingSystem,
    OperatingSystemName, ResolvedTarget, TargetOrigin, TargetPlatform,
};
pub use model::manifest::Manifest;
pub use presentation::{ColorPolicy, LogLevel, Presenter, Role};
pub use requirements::{
    BootstrapOutcome, CheckItem, CheckOutcome, CheckStatus, authorize_target_plan, check,
    check_plan, check_target_plan, prepare_target_plan_until_authorized,
};
