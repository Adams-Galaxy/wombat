use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use wombat::{AddMethod, AddStatus, BuildOptions, BuildOutcome, add, build};

const EMPTY_AUTO: &str =
    "local w = require(\"wombat\")\n\n-- wombat:add begin\n-- wombat:add end\n";

struct AddFixture {
    _temporary: TempDir,
    repository: PathBuf,
    home: PathBuf,
}

impl AddFixture {
    fn selected_auto() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("repository");
        let home = temporary.path().join("home");
        fs::create_dir_all(repository.join("modules")).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(
            repository.join("wombat.lua"),
            "local w = require(\"wombat\")\nw.use(\"auto\")\n",
        )
        .unwrap();
        fs::write(repository.join("modules/auto.lua"), EMPTY_AUTO).unwrap();
        Self {
            _temporary: temporary,
            repository,
            home,
        }
    }

    fn write_target(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let target = self.home.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, contents).unwrap();
        target
    }

    fn build(&self) -> wombat::Result<BuildOutcome> {
        build(BuildOptions::new(
            &self.repository,
            self._temporary.path().join("build"),
        ))
    }
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, path: &Path, output: &mut BTreeMap<String, Vec<u8>>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, output);
            } else {
                output.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    fs::read(path).unwrap(),
                );
            }
        }
    }

    let mut output = BTreeMap::new();
    visit(root, root, &mut output);
    output
}

#[test]
fn adds_config_and_literal_home_files_then_builds_them() {
    let fixture = AddFixture::selected_auto();
    let starship = fixture.write_target(".config/starship.toml", b"format = '$all'\n");
    let zshrc = fixture.write_target(".zshrc", b"export EDITOR=nvim\n");

    let home_outcome = add(&fixture.repository, &fixture.home, &zshrc).unwrap();
    let config_outcome = add(&fixture.repository, &fixture.home, &starship).unwrap();
    assert_eq!(home_outcome.status, AddStatus::Added);
    assert_eq!(config_outcome.status, AddStatus::Added);
    assert_eq!(config_outcome.source, "dot_config/starship.toml");
    assert_eq!(home_outcome.source, "home/.zshrc");

    let auto = fs::read_to_string(fixture.repository.join("modules/auto.lua")).unwrap();
    assert!(auto.contains("w.install(\"dot_config/starship.toml\")\nw.install(\"home/.zshrc\")"));
    let manifest = fixture.build().unwrap().manifest;
    assert_eq!(manifest.format_version, 6);
    assert_eq!(manifest.artifacts.len(), 2);
    assert_eq!(manifest.artifacts[0].target.display, "~/.zshrc");
    assert_eq!(
        manifest.artifacts[1].target.display,
        "~/.config/starship.toml"
    );
    assert_eq!(fs::read(&starship).unwrap(), b"format = '$all'\n");
}

#[test]
fn uniquely_covered_files_bypass_auto_for_config_local_and_home() {
    let fixture = AddFixture::selected_auto();
    fs::write(
        fixture.repository.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"trees\")\n",
    )
    .unwrap();
    fs::write(
        fixture.repository.join("modules/trees.lua"),
        "local w = require(\"wombat\")\nw.install(\"dot_config\")\nw.install(\"dot_local\")\nw.install(\"home\")\n",
    )
    .unwrap();
    fs::remove_file(fixture.repository.join("modules/auto.lua")).unwrap();
    for anchor in ["dot_config", "dot_local", "home"] {
        fs::create_dir(fixture.repository.join(anchor)).unwrap();
    }
    let config = fixture.write_target(".config/app/config.toml", b"config\n");
    let local = fixture.write_target(".local/bin/tool", b"tool\n");
    let home = fixture.write_target(".profile", b"profile\n");

    let outcomes = [
        add(&fixture.repository, &fixture.home, &config).unwrap(),
        add(&fixture.repository, &fixture.home, &local).unwrap(),
        add(&fixture.repository, &fixture.home, &home).unwrap(),
    ];

    assert_eq!(outcomes[0].source, "dot_config/app/config.toml");
    assert_eq!(outcomes[1].source, "dot_local/bin/tool");
    assert_eq!(outcomes[2].source, "home/.profile");
    for outcome in &outcomes {
        assert_eq!(outcome.status, AddStatus::Added);
        assert_eq!(
            outcome.method,
            AddMethod::Directory {
                owner: "trees".to_string(),
                declared_source: outcome.source.split('/').next().unwrap().to_string(),
            }
        );
        assert!(outcome.display().contains("owned by module `trees`"));
    }
    assert!(!fixture.repository.join("modules/auto.lua").exists());
    let manifest = fixture.build().unwrap().manifest;
    assert_eq!(manifest.artifacts.len(), 3);
    assert!(
        manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.target.display == "~/.local/bin/tool")
    );
}

#[test]
fn covered_add_is_idempotent_and_refuses_different_contents() {
    let fixture = AddFixture::selected_auto();
    fs::write(
        fixture.repository.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"config\")\n",
    )
    .unwrap();
    fs::write(
        fixture.repository.join("modules/config.lua"),
        "local w = require(\"wombat\")\nw.install(\"dot_config\")\n",
    )
    .unwrap();
    fs::create_dir(fixture.repository.join("dot_config")).unwrap();
    let target = fixture.write_target(".config/app.toml", b"same\n");

    add(&fixture.repository, &fixture.home, &target).unwrap();
    let repeated = add(&fixture.repository, &fixture.home, &target).unwrap();
    assert_eq!(repeated.status, AddStatus::AlreadyPresent);
    assert!(matches!(repeated.method, AddMethod::Directory { .. }));

    fs::write(&target, b"different\n").unwrap();
    let before = snapshot(&fixture.repository);
    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("different contents"), "{error}");
    assert_eq!(before, snapshot(&fixture.repository));
}

#[test]
fn add_refuses_ambiguous_empty_directory_coverage() {
    let fixture = AddFixture::selected_auto();
    fs::write(
        fixture.repository.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"a\")\nw.use(\"b\")\n",
    )
    .unwrap();
    for module in ["a", "b"] {
        fs::write(
            fixture.repository.join(format!("modules/{module}.lua")),
            "local w = require(\"wombat\")\nw.install(\"dot_config\")\n",
        )
        .unwrap();
    }
    fs::create_dir(fixture.repository.join("dot_config")).unwrap();
    let target = fixture.write_target(".config/new.toml", b"new\n");
    let before = snapshot(&fixture.repository);

    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("overlapping prospective targets"), "{error}");
    assert_eq!(before, snapshot(&fixture.repository));
}

#[test]
fn add_checks_prospective_directory_leaves_against_existing_artifacts() {
    let fixture = AddFixture::selected_auto();
    fs::write(
        fixture.repository.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"tree\")\nw.use(\"existing\")\n",
    )
    .unwrap();
    fs::write(
        fixture.repository.join("modules/tree.lua"),
        "local w = require(\"wombat\")\nw.install(\"dot_config\")\n",
    )
    .unwrap();
    fs::write(
        fixture.repository.join("modules/existing.lua"),
        "local w = require(\"wombat\")\nw.install(\"owned\", { to = \"~/.config/new/child\" })\n",
    )
    .unwrap();
    fs::create_dir(fixture.repository.join("dot_config")).unwrap();
    fs::write(fixture.repository.join("owned"), b"owned\n").unwrap();
    let target = fixture.write_target(".config/new", b"new\n");
    let before = snapshot(&fixture.repository);

    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("overlaps an artifact owned"), "{error}");
    assert_eq!(before, snapshot(&fixture.repository));
}

#[test]
fn add_is_idempotent_and_can_repair_only_the_declaration() {
    let fixture = AddFixture::selected_auto();
    let target = fixture.write_target(".config/starship.toml", b"starship\n");
    let source = fixture.repository.join("dot_config/starship.toml");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, b"starship\n").unwrap();

    let repaired = add(&fixture.repository, &fixture.home, &target).unwrap();
    assert_eq!(repaired.status, AddStatus::DeclarationAdded);
    let before = snapshot(&fixture.repository);
    let repeated = add(&fixture.repository, &fixture.home, &target).unwrap();
    let after = snapshot(&fixture.repository);
    assert_eq!(repeated.status, AddStatus::AlreadyPresent);
    assert_eq!(before, after);
}

#[test]
fn add_refuses_different_source_contents_without_mutation() {
    let fixture = AddFixture::selected_auto();
    let target = fixture.write_target(".config/starship.toml", b"target\n");
    let source = fixture.repository.join("dot_config/starship.toml");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, b"source\n").unwrap();
    let before = snapshot(&fixture.repository);

    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("different contents"), "{error}");
    assert_eq!(before, snapshot(&fixture.repository));
}

#[test]
fn add_refuses_targets_already_owned_by_other_modules_without_mutation() {
    let fixture = AddFixture::selected_auto();
    let target = fixture.write_target(".config/starship.toml", b"target\n");
    fs::create_dir_all(fixture.repository.join("modules/dot_config")).unwrap();
    fs::create_dir_all(fixture.repository.join("dot_config")).unwrap();
    fs::write(
        fixture.repository.join("modules/dot_config/starship.lua"),
        "local w = require(\"wombat\")\nw.install(\"owned.toml\", { to = \"~/.config/starship.toml\" })\n",
    )
    .unwrap();
    fs::write(fixture.repository.join("dot_config/owned.toml"), b"owned\n").unwrap();
    fs::write(
        fixture.repository.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"auto\")\nw.use(\"starship\")\n",
    )
    .unwrap();
    let before = snapshot(&fixture.repository);

    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("overlaps an artifact owned"), "{error}");
    assert_eq!(before, snapshot(&fixture.repository));
}

#[test]
fn add_requires_an_intact_selected_auto_module() {
    let fixture = AddFixture::selected_auto();
    let target = fixture.write_target(".config/starship.toml", b"starship\n");

    fs::write(fixture.repository.join("modules/auto.lua"), "return {}\n").unwrap();
    let before = snapshot(&fixture.repository);
    let malformed = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(malformed.contains("proposed declaration"), "{malformed}");
    assert_eq!(before, snapshot(&fixture.repository));

    fs::write(fixture.repository.join("modules/auto.lua"), EMPTY_AUTO).unwrap();
    fs::write(
        fixture.repository.join("wombat.lua"),
        "local w = require(\"wombat\")\nreturn w\n",
    )
    .unwrap();
    let before = snapshot(&fixture.repository);
    let unselected = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(unselected.contains("not selected"), "{unselected}");
    assert_eq!(before, snapshot(&fixture.repository));

    fs::remove_file(fixture.repository.join("modules/auto.lua")).unwrap();
    let before = snapshot(&fixture.repository);
    let absent = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(absent.contains("is required"), "{absent}");
    assert_eq!(before, snapshot(&fixture.repository));
}

#[test]
fn add_rejects_invalid_targets_without_source_state_changes() {
    let fixture = AddFixture::selected_auto();
    let outside = fixture._temporary.path().join("outside.txt");
    fs::write(&outside, b"outside\n").unwrap();
    let directory = fixture.home.join("directory");
    fs::create_dir(&directory).unwrap();
    let before = snapshot(&fixture.repository);

    for target in [
        Path::new("relative.txt"),
        outside.as_path(),
        directory.as_path(),
    ] {
        assert!(add(&fixture.repository, &fixture.home, target).is_err());
        assert_eq!(before, snapshot(&fixture.repository));
    }

    let missing = fixture.home.join("missing.txt");
    assert!(add(&fixture.repository, &fixture.home, &missing).is_err());
    assert_eq!(before, snapshot(&fixture.repository));
}

#[cfg(unix)]
#[test]
fn add_rejects_symlink_targets() {
    use std::os::unix::fs::symlink;

    let fixture = AddFixture::selected_auto();
    let real = fixture.write_target("real.txt", b"real\n");
    let link = fixture.home.join("link.txt");
    symlink(real, &link).unwrap();
    let before = snapshot(&fixture.repository);
    let error = add(&fixture.repository, &fixture.home, &link)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symbolic link"), "{error}");
    assert_eq!(before, snapshot(&fixture.repository));
}

#[test]
fn generated_paths_escape_lua_and_sort_deterministically() {
    let fixture = AddFixture::selected_auto();
    let quoted = fixture.write_target(".config/a \"quoted\".toml", b"quoted\n");
    let plain = fixture.write_target(".config/z.toml", b"plain\n");
    add(&fixture.repository, &fixture.home, &plain).unwrap();
    add(&fixture.repository, &fixture.home, &quoted).unwrap();

    let auto = fs::read_to_string(fixture.repository.join("modules/auto.lua")).unwrap();
    assert!(auto.contains("a \\\"quoted\\\".toml"));
    assert!(fixture.build().is_ok());
}

#[test]
fn artifact_can_graduate_from_auto_to_an_anchored_module() {
    let fixture = AddFixture::selected_auto();
    let target = fixture.write_target(".config/starship.toml", b"starship\n");
    add(&fixture.repository, &fixture.home, &target).unwrap();
    let generated = fixture.build().unwrap();
    let generated_artifact = generated.manifest.artifacts[0].clone();

    fs::create_dir_all(fixture.repository.join("modules/dot_config")).unwrap();
    fs::write(
        fixture.repository.join("modules/dot_config/starship.lua"),
        "local w = require(\"wombat\")\nw.install(\"starship.toml\")\n",
    )
    .unwrap();
    fs::write(
        fixture.repository.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"auto\")\nw.use(\"starship\")\n",
    )
    .unwrap();
    let duplicate = fixture.build().unwrap_err().to_string();
    assert!(duplicate.contains("same target"), "{duplicate}");

    fs::write(fixture.repository.join("modules/auto.lua"), EMPTY_AUTO).unwrap();
    let graduated = fixture.build().unwrap();
    let graduated_artifact = &graduated.manifest.artifacts[0];
    assert_eq!(generated_artifact.source, graduated_artifact.source);
    assert_eq!(
        generated_artifact.target.anchor,
        graduated_artifact.target.anchor
    );
    assert_eq!(
        generated_artifact.target.path,
        graduated_artifact.target.path
    );
    assert_eq!(
        generated_artifact.target.display,
        graduated_artifact.target.display
    );
    assert_eq!(graduated_artifact.owner, "starship");
}

#[test]
fn cli_add_uses_home_and_reports_the_source_mapping() {
    let fixture = AddFixture::selected_auto();
    let target = fixture.write_target(".config/starship.toml", b"starship\n");
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .arg("--source")
        .arg(&fixture.repository)
        .arg("add")
        .arg(&target)
        .env("HOME", &fixture.home)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert!(String::from_utf8_lossy(&output.stdout).contains("dot_config/starship.toml"));
    assert!(fixture.build().is_ok());
}

#[cfg(unix)]
#[test]
fn add_rejects_non_utf8_target_paths_without_mutation() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = AddFixture::selected_auto();
    let target = fixture.home.join(OsString::from_vec(vec![b'f', 0x80]));
    if fs::write(&target, b"invalid path\n").is_err() {
        return;
    }
    let before = snapshot(&fixture.repository);
    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();

    assert!(error.contains("valid UTF-8"), "{error}");
    assert_eq!(before, snapshot(&fixture.repository));
}
