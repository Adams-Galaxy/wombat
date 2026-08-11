use std::fs;

use wombat::{BuildOptions, build};

fn write(root: &std::path::Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

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
fn root_relative_targets_reject_home_and_absolute_syntax() {
    for target in ["~/app", "/tmp/app", "../app"] {
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
