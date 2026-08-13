use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use wombat::{BuildOptions, BuildStatus, build, verify_build};

struct Repository {
    temporary: TempDir,
    root: PathBuf,
    build_dir: PathBuf,
}

impl Repository {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repository");
        let build_dir = temporary.path().join("output");
        fs::create_dir_all(root.join("modules/apps")).unwrap();
        fs::create_dir_all(root.join("src/dot_config")).unwrap();
        fs::write(
            root.join("wombat.lua"),
            "local w = require(\"wombat\")\nw.use(\"app\")\nw.use(\"shell\")\n",
        )
        .unwrap();
        fs::write(
            root.join("modules/apps/app.lua"),
            "local w = require(\"wombat\")\nw.module.from(\".config\")\nw.install(\"app.toml\")\n",
        )
        .unwrap();
        fs::write(
            root.join("modules/apps/shell.lua"),
            "local w = require(\"wombat\")\nw.module.from(\".\")\nw.install(\".tool\")\n",
        )
        .unwrap();
        fs::write(root.join("src/dot_config/app.toml"), "theme = 'dark'\n").unwrap();
        fs::write(root.join("src/dot_tool"), "#!/bin/sh\necho tool\n").unwrap();
        Self {
            temporary,
            root,
            build_dir,
        }
    }

    fn build(&self) -> wombat::Result<wombat::BuildOutcome> {
        build(BuildOptions::new(&self.root, &self.build_dir))
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    let metadata = fs::symlink_metadata(source).unwrap();
    if metadata.file_type().is_dir() {
        fs::create_dir_all(destination).unwrap();
        let mut entries = fs::read_dir(source)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        }
    } else {
        fs::copy(source, destination).unwrap();
        fs::set_permissions(destination, metadata.permissions()).unwrap();
    }
}

fn copy_product(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    copy_tree(
        &source.join("manifest.json"),
        &destination.join("manifest.json"),
    );
    copy_tree(&source.join("tree"), &destination.join("tree"));
}

#[test]
fn build_product_is_exact_relocatable_and_cache_independent() {
    let repository = Repository::new();
    let first = repository.build().unwrap();
    assert_eq!(first.status, BuildStatus::Created);
    assert!(first.build_id.starts_with("sha256:"));
    assert_eq!(first.build_id.len(), 71);
    assert_eq!(first.artifact_count, 2);
    assert_eq!(
        fs::read(repository.build_dir.join("tree/.config/app.toml")).unwrap(),
        b"theme = 'dark'\n"
    );
    assert_eq!(
        fs::read(repository.build_dir.join("tree/.tool")).unwrap(),
        b"#!/bin/sh\necho tool\n"
    );
    assert!(verify_build(&repository.build_dir).is_ok());

    let second = repository.build().unwrap();
    assert_eq!(second.status, BuildStatus::Unchanged);
    assert_eq!(first.manifest, second.manifest);

    let cache = repository.build_dir.join(".wombat/cache/templates");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("unused"), "cache does not define output\n").unwrap();
    fs::remove_dir_all(repository.build_dir.join(".wombat/cache")).unwrap();
    let without_cache = repository.build().unwrap();
    assert_eq!(without_cache.status, BuildStatus::Unchanged);
    assert_eq!(first.manifest, without_cache.manifest);

    let relocated = repository.temporary.path().join("relocated");
    copy_product(&repository.build_dir, &relocated);
    let verified = verify_build(&relocated).unwrap();
    assert_eq!(verified.manifest, first.manifest);
}

#[test]
fn semantic_and_file_changes_produce_new_build_identities() {
    let repository = Repository::new();
    let first = repository.build().unwrap();

    fs::write(
        repository.root.join("src/dot_config/app.toml"),
        "theme = 'light'\n",
    )
    .unwrap();
    let content = repository.build().unwrap();
    assert_eq!(content.status, BuildStatus::Updated);
    assert_ne!(first.build_id, content.build_id);

    fs::write(
        repository.root.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"app\", { variant = \"work\" })\nw.use(\"shell\")\n",
    )
    .unwrap();
    let configuration = repository.build().unwrap();
    assert_ne!(content.build_id, configuration.build_id);

    fs::rename(
        repository.root.join("modules/apps/app.lua"),
        repository.root.join("modules/apps/renamed.lua"),
    )
    .unwrap();
    fs::write(
        repository.root.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"renamed\", { variant = \"work\" })\nw.use(\"shell\")\n",
    )
    .unwrap();
    let ownership = repository.build().unwrap();
    assert_ne!(configuration.build_id, ownership.build_id);
    assert_eq!(
        ownership
            .manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.target.path == ".config/app.toml")
            .unwrap()
            .owner,
        "renamed"
    );
}

#[cfg(unix)]
#[test]
fn executable_intent_is_normalized_and_affects_identity() {
    use std::os::unix::fs::PermissionsExt;

    let repository = Repository::new();
    let first = repository.build().unwrap();
    assert!(!first.manifest.artifacts[0].content.executable);
    assert!(!first.manifest.artifacts[1].content.executable);
    assert_eq!(
        fs::metadata(repository.build_dir.join("tree/.tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );

    let source = repository.root.join("src/dot_tool");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o711)).unwrap();
    let executable = repository.build().unwrap();
    assert_ne!(first.build_id, executable.build_id);
    assert!(
        executable
            .manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.target.path == ".tool")
            .unwrap()
            .content
            .executable
    );
    assert_eq!(
        fs::metadata(repository.build_dir.join("tree/.tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn rebuilding_replaces_tampered_or_extra_derived_output() {
    let repository = Repository::new();
    let first = repository.build().unwrap();
    fs::write(
        repository.build_dir.join("tree/.config/app.toml"),
        "tampered\n",
    )
    .unwrap();
    fs::write(repository.build_dir.join("tree/.config/extra"), "extra\n").unwrap();
    assert!(verify_build(&repository.build_dir).is_err());

    let repaired = repository.build().unwrap();
    assert_eq!(repaired.status, BuildStatus::Repaired);
    assert_eq!(repaired.build_id, first.build_id);
    assert!(!repository.build_dir.join("tree/.config/extra").exists());
    assert!(verify_build(&repository.build_dir).is_ok());
}

#[test]
fn failed_evaluation_leaves_the_previous_product_untouched() {
    let repository = Repository::new();
    let previous = repository.build().unwrap();
    fs::write(
        repository.root.join("wombat.lua"),
        "this is not valid Lua {{\n",
    )
    .unwrap();

    assert!(repository.build().is_err());
    let still_published = verify_build(&repository.build_dir).unwrap();
    assert_eq!(still_published.manifest.build_id, previous.build_id);
}

#[test]
fn products_survive_a_release_but_not_a_construction_change() {
    let repository = Repository::new();
    repository.build().unwrap();

    let released = repository.temporary.path().join("released");
    copy_product(&repository.build_dir, &released);
    retag(
        &released,
        "wombat_version",
        serde_json::Value::from("99.0.0"),
    );
    verify_build(&released).expect("a new release alone must not invalidate a product");

    let reconstructed = repository.temporary.path().join("reconstructed");
    copy_product(&repository.build_dir, &reconstructed);
    retag(
        &reconstructed,
        "construction_version",
        serde_json::Value::from(99),
    );
    let error = verify_build(&reconstructed).unwrap_err().to_string();
    assert!(error.contains("construction version"), "{error}");
    assert!(error.contains("rebuild"), "{error}");
}

fn retag(product: &std::path::Path, key: &str, value: serde_json::Value) {
    let path = product.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    manifest[key] = value;
    fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

#[test]
fn verifier_rejects_missing_extra_and_manifest_tampering() {
    let repository = Repository::new();
    repository.build().unwrap();

    let missing = repository.temporary.path().join("missing");
    copy_product(&repository.build_dir, &missing);
    fs::remove_file(missing.join("tree/.config/app.toml")).unwrap();
    assert!(
        verify_build(&missing)
            .unwrap_err()
            .to_string()
            .contains("missing")
    );

    let extra = repository.temporary.path().join("extra");
    copy_product(&repository.build_dir, &extra);
    fs::write(extra.join("tree/extra"), "extra\n").unwrap();
    assert!(
        verify_build(&extra)
            .unwrap_err()
            .to_string()
            .contains("extra file")
    );

    let manifest = repository.temporary.path().join("manifest-tamper");
    copy_product(&repository.build_dir, &manifest);
    let manifest_path = manifest.join("manifest.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    json["artifacts"][0]["owner"] = serde_json::Value::String("intruder".to_string());
    fs::write(&manifest_path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    assert!(
        verify_build(&manifest)
            .unwrap_err()
            .to_string()
            .contains("build ID mismatch")
    );
}

#[test]
fn verifier_rejects_legacy_unknown_v17_fields_and_internally_inconsistent_provenance() {
    let repository = Repository::new();
    repository.build().unwrap();

    let legacy = repository.temporary.path().join("legacy");
    copy_product(&repository.build_dir, &legacy);
    let legacy_manifest = legacy.join("manifest.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&legacy_manifest).unwrap()).unwrap();
    json["format_version"] = serde_json::Value::from(10);
    fs::write(&legacy_manifest, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    let error = verify_build(&legacy).unwrap_err().to_string();
    assert!(
        error.contains("unsupported manifest format version 10"),
        "{error}"
    );

    let unknown = repository.temporary.path().join("unknown-v15-field");
    copy_product(&repository.build_dir, &unknown);
    let unknown_manifest = unknown.join("manifest.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&unknown_manifest).unwrap()).unwrap();
    json["target"]["unknown"] = serde_json::Value::Bool(true);
    fs::write(&unknown_manifest, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    let error = verify_build(&unknown).unwrap_err().to_string();
    assert!(error.contains("unknown field `unknown`"), "{error}");

    let provenance = repository.temporary.path().join("provenance");
    copy_product(&repository.build_dir, &provenance);
    let provenance_manifest = provenance.join("manifest.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&provenance_manifest).unwrap()).unwrap();
    json["artifacts"][0]["source_origin"]["declared"] =
        serde_json::Value::String("different.toml".to_string());
    fs::write(
        &provenance_manifest,
        serde_json::to_vec_pretty(&json).unwrap(),
    )
    .unwrap();
    let error = verify_build(&provenance).unwrap_err().to_string();
    assert!(error.contains("build ID mismatch"), "{error}");

    let uncatalogued = repository.temporary.path().join("uncatalogued-source");
    copy_product(&repository.build_dir, &uncatalogued);
    let uncatalogued_manifest = uncatalogued.join("manifest.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&uncatalogued_manifest).unwrap()).unwrap();
    json["artifacts"][0]["declared_at"]["primary"]["source"] =
        serde_json::Value::String("lua/missing.lua".to_string());
    fs::write(
        &uncatalogued_manifest,
        serde_json::to_vec_pretty(&json).unwrap(),
    )
    .unwrap();
    let error = verify_build(&uncatalogued).unwrap_err().to_string();
    assert!(
        error.contains("uncatalogued source `lua/missing.lua`"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn verifier_rejects_symlinks_non_utf8_entries_and_wrong_modes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};

    let repository = Repository::new();
    repository.build().unwrap();

    let linked = repository.temporary.path().join("linked");
    copy_product(&repository.build_dir, &linked);
    fs::remove_file(linked.join("tree/.config/app.toml")).unwrap();
    symlink(
        repository.root.join("src/dot_config/app.toml"),
        linked.join("tree/.config/app.toml"),
    )
    .unwrap();
    assert!(
        verify_build(&linked)
            .unwrap_err()
            .to_string()
            .contains("symbolic link")
    );

    let linked_manifest = repository.temporary.path().join("linked-manifest");
    copy_product(&repository.build_dir, &linked_manifest);
    fs::rename(
        linked_manifest.join("manifest.json"),
        linked_manifest.join("actual-manifest.json"),
    )
    .unwrap();
    symlink(
        linked_manifest.join("actual-manifest.json"),
        linked_manifest.join("manifest.json"),
    )
    .unwrap();
    assert!(
        verify_build(&linked_manifest)
            .unwrap_err()
            .to_string()
            .contains("regular non-symlink file")
    );

    let invalid = repository.temporary.path().join("invalid-name");
    copy_product(&repository.build_dir, &invalid);
    let invalid_name_result = fs::write(
        invalid
            .join("tree/.config")
            .join(OsString::from_vec(vec![b'x', 0x80])),
        "invalid\n",
    );
    match invalid_name_result {
        Ok(()) => assert!(
            verify_build(&invalid)
                .unwrap_err()
                .to_string()
                .contains("valid UTF-8")
        ),
        // macOS filesystems reject invalid UTF-8 names before Wombat can see them.
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {}
        Err(error) => panic!("failed to create invalid UTF-8 test entry: {error}"),
    }

    let mode = repository.temporary.path().join("mode");
    copy_product(&repository.build_dir, &mode);
    fs::set_permissions(
        mode.join("tree/.config/app.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(
        verify_build(&mode)
            .unwrap_err()
            .to_string()
            .contains("mode")
    );
}

#[cfg(unix)]
#[test]
fn workspace_internal_symlinks_are_refused_without_traversal() {
    use std::os::unix::fs::symlink;

    let repository = Repository::new();
    let external = repository.temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("mine"), "preserve me\n").unwrap();

    let disguised = repository.temporary.path().join("internal-link");
    fs::create_dir(&disguised).unwrap();
    symlink(&external, disguised.join(".wombat")).unwrap();
    let error = build(BuildOptions::new(&repository.root, &disguised))
        .unwrap_err()
        .to_string();
    assert!(error.contains("non-symlink directory"), "{error}");
    assert_eq!(
        fs::read_to_string(external.join("mine")).unwrap(),
        "preserve me\n"
    );

    repository.build().unwrap();
    let lock = repository.build_dir.join(".wombat/lock");
    fs::remove_file(&lock).unwrap();
    symlink(external.join("mine"), &lock).unwrap();
    let error = verify_build(&repository.build_dir).unwrap_err().to_string();
    assert!(error.contains("regular non-symlink file"), "{error}");
    fs::remove_file(&lock).unwrap();
    fs::write(&lock, []).unwrap();

    let staging = repository.build_dir.join(".wombat/staging");
    fs::remove_dir(&staging).unwrap();
    symlink(&external, &staging).unwrap();
    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("non-symlink directory"), "{error}");
    assert_eq!(
        fs::read_to_string(external.join("mine")).unwrap(),
        "preserve me\n"
    );
}

#[test]
fn workspace_refuses_unsafe_ownership_and_source_mismatch() {
    let repository = Repository::new();
    let unmarked = repository.temporary.path().join("unmarked");
    fs::create_dir(&unmarked).unwrap();
    fs::write(unmarked.join("mine"), "do not replace\n").unwrap();
    let error = build(BuildOptions::new(&repository.root, &unmarked))
        .unwrap_err()
        .to_string();
    assert!(error.contains("nonempty unmarked"), "{error}");
    assert_eq!(
        fs::read_to_string(unmarked.join("mine")).unwrap(),
        "do not replace\n"
    );

    let disguised = repository.temporary.path().join("disguised");
    fs::create_dir_all(disguised.join(".wombat")).unwrap();
    fs::write(disguised.join(".wombat/mine"), "do not replace\n").unwrap();
    let error = build(BuildOptions::new(&repository.root, &disguised))
        .unwrap_err()
        .to_string();
    assert!(error.contains("nonempty unmarked"), "{error}");
    assert_eq!(
        fs::read_to_string(disguised.join(".wombat/mine")).unwrap(),
        "do not replace\n"
    );

    for unsafe_path in [
        repository.root.clone(),
        repository.temporary.path().to_path_buf(),
        repository.root.join("src/dot_config/build"),
        repository.root.join("dot_local/build"),
        repository.root.join("modules/build"),
    ] {
        let error = build(BuildOptions::new(&repository.root, unsafe_path))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("must not") || error.contains("artifact roots"),
            "{error}"
        );
    }

    repository.build().unwrap();
    let other = repository.temporary.path().join("other-source");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("wombat.lua"), "return true\n").unwrap();
    let mismatch = build(BuildOptions::new(&other, &repository.build_dir))
        .unwrap_err()
        .to_string();
    assert!(mismatch.contains("belongs to source"), "{mismatch}");
}

#[test]
fn concurrent_build_lock_is_fail_fast() {
    let repository = Repository::new();
    repository.build().unwrap();
    let lock_path = repository.build_dir.join(".wombat/lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    lock.try_lock().unwrap();
    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("in use by another process"), "{error}");
}

#[test]
fn interrupted_publication_recovers_the_previous_product() {
    let repository = Repository::new();
    let first = repository.build().unwrap();
    let rollback = repository.build_dir.join(".wombat/rollback");
    fs::create_dir(&rollback).unwrap();
    fs::rename(
        repository.build_dir.join("manifest.json"),
        rollback.join("manifest.json"),
    )
    .unwrap();
    fs::rename(repository.build_dir.join("tree"), rollback.join("tree")).unwrap();
    fs::create_dir(repository.build_dir.join("tree")).unwrap();
    fs::create_dir(repository.build_dir.join("tree/home")).unwrap();
    fs::create_dir(repository.build_dir.join("tree/config")).unwrap();

    let recovered = repository.build().unwrap();
    assert_eq!(recovered.status, BuildStatus::Unchanged);
    assert_eq!(recovered.build_id, first.build_id);
    assert!(!rollback.exists());
    assert!(verify_build(&repository.build_dir).is_ok());
}

/// Publication backs up and restores `scripts/`, so recovery has to clear it
/// first. Renaming a directory onto a non-empty one is ENOTEMPTY, which would
/// leave the rollback stranded and the next run hitting the same failure.
#[test]
fn interrupted_publication_recovers_a_product_that_has_scripts() {
    let repository = Repository::new();
    fs::create_dir_all(repository.root.join("scripts")).unwrap();
    fs::write(repository.root.join("scripts/mark.sh"), "exit 0\n").unwrap();
    fs::write(
        repository.root.join("wombat.lua"),
        "local w = require(\"wombat\")\nw.use(\"app\")\nw.use(\"shell\")\nw.script(\"mark.sh\")\n",
    )
    .unwrap();
    let state = repository.temporary.path().join("script-state");
    let first = build(
        BuildOptions::new(&repository.root, &repository.build_dir).with_script_state_root(&state),
    )
    .unwrap();
    assert!(repository.build_dir.join("scripts").exists());

    // Stage the directory an interrupted publication leaves behind: the previous
    // product under `rollback`, and a partially published one still in place.
    let rollback = repository.build_dir.join(".wombat/rollback");
    fs::create_dir(&rollback).unwrap();
    for name in ["manifest.json", "tree", "scripts"] {
        fs::rename(repository.build_dir.join(name), rollback.join(name)).unwrap();
    }
    fs::create_dir(repository.build_dir.join("tree")).unwrap();
    fs::create_dir(repository.build_dir.join("scripts")).unwrap();
    fs::write(repository.build_dir.join("scripts/stale.sh"), "exit 1\n").unwrap();

    let recovered = build(
        BuildOptions::new(&repository.root, &repository.build_dir).with_script_state_root(&state),
    )
    .unwrap();
    assert_eq!(recovered.build_id, first.build_id);
    assert!(!rollback.exists());
    assert!(!repository.build_dir.join("scripts/stale.sh").exists());
    assert!(verify_build(&repository.build_dir).is_ok());
}

#[test]
fn initialized_workspace_preserves_unrelated_top_level_files() {
    let repository = Repository::new();
    repository.build().unwrap();
    fs::write(repository.build_dir.join("notes.txt"), "user-owned note\n").unwrap();
    fs::write(
        repository.root.join("src/dot_config/app.toml"),
        "updated = true\n",
    )
    .unwrap();
    repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.build_dir.join("notes.txt")).unwrap(),
        "user-owned note\n"
    );
}
