use std::fs;

use wombat::{BuildOptions, build};

fn write(root: &std::path::Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn grouped_installs_mix_static_and_templates_with_shared_context_and_exclusions() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('.config/app', { with = { name = 'wombat' }, exclude = '*.skip' })\n",
    );
    write(temp.path(), "src/dot_config/app/static.toml", "static\n");
    write(
        temp.path(),
        "src/dot_config/app/dynamic.toml.tmpl",
        "name={{name}}\n",
    );
    write(temp.path(), "src/dot_config/app/ignored.skip", "ignored\n");
    build(BuildOptions::new(temp.path(), temp.path().join("build"))).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("build/tree/.config/app/static.toml")).unwrap(),
        "static\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("build/tree/.config/app/dynamic.toml")).unwrap(),
        "name=wombat\n"
    );
    assert!(
        !temp
            .path()
            .join("build/tree/.config/app/ignored.skip")
            .exists()
    );
}

#[test]
fn empty_set_selectors_are_opt_in() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('missing-*')\n",
    );
    fs::create_dir(temp.path().join("src")).unwrap();
    assert!(build(BuildOptions::new(temp.path(), temp.path().join("bad"))).is_err());
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('missing-*', { allow_empty = true })\n",
    );
    assert_eq!(
        build(BuildOptions::new(temp.path(), temp.path().join("good")))
            .unwrap()
            .artifact_count,
        0
    );
}

#[test]
fn glob_component_depth_basename_and_question_semantics_are_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        concat!(
            "local w = require('wombat')\n",
            "w.install('a/*.toml', { to = 'single' })\n",
            "w.install('b/**/*.toml', { to = 'deep' })\n",
            "w.install('c/app?.toml', { to = 'question' })\n",
            "w.install('*.yaml', { to = 'basename' })\n",
        ),
    );
    write(temp.path(), "src/a/root.toml", "a-root\n");
    write(temp.path(), "src/a/nested/deep.toml", "a-deep\n");
    write(temp.path(), "src/b/root.toml", "b-root\n");
    write(temp.path(), "src/b/nested/deep.toml", "b-deep\n");
    write(temp.path(), "src/c/app1.toml", "one\n");
    write(temp.path(), "src/c/app12.toml", "twelve\n");
    write(temp.path(), "src/d/e/value.yaml", "yaml\n");

    let outcome = build(BuildOptions::new(temp.path(), temp.path().join("build"))).unwrap();
    let targets = outcome
        .manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.target.path.as_str())
        .collect::<Vec<_>>();
    assert!(targets.contains(&"single/root.toml"));
    assert!(!targets.contains(&"single/nested/deep.toml"));
    assert!(targets.contains(&"deep/root.toml"));
    assert!(targets.contains(&"deep/nested/deep.toml"));
    assert!(targets.contains(&"question/app1.toml"));
    assert!(!targets.contains(&"question/app12.toml"));
    assert!(targets.contains(&"basename/d/e/value.yaml"));
    assert!(
        outcome
            .manifest
            .artifact_selections
            .iter()
            .all(|selection| selection.matches.windows(2).all(|pair| pair[0] < pair[1]))
    );
}

#[test]
fn explicit_group_roots_reattach_once_but_nested_unallocated_children_still_sever() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('@bundle', { to = 'bundle' })\n",
    );
    write(temp.path(), "src/unalloc_bundle/kept", "kept\n");
    write(
        temp.path(),
        "src/unalloc_bundle/unalloc_nested/skipped",
        "skipped\n",
    );
    let outcome = build(BuildOptions::new(temp.path(), temp.path().join("build"))).unwrap();
    assert!(temp.path().join("build/tree/bundle/kept").is_file());
    assert!(
        !temp
            .path()
            .join("build/tree/bundle/unalloc_nested/skipped")
            .exists()
    );
    assert_eq!(outcome.manifest.artifact_notices.len(), 1);
}

#[test]
fn literal_dot_descendants_are_ignored_by_ordinary_group_selection() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('.')\n",
    );
    write(temp.path(), "src/visible", "yes\n");
    write(temp.path(), "src/.private/hidden", "no\n");
    build(BuildOptions::new(temp.path(), temp.path().join("build"))).unwrap();
    assert!(temp.path().join("build/tree/visible").is_file());
    assert!(!temp.path().join("build/tree/.private/hidden").exists());
}

#[test]
fn unallocated_policy_ignore_warn_and_error_are_distinct_after_exclusions() {
    for (policy, notices, succeeds) in [("ignore", 0, true), ("warn", 1, true), ("error", 0, false)]
    {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "wombat.toml",
            &format!("format_version = 2\n[artifacts]\nunallocated = \"{policy}\"\n"),
        );
        write(
            temp.path(),
            "wombat.lua",
            "local w = require('wombat')\nw.install('.', { allow_empty = true })\n",
        );
        write(temp.path(), "src/unalloc_payload/file", "skipped\n");
        let result = build(BuildOptions::new(temp.path(), temp.path().join("build")));
        if succeeds {
            let outcome = result.unwrap();
            assert_eq!(outcome.manifest.artifact_notices.len(), notices);
            assert!(outcome.manifest.artifacts.is_empty());
            assert_eq!(outcome.manifest.artifact_selections.len(), 1);
            assert_eq!(
                outcome.manifest.artifact_selections[0].skipped_unallocated,
                ["unalloc_payload/file"]
            );
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("contains unallocated children"), "{error}");
        }
    }

    let excluded = tempfile::tempdir().unwrap();
    write(
        excluded.path(),
        "wombat.toml",
        "format_version = 2\n[artifacts]\nunallocated = \"error\"\n",
    );
    write(
        excluded.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('.', { exclude = { 'unalloc_payload/**' }, allow_empty = true })\n",
    );
    write(excluded.path(), "src/unalloc_payload/file", "excluded\n");
    build(BuildOptions::new(
        excluded.path(),
        excluded.path().join("build"),
    ))
    .unwrap();
}

#[cfg(unix)]
#[test]
fn hidden_and_excluded_subtrees_are_not_traversed_but_visible_symlinks_fail() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('.', { exclude = { 'excluded/**' } })\n",
    );
    write(temp.path(), "src/visible", "ok\n");
    fs::create_dir_all(temp.path().join("src/.hidden")).unwrap();
    fs::create_dir_all(temp.path().join("src/excluded")).unwrap();
    symlink("missing", temp.path().join("src/.hidden/link")).unwrap();
    symlink("missing", temp.path().join("src/excluded/link")).unwrap();
    build(BuildOptions::new(temp.path(), temp.path().join("build"))).unwrap();

    symlink("missing", temp.path().join("src/visible-link")).unwrap();
    let error = build(BuildOptions::new(temp.path(), temp.path().join("other")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("must not be a symbolic link"), "{error}");
}
