use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

use mlua::{Lua, Value};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cache::{BuildCache, CachedOutput};
use crate::manifest::{
    ArtifactKind, EvaluatedArtifact, EvaluatedManifest, EvaluatedProduction, EvaluatedTargetRoot,
    SourceOrigin, TaskLogPolicy, TaskOutput, TaskRunnerFamily,
};
use crate::path::{expand_target_root, validate_relative_path};
use crate::source::{fingerprint_regular_file, validate_source_components};
use crate::{Result, WombatError};

const MAX_TASK_FILES: usize = 4_096;
const MAX_TASK_FILE_SIZE: u64 = 16 * 1024 * 1024;
const MAX_TASK_OUTPUT_SIZE: u64 = 64 * 1024 * 1024;
const MAX_LOG_SIZE: usize = 1024 * 1024;
const LOG_TRUNCATION_MARKER: &[u8] = b"\n[Wombat log truncated at 1 MiB]\n";
const PYTHON_HELPER: &str = r#"from __future__ import annotations

import json
import sys
from pathlib import Path

_PREFIXES = ("--params=", "--output-dir=", "--work-dir=", "--cache-dir=", "--source-dir=", "--scope=", "--target-root=")
_values = {}
_remaining = [sys.argv[0]]
for _argument in sys.argv[1:]:
    _matched = False
    for _prefix in _PREFIXES:
        if _argument.startswith(_prefix):
            _values[_prefix[2:-1]] = _argument[len(_prefix):]
            _matched = True
            break
    if not _matched:
        _remaining.append(_argument)
sys.argv[:] = _remaining

params = json.loads(_values.get("params", "{}"))
output = Path(_values.get("output-dir", "output"))
work = Path(_values.get("work-dir", "work"))
cache = Path(_values.get("cache-dir", "cache"))
source = Path(_values.get("source-dir", "."))
scope = _values.get("scope", "target")
target_root = Path(_values["target-root"]) if "target-root" in _values else None
"#;

#[derive(Serialize)]
struct TaskKey<'a> {
    identity: &'a str,
    runner_contract: u32,
    entrypoint_digest: &'a str,
    params: &'a crate::frozen::FrozenValue,
    runner: &'a crate::manifest::TaskRunner,
    python_helper: bool,
    target_root: &'a Option<crate::manifest::TaskTargetRoot>,
    revision: &'a Option<String>,
    interpreter_identity: &'a str,
    at: &'a crate::ladder::RungId,
}

#[allow(dead_code)]
pub(crate) fn execute_tasks(
    source_root: &Path,
    build_dir: &Path,
    desired: &mut EvaluatedManifest,
) -> Result<()> {
    let rungs = desired
        .tasks
        .iter()
        .map(|task| task.task.at.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for rung in rungs {
        execute_tasks_at(source_root, build_dir, desired, &rung)?;
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn execute_tasks_at(
    source_root: &Path,
    build_dir: &Path,
    desired: &mut EvaluatedManifest,
    rung: &crate::ladder::RungId,
) -> Result<()> {
    execute_tasks_selected(source_root, build_dir, desired, rung, None)
}

pub(crate) fn execute_task(
    source_root: &Path,
    build_dir: &Path,
    desired: &mut EvaluatedManifest,
    rung: &crate::ladder::RungId,
    identity: &str,
) -> Result<()> {
    execute_tasks_selected(source_root, build_dir, desired, rung, Some(identity))
}

fn execute_tasks_selected(
    source_root: &Path,
    build_dir: &Path,
    desired: &mut EvaluatedManifest,
    rung: &crate::ladder::RungId,
    identity: Option<&str>,
) -> Result<()> {
    if desired.tasks.is_empty() {
        return Ok(());
    }
    let cache = BuildCache::open(build_dir)?;
    let tasks_root = build_dir.join(".wombat/tasks");
    ensure_private_directory(&tasks_root)?;
    let helper_root = prepare_python_helper(build_dir)?;

    for index in 0..desired.tasks.len() {
        let evaluated = desired.tasks[index].clone();
        let task = &evaluated.task;
        if &task.at != rung || identity.is_some_and(|identity| task.identity != identity) {
            continue;
        }
        let workspace = tasks_root.join(workspace_name(&task.identity));
        ensure_private_directory(&workspace)?;
        let output = workspace.join("output");
        let work = workspace.join("work");
        let private_cache = workspace.join("cache");
        reset_private_directory(&output)?;
        reset_private_directory(&work)?;
        ensure_private_directory(&private_cache)?;

        let entrypoint = source_root.join(&task.entrypoint);
        validate_source_components(source_root, &entrypoint)?;
        if fingerprint_regular_file(&entrypoint)? != evaluated.fingerprint {
            return Err(task_error(task, "entrypoint changed after planning"));
        }
        let current_bytes =
            fs::read(&entrypoint).map_err(|error| WombatError::io(&entrypoint, error))?;
        if digest(&current_bytes) != task.entrypoint_digest {
            return Err(task_error(
                task,
                "entrypoint content changed after planning",
            ));
        }

        let interpreter_identity = interpreter_identity(task, &entrypoint)?;
        let key = cache.key(
            "task-v1",
            &TaskKey {
                identity: &task.identity,
                runner_contract: task.runner.contract_version,
                entrypoint_digest: &task.entrypoint_digest,
                params: &task.params,
                runner: &task.runner,
                python_helper: task.python_helper,
                target_root: &task.target_root,
                revision: &task.cache.revision,
                interpreter_identity: &interpreter_identity,
                at: &task.at,
            },
        )?;

        let restored = if task.cache.enabled {
            cache.load_task(&key, &output)?
        } else {
            None
        };
        if restored.is_some() {
            eprintln!("task {}: cache hit", task.identity);
            let empty = CapturedStream {
                bytes: Vec::new(),
                truncated: false,
            };
            apply_log_policy(task, &workspace, &empty, &empty, true)?;
        } else {
            eprintln!("task {}: running", task.identity);
            let result = run_task(
                task,
                &entrypoint,
                &workspace,
                &output,
                &work,
                &private_cache,
                &helper_root,
            )?;
            apply_log_policy(
                task,
                &workspace,
                &result.stdout,
                &result.stderr,
                result.success,
            )?;
            if !result.success {
                return Err(task_error(
                    task,
                    &format!("process exited with {}", result.status),
                ));
            }
        }

        let outputs = scan_outputs(&output)
            .map_err(|error| task_error(task, &format!("invalid output: {error}")))?;
        if task.target_root.is_none() && !outputs.is_empty() {
            return Err(task_error(
                task,
                "root-owned task produced files but has no target anchor; provide `to`",
            ));
        }
        if restored.is_none() && task.cache.enabled {
            cache.store_task(&key, &outputs, &output)?;
        }

        let mut recorded_outputs = Vec::with_capacity(outputs.len());
        for cached in &outputs {
            let path = portable_join(&output, &cached.relative);
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            let target_root = task
                .target_root
                .as_ref()
                .expect("nonempty task output has a target root");
            let target = expand_target_root(
                &EvaluatedTargetRoot {
                    path: target_root.path.clone(),
                    origin: target_root.origin.clone(),
                },
                &cached.relative,
            )?;
            desired.artifacts.push(EvaluatedArtifact {
                kind: ArtifactKind::File,
                source: format!(
                    "task-output/{}/{}",
                    short_digest(&task.identity),
                    cached.relative
                ),
                source_origin: SourceOrigin::Task {
                    identity: task.identity.clone(),
                    relative: cached.relative.clone(),
                },
                source_projection: None,
                production: EvaluatedProduction::Task {
                    identity: task.identity.clone(),
                    output: cached.relative.clone(),
                    content: bytes,
                    executable: cached.executable,
                },
                target,
                fingerprint: None,
                owner: task.owner.clone(),
                declared_at: task.declared_at.clone(),
            });
            recorded_outputs.push(TaskOutput {
                relative: cached.relative.clone(),
                content: crate::manifest::FileContent {
                    digest: cached.digest.clone(),
                    size: cached.size,
                    executable: cached.executable,
                },
            });
        }
        desired.tasks[index].task.outputs = recorded_outputs;

        if fingerprint_regular_file(&entrypoint)? != evaluated.fingerprint
            || digest(&fs::read(&entrypoint).map_err(|error| WombatError::io(&entrypoint, error))?)
                != task.entrypoint_digest
        {
            return Err(task_error(task, "entrypoint changed during execution"));
        }
    }

    desired.artifacts.sort_by(|left, right| {
        left.target
            .key()
            .cmp(right.target.key())
            .then_with(|| left.owner.cmp(&right.owner))
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.declared_at.cmp(&right.declared_at))
    });
    crate::runtime::validate_artifact_conflicts(&desired.artifacts)
}

pub(crate) fn check_runners(tasks: &[crate::manifest::Task]) -> Result<()> {
    for task in tasks {
        if matches!(
            task.runner.family,
            TaskRunnerFamily::EmbeddedLua | TaskRunnerFamily::Direct
        ) {
            continue;
        }
        let command = task
            .runner
            .command
            .as_deref()
            .expect("external task runner command");
        if resolve_command(command).is_none() {
            return Err(task_error(
                task,
                &format!(
                    "interpreter `{command}` is unavailable; configure a runner or satisfy its requirement before materialisation"
                ),
            ));
        }
    }
    Ok(())
}

struct RunResult {
    success: bool,
    status: String,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

fn run_task(
    task: &crate::manifest::Task,
    entrypoint: &Path,
    workspace: &Path,
    output: &Path,
    work: &Path,
    cache: &Path,
    helper_root: &Path,
) -> Result<RunResult> {
    let params = serde_json::to_string(&task.params)?;
    let protocol = vec![
        format!("--params={params}"),
        format!("--output-dir={}", output.display()),
        format!("--work-dir={}", work.display()),
        format!("--cache-dir={}", cache.display()),
        format!(
            "--source-dir={}",
            entrypoint
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .display()
        ),
        "--scope=target".to_string(),
    ];
    if task.runner.family == TaskRunnerFamily::EmbeddedLua {
        return run_lua(task, entrypoint, workspace, &protocol);
    }

    if task.runner.family == TaskRunnerFamily::Python && task.python_helper {
        reject_python_helper_conflict(entrypoint)?;
    }
    let mut command = if task.runner.family == TaskRunnerFamily::Direct {
        Command::new(entrypoint)
    } else {
        let configured = task
            .runner
            .command
            .as_deref()
            .expect("external interpreter runners have a command");
        let executable = resolve_command(configured).ok_or_else(|| {
            task_error(
                task,
                &format!("interpreter `{configured}` is unavailable; configure a runner or satisfy its requirement before materialisation"),
            )
        })?;
        let mut command = Command::new(executable);
        command.args(&task.runner.args).arg(entrypoint);
        command
    };
    command.args(&protocol).current_dir(workspace);
    if task.runner.family == TaskRunnerFamily::Python && task.python_helper {
        let mut paths = vec![helper_root.to_path_buf()];
        if let Some(existing) = env::var_os("PYTHONPATH") {
            paths.extend(env::split_paths(&existing));
        }
        let joined = env::join_paths(paths).map_err(|error| {
            WombatError::configuration(format!("cannot construct Python helper path: {error}"))
        })?;
        command.env("PYTHONPATH", joined);
    }
    run_streaming(command, &task.identity)
}

fn run_streaming(mut command: Command, identity: &str) -> Result<RunResult> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| WombatError::io("task process", error))?;
    let stdout = child.stdout.take().expect("piped stdout is available");
    let stderr = child.stderr.take().expect("piped stderr is available");
    let stdout_identity = identity.to_string();
    let stderr_identity = identity.to_string();
    let stdout_thread = thread::spawn(move || stream(stdout, &stdout_identity, false));
    let stderr_thread = thread::spawn(move || stream(stderr, &stderr_identity, true));
    let status = child
        .wait()
        .map_err(|error| WombatError::io("task process", error))?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| WombatError::configuration("task stdout reader panicked"))?
        .map_err(|error| WombatError::io("task stdout", error))?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| WombatError::configuration("task stderr reader panicked"))?
        .map_err(|error| WombatError::io("task stderr", error))?;
    Ok(RunResult {
        success: status.success(),
        status: display_status(status),
        stdout,
        stderr,
    })
}

fn stream(mut pipe: impl Read, identity: &str, stderr: bool) -> std::io::Result<CapturedStream> {
    if stderr {
        let mut destination = std::io::stderr().lock();
        stream_to(&mut pipe, identity, &mut destination)
    } else {
        let mut destination = std::io::stdout().lock();
        stream_to(&mut pipe, identity, &mut destination)
    }
}

fn stream_to(
    pipe: &mut impl Read,
    identity: &str,
    destination: &mut impl Write,
) -> std::io::Result<CapturedStream> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    let prefix = format!("[{identity}] ");
    let mut line_start = true;
    loop {
        let read = pipe.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        if retained.len() < MAX_LOG_SIZE {
            let remaining = MAX_LOG_SIZE - retained.len();
            retained.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            truncated |= chunk.len() > remaining;
        } else {
            truncated = true;
        }
        let mut start = 0;
        while start < chunk.len() {
            if line_start {
                destination.write_all(prefix.as_bytes())?;
                line_start = false;
            }
            let end = chunk[start..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(chunk.len(), |offset| start + offset + 1);
            destination.write_all(&chunk[start..end])?;
            if chunk[end - 1] == b'\n' {
                line_start = true;
            }
            start = end;
        }
        destination.flush()?;
    }
    Ok(CapturedStream {
        bytes: retained,
        truncated,
    })
}

fn run_lua(
    task: &crate::manifest::Task,
    entrypoint: &Path,
    workspace: &Path,
    protocol: &[String],
) -> Result<RunResult> {
    let bytes = fs::read(entrypoint).map_err(|error| WombatError::io(entrypoint, error))?;
    let old = env::current_dir().map_err(|error| WombatError::io("current directory", error))?;
    env::set_current_dir(workspace).map_err(|error| WombatError::io(workspace, error))?;
    let execution = (|| -> Result<()> {
        let lua = Lua::new();
        if let Ok(os) = lua.globals().get::<mlua::Table>("os") {
            os.set("exit", Value::Nil)?;
        }
        if let Ok(package) = lua.globals().get::<mlua::Table>("package") {
            package.set("loadlib", Value::Nil)?;
        }
        for name in ["dofile", "loadfile"] {
            lua.globals().set(name, Value::Nil)?;
        }
        let arg = lua.create_table()?;
        arg.set(0, entrypoint.to_string_lossy().as_ref())?;
        for (index, value) in protocol.iter().enumerate() {
            arg.set(index + 1, value.as_str())?;
        }
        lua.globals().set("arg", arg)?;
        lua.load(&bytes)
            .set_name(format!("@{}", task.entrypoint))
            .exec()
            .map_err(|error| task_error(task, &format!("Lua execution failed: {error}")))
    })();
    let restore = env::set_current_dir(&old).map_err(|error| WombatError::io(&old, error));
    execution?;
    restore?;
    Ok(RunResult {
        success: true,
        status: "success".to_string(),
        stdout: CapturedStream {
            bytes: Vec::new(),
            truncated: false,
        },
        stderr: CapturedStream {
            bytes: Vec::new(),
            truncated: false,
        },
    })
}

fn apply_log_policy(
    task: &crate::manifest::Task,
    workspace: &Path,
    stdout: &CapturedStream,
    stderr: &CapturedStream,
    success: bool,
) -> Result<()> {
    let retain = match task.logs {
        TaskLogPolicy::Failure => !success,
        TaskLogPolicy::Always => true,
        TaskLogPolicy::Never => false,
    };
    for (name, captured) in [("stdout.log", stdout), ("stderr.log", stderr)] {
        let path = workspace.join(name);
        if path
            .try_exists()
            .map_err(|error| WombatError::io(&path, error))?
        {
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(task_error(task, &format!("log path `{name}` is unsafe")));
            }
            fs::remove_file(&path).map_err(|error| WombatError::io(&path, error))?;
        }
        if retain {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(|error| WombatError::io(&path, error))?;
            set_private_file(&file, &path)?;
            if captured.truncated {
                let body = MAX_LOG_SIZE.saturating_sub(LOG_TRUNCATION_MARKER.len());
                file.write_all(&captured.bytes[..captured.bytes.len().min(body)])
                    .map_err(|error| WombatError::io(&path, error))?;
                file.write_all(LOG_TRUNCATION_MARKER)
                    .map_err(|error| WombatError::io(&path, error))?;
            } else {
                file.write_all(&captured.bytes)
                    .map_err(|error| WombatError::io(&path, error))?;
            }
        }
    }
    Ok(())
}

fn scan_outputs(root: &Path) -> Result<Vec<CachedOutput>> {
    let mut outputs = Vec::new();
    let mut total = 0_u64;
    walk_outputs(root, root, &mut outputs, &mut total)?;
    outputs.sort_by(|left, right| left.relative.cmp(&right.relative));
    if outputs.len() > MAX_TASK_FILES {
        return Err(WombatError::configuration(format!(
            "task produced {} files; limit is {MAX_TASK_FILES}",
            outputs.len()
        )));
    }
    Ok(outputs)
}

fn walk_outputs(
    root: &Path,
    directory: &Path,
    outputs: &mut Vec<CachedOutput>,
    total: &mut u64,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| WombatError::io(directory, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| WombatError::io(directory, error))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WombatError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WombatError::configuration(format!(
                "task output `{}` must not be a symbolic link",
                path.display()
            )));
        }
        if metadata.file_type().is_dir() {
            walk_outputs(root, &path, outputs, total)?;
            continue;
        }
        if !metadata.file_type().is_file() {
            return Err(WombatError::configuration(format!(
                "task output `{}` is not a regular file or directory",
                path.display()
            )));
        }
        if metadata.len() > MAX_TASK_FILE_SIZE {
            return Err(WombatError::configuration(format!(
                "task output `{}` exceeds the 16 MiB file limit",
                path.display()
            )));
        }
        *total = total
            .checked_add(metadata.len())
            .ok_or_else(|| WombatError::configuration("task output size overflow"))?;
        if *total > MAX_TASK_OUTPUT_SIZE {
            return Err(WombatError::configuration(
                "task output exceeds the 64 MiB aggregate limit",
            ));
        }
        let relative = path
            .strip_prefix(root)
            .expect("walked outputs remain beneath their root")
            .to_str()
            .ok_or_else(|| {
                WombatError::configuration(format!(
                    "task output `{}` is not valid UTF-8",
                    path.display()
                ))
            })?
            .replace('\\', "/");
        validate_relative_path(&relative, "task output")?;
        let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
        outputs.push(CachedOutput {
            relative,
            digest: digest(&bytes),
            size: metadata.len(),
            executable: executable(&metadata),
        });
    }
    Ok(())
}

fn prepare_python_helper(build_dir: &Path) -> Result<PathBuf> {
    let root = build_dir.join(".wombat/runners/python");
    if !root
        .try_exists()
        .map_err(|error| WombatError::io(&root, error))?
    {
        fs::create_dir_all(&root).map_err(|error| WombatError::io(&root, error))?;
    }
    ensure_private_directory(&root)?;
    let helper = root.join("wombat.py");
    let rewrite = fs::read(&helper).map_or(true, |bytes| bytes != PYTHON_HELPER.as_bytes());
    if rewrite {
        if helper
            .try_exists()
            .map_err(|error| WombatError::io(&helper, error))?
        {
            fs::remove_file(&helper).map_err(|error| WombatError::io(&helper, error))?;
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&helper)
            .map_err(|error| WombatError::io(&helper, error))?;
        set_private_file(&file, &helper)?;
        file.write_all(PYTHON_HELPER.as_bytes())
            .map_err(|error| WombatError::io(&helper, error))?;
    }
    Ok(root)
}

fn reject_python_helper_conflict(entrypoint: &Path) -> Result<()> {
    let directory = entrypoint
        .parent()
        .ok_or_else(|| WombatError::configuration("task entrypoint has no parent"))?;
    for candidate in [directory.join("wombat.py"), directory.join("wombat")] {
        if candidate
            .try_exists()
            .map_err(|error| WombatError::io(&candidate, error))?
        {
            return Err(WombatError::configuration(format!(
                "Python task companion `{}` conflicts with Wombat's helper; set `python_helper = false`",
                candidate.display()
            )));
        }
    }
    Ok(())
}

fn interpreter_identity(task: &crate::manifest::Task, _entrypoint: &Path) -> Result<String> {
    if task.runner.family == TaskRunnerFamily::EmbeddedLua {
        return Ok(format!(
            "embedded-lua-{}",
            mlua::Lua::new().load("return _VERSION").eval::<String>()?
        ));
    }
    if task.runner.family == TaskRunnerFamily::Direct {
        return Ok(format!("direct:{}", task.entrypoint_digest));
    }
    let configured = task.runner.command.as_deref().expect("runner command");
    let resolved = resolve_command(configured).ok_or_else(|| {
        task_error(
            task,
            &format!("interpreter `{configured}` is unavailable; configure a runner or satisfy its requirement before materialisation"),
        )
    })?;
    let version = Command::new(&resolved)
        .arg("--version")
        .output()
        .ok()
        .map(|output| {
            let bytes = if output.stdout.is_empty() {
                output.stderr
            } else {
                output.stdout
            };
            String::from_utf8_lossy(&bytes)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    Ok(format!("{}\0{version}", resolved.display()))
}

fn resolve_command(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 || path.is_absolute() {
        return (path.is_file() && executable_path(path)).then(|| path.to_path_buf());
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(command))
            .find(|candidate| candidate.is_file() && executable_path(candidate))
    })
}

fn reset_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).map_err(|error| WombatError::io(path, error))?;
        }
        Ok(_) => {
            return Err(WombatError::configuration(format!(
                "task workspace `{}` must be a non-symlink directory",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(WombatError::io(path, error)),
    }
    fs::create_dir(path).map_err(|error| WombatError::io(path, error))?;
    set_private_directory(path)
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(WombatError::configuration(format!(
                "task workspace `{}` must be a non-symlink directory",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| WombatError::io(path, error))?;
        }
        Err(error) => return Err(WombatError::io(path, error)),
    }
    set_private_directory(path)
}

fn set_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

fn set_private_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn executable_path(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable_path(path: &Path) -> bool {
    path.is_file()
}

fn portable_join(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, component| path.join(component))
}

fn workspace_name(identity: &str) -> String {
    let prefix = identity
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .take(40)
        .collect::<String>();
    format!("{prefix}-{}", short_digest(identity))
}

fn short_digest(value: &str) -> String {
    hex_digest(value.as_bytes())[..16].to_string()
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_digest(bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn display_status(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| status.to_string(), |code| format!("exit status {code}"))
}

fn task_error(task: &crate::manifest::Task, reason: &str) -> WombatError {
    let mut diagnostic =
        crate::Diagnostic::new(format!("task `{}` failed: {reason}", task.identity));
    diagnostic.primary = Some(task.declared_at.primary.clone());
    WombatError::diagnostic(diagnostic)
}
