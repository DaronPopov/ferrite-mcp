//! Network / reachability tools.
//!
//! Tools:
//!   tailscale_status — Tailscale IP, peers, and connectivity summary.

use std::time::Duration;
use serde_json::{json, Value};

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

    // Try `tailscale status --json`
    let raw = run(
        &format!("{ts_bin} status --json"),
        &cwd, &[], "", Duration::from_secs(10),
    );

    if !raw["success"].as_bool().unwrap_or(false) {
        // Tailscale not running or not installed
        let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();
        let installed = !stderr.contains("not found") && !stderr.contains("No such file");
        return Ok(ToolResult::json(&json!({
            "running":    false,
            "installed":  installed,
            "self_ip":    null,
            "self_name":  null,
            "peers":      [],
            "note": if installed {
                "tailscale is installed but not running — try: sudo tailscale up"
            } else {
                "tailscale not found — install from https://tailscale.com/download"
            },
        })));
    }

    let ts: Value = serde_json::from_str(raw["stdout"].as_str().unwrap_or("{}"))
        .unwrap_or(json!({}));

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
            m.values().map(|p| {
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
            }).collect()
        })
        .unwrap_or_default();

    let online_peers = peers.iter().filter(|p| p["online"].as_bool().unwrap_or(false)).count();

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
