//! Shared subprocess execution and process-global working-directory
//! serialization.
//!
//! Every subprocess Wombat runs — construction observations, tasks, scripts,
//! provider queries — goes through [`run`]. One implementation means one place
//! that gets reaping, timeouts, and bounded capture right, instead of four that
//! each get them slightly wrong.
//!
//! The guarantees callers depend on:
//!
//! - output is bounded, so a runaway process cannot exhaust memory;
//! - a timeout terminates the whole process group, not just the child we
//!   spawned, so a shell that forked does not leave orphans behind;
//! - the child is always reaped, on every path including errors;
//! - nothing here writes to stdout or stderr. Forwarded output becomes an event
//!   the CLI renders, which is what keeps library use quiet.

use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Result, WombatError};

/// Retained output, and whether the limit cut it short.
///
/// `truncated` matters because a caller that verifies output must not treat a
/// clipped stream as the whole story.
#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Forwarding {
    Retained,
    Attributed,
}

#[derive(Debug)]
pub(crate) struct ProcessOutcome {
    pub(crate) success: bool,
    pub(crate) status: String,
    pub(crate) stdout: Captured,
    pub(crate) stderr: Captured,
    pub(crate) code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) timed_out: bool,
}

/// Runs a command to completion and returns what it did.
///
/// Never inherits stdin — a build must not stop waiting for input nobody is
/// there to type. `stdin` supplies bytes explicitly when a process needs them.
///
/// Returns `Err` only when the process could not be run or observed. A command
/// that ran and failed is a successful observation with `success: false`, which
/// leaves the caller to decide whether that is fatal.
pub(crate) fn run(
    command: &mut Command,
    label: &str,
    timeout: Option<Duration>,
    output_limit: usize,
    stdin: Option<&[u8]>,
    forwarding: Forwarding,
) -> Result<ProcessOutcome> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Give the child its own process group so a timeout can signal the whole
    // tree. Without this we would kill the shell and leave whatever it spawned
    // running.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| WombatError::io(format!("{label} process"), error))?;
    let process_id = child.id();
    let stdout = child.stdout.take().ok_or_else(|| {
        WombatError::invariant(format!("{label} process stdout was not captured"))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        WombatError::invariant(format!("{label} process stderr was not captured"))
    })?;
    let attribution = match forwarding {
        Forwarding::Retained => None,
        Forwarding::Attributed => Some(label.to_string()),
    };
    let stdout_attribution = attribution.clone();
    let stderr_attribution = attribution;
    // Both pipes are drained on their own threads. Reading them in sequence
    // would deadlock as soon as a process filled the pipe we were not reading.
    let stdout_thread =
        thread::spawn(move || read_bounded(stdout, output_limit, stdout_attribution));
    let stderr_thread =
        thread::spawn(move || read_bounded(stderr, output_limit, stderr_attribution));
    if let Some(input) = stdin {
        let mut child_stdin = child.stdin.take().ok_or_else(|| {
            WombatError::invariant(format!("{label} process stdin was not piped"))
        })?;
        child_stdin
            .write_all(input)
            .map_err(|error| WombatError::io(format!("{label} stdin"), error))?;
    }
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| WombatError::io(format!("{label} process"), error))?
        {
            break status;
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            // SIGTERM the group first so children get a chance to exit, then
            // kill the process we spawned. The `wait` that follows is what
            // actually reaps it — skipping it would leave a zombie.
            terminate_process_group(process_id);
            let _ = child.kill();
            timed_out = true;
            break child
                .wait()
                .map_err(|error| WombatError::io(format!("{label} process"), error))?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = join_reader(stdout_thread, label, "stdout")?;
    let stderr = join_reader(stderr_thread, label, "stderr")?;
    let code = status.code();
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;
    Ok(ProcessOutcome {
        success: status.success() && !timed_out,
        status: if timed_out {
            "timeout".to_string()
        } else {
            display_status(status)
        },
        stdout,
        stderr,
        code,
        signal,
        timed_out,
    })
}

/// Runs a command with the terminal attached, for provider operations that must
/// interact with the user.
///
/// This is how `sudo` and Homebrew reach the terminal to prompt for a password
/// or show progress. Nothing is captured, so the returned outcome carries status
/// only — use [`run`] whenever the output is evidence rather than a
/// conversation.
pub(crate) fn run_inherited(command: &mut Command, label: &str) -> Result<ProcessOutcome> {
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let status = command
        .spawn()
        .and_then(|mut child| child.wait())
        .map_err(|error| WombatError::io(format!("{label} process"), error))?;
    let code = status.code();
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;
    Ok(ProcessOutcome {
        success: status.success(),
        status: status.to_string(),
        stdout: Captured {
            bytes: Vec::new(),
            truncated: false,
        },
        stderr: Captured {
            bytes: Vec::new(),
            truncated: false,
        },
        code,
        signal,
        timed_out: false,
    })
}

fn read_bounded(
    mut stream: impl Read,
    limit: usize,
    attribution: Option<String>,
) -> std::io::Result<Captured> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    // Forwarding is line-oriented, so a chunk that splits a line is held here
    // until its newline arrives rather than being attributed twice.
    let mut pending = Vec::new();
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let available = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..count.min(available)]);
        truncated |= count > available;
        if let Some(identity) = attribution.as_deref() {
            pending.extend_from_slice(&buffer[..count]);
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                emit_attributed(identity, &line[..end]);
            }
        }
    }
    if let Some(identity) = attribution.as_deref()
        && !pending.is_empty()
    {
        emit_attributed(identity, &pending);
    }
    Ok(Captured { bytes, truncated })
}

fn emit_attributed(identity: &str, line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    crate::presentation::emit(crate::presentation::Event::Progress(format!(
        "[{identity}] {}",
        text.trim_end_matches('\r')
    )));
}

fn join_reader(
    reader: thread::JoinHandle<std::io::Result<Captured>>,
    label: &str,
    stream: &str,
) -> Result<Captured> {
    reader
        .join()
        .map_err(|_| WombatError::process(format!("{label} {stream} reader panicked")))?
        .map_err(|error| WombatError::io(format!("{label} {stream}"), error))
}

#[cfg(unix)]
fn terminate_process_group(process_id: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let _ = killpg(Pid::from_raw(process_id as i32), Signal::SIGTERM);
}

#[cfg(not(unix))]
fn terminate_process_group(_process_id: u32) {}

fn display_status(status: std::process::ExitStatus) -> String {
    status.to_string()
}

/// Runs `operation` with the process working directory changed, serialized
/// against every other caller.
///
/// The working directory is process-global, so two embedded Lua actions running
/// concurrently would otherwise see each other's directory. The lock makes that
/// impossible, and the guard restores the previous directory on success, error,
/// and panic alike.
///
/// A poisoned lock is recovered rather than propagated: the directory is
/// restored by the guard regardless, so a panicking caller does not need to take
/// the rest of the build down with it.
pub(crate) fn with_working_directory<T>(
    directory: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    static PROCESS_DIRECTORY: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = PROCESS_DIRECTORY
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous =
        std::env::current_dir().map_err(|error| WombatError::io("current directory", error))?;
    std::env::set_current_dir(directory).map_err(|error| WombatError::io(directory, error))?;
    let _restore = DirectoryRestore(previous);
    operation()
}

struct DirectoryRestore(PathBuf);

impl Drop for DirectoryRestore {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}
