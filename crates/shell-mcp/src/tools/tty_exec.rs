//! `tty_exec` — PTY-aware command execution with automatic prompt response.
//!
//! Solves the last major class of interactive blocker: programs that open
//! /dev/tty directly and cannot be silenced by env vars alone
//! (custom installers, license prompts, y/n confirmation loops, etc.).
//!
//! The agent supplies a `responses` map of {pattern → answer}. As the
//! program prints output, tty_exec sends the matching answer to stdin
//! the moment the pattern appears — exactly like `expect` but embedded.
//!
//! `default_yes: true` pre-loads a table of common affirmative responses
//! so the agent doesn't have to enumerate them every time.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::job_store::{JobStatus, JobStore};
use crate::permissions;
use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::state::resolve_or_cwd;

// ── Default response table (default_yes mode) ─────────────────────────────────
//
// Patterns are matched case-insensitively as substrings of the last 512 bytes
// of accumulated PTY output. When a pattern fires, the paired answer is sent.
//
// Order matters: more specific patterns first.
const DEFAULT_YES_RESPONSES: &[(&str, &str)] = &[
    // Licence / acceptance
    ("do you accept",              "y\n"),
    ("accept the license",         "y\n"),
    ("i have read",                "y\n"),
    // Confirmation / destructive ops
    ("are you sure",               "y\n"),
    ("are you absolutely sure",    "y\n"),
    ("do you want to continue",    "y\n"),
    ("do you want to proceed",     "y\n"),
    ("do you wish to continue",    "y\n"),
    ("proceed?",                   "y\n"),
    // Standard y/n prompts (most-specific first)
    ("[yes/no/quit]",              "yes\n"),
    ("(yes/no)",                   "yes\n"),
    ("[yes/no]",                   "yes\n"),
    ("[y/n/q]",                    "y\n"),
    ("[y/n]",                      "y\n"),
    ("(y/n)",                      "y\n"),
    ("y/n:",                       "y\n"),
    // Overwrite prompts
    ("overwrite?",                 "y\n"),
    ("already exists. overwrite",  "y\n"),
    // Pager / more
    ("--more--",                   "q"),
    ("(q to quit)",                "q"),
    ("press q to quit",            "q"),
    // "Press Enter to continue"
    ("press enter to continue",    "\n"),
    ("press return to continue",   "\n"),
    ("press enter",                "\n"),
    // Reboot / restart prompts (decline — agent shouldn't trigger reboots)
    ("restart now",                "n\n"),
    ("reboot now",                 "n\n"),
    ("do you want to restart",     "n\n"),
];

// ── tty_exec ──────────────────────────────────────────────────────────────────

pub fn tty_exec(args: &Value, store: &Arc<JobStore>, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let cmd = args["cmd"].as_str().ok_or("tty_exec: 'cmd' is required")?;

    let cwd = resolve_or_cwd(state, args["cwd"].as_str())?;

    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120);
    let default_yes  = args["default_yes"].as_bool().unwrap_or(true);
    let idle_done    = args["idle_done_secs"].as_u64().unwrap_or(3);

    // Build response table: default_yes baseline + caller overrides.
    let mut responses: Vec<(String, String)> = Vec::new();
    if default_yes {
        for (pat, ans) in DEFAULT_YES_RESPONSES {
            responses.push((pat.to_lowercase(), ans.to_string()));
        }
    }
    // Caller-supplied responses override defaults.
    if let Some(obj) = args["responses"].as_object() {
        for (pat, val) in obj {
            if let Some(ans) = val.as_str() {
                let key = pat.to_lowercase();
                // Replace existing entry if present.
                if let Some(pos) = responses.iter().position(|(k, _)| k == &key) {
                    responses[pos].1 = ans.to_owned();
                } else {
                    responses.push((key, ans.to_owned()));
                }
            }
        }
    }

    // Apply permission preprocessor for non-interactive env + command rewrite.
    let (cmd_rewritten, perm_env) = permissions::preprocess(cmd);

    // Spawn in a PTY so programs that open /dev/tty directly still work.
    let job = store.spawn_pty(&cmd_rewritten, cwd, Some(&format!("tty_exec: {}", &cmd[..cmd.len().min(40)])), perm_env)
        .map_err(|e| format!("tty_exec: spawn_pty: {e}"))?;

    // ── Pattern-response drive loop ────────────────────────────────────────────
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    // Track which response we've sent to avoid flooding on repeated output.
    // For each pattern, record the byte offset after which we last matched it
    // so we only re-fire if new output past that point matches again.
    let mut last_match_at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut last_output_len: usize = 0;
    let mut last_output_at  = Instant::now();

    // Byte offset we read up to so far (prevents re-scanning old output).
    let mut scan_cursor: usize = 0;

    let mut responses_sent: Vec<serde_json::Value> = Vec::new();
    let mut timed_out = false;

    loop {
        // ── Check job status ─────────────────────────────────────────────────
        let status = job.status.lock().unwrap().clone();
        if status.is_terminal() { break; }

        // ── Check deadline ────────────────────────────────────────────────────
        if Instant::now() >= deadline {
            // Kill if still running.
            unsafe { libc::kill(job.pid as i32, libc::SIGTERM); }
            timed_out = true;
            break;
        }

        // ── Read new output ───────────────────────────────────────────────────
        let full_output = {
            let buf = job.stdout_buf.lock().unwrap();
            buf.clone()
        };

        if full_output.len() != last_output_len {
            last_output_len = full_output.len();
            last_output_at  = Instant::now();
        } else if last_output_at.elapsed().as_secs() >= idle_done {
            // Program has been quiet for idle_done_secs — assume it's done
            // even if it hasn't sent EOF yet (common with some interactive progs).
            break;
        }

        // Only scan the new tail to avoid quadratic cost on large outputs.
        // Use last 512 bytes of total output for pattern matching (prompts
        // appear at the end of output, not in the middle of a long stream).
        let window_start = full_output.len().saturating_sub(512).max(scan_cursor);
        if window_start < full_output.len() {
            let window = String::from_utf8_lossy(&full_output[window_start..]).to_lowercase();

            for (pat, ans) in &responses {
                let last_at = *last_match_at.get(pat.as_str()).unwrap_or(&0);
                if window_start >= last_at {
                    // Check if pattern appears in this new window.
                    if window.contains(pat.as_str()) {
                        // Send the answer.
                        if let Some(ref tx) = job.stdin_tx {
                            let mut stdin = tx.lock().unwrap();
                            let _ = stdin.write_all(ans.as_bytes());
                            let _ = stdin.flush();
                        }
                        last_match_at.insert(pat.clone(), full_output.len());
                        responses_sent.push(json!({
                            "pattern": pat,
                            "answer":  ans.trim_end_matches('\n'),
                            "at_byte": window_start,
                        }));
                        // Short pause so the program can process the response.
                        std::thread::sleep(Duration::from_millis(80));
                        break; // Only fire one response per poll cycle.
                    }
                }
            }
            scan_cursor = window_start;
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    // Wait briefly for the process to actually exit after we break.
    let mut wait_ms = 0u64;
    let exit_code = loop {
        let s = job.status.lock().unwrap().clone();
        match s {
            JobStatus::Done(code) => break code,
            _ if wait_ms >= 3000  => break -1,
            _ => {
                std::thread::sleep(Duration::from_millis(50));
                wait_ms += 50;
            }
        }
    };

    // Collect full output, strip ANSI.
    let raw_out = job.stdout_buf.lock().unwrap().clone();
    let output  = strip_ansi(String::from_utf8_lossy(&raw_out).into_owned());

    Ok(ToolResult::json(&json!({
        "ok":             exit_code == 0 && !timed_out,
        "exit_code":      exit_code,
        "timed_out":      timed_out,
        "cmd":            cmd,
        "rewritten_cmd":  cmd_rewritten,
        "output":         output,
        "responses_sent": responses_sent,
        "job_id":         job.job_id,
        "note": if responses_sent.is_empty() {
            "No interactive prompts were detected."
        } else {
            "Interactive prompts were detected and automatically answered."
        }
    })))
}

// ── ANSI strip (duplicated locally to avoid cross-module dep) ─────────────────

fn strip_ansi(s: String) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() {
                match bytes[i] {
                    b'[' => {
                        i += 1;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) { i += 1; }
                        i += 1;
                    }
                    b']' => {
                        i += 1;
                        while i < bytes.len() && bytes[i] != 0x07 {
                            if bytes[i] == 0x1b && i+1 < bytes.len() && bytes[i+1] == b'\\' {
                                i += 2; break;
                            }
                            i += 1;
                        }
                        if i < bytes.len() { i += 1; }
                    }
                    _ => { i += 1; }
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
