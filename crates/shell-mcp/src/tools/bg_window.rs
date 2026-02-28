//! live_window tool implementation.
//!
//! Opens a kitty terminal window showing live output from a background job
//! (tail -f on the job's log file), or a ferrite interactive shell if no job_id
//! is given.
//!
//! Terminal preference order: kitty → xterm

use std::sync::Arc;

use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;
use crate::terminal;

pub fn live_window(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"].as_str();

    match job_id {
        Some(id) => open_job_window(id, args, store),
        None     => open_ferrite_window(args),
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

    // Build the shell command: colorized awk watcher with completion banner.
    // keep_open=true so the window pauses after the FERRITE_DONE banner appears.
    let shell_cmd = terminal::colorized_watch_cmd(&job.log_path, true);

    let pid = terminal::launch_terminal(&title, &shell_cmd, "auto")?;

    Ok(ToolResult::json(&json!({
        "ok":       true,
        "job_id":   job_id,
        "label":    job.label,
        "log_path": log_path,
        "window_pid": pid,
        "note":     format!("Live window opened (pid {pid}) — streaming output from job {job_id}")
    })))
}

// ── ferrite shell window ────────────────────────────────────────────────────────

fn open_ferrite_window(args: &Value) -> Result<ToolResult, String> {
    let title = args["title"].as_str().unwrap_or("ferrite").to_string();

    // Find ferrite binary
    let ferrite_bin = terminal::which_bin("ferrite").unwrap_or_else(|| "ferrite".to_string());
    let pid = terminal::launch_terminal(&title, &ferrite_bin, "auto")?;

    Ok(ToolResult::json(&json!({
        "ok":    true,
        "mode":  "ferrite-shell",
        "title": title,
        "window_pid": pid,
        "note":  format!("Cream interactive shell opened (pid {pid})")
    })))
}

