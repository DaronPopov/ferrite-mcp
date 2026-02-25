//! shell_state and set_cwd tool implementations.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;

// ── shell_state ───────────────────────────────────────────────────────────────

pub fn shell_state(_args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let state = state.lock().map_err(|e| format!("state lock poisoned: {e}"))?;

    let cwd = state.cwd.display().to_string();

    let path_entries: Vec<&str> = std::env::var("PATH")
        .unwrap_or_default()
        .leak() // acceptable: PATH is stable for the process lifetime
        .split(':')
        .collect();

    // Snapshot a curated set of env vars useful for development
    let key_vars = [
        "HOME", "USER", "SHELL",
        "CUDA_HOME", "CUDA_PATH", "CUDA_ROOT",
        "LIBTORCH", "TORCH_LIB",
        "LD_LIBRARY_PATH", "LIBRARY_PATH", "PKG_CONFIG_PATH",
        "CC", "CXX", "CMAKE_PREFIX_PATH",
        "VIRTUAL_ENV", "CONDA_PREFIX",
        "CARGO_HOME", "RUSTUP_HOME",
        "ORT_DYLIB_PATH",
    ];

    let mut env_snapshot = serde_json::Map::new();
    for key in key_vars {
        if let Ok(val) = std::env::var(key) {
            env_snapshot.insert(key.to_owned(), Value::String(val));
        }
    }

    let hostname = hostname();

    Ok(ToolResult::json(&json!({
        "cwd": cwd,
        "hostname": hostname,
        "path_entries": path_entries,
        "env": env_snapshot,
    })))
}

// ── set_cwd ───────────────────────────────────────────────────────────────────

pub fn set_cwd(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let raw = args["path"].as_str().ok_or("set_cwd: 'path' is required")?;

    // Tilde expansion
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_owned());
        format!("{home}/{rest}")
    } else if raw == "~" {
        std::env::var("HOME").unwrap_or_else(|_| "/".to_owned())
    } else {
        raw.to_owned()
    };

    let path = std::path::Path::new(&expanded);

    match path.canonicalize() {
        Err(e) => Ok(ToolResult::json(&json!({
            "ok": false,
            "error": format!("{e}"),
            "path": raw,
        }))),
        Ok(canonical) => {
            let cwd_str = canonical.display().to_string();
            let mut state = state.lock().map_err(|e| format!("state lock poisoned: {e}"))?;
            state.cwd = canonical;
            Ok(ToolResult::json(&json!({
                "ok": true,
                "cwd": cwd_str,
            })))
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_owned())
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}
