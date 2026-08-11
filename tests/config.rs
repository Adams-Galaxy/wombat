use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct CliFixture {
    temporary: TempDir,
    home: PathBuf,
    xdg: PathBuf,
    repository: PathBuf,
    unrelated: PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let xdg = temporary.path().join("xdg");
        let repository = temporary.path().join("repository");
        let unrelated = temporary.path().join("unrelated");
        fs::create_dir_all(repository.join("modules/dot_config")).unwrap();
        fs::create_dir_all(repository.join("dot_config")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(
            repository.join("wombat.lua"),
            "local w = require(\"wombat\")\nw.use(\"app\")\n",
        )
        .unwrap();
        fs::write(
            repository.join("modules/dot_config/app.lua"),
            "local w = require(\"wombat\")\nw.install(\"app.toml\")\n",
        )
        .unwrap();
        fs::write(repository.join("dot_config/app.toml"), "app = true\n").unwrap();
        Self {
            temporary,
            home,
            xdg,
            repository,
            unrelated,
        }
    }

    fn write_config(&self, contents: &str) {
        fs::create_dir_all(self.xdg.join("wombat")).unwrap();
        fs::write(self.xdg.join("wombat/config.toml"), contents).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wombat"))
            .args(args)
            .current_dir(&self.unrelated)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.xdg)
            .output()
            .unwrap()
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
}

fn repository_config(path: &Path) -> String {
    format!(
        "format_version = 1\nrepository = {:?}\n",
        path.to_str().unwrap()
    )
}

#[test]
fn configured_source_and_relative_build_are_independent_of_process_directory() {
    let fixture = CliFixture::new();
    fixture.write_config(&repository_config(&fixture.repository));
    let output = fixture.run(&["build", "-B", "alternate"]);
    assert_success(&output);
    assert!(fixture.repository.join("alternate/manifest.json").is_file());
    assert!(!fixture.unrelated.join("alternate").exists());
}

#[test]
fn fallback_config_expands_a_home_relative_repository() {
    let fixture = CliFixture::new();
    let repository = fixture.home.join("dotfiles");
    fs::rename(&fixture.repository, &repository).unwrap();
    let config = fixture.home.join(".config/wombat");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        "format_version = 1\nrepository = \"~/dotfiles\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["build", "-B", "configured-build"])
        .current_dir(&fixture.unrelated)
        .env("HOME", &fixture.home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert_success(&output);
    assert!(repository.join("configured-build/manifest.json").is_file());
}

#[test]
fn default_source_uses_local_share_wombat_and_its_build_directory() {
    let fixture = CliFixture::new();
    let default = fixture.home.join(".local/share/wombat");
    fs::create_dir_all(default.parent().unwrap()).unwrap();
    fs::rename(&fixture.repository, &default).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .arg("build")
        .current_dir(&fixture.unrelated)
        .env("HOME", &fixture.home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert_success(&output);
    assert!(default.join("build/manifest.json").is_file());
}

#[test]
fn explicit_source_still_loads_task_configuration() {
    let fixture = CliFixture::new();
    fixture.write_config("this is not TOML [[[");
    let build_dir = fixture.temporary.path().join("explicit-build");
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args([
            "build",
            "--source",
            fixture.repository.to_str().unwrap(),
            "-B",
            build_dir.to_str().unwrap(),
        ])
        .current_dir(&fixture.unrelated)
        .env_remove("HOME")
        .env("XDG_CONFIG_HOME", &fixture.xdg)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to parse Wombat config"));
    assert!(!build_dir.join("manifest.json").is_file());
}

#[cfg(unix)]
#[test]
fn explicit_source_uses_configured_task_interpreter() {
    let fixture = CliFixture::new();
    fs::create_dir_all(fixture.repository.join("tasks")).unwrap();
    fs::write(
        fixture.repository.join("modules/dot_config/app.lua"),
        "local w = require('wombat')\nw.build.task('generate.py')\n",
    )
    .unwrap();
    fs::write(
        fixture.repository.join("tasks/generate.py"),
        "from wombat import output\n(output / 'configured').write_text('yes\\n')\n",
    )
    .unwrap();
    let wrapper = fixture.temporary.path().join("python-wrapper");
    fs::write(&wrapper, "#!/bin/sh\nexec python3 \"$@\"\n").unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();
    fixture.write_config(&format!(
        "format_version = 1\nrepository = {:?}\n[tasks.interpreters.python]\ncommand = {:?}\n",
        fixture.repository.to_str().unwrap(),
        wrapper.to_str().unwrap(),
    ));

    let output = fixture.run(&["--source", fixture.repository.to_str().unwrap(), "build"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("generate.py: running"));
    assert_eq!(
        fs::read_to_string(fixture.repository.join("build/tree/config/configured")).unwrap(),
        "yes\n"
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.repository.join("build/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(
        manifest["tasks"][0]["runner"]["command"],
        wrapper.to_str().unwrap()
    );
}

#[test]
fn explicit_relative_source_resolves_from_process_directory() {
    let fixture = CliFixture::new();
    let relative_repository = fixture.unrelated.join("repo");
    fs::rename(&fixture.repository, &relative_repository).unwrap();
    let output = fixture.run(&["--source", "repo", "build"]);
    assert_success(&output);
    assert!(relative_repository.join("build/manifest.json").is_file());
}

#[test]
fn configuration_failures_are_precise() {
    let fixture = CliFixture::new();
    let cases = [
        (
            "format_version = 1\nrepository = \"relative\"\n",
            "must be absolute",
        ),
        (
            "format_version = 2\nrepository = \"/tmp/repo\"\n",
            "unsupported Wombat config",
        ),
        (
            "format_version = 1\nrepository = \"/tmp/repo\"\nunknown = true\n",
            "unknown field",
        ),
        ("not valid TOML [[", "failed to parse Wombat config"),
    ];
    for (contents, expected) in cases {
        fixture.write_config(contents);
        let output = fixture.run(&["build"]);
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "expected {expected:?} in {stderr:?}"
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn relative_xdg_and_missing_home_are_rejected() {
    let fixture = CliFixture::new();
    let relative_xdg = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .arg("build")
        .current_dir(&fixture.unrelated)
        .env("HOME", &fixture.home)
        .env("XDG_CONFIG_HOME", "relative")
        .output()
        .unwrap();
    assert_eq!(relative_xdg.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&relative_xdg.stderr).contains("must be absolute"));

    let missing_home = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .arg("build")
        .current_dir(&fixture.unrelated)
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert_eq!(missing_home.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing_home.stderr).contains("HOME is not set"));
}

#[test]
fn user_home_is_never_accepted_as_a_build_directory() {
    let fixture = CliFixture::new();
    let output = fixture.run(&[
        "--source",
        fixture.repository.to_str().unwrap(),
        "build",
        "-B",
        fixture.home.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("user home"));
}
