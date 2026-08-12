use std::fs;

use tempfile::tempdir;
use wombat::{BuildOptions, PlanInspectSection, inspect_plan, plan};

#[test]
fn construction_processes_toml_and_logs_are_tracked_without_raw_output() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::write(source.join("packages.toml"), "[tool]\nname = 'wombat'\n").unwrap();
    fs::write(source.join("src/dot_config"), "value\n").unwrap();
    fs::write(
        source.join("wombat.lua"),
        r#"local w = require('wombat')
local data = w.data.toml('packages.toml')
local direct = w.exec({ 'sh', '-c', 'printf direct; printf err >&2' }, { env = { REMOVE_ME = false } })
assert(direct:check().stdout == 'direct')
local shell = w.shell('printf shell | tr a-z A-Z')
assert(shell.stdout == 'SHELL')
w.log.warn('using test data', { name = data.tool.name })
w.install('.config', { to = '.config/test' })
"#,
    )
    .unwrap();
    let outcome = plan(BuildOptions::new(&source, "build")).unwrap();
    assert_eq!(outcome.plan.process_observations.len(), 2);
    let rendered = inspect_plan(&outcome.plan, PlanInspectSection::Observations);
    assert!(rendered.contains("Process observations"));
    assert!(rendered.contains("sha256:"));
    assert!(!rendered.contains("stdout: direct"));
    assert!(
        outcome
            .plan
            .sources
            .iter()
            .any(|source| source.path == "packages.toml")
    );
}

#[test]
fn process_failures_are_data_until_checked() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::write(source.join("src/dot_config"), "value\n").unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nlocal result = w.exec({ 'sh', '-c', 'exit 7' })\nassert(not result.ok and result.code == 7)\nw.install('.config', { to = '.config/test' })\n",
    )
    .unwrap();
    let outcome = plan(BuildOptions::new(&source, "build")).unwrap();
    assert!(!outcome.plan.process_observations[0].ok);
    assert_eq!(outcome.plan.process_observations[0].code, Some(7));
}
