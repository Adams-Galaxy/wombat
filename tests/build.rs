use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wombat::{Manifest, build};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn manifest_json(manifest: &Manifest) -> String {
    serde_json::to_string_pretty(manifest).unwrap()
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
    let expected = fs::read_to_string(root.join("expected-manifest.json")).unwrap();
    let actual = manifest_json(&build(&root).unwrap());

    assert_eq!(actual, expected.trim_end());
}

#[test]
fn repeated_builds_are_byte_identical_and_non_mutating() {
    let root = fixture("walking");
    let before = snapshot_tree(&root);
    let first = manifest_json(&build(&root).unwrap());
    let second = manifest_json(&build(&root).unwrap());
    let after = snapshot_tree(&root);

    assert_eq!(first, second);
    assert_eq!(before, after);
}

#[test]
fn root_selection_order_does_not_change_the_manifest() {
    let first = manifest_json(&build(&fixture("lifecycle-order-a")).unwrap());
    let second = manifest_json(&build(&fixture("lifecycle-order-b")).unwrap());

    assert_eq!(first, second);
    assert!(first.contains(r#""name": "helper""#));
    assert!(first.contains(r#""kind": "using""#));
    assert!(first.contains(r#""name": "kanagawa""#));
    assert!(!first.contains("export"));
}

#[test]
fn cli_build_accepts_an_explicit_root() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = fixture("walking");
    let output = run_wombat(&["build", root.to_str().unwrap()], repository);
    let expected = fs::read_to_string(root.join("expected-manifest.json")).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    assert!(output.stderr.is_empty());
}

#[test]
fn cli_build_defaults_to_the_current_directory() {
    let root = fixture("walking");
    let output = run_wombat(&["build"], &root);
    let expected = fs::read_to_string(root.join("expected-manifest.json")).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
}

#[test]
fn cli_exposes_help_version_and_usage_errors() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));

    let help = run_wombat(&["--help"], repository);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("A Lua-powered dotfiles compiler"));

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
fn cli_build_failures_use_stderr_and_exit_one() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = fixture("errors/missing-source");
    let output = run_wombat(&["build", root.to_str().unwrap()], repository);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not exist or is not a file"));
}

#[test]
fn build_errors_are_precise() {
    let cases = [
        ("errors/missing-source", "does not exist or is not a file"),
        ("errors/malformed-target", "must begin with `~/`"),
        ("errors/traversing-target", "must not be empty, traverse"),
        (
            "errors/traversing-source",
            "relative path without traversal",
        ),
        (
            "errors/unsupported-artifact",
            "does not support option `kind`",
        ),
        (
            "errors/missing-install-options",
            "requires an options table",
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
        ("errors/missing-module", "does-not-exist/init.lua"),
        (
            "errors/invalid-module-name",
            "invalid module name `themes.kanagawa`",
        ),
    ];

    for (name, expected) in cases {
        let error = build(&fixture(name)).unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "fixture {name:?} did not contain {expected:?}:\n{error}"
        );
    }
}

#[test]
fn missing_root_and_entrypoint_are_reported() {
    let missing_root = fixture("does-not-exist");
    assert!(
        build(&missing_root)
            .unwrap_err()
            .to_string()
            .contains("failed to access")
    );

    let no_entrypoint = fixture("errors");
    assert!(
        build(&no_entrypoint)
            .unwrap_err()
            .to_string()
            .contains("wombat.lua")
    );
}
