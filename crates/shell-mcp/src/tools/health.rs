use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;
use crate::server::{HealthSnapshot, RecyclePolicy, ServerMetrics, ServerState};

const RECOMMENDED_MAX_CALLS: u64 = 400;
const RECOMMENDED_MAX_UPTIME_SECS: u64 = 2 * 60 * 60;
const RECOMMENDED_MAX_RSS_BYTES: u64 = 512 * 1024 * 1024;
const RECOMMENDED_MAX_BUFFERED_JOB_BYTES: usize = 128 * 1024 * 1024;
const RECOMMENDED_MAX_NOTE_BYTES: usize = 256 * 1024;

pub fn health(
    _args: &Value,
    metrics: &Arc<ServerMetrics>,
    state: &Arc<Mutex<ServerState>>,
    store: &Arc<JobStore>,
) -> Result<ToolResult, String> {
    let snapshot = HealthSnapshot::collect(metrics, state, store);
    let recycle_policy = RecyclePolicy::from_env();

    let mut reasons = Vec::new();
    if snapshot.total_tool_calls >= RECOMMENDED_MAX_CALLS {
        reasons.push(format!(
            "tool calls {} >= recommended {}",
            snapshot.total_tool_calls, RECOMMENDED_MAX_CALLS
        ));
    }
    if snapshot.uptime_secs >= RECOMMENDED_MAX_UPTIME_SECS {
        reasons.push(format!(
            "uptime {}s >= recommended {}s",
            snapshot.uptime_secs, RECOMMENDED_MAX_UPTIME_SECS
        ));
    }
    if snapshot
        .rss_bytes
        .map(|rss| rss >= RECOMMENDED_MAX_RSS_BYTES)
        .unwrap_or(false)
    {
        reasons.push(format!(
            "RSS {} MB >= recommended {} MB",
            snapshot.rss_bytes.unwrap_or(0) / 1024 / 1024,
            RECOMMENDED_MAX_RSS_BYTES / 1024 / 1024
        ));
    }
    if snapshot.job_stats.buffered_bytes >= RECOMMENDED_MAX_BUFFERED_JOB_BYTES {
        reasons.push(format!(
            "job buffers {} MB >= recommended {} MB",
            snapshot.job_stats.buffered_bytes / 1024 / 1024,
            RECOMMENDED_MAX_BUFFERED_JOB_BYTES / 1024 / 1024
        ));
    }
    if snapshot.note_bytes >= RECOMMENDED_MAX_NOTE_BYTES {
        reasons.push(format!(
            "notes {} KB >= recommended {} KB",
            snapshot.note_bytes / 1024,
            RECOMMENDED_MAX_NOTE_BYTES / 1024
        ));
    }

    let auto_recycle_enabled = !recycle_policy.is_disabled();
    let auto_recycle_now = recycle_policy.should_recycle(&snapshot);

    Ok(ToolResult::json(&json!({
        "healthy": reasons.is_empty(),
        "restart_recommended": !reasons.is_empty(),
        "recommendation_reasons": reasons,
        "server": {
            "pid": std::process::id(),
            "uptime_secs": snapshot.uptime_secs,
            "total_tool_calls": snapshot.total_tool_calls,
            "rss_bytes": snapshot.rss_bytes,
            "rss_mb": snapshot.rss_bytes.map(|v| v / 1024 / 1024),
        },
        "notes": {
            "count": snapshot.note_count,
            "bytes": snapshot.note_bytes,
        },
        "jobs": {
            "total": snapshot.job_stats.total_jobs,
            "running": snapshot.job_stats.running_jobs,
            "attached": snapshot.job_stats.attached_jobs,
            "done": snapshot.job_stats.done_jobs,
            "killed": snapshot.job_stats.killed_jobs,
            "stdout_bytes": snapshot.job_stats.stdout_bytes,
            "stderr_bytes": snapshot.job_stats.stderr_bytes,
            "buffered_bytes": snapshot.job_stats.buffered_bytes,
            "buffered_mb": snapshot.job_stats.buffered_bytes / 1024 / 1024,
        },
        "auto_recycle": {
            "enabled": auto_recycle_enabled,
            "triggered_now": auto_recycle_now,
            "thresholds": {
                "max_calls": recycle_policy.max_calls,
                "max_uptime_secs": recycle_policy.max_uptime_secs,
                "max_rss_mb": recycle_policy.max_rss_bytes.map(|v| v / 1024 / 1024),
            },
            "env": {
                "FERRITE_MCP_MAX_CALLS": "restart after N tool calls",
                "FERRITE_MCP_MAX_UPTIME_SECS": "restart after N seconds uptime",
                "FERRITE_MCP_MAX_RSS_MB": "restart after N MB resident set size",
            }
        }
    })))
}
