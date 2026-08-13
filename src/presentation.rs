//! Colour policy, log levels, and the event sink the CLI installs.
//!
//! Nothing in Wombat prints directly. Subsystems emit events through this module's sink,
//! and only a caller that installed a sink sees them — which is what keeps
//! library use silent and leaves the CLI owning every decision about colour,
//! verbosity, and which stream output lands on.
//!
//! The sink is process-global and set once, so a library embedding Wombat gets
//! no output at all unless it asks for it.
use std::env;
use std::io::{self, IsTerminal as _, Write as _};
use std::sync::OnceLock;

use crate::{Result, WombatError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorPolicy {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LogLevel {
    Debug,
    Info,
    Notice,
    Warn,
    Error,
}

impl LogLevel {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "notice" => Some(Self::Notice),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Success,
    Warning,
    Error,
    Path,
    Identity,
    Muted,
    Heading,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Event {
    Log { level: LogLevel, message: String },
    Progress(String),
}

static HUMAN_EVENTS: OnceLock<Presenter> = OnceLock::new();

#[doc(hidden)]
pub fn install_human_event_sink(presenter: Presenter) {
    let _ = HUMAN_EVENTS.set(presenter);
}

pub(crate) fn emit(event: Event) {
    let Some(presenter) = HUMAN_EVENTS.get().copied() else {
        return;
    };
    let (role, message) = match event {
        Event::Log { level, message } => (
            match level {
                LogLevel::Warn => Role::Warning,
                LogLevel::Error => Role::Error,
                _ => Role::Muted,
            },
            message,
        ),
        Event::Progress(message) => (Role::Muted, message),
    };
    eprintln!("{}", presenter.paint(role, message));
}

pub(crate) fn confirm(prompt: &str, operation: &str) -> Result<()> {
    if HUMAN_EVENTS.get().is_none() || !io::stdin().is_terminal() {
        return Err(WombatError::policy(format!(
            "{operation} requires --yes when interactive confirmation is unavailable"
        )));
    }
    eprint!("{prompt}");
    io::stderr()
        .flush()
        .map_err(|error| WombatError::io("standard error", error))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| WombatError::io("standard input", error))?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(WombatError::policy(format!("{operation} cancelled")))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Presenter {
    color: bool,
}

impl Presenter {
    pub fn new(policy: ColorPolicy, terminal: bool) -> Self {
        let color = match policy {
            ColorPolicy::Always => true,
            ColorPolicy::Never => false,
            ColorPolicy::Auto => terminal && env::var_os("NO_COLOR").is_none(),
        };
        Self { color }
    }

    pub const fn color_enabled(self) -> bool {
        self.color
    }

    pub fn paint(self, role: Role, value: impl AsRef<str>) -> String {
        let value = value.as_ref();
        if !self.color {
            return value.to_string();
        }
        let code = match role {
            Role::Success => "32",
            Role::Warning => "33",
            Role::Error => "31",
            Role::Path => "36",
            Role::Identity => "34",
            Role::Muted => "2",
            Role::Heading => "1",
        };
        format!("\u{1b}[{code}m{value}\u{1b}[0m")
    }

    pub fn human_output(self, output: &str) -> String {
        if !self.color {
            return output.to_string();
        }
        let mut rendered = String::new();
        for line in output.split_inclusive('\n') {
            let (body, newline) = line
                .strip_suffix('\n')
                .map_or((line, ""), |body| (body, "\n"));
            let styled = if body.starts_with("Create ") || body.starts_with("Adopt ") {
                self.style_action(body, Role::Success)
            } else if body.trim_start().starts_with("satisfied ") {
                self.paint(Role::Success, body)
            } else if body.trim_start().starts_with("missing ")
                || body.trim_start().starts_with("outdated ")
            {
                self.paint(Role::Warning, body)
            } else if body.trim_start().starts_with("unavailable ") {
                self.paint(Role::Error, body)
            } else if body.starts_with("Update ")
                || body.starts_with("AdvanceState ")
                || body.starts_with("Forget ")
                || body.starts_with("Remove ")
            {
                self.style_action(body, Role::Warning)
            } else if body.starts_with("Conflict ") || body.starts_with("error:") {
                self.style_action(body, Role::Error)
            } else if body.starts_with("--- ") || body.starts_with("+++ ") || body.starts_with("@@")
            {
                self.paint(Role::Identity, body)
            } else if body.starts_with('+') {
                self.paint(Role::Success, body)
            } else if body.starts_with('-') {
                self.paint(Role::Error, body)
            } else if body.starts_with("  owner:")
                || body.starts_with("  source:")
                || body.starts_with("  production:")
            {
                self.paint(Role::Muted, body)
            } else if body.starts_with("  conflict:") {
                self.paint(Role::Error, body)
            } else if body.contains(" changes:")
                || body == "No differences."
                || body == "Repository build inputs"
                || body == "Options:"
                || body.starts_with("Usage:")
                || body.starts_with("requirements for ")
                || body.starts_with("bootstrap will reconcile:")
            {
                self.paint(Role::Heading, body)
            } else if body.trim_start().starts_with('-') && body.contains("--") {
                self.paint(Role::Identity, body)
            } else {
                body.to_string()
            };
            rendered.push_str(&styled);
            rendered.push_str(newline);
        }
        rendered
    }

    fn style_action(self, body: &str, role: Role) -> String {
        body.split_once(' ').map_or_else(
            || self.paint(role, body),
            |(action, target)| {
                format!(
                    "{} {}",
                    self.paint(role, action),
                    self.paint(Role::Path, target)
                )
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ColorPolicy, Presenter};

    #[test]
    fn never_is_byte_plain_and_always_resets_every_style() {
        let text = "Create ~/.config/app\n  owner: app\n1 changes: 1 create\n";
        assert_eq!(
            Presenter::new(ColorPolicy::Never, true).human_output(text),
            text
        );
        let colored = Presenter::new(ColorPolicy::Always, false).human_output(text);
        assert!(colored.contains("\u{1b}[32mCreate\u{1b}[0m"));
        assert!(colored.contains("\u{1b}[36m~/.config/app\u{1b}[0m"));
        assert!(!colored.ends_with("\u{1b}[0m\u{1b}[0m"));
    }
}
