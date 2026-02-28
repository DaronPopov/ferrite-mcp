//! Session persistence for ferrite background jobs.
//!
//! Saves job metadata to ~/.local/share/ferrite/session.json so job handles
//! survive ferrite restarts (Claude context resets, server crashes, etc.).
//!
//! Layout:
//!   ~/.local/share/ferrite/
//!     session.json      — job snapshots + counter
//!     logs/             — persistent combined stdout+stderr logs per job

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Paths ─────────────────────────────────────────────────────────────────────

pub fn data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("ferrite")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn log_path_for(job_id: &str) -> PathBuf {
    logs_dir().join(format!("{job_id}.log"))
}

fn session_path() -> PathBuf {
    data_dir().join("session.json")
}

// ── Snapshot types ────────────────────────────────────────────────────────────

/// Serializable snapshot of a Job's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub job_id:       String,
    pub pid:          u32,
    pub label:        String,
    pub cmd:          String,
    pub cwd:          String,
    pub started_secs: u64,
    /// "running" | "done" | "killed" | "attached"
    pub status:       String,
    pub exit_code:    Option<i32>,
    pub log_path:     String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionData {
    pub version: u32,
    pub counter: usize,
    pub jobs:    Vec<JobSnapshot>,
}

impl Default for SessionData {
    fn default() -> Self {
        Self { version: 1, counter: 1, jobs: Vec::new() }
    }
}

// ── Persistence handle ────────────────────────────────────────────────────────

pub struct Persistence {
    session_path: PathBuf,
}

impl Persistence {
    /// Create a new Persistence handle, ensuring storage directories exist.
    pub fn new() -> Self {
        let _ = fs::create_dir_all(data_dir());
        let _ = fs::create_dir_all(logs_dir());
        Self { session_path: session_path() }
    }

    /// Atomically write session data (write tmp → rename).
    pub fn save(&self, data: &SessionData) {
        let tmp = self.session_path.with_extension("tmp");
        if let Ok(json) = serde_json::to_string_pretty(data) {
            if fs::write(&tmp, &json).is_ok() {
                let _ = fs::rename(&tmp, &self.session_path);
            }
        }
    }

    /// Load session data. Returns default (empty) if file is missing or corrupt.
    pub fn load(&self) -> SessionData {
        fs::read_to_string(&self.session_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}
