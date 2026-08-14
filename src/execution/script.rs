//! Frozen stateful script execution and scheduling.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mlua::Lua;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution::ladder::RungId;
use crate::model::manifest::{
    ExecutionMode, Script, ScriptOutcome, ScriptOutcomeStatus, ScriptSchedule, ScriptScope,
    TaskLogPolicy,
};
use crate::{Result, WombatError};

const SCRIPT_STATE_FORMAT_VERSION: u32 = 1;
const MAX_LOG_SIZE: usize = 1024 * 1024;
const PYTHON_HELPER: &str = r#"from __future__ import annotations

import json
import sys
from pathlib import Path

_PREFIXES = ("--params=", "--work-dir=", "--cache-dir=", "--source-dir=", "--scope=", "--target-root=")
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
work = Path(_values.get("work-dir", "work"))
cache = Path(_values.get("cache-dir", "cache"))
source = Path(_values.get("source-dir", "source"))
scope = _values.get("scope", "target")
target_root = Path(_values["target-root"]) if "target-root" in _values else None
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PayloadKind {
    Plan,
    Product,
}

#[derive(Clone, Debug)]
pub(crate) struct ScriptExecutionOptions<'a> {
    pub state_root: &'a Path,
    pub payload_root: &'a Path,
    pub payload_kind: PayloadKind,
    pub project_identity: &'a str,
    pub plan_id: &'a str,
    pub build_id: Option<&'a str>,
    pub execution_mode: ExecutionMode,
    pub allow_host_scripts: bool,
    pub rerun: bool,
    pub run_scripts: bool,
    pub target_root: Option<&'a Path>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptState {
    format_version: u32,
    identity: String,
    successful: bool,
    change_digest: String,
    plan_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_id: Option<String>,
    rung: RungId,
    completed_at_unix_seconds: u64,
}

#[derive(Serialize)]
struct ChangeIdentity<'a> {
    payloads: &'a [crate::model::manifest::ScriptPayload],
    params: &'a crate::model::frozen::FrozenValue,
    runner: &'a crate::model::manifest::TaskRunner,
    python_helper: bool,
    logs: TaskLogPolicy,
    env: &'a BTreeMap<String, String>,
    scope: ScriptScope,
    rung: &'a RungId,
    timeout_seconds: Option<u64>,
    revision: &'a Option<String>,
}

pub(crate) fn publish_payloads(
    source_root: &Path,
    destination_root: &Path,
    scripts: &[Script],
    kind: PayloadKind,
) -> Result<()> {
    for script in scripts {
        let root = script_payload_root(destination_root, kind, &script.identity);
        for payload in &script.payloads {
            let source = source_root.join(&payload.source);
            let bytes = fs::read(&source).map_err(|error| WombatError::io(&source, error))?;
            if digest(&bytes) != payload.digest
                || u64::try_from(bytes.len()).ok() != Some(payload.size)
            {
                return Err(script_error(script, "payload changed during publication"));
            }
            let destination = root.join(&payload.relative);
            let parent = destination.expect_parent()?;
            fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
            fs::write(&destination, bytes).map_err(|error| WombatError::io(&destination, error))?;
            set_payload_permissions(&destination, payload.executable)?;
        }
    }
    Ok(())
}

pub(crate) fn verify_payloads(root: &Path, scripts: &[Script], kind: PayloadKind) -> Result<()> {
    for script in scripts {
        let payload_root = script_payload_root(root, kind, &script.identity);
        for payload in &script.payloads {
            let path = payload_root.join(&payload.relative);
            let bytes = fs::read(&path).map_err(|error| WombatError::io(&path, error))?;
            if digest(&bytes) != payload.digest
                || u64::try_from(bytes.len()).ok() != Some(payload.size)
            {
                return Err(script_error(script, "frozen payload failed verification"));
            }
        }
    }
    Ok(())
}

pub(crate) fn check_runners(scripts: &[Script]) -> Result<()> {
    for script in scripts {
        if script.runner.is_embedded_lua() || script.runner.is_direct() {
            continue;
        }
        let Some(command) = script.runner.command() else {
            return Err(script_error(
                script,
                "external runner has no interpreter command",
            ));
        };
        if resolve_command(command).is_none() {
            return Err(script_error(
                script,
                &format!("interpreter `{command}` is unavailable"),
            ));
        }
    }
    Ok(())
}

pub(crate) fn materialise_state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(root).join("wombat/scripts/materialise"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        WombatError::configuration(
            "script scheduling requires XDG_STATE_HOME or HOME to be configured",
        )
    })?;
    Ok(PathBuf::from(home).join(".local/state/wombat/scripts/materialise"))
}

pub(crate) fn execute_at(
    scripts: &[Script],
    rung: &RungId,
    options: &ScriptExecutionOptions<'_>,
) -> Result<Vec<ScriptOutcome>> {
    let mut outcomes = Vec::new();
    for script in scripts.iter().filter(|script| &script.at == rung) {
        outcomes.push(execute(script, options)?);
    }
    Ok(outcomes)
}

fn execute(script: &Script, options: &ScriptExecutionOptions<'_>) -> Result<ScriptOutcome> {
    if !options.run_scripts {
        return Ok(outcome(
            script,
            ScriptOutcomeStatus::ManualSkip,
            "script execution skipped by --skip-scripts",
        ));
    }

    if options.execution_mode == ExecutionMode::CompileOnly {
        match script.scope {
            ScriptScope::Target => {
                return Ok(outcome(
                    script,
                    ScriptOutcomeStatus::CompileOnlySkip,
                    "target-scoped script skipped in compile-only mode",
                ));
            }
            ScriptScope::Host if !options.allow_host_scripts => {
                return Err(script_error(
                    script,
                    "host-scoped script in compile-only mode requires --allow-host-scripts",
                ));
            }
            ScriptScope::Host => {}
        }
    }

    // State is keyed by project then by script, so two repositories cannot
    // collide and one script's schedule cannot be mistaken for another's. The
    // project component comes from the declared name when there is one, which is
    // what lets state survive relocating a checkout.
    let identity = short_digest(&script.identity);
    let project = options
        .project_identity
        .strip_prefix("sha256:")
        .unwrap_or(options.project_identity);
    let root = options.state_root.join(project).join(identity);
    ensure_private_directory(&root)?;
    let lock_path = root.join("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| WombatError::io(&lock_path, error))?;
    File::lock(&lock).map_err(|error| WombatError::io(&lock_path, error))?;

    let change_digest = change_digest(script)?;
    let state_path = root.join("state.json");
    let previous = read_state(&state_path, &script.identity)?;
    let due = options.rerun
        || match script.schedule {
            ScriptSchedule::Always => true,
            ScriptSchedule::Once => previous.as_ref().is_none_or(|state| !state.successful),
            ScriptSchedule::Onchange => previous
                .as_ref()
                .is_none_or(|state| !state.successful || state.change_digest != change_digest),
        };
    if !due {
        return Ok(outcome(
            script,
            ScriptOutcomeStatus::ScheduledSkip,
            "schedule is already satisfied",
        ));
    }

    let work = root.join("work");
    let cache = root.join("cache");
    let logs = root.join("logs");
    let payload = root.join("payload");
    reset_directory(&work)?;
    ensure_private_directory(&cache)?;
    ensure_private_directory(&logs)?;
    reset_directory(&payload)?;
    copy_verified_payload(script, options, &payload)?;

    let entrypoint_source = Path::new(&script.entrypoint)
        .strip_prefix("scripts")
        .map_err(|_| script_error(script, "entrypoint is not scripts-relative"))?;
    let source_dir = payload.join(entrypoint_source.parent().unwrap_or_else(|| Path::new("")));
    let entrypoint_relative = script
        .payloads
        .iter()
        .find(|payload| format!("scripts/{}", payload.relative) == script.entrypoint)
        .map(|payload| payload.relative.as_str())
        .ok_or_else(|| script_error(script, "entrypoint is absent from frozen payload"))?;
    let entrypoint = payload.join(entrypoint_relative);
    let params = serde_json::to_string(&script.params)?;
    let mut protocol = vec![
        format!("--params={params}"),
        format!("--work-dir={}", work.display()),
        format!("--cache-dir={}", cache.display()),
        format!("--source-dir={}", source_dir.display()),
        format!(
            "--scope={}",
            match script.scope {
                ScriptScope::Target => "target",
                ScriptScope::Host => "host",
            }
        ),
    ];
    if let Some(target_root) = options.target_root {
        protocol.push(format!("--target-root={}", target_root.display()));
    }
    let result = if script.runner.is_embedded_lua() {
        run_lua(script, &entrypoint, &work, &source_dir, &protocol)?
    } else {
        run_process(script, &entrypoint, &work, &source_dir, &root, &protocol)?
    };
    apply_logs(script, &logs, &result)?;
    if result.stdout.overflow || result.stderr.overflow {
        return Err(script_error(
            script,
            "output exceeded the 1 MiB per-stream retained evidence limit",
        ));
    }
    if !result.success {
        let detail = String::from_utf8_lossy(&result.stderr.bytes);
        return Err(script_error(
            script,
            &format!(
                "process exited with {}{}",
                result.status,
                if detail.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.trim())
                }
            ),
        ));
    }

    write_state(
        &state_path,
        &ScriptState {
            format_version: SCRIPT_STATE_FORMAT_VERSION,
            identity: script.identity.clone(),
            successful: true,
            change_digest,
            plan_id: options.plan_id.to_string(),
            build_id: options.build_id.map(str::to_string),
            rung: script.at.clone(),
            completed_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        },
    )?;
    Ok(outcome(
        script,
        ScriptOutcomeStatus::Ran,
        "completed successfully",
    ))
}

fn copy_verified_payload(
    script: &Script,
    options: &ScriptExecutionOptions<'_>,
    destination: &Path,
) -> Result<()> {
    let source_root =
        script_payload_root(options.payload_root, options.payload_kind, &script.identity);
    for item in &script.payloads {
        let source = source_root.join(&item.relative);
        let bytes = fs::read(&source).map_err(|error| WombatError::io(&source, error))?;
        if digest(&bytes) != item.digest || u64::try_from(bytes.len()).ok() != Some(item.size) {
            return Err(script_error(
                script,
                "frozen payload changed before execution",
            ));
        }
        let target = destination.join(&item.relative);
        let parent = target.expect_parent()?;
        fs::create_dir_all(parent).map_err(|error| WombatError::io(parent, error))?;
        fs::write(&target, bytes).map_err(|error| WombatError::io(&target, error))?;
        set_payload_permissions(&target, item.executable)?;
    }
    make_read_only(destination)?;
    Ok(())
}

fn run_process(
    script: &Script,
    entrypoint: &Path,
    work: &Path,
    source_dir: &Path,
    state_root: &Path,
    protocol: &[String],
) -> Result<RunResult> {
    let mut command = if script.runner.is_direct() {
        Command::new(entrypoint)
    } else {
        let Some(configured) = script.runner.command() else {
            return Err(script_error(
                script,
                "external runner has no interpreter command",
            ));
        };
        let executable = resolve_command(configured).ok_or_else(|| {
            script_error(
                script,
                &format!("interpreter `{configured}` is unavailable"),
            )
        })?;
        let mut command = Command::new(executable);
        command.args(script.runner.args()).arg(entrypoint);
        command
    };
    command.args(protocol).current_dir(work).envs(&script.env);
    if script.runner.is_python() && script.python_helper {
        let helper = state_root.join("python-helper");
        ensure_private_directory(&helper)?;
        fs::write(helper.join("wombat.py"), PYTHON_HELPER)
            .map_err(|error| WombatError::io(helper.join("wombat.py"), error))?;
        let mut paths = vec![helper, source_dir.to_path_buf()];
        if let Some(existing) = std::env::var_os("PYTHONPATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        command.env(
            "PYTHONPATH",
            std::env::join_paths(paths).map_err(|error| {
                WombatError::configuration(format!("cannot construct script Python path: {error}"))
            })?,
        );
    }
    run_streaming(command, &script.identity, script.timeout_seconds)
}

fn run_lua(
    script: &Script,
    entrypoint: &Path,
    work: &Path,
    source_dir: &Path,
    protocol: &[String],
) -> Result<RunResult> {
    let bytes = fs::read(entrypoint).map_err(|error| WombatError::io(entrypoint, error))?;
    super::process::with_working_directory(work, || {
        let lua = Lua::new();
        let args = lua.create_table()?;
        args.set(0, entrypoint.to_string_lossy().as_ref())?;
        for (index, value) in protocol.iter().enumerate() {
            args.set(index + 1, value.as_str())?;
        }
        lua.globals().set("arg", args)?;
        let package: mlua::Table = lua.globals().get("package")?;
        let existing_path: String = package.get("path")?;
        package.set(
            "path",
            format!(
                "{}/?.lua;{}/?/init.lua;{existing_path}",
                source_dir.display(),
                source_dir.display()
            ),
        )?;
        let declared_env = script.env.clone();
        let os: mlua::Table = lua.globals().get("os")?;
        os.set(
            "getenv",
            lua.create_function(move |_, key: String| {
                Ok(declared_env
                    .get(&key)
                    .cloned()
                    .or_else(|| std::env::var(key).ok()))
            })?,
        )?;
        let output = Arc::new(Mutex::new(Captured {
            bytes: Vec::new(),
            overflow: false,
        }));
        let printed = Arc::clone(&output);
        lua.globals().set(
            "print",
            lua.create_function(move |_, values: mlua::Variadic<mlua::Value>| {
                let mut line = values
                    .iter()
                    .map(|value| match value {
                        mlua::Value::String(value) => value.to_string_lossy(),
                        value => format!("{value:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
                    .into_bytes();
                line.push(b'\n');
                let mut capture = printed.lock().expect("Lua output capture");
                let available = MAX_LOG_SIZE.saturating_sub(capture.bytes.len());
                capture
                    .bytes
                    .extend_from_slice(&line[..line.len().min(available)]);
                capture.overflow |= line.len() > available;
                Ok(())
            })?,
        )?;
        if let Some(seconds) = script.timeout_seconds {
            let deadline = Instant::now() + Duration::from_secs(seconds);
            lua.set_hook(
                mlua::HookTriggers::new().every_nth_instruction(10_000),
                move |_, _| {
                    if Instant::now() >= deadline {
                        Err(mlua::Error::runtime("script timeout"))
                    } else {
                        Ok(mlua::VmState::Continue)
                    }
                },
            )?;
        }
        let result = lua
            .load(&bytes)
            .set_name(format!("@{}", entrypoint.display()))
            .exec();
        drop(lua);
        let stdout = Arc::try_unwrap(output)
            .expect("Lua output capture still referenced")
            .into_inner()
            .expect("Lua output capture lock");
        let mut execution = match result {
            Ok(()) => RunResult::success(),
            Err(error) => RunResult::failure(error.to_string()),
        };
        execution.stdout = stdout;
        // Embedded Lua output is buffered by the capture handle rather than a
        // pipe, so it is attributed here instead of by the shared executor.
        for line in execution.stdout.bytes.split(|byte| *byte == b'\n') {
            if !line.is_empty() {
                crate::presentation::emit(crate::presentation::Event::Progress(format!(
                    "[{}] {}",
                    script.identity,
                    String::from_utf8_lossy(line).trim_end_matches('\r')
                )));
            }
        }
        Ok(execution)
    })
}

#[derive(Debug)]
struct Captured {
    bytes: Vec<u8>,
    overflow: bool,
}

struct RunResult {
    success: bool,
    status: String,
    stdout: Captured,
    stderr: Captured,
}

impl RunResult {
    fn success() -> Self {
        Self {
            success: true,
            status: "success".to_string(),
            stdout: Captured {
                bytes: Vec::new(),
                overflow: false,
            },
            stderr: Captured {
                bytes: Vec::new(),
                overflow: false,
            },
        }
    }

    fn failure(message: String) -> Self {
        Self {
            success: false,
            status: "embedded Lua failure".to_string(),
            stdout: Captured {
                bytes: Vec::new(),
                overflow: false,
            },
            stderr: Captured {
                bytes: message.into_bytes(),
                overflow: false,
            },
        }
    }
}

fn run_streaming(mut command: Command, identity: &str, timeout: Option<u64>) -> Result<RunResult> {
    let outcome = super::process::run(
        &mut command,
        identity,
        timeout.map(Duration::from_secs),
        MAX_LOG_SIZE,
        None,
        super::process::Forwarding::Attributed,
    )?;
    Ok(RunResult {
        success: outcome.success,
        status: outcome.status,
        stdout: Captured {
            bytes: outcome.stdout.bytes,
            overflow: outcome.stdout.truncated,
        },
        stderr: Captured {
            bytes: outcome.stderr.bytes,
            overflow: outcome.stderr.truncated,
        },
    })
}

fn apply_logs(script: &Script, logs: &Path, result: &RunResult) -> Result<()> {
    let retain = match script.logs {
        TaskLogPolicy::Always => true,
        TaskLogPolicy::Failure => !result.success,
        TaskLogPolicy::Never => false,
    };
    for name in ["stdout.log", "stderr.log"] {
        let path = logs.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|error| WombatError::io(&path, error))?;
        }
    }
    if retain {
        fs::write(logs.join("stdout.log"), &result.stdout.bytes)
            .map_err(|error| WombatError::io(logs.join("stdout.log"), error))?;
        fs::write(logs.join("stderr.log"), &result.stderr.bytes)
            .map_err(|error| WombatError::io(logs.join("stderr.log"), error))?;
    }
    Ok(())
}

fn change_digest(script: &Script) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&ChangeIdentity {
        payloads: &script.payloads,
        params: &script.params,
        runner: &script.runner,
        python_helper: script.python_helper,
        logs: script.logs,
        env: &script.env,
        scope: script.scope,
        rung: &script.at,
        timeout_seconds: script.timeout_seconds,
        revision: &script.revision,
    })?))
}

fn outcome(script: &Script, status: ScriptOutcomeStatus, reason: &str) -> ScriptOutcome {
    ScriptOutcome {
        identity: script.identity.clone(),
        rung: script.at.clone(),
        status,
        reason: reason.to_string(),
    }
}

fn read_state(path: &Path, identity: &str) -> Result<Option<ScriptState>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WombatError::io(path, error)),
    };
    let state: ScriptState = serde_json::from_slice(&bytes)?;
    if state.format_version != SCRIPT_STATE_FORMAT_VERSION || state.identity != identity {
        return Err(WombatError::configuration(format!(
            "script state `{}` has an incompatible contract",
            path.display()
        )));
    }
    Ok(Some(state))
}

fn write_state(path: &Path, state: &ScriptState) -> Result<()> {
    crate::storage::atomic::write_json_pretty(path, state, true)
}

fn script_payload_root(root: &Path, kind: PayloadKind, identity: &str) -> PathBuf {
    let prefix = match kind {
        PayloadKind::Plan => "payloads/scripts",
        PayloadKind::Product => "scripts",
    };
    root.join(prefix).join(short_digest(identity))
}

fn short_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn digest(bytes: &[u8]) -> String {
    crate::storage::digest::sha256(bytes)
}

fn resolve_command(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(command))
            .find(|path| path.is_file())
    })
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    crate::storage::permissions::ensure_private_directory(path)
}

fn reset_directory(path: &Path) -> Result<()> {
    if path.exists() {
        make_writable(path)?;
        fs::remove_dir_all(path).map_err(|error| WombatError::io(path, error))?;
    }
    ensure_private_directory(path)
}

fn make_writable(root: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = fs::symlink_metadata(root).map_err(|error| WombatError::io(root, error))?;
        if metadata.is_dir() {
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))
                .map_err(|error| WombatError::io(root, error))?;
            for entry in fs::read_dir(root).map_err(|error| WombatError::io(root, error))? {
                make_writable(&entry.map_err(|error| WombatError::io(root, error))?.path())?;
            }
        } else {
            fs::set_permissions(root, fs::Permissions::from_mode(0o600))
                .map_err(|error| WombatError::io(root, error))?;
        }
    }
    Ok(())
}

fn set_payload_permissions(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o500 } else { 0o400 }),
        )
        .map_err(|error| WombatError::io(path, error))?;
    }
    Ok(())
}

fn make_read_only(root: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for entry in fs::read_dir(root).map_err(|error| WombatError::io(root, error))? {
            let path = entry.map_err(|error| WombatError::io(root, error))?.path();
            if path.is_dir() {
                make_read_only(&path)?;
            }
        }
        fs::set_permissions(root, fs::Permissions::from_mode(0o500))
            .map_err(|error| WombatError::io(root, error))?;
    }
    Ok(())
}

fn script_error(script: &Script, message: &str) -> WombatError {
    WombatError::configuration(format!("script `{}` failed: {message}", script.identity))
        .with_note(format!("declared at {}", script.declared_at))
}

trait ExpectParent {
    fn expect_parent(&self) -> Result<&Path>;
}

impl ExpectParent for Path {
    fn expect_parent(&self) -> Result<&Path> {
        self.parent().ok_or_else(|| {
            WombatError::configuration(format!("path `{}` has no parent", self.display()))
        })
    }
}
