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

    // ── git status (cap untracked to avoid blowup on dirty repos) ─────────────
    let git_val = match git::git_status(&json!({ "path": path })) {
        Ok(tr) => {
            let mut v = parse_tool_result_json(&tr);
            cap_array_field(&mut v, "untracked", 20);
            v
        }
        Err(_) => json!(null),
    };

    // ── recently changed files (paths only, strip size metadata) ─────────────
    let recent_val = match filesystem::changed_since(&json!({
        "path": path,
        "since_relative": recent_window,
        "max_results": 15
    })) {
        Ok(tr) => slim_changed(parse_tool_result_json(&tr)),
        Err(_) => json!([]),
    };

    // ── shallow directory tree (name+type only, hard entry cap) ──────────────
    let tree_val = match filesystem::list_dir(&json!({
        "path": path,
        "depth": tree_depth,
        "max_entries": 60
    })) {
        Ok(tr) => {
            let v = parse_tool_result_json(&tr);
            json!({
                "path":      v["path"],
                "truncated": v["truncated"],
                "entries":   slim_tree(&v["entries"]),
            })
        }
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

/// Truncate an array field inside a JSON object to `max` entries.
fn cap_array_field(v: &mut Value, key: &str, max: usize) {
    if let Some(arr) = v.get_mut(key).and_then(|a| a.as_array_mut()) {
        arr.truncate(max);
    }
}

/// Strip size_bytes from changed_since output — only paths and timestamps needed for orientation.
fn slim_changed(v: Value) -> Value {
    let changed = v["changed"].as_array().map(|arr| {
        arr.iter().map(|f| json!({
            "path":          f["path"],
            "modified_secs": f["modified_secs"],
        })).collect::<Vec<_>>()
    }).unwrap_or_default();

    json!({
        "since_secs": v["since_secs"],
        "count":      changed.len(),
        "truncated":  v["truncated"],
        "changed":    changed,
    })
}

/// Recursively strip path/size_bytes/modified_secs from tree entries.
/// Keeps only name, type, and children — sufficient for orientation.
fn slim_tree(entries: &Value) -> Value {
    match entries.as_array() {
        Some(arr) => Value::Array(arr.iter().map(|e| {
            let mut obj = serde_json::Map::new();
            if let Some(n) = e["name"].as_str() {
                obj.insert("name".into(), Value::String(n.to_owned()));
            }
            if let Some(t) = e["type"].as_str() {
                obj.insert("type".into(), Value::String(t.to_owned()));
            }
            if let Some(children) = e.get("children") {
                if !children.is_null() {
                    obj.insert("children".into(), slim_tree(children));
                }
            }
            Value::Object(obj)
        }).collect()),
        None => Value::Null,
    }
}
