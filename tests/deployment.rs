use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::process::{Command, Output};

use wombat::{
    ApplyStatus, Architecture, BuildOptions, ConflictPolicy, DeploymentOptions, HostContext,
    OperatingSystemName, ReconciliationAction, TargetPlatform, apply, build, diff, open_build,
    prepare_apply,
};

fn host(os: OperatingSystemName, arch: Architecture) -> HostContext {
    HostContext::fixture(TargetPlatform::minimal(os, arch))
}

struct Repository {
    root: PathBuf,
    build_dir: PathBuf,
    home: PathBuf,
    state: PathBuf,
    _temporary: tempfile::TempDir,
}

impl Repository {
    fn new(contents: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repository");
        let build_dir = root.join("build");
        let home = temporary.path().join("home");
        let state = temporary.path().join("state");
        fs::create_dir_all(root.join("modules/dot_config")).unwrap();
        fs::create_dir_all(root.join("dot_config")).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(
            root.join("wombat.lua"),
            "local w = require('wombat')\nw.use('app')\n",
        )
        .unwrap();
        fs::write(
            root.join("modules/dot_config/app.lua"),
            "local w = require('wombat')\nw.install('app.toml')\n",
        )
        .unwrap();
        fs::write(root.join("dot_config/app.toml"), contents).unwrap();
        Self {
            root,
            build_dir,
            home,
            state,
            _temporary: temporary,
        }
    }

    fn build(&self) -> wombat::BuildOutcome {
        build(BuildOptions::new(&self.root, &self.build_dir)).unwrap()
    }

    fn options(&self) -> DeploymentOptions {
        DeploymentOptions::new(&self.build_dir, &self.home).with_state_root(&self.state)
    }

    fn target(&self) -> PathBuf {
        self.home.join(".config/app.toml")
    }

    fn state_json(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.state_dir().join("state.json")).unwrap()).unwrap()
    }

    fn state_dir(&self) -> PathBuf {
        fs::read_dir(self.state.join("wombat/targets"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
}

fn run_wombat(args: &[&str], current_dir: &Path, home: &Path, state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(args)
        .current_dir(current_dir)
        .env("HOME", home)
        .env("XDG_STATE_HOME", state)
        .output()
        .unwrap()
}

fn run_wombat_with_input(
    args: &[&str],
    current_dir: &Path,
    home: &Path,
    state: &Path,
    input: &str,
) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(args)
        .current_dir(current_dir)
        .env("HOME", home)
        .env("XDG_STATE_HOME", state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn diff_apply_update_and_unchanged_form_a_complete_workflow() {
    let repository = Repository::new("theme = 'dark'\n");
    let first = repository.build();

    let first_diff = diff(&repository.options()).unwrap();
    assert!(first_diff.output.contains("Create ~/.config/app.toml"));
    assert!(!first_diff.output.contains("+theme = 'dark'"));
    assert!(first_diff.output.contains("1 changes: 1 create"));
    let patched = diff(&repository.options().with_patch(true)).unwrap();
    assert!(patched.output.contains("+theme = 'dark'"));
    assert_eq!(
        first_diff.plan.items[0].action,
        ReconciliationAction::Create
    );

    let applied = apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(applied.status, ApplyStatus::Applied);
    assert_eq!(applied.created, 1);
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "theme = 'dark'\n"
    );
    assert_eq!(
        repository.state_json()["complete_build_id"],
        serde_json::Value::String(first.build_id)
    );

    let unchanged = apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(unchanged.status, ApplyStatus::Unchanged);
    assert_eq!(
        diff(&repository.options()).unwrap().output,
        "No differences.\n"
    );

    fs::write(
        repository.root.join("dot_config/app.toml"),
        "theme = 'light'\n",
    )
    .unwrap();
    repository.build();
    let update = diff(&repository.options()).unwrap();
    assert_eq!(update.plan.items[0].action, ReconciliationAction::Update);
    assert!(update.output.contains("-theme = 'dark'"));
    assert!(update.output.contains("+theme = 'light'"));
    let applied = apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(applied.updated, 1);
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "theme = 'light'\n"
    );
}

#[test]
fn source_only_identity_changes_advance_complete_state_without_rewriting_targets() {
    let repository = Repository::new("theme = 'dark'\n");
    let first = repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    let before = fs::metadata(repository.target())
        .unwrap()
        .modified()
        .unwrap();

    let module = repository.root.join("modules/dot_config/app.lua");
    let original = fs::read_to_string(&module).unwrap();
    fs::write(&module, format!("-- provenance only\n{original}")).unwrap();
    let second = repository.build();
    assert_ne!(first.build_id, second.build_id);
    assert_eq!(
        first.manifest.artifacts[0].content,
        second.manifest.artifacts[0].content
    );

    let outcome = apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(outcome.status, ApplyStatus::Applied);
    assert_eq!(outcome.created + outcome.updated + outcome.removed, 0);
    assert_eq!(outcome.state_advanced, 1);
    assert_eq!(
        fs::metadata(repository.target())
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    assert_eq!(
        repository.state_json()["complete_build_id"],
        serde_json::Value::String(second.build_id)
    );
}

#[test]
fn downstream_changes_fail_skip_and_overwrite_with_incomplete_state() {
    let repository = Repository::new("version = 1\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();

    fs::write(repository.target(), "downstream = true\n").unwrap();
    fs::write(repository.root.join("dot_config/app.toml"), "version = 2\n").unwrap();
    let desired = repository.build();
    let error = apply(&repository.options(), ConflictPolicy::Fail)
        .unwrap_err()
        .to_string();
    assert!(error.contains("modified downstream"), "{error}");
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "downstream = true\n"
    );

    let skipped = apply(&repository.options(), ConflictPolicy::Skip).unwrap();
    assert_eq!(skipped.status, ApplyStatus::AppliedWithSkips);
    assert_eq!(skipped.skipped, vec!["~/.config/app.toml"]);
    assert!(repository.state_json()["complete_build_id"].is_null());
    assert_ne!(
        repository.state_json()["artifacts"][0]["content"],
        serde_json::to_value(&desired.manifest.artifacts[0].content).unwrap()
    );

    let overwritten = apply(&repository.options(), ConflictPolicy::Overwrite).unwrap();
    assert_eq!(overwritten.updated, 1);
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "version = 2\n"
    );
    assert_eq!(
        repository.state_json()["complete_build_id"],
        serde_json::Value::String(desired.build_id)
    );
}

#[test]
fn skips_preserve_conflicted_records_while_committing_successful_artifacts() {
    let repository = Repository::new("unused\n");
    fs::write(
        repository.root.join("modules/dot_config/app.lua"),
        "local w = require('wombat')\nw.install('a')\nw.install('b')\n",
    )
    .unwrap();
    fs::write(repository.root.join("dot_config/a"), "a1\n").unwrap();
    fs::write(repository.root.join("dot_config/b"), "b1\n").unwrap();
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    let old_state = repository.state_json();

    fs::write(repository.root.join("dot_config/a"), "a2\n").unwrap();
    fs::write(repository.root.join("dot_config/b"), "b2\n").unwrap();
    fs::write(repository.home.join(".config/b"), "downstream\n").unwrap();
    repository.build();
    let outcome = apply(&repository.options(), ConflictPolicy::Skip).unwrap();
    assert_eq!(outcome.updated, 1);
    assert_eq!(outcome.skipped, vec!["~/.config/b"]);
    assert_eq!(
        fs::read_to_string(repository.home.join(".config/a")).unwrap(),
        "a2\n"
    );
    assert_eq!(
        fs::read_to_string(repository.home.join(".config/b")).unwrap(),
        "downstream\n"
    );
    let new_state = repository.state_json();
    assert!(new_state["complete_build_id"].is_null());
    assert_ne!(
        new_state["artifacts"][0]["content"],
        old_state["artifacts"][0]["content"]
    );
    assert_eq!(
        new_state["artifacts"][1]["content"],
        old_state["artifacts"][1]["content"]
    );
}

#[test]
fn unmanaged_collisions_are_reported_together_and_fail_without_mutation() {
    let repository = Repository::new("unused\n");
    fs::write(
        repository.root.join("modules/dot_config/app.lua"),
        "local w = require('wombat')\nw.install('a')\nw.install('b')\n",
    )
    .unwrap();
    fs::write(repository.root.join("dot_config/a"), "desired a\n").unwrap();
    fs::write(repository.root.join("dot_config/b"), "desired b\n").unwrap();
    repository.build();
    fs::create_dir_all(repository.home.join(".config")).unwrap();
    fs::write(repository.home.join(".config/a"), "existing a\n").unwrap();
    fs::write(repository.home.join(".config/b"), "existing b\n").unwrap();
    let error = apply(&repository.options(), ConflictPolicy::Fail)
        .unwrap_err()
        .to_string();
    assert!(error.contains("~/.config/a"), "{error}");
    assert!(error.contains("~/.config/b"), "{error}");
    assert_eq!(
        fs::read_to_string(repository.home.join(".config/a")).unwrap(),
        "existing a\n"
    );
    assert_eq!(
        fs::read_to_string(repository.home.join(".config/b")).unwrap(),
        "existing b\n"
    );
}

#[test]
fn stale_files_are_removed_only_when_they_still_match_previous_state() {
    let repository = Repository::new("managed = true\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    fs::write(
        repository.root.join("modules/dot_config/app.lua"),
        "return true\n",
    )
    .unwrap();
    repository.build();

    let plan = diff(&repository.options()).unwrap().plan;
    assert_eq!(plan.items[0].action, ReconciliationAction::Remove);
    let removed = apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(removed.removed, 1);
    assert!(!repository.target().exists());
    assert_eq!(
        repository.state_json()["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    fs::write(
        repository.root.join("modules/dot_config/app.lua"),
        "local w = require('wombat')\nw.install('app.toml')\n",
    )
    .unwrap();
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    fs::write(repository.target(), "changed = true\n").unwrap();
    fs::write(
        repository.root.join("modules/dot_config/app.lua"),
        "return true\n",
    )
    .unwrap();
    repository.build();
    let error = apply(&repository.options(), ConflictPolicy::Fail)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("stale managed target was modified"),
        "{error}"
    );
    assert!(repository.target().is_file());
}

#[test]
fn an_already_correct_unmanaged_file_is_adopted_without_rewriting() {
    let repository = Repository::new("correct = true\n");
    repository.build();
    fs::create_dir_all(repository.home.join(".config")).unwrap();
    fs::write(repository.target(), "correct = true\n").unwrap();
    let before = fs::metadata(repository.target())
        .unwrap()
        .modified()
        .unwrap();

    let plan = diff(&repository.options()).unwrap().plan;
    assert_eq!(plan.items[0].action, ReconciliationAction::Adopt);
    let outcome = apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(outcome.state_advanced, 1);
    assert_eq!(
        fs::metadata(repository.target())
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
}

#[test]
fn downstream_deletion_conflicts_then_becomes_a_safe_forget_when_no_longer_desired() {
    let repository = Repository::new("managed = true\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    fs::remove_file(repository.target()).unwrap();

    let plan = diff(&repository.options()).unwrap().plan;
    assert_eq!(plan.items[0].action, ReconciliationAction::Conflict);
    assert!(
        plan.items[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("deleted downstream")
    );

    fs::write(
        repository.root.join("modules/dot_config/app.lua"),
        "return true\n",
    )
    .unwrap();
    repository.build();
    let plan = diff(&repository.options()).unwrap().plan;
    assert_eq!(plan.items[0].action, ReconciliationAction::Forget);
    let outcome = apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(outcome.state_advanced, 1);
    assert!(
        repository.state_json()["artifacts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn identical_content_ownership_transfer_advances_state_without_rewriting() {
    let repository = Repository::new("same = true\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    let before = fs::metadata(repository.target())
        .unwrap()
        .modified()
        .unwrap();

    fs::write(
        repository.root.join("wombat.lua"),
        "local w = require('wombat')\nw.use('other')\n",
    )
    .unwrap();
    fs::write(
        repository.root.join("modules/dot_config/other.lua"),
        "local w = require('wombat')\nw.install('app.toml')\n",
    )
    .unwrap();
    repository.build();
    let plan = diff(&repository.options()).unwrap().plan;
    assert_eq!(plan.items[0].action, ReconciliationAction::AdvanceState);
    let outcome = apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(outcome.state_advanced, 1);
    assert_eq!(
        fs::metadata(repository.target())
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    assert_eq!(repository.state_json()["artifacts"][0]["owner"], "other");
}

#[cfg(unix)]
#[test]
fn mode_only_downstream_changes_are_conflicts() {
    use std::os::unix::fs::PermissionsExt as _;

    let repository = Repository::new("mode = true\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    fs::set_permissions(repository.target(), fs::Permissions::from_mode(0o600)).unwrap();
    let plan = diff(&repository.options()).unwrap().plan;
    assert_eq!(plan.items[0].action, ReconciliationAction::Conflict);
    assert!(
        plan.items[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("modified downstream")
    );
}

#[test]
fn target_drift_after_preflight_aborts_before_any_mutation() {
    let repository = Repository::new("desired = true\n");
    repository.build();
    let prepared = prepare_apply(&repository.options()).unwrap();
    fs::create_dir_all(repository.home.join(".config")).unwrap();
    fs::write(repository.target(), "appeared = true\n").unwrap();

    let error = prepared.apply(&Default::default()).unwrap_err().to_string();
    assert!(
        error.contains("changed after deployment planning"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "appeared = true\n"
    );
    assert!(!repository.state_dir().join("state.json").exists());
}

#[cfg(unix)]
#[test]
fn path_component_swap_after_preflight_aborts_before_any_mutation() {
    use std::os::unix::fs::symlink;

    let repository = Repository::new("desired = true\n");
    repository.build();
    let prepared = prepare_apply(&repository.options()).unwrap();
    let elsewhere = repository.home.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, repository.home.join(".config")).unwrap();

    let error = prepared.apply(&Default::default()).unwrap_err().to_string();
    assert!(
        error.contains("changed after deployment planning"),
        "{error}"
    );
    assert!(!elsewhere.join("app.toml").exists());
    assert!(!repository.state_dir().join("state.json").exists());
}

#[test]
fn build_and_target_locks_are_held_for_the_entire_prepared_operation() {
    let repository = Repository::new("locked = true\n");
    repository.build();
    let opened = open_build(&repository.build_dir).unwrap();
    let error = build(BuildOptions::new(&repository.root, &repository.build_dir))
        .unwrap_err()
        .to_string();
    assert!(error.contains("in use by another process"), "{error}");
    drop(opened);

    let prepared = prepare_apply(&repository.options()).unwrap();
    let error = prepare_apply(&repository.options())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("in use by another Wombat process"),
        "{error}"
    );
    let error = diff(&repository.options()).unwrap_err().to_string();
    assert!(
        error.contains("in use by another Wombat process"),
        "{error}"
    );
    prepared.apply(&Default::default()).unwrap();
}

#[test]
fn partial_filesystem_failure_keeps_old_state_and_retry_adopts_completed_files() {
    let repository = Repository::new("a = true\n");
    fs::write(
        repository.root.join("modules/dot_config/app.lua"),
        "local w = require('wombat')\nw.install('a.toml')\nw.install('b.toml')\n",
    )
    .unwrap();
    fs::write(repository.root.join("dot_config/a.toml"), "a = true\n").unwrap();
    fs::write(repository.root.join("dot_config/b.toml"), "b = true\n").unwrap();
    repository.build();
    let prepared = prepare_apply(&repository.options()).unwrap();
    fs::write(repository.build_dir.join("tree/config/b.toml"), "corrupt\n").unwrap();

    let error = prepared.apply(&Default::default()).unwrap_err().to_string();
    assert!(
        error.contains("changed while it was being applied"),
        "{error}"
    );
    assert!(repository.home.join(".config/a.toml").is_file());
    assert!(!repository.home.join(".config/b.toml").exists());
    assert!(!repository.state_dir().join("state.json").exists());

    repository.build();
    let plan = diff(&repository.options()).unwrap().plan;
    assert!(
        plan.items
            .iter()
            .any(|item| item.action == ReconciliationAction::Adopt)
    );
    assert!(
        plan.items
            .iter()
            .any(|item| item.action == ReconciliationAction::Create)
    );
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
}

#[cfg(unix)]
#[test]
fn state_write_failure_keeps_old_state_and_retry_advances_it_safely() {
    use std::os::unix::fs::PermissionsExt as _;

    let repository = Repository::new("version = 1\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    let state_before = fs::read(repository.state_dir().join("state.json")).unwrap();
    fs::write(repository.root.join("dot_config/app.toml"), "version = 2\n").unwrap();
    repository.build();
    let prepared = prepare_apply(&repository.options()).unwrap();
    fs::set_permissions(repository.state_dir(), fs::Permissions::from_mode(0o500)).unwrap();
    let result = prepared.apply(&Default::default());
    fs::set_permissions(repository.state_dir(), fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err().to_string();
    assert!(error.contains("failed to access"), "{error}");
    assert_eq!(
        fs::read(repository.state_dir().join("state.json")).unwrap(),
        state_before
    );
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "version = 2\n"
    );

    let plan = diff(&repository.options()).unwrap().plan;
    assert_eq!(plan.items[0].action, ReconciliationAction::AdvanceState);
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
}

#[test]
fn binary_diff_reports_digests_sizes_and_modes_without_dumping_bytes() {
    let repository = Repository::new("text\n");
    fs::write(repository.root.join("dot_config/app.toml"), [0, 1, 2, 3]).unwrap();
    repository.build();
    let output = diff(&repository.options().with_patch(true)).unwrap().output;
    assert!(
        output.contains("binary: absent 0 bytes mode ---- -> sha256:"),
        "{output}"
    );
    assert!(output.contains("4 bytes mode 0644"), "{output}");
}

#[test]
fn target_compatibility_precedes_state_or_target_mutation_and_explicit_roots_allow_testing() {
    let repository = Repository::new("target = true\n");
    fs::write(
        repository.root.join("wombat.lua"),
        "local w = require('wombat')\nlocal i = w.inputs({ target = w.input.target() })\nw.target(i.target)\nw.use('app')\n",
    )
    .unwrap();
    build(
        BuildOptions::new(&repository.root, &repository.build_dir)
            .with_project_arguments(["--target", "linux/x86_64"])
            .with_host(host(OperatingSystemName::Macos, Architecture::Aarch64)),
    )
    .unwrap();
    let implicit_state = repository._temporary.path().join("implicit-state");
    let options = DeploymentOptions::new(&repository.build_dir, &repository.home)
        .with_state_root(&implicit_state)
        .with_target_home_explicit(false)
        .with_host(host(OperatingSystemName::Macos, Architecture::Aarch64));
    let error = prepare_apply(&options).unwrap_err().to_string();
    assert!(error.contains("target OS `linux`"), "{error}");
    assert!(error.contains("host OS `macos`"), "{error}");
    assert!(error.contains("--target-home"), "{error}");
    assert!(!implicit_state.exists());
    assert!(!repository.target().exists());

    let explicit = DeploymentOptions::new(&repository.build_dir, &repository.home)
        .with_state_root(&repository.state)
        .with_target_home_explicit(true)
        .with_host(host(OperatingSystemName::Macos, Architecture::Aarch64));
    let prepared = prepare_apply(&explicit).unwrap();
    assert_eq!(prepared.warnings().len(), 1);
    assert!(prepared.warnings()[0].contains("x86_64"));
    assert!(prepared.warnings()[0].contains("aarch64"));
    let applied = prepared.apply(&std::collections::BTreeMap::new()).unwrap();
    assert_eq!(applied.created, 1);
    assert_eq!(applied.warnings.len(), 1);
}

#[test]
fn focused_prepared_diff_contains_only_the_selected_conflict() {
    let repository = Repository::new("first = 1\n");
    fs::write(
        repository.root.join("modules/dot_config/app.lua"),
        "local w = require('wombat')\nw.install('app.toml')\nw.install('other.toml')\n",
    )
    .unwrap();
    fs::write(
        repository.root.join("dot_config/other.toml"),
        "second = 1\n",
    )
    .unwrap();
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    fs::write(repository.target(), "downstream = 'first'\n").unwrap();
    fs::write(
        repository.home.join(".config/other.toml"),
        "downstream = 'second'\n",
    )
    .unwrap();
    fs::write(repository.root.join("dot_config/app.toml"), "first = 2\n").unwrap();
    fs::write(
        repository.root.join("dot_config/other.toml"),
        "second = 2\n",
    )
    .unwrap();
    repository.build();
    let prepared = prepare_apply(&repository.options()).unwrap();
    let focused = prepared.rendered_diff_for("~/.config/app.toml").unwrap();
    assert!(focused.contains("~/.config/app.toml"), "{focused}");
    assert!(!focused.contains("other.toml"), "{focused}");
    assert!(focused.contains("downstream = 'first'"), "{focused}");
}

#[cfg(unix)]
#[test]
fn symlink_leaves_and_components_are_conflicts_and_unknown_neighbours_survive() {
    use std::os::unix::fs::symlink;

    let repository = Repository::new("safe = true\n");
    repository.build();
    fs::create_dir_all(repository.home.join("elsewhere")).unwrap();
    symlink(
        repository.home.join("elsewhere"),
        repository.home.join(".config"),
    )
    .unwrap();
    let error = apply(&repository.options(), ConflictPolicy::Fail)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symbolic link"), "{error}");

    fs::remove_file(repository.home.join(".config")).unwrap();
    fs::create_dir(repository.home.join(".config")).unwrap();
    fs::write(repository.home.join("elsewhere/app.toml"), "linked\n").unwrap();
    symlink(
        repository.home.join("elsewhere/app.toml"),
        repository.target(),
    )
    .unwrap();
    let error = apply(&repository.options(), ConflictPolicy::Fail)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symbolic link"), "{error}");
    fs::remove_file(repository.target()).unwrap();
    fs::write(repository.home.join(".config/unknown"), "keep\n").unwrap();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    assert_eq!(
        fs::read_to_string(repository.home.join(".config/unknown")).unwrap(),
        "keep\n"
    );
}

#[test]
fn incompatible_target_types_are_conflicts_and_can_be_skipped_without_claiming_completion() {
    let repository = Repository::new("file = true\n");
    repository.build();
    fs::create_dir_all(repository.target()).unwrap();
    let plan = diff(&repository.options()).unwrap().plan;
    assert_eq!(plan.items[0].action, ReconciliationAction::Conflict);
    assert!(
        plan.items[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("not a regular file")
    );
    let outcome = apply(&repository.options(), ConflictPolicy::Skip).unwrap();
    assert_eq!(outcome.status, ApplyStatus::AppliedWithSkips);
    assert!(repository.target().is_dir());
    assert!(repository.state_json()["complete_build_id"].is_null());
}

#[test]
fn relocated_products_apply_without_source_or_workspace_state() {
    let repository = Repository::new("portable = true\n");
    let built = repository.build();
    let relocated = repository._temporary.path().join("relocated");
    fs::create_dir(&relocated).unwrap();
    fs::copy(
        repository.build_dir.join("manifest.json"),
        relocated.join("manifest.json"),
    )
    .unwrap();
    copy_directory(&repository.build_dir.join("tree"), &relocated.join("tree"));

    let opened = open_build(&relocated).unwrap();
    assert_eq!(opened.manifest.build_id, built.build_id);
    assert_ne!(opened.product_dir, relocated);
    drop(opened);
    let options =
        DeploymentOptions::new(&relocated, &repository.home).with_state_root(&repository.state);
    apply(&options, ConflictPolicy::Fail).unwrap();
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "portable = true\n"
    );
}

#[test]
fn cli_deploy_builds_and_applies_once_and_noninteractive_conflicts_fail() {
    let repository = Repository::new("cli = 1\n");
    let output = run_wombat(
        &[
            "--source",
            repository.root.to_str().unwrap(),
            "deploy",
            "--target-home",
            repository.home.to_str().unwrap(),
        ],
        repository._temporary.path(),
        &repository.home,
        &repository.state,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "cli = 1\n"
    );

    fs::write(repository.target(), "downstream = true\n").unwrap();
    fs::write(repository.root.join("dot_config/app.toml"), "cli = 2\n").unwrap();
    let output = run_wombat(
        &[
            "--source",
            repository.root.to_str().unwrap(),
            "deploy",
            "--target-home",
            repository.home.to_str().unwrap(),
        ],
        repository._temporary.path(),
        &repository.home,
        &repository.state,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unresolved conflicts"));
}

#[test]
fn cli_absolute_diff_and_apply_need_no_source_and_explicit_ask_gathers_decisions() {
    let repository = Repository::new("version = 1\n");
    repository.build();
    let build = repository.build_dir.to_str().unwrap();
    let home = repository.home.to_str().unwrap();
    let output = run_wombat(
        &["diff", "-B", build, "--target-home", home],
        repository._temporary.path(),
        &repository.home,
        &repository.state,
    );
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Create ~/.config/app.toml"));
    let output = run_wombat(
        &["apply", "-B", build, "--target-home", home],
        repository._temporary.path(),
        &repository.home,
        &repository.state,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::write(repository.target(), "downstream = true\n").unwrap();
    fs::write(repository.root.join("dot_config/app.toml"), "version = 2\n").unwrap();
    repository.build();
    let skipped = run_wombat_with_input(
        &[
            "apply",
            "-B",
            build,
            "--target-home",
            home,
            "--conflict",
            "ask",
        ],
        repository._temporary.path(),
        &repository.home,
        &repository.state,
        "diff\nskip\n",
    );
    assert!(
        skipped.status.success(),
        "{}",
        String::from_utf8_lossy(&skipped.stderr)
    );
    assert!(String::from_utf8_lossy(&skipped.stderr).contains("@@"));
    assert!(String::from_utf8_lossy(&skipped.stdout).contains("applied with skips"));
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "downstream = true\n"
    );

    let overwritten = run_wombat_with_input(
        &[
            "apply",
            "-B",
            build,
            "--target-home",
            home,
            "--conflict",
            "ask",
        ],
        repository._temporary.path(),
        &repository.home,
        &repository.state,
        "overwrite\n",
    );
    assert!(overwritten.status.success());
    assert_eq!(
        fs::read_to_string(repository.target()).unwrap(),
        "version = 2\n"
    );
}

#[test]
fn target_config_is_literal_dot_config_even_when_xdg_config_home_differs() {
    let repository = Repository::new("literal = true\n");
    repository.build();
    let other_config = repository._temporary.path().join("other-config");
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args([
            "apply",
            "-B",
            repository.build_dir.to_str().unwrap(),
            "--conflict",
            "fail",
        ])
        .current_dir(repository._temporary.path())
        .env("HOME", &repository.home)
        .env("XDG_CONFIG_HOME", &other_config)
        .env("XDG_STATE_HOME", &repository.state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repository.home.join(".config/app.toml").is_file());
    assert!(!other_config.join("app.toml").exists());
}

#[test]
fn cli_state_falls_back_beneath_invoking_home_when_xdg_state_home_is_absent() {
    let repository = Repository::new("fallback = true\n");
    repository.build();
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args([
            "apply",
            "-B",
            repository.build_dir.to_str().unwrap(),
            "--conflict",
            "fail",
        ])
        .current_dir(repository._temporary.path())
        .env("HOME", &repository.home)
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repository.home.join(".local/state/wombat/targets").is_dir());
}

#[test]
fn malformed_or_unknown_target_state_is_rejected_before_target_access() {
    let repository = Repository::new("state = true\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    let state_path = repository.state_dir().join("state.json");
    let mut state = repository.state_json();
    state["unknown"] = serde_json::json!(true);
    fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    let error = diff(&repository.options()).unwrap_err().to_string();
    assert!(error.contains("unknown field `unknown`"), "{error}");
}

#[test]
fn unsupported_target_state_versions_are_rejected() {
    let repository = Repository::new("state = true\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    let state_path = repository.state_dir().join("state.json");
    let mut state = repository.state_json();
    state["format_version"] = serde_json::json!(3);
    fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    let error = diff(&repository.options()).unwrap_err().to_string();
    assert!(
        error.contains("unsupported target state format version 3") && error.contains("expected 2"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn target_state_and_lock_use_private_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let repository = Repository::new("private = true\n");
    repository.build();
    apply(&repository.options(), ConflictPolicy::Fail).unwrap();
    let directory = repository.state_dir();
    assert_eq!(directory.file_name().unwrap().to_string_lossy().len(), 64);
    assert_eq!(repository.state_json()["artifacts"][0]["mode"], 0o644);
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(directory.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(directory.join("lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
