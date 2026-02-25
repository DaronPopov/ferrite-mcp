use shell_core::{state::ShellState, types::ExitStatus};
use super::Builtin;

pub struct Pwd;

impl Builtin for Pwd {
    fn name(&self) -> &'static str { "pwd" }

    fn run(&self, _args: &[String], state: &mut ShellState) -> ExitStatus {
        println!("{}", state.cwd.display());
        ExitStatus::SUCCESS
    }
}
