//! Tab completion engine.
//!
//! Completion sources (in priority order):
//!   1. Hook-injected completions (LLM / custom plugins via shell-hooks)
//!   2. Command name from PATH
//!   3. File/directory path completion
//!   4. Shell variable names

use shell_core::state::ShellState;
use shell_hooks::{events::HookEvent, registry::HookRegistry};

pub struct Completer;

impl Completer {
    /// Generate completion candidates for the given input up to `cursor`.
    pub fn complete(
        input: &str,
        cursor: usize,
        state: &ShellState,
        hooks: &mut HookRegistry,
    ) -> Vec<String> {
        let prefix = &input[..cursor.min(input.len())];
        let word = last_word(prefix);

        let mut candidates: Vec<String> = Vec::new();

        // Fire completion hook — plugins (eventually LLM) may inject suggestions
        let event = HookEvent::Complete {
            input: input.to_owned(),
            cursor,
            completions: candidates.clone(),
        };
        hooks.fire(event);

        // File/path completion
        candidates.extend(complete_path(word));

        // If at the first word position, also complete command names
        if !word.contains('/') && is_command_position(prefix) {
            candidates.extend(complete_commands(word, state));
        }

        candidates.sort();
        candidates.dedup();
        candidates
    }
}

fn last_word(s: &str) -> &str {
    s.split_whitespace().last().unwrap_or("")
}

fn is_command_position(prefix: &str) -> bool {
    let trimmed = prefix.trim_start();
    !trimmed.contains(' ')
        || trimmed.ends_with("| ")
        || trimmed.ends_with("&& ")
        || trimmed.ends_with("|| ")
}

fn complete_path(prefix: &str) -> Vec<String> {
    let (dir, file_prefix) = if let Some(pos) = prefix.rfind('/') {
        (&prefix[..=pos], &prefix[pos + 1..])
    } else {
        (".", prefix)
    };

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(file_prefix) {
                let mut completion = if dir == "." {
                    name.clone()
                } else {
                    format!("{dir}{name}")
                };
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    completion.push('/');
                }
                Some(completion)
            } else {
                None
            }
        })
        .collect()
}

fn complete_commands(prefix: &str, state: &ShellState) -> Vec<String> {
    let path_var = state.env.get("PATH").unwrap_or("").to_owned();
    let mut results = Vec::new();

    for dir in path_var.split(':') {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(prefix) {
                results.push(name);
            }
        }
    }

    results
}
