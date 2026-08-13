use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wombat::{
    Architecture, BuildOptions, BuildOutcome, BuildStatus, HostContext, Manifest,
    OperatingSystemName, TargetPlatform, build, verify_build,
};

fn fixture_host() -> HostContext {
    HostContext::fixture(TargetPlatform::minimal(
        OperatingSystemName::Macos,
        Architecture::Aarch64,
    ))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn manifest_json(manifest: &Manifest) -> String {
    serde_json::to_string_pretty(manifest).unwrap()
}

/// `project_identity` digests the repository root, so it is checked for shape
/// rather than pinned. Every other field, including the identities derived from
/// content, is compared exactly.
fn exact_manifest_json(value: &str) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_str(value).unwrap();
    let object = value.as_object_mut().unwrap();
    for key in ["project_identity"] {
        let Some(identity) = object.remove(key) else {
            continue;
        };
        let identity = identity.as_str().unwrap();
        assert!(
            identity.strip_prefix("sha256:").is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
            }),
            "{key} must be a sha256 digest, got {identity}"
        );
    }
    value
}

fn build_at(root: &Path, build_dir: &Path) -> wombat::Result<BuildOutcome> {
    build(BuildOptions::new(root, build_dir).with_host(fixture_host()))
}

fn run_wombat(args: &[&str], current_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(args)
        .current_dir(current_dir)
        .output()
        .unwrap()
}

fn snapshot_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(relative, fs::read(path).unwrap());
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[test]
fn walking_fixture_matches_the_expected_manifest() {
    let root = fixture("walking");
    let temporary = tempfile::tempdir().unwrap();
    let expected = fs::read_to_string(root.join("expected-manifest.json")).unwrap();
    let actual = manifest_json(
        &build_at(&root, &temporary.path().join("build"))
            .unwrap()
            .manifest,
    );

    assert_eq!(exact_manifest_json(&actual), exact_manifest_json(&expected));
}

#[test]
fn repeated_builds_are_byte_identical_and_non_mutating() {
    let root = fixture("walking");
    let temporary = tempfile::tempdir().unwrap();
    let build_dir = temporary.path().join("build");
    let before = snapshot_tree(&root);
    let first_outcome = build_at(&root, &build_dir).unwrap();
    let first = manifest_json(&first_outcome.manifest);
    let second_outcome = build_at(&root, &build_dir).unwrap();
    let second = manifest_json(&second_outcome.manifest);
    let after = snapshot_tree(&root);

    assert_eq!(first, second);
    assert_eq!(first_outcome.status, BuildStatus::Created);
    assert_eq!(second_outcome.status, BuildStatus::Unchanged);
    assert_eq!(before, after);
}

#[test]
fn root_selection_order_preserves_outputs_but_changes_exact_source_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let first = build_at(&fixture("lifecycle-order-a"), &temporary.path().join("a"))
        .unwrap()
        .manifest;
    let second = build_at(&fixture("lifecycle-order-b"), &temporary.path().join("b"))
        .unwrap()
        .manifest;

    assert_ne!(first.build_id, second.build_id);
    assert_ne!(first.sources, second.sources);
    assert_eq!(first.modules, second.modules);
    assert_eq!(first.artifacts, second.artifacts);
    let first = manifest_json(&first);
    assert!(first.contains(r#""name": "helper""#));
    assert!(first.contains(r#""kind": "using""#));
    assert!(first.contains(r#""name": "kanagawa""#));
    assert!(!first.contains("export"));
}

#[test]
fn path_fixture_matches_the_exact_manifest_v17() {
    let root = fixture("paths");
    let temporary = tempfile::tempdir().unwrap();
    let expected = fs::read_to_string(root.join("expected-manifest.json")).unwrap();
    let actual = manifest_json(
        &build_at(&root, &temporary.path().join("build"))
            .unwrap()
            .manifest,
    );

    assert_eq!(exact_manifest_json(&actual), exact_manifest_json(&expected));
}

#[test]
fn directory_fixture_matches_manifest_v17_and_materialised_tree() {
    let root = fixture("directories");
    let temporary = tempfile::tempdir().unwrap();
    let build_dir = temporary.path().join("build");
    let expected = fs::read_to_string(root.join("expected-manifest.json")).unwrap();
    let outcome = build_at(&root, &build_dir).unwrap();

    assert_eq!(
        exact_manifest_json(&manifest_json(&outcome.manifest)),
        exact_manifest_json(&expected)
    );
    assert_eq!(
        fs::read(build_dir.join("tree/.config/app/.hidden")).unwrap(),
        b"hidden\n"
    );
    assert_eq!(
        fs::read(build_dir.join("tree/.local/bin/tool")).unwrap(),
        b"#!/bin/sh\necho wombat\n"
    );
}

#[test]
fn artifact_lua_is_opaque_and_control_helpers_are_isolated() {
    let root = fixture("opaque-lua");
    let temporary = tempfile::tempdir().unwrap();
    let before = snapshot_tree(&root);
    let manifest = build_at(&root, &temporary.path().join("build"))
        .unwrap()
        .manifest;
    let after = snapshot_tree(&root);

    assert_eq!(before, after);
    assert_eq!(manifest.artifacts.len(), 2);
    assert_eq!(manifest.artifacts[0].source, "src/dot_config/nvim/init.lua");
    assert_eq!(
        manifest.artifacts[1].source,
        "src/dot_config/nvim/lua/plugins/example.lua"
    );
    assert!(
        fs::read_to_string(root.join("src/dot_config/nvim/init.lua"))
            .unwrap()
            .contains("must remain opaque")
    );
}

#[test]
fn cli_build_accepts_an_explicit_source_and_build_directory() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = fixture("walking");
    let temporary = tempfile::tempdir().unwrap();
    let build_dir = temporary.path().join("product");
    let output = run_wombat(
        &[
            "--source",
            root.to_str().unwrap(),
            "build",
            "--build-dir",
            build_dir.to_str().unwrap(),
        ],
        repository,
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("created sha256:"), "{stdout}");
    assert!(stdout.contains(build_dir.to_str().unwrap()), "{stdout}");
    assert!(output.stderr.is_empty());
    assert!(verify_build(&build_dir).is_ok());
}

#[test]
fn cli_build_does_not_default_to_the_current_directory() {
    let root = fixture("walking");
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    fs::create_dir(&home).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .arg("build")
        .current_dir(&root)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains(".local/share/wombat"));
}

#[test]
fn cli_exposes_help_version_and_usage_errors() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));

    let help = run_wombat(&["--help"], repository);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("A Lua-powered dotfiles compiler"));
    let help = String::from_utf8_lossy(&help.stdout);
    for command in ["add", "build", "diff", "apply", "deploy"] {
        assert!(help.contains(command), "{help}");
    }

    let apply_help = run_wombat(&["apply", "--help"], repository);
    let apply_help = String::from_utf8_lossy(&apply_help.stdout);
    assert!(
        apply_help.contains("ask, fail, skip, overwrite"),
        "{apply_help}"
    );
    assert!(apply_help.contains("--target-root"), "{apply_help}");

    let version = run_wombat(&["--version"], repository);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "wombat 0.1.0"
    );

    let invalid = run_wombat(&["not-a-command"], repository);
    assert_eq!(invalid.status.code(), Some(2));
}

#[test]
fn cli_color_policy_covers_help_success_and_errors_without_polluting_plain_output() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let always_help = run_wombat(&["--color", "always", "--help"], repository);
    assert!(always_help.status.success());
    assert!(
        always_help
            .stdout
            .windows(2)
            .any(|window| window == b"\x1b[")
    );

    let never_help = run_wombat(&["--color", "never", "--help"], repository);
    assert!(never_help.status.success());
    assert!(
        !never_help
            .stdout
            .windows(2)
            .any(|window| window == b"\x1b[")
    );

    let root = fixture("walking");
    let temporary = tempfile::tempdir().unwrap();
    let build_dir = temporary.path().join("colored");
    let colored = run_wombat(
        &[
            "--color",
            "always",
            "--source",
            root.to_str().unwrap(),
            "build",
            "-B",
            build_dir.to_str().unwrap(),
        ],
        repository,
    );
    assert!(
        colored.status.success(),
        "{}",
        String::from_utf8_lossy(&colored.stderr)
    );
    assert!(colored.stdout.windows(2).any(|window| window == b"\x1b["));

    let error = run_wombat(
        &[
            "--color",
            "always",
            "--source",
            root.to_str().unwrap(),
            "build",
            "-B",
            "/",
        ],
        repository,
    );
    assert_eq!(error.status.code(), Some(1));
    assert!(error.stderr.windows(2).any(|window| window == b"\x1b["));

    let no_color = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "auto", "--help"])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(no_color.status.success());
    assert!(!no_color.stdout.windows(2).any(|window| window == b"\x1b["));
}

#[test]
fn cli_build_failures_use_stderr_and_exit_one() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = fixture("errors/missing-source");
    let temporary = tempfile::tempdir().unwrap();
    let output = run_wombat(
        &[
            "--source",
            root.to_str().unwrap(),
            "build",
            "-B",
            temporary.path().join("build").to_str().unwrap(),
        ],
        repository,
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("does not exist beneath its declaration base")
    );
}

#[test]
fn build_errors_are_precise() {
    let cases = [
        (
            "errors/missing-source",
            "does not exist beneath its declaration base",
        ),
        ("errors/malformed-target", "must not use `~`"),
        ("errors/traversing-target", "without traversal"),
        (
            "errors/traversing-source",
            "invalid artifact source selector",
        ),
        (
            "errors/unsupported-artifact",
            "does not support option `kind`",
        ),
        (
            "errors/missing-install-options",
            "requires an explicit `to`",
        ),
        ("errors/lua-runtime", "deliberate fixture failure"),
        (
            "errors/conflicting-config",
            "wombat.lua:3, wombat.lua:6; conflicting selection at wombat.lua:9",
        ),
        ("errors/missing-provider", "was not selected with use()"),
        (
            "errors/root-using",
            "using() may only be called while evaluating",
        ),
        (
            "errors/root-module-config",
            "module.config() may only be called",
        ),
        (
            "errors/configured-module-use",
            "configuration-bearing use() belongs to root policy",
        ),
        ("errors/use-cycle", "module cycle: a -> b -> a"),
        ("errors/self-cycle", "module cycle: a -> a"),
        ("errors/using-cycle", "module cycle: a -> b -> a"),
        (
            "errors/invalid-config-function",
            "unsupported Lua function value",
        ),
        ("errors/invalid-config-sparse", "sparse Lua arrays"),
        (
            "errors/invalid-config-mixed",
            "contiguous arrays or string-keyed maps",
        ),
        ("errors/invalid-config-cycle", "cyclic Lua tables"),
        ("errors/invalid-config-number", "must be finite"),
        ("errors/invalid-export", "unsupported Lua function value"),
        ("errors/missing-module", "was not found beneath `modules/`"),
        (
            "errors/invalid-module-name",
            "invalid module name `themes.kanagawa`",
        ),
    ];

    for (name, expected) in cases {
        let temporary = tempfile::tempdir().unwrap();
        let error = build_at(&fixture(name), &temporary.path().join("build"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(expected),
            "fixture {name:?} did not contain {expected:?}:\n{error}"
        );
    }
}

#[test]
fn missing_root_and_entrypoint_are_reported() {
    let missing_root = fixture("does-not-exist");
    let temporary = tempfile::tempdir().unwrap();
    assert!(
        build_at(&missing_root, &temporary.path().join("missing"))
            .unwrap_err()
            .to_string()
            .contains("failed to access")
    );

    let no_entrypoint = fixture("errors");
    assert!(
        build_at(&no_entrypoint, &temporary.path().join("no-entrypoint"))
            .unwrap_err()
            .to_string()
            .contains("wombat.lua")
    );
}
