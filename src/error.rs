//! Wombat's error type, its coarse classification, and diagnostic rendering.
//!
//! [`ErrorKind`] is deliberately small. It exists so callers and the CLI can
//! reason about what went wrong in categories — bad configuration, a corrupt
//! product, a refused conflict, an external process, the filesystem, or a broken
//! internal invariant — without matching on message text.
//!
//! Errors carry source provenance where they have it, so a Lua mistake can be
//! rendered as a compiler-style diagnostic pointing at the line that caused it
//! rather than a stack trace of Wombat's own internals.
use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::model::manifest::SourceLocation;

pub type Result<T> = std::result::Result<T, WombatError>;

/// Coarse classification of what went wrong.
///
/// Deliberately small, and matched on rather than message text. Callers use it
/// to decide whether a failure is the user's to fix, the environment's, or ours.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// The repository or invocation asked for something invalid. The user can
    /// fix this by editing configuration or changing the command.
    Configuration,
    /// Persisted data could not be trusted — a manifest, plan, journal, or state
    /// file that is corrupt or from an unsupported version. Usually resolved by
    /// rebuilding.
    CorruptState,
    /// Two things want the same resource: another Wombat holds a lock, or a
    /// target file is not what we expected. Ordinary contention, not a fault.
    Conflict,
    /// Refused on policy grounds — an unauthorised mutation, or a guard the user
    /// has to lift explicitly. Never escalate this into an attempt.
    Policy,
    /// An external process failed, timed out, or could not be started. The
    /// failure belongs to the thing we invoked, not to Wombat.
    Process,
    /// The filesystem refused an operation.
    Filesystem,
    /// An invariant Wombat is responsible for did not hold. Always a bug here,
    /// never something the user can fix.
    Internal,
}

/// Every error Wombat produces.
///
/// [`Diagnostic`](WombatError::Diagnostic) is the rich form, carrying source
/// provenance so a configuration mistake can be rendered pointing at the line
/// that caused it. The other variants are for failures with no user source to
/// point at.
///
/// The CLI maps all of these to exit code 1, except usage errors and `check`'s
/// operational failures, which exit 2.
#[derive(Debug, Error)]
pub enum WombatError {
    #[error("{}", .0.message)]
    Diagnostic(Box<Diagnostic>),

    #[error("{message}")]
    Classified { kind: ErrorKind, message: String },

    #[error("failed to access `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    Lua(#[from] mlua::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("project help requested")]
    ProjectHelpRequested,
}

/// A compiler-style diagnostic: what went wrong, where, and what to do.
///
/// `primary` and `source_line` are what let Wombat render a caret under the
/// offending line. `user_frames` holds filtered Lua frames — bundled and C
/// frames are excluded, because a stack through Wombat's own runtime tells the
/// user nothing about their configuration.
///
/// `underlying` keeps the raw error as fallback evidence, shown only under
/// `--trace`, so the concise default stays readable without losing detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub kind: ErrorKind,
    pub message: String,
    pub primary: Option<SourceLocation>,
    pub source_line: Option<String>,
    pub notes: Vec<String>,
    pub help: Vec<String>,
    pub user_frames: Vec<SourceLocation>,
    pub underlying: Option<String>,
}

impl Diagnostic {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Configuration,
            message: message.into(),
            primary: None,
            source_line: None,
            notes: Vec::new(),
            help: Vec::new(),
            user_frames: Vec::new(),
            underlying: None,
        }
    }

    pub fn render(&self, trace: bool) -> String {
        let mut output = format!("error: {}\n", self.message);
        if let Some(primary) = &self.primary {
            output.push_str(&format!("  --> {primary}\n"));
            if let (Some(line_number), Some(line)) = (primary.line, &self.source_line) {
                let width = line_number.to_string().len();
                output.push_str(&format!("{:width$} |\n", "", width = width));
                output.push_str(&format!("{line_number:>width$} | {line}\n"));
                if let Some(column) = primary.column {
                    let padding = " ".repeat(column.saturating_sub(1) as usize);
                    output.push_str(&format!("{:width$} | {padding}^\n", "", width = width));
                }
                output.push_str(&format!("{:width$} |\n", "", width = width));
            }
        }
        for note in &self.notes {
            output.push_str(&format!("  = note: {note}\n"));
        }
        for help in &self.help {
            output.push_str(&format!("  = help: {help}\n"));
        }
        if trace {
            if !self.user_frames.is_empty() {
                output.push_str("  = user trace:\n");
                for frame in &self.user_frames {
                    output.push_str(&format!("      {frame}\n"));
                }
            }
            if let Some(underlying) = &self.underlying {
                output.push_str(&format!("  = underlying: {underlying}\n"));
            }
        }
        output
    }
}

impl WombatError {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn configuration(message: impl Into<String>) -> Self {
        Self::classified(ErrorKind::Configuration, message)
    }

    pub fn corrupt_state(message: impl Into<String>) -> Self {
        Self::classified(ErrorKind::CorruptState, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::classified(ErrorKind::Conflict, message)
    }

    pub fn policy(message: impl Into<String>) -> Self {
        Self::classified(ErrorKind::Policy, message)
    }

    pub fn process(message: impl Into<String>) -> Self {
        Self::classified(ErrorKind::Process, message)
    }

    pub fn invariant(message: impl Into<String>) -> Self {
        Self::classified(ErrorKind::Internal, message)
    }

    pub fn diagnostic(diagnostic: Diagnostic) -> Self {
        Self::Diagnostic(Box::new(diagnostic))
    }

    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Diagnostic(diagnostic) => diagnostic.kind,
            Self::Classified { kind, .. } => *kind,
            Self::Io { .. } => ErrorKind::Filesystem,
            Self::Lua(_) => ErrorKind::Configuration,
            Self::Json(_) => ErrorKind::CorruptState,
            Self::ProjectHelpRequested => ErrorKind::Internal,
        }
    }

    pub fn with_note(self, note: impl Into<String>) -> Self {
        let note = note.into();
        let kind = self.kind();
        match self {
            Self::Diagnostic(mut diagnostic) => {
                diagnostic.notes.push(note);
                Self::Diagnostic(diagnostic)
            }
            other => {
                let message = other.to_string();
                let mut diagnostic = Diagnostic::new(&message);
                diagnostic.kind = kind;
                diagnostic.notes.push(note);
                diagnostic.underlying = Some(message);
                Self::Diagnostic(Box::new(diagnostic))
            }
        }
    }

    pub fn render(&self, trace: bool) -> String {
        match self {
            Self::Diagnostic(diagnostic) => diagnostic.render(trace),
            Self::Io { path, source } => {
                let mut diagnostic =
                    Diagnostic::new(format!("failed to access `{}`: {source}", path.display()));
                diagnostic.underlying = Some(source.to_string());
                diagnostic.render(trace)
            }
            Self::Lua(error) => {
                let mut diagnostic = Diagnostic::new(lua_reason(error));
                diagnostic.underlying = Some(error.to_string());
                diagnostic.render(trace)
            }
            Self::Json(error) => {
                let mut diagnostic = Diagnostic::new(error.to_string());
                diagnostic.underlying = Some(error.to_string());
                diagnostic.render(trace)
            }
            Self::Classified { message, .. } => Diagnostic::new(message).render(trace),
            Self::ProjectHelpRequested => Diagnostic::new("project help requested").render(trace),
        }
    }

    fn classified(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::Classified {
            kind,
            message: message.into(),
        }
    }
}

fn lua_reason(error: &mlua::Error) -> String {
    match error {
        mlua::Error::RuntimeError(message) | mlua::Error::SafetyError(message) => message
            .split("\nstack traceback:")
            .next()
            .unwrap_or(message)
            .to_string(),
        mlua::Error::SyntaxError { message, .. } => message.clone(),
        mlua::Error::CallbackError { cause, .. } => lua_reason(cause),
        mlua::Error::BadArgument { cause, .. } => lua_reason(cause),
        other => other.to_string(),
    }
}
