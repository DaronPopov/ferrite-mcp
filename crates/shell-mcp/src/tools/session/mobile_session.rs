//! mobile_session — trigger the Codex mobile session email flow.
//!
//! This is intended to be the backend action for `/mobile-session`.
//! It pings the codex-remote-auth sidecar, optionally starts it if down,
//! then requests a magic-link email for the configured/provided address.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::ToolResult;

pub fn mobile_session(args: &Value) -> Result<ToolResult, String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
    let default_cfg = PathBuf::from(&home).join(".config/ferrite/codex_remote_auth.toml");

    let cfg_path = args["config_path"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or(default_cfg);

    let api_base = args["api_base"]
        .as_str()
        .unwrap_or("http://127.0.0.1:8787")
        .trim_end_matches('/')
        .to_owned();

    let email = if let Some(e) = args["email"].as_str() {
        e.trim().to_lowercase()
    } else {
        read_allowed_email(&cfg_path)
            .unwrap_or_default()
            .to_lowercase()
    };

    if email.is_empty() {
        return Err(format!(
            "mobile_session: email not provided and couldn't read 'allowed_email' from {}",
            cfg_path.display()
        ));
    }

    let mut sidecar_started = false;
    let start_if_down = args["start_if_down"].as_bool().unwrap_or(true);

    if !health_ok(&api_base) && start_if_down {
        start_sidecar(args, &cfg_path)?;
        sidecar_started = true;
        thread::sleep(Duration::from_millis(900));
    }

    if !health_ok(&api_base) {
        return Err(format!(
            "mobile_session: auth sidecar unreachable at {api_base}. Start it with: codex-remote-auth --config {}",
            cfg_path.display()
        ));
    }

    let req_body = json!({ "email": email }).to_string();
    let request_url = format!("{api_base}/auth/request");

    let out = Command::new("curl")
        .args([
            "-fsS",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "--data",
            &req_body,
            &request_url,
        ])
        .output()
        .map_err(|e| format!("mobile_session: failed to invoke curl: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(format!("mobile_session: /auth/request failed: {stderr}"));
    }

    let response = String::from_utf8_lossy(&out.stdout).to_string();

    Ok(ToolResult::json(&json!({
        "status": "mobile_session_requested",
        "slash_command": "/mobile-session",
        "api_base": api_base,
        "config_path": cfg_path.display().to_string(),
        "email": email,
        "sidecar_started": sidecar_started,
        "auth_request_response": response,
        "note": "If the address is allowed, sign-in email(s) were sent. Open the link on phone and complete OTP verification.",
    })))
}

fn read_allowed_email(path: &PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("allowed_email")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

fn health_ok(api_base: &str) -> bool {
    let url = format!("{api_base}/health");
    let out = Command::new("curl")
        .args(["-fsS", "--max-time", "2", &url])
        .output();
    out.map(|o| o.status.success()).unwrap_or(false)
}

fn start_sidecar(args: &Value, cfg_path: &Path) -> Result<(), String> {
    let start_cmd = args["start_cmd"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "nohup codex-remote-auth --config '{}' > /tmp/codex-remote-auth.log 2>&1 &",
                cfg_path.display()
            )
        });

    let out = Command::new("/bin/sh")
        .args(["-c", &start_cmd])
        .output()
        .map_err(|e| format!("mobile_session: failed to start sidecar: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(format!("mobile_session: start command failed: {stderr}"));
    }

    Ok(())
}
