use std::fs;

use wombat::{AddMethod, AddStatus, BuildOptions, add, build, initialize};

#[test]
fn add_maps_a_strict_target_descendant_into_src_and_auto_lua() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let target = temp.path().join("target");
    fs::create_dir_all(target.join(".config")).unwrap();
    fs::write(target.join(".config/starship.toml"), "format = 'cube'\n").unwrap();
    initialize(&repo).unwrap();
    fs::create_dir_all(repo.join("src/dot_config")).unwrap();

    let outcome = add(&repo, &target, &target.join(".config/starship.toml")).unwrap();
    assert_eq!(outcome.source, "src/dot_config/starship.toml");
    assert_eq!(outcome.status, AddStatus::Added);
    assert!(
        fs::read_to_string(repo.join("modules/auto.lua"))
            .unwrap()
            .contains("w.install(\".config/starship.toml\")")
    );
    build(BuildOptions::new(&repo, repo.join("build"))).unwrap();
    assert_eq!(
        fs::read_to_string(repo.join("build/tree/.config/starship.toml")).unwrap(),
        "format = 'cube'\n"
    );

    assert_eq!(
        add(&repo, &target, &target.join(".config/starship.toml"))
            .unwrap()
            .status,
        AddStatus::AlreadyPresent
    );
}

#[test]
fn add_reuses_one_glob_coverage_but_exclusions_and_ambiguity_are_conservative() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let target = temp.path().join("target");
    fs::create_dir_all(target.join(".config")).unwrap();
    fs::write(target.join(".config/app.toml"), "value = true\n").unwrap();
    initialize(&repo).unwrap();
    fs::create_dir_all(repo.join("src/dot_config")).unwrap();
    fs::write(
        repo.join("wombat.lua"),
        "local w = require('wombat')\nw.use('auto')\nw.use('bulk')\n",
    )
    .unwrap();
    fs::write(
        repo.join("modules/bulk.lua"),
        "local w = require('wombat')\nw.module.from('.config')\nw.install('**/*.toml', { allow_empty = true })\n",
    )
    .unwrap();

    let outcome = add(&repo, &target, &target.join(".config/app.toml")).unwrap();
    assert_eq!(
        outcome.method,
        AddMethod::Directory {
            owner: "bulk".to_string(),
            declared_source: "**/*.toml".to_string(),
        }
    );
    assert!(
        !fs::read_to_string(repo.join("modules/auto.lua"))
            .unwrap()
            .contains("app.toml")
    );

    fs::write(target.join(".config/ignored.toml"), "ignored = true\n").unwrap();
    fs::write(
        repo.join("modules/bulk.lua"),
        "local w = require('wombat')\nw.module.from('.config')\nw.install('**/*.toml', { exclude = { 'ignored.toml' }, allow_empty = true })\n",
    )
    .unwrap();
    let excluded = add(&repo, &target, &target.join(".config/ignored.toml")).unwrap();
    assert_eq!(excluded.method, AddMethod::GeneratedAuto);

    fs::write(target.join(".config/ambiguous.toml"), "ambiguous = true\n").unwrap();
    fs::write(
        repo.join("wombat.lua"),
        "local w = require('wombat')\nw.use('auto')\nw.use('bulk')\nw.use('other')\n",
    )
    .unwrap();
    fs::write(
        repo.join("modules/other.lua"),
        "local w = require('wombat')\nw.module.from('.config')\nw.install('ambiguous*.toml', { allow_empty = true })\n",
    )
    .unwrap();
    let error = add(&repo, &target, &target.join(".config/ambiguous.toml"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("ambiguous selection coverage"), "{error}");
    assert!(!repo.join("src/dot_config/ambiguous.toml").exists());
}

#[test]
fn add_escapes_metadata_like_target_names_and_rolls_back_failed_auto_updates() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let target = temp.path().join("target");
    fs::create_dir_all(target.join("dot_config")).unwrap();
    fs::write(target.join("dot_config/@file"), "literal\n").unwrap();
    initialize(&repo).unwrap();

    let outcome = add(&repo, &target, &target.join("dot_config/@file")).unwrap();
    assert_eq!(outcome.source, "src/literal_dot_config/literal_@file");
    assert!(
        fs::read_to_string(repo.join("modules/auto.lua"))
            .unwrap()
            .contains("w.install(\"dot_config/@file\")")
    );

    fs::write(target.join("second"), "second\n").unwrap();
    fs::write(
        repo.join("modules/auto.lua"),
        "local w = require('wombat')\n-- generated region removed\n",
    )
    .unwrap();
    let error = add(&repo, &target, &target.join("second"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cannot update `modules/auto.lua`"),
        "{error}"
    );
    assert!(!repo.join("src/second").exists());
}

#[test]
fn add_rejects_the_root_parents_symlinks_and_divergent_existing_sources() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let target = temp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    initialize(&repo).unwrap();
    assert!(add(&repo, &target, &target).is_err());
    assert!(add(&repo, &target, temp.path()).is_err());

    fs::write(target.join("file"), "one\n").unwrap();
    add(&repo, &target, &target.join("file")).unwrap();
    fs::write(target.join("file"), "two\n").unwrap();
    assert!(add(&repo, &target, &target.join("file")).is_err());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(target.join("file"), target.join("link")).unwrap();
        assert!(add(&repo, &target, &target.join("link")).is_err());
    }
}
