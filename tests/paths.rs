use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use wombat::{BuildOptions, BuildOutcome, build};

struct Repository {
    _temporary: TempDir,
    root: PathBuf,
}

impl Repository {
    fn new(root_lua: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repository");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("wombat.lua"), root_lua).unwrap();
        Self {
            _temporary: temporary,
            root,
        }
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn build(&self) -> wombat::Result<BuildOutcome> {
        build(BuildOptions::new(
            &self.root,
            self._temporary.path().join("build"),
        ))
    }
}

#[test]
fn module_names_are_global_and_ambiguous_anchor_matches_are_rejected() {
    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"theme\")\n");
    repository.write("modules/theme.lua", "return { source = \"policy\" }\n");
    repository.write("modules/home/theme.lua", "return { source = \"home\" }\n");

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("ambiguous"), "{error}");
    assert!(error.contains("modules/theme.lua"), "{error}");
    assert!(error.contains("modules/home/theme.lua"), "{error}");
}

#[test]
fn legacy_dot_config_source_spelling_is_rejected() {
    let repository = Repository::new("return true\n");
    repository.write(".config/starship.toml", "format = '$all'\n");

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("dot_config/"), "{error}");
    assert!(error.contains("unsupported source tree"), "{error}");
}

#[test]
fn anchorless_install_requires_a_prefix_or_explicit_target() {
    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"plain\")\n");
    repository.write(
        "modules/plain.lua",
        "local w = require(\"wombat\")\nw.install(\"plain.txt\")\n",
    );
    repository.write("plain.txt", "plain\n");

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("cannot infer a target"), "{error}");
    assert!(error.contains("dot_config/"), "{error}");
}

#[test]
fn duplicate_explicit_and_inferred_targets_report_all_declarations() {
    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"a\")\nw.use(\"b\")\n");
    repository.write(
        "modules/dot_config/a.lua",
        "local w = require(\"wombat\")\nw.install(\"a.toml\", { to = \"~/.config/shared.toml\" })\n",
    );
    repository.write(
        "modules/dot_config/b.lua",
        "local w = require(\"wombat\")\nw.install(\"shared.toml\")\n",
    );
    repository.write("dot_config/a.toml", "a\n");
    repository.write("dot_config/shared.toml", "b\n");

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("same target"), "{error}");
    assert!(error.contains("modules/dot_config/a.lua"), "{error}");
    assert!(error.contains("modules/dot_config/b.lua"), "{error}");
    assert!(error.contains("~/.config/shared.toml"), "{error}");
}

#[test]
fn file_ancestor_targets_report_every_descendant_declaration() {
    let repository = Repository::new(
        "local w = require(\"wombat\")\nw.use(\"a\")\nw.use(\"b\")\nw.use(\"c\")\n",
    );
    repository.write(
        "modules/dot_config/a.lua",
        "local w = require(\"wombat\")\nw.install(\"nvim-file\", { to = \"~/.config/nvim\" })\n",
    );
    repository.write(
        "modules/dot_config/b.lua",
        "local w = require(\"wombat\")\nw.install(\"init.lua\", { to = \"~/.config/nvim/init.lua\" })\n",
    );
    repository.write(
        "modules/dot_config/c.lua",
        "local w = require(\"wombat\")\nw.install(\"plugin.lua\", { to = \"~/.config/nvim/lua/plugin.lua\" })\n",
    );
    repository.write("dot_config/nvim-file", "nvim\n");
    repository.write("dot_config/init.lua", "return true\n");
    repository.write("dot_config/plugin.lua", "return true\n");

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("is an ancestor"), "{error}");
    assert!(error.contains("~/.config/nvim/init.lua"), "{error}");
    assert!(error.contains("~/.config/nvim/lua/plugin.lua"), "{error}");
    assert!(error.contains("modules/dot_config/a.lua"), "{error}");
    assert!(error.contains("modules/dot_config/b.lua"), "{error}");
    assert!(error.contains("modules/dot_config/c.lua"), "{error}");
}

#[cfg(unix)]
#[test]
fn source_symlinks_and_symlinked_anchor_roots_are_rejected() {
    use std::os::unix::fs::symlink;

    fn configured_repository() -> Repository {
        let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"starship\")\n");
        repository.write(
            "modules/dot_config/starship.lua",
            "local w = require(\"wombat\")\nw.install(\"starship.toml\")\n",
        );
        repository
    }

    let leaf = configured_repository();
    let external = leaf._temporary.path().join("external.toml");
    fs::write(&external, "external\n").unwrap();
    fs::create_dir(leaf.root.join("dot_config")).unwrap();
    symlink(&external, leaf.root.join("dot_config/starship.toml")).unwrap();
    let error = leaf.build().unwrap_err().to_string();
    assert!(error.contains("symbolic links"), "{error}");

    let anchored = configured_repository();
    let external_directory = anchored._temporary.path().join("external-config");
    fs::create_dir(&external_directory).unwrap();
    fs::write(external_directory.join("starship.toml"), "external\n").unwrap();
    symlink(&external_directory, anchored.root.join("dot_config")).unwrap();
    let error = anchored.build().unwrap_err().to_string();
    assert!(error.contains("symbolic links"), "{error}");
}

#[test]
fn explicit_home_and_config_targets_have_distinct_anchors() {
    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"targets\")\n");
    repository.write(
        "modules/targets.lua",
        "local w = require(\"wombat\")\nw.install(\"a\", { to = \"~/.config/a\" })\nw.install(\"b\", { to = \"~/.config-file\" })\n",
    );
    repository.write("a", "a\n");
    repository.write("b", "b\n");

    let manifest = repository.build().unwrap().manifest;
    assert_eq!(manifest.artifacts[0].target.display, "~/.config-file");
    assert_eq!(manifest.artifacts[1].target.display, "~/.config/a");
}
