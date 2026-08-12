use std::fs;

use tempfile::tempdir;
use wombat::{BuildOptions, build, ladder};

#[test]
fn materialisation_records_the_fixed_core_ladder() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::write(source.join("src/dot_config"), "value\n").unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.install('.config', { to = '.config/test' })\n",
    )
    .unwrap();
    let built = build(BuildOptions::new(&source, "build")).unwrap();
    let journal = ladder::read(&built.build_dir).unwrap();
    assert_eq!(journal.format_version, 2);
    assert_eq!(journal.plan_id, built.manifest.plan_id);
    assert_eq!(journal.rungs.len(), 8);
    assert_eq!(
        journal.rungs[4],
        (
            ladder::CoreRung::MaterialiseAfter.into(),
            ladder::ExecutionStatus::Succeeded
        )
    );
    assert_eq!(
        journal.rungs[5],
        (
            ladder::CoreRung::DeployBefore.into(),
            ladder::ExecutionStatus::Pending
        )
    );
}

#[test]
fn reopening_a_running_journal_marks_it_interrupted() {
    let temporary = tempdir().unwrap();
    let mut journal =
        ladder::ExecutionJournal::new("plan".to_string(), ladder::CoreRung::MaterialiseAfter);
    journal.set(
        ladder::CoreRung::MaterialiseTasks,
        ladder::ExecutionStatus::Running,
    );
    ladder::write(temporary.path(), &journal).unwrap_err();
    fs::create_dir(temporary.path().join(".wombat")).unwrap();
    ladder::write(temporary.path(), &journal).unwrap();
    let reopened = ladder::read(temporary.path())
        .unwrap()
        .reopen("plan", ladder::CoreRung::MaterialiseAfter);
    assert_eq!(
        reopened
            .rungs
            .iter()
            .find(|(rung, _)| *rung == ladder::CoreRung::MaterialiseTasks)
            .unwrap()
            .1,
        ladder::ExecutionStatus::Interrupted
    );
}

#[test]
fn rungs_normalize_and_compile_only_products_record_skipped_gates() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::write(source.join("src/dot_config"), "value\n").unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.providers({ 'brew' })\nw.need.command('sh', { when = 'materialise.tasks' })\nw.install('.config', { to = '.config/test' })\n",
    )
    .unwrap();
    let built = build(BuildOptions::new(&source, "build").with_compile_only(true)).unwrap();
    assert_eq!(
        built.manifest.execution_mode,
        wombat::manifest::ExecutionMode::CompileOnly
    );
    assert_eq!(
        built.manifest.skipped_requirement_gates,
        ["materialise.tasks"]
    );
}

#[test]
fn obsolete_build_requirement_namespace_is_not_an_alias() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.build.need.command('sh')\n",
    )
    .unwrap();
    assert!(
        build(BuildOptions::new(&source, "build"))
            .unwrap_err()
            .to_string()
            .contains("nil value")
    );
}

#[test]
fn unified_requirements_are_the_only_persisted_requirement_scope() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nassert(type(w.rungs.materialise.tasks) == 'table')\nassert(tostring(w.rungs.materialise.tasks) == 'materialise.tasks')\nlocal ok = pcall(function() w.rungs.materialise.tasks.value = 'changed' end)\nassert(not ok)\nw.providers({ 'brew' })\nw.need.command('sh', { when = w.rungs.materialise.tasks })\n",
    )
    .unwrap();
    let built = build(BuildOptions::new(&source, "build")).unwrap();
    let encoded = serde_json::to_string(&built.manifest).unwrap();
    assert!(encoded.contains("materialise.tasks"));
    assert!(!encoded.contains("build_requirements"));
    assert!(!encoded.contains("build_providers"));
}

#[test]
fn duplicate_requirement_merges_to_its_earliest_deadline() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.providers({ 'brew' })\nw.need.command('sh', { when = w.rungs.materialise.tasks })\nw.need.command('sh')\n",
    )
    .unwrap();
    let plan = wombat::plan(BuildOptions::new(&source, "build")).unwrap();
    assert_eq!(plan.plan.requirements.len(), 1);
    assert_eq!(
        plan.plan.requirements[0].when,
        ladder::CoreRung::MaterialiseBefore
    );
}

#[test]
fn root_build_reuses_a_fresh_matching_product_without_reconstructing() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    let marker = temporary.path().join("marker");
    fs::write(
        source.join("wombat.lua"),
        format!(
            "local w = require('wombat')\nw.exec({{ 'sh', '-c', 'printf x >> {}' }}):check()\n",
            marker.display()
        ),
    )
    .unwrap();
    let first =
        build(BuildOptions::new(&source, "build").with_provider_reconciliation(true)).unwrap();
    let second =
        build(BuildOptions::new(&source, "build").with_provider_reconciliation(true)).unwrap();
    assert_eq!(first.build_id, second.build_id);
    assert_eq!(second.status, wombat::BuildStatus::Reused);
    assert_eq!(fs::read_to_string(marker).unwrap(), "x");
}

#[test]
fn check_style_plan_reuse_does_not_repeat_configuration_processes() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    let marker = temporary.path().join("marker");
    fs::write(
        source.join("wombat.lua"),
        format!(
            "local w=require('wombat')\nw.exec({{ 'sh', '-c', 'printf x >> {}' }}):check()\n",
            marker.display()
        ),
    )
    .unwrap();
    let options = BuildOptions::new(&source, "build");
    let first = wombat::plan_or_reuse(options.clone()).unwrap();
    let second = wombat::plan_or_reuse(options).unwrap();
    assert_eq!(first.plan.plan_id, second.plan.plan_id);
    assert_eq!(fs::read_to_string(marker).unwrap(), "x");
}

#[test]
fn workflow_reuse_false_forces_reconstruction() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    let marker = temporary.path().join("marker");
    fs::write(
        source.join("wombat.toml"),
        "format_version = 3\n[workflow]\nreuse = false\nfreshness = '5m'\n",
    )
    .unwrap();
    fs::write(
        source.join("wombat.lua"),
        format!(
            "local w = require('wombat')\nw.exec({{ 'sh', '-c', 'printf x >> {}' }}):check()\n",
            marker.display()
        ),
    )
    .unwrap();
    build(BuildOptions::new(&source, "build").with_provider_reconciliation(true)).unwrap();
    build(BuildOptions::new(&source, "build").with_provider_reconciliation(true)).unwrap();
    assert_eq!(fs::read_to_string(marker).unwrap(), "xx");
}

#[test]
fn requirement_authorization_is_ephemeral_and_not_journal_state() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.providers({ 'brew' })\nw.need.command('sh')\n",
    )
    .unwrap();
    let plan = wombat::plan(BuildOptions::new(&source, "build")).unwrap();
    let journal = ladder::ExecutionJournal::new(
        plan.plan.plan_id.clone(),
        ladder::CoreRung::MaterialiseAfter,
    );
    ladder::write(&source.join("build"), &journal).unwrap();
    assert!(
        !fs::read_to_string(source.join("build/.wombat/execution-journal.json"))
            .unwrap()
            .contains("approv")
    );
}

#[test]
fn normal_materialisation_refuses_an_incompatible_target_without_requirements() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w=require('wombat')\nw.target('linux/x86_64')\n",
    )
    .unwrap();
    let options = BuildOptions::new(&source, "build").with_host(wombat::HostContext::fixture(
        wombat::TargetPlatform::minimal(
            wombat::OperatingSystemName::Macos,
            wombat::Architecture::Aarch64,
        ),
    ));
    let error = build(options.clone()).unwrap_err().to_string();
    assert!(error.contains("--compile-only"), "{error}");
    let built = build(options.with_compile_only(true)).unwrap();
    assert_eq!(
        built.manifest.execution_mode,
        wombat::manifest::ExecutionMode::CompileOnly
    );
}

#[test]
fn fresh_reuse_reconstructs_when_a_glob_discovery_closure_changes() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("src/dot_config/app")).unwrap();
    fs::write(source.join("src/dot_config/app/one"), "one\n").unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w=require('wombat')\nw.install('.config/app/**')\n",
    )
    .unwrap();
    let options = BuildOptions::new(&source, "build").with_provider_reconciliation(true);
    let first = build(options.clone()).unwrap();
    fs::write(source.join("src/dot_config/app/two"), "two\n").unwrap();
    let second = build(options).unwrap();
    assert_ne!(first.manifest.plan_id, second.manifest.plan_id);
    assert!(second.build_dir.join("tree/.config/app/two").is_file());
}

#[test]
fn journal_records_mode_skipped_gates_build_identity_and_failure_field() {
    let temporary = tempdir().unwrap();
    let mut journal =
        ladder::ExecutionJournal::new("plan".to_string(), ladder::CoreRung::MaterialiseAfter);
    journal.configure(
        wombat::manifest::ExecutionMode::CompileOnly,
        vec!["materialise.before".to_string()],
    );
    journal.build_id = Some(format!("sha256:{}", "0".repeat(64)));
    journal.record_reuse("product");
    let error = wombat::WombatError::configuration("task failed");
    journal.fail(ladder::CoreRung::MaterialiseTasks, &error);
    fs::create_dir(temporary.path().join(".wombat")).unwrap();
    ladder::write(temporary.path(), &journal).unwrap();
    let reopened = ladder::read(temporary.path()).unwrap();
    assert_eq!(
        reopened.execution_mode,
        wombat::manifest::ExecutionMode::CompileOnly
    );
    assert_eq!(reopened.skipped_requirement_gates, ["materialise.before"]);
    assert_eq!(reopened.reuse_decisions, ["product"]);
    assert!(
        reopened
            .last_failure
            .as_deref()
            .unwrap()
            .contains("task failed")
    );
}
