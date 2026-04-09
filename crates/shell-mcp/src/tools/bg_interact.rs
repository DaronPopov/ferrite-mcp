//! bg_send — send text to a PTY job's stdin.

use std::io::Write;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;
use crate::tools::state::lock_mutex;

// ── bg_send ───────────────────────────────────────────────────────────────────

pub fn bg_send(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"]
        .as_str()
        .ok_or("bg_send: 'job_id' required")?;
    let text = args["text"].as_str().ok_or("bg_send: 'text' required")?;
    let append_newline = args["newline"].as_bool().unwrap_or(true);

    let job = store
        .get(job_id)
        .ok_or_else(|| format!("bg_send: job '{job_id}' not found"))?;

    if !job.is_pty {
        return Err(format!(
            "bg_send: job '{job_id}' was not spawned with pty:true — \
             use bg_spawn with pty:true to create an interactive session"
        ));
    }

    let stdin_tx = job
        .stdin_tx
        .as_ref()
        .ok_or_else(|| format!("bg_send: job '{job_id}' has no stdin handle (already closed?)"))?;

    let mut writer = lock_mutex(stdin_tx, "pty stdin");
    writer
        .write_all(text.as_bytes())
        .map_err(|e| format!("bg_send: write error: {e}"))?;
    if append_newline && !text.ends_with('\n') {
        writer
            .write_all(b"\n")
            .map_err(|e| format!("bg_send: write newline: {e}"))?;
    }
    writer.flush().map_err(|e| format!("bg_send: flush: {e}"))?;

    let sent = text.len()
        + if append_newline && !text.ends_with('\n') {
            1
        } else {
            0
        };
    Ok(ToolResult::json(&json!({
        "job_id":     job_id,
        "sent_bytes": sent,
        "text":       text,
        "note":       "Use bg_status to read the response output."
    })))
}
