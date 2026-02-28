//! session_status — report the state of the auto-started ferrite/claude session.
//!
//! Reads:
//!   - tmux session alive?
//!   - Saved remote-control URL (~/.local/share/ferrite/remote-session-url.txt)
//!   - autostart log tail (~/.local/share/ferrite/autostart.log)
//!   - systemd unit status

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::tools::execution::run;

pub fn session_status(_args: &Value) -> Result<ToolResult, String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
    let home = PathBuf::from(&home);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    // ── tmux session ──────────────────────────────────────────────────────────
    let session_name = std::env::var("FERRITE_SESSION").unwrap_or_else(|_| "claude-remote".to_owned());
    let tmux_raw = run(
        &format!("tmux has-session -t '{}' 2>/dev/null && echo alive || echo dead", session_name),
        &cwd, &[], "", Duration::from_secs(5),
    );
    let tmux_alive = tmux_raw["stdout"].as_str().unwrap_or("").trim() == "alive";

    // ── remote URL ────────────────────────────────────────────────────────────
    let url_file = home.join(".local/share/ferrite/remote-session-url.txt");
    let (remote_url, url_age_secs) = if url_file.exists() {
        let url = std::fs::read_to_string(&url_file)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let age = url_file.metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (url, Some(age))
    } else {
        (String::new(), None)
    };

    // ── autostart log tail (last 10 lines) ────────────────────────────────────
    let log_file = home.join(".local/share/ferrite/autostart.log");
    let log_tail = if log_file.exists() {
        let raw = run(
            &format!("tail -10 '{}'", log_file.display()),
            &cwd, &[], "", Duration::from_secs(3),
        );
        raw["stdout"].as_str().unwrap_or("").trim().to_owned()
    } else {
        String::new()
    };

    // ── systemd unit status ───────────────────────────────────────────────────
    let systemd_raw = run(
        "systemctl --user is-active ferrite-session.service 2>/dev/null || echo inactive",
        &cwd, &[], "", Duration::from_secs(5),
    );
    let systemd_state = systemd_raw["stdout"].as_str().unwrap_or("unknown").trim().to_owned();

    // ── ntfy config ───────────────────────────────────────────────────────────
    let ntfy_configured = std::env::var("NTFY_TOPIC")
        .map(|t| !t.is_empty())
        .unwrap_or(false)
        || {
            let env_file = home.join(".config/ferrite/env");
            std::fs::read_to_string(&env_file)
                .unwrap_or_default()
                .lines()
                .any(|l| l.starts_with("NTFY_TOPIC=") && l.len() > "NTFY_TOPIC=".len())
        };

    let status = if tmux_alive && !remote_url.is_empty() {
        "ready"
    } else if tmux_alive {
        "running_no_url"
    } else {
        "offline"
    };

    Ok(ToolResult::json(&json!({
        "status":           status,
        "tmux_session":     session_name,
        "tmux_alive":       tmux_alive,
        "remote_url":       if remote_url.is_empty() { json!(null) } else { json!(remote_url) },
        "url_age_secs":     url_age_secs,
        "systemd_state":    systemd_state,
        "ntfy_configured":  ntfy_configured,
        "autostart_log":    log_tail,
        "config_file":      home.join(".config/ferrite/env").display().to_string(),
        "note": match status {
            "ready"           => "Session live — remote_url is the link to open on your phone",
            "running_no_url"  => "Session running but URL not captured — check autostart_log",
            _                 => "Session offline — run ferrite-autostart or enable systemd service",
        },
    })))
}

/// session_restart — kill the current tmux Claude session so the watchdog
/// respawns a fresh one (new session ID, new remote-control URL, new email).
///
/// Because this tool kills the tmux pane Claude is running inside, the MCP
/// response may not be delivered — that is expected. The watchdog detects the
/// dead session within 30 s and calls run_boot_sequence() automatically.
pub fn session_restart(_args: &Value) -> Result<ToolResult, String> {
    let session_name = std::env::var("FERRITE_SESSION")
        .unwrap_or_else(|_| "claude-remote".to_owned());
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));

    // Spawn the kill in a detached subprocess so the response can be flushed
    // before the pane disappears.
    let result = run(
        &format!(
            "nohup bash -c 'sleep 1; tmux kill-session -t {:?}' >/dev/null 2>&1 &",
            session_name
        ),
        &cwd, &[], "", Duration::from_secs(3),
    );

    let launched = result["exit_code"].as_i64().unwrap_or(1) == 0;

    Ok(ToolResult::json(&json!({
        "status":       if launched { "killing" } else { "error" },
        "session":      session_name,
        "note":         "tmux session will be killed in ~1 s. The ferrite watchdog will spawn a fresh Claude session and send a new email within ~30 s.",
        "error":        if launched { json!(null) } else { result["stderr"].clone() },
    })))
}
