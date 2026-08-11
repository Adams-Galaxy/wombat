use std::fs;
use std::path::Path;
use std::process::Command;

use wombat::{BuildOptions, build};

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn cli_surfaces_consolidated_unallocated_warnings() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    fs::create_dir(&root).unwrap();
    write(
        &root,
        "wombat.lua",
        "local w = require('wombat')\nw.install('.', { allow_empty = true })\n",
    );
    write(&root, "src/unalloc_payload/one", "one\n");
    write(&root, "src/unalloc_payload/two", "two\n");
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "never", "--source"])
        .arg(&root)
        .args(["build", "-B"])
        .arg(temporary.path().join("build"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let warning = String::from_utf8(output.stderr).unwrap();
    assert!(
        warning.contains(
            "warning: artifact selector `.` owned by `<root>` skipped unallocated sources"
        ),
        "{warning}"
    );
    assert!(warning.contains("`unalloc_payload/one`"), "{warning}");
    assert!(warning.contains("`unalloc_payload/two`"), "{warning}");
}

#[test]
fn generic_projection_globs_templates_and_unallocated_policy_share_one_tree() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    let product = temporary.path().join("product");
    fs::create_dir(&root).unwrap();
    write(
        &root,
        "wombat.toml",
        "format_version = 1\n[artifacts]\nunallocated = \"warn\"\n",
    );
    write(
        &root,
        "wombat.lua",
        concat!(
            "local w = require('wombat')\n",
            "w.use('app')\n",
            "w.install('.', { with = { value = 'rendered' }, exclude = { '*.skip', '.config/app/**' } })\n",
            "w.install('missing-*.optional', { allow_empty = true })\n",
            "w.install(w.hidden('.external/secret'), { to = 'private/secret' })\n",
        ),
    );
    write(
        &root,
        "modules/apps/app.lua",
        concat!(
            "local w = require('wombat')\n",
            "w.module.from('.config/app')\n",
            "w.install('settings.toml')\n",
        ),
    );
    write(
        &root,
        "src/dot_config/app/settings.toml",
        "setting = true\n",
    );
    write(&root, "src/dot_zshrc.tmpl", "{{value}}\n");
    write(&root, "src/readme.skip", "ignored\n");
    write(&root, "src/unalloc_payload/file", "not allocated\n");
    write(&root, "src/.external/secret", "hidden\n");

    let outcome = build(BuildOptions::new(&root, &product)).unwrap();
    assert_eq!(
        fs::read_to_string(product.join("tree/.config/app/settings.toml")).unwrap(),
        "setting = true\n"
    );
    assert_eq!(
        fs::read_to_string(product.join("tree/.zshrc")).unwrap(),
        "rendered\n"
    );
    assert_eq!(
        fs::read_to_string(product.join("tree/private/secret")).unwrap(),
        "hidden\n"
    );
    assert!(!product.join("tree/readme.skip").exists());
    assert_eq!(outcome.manifest.artifact_notices.len(), 1);
    assert_eq!(
        outcome.manifest.artifact_notices[0].skipped,
        ["unalloc_payload/file"]
    );
    assert!(outcome.manifest.modules.iter().any(|module| {
        module.name == "app"
            && module
                .source_base
                .as_ref()
                .is_some_and(|base| base.target.as_deref() == Some(".config/app"))
    }));
}

#[test]
fn module_ids_are_recursive_global_and_physical_location_has_no_target_meaning() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "wombat.lua",
        "local w = require('wombat')\nw.use('app')\n",
    );
    write(
        root,
        "modules/organized/deep/app.lua",
        "local w = require('wombat')\nw.module.from('.config')\nw.install('app')\n",
    );
    write(root, "src/dot_config/app", "ok\n");
    build(BuildOptions::new(root, root.join("build"))).unwrap();

    write(root, "modules/elsewhere/app.lua", "return true\n");
    let error = build(BuildOptions::new(root, root.join("other")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicated by filename stem"), "{error}");
}

#[test]
fn module_source_bases_are_single_ordered_and_support_explicit_reattachment() {
    for (body, expected) in [
        (
            "w.module.from('.config')\nw.module.from('.local')\n",
            "more than once",
        ),
        (
            "w.install('.config/app')\nw.module.from('.local')\n",
            "must run before artifact or task declarations",
        ),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        write(
            temporary.path(),
            "wombat.lua",
            "local w = require('wombat')\nw.use('app')\n",
        );
        write(
            temporary.path(),
            "modules/organization/app.lua",
            &format!("local w = require('wombat')\n{body}"),
        );
        fs::create_dir_all(temporary.path().join("src/dot_config")).unwrap();
        fs::create_dir_all(temporary.path().join("src/dot_local")).unwrap();
        write(temporary.path(), "src/dot_config/app", "app\n");
        let error = build(BuildOptions::new(
            temporary.path(),
            temporary.path().join("build"),
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains(expected), "{error}");
    }

    let paired = tempfile::tempdir().unwrap();
    write(
        paired.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.use('app')\n",
    );
    write(
        paired.path(),
        "modules/app.lua",
        "local w = require('wombat')\nw.module.from('@shared', { to = '.config/app' })\nw.install('.')\n",
    );
    write(paired.path(), "src/unalloc_shared/settings", "paired\n");
    build(BuildOptions::new(
        paired.path(),
        paired.path().join("build"),
    ))
    .unwrap();
    assert_eq!(
        fs::read_to_string(paired.path().join("build/tree/.config/app/settings")).unwrap(),
        "paired\n"
    );
}

#[test]
fn legacy_trees_and_unallocated_exact_sources_fail_without_compatibility() {
    let legacy = tempfile::tempdir().unwrap();
    write(legacy.path(), "wombat.lua", "return true\n");
    write(legacy.path(), "dot_config/app", "legacy\n");
    let error = build(BuildOptions::new(
        legacy.path(),
        legacy.path().join("build"),
    ))
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("legacy source tree `dot_config/` is unsupported"),
        "{error}"
    );

    let unallocated = tempfile::tempdir().unwrap();
    write(
        unallocated.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('@secret')\n",
    );
    write(unallocated.path(), "src/unalloc_secret", "secret\n");
    let error = build(BuildOptions::new(
        unallocated.path(),
        unallocated.path().join("build"),
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("requires an explicit `to`"), "{error}");
}

#[test]
fn exact_target_names_resolve_template_sources_and_reject_ambiguity() {
    let resolved = tempfile::tempdir().unwrap();
    write(
        resolved.path(),
        "wombat.lua",
        concat!(
            "local w = require('wombat')\n",
            "w.install('.config/app.toml', { with = { value = 'rendered' } })\n",
        ),
    );
    write(
        resolved.path(),
        "src/dot_config/app.toml.tmpl",
        "value = '{{value}}'\n",
    );

    let outcome = build(BuildOptions::new(
        resolved.path(),
        resolved.path().join("build"),
    ))
    .unwrap();
    assert_eq!(
        fs::read_to_string(resolved.path().join("build/tree/.config/app.toml")).unwrap(),
        "value = 'rendered'\n"
    );
    let artifact = &outcome.manifest.artifacts[0];
    assert!(matches!(
        &artifact.source_origin,
        wombat::manifest::SourceOrigin::Direct { declared, expanded }
            if declared == ".config/app.toml" && expanded == ".config/app.toml.tmpl"
    ));
    assert!(artifact.source.ends_with("src/dot_config/app.toml.tmpl"));

    let ambiguous = tempfile::tempdir().unwrap();
    write(
        ambiguous.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('app.toml')\n",
    );
    write(ambiguous.path(), "src/app.toml", "static\n");
    write(ambiguous.path(), "src/app.toml.tmpl", "template\n");
    let error = build(BuildOptions::new(
        ambiguous.path(),
        ambiguous.path().join("build"),
    ))
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("artifact source `app.toml` is ambiguous"),
        "{error}"
    );
    assert!(error.contains("src/app.toml"), "{error}");
    assert!(error.contains("src/app.toml.tmpl"), "{error}");

    write(
        ambiguous.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('app.toml.tmpl')\n",
    );
    build(BuildOptions::new(
        ambiguous.path(),
        ambiguous.path().join("explicit"),
    ))
    .unwrap();
    assert_eq!(
        fs::read_to_string(ambiguous.path().join("explicit/tree/app.toml")).unwrap(),
        "template\n"
    );
}

#[test]
fn repository_artifact_policy_is_strict_tracked_and_identity_bearing() {
    let repository = tempfile::tempdir().unwrap();
    write(repository.path(), "wombat.lua", "return true\n");
    let first = build(BuildOptions::new(
        repository.path(),
        repository.path().join("first"),
    ))
    .unwrap();

    write(
        repository.path(),
        "wombat.toml",
        "format_version = 1\n[artifacts]\nunallocated = \"warn\"\n",
    );
    let configured = build(BuildOptions::new(
        repository.path(),
        repository.path().join("configured"),
    ))
    .unwrap();
    assert_ne!(first.build_id, configured.build_id);
    assert!(
        configured
            .manifest
            .sources
            .iter()
            .any(|source| source.path == "wombat.toml")
    );

    for contents in [
        "format_version = 2\n",
        "format_version = 1\nunknown = true\n",
        "format_version = 1\n[artifacts]\nunallocated = \"sometimes\"\n",
    ] {
        write(repository.path(), "wombat.toml", contents);
        let error = build(BuildOptions::new(
            repository.path(),
            repository.path().join("invalid"),
        ))
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("repository `wombat.toml`")
                || error.contains("unsupported repository config"),
            "{error}"
        );
    }
}

#[cfg(unix)]
#[test]
fn recursive_module_discovery_rejects_symlinks_and_special_entries() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    let linked = tempfile::tempdir().unwrap();
    write(
        linked.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.use('app')\n",
    );
    write(linked.path(), "real.lua", "return true\n");
    fs::create_dir(linked.path().join("modules")).unwrap();
    symlink("../real.lua", linked.path().join("modules/app.lua")).unwrap();
    let error = build(BuildOptions::new(
        linked.path(),
        linked.path().join("build"),
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("must not be a symbolic link"), "{error}");

    let special = tempfile::tempdir().unwrap();
    write(
        special.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.use('missing')\n",
    );
    fs::create_dir(special.path().join("modules")).unwrap();
    let _listener = UnixListener::bind(special.path().join("modules/agent.socket")).unwrap();
    let error = build(BuildOptions::new(
        special.path(),
        special.path().join("build"),
    ))
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("must be a regular file or directory"),
        "{error}"
    );
}
