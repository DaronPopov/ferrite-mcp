//! Network / reachability tools.
//!
//! Tools:
//!   tailscale_status — Tailscale IP, peers, and connectivity summary.

use serde_json::{json, Value};
use std::time::Duration;

use crate::protocol::ToolResult;
use crate::tools::execution::run;

pub fn tailscale_status(_args: &Value) -> Result<ToolResult, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));

    // Check if tailscale binary exists
    let which = std::process::Command::new("which")
        .arg("tailscale")
        .output()
        .ok();
    let ts_bin = which
        .as_ref()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "tailscale".to_owned());

    // Prefer unprivileged status first; fallback to sudo -n when needed.
    let mut raw = run(
        &format!("{ts_bin} status --json"),
        &cwd,
        &[],
        "",
        Duration::from_secs(10),
    );

    // Parse JSON from stdout regardless of exit code — tailscale exits 1
    // on health warnings (e.g. DNS issues) even when fully running.
    let mut ts: Value =
        serde_json::from_str(raw["stdout"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
    if ts.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        let raw_sudo = run(
            &format!("sudo -n {ts_bin} status --json"),
            &cwd,
            &[],
            "",
            Duration::from_secs(10),
        );
        let ts_sudo: Value =
            serde_json::from_str(raw_sudo["stdout"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        if !ts_sudo.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            raw = raw_sudo;
            ts = ts_sudo;
        }
    }

    let backend_state = ts["BackendState"].as_str().unwrap_or("");
    let is_running = backend_state == "Running" || backend_state == "Starting";

    if !is_running {
        let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();
        let installed = !stderr.contains("not found") && !stderr.contains("No such file");

        if !installed {
            return Ok(ToolResult::json(&json!({
                "running":   false,
                "installed": false,
                "self_ip":   null,
                "self_name": null,
                "peers":     [],
                "note":      "tailscale not found — install from https://tailscale.com/download",
            })));
        }

        // Attempt to bring tailscale up (sudoers should allow this without password)
        let up = run(
            &format!("sudo -n {ts_bin} up"),
            &cwd,
            &[],
            "",
            Duration::from_secs(15),
        );
        let up_ok = up["exit_code"].as_i64().unwrap_or(1) == 0;

        if !up_ok {
            // Try without sudo (may work if already in tailscale group)
            let up2 = run(
                &format!("{ts_bin} up"),
                &cwd,
                &[],
                "",
                Duration::from_secs(15),
            );
            let up2_ok = up2["exit_code"].as_i64().unwrap_or(1) == 0;
            if !up2_ok {
                return Ok(ToolResult::json(&json!({
                    "running":    false,
                    "installed":  true,
                    "self_ip":    null,
                    "self_name":  null,
                    "peers":      [],
                    "note":       "tailscale installed but could not start — check sudoers or run: sudo tailscale up",
                })));
            }
        }

        // Re-query status after bringing up
        let raw = run(
            &format!("{ts_bin} status --json"),
            &cwd,
            &[],
            "",
            Duration::from_secs(10),
        );
        let mut ts: Value =
            serde_json::from_str(raw["stdout"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        if ts.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            let raw_sudo = run(
                &format!("sudo -n {ts_bin} status --json"),
                &cwd,
                &[],
                "",
                Duration::from_secs(10),
            );
            let ts_sudo: Value = serde_json::from_str(raw_sudo["stdout"].as_str().unwrap_or("{}"))
                .unwrap_or(json!({}));
            if !ts_sudo.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                ts = ts_sudo;
            }
        }
        let backend_state = ts["BackendState"].as_str().unwrap_or("");
        let is_running = backend_state == "Running" || backend_state == "Starting";
        if !is_running {
            return Ok(ToolResult::json(&json!({
                "running":   false,
                "installed": true,
                "self_ip":   null,
                "self_name": null,
                "peers":     [],
                "note":      "tailscale up succeeded but daemon not yet Running",
            })));
        }

        // Re-parse full status below by falling through — rebuild ts for the running path
        let self_node = &ts["Self"];
        let self_ip = self_node["TailscaleIPs"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let self_name = self_node["HostName"].as_str().unwrap_or("").to_owned();
        let self_online = self_node["Online"].as_bool().unwrap_or(false);
        return Ok(ToolResult::json(&json!({
            "running":      true,
            "self_ip":      self_ip,
            "self_name":    self_name,
            "self_online":  self_online,
            "peer_count":   0,
            "online_peers": 0,
            "peers":        [],
            "ssh_cmd":      format!("ssh {}@{}", self_name, self_ip),
            "note":         "tailscale was down — auto-started successfully",
        })));
    }

    // Self node
    let self_node = &ts["Self"];
    let self_ip = self_node["TailscaleIPs"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let self_name = self_node["HostName"].as_str().unwrap_or("").to_owned();
    let self_online = self_node["Online"].as_bool().unwrap_or(false);

    // Peer list
    let peers: Vec<Value> = ts["Peer"]
        .as_object()
        .map(|m| {
            m.values()
                .map(|p| {
                    let ip = p["TailscaleIPs"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    json!({
                        "name":    p["HostName"].as_str().unwrap_or(""),
                        "ip":      ip,
                        "online":  p["Online"].as_bool().unwrap_or(false),
                        "os":      p["OS"].as_str().unwrap_or(""),
                        "last_seen": p["LastSeen"].as_str().unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let online_peers = peers
        .iter()
        .filter(|p| p["online"].as_bool().unwrap_or(false))
        .count();

    Ok(ToolResult::json(&json!({
        "running":       true,
        "self_ip":       self_ip,
        "self_name":     self_name,
        "self_online":   self_online,
        "peer_count":    peers.len(),
        "online_peers":  online_peers,
        "peers":         peers,
        "ssh_cmd":       format!("ssh {}@{}", self_name, self_ip),
    })))
}
