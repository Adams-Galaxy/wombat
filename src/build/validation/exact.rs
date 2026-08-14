//! Exact verification of published artifact and provider-payload trees.

use std::fs::File;

use super::*;

pub(super) fn verify_tree(tree: &Path, manifest: &Manifest) -> Result<()> {
    let mut expected_files = BTreeMap::new();
    let mut expected_dirs = BTreeSet::new();
    for artifact in &manifest.artifacts {
        let relative = artifact.target.path.clone();
        if expected_files.insert(relative.clone(), artifact).is_some() {
            return Err(WombatError::configuration(format!(
                "manifest contains duplicate tree path `{relative}`"
            )));
        }
        let mut parent = Path::new(&relative).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            expected_dirs.insert(path.to_string_lossy().replace('\\', "/"));
            parent = path.parent();
        }
    }
    let metadata = fs::symlink_metadata(tree).map_err(|error| WombatError::io(tree, error))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(WombatError::configuration(format!(
            "build tree `{}` must be a non-symlink directory",
            tree.display()
        )));
    }
    let mut seen_files = BTreeSet::new();
    let mut seen_dirs = BTreeSet::new();
    walk_tree(
        tree,
        tree,
        &expected_files,
        &expected_dirs,
        &mut seen_files,
        &mut seen_dirs,
    )?;
    if seen_files.len() != expected_files.len() || seen_dirs != expected_dirs {
        return Err(WombatError::configuration(
            "build tree is missing manifest-required entries",
        ));
    }
    Ok(())
}

pub(super) fn verify_provider_payloads(root: &Path, manifest: &Manifest) -> Result<()> {
    let providers_root = root.join("providers");
    let mut expected = BTreeMap::new();
    for provider in &manifest.providers {
        if let crate::model::manifest::ProviderOrigin::Custom { files, .. } = &provider.origin {
            for file in files {
                expected.insert(file.payload.as_str(), file);
            }
        }
    }
    if expected.is_empty() {
        if providers_root
            .try_exists()
            .map_err(|error| WombatError::io(&providers_root, error))?
        {
            return Err(WombatError::configuration(
                "build product contains an unexpected provider payload tree",
            ));
        }
        return Ok(());
    }
    ensure_plain_directory(&providers_root)?;
    let mut seen = BTreeSet::new();
    verify_provider_directory(&providers_root, &providers_root, &expected, &mut seen)?;
    if seen.len() != expected.len() {
        return Err(WombatError::configuration(
            "provider payload tree is missing manifest-required files",
        ));
    }
    Ok(())
}

fn verify_provider_directory<'a>(
    root: &Path,
    directory: &Path,
    expected: &BTreeMap<&'a str, &'a crate::model::manifest::ProviderFile>,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| WombatError::io(directory, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("provider payload remains under its root")
            .to_str()
            .ok_or_else(|| WombatError::configuration("provider payload path is not UTF-8"))?
            .replace('\\', "/");
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "provider payload `{relative}` must not be a symbolic link"
            )));
        }
        if metadata.file_type().is_dir() {
            let prefix = format!("{relative}/");
            if !expected
                .keys()
                .any(|candidate| candidate.starts_with(&prefix))
            {
                return Err(WombatError::configuration(format!(
                    "provider payload tree contains extra directory `{relative}`"
                )));
            }
            verify_provider_directory(root, &path, expected, seen)?;
        } else if metadata.file_type().is_file() {
            let file = expected.get(relative.as_str()).ok_or_else(|| {
                WombatError::configuration(format!(
                    "provider payload tree contains extra file `{relative}`"
                ))
            })?;
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            if u64::try_from(bytes.len()).ok() != Some(file.size)
                || digest_string(Sha256::digest(&bytes)) != file.digest
                || executable_intent(&metadata)
            {
                return Err(WombatError::configuration(format!(
                    "provider payload `{relative}` does not match its manifest identity"
                )));
            }
            seen.insert(relative);
        } else {
            return Err(WombatError::configuration(format!(
                "provider payload `{relative}` must be a regular file or directory"
            )));
        }
    }
    Ok(())
}

fn walk_tree(
    root: &Path,
    directory: &Path,
    expected_files: &BTreeMap<String, &Artifact>,
    expected_dirs: &BTreeSet<String>,
    seen_files: &mut BTreeSet<String>,
    seen_dirs: &mut BTreeSet<String>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| WombatError::io(directory, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walked entries remain under root");
        let relative = relative
            .to_str()
            .ok_or_else(|| {
                WombatError::configuration(format!(
                    "build tree entry `{}` is not valid UTF-8",
                    path.display()
                ))
            })?
            .replace('\\', "/");
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "build tree entry `{relative}` must not be a symbolic link"
            )));
        }
        if metadata.file_type().is_dir() {
            if !expected_dirs.contains(&relative) {
                return Err(WombatError::configuration(format!(
                    "build tree contains extra directory `{relative}`"
                )));
            }
            seen_dirs.insert(relative);
            walk_tree(
                root,
                &path,
                expected_files,
                expected_dirs,
                seen_files,
                seen_dirs,
            )?;
        } else if metadata.file_type().is_file() {
            let artifact = expected_files.get(&relative).ok_or_else(|| {
                WombatError::configuration(format!("build tree contains extra file `{relative}`"))
            })?;
            verify_file(&path, artifact)?;
            seen_files.insert(relative);
        } else {
            return Err(WombatError::configuration(format!(
                "build tree entry `{relative}` is not a regular file or directory"
            )));
        }
    }
    Ok(())
}

fn verify_file(path: &Path, artifact: &Artifact) -> Result<()> {
    let mut file = File::open(path).map_err(|error| WombatError::io(path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| WombatError::io(path, error))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| WombatError::io(path, error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).expect("buffer lengths fit in u64"))
            .ok_or_else(|| WombatError::configuration("artifact size exceeds u64"))?;
    }
    let digest = digest_string(hasher.finalize());
    if size != artifact.content.size || digest != artifact.content.digest {
        return Err(WombatError::configuration(format!(
            "build tree file `{}` does not match its manifest content identity",
            path.display()
        )));
    }
    if executable_intent(&metadata) != artifact.content.executable {
        return Err(WombatError::configuration(format!(
            "build tree file `{}` has incorrect executable intent",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let expected = if artifact.content.executable {
            0o755
        } else {
            0o644
        };
        if metadata.permissions().mode() & 0o777 != expected {
            return Err(WombatError::configuration(format!(
                "build tree file `{}` has mode {:o}, expected {expected:o}",
                path.display(),
                metadata.permissions().mode() & 0o777
            )));
        }
    }
    Ok(())
}
