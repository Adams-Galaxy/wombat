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

#[cfg(unix)]
#[test]
fn fedora_installer_uses_dnf_for_the_complete_prerequisite_layer() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let bin = temporary.path().join("bin");
    let home = temporary.path().join("home");
    let install_root = temporary.path().join("install");
    let dnf_args = temporary.path().join("dnf-args");
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&home).unwrap();

    for command in ["sh", "mktemp", "rm", "mkdir", "chmod"] {
        let output = Command::new("sh")
            .args(["-c", "command -v \"$1\"", "wombat-installer-test", command])
            .output()
            .unwrap();
        assert!(output.status.success());
        let path = Path::new(std::str::from_utf8(&output.stdout).unwrap().trim());
        symlink(path, bin.join(command)).unwrap();
    }
    executable(&bin.join("uname"), "#!/bin/sh\nprintf 'Linux\\n'\n");
    executable(&bin.join("id"), "#!/bin/sh\nprintf '0\\n'\n");
    executable(&bin.join("git"), "#!/bin/sh\nexit 0\n");
    executable(&bin.join("cc"), "#!/bin/sh\nexit 0\n");
    executable(
        &bin.join("dnf"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$WOMBAT_TEST_DNF_ARGS\"\n",
    );
    executable(
        &bin.join("curl"),
        r##"#!/bin/sh
out=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then out=$2; shift 2; else shift; fi
done
printf '%s\n' '#!/bin/sh' \
  'mkdir -p "$HOME/.cargo/bin"' \
  'printf '\''%s\n'\'' '\''#!/bin/sh'\'' '\''root='\'' '\''while [ "$#" -gt 0 ]; do if [ "$1" = "--root" ]; then root=$2; break; fi; shift; done'\'' '\''mkdir -p "$root/bin"'\'' '\''printf "#!/bin/sh\\nexit 0\\n" > "$root/bin/wombat"'\'' '\''chmod +x "$root/bin/wombat"'\'' > "$HOME/.cargo/bin/cargo"' \
  'chmod +x "$HOME/.cargo/bin/cargo"' > "$out"
"##,
    );

    let output = Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
        .args(["--install-prerequisites", "setup", "Adams-Galaxy", "--yes"])
        .env("PATH", &bin)
        .env("HOME", &home)
        .env("WOMBAT_INSTALL_ROOT", &install_root)
        .env("WOMBAT_TEST_DNF_ARGS", &dnf_args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dnf_args).unwrap(),
        "install\n--assumeyes\nca-certificates\ncurl\ngit\ngcc\nmake\n"
    );
}
