//! Built-in commands — implemented directly in the shell process.

use shell_core::{state::ShellState, types::ExitStatus};

pub mod cd;
pub mod echo;
pub mod exit;
pub mod export;
pub mod pwd;

/// A built-in command implementation.
pub trait Builtin: Sync {
    fn name(&self) -> &'static str;
    fn run(&self, args: &[String], state: &mut ShellState) -> ExitStatus;
}

/// Look up a builtin by name. Returns `None` for external commands.
pub fn lookup(name: &str) -> Option<&'static dyn Builtin> {
    BUILTINS.iter().find(|b| b.name() == name).copied()
}

static BUILTINS: &[&dyn Builtin] = &[
    &cd::Cd,
    &echo::Echo,
    &export::Export,
    &exit::Exit,
    &pwd::Pwd,
];
