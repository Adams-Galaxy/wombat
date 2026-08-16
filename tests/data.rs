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
fn json_toml_and_yaml_data_are_read_tracked_and_typed_the_same_way() {
    let temporary = source_with(
        r#"local w = require('wombat')
local json = w.json.decode('packages.json')
local toml = w.toml.decode('packages.toml')
local yaml = w.yaml.decode('packages.yaml')
local empty = w.yaml.decode('empty.yaml')
assert(json.tool.name == 'wombat')
assert(toml.tool.name == 'wombat')
assert(yaml.tool.name == 'wombat')
assert(json.tool.count == 3)
assert(toml.tool.count == 3)
assert(yaml.tool.count == 3)
assert(yaml.tool.answer == 'yes')
assert(yaml.tool.tagged == '123')
assert(empty == w.null)
w.install('.config', { to = '.config/test' })
"#,
        &[
            (
                "packages.json",
                r#"{"tool": {"name": "wombat", "count": 3}}"#,
            ),
            ("packages.toml", "[tool]\nname = 'wombat'\ncount = 3\n"),
            (
                "packages.yaml",
                "tool:\n  name: wombat\n  count: 3\n  answer: yes\n  tagged: !!str 123\n",
            ),
            ("empty.yaml", ""),
        ],
    );
    let outcome = plan(BuildOptions::new(temporary.path().join("source"), "build")).unwrap();
    for path in [
        "packages.json",
        "packages.toml",
        "packages.yaml",
        "empty.yaml",
    ] {
        assert!(
            outcome
                .plan
                .sources
                .iter()
                .any(|source| source.path == path),
            "{path} should be tracked"
        );
    }
    fs::write(
        temporary.path().join("source/packages.yaml"),
        "tool:\n  name: wombat\n  count: 3\n  answer: yes\n  tagged: !!str 123\n  changed: true\n",
    )
    .unwrap();
    let changed = plan(BuildOptions::new(
        temporary.path().join("source"),
        "build-changed",
    ))
    .unwrap();
    assert_ne!(outcome.plan.plan_id, changed.plan.plan_id);
}

#[test]
fn yaml_round_trip_preserves_frozen_shapes_and_has_canonical_output() {
    let temporary = source_with(
        r#"local w = require('wombat')
local decoded = w.yaml.decode('shapes.yaml')
assert(decoded.nothing == w.null)
local context = w.template.context({ paths = w.paths })
local value = {
    zed = 'last',
    answer = 'yes',
    switch = 'on',
    number = '123',
    nullable = 'null',
    empty = '',
    multiline = 'first\nsecond',
    unicode = 'wombat 🐾',
    decoded = decoded,
}
w.generate('data.yaml', { content = w.yaml.encode(value), to = 'data.yaml' })
w.generate('context.yaml', { content = w.yaml.encode(context), to = 'context.yaml' })
w.generate('scalar.yaml', { content = w.yaml.encode(false), to = 'scalar.yaml' })
w.generate('null.yaml', { content = w.yaml.encode(w.null), to = 'null.yaml' })
"#,
        &[(
            "shapes.yaml",
            "list: []\nmap: {}\nnothing: null\nnested:\n  - []\n  - {}\n",
        )],
    );
    let outcome = build(BuildOptions::new(temporary.path().join("source"), "build")).unwrap();
    let encoded = fs::read_to_string(outcome.build_dir.join("tree/data.yaml")).unwrap();

    assert_eq!(
        encoded,
        concat!(
            "answer: \"yes\"\n",
            "decoded:\n",
            "  list: []\n",
            "  map: {}\n",
            "  nested:\n",
            "    - []\n",
            "    - {}\n",
            "  nothing: null\n",
            "empty: \"\"\n",
            "multiline: |-\n",
            "  first\n",
            "  second\n",
            "nullable: \"null\"\n",
            "number: \"123\"\n",
            "switch: \"on\"\n",
            "unicode: wombat 🐾\n",
            "zed: last\n",
        )
    );
    let context = fs::read_to_string(outcome.build_dir.join("tree/context.yaml")).unwrap();
    assert!(context.contains("repository:"), "{context}");
    let round_trip: serde_json::Value = serde_saphyr::from_str(&encoded).unwrap();
    assert_eq!(round_trip["answer"], "yes");
    assert_eq!(round_trip["decoded"]["list"], serde_json::json!([]));
    assert_eq!(round_trip["decoded"]["map"], serde_json::json!({}));
    assert!(round_trip["decoded"]["nothing"].is_null());
    assert_eq!(
        fs::read_to_string(outcome.build_dir.join("tree/scalar.yaml")).unwrap(),
        "false\n"
    );
    assert_eq!(
        fs::read_to_string(outcome.build_dir.join("tree/null.yaml")).unwrap(),
        "null\n"
    );
}

#[test]
fn yaml_decodes_anchors_and_rejects_unrepresentable_semantics() {
    let temporary = source_with(
        r#"local w = require('wombat')
local value = w.yaml.decode('anchors.yaml')
assert(value.first.name == 'wombat')
assert(value.second.name == 'wombat')
"#,
        &[(
            "anchors.yaml",
            "first: &tool\n  name: wombat\nsecond: *tool\n",
        )],
    );
    plan(BuildOptions::new(temporary.path().join("source"), "build")).unwrap();

    for (name, source, expected) in [
        ("duplicate", "key: one\nkey: two\n", "duplicate"),
        (
            "merge",
            "base: &base { key: value }\nitem:\n  <<: *base\n",
            "merge",
        ),
        ("multiple", "---\none\n---\ntwo\n", "document"),
        ("numeric-key", "1: value\n", "string"),
        ("sequence-key", "? [one, two]\n: value\n", "string"),
        ("overflow", "value: 9223372036854775808\n", "signed 64-bit"),
        ("nan", "value: .nan\n", "finite"),
        (
            "custom-tag",
            "value: !rgb '#ff0000'\n",
            "unsupported YAML tag",
        ),
        (
            "timestamp-tag",
            "value: !!timestamp 2026-08-16\n",
            "unsupported YAML tag",
        ),
        (
            "binary-tag",
            "value: !!binary SGVsbG8=\n",
            "unsupported YAML tag",
        ),
        (
            "incompatible-core-tag",
            "value: !!seq scalar\n",
            "tagged scalar",
        ),
        ("recursive-alias", "value: &self [*self]\n", "alias"),
    ] {
        let filename = format!("{name}.yaml");
        let entrypoint = format!("local w = require('wombat')\nw.yaml.decode({filename:?})\n");
        let temporary = source_with(&entrypoint, &[(filename.as_str(), source)]);
        let error = plan(BuildOptions::new(temporary.path().join("source"), "build"))
            .unwrap_err()
            .to_string();
        assert!(
            error.to_lowercase().contains(&expected.to_lowercase()),
            "{name}: expected {expected:?} in {error}"
        );
    }
}

#[test]
fn yaml_alias_replay_is_bounded() {
    let mut yaml = String::from("base: &base [one, two, three]\nvalues:\n");
    for _ in 0..4_097 {
        yaml.push_str("  - *base\n");
    }
    let temporary = source_with(
        "local w = require('wombat')\nw.yaml.decode('aliases.yaml')\n",
        &[("aliases.yaml", &yaml)],
    );
    let error = plan(BuildOptions::new(temporary.path().join("source"), "build"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("alias") && error.contains("4096"), "{error}");
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
        ("w.yaml.decode", "/etc/passwd"),
        ("w.yaml.decode", "../outside.yaml"),
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

    let temporary = source_with(
        "local w = require('wombat')\nw.yaml.decode('broken.yaml')\n",
        &[("broken.yaml", "value: [not closed")],
    );
    let error = plan(BuildOptions::new(temporary.path().join("source"), "build"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("failed to parse YAML data `broken.yaml`") && error.contains("wombat.lua:2"),
        "{error}"
    );
}
