//! AST executor: walks the command list and runs commands.

use shell_core::{state::ShellState, types::ExitStatus};
use shell_hooks::{events::HookEvent, registry::HookRegistry};
use shell_parser::ast::{Command, List, Pipeline, Separator, SimpleCommand, WordPart};

use crate::{builtins, expand::expand_word, jobs::JobTable};

pub struct Executor {
    pub state: ShellState,
    pub hooks: HookRegistry,
    pub jobs: JobTable,
}

impl Executor {
    pub fn new(state: ShellState) -> Self {
        Self {
            state,
            hooks: HookRegistry::new(),
            jobs: JobTable::new(),
        }
    }

    pub fn execute_list(&mut self, list: &List) -> ExitStatus {
        let mut last = ExitStatus::SUCCESS;

        for (i, item) in list.items.iter().enumerate() {
            let should_run = match item.separator {
                // These separators govern whether the *previous* item's
                // status allows the current item to run.
                _ => {
                    if i == 0 {
                        true
                    } else {
                        let prev_sep = &list.items[i - 1].separator;
                        match prev_sep {
                            Separator::And => last.success(),
                            Separator::Or => !last.success(),
                            _ => true,
                        }
                    }
                }
            };

            if !should_run {
                continue;
            }

            last = self.execute_pipeline(&item.pipeline);
            self.state.last_status = last;
        }

        last
    }

    fn execute_pipeline(&mut self, pipeline: &Pipeline) -> ExitStatus {
        if pipeline.commands.len() == 1 {
            let status = self.execute_command(&pipeline.commands[0]);
            if pipeline.negate { ExitStatus(!status.success() as i32) } else { status }
        } else {
            // TODO: pipe setup with fork + dup2
            let mut last = ExitStatus::SUCCESS;
            for cmd in &pipeline.commands {
                last = self.execute_command(cmd);
            }
            if pipeline.negate { ExitStatus(!last.success() as i32) } else { last }
        }
    }

    fn execute_command(&mut self, command: &Command) -> ExitStatus {
        match command {
            Command::Simple(sc) => self.execute_simple(sc),
        }
    }

    fn execute_simple(&mut self, cmd: &SimpleCommand) -> ExitStatus {
        // Expand all words
        let mut expanded_words: Vec<String> = Vec::new();
        for word in &cmd.words {
            let raw: String = word.parts.iter().map(|p| match p {
                WordPart::Literal(s) => s.clone(),
                WordPart::Variable(v) => self.state.env.get(v).unwrap_or("").to_owned(),
                WordPart::CmdSubst(_) | WordPart::Glob(_) => String::new(), // TODO
            }).collect();

            match expand_word(&raw, &self.state.env) {
                Ok(words) => expanded_words.extend(words),
                Err(e) => {
                    eprintln!("ferrite: expand error: {e}");
                    return ExitStatus(1);
                }
            }
        }

        let (name, args) = match expanded_words.split_first() {
            Some(pair) => pair,
            None => return ExitStatus::SUCCESS,
        };

        // Fire PreExec hook
        self.hooks.fire(HookEvent::PreExec { cmdline: name.clone() });

        // Check builtins first
        let status = if let Some(builtin) = builtins::lookup(name) {
            builtin.run(args, &mut self.state)
        } else {
            self.spawn_external(name, args)
        };

        // Fire PostExec hook
        self.hooks.fire(HookEvent::PostExec { cmdline: name.clone(), status });

        status
    }

    fn spawn_external(&mut self, name: &str, args: &[String]) -> ExitStatus {
        use std::process::Command;

        match Command::new(name).args(args).status() {
            Ok(status) => ExitStatus(status.code().unwrap_or(1)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.hooks.fire(HookEvent::CommandNotFound { name: name.to_owned() });
                eprintln!("ferrite: {name}: command not found");
                ExitStatus(127)
            }
            Err(e) => {
                eprintln!("ferrite: {name}: {e}");
                ExitStatus(1)
            }
        }
    }
}
