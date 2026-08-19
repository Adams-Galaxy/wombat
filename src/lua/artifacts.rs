//! Source selection, projection, and artifact registration.

use super::*;

pub(super) struct ArtifactDeclaration<'a> {
    pub(super) source_path: &'a str,
    pub(super) hidden: bool,
    pub(super) explicit_target: Option<&'a str>,
    pub(super) requested_kind: &'a str,
    pub(super) context: Value,
    pub(super) exclusions: Vec<String>,
    pub(super) allow_empty: bool,
    pub(super) location: Location,
}

pub(super) fn register_artifact(
    state: &Rc<RefCell<RuntimeState>>,
    declaration: ArtifactDeclaration<'_>,
) -> Result<()> {
    let ArtifactDeclaration {
        source_path,
        hidden,
        explicit_target,
        requested_kind,
        context,
        exclusions,
        allow_empty,
        location,
    } = declaration;
    if !matches!(requested_kind, "auto" | "file" | "template") {
        return Err(WombatError::configuration(format!(
            "unsupported artifact production kind `{requested_kind}`"
        )));
    }

    let mut selector = compile_selector(source_path, hidden)?;
    let exclusion_matchers = exclusions
        .iter()
        .map(|value| compile_selector(value, hidden).and_then(|value| matcher(&value.physical)))
        .collect::<Result<Vec<_>>>()?;
    let mut state = state.borrow_mut();
    let repository_root = state.root.clone();
    let (source_base, base_logical, base_target, base_hidden) = state.active_location();
    let owner = state.active_module().unwrap_or(ROOT_MODULE).to_string();
    if let Some(module) = state.active_module().map(str::to_string) {
        state
            .modules
            .get_mut(&module)
            .expect("active module exists")
            .declarations_started = true;
    }
    let mut absolute_selection = if selector.physical == "." {
        source_base.clone()
    } else {
        source_base.join(&selector.physical)
    };
    if !selector.glob && !selector.physical.ends_with(".tmpl") {
        let template_physical = format!("{}.tmpl", selector.physical);
        let template_selection = source_base.join(&template_physical);
        let exact_metadata = fs::symlink_metadata(&absolute_selection);
        let template_metadata = fs::symlink_metadata(&template_selection);
        match (&exact_metadata, &template_metadata) {
            (Ok(exact), Ok(template))
                if exact.file_type().is_file() && template.file_type().is_file() =>
            {
                return Err(WombatError::configuration(format!(
                    "artifact source `{source_path}` is ambiguous: both `{}` and `{}` exist; name the physical `.tmpl` source explicitly or remove one candidate",
                    display_path(&repository_root, &absolute_selection),
                    display_path(&repository_root, &template_selection),
                )));
            }
            (Err(error), Ok(template))
                if error.kind() == std::io::ErrorKind::NotFound
                    && template.file_type().is_file() =>
            {
                selector.physical = template_physical;
                selector.expanded.push_str(".tmpl");
                absolute_selection = template_selection;
            }
            _ => {}
        }
    }
    let mut selected = Vec::new();
    let mut selected_snapshot = None;
    let mut selected_snapshot_root = source_base.clone();
    let directory_selector = !selector.glob && absolute_selection.is_dir();
    let set_selector = selector.glob || directory_selector;
    if directory_selector && requested_kind != "auto" {
        return Err(WombatError::configuration(format!(
            "install.{requested_kind}() cannot select a directory; use install() for directory selection"
        )));
    }
    if selector.glob {
        let selector_matcher = matcher(&selector.physical)?;
        let snapshot = snapshot_directory_filtered(
            &repository_root,
            &source_base,
            |relative, is_directory| {
                in_static_scope(relative, &selector.static_root)
                    && hidden_components_authorized(relative, &selector.physical)
                    && !is_excluded(&exclusion_matchers, relative, is_directory)
            },
        )?;
        for leaf in &snapshot {
            if selector_matcher.is_match(&leaf.relative) {
                selected.push((leaf.relative.clone(), leaf.fingerprint.clone()));
            }
        }
        selected_snapshot = Some(snapshot);
    } else if absolute_selection.is_dir() {
        let snapshot = snapshot_directory_filtered(
            &repository_root,
            &absolute_selection,
            |relative, is_directory| {
                !relative
                    .split('/')
                    .any(crate::model::selection::is_hidden_component)
                    && !is_excluded(&exclusion_matchers, relative, is_directory)
            },
        )?;
        for leaf in &snapshot {
            let relative = if selector.physical == "." {
                leaf.relative.clone()
            } else {
                format!("{}/{}", selector.physical, leaf.relative)
            };
            selected.push((relative, leaf.fingerprint.clone()));
        }
        selected_snapshot_root = absolute_selection.clone();
        selected_snapshot = Some(snapshot);
    } else {
        if !exclusions.is_empty() || allow_empty {
            return Err(WombatError::configuration(
                "`exclude` and `allow_empty` are only valid for directory or glob selectors",
            ));
        }
        let metadata = fs::symlink_metadata(&absolute_selection).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WombatError::configuration(format!(
                    "artifact source `{source_path}` does not exist beneath its declaration base"
                ))
            } else {
                WombatError::io(&absolute_selection, error)
            }
        })?;
        if !metadata.file_type().is_file() {
            return Err(WombatError::configuration(format!(
                "artifact source `{source_path}` must be a regular file or directory"
            )));
        }
        selected.push((
            selector.physical.clone(),
            SourceFingerprint::from_metadata(&metadata),
        ));
    }
    if selected.is_empty() && !(set_selector && allow_empty) {
        return Err(WombatError::configuration(format!(
            "artifact selector `{source_path}` matched no files; set `allow_empty = true` if this is intentional"
        )));
    }
    let context = FrozenValue::from_lua(context)?;
    if !matches!(context, FrozenValue::Null | FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "template `with` context must be a string-keyed map",
        ));
    }
    let explicit_root = explicit_target
        .map(crate::model::path::parse_install_target_root)
        .transpose()?;
    let selection_root = if selector.glob {
        selector.static_root.trim_end_matches('/').to_string()
    } else if set_selector {
        selector.physical.clone()
    } else {
        selector
            .physical
            .rsplit_once('/')
            .map_or("", |(root, _)| root)
            .to_string()
    };
    let mut skipped = Vec::new();
    let mut matched = Vec::new();
    for (relative, fingerprint) in selected {
        let hidden_authorized = hidden_components_authorized(&relative, &selector.physical);
        if relative
            .split('/')
            .any(crate::model::selection::is_hidden_component)
            && !hidden_authorized
        {
            continue;
        }
        let mut projection = project_physical(&relative, hidden_authorized)?;
        let relative_from_root = relative
            .strip_prefix(&selection_root)
            .unwrap_or(&relative)
            .trim_start_matches('/');
        let relative_projection = if relative_from_root.is_empty() {
            projection.clone()
        } else {
            project_physical(
                relative_from_root,
                hidden_components_authorized(relative_from_root, &selector.physical),
            )?
        };
        let projected_relative = relative_projection.logical.clone();
        let target_path = if !set_selector {
            explicit_target.map(str::to_string).or_else(|| {
                projection
                    .allocated
                    .then(|| {
                        base_target.as_ref().map(|base| {
                            crate::model::path::join_relative(base, &projection.logical)
                        })
                    })
                    .flatten()
            })
        } else if let Some(root) = &explicit_root {
            relative_projection
                .allocated
                .then(|| crate::model::path::join_relative(&root.path, &projected_relative))
        } else if projection.allocated {
            base_target
                .as_ref()
                .map(|base| crate::model::path::join_relative(base, &projection.logical))
        } else {
            None
        };
        let Some(mut target_path) = target_path else {
            skipped.push(relative.clone());
            continue;
        };
        let template = match requested_kind {
            "template" => true,
            "file" => false,
            _ => {
                relative.ends_with(".tmpl")
                    || (!set_selector && !matches!(context, FrozenValue::Null))
            }
        };
        if template && explicit_target.is_none() {
            target_path = target_path
                .strip_suffix(".tmpl")
                .unwrap_or(&target_path)
                .to_string();
        }
        let source = display_path(&repository_root, &source_base.join(&relative));
        projection.physical = source.clone();
        let origin = if set_selector {
            SourceOrigin::Directory {
                declared: source_path.to_string(),
                expanded: selector.expanded.clone(),
                root: display_path(&repository_root, &source_base.join(&selection_root)),
                relative: relative_from_root.to_string(),
                exclusions: exclusions.clone(),
                allow_empty,
            }
        } else {
            SourceOrigin::Direct {
                declared: source_path.to_string(),
                expanded: selector.expanded.clone(),
            }
        };
        let target = if !set_selector && explicit_target.is_some() {
            parse_explicit_target(&target_path)?
        } else if let Some(root) = &explicit_root {
            crate::model::manifest::TargetPath {
                path: target_path,
                scope: root.scope,
                origin: crate::model::manifest::TargetOrigin::DirectoryExplicit {
                    declared: root.path.clone(),
                    relative: projected_relative,
                },
            }
        } else {
            infer_target(&target_path, source.clone())?
        };
        state.artifacts.push(EvaluatedArtifact {
            kind: ArtifactKind::File,
            source,
            source_origin: origin,
            source_projection: Some(projection),
            production: if template {
                EvaluatedProduction::Template {
                    context: match &context {
                        FrozenValue::Null => FrozenValue::empty_map(),
                        value => value.clone(),
                    },
                }
            } else {
                EvaluatedProduction::Static
            },
            target,
            fingerprint: Some(fingerprint),
            owner: owner.clone(),
            declared_at: location.trace.clone(),
        });
        matched.push(relative);
    }
    if !skipped.is_empty() {
        if !set_selector {
            return Err(WombatError::configuration(format!(
                "unallocated artifact source `{source_path}` requires an explicit `to`"
            )));
        }
        match state.artifact_policy.unallocated {
            crate::model::manifest::UnallocatedPolicy::Ignore => {}
            crate::model::manifest::UnallocatedPolicy::Warn => {
                state.artifact_notices.push(ArtifactNotice {
                    kind: ArtifactNoticeKind::UnallocatedSkipped,
                    owner: owner.clone(),
                    selector: source_path.to_string(),
                    skipped: skipped.clone(),
                    declared_at: location.trace.clone(),
                })
            }
            crate::model::manifest::UnallocatedPolicy::Error => {
                return Err(WombatError::configuration(format!(
                    "artifact selector `{source_path}` contains unallocated children without an explicit `to`"
                )));
            }
        }
    }
    if set_selector && matched.is_empty() && !allow_empty {
        return Err(WombatError::configuration(format!(
            "artifact selector `{source_path}` produced no allocated files after exclusions and source policy; set `allow_empty = true` if this is intentional"
        )));
    }
    let selection_kind = if selector.glob {
        ArtifactSelectionKind::Glob
    } else if set_selector {
        ArtifactSelectionKind::Directory
    } else {
        ArtifactSelectionKind::Exact
    };
    state.artifact_selections.push(ArtifactSelection {
        owner: owner.clone(),
        declared: source_path.to_string(),
        expanded: selector.expanded.clone(),
        physical: selector.physical.clone(),
        source_base: display_path(&repository_root, &source_base),
        source_base_logical: base_logical,
        source_base_target: base_target.clone(),
        source_base_hidden: base_hidden,
        hidden,
        kind: selection_kind,
        static_root: selector.static_root.clone(),
        exclusions: exclusions.clone(),
        allow_empty,
        explicit_target: explicit_target.map(str::to_string),
        matches: matched,
        skipped_unallocated: skipped,
        declared_at: location.trace.clone(),
    });
    if set_selector {
        let snapshot = selected_snapshot.expect("set selectors record a traversal snapshot");
        let target_root = match explicit_root {
            Some(root) => Some(root),
            None => base_target
                .as_deref()
                .map(|target| infer_target_root(target, format!("selector:{source_path}")))
                .transpose()?,
        };
        state.directories.push(EvaluatedDirectory {
            declared_source: source_path.to_string(),
            root: display_path(&repository_root, &selected_snapshot_root),
            physical_selector: selector.physical,
            static_root: selector.static_root,
            hidden,
            glob: selector.glob,
            exclusions,
            target_root,
            owner,
            declared_at: location.trace,
            snapshot,
        });
    }
    Ok(())
}
