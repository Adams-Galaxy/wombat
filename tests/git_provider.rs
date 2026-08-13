use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn head(checkout: &Path) -> String {
    let output = run(Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "HEAD"]));
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A bare "remote" with one commit, for cloning from in tests. Returns the
/// remote path and the checkout used to build it, so callers can add further
/// commits and tags before the test clones from it.
fn bare_remote(root: &Path) -> (PathBuf, PathBuf) {
    let checkout = root.join("plugin-checkout");
    let remote = root.join("plugin.git");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(checkout.join("plugin.sh"), "echo one\n").unwrap();
    run(Command::new("git")
        .args(["init", "-b", "main"])
        .arg(&checkout));
    run(Command::new("git").arg("-C").arg(&checkout).args([
        "config",
        "user.email",
        "tests@example.invalid",
    ]));
    run(Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["config", "user.name", "Wombat Tests"]));
    run(Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["add", "."]));
    run(Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["commit", "-m", "initial"]));
    run(Command::new("git")
        .arg("clone")
        .arg("--bare")
        .arg(&checkout)
        .arg(&remote));
    (remote, checkout)
}

fn push(checkout: &Path, remote: &Path) {
    run(Command::new("git")
        .arg("-C")
        .arg(checkout)
        .arg("push")
        .arg(remote)
        .args(["main", "--tags"]));
}

/// Each test gets its own `XDG_STATE_HOME`, so the process-wide environment
/// lock in `~/.local/state/wombat` doesn't serialize unrelated tests running
/// in parallel against each other.
fn wombat(root: &Path, source: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wombat"))
        .arg("--source")
        .arg(source)
        .args(arguments)
        .env("XDG_STATE_HOME", root.join("state"))
        .output()
        .unwrap()
}

#[test]
fn git_package_clones_without_an_explicit_provider_and_rechecks_satisfied() {
    let temporary = tempfile::tempdir().unwrap();
    let (remote, _checkout) = bare_remote(temporary.path());
    let destination = temporary.path().join("installed/plugin");
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        format!(
            "local w = require('wombat')\nw.providers({{ 'git' }})\nw.need.package('plugin', {{ with = {{ repository = {:?}, to = {:?} }} }})\n",
            remote.to_str().unwrap(),
            destination.to_str().unwrap(),
        ),
    )
    .unwrap();

    let build = wombat(temporary.path(), &source, &["build", "--yes"]);
    assert!(
        build.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert_eq!(
        fs::read_to_string(destination.join("plugin.sh")).unwrap(),
        "echo one\n"
    );

    let check = wombat(temporary.path(), &source, &["check"]);
    assert_eq!(check.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&check.stdout).contains("satisfied"));
}

#[test]
fn git_package_pins_a_ref_and_ignores_later_remote_commits() {
    let temporary = tempfile::tempdir().unwrap();
    let (remote, checkout) = bare_remote(temporary.path());
    run(Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["tag", "v1"]));
    let pinned = head(&checkout);
    fs::write(checkout.join("plugin.sh"), "echo two\n").unwrap();
    run(Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["commit", "-am", "second"]));
    push(&checkout, &remote);

    let destination = temporary.path().join("installed/plugin");
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        format!(
            "local w = require('wombat')\nw.providers({{ 'git' }})\nw.need.package('plugin', {{ provider = 'git', with = {{ repository = {:?}, to = {:?}, ref = 'v1' }} }})\n",
            remote.to_str().unwrap(),
            destination.to_str().unwrap(),
        ),
    )
    .unwrap();

    let build = wombat(temporary.path(), &source, &["build", "--yes"]);
    assert!(
        build.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert_eq!(
        fs::read_to_string(destination.join("plugin.sh")).unwrap(),
        "echo one\n"
    );
    assert_eq!(head(&destination), pinned);

    let check = wombat(temporary.path(), &source, &["check"]);
    assert_eq!(check.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&check.stdout).contains("satisfied"));
}

#[test]
fn git_package_refuses_an_unrelated_existing_destination() {
    let temporary = tempfile::tempdir().unwrap();
    let (remote, _checkout) = bare_remote(temporary.path());
    let destination = temporary.path().join("installed/plugin");
    fs::create_dir_all(&destination).unwrap();
    run(Command::new("git").arg("init").arg(&destination));
    fs::write(destination.join("unrelated.txt"), "keep me\n").unwrap();
    run(Command::new("git").arg("-C").arg(&destination).args([
        "config",
        "user.email",
        "tests@example.invalid",
    ]));
    run(Command::new("git").arg("-C").arg(&destination).args([
        "config",
        "user.name",
        "Wombat Tests",
    ]));
    run(Command::new("git")
        .arg("-C")
        .arg(&destination)
        .args(["add", "."]));
    run(Command::new("git")
        .arg("-C")
        .arg(&destination)
        .args(["commit", "-m", "unrelated"]));

    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        format!(
            "local w = require('wombat')\nw.providers({{ 'git' }})\nw.need.package('plugin', {{ with = {{ repository = {:?}, to = {:?} }} }})\n",
            remote.to_str().unwrap(),
            destination.to_str().unwrap(),
        ),
    )
    .unwrap();

    let build = wombat(temporary.path(), &source, &["build", "--yes"]);
    assert!(!build.status.success());
    let error = String::from_utf8_lossy(&build.stderr);
    assert!(
        error.contains("already exists and is not a checkout of"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(destination.join("unrelated.txt")).unwrap(),
        "keep me\n"
    );
}

#[test]
fn git_package_resolve_validates_with_options() {
    let temporary = tempfile::tempdir().unwrap();
    let (remote, _checkout) = bare_remote(temporary.path());

    let missing_repository = temporary.path().join("missing-repository");
    fs::create_dir(&missing_repository).unwrap();
    fs::write(
        missing_repository.join("wombat.lua"),
        "local w = require('wombat')\nw.providers({ 'git' })\nw.need.package('plugin', { with = { to = '/tmp/plugin' } })\n",
    )
    .unwrap();
    let error = wombat(
        temporary.path(),
        &missing_repository,
        &["plan", "construct"],
    );
    assert!(!error.status.success());
    assert!(
        String::from_utf8_lossy(&error.stderr).contains("requires `with.repository`"),
        "{}",
        String::from_utf8_lossy(&error.stderr)
    );

    let relative_to = temporary.path().join("relative-to");
    fs::create_dir(&relative_to).unwrap();
    fs::write(
        relative_to.join("wombat.lua"),
        format!(
            "local w = require('wombat')\nw.providers({{ 'git' }})\nw.need.package('plugin', {{ with = {{ repository = {:?}, to = 'relative/plugin' }} }})\n",
            remote.to_str().unwrap(),
        ),
    )
    .unwrap();
    let error = wombat(temporary.path(), &relative_to, &["plan", "construct"]);
    assert!(!error.status.success());
    assert!(
        String::from_utf8_lossy(&error.stderr).contains("requires an absolute `with.to`"),
        "{}",
        String::from_utf8_lossy(&error.stderr)
    );
}
