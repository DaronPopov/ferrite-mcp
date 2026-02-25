use shell_core::{state::ShellState, types::ExitStatus};
use super::Builtin;

pub struct Echo;

impl Builtin for Echo {
    fn name(&self) -> &'static str { "echo" }

    fn run(&self, args: &[String], _state: &mut ShellState) -> ExitStatus {
        let mut no_newline = false;
        let mut start = 0;

        if args.first().map(String::as_str) == Some("-n") {
            no_newline = true;
            start = 1;
        }

        let output = args[start..].join(" ");
        if no_newline {
            print!("{output}");
        } else {
            println!("{output}");
        }

        ExitStatus::SUCCESS
    }
}
