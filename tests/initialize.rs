use std::fs;
use std::process::Command;

use wombat::{BuildOptions, InitStatus, build, initialize};

#[test]
fn initializes_the_minimal_buildable_repository_and_is_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");

    let first = initialize(&root).unwrap();
    assert_eq!(first.status, InitStatus::Initialized);
    assert_eq!(
        fs::read_to_string(root.join("wombat.lua")).unwrap(),
        "local w = require(\"wombat\")\n\nw.use(\"auto\")\n"
    );
    assert!(
        fs::read_to_string(root.join("modules/auto.lua"))
            .unwrap()
            .contains("-- wombat:add begin\n-- wombat:add end")
    );
    assert_eq!(
        fs::read_to_string(root.join(".gitignore")).unwrap(),
        "/build/\n"
    );
    assert!(build(BuildOptions::new(&root, "build")).is_ok());

    let second = initialize(&root).unwrap();
    assert_eq!(second.status, InitStatus::AlreadyInitialized);
}

#[test]
fn permits_unrelated_files_and_leaves_an_existing_gitignore_untouched() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("README.md"), "hello\n").unwrap();
    fs::write(root.join(".gitignore"), "/target/\n").unwrap();

    let outcome = initialize(&root).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        fs::read_to_string(root.join(".gitignore")).unwrap(),
        "/target/\n"
    );
    assert!(outcome.warning.unwrap().contains("/build/"));
}

#[test]
fn conflicting_reserved_paths_fail_before_any_scaffold_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("wombat.lua"), "-- handwritten\n").unwrap();

    let error = initialize(&root).unwrap_err().to_string();
    assert!(error.contains("will not overwrite"));
    assert!(!root.join("modules").exists());
    assert!(!root.join(".gitignore").exists());
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_reserved_paths_without_mutation() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");
    let elsewhere = temporary.path().join("elsewhere");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, root.join("modules")).unwrap();

    let error = initialize(&root).unwrap_err().to_string();
    assert!(error.contains("non-symlink directory"));
    assert!(!root.join("wombat.lua").exists());
}

#[test]
fn cli_defaults_to_the_wombat_data_directory_without_using_cwd() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let cwd = temporary.path().join("cwd");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&cwd).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .arg("init")
        .current_dir(&cwd)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.join(".local/share/wombat/wombat.lua").is_file());
    assert!(!cwd.join("wombat.lua").exists());
}
