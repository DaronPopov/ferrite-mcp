//! System introspection: running processes, listening ports, journal logs.
//!
//! Tools:
//!   process_tree  — list running processes from /proc with hierarchy
//!   port_list     — listening TCP/UDP ports via `ss`
//!   journal_query — systemd journal entries via journalctl

use std::collections::HashMap;
use serde_json::{json, Value};
use crate::protocol::ToolResult;

// ── process_tree ──────────────────────────────────────────────────────────────

/// List running processes. Reads /proc directly — no external binary needed.
/// Optional filter by process name substring.
pub fn process_tree(args: &Value) -> Result<ToolResult, String> {
    let filter = args["filter"].as_str().unwrap_or("").to_lowercase();
    let limit  = args["limit"].as_u64().unwrap_or(200) as usize;

    let mut procs: Vec<Value> = Vec::new();

    let proc_dir = std::fs::read_dir("/proc")
        .map_err(|e| format!("process_tree: /proc: {e}"))?;

    for entry in proc_dir.filter_map(|e| e.ok()) {
        if procs.len() >= limit { break; }
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();

        // Only numeric entries are PIDs
        let pid: u32 = match pid_str.parse() {
            Ok(n)  => n,
            Err(_) => continue,
        };

        let proc_path = entry.path();

        // Read status file: Name, State, PPid, VmRSS
        let status = read_proc_status(&proc_path);
        let proc_name = status.get("Name").cloned().unwrap_or_default();

        // Apply filter
        if !filter.is_empty() && !proc_name.to_lowercase().contains(&filter) {
            continue;
        }

        let ppid:   u32  = status.get("PPid").and_then(|v| v.parse().ok()).unwrap_or(0);
        let state:  &str = status.get("State").map(String::as_str).unwrap_or("?");
        let vm_rss: u64  = status.get("VmRSS")
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        // Read cmdline (null-byte separated)
        let cmdline = std::fs::read(proc_path.join("cmdline"))
            .map(|bytes| {
                bytes.split(|&b| b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();

        procs.push(json!({
            "pid":       pid,
            "ppid":      ppid,
            "name":      proc_name,
            "state":     state.chars().next().map(|c| c.to_string()).unwrap_or_default(),
            "mem_kb":    vm_rss,
            "cmdline":   cmdline,
        }));
    }

    // Sort by PID for stable output
    procs.sort_by_key(|p| p["pid"].as_u64().unwrap_or(0));

    Ok(ToolResult::json(&json!({
        "count":     procs.len(),
        "processes": procs,
    })))
}

fn read_proc_status(proc_path: &std::path::Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string(proc_path.join("status")) else {
        return map;
    };
    for line in content.lines() {
        if let Some((key, val)) = line.split_once(':') {
            map.insert(key.trim().to_owned(), val.trim().to_owned());
        }
    }
    map
}

// ── port_list ─────────────────────────────────────────────────────────────────

/// List listening TCP/UDP ports using `ss -tlnup`.
/// Returns [{proto, addr, port, pid, process}].
pub fn port_list(args: &Value) -> Result<ToolResult, String> {
    let proto_filter = args["proto"].as_str().unwrap_or(""); // "tcp", "udp", or ""

    let out = std::process::Command::new("ss")
        .args(["-tlnup"])
        .output()
        .map_err(|e| format!("port_list: ss: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Ok(ToolResult::error(format!("port_list: ss failed: {err}")));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut ports: Vec<Value> = Vec::new();

    for line in stdout.lines().skip(1) { // skip header
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 { continue; }

        let netid = cols[0]; // "tcp", "udp", "tcp6", "udp6"
        let proto = if netid.starts_with("tcp") { "tcp" } else if netid.starts_with("udp") { "udp" } else { continue };

        if !proto_filter.is_empty() && proto != proto_filter { continue; }
        if cols[1] != "LISTEN" && proto == "tcp" { continue; }

        // Local address is col 4: "0.0.0.0:8080" or "[::]:22" or "*:68"
        let local = cols[4];
        let (addr, port) = split_addr_port(local);

        // Process info is the last column: users:(("name",pid=N,fd=M))
        let (proc_name, pid) = parse_ss_process(cols.last().unwrap_or(&""));

        ports.push(json!({
            "proto":   proto,
            "addr":    addr,
            "port":    port,
            "pid":     pid,
            "process": proc_name,
        }));
    }

    // Sort by port
    ports.sort_by_key(|p| p["port"].as_u64().unwrap_or(0));

    Ok(ToolResult::json(&json!({
        "count": ports.len(),
        "ports": ports,
    })))
}

fn split_addr_port(s: &str) -> (String, u64) {
    // IPv6: "[::1]:80" or "*:80"
    if s.starts_with('[') {
        if let Some(bracket_end) = s.find(']') {
            let addr = s[1..bracket_end].to_owned();
            let port = s[bracket_end+2..].parse().unwrap_or(0);
            return (addr, port);
        }
    }
    // IPv4: "0.0.0.0:80" or "*:80"
    if let Some(colon) = s.rfind(':') {
        let addr = s[..colon].to_owned();
        let port = s[colon+1..].parse().unwrap_or(0);
        return (addr, port);
    }
    (s.to_owned(), 0)
}

fn parse_ss_process(s: &str) -> (String, Option<u64>) {
    // Format: users:(("name",pid=N,fd=M))
    if !s.starts_with("users:") { return (String::new(), None); }
    let name = s.find('"')
        .and_then(|start| s[start+1..].find('"').map(|end| s[start+1..start+1+end].to_owned()))
        .unwrap_or_default();
    let pid = s.find("pid=")
        .and_then(|i| s[i+4..].split(',').next())
        .and_then(|n| n.parse().ok());
    (name, pid)
}

// ── journal_query ─────────────────────────────────────────────────────────────

/// Query systemd journal via journalctl.
/// Filter by unit, time range, grep pattern. Returns [{time, unit, pid, message}].
pub fn journal_query(args: &Value) -> Result<ToolResult, String> {
    let unit    = args["unit"].as_str().unwrap_or("");
    let since   = args["since"].as_str().unwrap_or("1h ago");
    let grep    = args["grep"].as_str().unwrap_or("");
    let limit   = args["limit"].as_u64().unwrap_or(50);
    let boot    = args["boot"].as_bool().unwrap_or(false);

    let mut cmd = std::process::Command::new("journalctl");
    cmd.arg("--no-pager")
       .arg("-n").arg(limit.to_string())
       .arg("--output=short-iso");

    if !unit.is_empty()  { cmd.args(["-u", unit]); }
    if !since.is_empty() { cmd.args(["--since", since]); }
    if !grep.is_empty()  { cmd.args(["--grep", grep]); }
    if boot              { cmd.arg("-b"); }

    let out = cmd.output().map_err(|e| format!("journal_query: journalctl: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Ok(ToolResult::error(format!("journal_query: {err}")));
    }

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let entries = parse_journal_output(&stdout);

    Ok(ToolResult::json(&json!({
        "count":   entries.len(),
        "since":   since,
        "entries": entries,
    })))
}

/// Parse short-iso journal format:
/// "2024-01-01T10:00:00+0000 hostname unit[pid]: message"
fn parse_journal_output(output: &str) -> Vec<Value> {
    output.lines()
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .map(|line| {
            // Split on first space: timestamp
            let (ts, rest) = line.split_once(' ').unwrap_or(("", line));
            // Split rest on first space: hostname
            let (_, rest2) = rest.split_once(' ').unwrap_or(("", rest));
            // rest2: "unit[pid]: message" or "unit: message"
            let (unit_pid, message) = rest2.split_once(": ").unwrap_or(("", rest2));

            let (unit, pid) = if let Some(b) = unit_pid.find('[') {
                let u = unit_pid[..b].to_owned();
                let p = unit_pid[b+1..].trim_end_matches(']').to_owned();
                (u, p)
            } else {
                (unit_pid.to_owned(), String::new())
            };

            json!({
                "time":    ts,
                "unit":    unit,
                "pid":     pid,
                "message": message,
            })
        })
        .collect()
}
