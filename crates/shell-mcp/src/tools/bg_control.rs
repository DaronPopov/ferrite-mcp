//! bg_kill tool implementation.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::job_store::{JobStatus, JobStore};
use crate::protocol::ToolResult;

pub fn bg_kill(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"].as_str().ok_or("bg_kill: 'job_id' is required")?;
    let signal  = args["signal"].as_str().unwrap_or("TERM");

    let job = store.get(job_id)
        .ok_or_else(|| format!("bg_kill: job '{job_id}' not found"))?;

    // Don't kill a job that's already done.
    let current = job.current_status();
    if current.is_terminal() {
        return Ok(ToolResult::json(&json!({
            "ok":      false,
            "job_id":  job_id,
            "message": format!("job is already in terminal state: {}", current.label()),
        })));
    }

    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let sig = match signal {
        "KILL" => Signal::SIGKILL,
        _      => Signal::SIGTERM,
    };

    let pid = Pid::from_raw(job.pid as i32);
    match kill(pid, sig) {
        Ok(()) => {
            *job.status.lock().unwrap() = JobStatus::Killed;
            Ok(ToolResult::json(&json!({
                "ok":      true,
                "job_id":  job_id,
                "pid":     job.pid,
                "signal":  signal,
                "message": format!("SIG{signal} sent to pid {}", job.pid),
            })))
        }
        Err(e) => Ok(ToolResult::json(&json!({
            "ok":      false,
            "job_id":  job_id,
            "pid":     job.pid,
            "message": format!("kill failed: {e}"),
        }))),
    }
}
