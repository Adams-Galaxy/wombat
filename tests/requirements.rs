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

    assert_eq!(outcome.manifest.format_version, 15);
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
    assert_eq!(outcome.manifest.format_version, 15);
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
