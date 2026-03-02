//! tmux_ctl — create, query, and send commands to tmux sessions.
//!
//! Operations:
//!   new     — create a new detached session (idempotent)
//!   list    — list all sessions + windows
//!   send    — send a command string to a session (Enter appended)
//!   capture — capture visible pane output
//!   kill    — kill a session
//!   has     — check if a session exists (returns bool, no error)

use std::time::Duration;
use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::tools::execution::run;

pub fn tmux_ctl(args: &Value) -> Result<ToolResult, String> {
    let op = args["op"].as_str().ok_or("tmux_ctl: 'op' is required (new|list|send|capture|kill|has)")?;

    match op {
        "new"     => tmux_new(args),
        "list"    => tmux_list(),
        "send"    => tmux_send(args),
        "capture" => tmux_capture(args),
        "kill"    => tmux_kill(args),
        "has"     => tmux_has(args),
        other     => Err(format!("tmux_ctl: unknown op '{other}' — use new|list|send|capture|kill|has")),
    }
}

// ── new ───────────────────────────────────────────────────────────────────────

fn tmux_new(args: &Value) -> Result<ToolResult, String> {
    let session = args["session"].as_str().unwrap_or("main");
    let start_cmd = args["cmd"].as_str().unwrap_or("");
    let cwd_str = args["cwd"].as_str().unwrap_or("~");
    let cwd = crate::tools::project::expand_tilde(cwd_str);

    // Check if already exists
    let exists = session_exists(session);
    if exists {
        return Ok(ToolResult::json(&json!({
            "created": false,
            "session": session,
            "note":    "session already exists — use op:send to run commands in it",
        })));
    }

    let cmd = if start_cmd.is_empty() {
        format!("tmux new-session -d -s '{}' -c '{}'", session, cwd.display())
    } else {
        format!("tmux new-session -d -s '{}' -c '{}' '{}'", session, cwd.display(), start_cmd)
    };

    let base = std::path::PathBuf::from("/");
    let raw = run(&cmd, &base, &[], "", Duration::from_secs(10));
    let success = raw["success"].as_bool().unwrap_or(false);

    if !success {
        return Err(format!("tmux new-session failed: {}", raw["stderr"].as_str().unwrap_or("")));
    }

    Ok(ToolResult::json(&json!({
        "created": true,
        "session": session,
        "cwd":     cwd.display().to_string(),
        "note":    format!("use op:send to run commands — or: tmux attach -t {session}"),
    })))
}

// ── list ──────────────────────────────────────────────────────────────────────

fn tmux_list() -> Result<ToolResult, String> {
    let base = std::path::PathBuf::from("/");
    // Sessions
    let sess_raw = run(
        "tmux list-sessions -F '#{session_name}|#{session_windows}|#{session_attached}|#{session_created_string}'",
        &base, &[], "", Duration::from_secs(5),
    );

    if !sess_raw["success"].as_bool().unwrap_or(false) {
        let stderr = sess_raw["stderr"].as_str().unwrap_or("");
        let stdout = sess_raw["stdout"].as_str().unwrap_or("");
        let err_text = format!("{stderr}\n{stdout}");
        if err_text.contains("no server running")
            || err_text.contains("No such file")
            || err_text.contains("failed to connect")
            || err_text.contains("error connecting")
            || err_text.trim().is_empty()
        {
            return Ok(ToolResult::json(&json!({
                "running":  false,
                "sessions": [],
                "note":     "tmux server not running — use op:new to create a session",
            })));
        }
        return Err(format!("tmux list-sessions: {}", err_text.trim()));
    }

    let sessions: Vec<Value> = sess_raw["stdout"].as_str().unwrap_or("").lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            json!({
                "name":     parts.first().copied().unwrap_or(""),
                "windows":  parts.get(1).copied().unwrap_or("0").parse::<u64>().unwrap_or(0),
                "attached": parts.get(2).copied().unwrap_or("0") == "1",
                "created":  parts.get(3).copied().unwrap_or(""),
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "running":        true,
        "session_count":  sessions.len(),
        "sessions":       sessions,
    })))
}

// ── send ──────────────────────────────────────────────────────────────────────

fn tmux_send(args: &Value) -> Result<ToolResult, String> {
    let session = args["session"].as_str().unwrap_or("main");
    let window  = args["window"].as_str().unwrap_or("");
    let cmd     = args["cmd"].as_str().ok_or("tmux_ctl send: 'cmd' is required")?;
    let enter   = args["enter"].as_bool().unwrap_or(true);

    // Target: session or session:window.pane
    let target = if window.is_empty() {
        format!("{session}")
    } else {
        format!("{session}:{window}")
    };

    let keys = if enter {
        format!("tmux send-keys -t '{}' {} Enter", target, shell_quote(cmd))
    } else {
        format!("tmux send-keys -t '{}' {}", target, shell_quote(cmd))
    };

    let base = std::path::PathBuf::from("/");
    let raw = run(&keys, &base, &[], "", Duration::from_secs(10));
    let success = raw["success"].as_bool().unwrap_or(false);

    if !success {
        return Err(format!("tmux send-keys failed: {}", raw["stderr"].as_str().unwrap_or("")));
    }

    Ok(ToolResult::json(&json!({
        "success": true,
        "session": session,
        "target":  target,
        "cmd":     cmd,
        "note":    "use op:capture to read pane output after the command runs",
    })))
}

// ── capture ───────────────────────────────────────────────────────────────────

fn tmux_capture(args: &Value) -> Result<ToolResult, String> {
    let session  = args["session"].as_str().unwrap_or("main");
    let window   = args["window"].as_str().unwrap_or("");
    let lines    = args["lines"].as_u64().unwrap_or(50);

    let target = if window.is_empty() {
        session.to_owned()
    } else {
        format!("{session}:{window}")
    };

    // capture-pane -p prints to stdout; -S -N gets last N lines from scrollback
    let cmd = format!(
        "tmux capture-pane -t '{}' -p -S -{lines}",
        target
    );

    let base = std::path::PathBuf::from("/");
    let raw = run(&cmd, &base, &[], "", Duration::from_secs(5));

    if !raw["success"].as_bool().unwrap_or(false) {
        return Err(format!("tmux capture-pane: {}", raw["stderr"].as_str().unwrap_or("")));
    }

    let output = raw["stdout"].as_str().unwrap_or("").to_owned();

    Ok(ToolResult::json(&json!({
        "session": session,
        "target":  target,
        "lines":   lines,
        "output":  output,
    })))
}

// ── kill ──────────────────────────────────────────────────────────────────────

fn tmux_kill(args: &Value) -> Result<ToolResult, String> {
    let session = args["session"].as_str().ok_or("tmux_ctl kill: 'session' is required")?;

    if !session_exists(session) {
        return Ok(ToolResult::json(&json!({
            "killed":  false,
            "session": session,
            "note":    "session did not exist",
        })));
    }

    let cmd = format!("tmux kill-session -t '{session}'");
    let base = std::path::PathBuf::from("/");
    let raw = run(&cmd, &base, &[], "", Duration::from_secs(5));
    let success = raw["success"].as_bool().unwrap_or(false);

    Ok(ToolResult::json(&json!({
        "killed":  success,
        "session": session,
    })))
}

// ── has ───────────────────────────────────────────────────────────────────────

fn tmux_has(args: &Value) -> Result<ToolResult, String> {
    let session = args["session"].as_str().ok_or("tmux_ctl has: 'session' is required")?;
    Ok(ToolResult::json(&json!({
        "exists":  session_exists(session),
        "session": session,
    })))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn session_exists(session: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
