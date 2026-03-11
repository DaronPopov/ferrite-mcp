//! `env_doctor` — environment pre-flight check.
//!
//! Before kicking off a complex workflow, the agent calls env_doctor to
//! get a structured report on everything that might block it later:
//!
//!   - Are required binaries in PATH?
//!   - Is there enough disk space?
//!   - Can we reach key package registries? (pypi, crates.io, npmjs, github)
//!   - What tool versions are available?
//!   - Are key directories writable?
//!   - Is the sudoers entry active?

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use serde_json::{json, Value};

use crate::permissions;
use crate::protocol::ToolResult;

// ── Default binary list to check ──────────────────────────────────────────────

const DEFAULT_BINS: &[&str] = &[
    "python3", "pip3", "node", "npm", "cargo", "rustup",
    "git", "curl", "wget", "tar", "unzip", "make",
    "gcc", "g++", "cmake",
    "apt", "apt-get", "dpkg",
    "systemctl", "sudo",
    "docker", "snap",
];

// ── Endpoints to probe for network reachability ───────────────────────────────

const NETWORK_PROBES: &[(&str, &str)] = &[
    ("github.com",   "https://github.com"),
    ("pypi.org",     "https://pypi.org/simple/"),
    ("crates.io",    "https://crates.io/"),
    ("npmjs.com",    "https://registry.npmjs.org/"),
    ("ubuntu (apt)", "http://archive.ubuntu.com/ubuntu/"),
];

// ── Minimum free disk space (bytes) to flag as warning ───────────────────────

const MIN_FREE_BYTES: u64 = 512 * 1024 * 1024; // 512 MB

// ── env_doctor ────────────────────────────────────────────────────────────────

pub fn env_doctor(args: &Value) -> Result<ToolResult, String> {
    // Which binaries to check (default list + caller extras).
    let mut bins: Vec<String> = DEFAULT_BINS.iter().map(|s| s.to_string()).collect();
    if let Some(extra) = args["check_bins"].as_array() {
        for v in extra {
            if let Some(s) = v.as_str() {
                if !bins.contains(&s.to_string()) {
                    bins.push(s.to_string());
                }
            }
        }
    }

    let check_network = args["check_network"].as_bool().unwrap_or(true);
    let check_disk    = args["check_disk"].as_bool().unwrap_or(true);
    let check_versions = args["check_versions"].as_bool().unwrap_or(true);

    // ── Binary availability ────────────────────────────────────────────────────
    let mut found:   Vec<Value> = Vec::new();
    let mut missing: Vec<Value> = Vec::new();

    for bin in &bins {
        match which_path(bin) {
            Some(path) => {
                let ver = if check_versions { tool_version(bin) } else { None };
                found.push(json!({
                    "bin":     bin,
                    "path":    path,
                    "version": ver,
                }));
            }
            None => {
                missing.push(json!({
                    "bin":     bin,
                    "install": suggest_install(bin),
                }));
            }
        }
    }

    // ── Disk space ────────────────────────────────────────────────────────────
    let disk = if check_disk {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_owned());
        let root_free = free_bytes("/");
        let home_free = free_bytes(&home);
        json!({
            "root_free_bytes": root_free,
            "root_free_mb":    root_free.map(|b| b / 1024 / 1024),
            "home_free_bytes": home_free,
            "home_free_mb":    home_free.map(|b| b / 1024 / 1024),
            "root_ok":         root_free.map(|b| b >= MIN_FREE_BYTES).unwrap_or(false),
            "home_ok":         home_free.map(|b| b >= MIN_FREE_BYTES).unwrap_or(false),
            "min_recommended_mb": MIN_FREE_BYTES / 1024 / 1024,
        })
    } else {
        Value::Null
    };

    // ── Network reachability ───────────────────────────────────────────────────
    let network = if check_network {
        let results: Vec<Value> = NETWORK_PROBES.iter().map(|(name, url)| {
            let (ok, latency_ms) = probe_url(url);
            json!({
                "host":       name,
                "url":        url,
                "reachable":  ok,
                "latency_ms": latency_ms,
            })
        }).collect();
        let all_ok = results.iter().all(|r| r["reachable"].as_bool().unwrap_or(false));
        json!({ "probes": results, "all_reachable": all_ok })
    } else {
        Value::Null
    };

    // ── Writable directories ───────────────────────────────────────────────────
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    let write_checks: Vec<Value> = [
        home.as_str(),
        "/tmp",
        "/usr/local/bin",
        "/etc/sudoers.d",
    ].iter().map(|dir| {
        let writable = is_writable(dir);
        json!({ "path": dir, "writable": writable })
    }).collect();

    // ── Sudoers status ────────────────────────────────────────────────────────
    let sudo_status = match permissions::sudoers_status() {
        permissions::SudoersStatus::Active    => "active",
        permissions::SudoersStatus::StaleUser => "stale_user",
        permissions::SudoersStatus::Missing   => "missing",
    };

    // ── Overall health ────────────────────────────────────────────────────────
    let critical_missing: Vec<&str> = ["git", "curl", "python3", "pip3"]
        .iter()
        .filter(|&&b| missing.iter().any(|m| m["bin"].as_str() == Some(b)))
        .copied()
        .collect();

    let disk_ok = !check_disk || {
        disk["root_ok"].as_bool().unwrap_or(true)
            && disk["home_ok"].as_bool().unwrap_or(true)
    };

    let net_ok = !check_network
        || network["all_reachable"].as_bool().unwrap_or(true);

    let healthy = critical_missing.is_empty() && disk_ok && net_ok;

    let mut warnings: Vec<String> = Vec::new();
    if !critical_missing.is_empty() {
        warnings.push(format!("Critical tools missing: {}", critical_missing.join(", ")));
    }
    if check_disk && !disk["root_ok"].as_bool().unwrap_or(true) {
        warnings.push("Low disk space on /".to_owned());
    }
    if check_disk && !disk["home_ok"].as_bool().unwrap_or(true) {
        warnings.push(format!("Low disk space on {home}"));
    }
    if check_network && !net_ok {
        warnings.push("Some package registries unreachable".to_owned());
    }
    if sudo_status != "active" {
        warnings.push(format!("Sudoers not active (status: {sudo_status}) — privileged commands may prompt for password"));
    }

    Ok(ToolResult::json(&json!({
        "healthy":        healthy,
        "warnings":       warnings,
        "bins": {
            "found":   found,
            "missing": missing,
            "missing_count": missing.len(),
        },
        "disk":    disk,
        "network": network,
        "write_access": write_checks,
        "sudo_status":  sudo_status,
        "sudoers_path": permissions::sudoers_path(),
    })))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn which_path(bin: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(bin))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}

fn tool_version(bin: &str) -> Option<String> {
    // Most tools support --version; a few use -V or version.
    let flag = match bin {
        "cargo" | "rustup" | "rustc" => "-V",
        _                             => "--version",
    };

    Command::new(bin)
        .arg(flag)
        .output()
        .ok()
        .and_then(|o| {
            let out = String::from_utf8_lossy(&o.stdout).into_owned()
                + &String::from_utf8_lossy(&o.stderr);
            // First non-empty line, truncated to 80 chars.
            out.lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().chars().take(80).collect())
        })
}

fn suggest_install(bin: &str) -> &'static str {
    match bin {
        "python3" | "pip3" => "sudo apt install python3 python3-pip",
        "node" | "npm"     => "curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash - && sudo apt install nodejs",
        "cargo" | "rustup" | "rustc" => "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh",
        "git"              => "sudo apt install git",
        "curl"             => "sudo apt install curl",
        "wget"             => "sudo apt install wget",
        "make"             => "sudo apt install make",
        "gcc" | "g++"      => "sudo apt install build-essential",
        "cmake"            => "sudo apt install cmake",
        "docker"           => "sudo apt install docker.io",
        "snap"             => "sudo apt install snapd",
        _                  => "check your package manager",
    }
}

fn free_bytes(path: &str) -> Option<u64> {
    let output = Command::new("df")
        .args(["--block-size=1", "--output=avail", path])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // df output: header line + data line.
    text.lines()
        .nth(1)
        .and_then(|l| l.trim().parse::<u64>().ok())
}

fn probe_url(url: &str) -> (bool, Option<u64>) {
    let start = Instant::now();
    let output = Command::new("curl")
        .args([
            "--silent",
            "--head",
            "--max-time", "5",
            "--write-out", "%{http_code}",
            "--output", "/dev/null",
            url,
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let code_str = String::from_utf8_lossy(&o.stdout);
            let code: u16 = code_str.trim().parse().unwrap_or(0);
            let ok = code > 0 && code < 500;
            (ok, Some(start.elapsed().as_millis() as u64))
        }
        _ => (false, None),
    }
}

fn is_writable(path: &str) -> bool {
    let p = Path::new(path);
    if !p.exists() { return false; }
    // Try creating a temp file in the directory.
    let test = p.join(".ferrite_write_test");
    match std::fs::write(&test, b"") {
        Ok(_) => { let _ = std::fs::remove_file(&test); true }
        Err(_) => false,
    }
}
