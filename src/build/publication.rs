//! Transactional publication, rollback recovery, and interruption tests.
//!
//! Publishing swaps a staged product into the build directory. The ordering
//! here is the whole point: a build that is interrupted — power loss, a killed
//! process, a full disk — must leave either the previous verified product or a
//! state the next run can recover, never a directory that looks like a finished
//! product but is not one.
//!
//! Two rules produce that. The previous product moves aside into `.wombat/
//! rollback` before anything new lands, and `manifest.json` is published last.
//! The manifest is what makes a directory a product, so until it is in place a
//! half-published tree is simply not a product yet. `recover_publication` runs
//! at the start of the next build and decides, from what survived, which side of
//! the swap to keep.

#[cfg(test)]
use super::materialisation::{
    MaterialisationPoint, copy_and_hash_with_hook, materialise_with_hook,
};
use super::validation::verify_product;
use super::*;

/// Classifies what is sitting in a build directory before we touch it.
///
/// A directory with neither manifest nor tree is empty rather than broken, which
/// is the ordinary first-build case. Anything else has to verify: a product that
/// fails verification is `Invalid` and will be replaced, not trusted.
pub(super) fn inspect_product(root: &Path) -> CurrentProduct {
    let manifest = root.join("manifest.json");
    let tree = root.join("tree");
    let manifest_exists = manifest.try_exists().unwrap_or(false);
    let tree_exists = tree.try_exists().unwrap_or(false);
    if !manifest_exists && !tree_exists {
        CurrentProduct::Missing
    } else {
        match verify_product(root) {
            Ok(manifest) => CurrentProduct::Valid(Box::new(manifest)),
            Err(_) => CurrentProduct::Invalid,
        }
    }
}

/// Repairs a build directory left mid-swap by an interrupted publication.
///
/// A surviving `.wombat/rollback` means a previous run was interrupted between
/// backing the old product up and finishing the new one. Which side to keep is
/// decided by verification rather than by guessing how far the swap got:
///
/// - the current product verifies, so the swap completed and the backup is
///   stale;
/// - it does not, but the backup does, so restore the backup;
/// - neither verifies, so drop the backup and let the caller rebuild.
///
/// Called before publication, so every build starts from a coherent directory.
pub(super) fn recover_publication(build_dir: &Path) -> Result<()> {
    let rollback = build_dir.join(".wombat/rollback");
    if !rollback
        .try_exists()
        .map_err(|error| WombatError::io(&rollback, error))?
    {
        return Ok(());
    }
    ensure_plain_directory(&rollback)?;
    if verify_product(build_dir).is_ok() {
        remove_entry(&rollback)?;
        return Ok(());
    }
    if verify_product(&rollback).is_ok() {
        remove_reserved_product(build_dir)?;
        restore_rollback(build_dir, &rollback)?;
    } else {
        remove_entry(&rollback)?;
    }
    Ok(())
}

pub(super) fn publish(build_dir: &Path, staged: &Path) -> Result<()> {
    publish_with_hook(build_dir, staged, |_| Ok(()))
}

/// Points at which a test can interrupt publication.
///
/// These exist so the recovery paths above are exercised for real — a test
/// stops the swap at each step and asserts the directory is still recoverable —
/// rather than being reasoned about and hoped for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationStep {
    BeforeBackup,
    PreviousBackedUp,
    TreePublished,
    ManifestPublished,
}

fn publish_with_hook(
    build_dir: &Path,
    staged: &Path,
    mut after_step: impl FnMut(PublicationStep) -> Result<()>,
) -> Result<()> {
    let rollback = build_dir.join(".wombat/rollback");
    remove_entry_if_exists(&rollback)?;
    fs::create_dir(&rollback).map_err(|error| WombatError::io(&rollback, error))?;
    // Tracks whether the build directory still holds the previous product
    // untouched. While it does, failing just means deleting the backup; once we
    // have started moving things, failing means putting the old product back.
    let mut product_was_mutated = false;
    let result = (|| {
        after_step(PublicationStep::BeforeBackup)?;
        // Move the previous product aside rather than deleting it. Renames
        // within the same directory are atomic, so an interruption here leaves
        // the old product intact under `rollback` for the next run to find.
        for name in ["tree", "providers", "scripts", "manifest.json"] {
            let current = build_dir.join(name);
            if current
                .try_exists()
                .map_err(|error| WombatError::io(&current, error))?
            {
                fs::rename(&current, rollback.join(name))
                    .map_err(|error| WombatError::io(&current, error))?;
                product_was_mutated = true;
            }
        }
        after_step(PublicationStep::PreviousBackedUp)?;
        fs::rename(staged.join("tree"), build_dir.join("tree"))
            .map_err(|error| WombatError::io(build_dir.join("tree"), error))?;
        let staged_providers = staged.join("providers");
        if staged_providers
            .try_exists()
            .map_err(|error| WombatError::io(&staged_providers, error))?
        {
            fs::rename(&staged_providers, build_dir.join("providers"))
                .map_err(|error| WombatError::io(build_dir.join("providers"), error))?;
        }
        let staged_scripts = staged.join("scripts");
        if staged_scripts
            .try_exists()
            .map_err(|error| WombatError::io(&staged_scripts, error))?
        {
            fs::rename(&staged_scripts, build_dir.join("scripts"))
                .map_err(|error| WombatError::io(build_dir.join("scripts"), error))?;
        }
        product_was_mutated = true;
        after_step(PublicationStep::TreePublished)?;
        // The manifest goes last, and only after the tree is fully in place. It
        // is what makes this directory a product, so publishing it earlier would
        // let an interruption leave a manifest describing a tree that is not
        // there yet.
        fs::rename(
            staged.join("manifest.json"),
            build_dir.join("manifest.json"),
        )
        .map_err(|error| WombatError::io(build_dir.join("manifest.json"), error))?;
        after_step(PublicationStep::ManifestPublished)?;
        verify_product(build_dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        // Clear whatever we managed to publish before restoring, so the restore
        // renames land on empty paths instead of failing halfway and leaving
        // both products partially present.
        if product_was_mutated {
            remove_reserved_product(build_dir)?;
            restore_rollback(build_dir, &rollback)?;
        } else {
            remove_entry(&rollback)?;
        }
        return Err(error);
    }
    remove_entry(&rollback)
}

/// Puts a backed-up product back, in the same order publication used.
///
/// Callers must clear the reserved paths first; these renames expect their
/// destinations to be free.
fn restore_rollback(build_dir: &Path, rollback: &Path) -> Result<()> {
    for name in ["tree", "providers", "scripts", "manifest.json"] {
        let source = rollback.join(name);
        if source
            .try_exists()
            .map_err(|error| WombatError::io(&source, error))?
        {
            fs::rename(&source, build_dir.join(name))
                .map_err(|error| WombatError::io(&source, error))?;
        }
    }
    remove_entry_if_exists(rollback)
}

/// Clears the paths publication owns, leaving everything else in the build
/// directory alone. Callers run this before [`restore_rollback`], whose renames
/// need free destinations.
fn remove_reserved_product(build_dir: &Path) -> Result<()> {
    remove_entry_if_exists(&build_dir.join("manifest.json"))?;
    remove_entry_if_exists(&build_dir.join("tree"))?;
    remove_entry_if_exists(&build_dir.join("providers"))?;
    remove_entry_if_exists(&build_dir.join("scripts"))
}

pub(super) fn clear_directory_contents(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(|error| WombatError::io(directory, error))? {
        let entry = entry.map_err(|error| WombatError::io(directory, error))?;
        remove_entry(&entry.path())?;
    }
    Ok(())
}

/// Requires a real directory, creating it when absent.
///
/// Symlinks are refused throughout publication: following one would let a
/// crafted or careless workspace redirect writes outside the build directory.
pub(super) fn ensure_plain_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(WombatError::configuration(format!(
            "workspace path `{}` must be a non-symlink directory",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| WombatError::io(path, error))
        }
        Err(error) => Err(WombatError::io(path, error)),
    }
}

pub(super) fn ensure_plain_file_or_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => ensure_plain_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

pub(super) fn ensure_plain_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(WombatError::configuration(format!(
            "workspace path `{}` must be a regular non-symlink file",
            path.display()
        )))
    }
}

fn remove_entry_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => remove_entry(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

fn remove_entry(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|error| WombatError::io(path, error))
    } else {
        fs::remove_file(path).map_err(|error| WombatError::io(path, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::evaluate;

    fn repository(root: &Path) {
        fs::create_dir_all(root.join("modules")).unwrap();
        fs::create_dir_all(root.join("src/dot_config")).unwrap();
        fs::write(
            root.join("wombat.lua"),
            "local w = require(\"wombat\")\nw.use(\"app\")\n",
        )
        .unwrap();
        fs::write(
            root.join("modules/app.lua"),
            "local w = require(\"wombat\")\nw.module.from(\".config\")\nw.install(\"app.toml\")\n",
        )
        .unwrap();
        fs::write(root.join("src/dot_config/app.toml"), "version = 1\n").unwrap();
    }

    #[test]
    fn source_mutation_during_copy_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::write(&source, "before\n").unwrap();
        let expected = fingerprint_regular_file(&source).unwrap();

        let error = copy_and_hash_with_hook(&source, &destination, &expected, || {
            fs::write(&source, "changed while materialising\n").unwrap();
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("changed during materialisation"), "{error}");
    }

    #[test]
    fn template_source_mutation_before_final_validation_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("repository");
        let staged = temporary.path().join("staged");
        fs::create_dir_all(source.join("modules")).unwrap();
        fs::create_dir_all(source.join("src/dot_config")).unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(
            source.join("wombat.lua"),
            "local w = require('wombat')\nw.use('app')\n",
        )
        .unwrap();
        fs::write(
            source.join("modules/app.lua"),
            "local w = require('wombat')\nw.module.from('.config')\nw.install('app.tmpl', { with = { value = 'before' } })\n",
        )
        .unwrap();
        let template = source.join("src/dot_config/app.tmpl");
        fs::write(&template, "{{ value }}\n").unwrap();
        let desired = evaluate(&source).unwrap();

        let error = materialise_with_hook(&source, &staged, desired, |point| {
            if point == MaterialisationPoint::BeforeFinalValidation {
                fs::write(&template, "changed {{ value }}\n").unwrap();
            }
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("changed during materialisation"), "{error}");
    }

    #[test]
    fn lua_source_mutation_before_final_validation_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("repository");
        let staged = temporary.path().join("staged");
        repository(&source);
        fs::create_dir(&staged).unwrap();
        let desired = evaluate(&source).unwrap();
        let module = source.join("modules/app.lua");

        let error = materialise_with_hook(&source, &staged, desired, |point| {
            if point == MaterialisationPoint::BeforeFinalValidation {
                fs::write(
                    &module,
                    "-- changed\nlocal w = require(\"wombat\")\nw.module.from(\".config\")\nw.install(\"app.toml\")\n",
                )
                .unwrap();
            }
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Lua source") && error.contains("changed"),
            "{error}"
        );
    }

    #[test]
    fn final_directory_rewalk_rejects_every_membership_and_metadata_change() {
        #[derive(Clone, Copy, Debug)]
        enum Mutation {
            Content,
            Add,
            Remove,
            Rename,
            Type,
            #[cfg(unix)]
            Mode,
        }

        let mutations = [
            Mutation::Content,
            Mutation::Add,
            Mutation::Remove,
            Mutation::Rename,
            Mutation::Type,
            #[cfg(unix)]
            Mutation::Mode,
        ];
        for mutation in mutations {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("repository");
            let current = temporary.path().join("current");
            fs::create_dir_all(source.join("modules")).unwrap();
            fs::create_dir_all(source.join("src/dot_config/tree")).unwrap();
            fs::write(
                source.join("wombat.lua"),
                "local w = require(\"wombat\")\nw.use(\"tree\")\n",
            )
            .unwrap();
            fs::write(
                source.join("modules/tree.lua"),
                "local w = require(\"wombat\")\nw.module.from(\".config\")\nw.install(\"tree\")\n",
            )
            .unwrap();
            let leaf = source.join("src/dot_config/tree/file");
            fs::write(&leaf, "before\n").unwrap();
            let previous = build(BuildOptions::new(&source, &current)).unwrap();
            let desired = evaluate(&source).unwrap();
            let staged = temporary.path().join("staged");
            fs::create_dir(&staged).unwrap();

            let error = materialise_with_hook(&source, &staged, desired, |point| {
                if point != MaterialisationPoint::BeforeFinalValidation {
                    return;
                }
                match mutation {
                    Mutation::Content => fs::write(&leaf, "changed\n").unwrap(),
                    Mutation::Add => {
                        fs::write(source.join("src/dot_config/tree/added"), "added\n").unwrap();
                    }
                    Mutation::Remove => fs::remove_file(&leaf).unwrap(),
                    Mutation::Rename => {
                        fs::rename(&leaf, source.join("src/dot_config/tree/renamed")).unwrap();
                    }
                    Mutation::Type => {
                        fs::remove_file(&leaf).unwrap();
                        fs::create_dir(&leaf).unwrap();
                    }
                    #[cfg(unix)]
                    Mutation::Mode => {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o755)).unwrap();
                    }
                }
            })
            .unwrap_err()
            .to_string();

            assert!(
                error.contains("changed during materialisation")
                    || error.contains("No such file")
                    || error.contains("not a regular file"),
                "{mutation:?}: {error}"
            );
            let verified = verify_build(&current).unwrap();
            assert_eq!(verified.manifest.build_id, previous.build_id);
        }
    }

    #[test]
    fn every_publication_transition_restores_the_previous_product_on_failure() {
        for failure_step in [
            PublicationStep::BeforeBackup,
            PublicationStep::PreviousBackedUp,
            PublicationStep::TreePublished,
            PublicationStep::ManifestPublished,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("repository");
            let current = temporary.path().join("current");
            let staged = temporary.path().join("staged");
            repository(&source);
            let previous = build(BuildOptions::new(&source, &current)).unwrap();
            fs::write(source.join("src/dot_config/app.toml"), "version = 2\n").unwrap();
            let replacement = build(BuildOptions::new(&source, &staged)).unwrap();
            assert_ne!(previous.build_id, replacement.build_id);

            let error = publish_with_hook(&current, &staged, |step| {
                if step == failure_step {
                    Err(WombatError::configuration(format!(
                        "injected failure after {step:?}"
                    )))
                } else {
                    Ok(())
                }
            })
            .unwrap_err()
            .to_string();

            assert!(error.contains("injected failure"), "{error}");
            let restored = verify_product(&current).unwrap();
            assert_eq!(restored.build_id, previous.build_id);
            assert!(!current.join(".wombat/rollback").exists());
        }
    }
}
