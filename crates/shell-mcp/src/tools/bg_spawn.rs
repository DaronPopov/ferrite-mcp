//! bg_spawn and bg_attach tool implementations.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;

// ── bg_spawn ──────────────────────────────────────────────────────────────────

pub fn bg_spawn(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let cmd = args["cmd"].as_str().ok_or("bg_spawn: 'cmd' is required")?;

    // Resolve cwd: caller-supplied → cream's state → process cwd.
    let cwd = args["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    if !cwd.exists() {
        return Err(format!("bg_spawn: cwd does not exist: {}", cwd.display()));
    }

    let label = args["label"].as_str();

    let extra_env: Vec<(String, String)> = args["env"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default();

    let use_pty = args["pty"].as_bool().unwrap_or(false);

    let job = if use_pty {
        store.spawn_pty(cmd, cwd, label, extra_env)?
    } else {
        store.spawn(cmd, cwd, label, extra_env)?
    };

    let note = if use_pty {
        "PTY job — use bg_send to write to stdin, bg_status to read output."
    } else {
        "Use bg_status to poll for output, bg_wait to block, or live_window to stream."
    };

    Ok(ToolResult::json(&json!({
        "job_id":      job.job_id,
        "pid":         job.pid,
        "label":       job.label,
        "cmd":         job.cmd,
        "cwd":         job.cwd.display().to_string(),
        "log_path":    job.log_path.display().to_string(),
        "status":      "running",
        "is_pty":      job.is_pty,
        "note":        note
    })))
}

// ── bg_attach ─────────────────────────────────────────────────────────────────

pub fn bg_attach(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let pid = args["pid"]
        .as_u64()
        .ok_or("bg_attach: 'pid' is required and must be a positive integer")? as u32;

    let label = args["label"].as_str();

    let job = store.attach(pid, label)?;

    Ok(ToolResult::json(&json!({
        "job_id":  job.job_id,
        "pid":     job.pid,
        "label":   job.label,
        "status":  "attached",
        "note":    "Monitoring PID via /proc. Use bg_status to check completion. Output not captured for attached jobs."
    })))
}
