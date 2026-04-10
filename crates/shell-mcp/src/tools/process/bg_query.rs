//! bg_status, bg_wait, bg_tail, bg_list, wait_for_pattern, wait_for_idle,
//! output_summary tool implementations.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;
use crate::tools::state::lock_mutex;

// ── bg_status ─────────────────────────────────────────────────────────────────

pub fn bg_status(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"]
        .as_str()
        .ok_or("bg_status: 'job_id' is required")?;
    let from_start = args["from_start"].as_bool().unwrap_or(false);

    let job = store
        .get(job_id)
        .ok_or_else(|| format!("bg_status: job '{job_id}' not found"))?;

    let status = job.current_status();

    let (new_stdout, new_stderr) = if from_start {
        {
            let buf = lock_mutex(&job.stdout_buf, "job stdout buffer");
            *lock_mutex(&job.stdout_cursor, "job stdout cursor") = buf.len();
        }
        {
            let buf = lock_mutex(&job.stderr_buf, "job stderr buffer");
            *lock_mutex(&job.stderr_cursor, "job stderr cursor") = buf.len();
        }
        (job.full_stdout(), job.full_stderr())
    } else {
        (job.drain_new_stdout(), job.drain_new_stderr())
    };

    // Lazily persist after status changes are observed.
    if status.is_terminal() {
        store.persist_all();
    }

    Ok(ToolResult::json(&json!({
        "job_id":             job.job_id,
        "pid":                job.pid,
        "label":              job.label,
        "status":             status.label(),
        "exit_code":          status.exit_code(),
        "elapsed_secs":       round1(job.elapsed_secs()),
        "new_stdout":         String::from_utf8_lossy(&new_stdout),
        "new_stderr":         String::from_utf8_lossy(&new_stderr),
        "new_stdout_bytes":   new_stdout.len(),
        "new_stderr_bytes":   new_stderr.len(),
        "total_stdout_bytes": job.stdout_bytes(),
        "total_stderr_bytes": job.stderr_bytes(),
        "log_path":           job.log_path.display().to_string(),
    })))
}

// ── bg_wait ───────────────────────────────────────────────────────────────────

pub fn bg_wait(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"]
        .as_str()
        .ok_or("bg_wait: 'job_id' is required")?;
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(3600);

    let job = store
        .get(job_id)
        .ok_or_else(|| format!("bg_wait: job '{job_id}' not found"))?;

    let completed = job.wait(Duration::from_secs(timeout_secs));
    let status = job.current_status();
    store.persist_all();

    Ok(ToolResult::json(&json!({
        "job_id":       job.job_id,
        "pid":          job.pid,
        "label":        job.label,
        "status":       status.label(),
        "exit_code":    status.exit_code(),
        "timed_out":    !completed,
        "elapsed_secs": round1(job.elapsed_secs()),
        "stdout":       String::from_utf8_lossy(&job.full_stdout()),
        "stderr":       String::from_utf8_lossy(&job.full_stderr()),
        "stdout_bytes": job.stdout_bytes(),
        "stderr_bytes": job.stderr_bytes(),
    })))
}

// ── bg_tail ───────────────────────────────────────────────────────────────────

pub fn bg_tail(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"]
        .as_str()
        .ok_or("bg_tail: 'job_id' is required")?;
    let lines = args["lines"].as_u64().unwrap_or(50) as usize;

    let job = store
        .get(job_id)
        .ok_or_else(|| format!("bg_tail: job '{job_id}' not found"))?;

    let output = if job.log_path.exists() {
        match std::fs::read_to_string(&job.log_path) {
            Ok(content) => last_n_lines(&content, lines),
            Err(e) => format!("[log read error: {e}]"),
        }
    } else {
        last_n_lines(&String::from_utf8_lossy(&job.full_stdout()), lines)
    };

    let status = job.current_status();
    Ok(ToolResult::json(&json!({
        "job_id":          job.job_id,
        "label":           job.label,
        "status":          status.label(),
        "elapsed_secs":    round1(job.elapsed_secs()),
        "lines_requested": lines,
        "output":          output,
    })))
}

// ── bg_list ───────────────────────────────────────────────────────────────────

pub fn bg_list(_args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let jobs = store.all();

    if jobs.is_empty() {
        return Ok(ToolResult::json(&json!({
            "jobs": [],
            "note": "No background jobs. Use bg_spawn to start one."
        })));
    }

    // Persist any status changes we observe during listing.
    let mut any_terminal_observed = false;
    let list: Vec<Value> = jobs
        .iter()
        .map(|job| {
            let status = job.current_status();
            if status.is_terminal() {
                any_terminal_observed = true;
            }
            json!({
                "job_id":       job.job_id,
                "pid":          job.pid,
                "label":        job.label,
                "cmd":          job.cmd,
                "status":       status.label(),
                "exit_code":    status.exit_code(),
                "elapsed_secs": round1(job.elapsed_secs()),
                "stdout_bytes": job.stdout_bytes(),
                "stderr_bytes": job.stderr_bytes(),
                "log_path":     job.log_path.display().to_string(),
            })
        })
        .collect();

    if any_terminal_observed {
        store.persist_all();
    }

    let running = list.iter().filter(|j| j["status"] == "running").count();
    let done = list.iter().filter(|j| j["status"] == "done").count();
    let failed = list.iter().filter(|j| j["status"] == "failed").count();

    Ok(ToolResult::json(&json!({
        "jobs":    list,
        "summary": { "total": jobs.len(), "running": running, "done": done, "failed": failed }
    })))
}

// ── wait_for_pattern ─────────────────────────────────────────────────────────

pub fn wait_for_pattern(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"]
        .as_str()
        .ok_or("wait_for_pattern: 'job_id' is required")?;
    let pattern = args["pattern"]
        .as_str()
        .ok_or("wait_for_pattern: 'pattern' is required")?;
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(60);
    let from_cursor = args["from_cursor"].as_bool().unwrap_or(false);
    let check_stderr = args["include_stderr"].as_bool().unwrap_or(true);

    let job = store
        .get(job_id)
        .ok_or_else(|| format!("wait_for_pattern: job '{job_id}' not found"))?;

    let re = Regex::new(pattern)
        .map_err(|e| format!("wait_for_pattern: invalid regex '{pattern}': {e}"))?;

    // Where to start searching in the buffer.
    let search_from: usize = if from_cursor {
        *lock_mutex(&job.stdout_cursor, "job stdout cursor")
    } else {
        0
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    loop {
        // Search stdout from search_from.
        let (found, match_line, line_num) = {
            let buf = lock_mutex(&job.stdout_buf, "job stdout buffer");
            let text = String::from_utf8_lossy(&buf[search_from..]);
            find_first_match(&text, &re)
        };

        if found {
            store.persist_all();
            return Ok(ToolResult::json(&json!({
                "matched":     true,
                "stream":      "stdout",
                "match_line":  match_line,
                "line_number": line_num,
                "elapsed_secs": round1(deadline_elapsed(deadline, Duration::from_secs(timeout_secs))),
                "job_id":      job.job_id,
                "status":      job.current_status().label(),
            })));
        }

        // Optionally search stderr.
        if check_stderr {
            let (found, match_line, line_num) = {
                let buf = lock_mutex(&job.stderr_buf, "job stderr buffer");
                let from = if from_cursor {
                    *lock_mutex(&job.stderr_cursor, "job stderr cursor")
                } else {
                    0
                };
                let text = String::from_utf8_lossy(&buf[from..]);
                find_first_match(&text, &re)
            };
            if found {
                store.persist_all();
                return Ok(ToolResult::json(&json!({
                    "matched":     true,
                    "stream":      "stderr",
                    "match_line":  match_line,
                    "line_number": line_num,
                    "elapsed_secs": round1(deadline_elapsed(deadline, Duration::from_secs(timeout_secs))),
                    "job_id":      job.job_id,
                    "status":      job.current_status().label(),
                })));
            }
        }

        // If job is done and we haven't matched, we won't match.
        if job.current_status().is_terminal() {
            store.persist_all();
            return Ok(ToolResult::json(&json!({
                "matched":           false,
                "reason":            "job_ended_without_match",
                "elapsed_secs":      round1(deadline_elapsed(deadline, Duration::from_secs(timeout_secs))),
                "job_id":            job.job_id,
                "exit_code":         job.current_status().exit_code(),
                "total_stdout_bytes": job.stdout_bytes(),
            })));
        }

        if Instant::now() >= deadline {
            return Ok(ToolResult::json(&json!({
                "matched":     false,
                "reason":      "timeout",
                "timed_out":   true,
                "job_id":      job.job_id,
                "status":      job.current_status().label(),
            })));
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── wait_for_idle ─────────────────────────────────────────────────────────────

pub fn wait_for_idle(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"]
        .as_str()
        .ok_or("wait_for_idle: 'job_id' is required")?;
    let idle_secs = args["idle_secs"].as_u64().unwrap_or(2);
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(300);

    let job = store
        .get(job_id)
        .ok_or_else(|| format!("wait_for_idle: job '{job_id}' not found"))?;

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let idle_window = Duration::from_secs(idle_secs);

    let mut last_total = job.stdout_bytes() + job.stderr_bytes();
    let mut quiet_since = Instant::now();

    loop {
        let current_total = job.stdout_bytes() + job.stderr_bytes();
        if current_total > last_total {
            last_total = current_total;
            quiet_since = Instant::now();
        }

        // Job ended — that's inherently "idle".
        if job.current_status().is_terminal() {
            store.persist_all();
            return Ok(ToolResult::json(&json!({
                "idle_reached": true,
                "reason":       "job_ended",
                "elapsed_secs": round1(job.elapsed_secs()),
                "job_id":       job.job_id,
                "exit_code":    job.current_status().exit_code(),
            })));
        }

        if quiet_since.elapsed() >= idle_window {
            return Ok(ToolResult::json(&json!({
                "idle_reached":     true,
                "reason":           "output_quiet",
                "quiet_for_secs":   idle_secs,
                "elapsed_secs":     round1(job.elapsed_secs()),
                "job_id":           job.job_id,
                "total_bytes":      current_total,
            })));
        }

        if Instant::now() >= deadline {
            return Ok(ToolResult::json(&json!({
                "idle_reached": false,
                "reason":       "timeout",
                "timed_out":    true,
                "job_id":       job.job_id,
                "total_bytes":  current_total,
            })));
        }

        std::thread::sleep(Duration::from_millis(200));
    }
}

// ── output_summary ────────────────────────────────────────────────────────────

pub fn output_summary(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let job_id = args["job_id"]
        .as_str()
        .ok_or("output_summary: 'job_id' is required")?;
    let head_lines = args["head_lines"].as_u64().unwrap_or(20) as usize;
    let tail_lines = args["tail_lines"].as_u64().unwrap_or(30) as usize;
    let err_pattern = args["error_pattern"]
        .as_str()
        .unwrap_or(r"(?i)(error|fatal|panic|failed|traceback|exception)");
    let warn_pattern = args["warn_pattern"]
        .as_str()
        .unwrap_or(r"(?i)(warning|warn\b|deprecated)");

    let job = store
        .get(job_id)
        .ok_or_else(|| format!("output_summary: job '{job_id}' not found"))?;

    let err_re =
        Regex::new(err_pattern).map_err(|e| format!("output_summary: bad error_pattern: {e}"))?;
    let warn_re =
        Regex::new(warn_pattern).map_err(|e| format!("output_summary: bad warn_pattern: {e}"))?;

    // Read combined log for interleaved output — fall back to stdout if missing.
    let combined = if job.log_path.exists() {
        std::fs::read_to_string(&job.log_path).unwrap_or_default()
    } else {
        String::from_utf8_lossy(&job.full_stdout()).into_owned()
    };

    let all_lines: Vec<&str> = combined.lines().collect();
    let total_lines = all_lines.len();

    let mut errors: Vec<&str> = Vec::new();
    let mut warnings: Vec<&str> = Vec::new();

    for line in &all_lines {
        if err_re.is_match(line) {
            errors.push(line);
        } else if warn_re.is_match(line) {
            warnings.push(line);
        }
    }

    // Cap error/warning lists to avoid flooding context.
    let error_cap = 50;
    let warning_cap = 30;
    let errors_truncated = errors.len() > error_cap;
    let warnings_truncated = warnings.len() > warning_cap;
    errors.truncate(error_cap);
    warnings.truncate(warning_cap);

    let head = all_lines[..head_lines.min(total_lines)].join("\n");
    let tail_start = total_lines.saturating_sub(tail_lines);
    let tail = all_lines[tail_start..].join("\n");

    let status = job.current_status();

    Ok(ToolResult::json(&json!({
        "job_id":      job.job_id,
        "label":       job.label,
        "status":      status.label(),
        "exit_code":   status.exit_code(),
        "elapsed_secs": round1(job.elapsed_secs()),
        "stats": {
            "total_lines":   total_lines,
            "total_bytes":   combined.len(),
            "error_count":   errors.len(),
            "warning_count": warnings.len(),
            "errors_truncated_at":   if errors_truncated   { Some(error_cap)   } else { None },
            "warnings_truncated_at": if warnings_truncated { Some(warning_cap) } else { None },
        },
        "errors":   errors,
        "warnings": warnings,
        "head":     head,
        "tail":     tail,
    })))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn last_n_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Find first line matching `re`. Returns (found, line_text, line_number_1indexed).
fn find_first_match(text: &str, re: &Regex) -> (bool, String, usize) {
    for (i, line) in text.lines().enumerate() {
        if re.is_match(line) {
            return (true, line.to_string(), i + 1);
        }
    }
    (false, String::new(), 0)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn deadline_elapsed(deadline: Instant, total: Duration) -> f64 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    (total.as_secs_f64() - remaining.as_secs_f64()).max(0.0)
}
