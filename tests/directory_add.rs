use std::fs;

use wombat::manifest::SourceOrigin;
use wombat::{AddStatus, BuildOptions, add, build, initialize};

struct Fixture {
    _temporary: tempfile::TempDir,
    repository: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("source");
        let home = temporary.path().join("home");
        fs::create_dir(&home).unwrap();
        initialize(&repository).unwrap();
        Self {
            _temporary: temporary,
            repository,
            home,
        }
    }

    fn target(&self) -> std::path::PathBuf {
        self.home.join(".config/nvim")
    }
}

#[test]
fn imports_a_complete_tree_as_one_directory_install() {
    let fixture = Fixture::new();
    let target = fixture.target();
    fs::create_dir_all(target.join("lua/plugins")).unwrap();
    fs::write(target.join("init.lua"), "require('plugins')\n").unwrap();
    fs::write(target.join(".hidden"), "hidden\n").unwrap();
    fs::write(target.join("lua/plugins/example.lua"), "return {}\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(target.join("init.lua"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    let outcome = add(&fixture.repository, &fixture.home, &target).unwrap();
    assert_eq!(outcome.status, AddStatus::Added);
    assert_eq!(outcome.source, "dot_config/nvim");
    let auto = fs::read_to_string(fixture.repository.join("modules/auto.lua")).unwrap();
    assert!(auto.contains("w.install(\"dot_config/nvim\")"));
    assert!(!auto.contains("init.lua\")"));

    let built = build(BuildOptions::new(&fixture.repository, "build")).unwrap();
    assert_eq!(built.manifest.artifacts.len(), 3);
    assert!(built.manifest.artifacts.iter().all(|artifact| matches!(
        &artifact.source_origin,
        SourceOrigin::Directory { root, .. } if root == "dot_config/nvim"
    )));
    #[cfg(unix)]
    assert!(
        built
            .manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.source.ends_with("init.lua"))
            .unwrap()
            .content
            .executable
    );

    let second = add(&fixture.repository, &fixture.home, &target).unwrap();
    assert_eq!(second.status, AddStatus::AlreadyPresent);
}

#[test]
fn existing_directory_coverage_avoids_a_redundant_auto_declaration() {
    let fixture = Fixture::new();
    fs::write(
        fixture.repository.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"auto\")\nw.use(\"nvim\")\n",
    )
    .unwrap();
    fs::create_dir_all(fixture.repository.join("modules/dot_config")).unwrap();
    fs::write(
        fixture.repository.join("modules/dot_config/nvim.lua"),
        "local w = require(\"wombat\")\nw.install(\"nvim\")\n",
    )
    .unwrap();
    fs::create_dir_all(fixture.repository.join("dot_config/nvim")).unwrap();
    let target = fixture.target();
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("init.lua"), "return {}\n").unwrap();

    let outcome = add(&fixture.repository, &fixture.home, &target).unwrap();
    assert_eq!(outcome.status, AddStatus::Added);
    let auto = fs::read_to_string(fixture.repository.join("modules/auto.lua")).unwrap();
    assert!(!auto.contains("dot_config/nvim"));
    assert_eq!(
        fs::read(fixture.repository.join("dot_config/nvim/init.lua")).unwrap(),
        b"return {}\n"
    );
}

#[test]
fn differing_existing_source_refuses_the_whole_import() {
    let fixture = Fixture::new();
    let target = fixture.target();
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("a"), "target\n").unwrap();
    fs::create_dir_all(fixture.repository.join("dot_config/nvim")).unwrap();
    fs::write(fixture.repository.join("dot_config/nvim/a"), "source\n").unwrap();
    let before = fs::read_to_string(fixture.repository.join("modules/auto.lua")).unwrap();

    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("different directory tree"));
    assert_eq!(
        fs::read_to_string(fixture.repository.join("modules/auto.lua")).unwrap(),
        before
    );
    assert_eq!(
        fs::read_to_string(fixture.repository.join("dot_config/nvim/a")).unwrap(),
        "source\n"
    );
}

#[test]
fn empty_and_symlinked_trees_are_rejected_without_source_mutation() {
    let fixture = Fixture::new();
    let target = fixture.target();
    fs::create_dir_all(&target).unwrap();
    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("contains no regular files"));
    assert!(!fixture.repository.join("dot_config/nvim").exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        fs::write(fixture.home.join("outside"), "outside\n").unwrap();
        symlink(fixture.home.join("outside"), target.join("linked")).unwrap();
        let error = add(&fixture.repository, &fixture.home, &target)
            .unwrap_err()
            .to_string();
        assert!(error.contains("must not be a symbolic link"));
        assert!(!fixture.repository.join("dot_config/nvim").exists());
    }
}

#[cfg(unix)]
#[test]
fn special_and_non_utf8_entries_are_rejected_without_source_mutation() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::net::UnixListener;

    let fixture = Fixture::new();
    let target = fixture.target();
    fs::create_dir_all(&target).unwrap();
    let listener = UnixListener::bind(target.join("socket")).unwrap();
    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not a regular file or directory"), "{error}");
    assert!(!fixture.repository.join("dot_config/nvim").exists());
    drop(listener);
    fs::remove_file(target.join("socket")).unwrap();

    if fs::write(target.join(OsString::from_vec(vec![0xff])), "invalid\n").is_err() {
        // Some macOS volumes reject non-UTF-8 names before Wombat can observe
        // them. The filesystem has already enforced the invariant in that case.
        assert!(!fixture.repository.join("dot_config/nvim").exists());
        return;
    }
    let error = add(&fixture.repository, &fixture.home, &target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not valid UTF-8"), "{error}");
    assert!(!fixture.repository.join("dot_config/nvim").exists());
}
