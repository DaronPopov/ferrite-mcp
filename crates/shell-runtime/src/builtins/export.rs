use super::Builtin;
use shell_core::{state::ShellState, types::ExitStatus};

pub struct Export;

impl Builtin for Export {
    fn name(&self) -> &'static str {
        "export"
    }

    fn run(&self, args: &[String], state: &mut ShellState) -> ExitStatus {
        for arg in args {
            if let Some((k, v)) = arg.split_once('=') {
                state.env.export(k, v);
                std::env::set_var(k, v);
            } else {
                // export an already-set variable
                if let Some(val) = state.env.get(arg).map(str::to_owned) {
                    state.env.export(arg, val);
                }
            }
        }
        ExitStatus::SUCCESS
    }
}
