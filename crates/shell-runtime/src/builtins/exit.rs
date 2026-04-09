use super::Builtin;
use shell_core::{state::ShellState, types::ExitStatus};

pub struct Exit;

impl Builtin for Exit {
    fn name(&self) -> &'static str {
        "exit"
    }

    fn run(&self, args: &[String], _state: &mut ShellState) -> ExitStatus {
        let code: i32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        std::process::exit(code);
    }
}
