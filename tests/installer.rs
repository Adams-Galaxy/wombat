use std::fs;
use std::path::Path;
use std::process::Command;

#[cfg(unix)]
fn executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn installer_is_valid_posix_shell() {
    let status = Command::new("sh")
        .arg("-n")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn installer_uses_main_and_forwards_setup_arguments_exactly() {
    let temporary = tempfile::tempdir().unwrap();
    let bin = temporary.path().join("bin");
    let install_root = temporary.path().join("install");
    let cargo_args = temporary.path().join("cargo-args");
    let forwarded = temporary.path().join("forwarded");
    fs::create_dir(&bin).unwrap();
    executable(&bin.join("git"), "#!/bin/sh\nexit 0\n");
    executable(&bin.join("cc"), "#!/bin/sh\nexit 0\n");
    executable(
        &bin.join("cargo"),
        r#"#!/bin/sh
printf '%s\n' "$@" > "$WOMBAT_TEST_CARGO_ARGS"
root=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--root" ]; then root=$2; break; fi
  shift
done
mkdir -p "$root/bin"
cat > "$root/bin/wombat" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" > "$WOMBAT_TEST_FORWARDED"
EOF
chmod +x "$root/bin/wombat"
"#,
    );
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let output = Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
        .args(["setup", "Adams-Galaxy", "--yes", "--", "--theme", "dark"])
        .env("PATH", path)
        .env("WOMBAT_INSTALL_ROOT", &install_root)
        .env("WOMBAT_INSTALL_REPOSITORY", "file:///fixture/wombat.git")
        .env("WOMBAT_TEST_CARGO_ARGS", &cargo_args)
        .env("WOMBAT_TEST_FORWARDED", &forwarded)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cargo = fs::read_to_string(cargo_args).unwrap();
    assert!(cargo.contains("--branch\nmain\n"), "{cargo}");
    assert!(cargo.contains("--locked\n"), "{cargo}");
    assert_eq!(
        fs::read_to_string(forwarded).unwrap(),
        "setup\nAdams-Galaxy\n--yes\n--\n--theme\ndark\n"
    );
}

#[test]
fn noninteractive_installer_refuses_undeclared_prerequisite_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let output = Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
        .args(["setup", "Adams-Galaxy"])
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", temporary.path())
        .output()
        .unwrap();
    if output.status.success() {
        // Some development hosts place a complete Rust toolchain in /usr/bin.
        return;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--install-prerequisites") || stderr.contains("development prerequisites"),
        "{stderr}"
    );
}
