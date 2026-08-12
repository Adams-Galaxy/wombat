//! Shared subprocess execution and process-global working-directory serialization.

use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Result, WombatError};

#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
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

pub(crate) fn run(
    command: &mut Command,
    label: &str,
    timeout: Option<Duration>,
    output_limit: usize,
    stdin: Option<&[u8]>,
) -> Result<ProcessOutcome> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    let stdout_thread = thread::spawn(move || read_bounded(stdout, output_limit));
    let stderr_thread = thread::spawn(move || read_bounded(stderr, output_limit));
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

fn read_bounded(mut stream: impl Read, limit: usize) -> std::io::Result<Captured> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let available = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..count.min(available)]);
        truncated |= count > available;
    }
    Ok(Captured { bytes, truncated })
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
