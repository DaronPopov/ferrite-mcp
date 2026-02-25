use shell_core::{state::ShellState, types::ExitStatus};
use super::Builtin;

pub struct Cd;

impl Builtin for Cd {
    fn name(&self) -> &'static str { "cd" }

    fn run(&self, args: &[String], state: &mut ShellState) -> ExitStatus {
        let target = match args.first() {
            Some(path) => std::path::PathBuf::from(path),
            None => {
                let home = state.env.get("HOME").unwrap_or("/").to_owned();
                std::path::PathBuf::from(home)
            }
        };

        match std::env::set_current_dir(&target) {
            Ok(_) => {
                state.cwd = target.canonicalize().unwrap_or(target);
                ExitStatus::SUCCESS
            }
            Err(e) => {
                eprintln!("cd: {e}");
                ExitStatus(1)
            }
        }
    }
}
