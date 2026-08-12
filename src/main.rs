//! Wombat CLI composition root.

mod cli;

fn main() -> std::process::ExitCode {
    cli::run()
}
