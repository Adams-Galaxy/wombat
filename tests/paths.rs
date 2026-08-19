use std::fs;

use wombat::{BuildOptions, build};

mod support;
use support::write;

#[test]
fn target_framed_sources_and_composable_metadata_are_generic() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        concat!(
            "local w = require('wombat')\n",
            "w.install('.config/app')\n",
            "w.install('literal_dot_name', { to = 'literal' })\n",
            "w.install('unalloc_dot_secret', { to = 'private/secret' })\n",
        ),
    );
    write(temp.path(), "src/dot_config/app", "app\n");
    write(temp.path(), "src/literal_dot_name", "literal\n");
    write(temp.path(), "src/unalloc_dot_secret", "secret\n");
    let output = build(BuildOptions::new(temp.path(), temp.path().join("build"))).unwrap();
    let targets = output
        .manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.target.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(targets, [".config/app", "literal", "private/secret"]);
}

#[test]
fn literal_dot_sources_are_invisible_without_hidden_escape() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('.hidden', { to = 'ordinary' })\n",
    );
    write(temp.path(), "src/.hidden", "hidden\n");
    let error = build(BuildOptions::new(temp.path(), temp.path().join("bad")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not exist"), "{error}");
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install(w.hidden('.hidden'), { to = 'explicit' })\n",
    );
    build(BuildOptions::new(temp.path(), temp.path().join("good"))).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("good/tree/explicit")).unwrap(),
        "hidden\n"
    );
}

#[test]
fn targets_reject_shell_expansion_and_traversal() {
    for target in [
        "~/app",
        "%USERPROFILE%/app",
        "../app",
        "C:\\\\Users\\\\adam\\\\app",
    ] {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "wombat.lua",
            &format!("local w = require('wombat')\nw.install('file', {{ to = {target:?} }})\n"),
        );
        write(temp.path(), "src/file", "value\n");
        assert!(
            build(BuildOptions::new(temp.path(), temp.path().join("build"))).is_err(),
            "{target}"
        );
    }
}

#[test]
fn direct_files_can_rename_to_external_absolute_targets() {
    let temp = tempfile::tempdir().unwrap();
    let external = temp.path().join("windows-home/.wezterm.lua");
    write(
        temp.path(),
        "wombat.lua",
        &format!(
            "local w = require('wombat')\nw.install('wezterm.lua', {{ to = {:?} }})\n",
            external
        ),
    );
    write(temp.path(), "src/wezterm.lua", "return {}\n");
    let output = build(BuildOptions::new(temp.path(), temp.path().join("build"))).unwrap();
    let artifact = &output.manifest.artifacts[0];
    assert_eq!(artifact.target.path, external.to_string_lossy());
    assert_eq!(
        artifact.target.scope,
        wombat::manifest::TargetScope::Absolute
    );
    let payloads = fs::read_dir(temp.path().join("build/tree/external"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(
        fs::read_to_string(payloads[0].path()).unwrap(),
        "return {}\n"
    );
}

#[test]
fn external_directory_targets_preserve_selected_relative_paths() {
    let temp = tempfile::tempdir().unwrap();
    let external = temp.path().join("windows-home/config");
    write(
        temp.path(),
        "wombat.lua",
        &format!(
            "local w = require('wombat')\nw.install('wezterm', {{ to = {:?} }})\n",
            external
        ),
    );
    write(temp.path(), "src/wezterm/colors/theme.lua", "theme\n");
    let output = build(BuildOptions::new(temp.path(), temp.path().join("build"))).unwrap();
    let artifact = &output.manifest.artifacts[0];
    assert_eq!(
        artifact.target.path,
        external.join("colors/theme.lua").to_string_lossy()
    );
    assert_eq!(
        artifact.target.scope,
        wombat::manifest::TargetScope::Absolute
    );
}

#[test]
fn external_targets_are_refused_for_compile_only_products() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        concat!(
            "local w = require('wombat')\n",
            "w.target('linux/x86_64')\n",
            "w.install('file', { to = '/tmp/wombat-external-file' })\n",
        ),
    );
    write(temp.path(), "src/file", "value\n");
    let error =
        build(BuildOptions::new(temp.path(), temp.path().join("build")).with_compile_only(true))
            .unwrap_err()
            .to_string();
    assert!(error.contains("compile-only"), "{error}");
    assert!(error.contains("external target"), "{error}");
}

#[cfg(unix)]
#[test]
fn source_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        "wombat.lua",
        "local w = require('wombat')\nw.install('link')\n",
    );
    write(temp.path(), "outside", "value\n");
    fs::create_dir(temp.path().join("src")).unwrap();
    symlink(temp.path().join("outside"), temp.path().join("src/link")).unwrap();
    assert!(build(BuildOptions::new(temp.path(), temp.path().join("build"))).is_err());
}
