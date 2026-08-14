use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Output};

use tempfile::TempDir;

fn debian_host() -> wombat::HostContext {
    wombat::HostContext {
        platform: wombat::TargetPlatform {
            os: wombat::OperatingSystem {
                name: wombat::OperatingSystemName::Linux,
                family: "unix".into(),
                version: None,
                kernel: Some(wombat::Kernel {
                    name: "linux".into(),
                    release: "6.8.0".into(),
                }),
                distribution: Some(wombat::Distribution {
                    id: "ubuntu".into(),
                    id_like: vec!["debian".into()],
                    version: Some(wombat::LooseVersion::parse("24.04")),
                    pretty_name: Some("Ubuntu 24.04 LTS".into()),
                }),
            },
            arch: wombat::Architecture::X86_64,
        },
        hostname: Some("ubuntu-test".into()),
        username: Some("wombat".into()),
        home: Some(PathBuf::from("/home/wombat")),
    }
}

fn macos_host() -> wombat::HostContext {
    wombat::HostContext::fixture(wombat::TargetPlatform::minimal(
        wombat::OperatingSystemName::Macos,
        wombat::Architecture::Aarch64,
    ))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn build_fixture(name: &str) -> (TempDir, wombat::BuildOutcome) {
    let temporary = tempfile::tempdir().unwrap();
    let build_dir = temporary.path().join("build");
    let outcome = wombat::build(
        wombat::BuildOptions::new(fixture(name), &build_dir).with_host(
            wombat::HostContext::fixture(wombat::TargetPlatform::minimal(
                wombat::OperatingSystemName::Macos,
                wombat::Architecture::Aarch64,
            )),
        ),
    )
    .unwrap();
    (temporary, outcome)
}

#[test]
fn built_in_provider_resolves_commands_alternatives_formulae_and_casks() {
    let (_temporary, outcome) = build_fixture("requirements");

    assert_eq!(outcome.manifest.format_version, 18);
    assert_eq!(outcome.manifest.providers.len(), 1);
    assert_eq!(outcome.manifest.requirements.len(), 2);
    let search = &outcome.manifest.requirements[0];
    assert_eq!(search.candidates.len(), 2);
    assert_eq!(search.binding.identity, "formula:ripgrep");
    assert_eq!(search.binding.publications.commands, ["rg"]);
    let editor = &outcome.manifest.requirements[1];
    assert_eq!(editor.binding.identity, "cask:visual-studio-code");
    assert_eq!(editor.binding.publications.commands, ["code"]);
}

#[test]
fn apt_resolves_debian_products_and_freezes_one_update_preparation() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        r#"local w = require("wombat")
w.providers({ { name = "apt", with = { update = true } } })
local search = w.need.command("rg")
w.need.package("zsh", { provider = "apt", publishes = { commands = { "zsh" } } })
assert(search.package == "ripgrep")
"#,
    )
    .unwrap();
    let outcome = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("build")).with_host(debian_host()),
    )
    .unwrap();
    assert_eq!(outcome.manifest.format_version, 18);
    assert_eq!(outcome.manifest.requirements.len(), 2);
    assert_eq!(
        outcome.manifest.requirements[0].binding.package.as_deref(),
        Some("ripgrep")
    );
    assert_eq!(outcome.manifest.preparations.len(), 1);
    let preparation = &outcome.manifest.preparations[0];
    assert_eq!(preparation.provider, "apt");
    assert_eq!(preparation.identity, "update-index");
    assert!(preparation.elevated);
    let inspected = wombat::inspect(&outcome.build_dir, wombat::InspectSection::Providers).unwrap();
    assert!(inspected.contains("Update the Apt package index"));
    let provider = wombat::explain(&outcome.build_dir, "provider:apt", None, None).unwrap();
    let preparation = wombat::explain(
        &outcome.build_dir,
        "preparation:apt:update-index",
        None,
        None,
    )
    .unwrap();
    assert!(provider.contains("update-index"));
    assert!(preparation.contains("elevated: true"));
}

#[test]
fn package_requirement_defaults_to_the_sole_configured_provider() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        r#"local w = require("wombat")
w.providers({ "apt" })
local essential = w.need.package("build-essential")
assert(essential.provider == "apt")
"#,
    )
    .unwrap();
    let outcome = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("build")).with_host(debian_host()),
    )
    .unwrap();
    let requirement = &outcome.manifest.requirements[0];
    assert_eq!(requirement.binding.provider, "apt");
    assert_eq!(
        requirement.binding.package.as_deref(),
        Some("build-essential")
    );
    assert_eq!(requirement.attempts.len(), 1);
}

#[test]
fn unpinned_package_requirement_tries_configured_providers_in_priority_order() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        r#"local w = require("wombat")
w.providers({ "apt", "brew" })
w.need.package("zsh", { publishes = { commands = { "zsh" } } })
"#,
    )
    .unwrap();

    let debian = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("debian"))
            .with_host(debian_host()),
    )
    .unwrap();
    let requirement = &debian.manifest.requirements[0];
    assert_eq!(requirement.binding.provider, "apt");
    assert_eq!(requirement.attempts.len(), 1);

    let macos = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("macos")).with_host(macos_host()),
    )
    .unwrap();
    let requirement = &macos.manifest.requirements[0];
    assert_eq!(requirement.binding.provider, "brew");
    assert_eq!(requirement.attempts.len(), 2);
    assert!(matches!(
        requirement.attempts[0].outcome,
        wombat::manifest::ResolutionOutcome::Unsupported { .. }
    ));
}

#[test]
fn explicit_package_provider_still_pins_selection_and_rejects_unconfigured_names() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        r#"local w = require("wombat")
w.providers({ "apt", "brew" })
w.need.package("zsh", { provider = "apt", publishes = { commands = { "zsh" } } })
"#,
    )
    .unwrap();
    let outcome = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("build")).with_host(debian_host()),
    )
    .unwrap();
    let requirement = &outcome.manifest.requirements[0];
    assert_eq!(requirement.binding.provider, "apt");
    assert_eq!(requirement.attempts.len(), 1);

    fs::write(
        source.join("wombat.lua"),
        r#"local w = require("wombat")
w.providers({ "apt" })
w.need.package("zsh", { provider = "brew", publishes = { commands = { "zsh" } } })
"#,
    )
    .unwrap();
    let error = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("unconfigured"))
            .with_host(debian_host()),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("requests provider `brew`, which is not configured"),
        "{error}"
    );
}

#[test]
fn custom_provider_planning_requires_and_packages_prepare() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("providers")).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.providers({'custom'})\nw.need.package('tool', { provider = 'custom' })\n",
    )
    .unwrap();
    fs::write(
        source.join("providers/custom.lua"),
        r#"local provider = require("wombat.provider")
return provider.define({
  resolve = function(candidate)
    return provider.binding({ identity = candidate.name, package = candidate.name, data = {} })
  end,
  plan = function(bindings)
    return { provider.operation({ identity = "catalog", description = "Prepare catalog", data = { count = #bindings } }) }
  end,
  check = function() return provider.satisfied("fixture") end,
  prepare = function() return true end,
  reconcile = function() return true end,
})
"#,
    )
    .unwrap();
    let outcome = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("build")).with_host(
            wombat::HostContext::fixture(wombat::TargetPlatform::minimal(
                wombat::OperatingSystemName::Macos,
                wombat::Architecture::Aarch64,
            )),
        ),
    )
    .unwrap();
    assert_eq!(outcome.manifest.preparations.len(), 1);
    assert_eq!(outcome.manifest.preparations[0].identity, "catalog");
    fs::write(
        source.join("providers/custom.lua"),
        "local p=require('wombat.provider') return p.define({ resolve=function(c) return p.binding({identity=c.name,data={}}) end, plan=function() return {} end, check=function() return p.satisfied() end, reconcile=function() end })",
    )
    .unwrap();
    let error = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("other")).with_host(
            wombat::HostContext::fixture(wombat::TargetPlatform::minimal(
                wombat::OperatingSystemName::Macos,
                wombat::Architecture::Aarch64,
            )),
        ),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("requires prepare()"), "{error}");

    fs::write(
        source.join("providers/custom.lua"),
        "local p=require('wombat.provider') return p.define({ resolve=function(c) return p.binding({identity=c.name,data={}}) end, plan=function() local op=p.operation({identity='same',description='Same',data={}}) return {op,op} end, check=function() return p.satisfied() end, prepare=function() end, reconcile=function() end })",
    )
    .unwrap();
    let duplicate = wombat::build(
        wombat::BuildOptions::new(&source, temporary.path().join("duplicates"))
            .with_host(macos_host()),
    )
    .unwrap_err()
    .to_string();
    assert!(
        duplicate.contains("duplicate operation `same`"),
        "{duplicate}"
    );
}

#[test]
fn custom_provider_helpers_are_captured_verified_and_relocatable() {
    let (temporary, outcome) = build_fixture("custom-provider");
    let provider = &outcome.manifest.providers[0];
    let wombat::manifest::ProviderOrigin::Custom { files, .. } = &provider.origin else {
        panic!("expected custom provider");
    };
    assert_eq!(
        files
            .iter()
            .map(|file| file.payload.as_str())
            .collect::<Vec<_>>(),
        ["company.lua", "company/naming.lua"]
    );
    assert_eq!(
        outcome.manifest.requirements[0].binding.package.as_deref(),
        Some("company-tool-stable")
    );

    let relocated = temporary.path().join("relocated");
    fs::create_dir(&relocated).unwrap();
    fs::copy(
        outcome.build_dir.join("manifest.json"),
        relocated.join("manifest.json"),
    )
    .unwrap();
    copy_dir(&outcome.build_dir.join("tree"), &relocated.join("tree"));
    copy_dir(
        &outcome.build_dir.join("providers"),
        &relocated.join("providers"),
    );
    wombat::verify_build(&relocated).unwrap();

    fs::write(relocated.join("providers/company/naming.lua"), "tampered").unwrap();
    assert!(wombat::verify_build(&relocated).is_err());
}

fn copy_dir(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &destination.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn cli_check_reports_without_provider_mutation() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let bin = temporary.path().join("bin");
    let state = temporary.path().join("brew-state");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.providers({'brew'})\nw.need.package('hello', { provider = 'brew', publishes = { commands = {'hello'} } })\n",
    )
    .unwrap();
    let brew = bin.join("brew");
    fs::write(
        &brew,
        r#"#!/bin/sh
if [ "$1" = "info" ]; then
  if [ -f "$FAKE_BREW_STATE" ]; then installed='[{"version":"1.0.0"}]'; else installed='[]'; fi
  printf '{"formulae":[{"installed":%s}],"casks":[]}' "$installed"
  exit 0
fi
if [ "$2" = "--dry-run" ]; then exit 0; fi
if [ "$1" = "install" ] || [ "$1" = "upgrade" ]; then : > "$FAKE_BREW_STATE"; exit 0; fi
exit 9
"#,
    )
    .unwrap();
    fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();
    let hello = bin.join("hello");
    fs::write(&hello, "#!/bin/sh\nprintf 'hello\\n'\n").unwrap();
    fs::set_permissions(&hello, fs::Permissions::from_mode(0o755)).unwrap();
    let build_dir = temporary.path().join("build");
    wombat::build(wombat::BuildOptions::new(&source, &build_dir).with_host(macos_host())).unwrap();

    let run = |arguments: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_wombat"))
            .args(arguments)
            .env("PATH", &bin)
            .env("FAKE_BREW_STATE", &state)
            .env("XDG_STATE_HOME", temporary.path().join("state"))
            .output()
            .unwrap()
    };

    let missing = run(&[
        "--source",
        source.to_str().unwrap(),
        "check",
        "-B",
        build_dir.to_str().unwrap(),
    ]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stdout).contains("missing"));
    assert!(!missing.stdout.contains(&0x1b));
    assert!(!state.exists(), "check must not mutate provider state");

    let colored = run(&[
        "--color",
        "always",
        "--source",
        source.to_str().unwrap(),
        "check",
        "-B",
        build_dir.to_str().unwrap(),
    ]);
    assert_eq!(colored.status.code(), Some(1));
    assert!(colored.stdout.contains(&0x1b));

    fs::write(&state, "installed\n").unwrap();

    let satisfied = run(&[
        "--source",
        source.to_str().unwrap(),
        "check",
        "-B",
        build_dir.to_str().unwrap(),
    ]);
    assert!(satisfied.status.success());
    assert!(String::from_utf8_lossy(&satisfied.stdout).contains("satisfied"));
}

#[cfg(target_os = "macos")]
#[test]
fn deploy_deadline_requirement_runs_only_during_the_deploy_segment() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let bin = temporary.path().join("bin");
    let home = temporary.path().join("home");
    let state = temporary.path().join("brew-state");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&home).unwrap();
    fs::write(source.join("src/dot_marker"), "deployed\n").unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w=require('wombat')\nw.providers({'brew'})\nw.need.package('hello', { provider='brew', publishes={commands={'hello'}}, when=w.rungs.deploy.before })\nw.install('.marker')\n",
    )
    .unwrap();
    let brew = bin.join("brew");
    fs::write(
        &brew,
        r#"#!/bin/sh
if [ "$1" = "info" ]; then
  if [ -f "$FAKE_BREW_STATE" ]; then installed='[{"version":"1.0.0"}]'; else installed='[]'; fi
  printf '{"formulae":[{"installed":%s}],"casks":[]}' "$installed"
  exit 0
fi
if [ "$2" = "--dry-run" ]; then exit 0; fi
if [ "$1" = "install" ] || [ "$1" = "upgrade" ]; then : > "$FAKE_BREW_STATE"; exit 0; fi
exit 9
"#,
    )
    .unwrap();
    fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();
    let hello = bin.join("hello");
    fs::write(&hello, "#!/bin/sh\nprintf 'hello\\n'\n").unwrap();
    fs::set_permissions(&hello, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let build_dir = source.join("build");
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_wombat"))
            .args(arguments)
            .env("PATH", &path)
            .env("FAKE_BREW_STATE", &state)
            .env("HOME", &home)
            .env("XDG_STATE_HOME", temporary.path().join("state"))
            .output()
            .unwrap()
    };
    let built = run(&[
        "--source",
        source.to_str().unwrap(),
        "build",
        "-B",
        build_dir.to_str().unwrap(),
        "--yes",
    ]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(
        !state.exists(),
        "deploy deadline ran during materialisation"
    );
    let deployed = run(&[
        "--source",
        source.to_str().unwrap(),
        "plan",
        "deploy",
        "-B",
        build_dir.to_str().unwrap(),
        "--target-root",
        home.to_str().unwrap(),
        "--conflict",
        "fail",
        "--yes",
    ]);
    assert!(
        deployed.status.success(),
        "{}",
        String::from_utf8_lossy(&deployed.stderr)
    );
    assert!(state.is_file());
    assert_eq!(
        fs::read_to_string(home.join(".marker")).unwrap(),
        "deployed\n"
    );
}

#[test]
fn inspection_exposes_provider_and_requirement_semantics_without_lua() {
    let (_temporary, outcome) = build_fixture("requirements");
    let providers = wombat::inspect(&outcome.build_dir, wombat::InspectSection::Providers).unwrap();
    let requirements =
        wombat::inspect(&outcome.build_dir, wombat::InspectSection::Requirements).unwrap();
    let explanation = wombat::explain(
        &outcome.build_dir,
        "command:rg",
        Some(&fixture("requirements")),
        None,
    )
    .unwrap();

    assert!(providers.contains("brew"));
    assert!(requirements.contains("formula:ripgrep"));
    assert!(explanation.contains("attempts:"));
    assert!(explanation.contains("command:rg"));
}

/// `brew info` cold-starts Ruby on every invocation, so checking N brew
/// packages one at a time is the dominant cost of an otherwise-instant no-op
/// `check`/`apply`. Three packages should still cost exactly one `brew info`
/// call, not three.
#[cfg(target_os = "macos")]
#[test]
fn brew_package_checks_are_batched_into_one_info_call() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let bin = temporary.path().join("bin");
    let log = temporary.path().join("brew-calls.log");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.providers({'brew'})\nw.need.package('alpha', { provider = 'brew' })\nw.need.package('beta', { provider = 'brew' })\nw.need.package('gamma', { provider = 'brew' })\n",
    )
    .unwrap();
    let brew = bin.join("brew");
    fs::write(
        &brew,
        r#"#!/bin/sh
echo "$@" >> "$BREW_CALL_LOG"
if [ "$1" = "info" ]; then
  printf '{"formulae":[{"name":"alpha","installed":[{"version":"1.0.0"}]},{"name":"beta","installed":[{"version":"1.0.0"}]},{"name":"gamma","installed":[{"version":"1.0.0"}]}],"casks":[]}'
  exit 0
fi
exit 9
"#,
    )
    .unwrap();
    fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();
    let build_dir = temporary.path().join("build");
    wombat::build(wombat::BuildOptions::new(&source, &build_dir).with_host(macos_host())).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args([
            "--source",
            source.to_str().unwrap(),
            "check",
            "-B",
            build_dir.to_str().unwrap(),
        ])
        .env("PATH", &bin)
        .env("BREW_CALL_LOG", &log)
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("satisfied"), "{stdout}");
    assert!(
        stdout.contains("ms)"),
        "check output should report how long each requirement took: {stdout}"
    );

    let calls = fs::read_to_string(&log).unwrap();
    let info_calls = calls
        .lines()
        .filter(|line| line.starts_with("info"))
        .count();
    assert_eq!(
        info_calls, 1,
        "three brew packages should cost one `brew info` call, not one per package: {calls}"
    );
    assert!(calls.contains("alpha") && calls.contains("beta") && calls.contains("gamma"));
}

/// `--skip-requirements` exists for the common edit-and-rebuild loop, where
/// paying for a package check on every invocation is wasted work. It must
/// skip the check without disabling reuse of an already-fresh product —
/// wiring it straight to `with_provider_reconciliation` would have disabled
/// [`try_reuse_product`]'s fast path too, making the flag slower than doing
/// nothing on the second run.
#[test]
fn skip_requirements_avoids_the_package_check_and_still_reuses_a_fresh_product() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let bin = temporary.path().join("bin");
    let log = temporary.path().join("brew-calls.log");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.providers({'brew'})\nw.need.package('alpha', { provider = 'brew' })\n",
    )
    .unwrap();
    let brew = bin.join("brew");
    fs::write(
        &brew,
        r#"#!/bin/sh
echo "$@" >> "$BREW_CALL_LOG"
if [ "$1" = "info" ]; then
  printf '{"formulae":[{"name":"alpha","installed":[{"version":"1.0.0"}]}],"casks":[]}'
  exit 0
fi
exit 9
"#,
    )
    .unwrap();
    fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();
    let build_dir = temporary.path().join("build");

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_wombat"))
            .arg("--source")
            .arg(&source)
            .args(args)
            .arg("-B")
            .arg(&build_dir)
            .env("PATH", &bin)
            .env("BREW_CALL_LOG", &log)
            .env("XDG_STATE_HOME", temporary.path().join("state"))
            .output()
            .unwrap()
    };
    let info_call_count = |calls: &str| {
        calls
            .lines()
            .filter(|line| line.starts_with("info"))
            .count()
    };

    let first = run(&["build", "--skip-requirements"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        info_call_count(&fs::read_to_string(&log).unwrap_or_default()),
        0,
        "--skip-requirements must not invoke the package manager"
    );

    let second = run(&["build", "--skip-requirements"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after_second = fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        info_call_count(&after_second),
        0,
        "a repeated --skip-requirements build must still not check packages"
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("reused"),
        "skipping requirement checks must not disable reuse of the fresh product: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let journal = wombat::ladder::read(&build_dir).unwrap();
    assert_eq!(journal.skipped_requirement_gates, ["materialise.before"]);
    assert!(journal.actions.iter().any(|action| {
        action.identity == "requirements:check"
            && action.status == wombat::ladder::ExecutionStatus::Skipped
            && action.reason.contains("--skip-requirements")
    }));
    let timeline = run(&["inspect", "timeline"]);
    assert!(timeline.status.success());
    let timeline = String::from_utf8_lossy(&timeline.stdout);
    assert!(
        timeline.contains("requirements:check") && timeline.contains("skipped"),
        "inspection must surface the skipped requirement action: {timeline}"
    );

    let third = run(&["build"]);
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert_eq!(
        info_call_count(&fs::read_to_string(&log).unwrap()),
        1,
        "a normal build must still check requirements once the flag is dropped"
    );
}

/// `prepare_product_deploy_at_authorized` re-verifies its rung's requirements
/// against the live environment at every deploy rung boundary crossed. When
/// authorization already found nothing pending for the whole plan, that
/// re-verification is pure redundant provider-check cost — one `apply` used
/// to pay for a brew check at every one of `deploy.before`/`deploy.apply`/
/// `deploy.after`, not just once at authorization time.
#[cfg(target_os = "macos")]
#[test]
fn a_satisfied_deploy_scoped_package_is_checked_once_not_per_rung() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let bin = temporary.path().join("bin");
    let home = temporary.path().join("home");
    let log = temporary.path().join("brew-calls.log");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&home).unwrap();
    fs::write(source.join("src/dot_marker"), "deployed\n").unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w=require('wombat')\nw.providers({'brew'})\nw.need.package('hello', { provider='brew', publishes={commands={'hello'}}, when=w.rungs.deploy.before })\nw.install('.marker')\n",
    )
    .unwrap();
    let brew = bin.join("brew");
    fs::write(
        &brew,
        r#"#!/bin/sh
echo "$@" >> "$BREW_CALL_LOG"
if [ "$1" = "info" ]; then
  printf '{"formulae":[{"name":"hello","installed":[{"version":"1.0.0"}]}],"casks":[]}'
  exit 0
fi
exit 9
"#,
    )
    .unwrap();
    fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();
    let hello = bin.join("hello");
    fs::write(&hello, "#!/bin/sh\nprintf 'hello\\n'\n").unwrap();
    fs::set_permissions(&hello, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let build_dir = source.join("build");

    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args([
            "--source",
            source.to_str().unwrap(),
            "apply",
            "-B",
            build_dir.to_str().unwrap(),
            "--target-root",
            home.to_str().unwrap(),
            "--conflict",
            "fail",
            "--yes",
        ])
        .env("PATH", &path)
        .env("BREW_CALL_LOG", &log)
        .env("HOME", &home)
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(home.join(".marker")).unwrap(),
        "deployed\n"
    );

    let calls = fs::read_to_string(&log).unwrap();
    let info_calls = calls
        .lines()
        .filter(|line| line.starts_with("info"))
        .count();
    assert_eq!(
        info_calls, 1,
        "a package satisfied for the whole plan should cost one `brew info` call \
         across build and deploy, not one per rung boundary crossed: {calls}"
    );
}
