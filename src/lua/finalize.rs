//! Dependency evaluation, manifest finalization, and reflected Lua diagnostics.

use super::*;

pub(super) fn evaluate_selected_modules(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
) -> Result<()> {
    loop {
        let next = state.borrow().modules.iter().find_map(|(name, module)| {
            (module.state == EvaluationState::Selected).then(|| name.clone())
        });
        let Some(name) = next else {
            break;
        };
        evaluate_module(lua, state, &name)?;
    }
    Ok(())
}

pub(super) fn evaluate_module(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    name: &str,
) -> Result<()> {
    let resolved_location = {
        let state = state.borrow();
        state
            .modules
            .get(name)
            .and_then(|module| module.location.clone())
            .map_or_else(|| resolve_module(&state.root, name), Ok)?
    };

    {
        let mut state = state.borrow_mut();
        let module = state.modules.get(name).ok_or_else(|| {
            WombatError::configuration(format!("module `{name}` was not selected"))
        })?;
        match module.state {
            EvaluationState::Evaluated => return Ok(()),
            EvaluationState::Evaluating => {
                let start = state
                    .stack
                    .iter()
                    .position(|active| active == name)
                    .unwrap_or(0);
                let mut cycle = state.stack[start..].to_vec();
                cycle.push(name.to_string());
                return Err(WombatError::configuration(format!(
                    "module cycle: {}",
                    cycle.join(" -> ")
                )));
            }
            EvaluationState::Failed => {
                return Err(WombatError::configuration(format!(
                    "module `{name}` previously failed to evaluate"
                )));
            }
            EvaluationState::Selected => {}
        }

        state
            .modules
            .get_mut(name)
            .expect("module was checked above")
            .location = Some(resolved_location.clone());
        state
            .modules
            .get_mut(name)
            .expect("module was checked above")
            .state = EvaluationState::Evaluating;
        state.stack.push(name.to_string());
    }

    let path = resolved_location.file;
    let result = load_tracked_source(state, &path).and_then(|source| {
        let value = execute_tracked_chunk(lua, state, &source, &path)?;
        FrozenValue::from_lua(value)
    });
    let selection = state
        .borrow()
        .dependencies
        .iter()
        .find(|dependency| dependency.kind == DependencyKind::Use && dependency.to == name)
        .cloned();
    let result = result.map_err(|error| match &selection {
        Some(selection) => error.with_note(format!(
            "module `{name}` was selected at {}",
            selection.declared_at
        )),
        None => error,
    });

    let mut state = state.borrow_mut();
    let popped = state.stack.pop();
    debug_assert_eq!(popped.as_deref(), Some(name));
    let module = state
        .modules
        .get_mut(name)
        .expect("an evaluating module must remain registered");
    match result {
        Ok(export) => {
            module.export = Some(export);
            module.state = EvaluationState::Evaluated;
            Ok(())
        }
        Err(error) => {
            module.state = EvaluationState::Failed;
            Err(error)
        }
    }
}

pub(super) fn resolve_module(root: &Path, name: &str) -> Result<ModuleLocation> {
    let mut candidates = Vec::new();
    collect_module_files(&root.join("modules"), &mut candidates)?;
    let matches = candidates
        .iter()
        .filter(|file| {
            file.extension().is_some_and(|ext| ext == "lua")
                && file.file_stem().is_some_and(|stem| stem == name)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [file] => Ok(ModuleLocation {
            file: (*file).clone(),
        }),
        [] => Err(WombatError::configuration(format!(
            "module `{name}` was not found beneath `modules/`"
        ))),
        _ => {
            let found = matches
                .iter()
                .map(|file| display_path(root, file))
                .collect::<Vec<_>>()
                .join(", ");
            Err(WombatError::configuration(format!(
                "module id `{name}` is duplicated by filename stem: {found}"
            )))
        }
    }
}

pub(super) fn collect_module_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(WombatError::io(directory, error)),
    };
    let mut entries = entries
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "module path `{}` must not be a symbolic link",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_module_files(&path, files)?;
        } else if metadata.is_file() {
            if path.extension().is_some_and(|ext| ext == "lua") {
                files.push(path);
            }
        } else {
            return Err(WombatError::configuration(format!(
                "module path `{}` must be a regular file or directory",
                path.display()
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_dependency_cycles(state: &RuntimeState) -> Result<()> {
    let mut graph: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for dependency in &state.dependencies {
        if dependency.from != ROOT_MODULE {
            graph
                .entry(&dependency.from)
                .or_default()
                .insert(&dependency.to);
        }
    }

    let mut complete = BTreeSet::new();
    let mut stack = Vec::new();
    for module in state.modules.keys() {
        visit_dependency(module, &graph, &mut complete, &mut stack)?;
    }
    Ok(())
}

pub(super) fn visit_dependency<'a>(
    module: &'a str,
    graph: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    complete: &mut BTreeSet<&'a str>,
    stack: &mut Vec<&'a str>,
) -> Result<()> {
    if complete.contains(module) {
        return Ok(());
    }
    if let Some(start) = stack.iter().position(|active| *active == module) {
        let mut cycle = stack[start..].to_vec();
        cycle.push(module);
        return Err(WombatError::configuration(format!(
            "module cycle: {}",
            cycle.join(" -> ")
        )));
    }

    stack.push(module);
    if let Some(dependencies) = graph.get(module) {
        for dependency in dependencies {
            visit_dependency(dependency, graph, complete, stack)?;
        }
    }
    stack.pop();
    complete.insert(module);
    Ok(())
}

pub(super) fn build_manifest(
    state: &RuntimeState,
    preparations: Vec<ProviderPreparation>,
) -> Result<EvaluatedManifest> {
    let modules = state
        .modules
        .iter()
        .map(|(name, module)| ManifestModule {
            name: name.clone(),
            source: module
                .location
                .as_ref()
                .map(|location| display_path(&state.root, &location.file))
                .expect("selected modules are evaluated before manifest construction"),
            config: module.config(),
            source_base: module.source_base.clone(),
        })
        .collect();
    let dependencies = state.dependencies.iter().cloned().collect();
    let mut artifacts = state.artifacts.clone();
    artifacts.sort_by(|left, right| {
        left.target
            .key()
            .cmp(right.target.key())
            .then_with(|| left.owner.cmp(&right.owner))
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.declared_at.cmp(&right.declared_at))
    });
    let mut directories = state.directories.clone();
    directories.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.owner.cmp(&right.owner))
            .then_with(|| left.declared_at.cmp(&right.declared_at))
    });

    let ladder = state.ladder.clone().unwrap_or_default();
    validate_ladder_actions(&ladder, &state.requirements, &state.tasks, &state.scripts)?;
    let requirements = normalize_requirements(state.requirements.clone(), &ladder)?;
    let project_identity = digest_bytes(state.root.to_string_lossy().as_bytes());
    Ok(EvaluatedManifest {
        plan_id: String::new(),
        project_arguments: state
            .project_arguments
            .iter()
            .map(|argument| {
                argument.to_str().map(str::to_owned).ok_or_else(|| {
                    WombatError::configuration("project arguments must be valid UTF-8")
                })
            })
            .collect::<Result<Vec<_>>>()?,
        sources: state
            .sources
            .values()
            .map(|source| source.manifest.clone())
            .collect(),
        inputs: state.inputs.clone(),
        target: state.target.clone(),
        observations: state.observations.values().cloned().collect(),
        process_observations: state.process_observations.clone(),
        modules,
        dependencies,
        project_identity,
        ladder,
        providers: state.providers.clone(),
        requirements,
        preparations,
        tasks: state.tasks.clone(),
        scripts: state.scripts.clone(),
        artifact_policy: state.artifact_policy,
        artifact_notices: state.artifact_notices.clone(),
        artifact_selections: state.artifact_selections.clone(),
        artifacts,
        directories,
    })
}

pub(super) fn normalize_requirements(
    mut requirements: Vec<Requirement>,
    ladder: &ExecutionLadder,
) -> Result<Vec<Requirement>> {
    requirements.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| {
                left.candidates[left.selected as usize]
                    .name()
                    .cmp(right.candidates[right.selected as usize].name())
            })
            .then_with(|| left.binding.provider.cmp(&right.binding.provider))
            .then_with(|| left.binding.identity.cmp(&right.binding.identity))
            .then_with(|| left.declared_at.cmp(&right.declared_at))
    });
    let mut normalized: Vec<Requirement> = Vec::new();
    for requirement in requirements {
        let same = normalized.last().is_some_and(|previous| {
            previous.kind == requirement.kind
                && previous.candidates[previous.selected as usize].name()
                    == requirement.candidates[requirement.selected as usize].name()
                && previous.binding.provider == requirement.binding.provider
                && previous.binding.identity == requirement.binding.identity
        });
        if same {
            let previous = normalized.last_mut().expect("same requirement exists");
            if previous.candidates != requirement.candidates
                || previous.choice != requirement.choice
            {
                return Err(WombatError::configuration(format!(
                    "conflicting requirement declarations for {} through `{}` at {} and {}",
                    requirement.candidates[requirement.selected as usize].name(),
                    requirement.binding.provider,
                    previous.declared_at,
                    requirement.declared_at,
                )));
            }
            if ladder.position(&requirement.when) < ladder.position(&previous.when) {
                previous.when = requirement.when;
            }
        } else {
            normalized.push(requirement);
        }
    }
    Ok(normalized)
}

pub(super) fn validate_ladder_actions(
    ladder: &ExecutionLadder,
    requirements: &[Requirement],
    tasks: &[EvaluatedTask],
    scripts: &[Script],
) -> Result<()> {
    let mut used = BTreeSet::new();
    for (kind, id, location) in requirements
        .iter()
        .map(|value| ("requirement", &value.when, &value.declared_at))
        .chain(
            tasks
                .iter()
                .map(|value| ("task", &value.task.at, &value.task.declared_at)),
        )
        .chain(
            scripts
                .iter()
                .map(|value| ("script", &value.at, &value.declared_at)),
        )
    {
        if !ladder.contains(id) {
            return Err(WombatError::configuration(format!(
                "{kind} targets unknown rung `{id}` at {location}"
            )));
        }
        if ladder.is_container(id) {
            return Err(WombatError::configuration(format!(
                "{kind} cannot target container rung `{id}` at {location}"
            )));
        }
        used.insert(id.clone());
    }
    let first_task: RungId = CoreRung::MaterialiseBefore.into();
    let last_task: RungId = CoreRung::MaterialiseArtifacts.into();
    let first = ladder.position(&first_task).expect("fixed rung exists");
    let last = ladder.position(&last_task).expect("fixed rung exists");
    for task in tasks {
        let position = ladder
            .position(&task.task.at)
            .expect("task rung was validated");
        if position < first || position > last {
            return Err(WombatError::configuration(format!(
                "task `{}` rung `{}` must be between materialise.before and materialise.artifacts",
                task.task.identity, task.task.at
            )));
        }
    }
    for rung in &ladder.flattened {
        if rung.core.is_none() && !ladder.is_container(&rung.id) && !used.contains(&rung.id) {
            return Err(WombatError::configuration(format!(
                "custom leaf rung `{}` has no actions",
                rung.id
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_module_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(WombatError::configuration(format!(
            "invalid module name `{name}`; expected ASCII letters, numbers, `_`, or `-`"
        )));
    }
    Ok(())
}

pub(crate) fn validate_artifact_conflicts(artifacts: &[EvaluatedArtifact]) -> Result<()> {
    let mut ordered = artifacts.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.target
            .key()
            .cmp(right.target.key())
            .then_with(|| left.owner.cmp(&right.owner))
            .then_with(|| left.source.cmp(&right.source))
    });

    for (index, artifact) in ordered.iter().enumerate() {
        let duplicates = ordered
            .iter()
            .filter(|candidate| candidate.target.key() == artifact.target.key())
            .copied()
            .collect::<Vec<_>>();
        if duplicates.len() > 1
            && ordered[..index]
                .iter()
                .all(|prior| prior.target.key() != artifact.target.key())
        {
            return Err(artifact_conflict(
                &artifact.target.path,
                "multiple artifacts resolve to the same target",
                &duplicates,
            ));
        }

        let descendants = ordered
            .iter()
            .skip(index + 1)
            .filter(|descendant| is_path_ancestor(&artifact.target.path, &descendant.target.path))
            .copied()
            .collect::<Vec<_>>();
        if !descendants.is_empty() {
            let displays = descendants
                .iter()
                .map(|descendant| format!("`{}`", descendant.target.path))
                .collect::<Vec<_>>()
                .join(", ");
            let mut conflicts = Vec::with_capacity(descendants.len() + 1);
            conflicts.push(*artifact);
            conflicts.extend(descendants);
            return Err(artifact_conflict(
                &artifact.target.path,
                &format!("file target is an ancestor of {displays}"),
                &conflicts,
            ));
        }
    }
    Ok(())
}

pub(super) fn is_path_ancestor(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(super) fn artifact_conflict(
    target: &str,
    reason: &str,
    artifacts: &[&EvaluatedArtifact],
) -> WombatError {
    let declarations = artifacts
        .iter()
        .map(|artifact| {
            let source = match &artifact.source_origin {
                SourceOrigin::Direct { declared, .. } => {
                    format!("`{}` (direct source `{declared}`)", artifact.source)
                }
                SourceOrigin::Directory {
                    declared,
                    root,
                    relative,
                    ..
                } => format!(
                    "`{}` (leaf `{relative}` expanded from directory `{declared}` at `{root}`)",
                    artifact.source
                ),
                SourceOrigin::Generated { name } => {
                    format!("`{}` (generated value `{name}`)", artifact.source)
                }
                SourceOrigin::Task { identity, relative } => format!(
                    "`{}` (task `{identity}` output `{relative}`)",
                    artifact.source
                ),
            };
            format!(
                "{} from {source} declared at {}",
                artifact.owner, artifact.declared_at
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    WombatError::configuration(format!(
        "artifact conflict at `{target}`: {reason}; declarations: {declarations}"
    ))
}

pub(super) fn execute_tracked_chunk(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    source: &str,
    path: &Path,
) -> Result<Value> {
    state.borrow_mut().failure_frames.clear();
    state.borrow_mut().failure_tail_call = false;
    let chunk = lua
        .load(source)
        .set_name(format!("@{}", path.to_string_lossy()))
        .into_function()
        .map_err(|error| lua_diagnostic(state, error, Some(path)))?;
    let handler_state = Rc::clone(state);
    let handler = lua.create_function(move |lua, error: Value| {
        let (frames, tail_call) = capture_user_frames(lua, &handler_state);
        let mut state = handler_state.borrow_mut();
        state.failure_frames = frames;
        state.failure_tail_call = tail_call;
        Ok(error)
    })?;
    let protected: Function = lua
        .load(
            "return function(chunk, handler)\n\
             local result = table.pack(xpcall(chunk, handler))\n\
             if not result[1] then error(result[2], 0) end\n\
             return table.unpack(result, 2, result.n)\n\
             end",
        )
        .set_name("=<wombat>/protected.lua")
        .eval()?;
    protected
        .call((chunk, handler))
        .map_err(|error| lua_diagnostic(state, error, Some(path)))
}

pub(super) fn lua_diagnostic(
    state: &Rc<RefCell<RuntimeState>>,
    error: mlua::Error,
    fallback_path: Option<&Path>,
) -> WombatError {
    let state = state.borrow();
    let mut frames = state.failure_frames.clone();
    if frames.is_empty()
        && let Some(path) = fallback_path
    {
        frames.push(SourceLocation {
            source: display_path(&state.root, path),
            line: syntax_line(&error),
            column: None,
        });
    }
    let primary = frames.first().cloned();
    let source_line = primary.as_ref().and_then(|location| {
        let line = usize::try_from(location.line?).ok()?;
        state
            .sources
            .get(&location.source)?
            .snapshot
            .lines()
            .nth(line.saturating_sub(1))
            .map(str::to_string)
    });
    let raw = error.to_string();
    let mut diagnostic = Diagnostic::new(clean_lua_reason(&raw));
    diagnostic.primary = primary;
    diagnostic.source_line = source_line;
    diagnostic.user_frames = frames;
    if let (Some(primary), Some(caller)) = (
        diagnostic.user_frames.first(),
        diagnostic.user_frames.get(1),
    ) && primary.source != caller.source
    {
        diagnostic.notes.push(format!("called from {caller}"));
    }
    if state.failure_tail_call {
        diagnostic.notes.push(
            "Lua reported a tail call; intermediate user frames may be unavailable".to_string(),
        );
    }
    diagnostic.underlying = Some(raw);
    WombatError::diagnostic(diagnostic)
}

pub(super) fn syntax_line(error: &mlua::Error) -> Option<u32> {
    let message = match error {
        mlua::Error::SyntaxError { message, .. } => message,
        _ => return None,
    };
    parse_lua_line(message)
}

pub(super) fn parse_lua_line(message: &str) -> Option<u32> {
    message.split(':').find_map(|part| part.parse::<u32>().ok())
}

pub(super) fn clean_lua_reason(raw: &str) -> String {
    let first = raw.split("\nstack traceback:").next().unwrap_or(raw);
    let bytes = first.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b':' {
            continue;
        }
        let digits_start = start + 1;
        let mut end = digits_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > digits_start && end < bytes.len() && bytes[end] == b':' {
            return first[end + 1..].trim().to_string();
        }
    }
    first
        .trim()
        .strip_prefix("runtime error:")
        .or_else(|| first.trim().strip_prefix("syntax error:"))
        .unwrap_or(first.trim())
        .trim()
        .to_string()
}

pub(super) fn capture_user_frames(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
) -> (Vec<SourceLocation>, bool) {
    let root = state.borrow().root.clone();
    let mut frames = Vec::new();
    let mut tail_call = false;
    for level in 1..=48 {
        if frames.len() == MAX_SOURCE_TRACE_FRAMES {
            break;
        }
        let frame = lua
            .inspect_stack(level, |debug| {
                let source = debug.source().source?.into_owned();
                if source == "<wombat>/init.lua"
                    || source == "=<wombat>/init.lua"
                    || source == "<wombat>/protected.lua"
                    || source == "=<wombat>/protected.lua"
                    || source == "=[C]"
                    || source == "[C]"
                    || source == "<unknown>"
                {
                    return None;
                }
                let source = source.strip_prefix('@').unwrap_or(&source);
                Some((
                    SourceLocation {
                        source: display_path(&root, Path::new(source)),
                        line: debug
                            .current_line()
                            .and_then(|line| u32::try_from(line).ok()),
                        column: None,
                    },
                    debug.is_tail_call(),
                ))
            })
            .flatten();
        let Some((frame, is_tail_call)) = frame else {
            continue;
        };
        tail_call |= is_tail_call;
        if frames.last() != Some(&frame) {
            frames.push(frame);
        }
    }
    (frames, tail_call)
}

pub(super) fn caller_location(lua: &Lua, state: &Rc<RefCell<RuntimeState>>) -> Location {
    let (frames, _) = capture_user_frames(lua, state);
    let primary = frames.first().cloned().unwrap_or(SourceLocation {
        source: "<unknown>".to_string(),
        line: None,
        column: None,
    });
    Location {
        trace: SourceTrace {
            primary,
            callers: frames.into_iter().skip(1).collect(),
        },
    }
}

pub(super) fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn load_tracked_source(
    state: &Rc<RefCell<RuntimeState>>,
    path: &Path,
) -> Result<String> {
    let root = state.borrow().root.clone();
    let relative = path.strip_prefix(&root).map_err(|_| {
        WombatError::configuration(format!(
            "Lua source `{}` escapes the repository",
            path.display()
        ))
    })?;
    let mut current = root.clone();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(WombatError::configuration(format!(
                "Lua source `{}` contains an invalid path component",
                path.display()
            )));
        };
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| WombatError::io(&current, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "Lua source `{}` must not contain symbolic links",
                path.display()
            )));
        }
    }
    let before = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if !before.file_type().is_file() {
        return Err(WombatError::configuration(format!(
            "Lua source `{}` is not a regular file",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| WombatError::io(path, error))?;
    let after = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    let before_fingerprint = SourceFingerprint::from_metadata(&before);
    if SourceFingerprint::from_metadata(&after) != before_fingerprint {
        return Err(WombatError::configuration(format!(
            "Lua source `{}` changed while it was being read",
            path.display()
        )));
    }
    let snapshot = String::from_utf8(bytes.clone()).map_err(|_| {
        WombatError::configuration(format!(
            "Lua source `{}` is not valid UTF-8",
            path.display()
        ))
    })?;
    let portable = display_path(&root, path);
    let manifest = SourceFile {
        path: portable.clone(),
        digest: digest_bytes(&bytes),
    };
    let mut state = state.borrow_mut();
    if let Some(existing) = state.sources.get(&portable) {
        if existing.manifest != manifest || existing.fingerprint != before_fingerprint {
            return Err(WombatError::configuration(format!(
                "Lua source `{portable}` changed during evaluation"
            )));
        }
        return Ok(existing.snapshot.clone());
    }
    state.sources.insert(
        portable,
        TrackedSource {
            manifest,
            fingerprint: before_fingerprint,
            snapshot: snapshot.clone(),
        },
    );
    Ok(snapshot)
}

pub(super) fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(7 + digest.len() * 2);
    output.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{helper_module_path, is_path_ancestor, validate_module_name};

    #[test]
    fn validates_initial_module_names() {
        assert!(validate_module_name("theme-2").is_ok());
        assert!(validate_module_name("themes.kanagawa").is_err());
        assert!(validate_module_name("../theme").is_err());
    }

    #[test]
    fn detects_only_segment_ancestor_paths() {
        assert!(is_path_ancestor("nvim", "nvim/init.lua"));
        assert!(!is_path_ancestor("nvim", "nvim-old/init.lua"));
        assert!(!is_path_ancestor("nvim", "nvim"));
    }

    #[test]
    fn normalizes_safe_repository_helper_names() {
        assert_eq!(helper_module_path("theme.colors").unwrap(), "theme/colors");
        assert!(helper_module_path("../theme").is_err());
        assert!(helper_module_path("theme/path").is_err());
    }
}
