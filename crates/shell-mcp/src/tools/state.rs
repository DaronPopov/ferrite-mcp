//! shell_state and set_cwd tool implementations.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde_json::{json, Value};

use crate::config::FerriteConfig;
use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::project::expand_tilde;

pub fn lock_mutex<'a, T>(mutex: &'a Mutex<T>, label: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("warning: recovering from poisoned mutex: {label}");
            poisoned.into_inner()
        }
    }
}

pub fn lock_state<'a>(state: &'a Arc<Mutex<ServerState>>) -> MutexGuard<'a, ServerState> {
    lock_mutex(state, "server state")
}

pub fn read_rwlock<'a, T>(lock: &'a RwLock<T>, label: &str) -> RwLockReadGuard<'a, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("warning: recovering from poisoned rwlock read: {label}");
            poisoned.into_inner()
        }
    }
}

pub fn write_rwlock<'a, T>(lock: &'a RwLock<T>, label: &str) -> RwLockWriteGuard<'a, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("warning: recovering from poisoned rwlock write: {label}");
            poisoned.into_inner()
        }
    }
}

pub fn read_cwd(state: &Arc<Mutex<ServerState>>) -> PathBuf {
    let guard = lock_state(state);
    let cwd = read_rwlock(&guard.cwd, "server cwd").clone();
    cwd
}

pub fn with_write_cwd<T>(state: &Arc<Mutex<ServerState>>, f: impl FnOnce(&mut PathBuf) -> T) -> T {
    let guard = lock_state(state);
    let mut cwd = write_rwlock(&guard.cwd, "server cwd");
    f(&mut cwd)
}

pub fn read_config(state: &Arc<Mutex<ServerState>>) -> FerriteConfig {
    let guard = lock_state(state);
    let config = read_rwlock(&guard.config, "server config").clone();
    config
}

pub fn with_write_config<T>(
    state: &Arc<Mutex<ServerState>>,
    f: impl FnOnce(&mut FerriteConfig) -> T,
) -> T {
    let guard = lock_state(state);
    let mut config = write_rwlock(&guard.config, "server config");
    f(&mut config)
}

pub fn with_notes<T>(state: &Arc<Mutex<ServerState>>, f: impl FnOnce(&mut Vec<String>) -> T) -> T {
    let guard = lock_state(state);
    let mut notes = lock_mutex(&guard.notes, "server notes");
    f(&mut notes)
}

pub fn read_notes(state: &Arc<Mutex<ServerState>>) -> Vec<String> {
    with_notes(state, |notes| notes.clone())
}

// ── shell_state ───────────────────────────────────────────────────────────────

pub fn shell_state(_args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let cwd = read_cwd(state).display().to_string();

    let path_entries: Vec<&str> = std::env::var("PATH")
        .unwrap_or_default()
        .leak() // acceptable: PATH is stable for the process lifetime
        .split(':')
        .collect();

    // Snapshot a curated set of env vars useful for development
    let key_vars = [
        "HOME",
        "USER",
        "SHELL",
        "CUDA_HOME",
        "CUDA_PATH",
        "CUDA_ROOT",
        "LIBTORCH",
        "TORCH_LIB",
        "LD_LIBRARY_PATH",
        "LIBRARY_PATH",
        "PKG_CONFIG_PATH",
        "CC",
        "CXX",
        "CMAKE_PREFIX_PATH",
        "VIRTUAL_ENV",
        "CONDA_PREFIX",
        "CARGO_HOME",
        "RUSTUP_HOME",
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

pub fn session_cwd(state: &Arc<Mutex<ServerState>>) -> Result<PathBuf, String> {
    Ok(read_cwd(state))
}

pub fn resolve_path(state: &Arc<Mutex<ServerState>>, raw: &str) -> Result<PathBuf, String> {
    let expanded = expand_tilde(raw);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(session_cwd(state)?.join(expanded))
    }
}

pub fn resolve_or_cwd(
    state: &Arc<Mutex<ServerState>>,
    raw: Option<&str>,
) -> Result<PathBuf, String> {
    match raw {
        Some(path) => resolve_path(state, path),
        None => session_cwd(state),
    }
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
            with_write_cwd(state, |cwd| *cwd = canonical);
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
