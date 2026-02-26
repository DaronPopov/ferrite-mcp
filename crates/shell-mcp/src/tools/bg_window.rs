//! live_window tool implementation.
//!
//! Opens a kitty terminal window showing live output from a background job
//! (tail -f on the job's log file), or a cream interactive shell if no job_id
//! is given.
//!
//! Terminal preference order: kitty → xterm

use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;

pub fn live_window(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"].as_str();

    match job_id {
        Some(id) => open_job_window(id, args, store),
        None     => open_cream_window(args),
    }
}

// ── job output window ─────────────────────────────────────────────────────────

fn open_job_window(job_id: &str, args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job = store.get(job_id)
        .ok_or_else(|| format!("live_window: job '{job_id}' not found"))?;

    let title = args["title"]
        .as_str()
        .unwrap_or(&job.label)
        .to_string();

    // The log file must exist — bg_spawn creates it immediately.
    // If not (e.g. bg_attach job or log write failed), surface a clear error.
    if !job.log_path.exists() {
        return Ok(ToolResult::json(&json!({
            "ok":      false,
            "job_id":  job_id,
            "message": format!(
                "No log file at {} — this job has no captured output (was it created with bg_attach?)",
                job.log_path.display()
            ),
        })));
    }

    let log_path = job.log_path.display().to_string();

    // Build the shell command that tail -f the log.
    // We wrap in a shell so the window stays open after the process ends.
    let shell_cmd = format!(
        "tail -f --retry '{}'; echo; echo '--- job {job_id} ended ---'; read -r",
        log_path
    );

    let pid = launch_terminal(&title, &shell_cmd)?;

    Ok(ToolResult::json(&json!({
        "ok":       true,
        "job_id":   job_id,
        "label":    job.label,
        "log_path": log_path,
        "window_pid": pid,
        "note":     format!("Live window opened (pid {pid}) — streaming output from job {job_id}")
    })))
}

// ── cream shell window ────────────────────────────────────────────────────────

fn open_cream_window(args: &Value) -> Result<ToolResult, String> {
    let title = args["title"].as_str().unwrap_or("cream").to_string();

    // Find cream binary
    let cream_bin = which("cream").unwrap_or_else(|| "cream".to_string());
    let pid = launch_terminal(&title, &cream_bin)?;

    Ok(ToolResult::json(&json!({
        "ok":    true,
        "mode":  "cream-shell",
        "title": title,
        "window_pid": pid,
        "note":  format!("Cream interactive shell opened (pid {pid})")
    })))
}

// ── terminal launcher ─────────────────────────────────────────────────────────

/// Launch a terminal running `cmd`. Returns the terminal's PID.
fn launch_terminal(title: &str, cmd: &str) -> Result<u32, String> {
    // Try kitty first, fall back to xterm.
    if let Some(kitty) = which("kitty") {
        return spawn_detached(&[
            &kitty,
            "--title", title,
            "--",
            "/bin/sh", "-c", cmd,
        ]);
    }

    if let Some(xterm) = which("xterm") {
        return spawn_detached(&[
            &xterm,
            "-title", title,
            "-e",
            "/bin/sh", "-c", cmd,
        ]);
    }

    Err("live_window: no suitable terminal found (tried kitty, xterm). \
         Install kitty or xterm and ensure it is in PATH.".to_string())
}

fn spawn_detached(args: &[&str]) -> Result<u32, String> {
    let (bin, rest) = args.split_first().ok_or("empty args")?;

    let child = Command::new(bin)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("live_window: failed to spawn terminal: {e}"))?;

    let pid = child.id();
    // Detach — the terminal manages its own lifetime.
    std::mem::drop(child);
    Ok(pid)
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn which(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| std::path::PathBuf::from(d).join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}
