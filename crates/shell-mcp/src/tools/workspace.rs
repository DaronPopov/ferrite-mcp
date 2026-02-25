//! Workspace-level orientation and session notes.
//!
//! Tools:
//!   orient — single-call situational awareness: cwd, git state, recent changes, dir tree, ports
//!   note   — session scratchpad stored in ServerState (read/append/clear)

use std::sync::{Arc, Mutex};
use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use super::{filesystem, git, system};

// ── orient ────────────────────────────────────────────────────────────────────

/// Single-call situational awareness.
///
/// Composes: cwd, git status, recently changed files, shallow dir tree, listening ports.
/// Replaces the pattern of calling git_status + changed_since + list_dir + port_list separately.
pub fn orient(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let cwd = {
        let s = state.lock().unwrap();
        s.cwd.to_string_lossy().into_owned()
    };

    let path = args["path"].as_str().unwrap_or(&cwd).to_owned();
    let recent_window = args["since"].as_str().unwrap_or("2h");
    let tree_depth   = args["depth"].as_u64().unwrap_or(2) as u64;

    // ── git status ────────────────────────────────────────────────────────────
    let git_val = match git::git_status(&json!({ "path": path })) {
        Ok(tr) => parse_tool_result_json(&tr),
        Err(_) => json!(null),
    };

    // ── recently changed files ────────────────────────────────────────────────
    let recent_val = match filesystem::changed_since(&json!({
        "path": path,
        "since_relative": recent_window,
        "max_results": 30
    })) {
        Ok(tr) => parse_tool_result_json(&tr),
        Err(_) => json!([]),
    };

    // ── shallow directory tree ────────────────────────────────────────────────
    let tree_val = match filesystem::list_dir(&json!({
        "path": path,
        "depth": tree_depth
    })) {
        Ok(tr) => parse_tool_result_json(&tr),
        Err(_) => json!(null),
    };

    // ── listening ports ───────────────────────────────────────────────────────
    let ports_val = match system::port_list(&json!({})) {
        Ok(tr) => parse_tool_result_json(&tr),
        Err(_) => json!([]),
    };

    Ok(ToolResult::json(&json!({
        "cwd":    cwd,
        "git":    git_val,
        "recent": recent_val,
        "tree":   tree_val,
        "ports":  ports_val,
    })))
}

// ── note ──────────────────────────────────────────────────────────────────────

/// Session scratchpad.
///
/// op=read   — return all notes
/// op=append — add a new note line
/// op=clear  — delete all notes
pub fn note(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let op = args["op"].as_str().unwrap_or("read");

    match op {
        "read" => {
            let s = state.lock().unwrap();
            Ok(ToolResult::json(&json!({
                "count": s.notes.len(),
                "notes": s.notes,
            })))
        }
        "append" => {
            let content = args["content"].as_str()
                .ok_or("note append: 'content' required")?
                .to_owned();
            let mut s = state.lock().unwrap();
            s.notes.push(content.clone());
            Ok(ToolResult::json(&json!({
                "ok":    true,
                "count": s.notes.len(),
                "added": content,
            })))
        }
        "clear" => {
            let mut s = state.lock().unwrap();
            let prev = s.notes.len();
            s.notes.clear();
            Ok(ToolResult::json(&json!({
                "ok":     true,
                "cleared": prev,
            })))
        }
        other => Err(format!("note: unknown op '{}' — use read|append|clear", other)),
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Extract JSON from a ToolResult's first content item, or return raw text as string value.
fn parse_tool_result_json(tr: &ToolResult) -> Value {
    tr.content.first()
        .and_then(|c| serde_json::from_str(&c.text).ok())
        .unwrap_or_else(|| {
            tr.content.first()
                .map(|c| Value::String(c.text.clone()))
                .unwrap_or(Value::Null)
        })
}
