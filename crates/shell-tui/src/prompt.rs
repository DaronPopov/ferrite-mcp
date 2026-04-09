//! Prompt rendering — builds the two-line cream-themed prompt.
//!
//! Visual design:
//!
//! ```text
//!  ~/projects/foo  main                   (line 1: cwd + git branch)
//!  ▸                                      (line 2: arrow in ok/fail color)
//! ```

use crossterm::style::{Print, ResetColor, SetForegroundColor};
use shell_core::state::ShellState;

use crate::theme::*;

/// Render the full prompt to stdout.
pub fn render(state: &ShellState) {
    let cwd = pretty_cwd(state);
    let branch = git_branch().unwrap_or_default();
    let arrow_color = if state.last_status.success() {
        COLOR_PROMPT_OK
    } else {
        COLOR_PROMPT_FAIL
    };

    let mut out = std::io::stdout();

    // Line 1
    let _ = crossterm::execute!(
        out,
        SetForegroundColor(COLOR_PROMPT_CWD),
        Print(&cwd),
        SetForegroundColor(DUST),
        Print(if branch.is_empty() {
            String::new()
        } else {
            format!("  {branch}")
        }),
        ResetColor,
        Print("\n"),
    );

    // Line 2 — the arrow
    let _ = crossterm::execute!(
        out,
        SetForegroundColor(arrow_color),
        Print("▸ "),
        ResetColor,
    );
}

fn pretty_cwd(state: &ShellState) -> String {
    let home = state.env.get("HOME").unwrap_or("").to_owned();
    let path = state.cwd.display().to_string();
    if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path
    }
}

fn git_branch() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}
