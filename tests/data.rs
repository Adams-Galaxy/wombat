use std::fs;

use tempfile::tempdir;
use wombat::{BuildOptions, build, plan};

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
local json = w.json.decode('packages.json')
local toml = w.toml.decode('packages.toml')
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
fn json_and_toml_encode_preserve_explicit_empty_container_shapes() {
    let temporary = source_with(
        r#"local w = require('wombat')
local decoded = w.json.decode('shapes.json')
local value = { tool = { name = 'wombat', count = 3 }, list = w.array(), map = {} }
w.generate('data.json', { content = w.json.encode(decoded), to = 'data.json' })
w.generate('scalar.json', { content = w.json.encode(false), to = 'scalar.json' })
w.generate('null.json', { content = w.json.encode(w.null), to = 'null.json' })
w.generate('data.toml', { content = w.toml.encode(value), to = 'data.toml' })
"#,
        &[("shapes.json", r#"{"list":[],"map":{},"nothing":null}"#)],
    );
    let outcome = build(BuildOptions::new(temporary.path().join("source"), "build")).unwrap();

    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(outcome.build_dir.join("tree/data.json")).unwrap())
            .unwrap();
    assert_eq!(json["list"], serde_json::json!([]));
    assert_eq!(json["map"], serde_json::json!({}));
    assert!(json["nothing"].is_null());
    assert_eq!(
        fs::read_to_string(outcome.build_dir.join("tree/scalar.json")).unwrap(),
        "false"
    );
    assert_eq!(
        fs::read_to_string(outcome.build_dir.join("tree/null.json")).unwrap(),
        "null"
    );

    let toml: toml::Value =
        toml::from_str(&fs::read_to_string(outcome.build_dir.join("tree/data.toml")).unwrap())
            .unwrap();
    assert_eq!(toml["tool"]["name"].as_str(), Some("wombat"));
    assert_eq!(toml["tool"]["count"].as_integer(), Some(3));
    assert!(toml["list"].as_array().is_some_and(Vec::is_empty));
    assert!(toml["map"].as_table().is_some_and(toml::map::Map::is_empty));
}

#[test]
fn explicit_arrays_reject_sparse_mixed_and_non_table_values() {
    for expression in [
        "w.array({ [2] = 'b' })",
        "w.array({ 'a', named = true })",
        "w.array({ named = true })",
        "w.array('no')",
    ] {
        let temporary = source_with(&format!("local w = require('wombat')\n{expression}\n"), &[]);
        let error = plan(BuildOptions::new(temporary.path().join("source"), "build"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("array") || error.contains("requires a table"),
            "{expression}: {error}"
        );
    }
}

#[test]
fn toml_encode_requires_a_map_root_and_rejects_null_with_its_path() {
    for (expression, expected) in [
        ("w.toml.encode(w.array())", "document root"),
        (
            "w.toml.encode({ nested = { value = w.null } })",
            "root.nested.value",
        ),
    ] {
        let temporary = source_with(&format!("local w = require('wombat')\n{expression}\n"), &[]);
        let error = plan(BuildOptions::new(temporary.path().join("source"), "build"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{expression}: {error}");
    }
}

#[test]
fn data_readers_reject_unsafe_paths_for_either_format() {
    for (function, path) in [
        ("w.json.decode", "/etc/passwd"),
        ("w.json.decode", "../outside.json"),
        ("w.toml.decode", "/etc/passwd"),
        ("w.toml.decode", "../outside.toml"),
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
        "local w = require('wombat')\nw.json.decode('broken.json')\n",
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
        "local w = require('wombat')\nw.toml.decode('broken.toml')\n",
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
