use std::fs;

use tempfile::tempdir;
use wombat::{BuildOptions, plan};

fn source_with(entrypoint: &str, extra: &[(&str, &str)]) -> tempfile::TempDir {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::write(source.join("src/dot_config"), "value\n").unwrap();
    fs::write(source.join("wombat.lua"), entrypoint).unwrap();
    for (path, contents) in extra {
        fs::write(source.join(path), contents).unwrap();
    }
    temporary
}

#[test]
fn json_and_toml_data_are_both_read_tracked_and_typed_the_same_way() {
    let temporary = source_with(
        r#"local w = require('wombat')
local json = w.data.json('packages.json')
local toml = w.data.toml('packages.toml')
assert(json.tool.name == 'wombat')
assert(toml.tool.name == 'wombat')
assert(json.tool.count == 3)
assert(toml.tool.count == 3)
w.install('.config', { to = '.config/test' })
"#,
        &[
            (
                "packages.json",
                r#"{"tool": {"name": "wombat", "count": 3}}"#,
            ),
            ("packages.toml", "[tool]\nname = 'wombat'\ncount = 3\n"),
        ],
    );
    let outcome = plan(BuildOptions::new(temporary.path().join("source"), "build")).unwrap();
    for path in ["packages.json", "packages.toml"] {
        assert!(
            outcome
                .plan
                .sources
                .iter()
                .any(|source| source.path == path),
            "{path} should be tracked"
        );
    }
}

#[test]
fn data_readers_reject_unsafe_paths_for_either_format() {
    for (function, path) in [
        ("w.data.json", "/etc/passwd"),
        ("w.data.json", "../outside.json"),
        ("w.data.toml", "/etc/passwd"),
        ("w.data.toml", "../outside.toml"),
    ] {
        let temporary = source_with(
            &format!("local w = require('wombat')\n{function}({path:?})\n"),
            &[],
        );
        let error = plan(BuildOptions::new(temporary.path().join("source"), "build"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("requires a safe repository-relative path"),
            "{function}({path}): {error}"
        );
    }
}

#[test]
fn data_readers_report_the_source_and_reason_for_a_parse_failure() {
    let temporary = source_with(
        "local w = require('wombat')\nw.data.json('broken.json')\n",
        &[("broken.json", "{ not json")],
    );
    let error = plan(BuildOptions::new(temporary.path().join("source"), "build"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("failed to parse JSON data `broken.json`"),
        "{error}"
    );

    let temporary = source_with(
        "local w = require('wombat')\nw.data.toml('broken.toml')\n",
        &[("broken.toml", "not = = toml")],
    );
    let error = plan(BuildOptions::new(temporary.path().join("source"), "build"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("failed to parse TOML data `broken.toml`"),
        "{error}"
    );
}
