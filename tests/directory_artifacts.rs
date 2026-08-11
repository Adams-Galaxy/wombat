use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use wombat::manifest::{SourceOrigin, TargetOrigin};
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
fn anchored_directories_expand_hidden_nested_and_dot_local_files() {
    let repository = Repository::new(
        "local w = require(\"wombat\")\nw.use(\"nvim\")\nw.use(\"tools\")\nw.use(\"shell\")\n",
    );
    repository.write(
        "modules/dot_config/nvim.lua",
        "local w = require(\"wombat\")\nw.install(\"nvim\")\n",
    );
    repository.write(
        "modules/dot_local/tools.lua",
        "local w = require(\"wombat\")\nw.install(\".\")\n",
    );
    repository.write(
        "modules/home/shell.lua",
        "local w = require(\"wombat\")\nw.install(\".\")\n",
    );
    repository.write("dot_config/nvim/init.lua", "return true\n");
    repository.write("dot_config/nvim/.state/keep", "hidden\n");
    repository.write("dot_local/bin/tool", "#!/bin/sh\n");
    repository.write("home/.profile", "export EDITOR=nvim\n");
    fs::create_dir_all(repository.root.join("dot_config/nvim/empty/nested")).unwrap();

    let outcome = repository.build().unwrap();
    let manifest = outcome.manifest;
    assert_eq!(manifest.format_version, 9);
    assert_eq!(manifest.artifacts.len(), 4);
    assert!(
        manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.target.display == "~/.local/bin/tool")
    );
    assert!(
        manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.target.display == "~/.config/nvim/.state/keep")
    );
    for artifact in &manifest.artifacts {
        assert!(matches!(
            artifact.source_origin,
            SourceOrigin::Directory { .. }
        ));
    }
    assert_eq!(
        fs::read_to_string(outcome.build_dir.join("tree/home/.local/bin/tool")).unwrap(),
        "#!/bin/sh\n"
    );
    assert!(!outcome.build_dir.join("tree/config/nvim/empty").exists());
}

#[test]
fn anchorless_directories_support_inferred_and_explicit_roots() {
    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"directories\")\n");
    repository.write(
        "modules/directories.lua",
        "local w = require(\"wombat\")\nw.install(\"dot_config/app\")\nw.install(\"dot_config/other\", { to = \"~/.config\" })\nw.install(\"home/files\", { to = \"~/\" })\n",
    );
    repository.write("dot_config/app/settings.toml", "setting = true\n");
    repository.write("dot_config/other/other.toml", "other = true\n");
    repository.write("home/files/readme", "home\n");

    let manifest = repository.build().unwrap().manifest;
    let explicit = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.source == "home/files/readme")
        .unwrap();
    assert_eq!(explicit.target.display, "~/readme");
    assert!(matches!(
        explicit.target.origin,
        TargetOrigin::DirectoryExplicit { ref declared, ref relative }
            if declared == "~/" && relative == "readme"
    ));
    let inferred = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.source == "dot_config/app/settings.toml")
        .unwrap();
    assert_eq!(inferred.target.display, "~/.config/app/settings.toml");
    let config_root = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.source == "dot_config/other/other.toml")
        .unwrap();
    assert_eq!(config_root.target.display, "~/.config/other.toml");
}

#[test]
fn empty_directory_declarations_emit_no_artifacts_but_retain_source_identity() {
    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"empty\")\n");
    repository.write(
        "modules/dot_config/empty.lua",
        "local w = require(\"wombat\")\nw.install(\"empty\")\n",
    );
    fs::create_dir_all(repository.root.join("dot_config/empty/nested")).unwrap();

    let declared = repository.build().unwrap();
    assert!(declared.manifest.artifacts.is_empty());
    repository.write("modules/dot_config/empty.lua", "return true\n");
    let omitted = repository.build().unwrap();
    assert_ne!(declared.build_id, omitted.build_id);
    assert_ne!(declared.manifest.sources, omitted.manifest.sources);
}

#[test]
fn direct_and_directory_provenance_produce_distinct_build_identities() {
    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"tree\")\n");
    repository.write(
        "modules/dot_config/tree.lua",
        "local w = require(\"wombat\")\nw.install(\"tree/file\")\n",
    );
    repository.write("dot_config/tree/file", "same\n");
    let direct = repository.build().unwrap();

    repository.write(
        "modules/dot_config/tree.lua",
        "local w = require(\"wombat\")\nw.install(\"tree\")\n",
    );
    let directory = repository.build().unwrap();

    assert_ne!(direct.build_id, directory.build_id);
    assert!(matches!(
        direct.manifest.artifacts[0].source_origin,
        SourceOrigin::Direct { .. }
    ));
    assert!(matches!(
        directory.manifest.artifacts[0].source_origin,
        SourceOrigin::Directory { .. }
    ));
}

#[test]
fn expanded_conflicts_include_concrete_directory_leaves() {
    let repository =
        Repository::new("local w = require(\"wombat\")\nw.use(\"directory\")\nw.use(\"direct\")\n");
    repository.write(
        "modules/directory.lua",
        "local w = require(\"wombat\")\nw.install(\"dot_config/tree\")\n",
    );
    repository.write(
        "modules/direct.lua",
        "local w = require(\"wombat\")\nw.install(\"other\", { to = \"~/.config/tree/file\" })\n",
    );
    repository.write("dot_config/tree/file", "directory\n");
    repository.write("other", "direct\n");

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("same target"), "{error}");
    assert!(error.contains("dot_config/tree/file"), "{error}");
    assert!(
        error.contains("expanded from directory `dot_config/tree`"),
        "{error}"
    );
    assert!(error.contains("modules/directory.lua"), "{error}");
    assert!(error.contains("modules/direct.lua"), "{error}");
}

#[test]
fn home_root_expansion_is_canonicalized_before_conflict_checks() {
    let repository =
        Repository::new("local w = require(\"wombat\")\nw.use(\"directory\")\nw.use(\"direct\")\n");
    repository.write(
        "modules/directory.lua",
        "local w = require(\"wombat\")\nw.install(\"dot_config/tree\", { to = \"~/\" })\n",
    );
    repository.write(
        "modules/direct.lua",
        "local w = require(\"wombat\")\nw.install(\"other\", { to = \"~/.config/file\" })\n",
    );
    repository.write("dot_config/tree/.config/file", "directory\n");
    repository.write("other", "direct\n");

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("same target"), "{error}");
    assert!(error.contains("~/.config/file"), "{error}");
}

#[test]
fn overlapping_directory_ranges_are_allowed_when_leaves_are_disjoint() {
    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"a\")\nw.use(\"b\")\n");
    repository.write(
        "modules/a.lua",
        "local w = require(\"wombat\")\nw.install(\"dot_config/a\", { to = \"~/.config/shared\" })\n",
    );
    repository.write(
        "modules/b.lua",
        "local w = require(\"wombat\")\nw.install(\"dot_config/b\", { to = \"~/.config/shared\" })\n",
    );
    repository.write("dot_config/a/one", "one\n");
    repository.write("dot_config/b/two", "two\n");

    let manifest = repository.build().unwrap().manifest;
    assert_eq!(manifest.artifacts.len(), 2);
    assert_eq!(manifest.artifacts[0].target.display, "~/.config/shared/one");
    assert_eq!(manifest.artifacts[1].target.display, "~/.config/shared/two");
}

#[cfg(unix)]
#[test]
fn directory_descendants_reject_symlinks_and_special_files() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    fn repository() -> Repository {
        let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"tree\")\n");
        repository.write(
            "modules/dot_config/tree.lua",
            "local w = require(\"wombat\")\nw.install(\"tree\")\n",
        );
        fs::create_dir_all(repository.root.join("dot_config/tree")).unwrap();
        repository
    }

    let linked = repository();
    let outside = linked._temporary.path().join("outside");
    fs::write(&outside, "outside\n").unwrap();
    symlink(&outside, linked.root.join("dot_config/tree/link")).unwrap();
    let error = linked.build().unwrap_err().to_string();
    assert!(error.contains("symbolic link"), "{error}");

    let root_link = repository();
    let external_tree = root_link._temporary.path().join("external-tree");
    fs::create_dir(&external_tree).unwrap();
    fs::remove_dir(root_link.root.join("dot_config/tree")).unwrap();
    symlink(&external_tree, root_link.root.join("dot_config/tree")).unwrap();
    let error = root_link.build().unwrap_err().to_string();
    assert!(error.contains("symbolic links"), "{error}");

    let component = repository();
    let external_directory = component._temporary.path().join("external-directory");
    fs::create_dir(&external_directory).unwrap();
    fs::write(external_directory.join("file"), "outside\n").unwrap();
    symlink(
        &external_directory,
        component.root.join("dot_config/tree/nested"),
    )
    .unwrap();
    let error = component.build().unwrap_err().to_string();
    assert!(error.contains("symbolic link"), "{error}");

    let special = repository();
    let socket = special.root.join("dot_config/tree/socket");
    let _listener = UnixListener::bind(&socket).unwrap();
    let error = special.build().unwrap_err().to_string();
    assert!(error.contains("not a regular file or directory"), "{error}");
}

#[cfg(unix)]
#[test]
fn directory_entries_must_have_utf8_paths() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let repository = Repository::new("local w = require(\"wombat\")\nw.use(\"tree\")\n");
    repository.write(
        "modules/dot_config/tree.lua",
        "local w = require(\"wombat\")\nw.install(\"tree\")\n",
    );
    let directory = repository.root.join("dot_config/tree");
    fs::create_dir_all(&directory).unwrap();
    if fs::write(
        directory.join(OsString::from_vec(vec![b'f', 0x80])),
        "bad\n",
    )
    .is_err()
    {
        return;
    }

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("not valid UTF-8"), "{error}");
}
