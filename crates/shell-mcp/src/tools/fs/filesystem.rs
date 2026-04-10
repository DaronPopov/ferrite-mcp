//! Filesystem tools: read_file, list_dir, glob, which, changed_since,
//! move_file, mkdir, delete_file.
//!
//! Every read-side tool routes through the determinism layer:
//!
//! - `read_file` — routes through `tools::text` for bounded, binary-safe
//!   reads that return encoding + content hash.
//! - `list_dir` — routes through `tools::walk::walk_sequential`, so
//!   `.gitignore` and `.ferriteignore` are honoured and `max_depth` works.
//! - `glob` — routes through `tools::walk` with an `include` pattern, so
//!   brace expansion and gitignore apply.
//! - `changed_since` — routes through `tools::walk` with a post-yield
//!   `mtime >= since` filter.
//!
//! Write-side tools (`move_file`, `mkdir`, `delete_file`) are left as thin
//! syscalls for now. The full determinism-contract write surface lives in
//! `tools::edit` and is wired through separate MCP tools (`write_file`,
//! `edit_file`, `apply_patch`, …) in later steps.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::hash;
use crate::tools::state::{resolve_or_cwd, resolve_path};
use crate::tools::text::{self, TextOptions};
use crate::tools::walk::{self, FileType, WalkOptions};

// ── read_file ─────────────────────────────────────────────────────────────────

/// Hard cap on lines returned in a single call. Matches Claude Code's
/// practical ceiling — callers asking for more get a truncated slice and
/// a `truncated: true` flag.
const MAX_LINES_PER_CALL: usize = 2_000;

pub fn read_file(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path_str = args["path"]
        .as_str()
        .ok_or("read_file: 'path' is required")?;
    let path_buf = resolve_path(state, path_str)?;

    // Pull explicit line range if provided; default to "whole file
    // clamped to MAX_LINES_PER_CALL" which matches the historic API.
    let start_requested = args["start_line"].as_u64().map(|n| n as usize);
    let end_requested = args["end_line"].as_u64().map(|n| n as usize);

    let opts = TextOptions::default();
    let (text_str, report) = match text::read_all(&path_buf, &opts) {
        Ok(v) => v,
        Err(text::TextError::TooLarge { size, limit, .. }) => {
            return Ok(ToolResult::error(format!(
                "read_file: {} is {size} bytes, exceeds {limit}-byte budget — pass an explicit start_line/end_line range or use read_slice",
                path_buf.display()
            )));
        }
        Err(text::TextError::Binary { .. }) => {
            return Ok(ToolResult::error(format!(
                "read_file: {} looks binary (NUL bytes detected) — refusing to return as text",
                path_buf.display()
            )));
        }
        Err(e) => return Ok(ToolResult::error(format!("read_file: {e}"))),
    };

    let total_lines = report.line_count;

    // Clamp the requested range against file extent and the per-call cap.
    let start = start_requested.unwrap_or(1).max(1);
    let end = end_requested.unwrap_or(total_lines).min(total_lines);
    let window_end = end.min(start.saturating_add(MAX_LINES_PER_CALL).saturating_sub(1));
    let truncated_by_cap = window_end < end;

    let content = if start == 0 || start > total_lines || start > window_end {
        String::new()
    } else {
        slice_lines_from_string(&text_str, start, window_end)
    };

    Ok(ToolResult::json(&json!({
        "path":         path_buf.display().to_string(),
        "total_lines":  total_lines,
        "start":        start,
        "end":          window_end,
        "truncated":    truncated_by_cap,
        "encoding":     report.encoding,
        "size_bytes":   report.size_bytes,
        "content_hash": hash::hash_bytes(text_str.as_bytes()),
        "content":      content,
    })))
}

/// Slice a 1-indexed inclusive line window out of an already-decoded
/// string. Preserves line endings present in the source.
fn slice_lines_from_string(text: &str, start: usize, end: usize) -> String {
    if start == 0 || end == 0 || start > end {
        return String::new();
    }
    let bytes = text.as_bytes();
    let mut line_idx = 1usize;
    let mut byte_cursor = 0usize;
    let mut slice_start: Option<usize> = None;
    let mut slice_end: usize = bytes.len();
    while byte_cursor <= bytes.len() {
        if line_idx == start && slice_start.is_none() {
            slice_start = Some(byte_cursor);
        }
        if line_idx == end + 1 {
            slice_end = byte_cursor;
            break;
        }
        match bytes[byte_cursor..].iter().position(|&b| b == b'\n') {
            Some(off) => {
                byte_cursor += off + 1;
                line_idx += 1;
            }
            None => {
                slice_end = bytes.len();
                break;
            }
        }
    }
    let s = slice_start.unwrap_or(bytes.len());
    text[s..slice_end].to_owned()
}

// ── list_dir ──────────────────────────────────────────────────────────────────

pub fn list_dir(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"].as_str();
    let depth = (args["depth"].as_u64().unwrap_or(1) as usize).min(10);
    let show_hidden = args["show_hidden"].as_bool().unwrap_or(false);
    let respect_gitignore = args["respect_gitignore"].as_bool().unwrap_or(true);
    let max_entries = args["max_entries"].as_u64().unwrap_or(5000) as usize;

    let root = resolve_or_cwd(state, path)?;
    if !root.exists() {
        return Ok(ToolResult::error(format!(
            "list_dir: {}: no such directory",
            root.display()
        )));
    }
    let canonical = root
        .canonicalize()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| root.display().to_string());

    let opts = WalkOptions {
        root: root.clone(),
        max_depth: Some(depth),
        hidden: show_hidden,
        respect_gitignore,
        respect_ferriteignore: true,
        ..WalkOptions::default()
    };

    let walk_iter = match walk::walk_sequential(&opts) {
        Ok(it) => it,
        Err(e) => return Ok(ToolResult::error(format!("list_dir: walk failed: {e:?}"))),
    };

    let mut entries: Vec<Value> = Vec::new();
    let mut truncated = false;
    for entry in walk_iter {
        // Skip the root itself — `walk` yields it as depth=0.
        if entry.depth == 0 {
            continue;
        }
        if entries.len() >= max_entries {
            truncated = true;
            break;
        }
        let kind = match entry.file_type {
            FileType::File => "file",
            FileType::Dir => "dir",
            FileType::Symlink => "symlink",
            FileType::Other => "other",
        };
        entries.push(json!({
            "name":          entry.path.file_name().map(|n| n.to_string_lossy().into_owned()),
            "path":          entry.path.display().to_string(),
            "relative":      entry.relative.display().to_string(),
            "depth":         entry.depth,
            "type":          kind,
            "size_bytes":    entry.size,
            "modified_secs": entry
                .mtime
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs()),
        }));
    }

    // Sort: dirs before files, then alphabetical by relative path.
    entries.sort_by(|a, b| {
        let a_dir = a["type"].as_str() == Some("dir");
        let b_dir = b["type"].as_str() == Some("dir");
        b_dir
            .cmp(&a_dir)
            .then_with(|| a["relative"].as_str().cmp(&b["relative"].as_str()))
    });

    Ok(ToolResult::json(&json!({
        "path":      canonical,
        "depth":     depth,
        "total":     entries.len(),
        "truncated": truncated,
        "entries":   entries,
    })))
}

// ── glob ─────────────────────────────────────────────────────────────────────

pub fn glob_files(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or("glob: 'pattern' is required")?;
    let max = args["max_results"].as_u64().unwrap_or(500) as usize;
    let respect_gitignore = args["respect_gitignore"].as_bool().unwrap_or(true);
    let hidden = args["hidden"].as_bool().unwrap_or(false);

    let cwd = resolve_or_cwd(state, args["cwd"].as_str())?;

    // If the pattern is absolute, strip off its directory prefix and use
    // the remainder as a relative glob against that directory. Otherwise
    // walk cwd with the pattern as-is.
    let (root, rel_pattern) = if pattern.starts_with('/') {
        let p = PathBuf::from(pattern);
        // Split at the first glob meta character.
        let meta_idx = p
            .components()
            .scan(PathBuf::new(), |acc, c| {
                acc.push(c.as_os_str());
                Some((acc.clone(), c.as_os_str().to_string_lossy().into_owned()))
            })
            .find(|(_, s)| s.contains('*') || s.contains('?') || s.contains('['))
            .map(|(acc, _)| acc);
        match meta_idx {
            Some(first_meta) => {
                let root_path = first_meta
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"));
                let rel = PathBuf::from(pattern)
                    .strip_prefix(&root_path)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| pattern.to_owned());
                (root_path, rel)
            }
            None => {
                // No metacharacters — treat as literal path
                let root_path = p
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"));
                let rel = p
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| pattern.to_owned());
                (root_path, rel)
            }
        }
    } else {
        (cwd, pattern.to_owned())
    };

    let opts = WalkOptions {
        root: root.clone(),
        include: vec![rel_pattern.clone()],
        hidden,
        respect_gitignore,
        respect_ferriteignore: true,
        ..WalkOptions::default()
    };

    let iter = match walk::walk_sequential(&opts) {
        Ok(it) => it,
        Err(walk::WalkError::InvalidPattern { pattern, reason }) => {
            return Ok(ToolResult::error(format!(
                "glob: invalid pattern {pattern:?}: {reason}"
            )));
        }
        Err(e) => return Ok(ToolResult::error(format!("glob: {e:?}"))),
    };

    let mut matches: Vec<String> = Vec::new();
    let mut truncated = false;
    for entry in iter {
        if matches!(entry.file_type, FileType::Dir) {
            continue;
        }
        if matches.len() >= max {
            truncated = true;
            break;
        }
        matches.push(entry.path.display().to_string());
    }
    let count = matches.len();

    Ok(ToolResult::json(&json!({
        "pattern":    pattern,
        "root":       root.display().to_string(),
        "matches":    matches,
        "count":      count,
        "truncated":  truncated,
    })))
}

// ── which ─────────────────────────────────────────────────────────────────────

pub fn which_bin(args: &Value) -> Result<ToolResult, String> {
    let name = args["name"].as_str().ok_or("which: 'name' is required")?;

    let path_var = std::env::var("PATH").unwrap_or_default();
    let found_path = path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|dir| std::path::PathBuf::from(dir).join(name))
        .find(|p| is_executable(p));

    match found_path {
        None => Ok(ToolResult::json(&json!({
            "found": false,
            "name": name,
        }))),
        Some(path) => {
            let path_str = path.display().to_string();
            let version = probe_version(&path_str);
            Ok(ToolResult::json(&json!({
                "found": true,
                "name": name,
                "path": path_str,
                "version": version,
            })))
        }
    }
}

// ── changed_since ─────────────────────────────────────────────────────────────

/// Find files modified after a given timestamp.
/// Provide either `since_secs` (Unix timestamp) or `since_relative` ("10m", "2h", "1d", "30s").
pub fn changed_since(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let root = resolve_or_cwd(state, args["path"].as_str())?;
    let max = args["max_results"].as_u64().unwrap_or(500) as usize;

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let since_secs = if let Some(ts) = args["since_secs"].as_u64() {
        ts
    } else if let Some(rel) = args["since_relative"].as_str() {
        let delta = parse_relative_duration(rel).ok_or_else(|| {
            format!("changed_since: bad since_relative '{rel}'. Use e.g. '10m', '2h', '1d'")
        })?;
        now_secs.saturating_sub(delta)
    } else {
        return Err("changed_since: provide 'since_secs' or 'since_relative'".into());
    };

    if !root.exists() {
        return Ok(ToolResult::error(format!(
            "changed_since: path '{}' not found",
            root.display()
        )));
    }

    let opts = WalkOptions {
        root: root.clone(),
        respect_gitignore: true,
        respect_ferriteignore: true,
        ..WalkOptions::default()
    };
    let iter = match walk::walk_sequential(&opts) {
        Ok(it) => it,
        Err(e) => return Ok(ToolResult::error(format!("changed_since: {e:?}"))),
    };

    let mut changed: Vec<Value> = Vec::new();
    let mut truncated = false;

    for entry in iter {
        if !matches!(entry.file_type, FileType::File) {
            continue;
        }
        let mtime_secs = entry
            .mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if mtime_secs < since_secs {
            continue;
        }
        if changed.len() >= max {
            truncated = true;
            break;
        }
        changed.push(json!({
            "path":          entry.path.display().to_string(),
            "relative":      entry.relative.display().to_string(),
            "modified_secs": mtime_secs,
            "size_bytes":    entry.size,
        }));
    }

    // Sort newest-first
    changed.sort_by(|a, b| {
        let ta = a["modified_secs"].as_u64().unwrap_or(0);
        let tb = b["modified_secs"].as_u64().unwrap_or(0);
        tb.cmp(&ta)
    });

    let count = changed.len();
    Ok(ToolResult::json(&json!({
        "path":        root.display().to_string(),
        "since_secs":  since_secs,
        "now_secs":    now_secs,
        "count":       count,
        "truncated":   truncated,
        "changed":     changed,
    })))
}

fn parse_relative_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num_str, unit) = s.split_at(s.len().saturating_sub(1));
    let num: u64 = num_str.parse().ok()?;
    match unit {
        "s" => Some(num),
        "m" => Some(num * 60),
        "h" => Some(num * 3600),
        "d" => Some(num * 86400),
        _ => None,
    }
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Try common `--version` / `-V` flags to extract a version string.
/// Bounded by a short timeout so nonstandard CLIs (e.g. ferrite shell) never hang.
fn probe_version(bin: &str) -> Option<String> {
    use std::fs::{self, OpenOptions};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(2);

    for flag in ["--version", "-V", "version"] {
        let unique_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let capture_path = std::env::temp_dir().join(format!(
            "ferrite-which-{}-{}.log",
            std::process::id(),
            unique_ns,
        ));
        let capture = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&capture_path)
        {
            Ok(file) => file,
            Err(_) => continue,
        };
        let capture_err = match capture.try_clone() {
            Ok(file) => file,
            Err(_) => {
                let _ = fs::remove_file(&capture_path);
                continue;
            }
        };

        let mut child = match Command::new(bin)
            .arg(flag)
            .stdin(Stdio::null())
            .stdout(Stdio::from(capture))
            .stderr(Stdio::from(capture_err))
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                let _ = fs::remove_file(&capture_path);
                continue;
            }
        };

        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break None,
            }
        };

        let text = fs::read_to_string(&capture_path).unwrap_or_default();
        let _ = fs::remove_file(&capture_path);

        let Some(st) = status else { continue };
        if !st.success() {
            continue;
        }

        if let Some(line) = text.lines().find(|l| !l.trim().is_empty()) {
            return Some(line.trim().to_owned());
        }
    }
    None
}

// ── move_file ─────────────────────────────────────────────────────────────────

/// Move or rename a file/directory. Optionally scans .rs files for mod/use
/// references that mention the old path, returning them for manual update.
pub fn move_file(args: &Value) -> Result<ToolResult, String> {
    let src = args["src"].as_str().ok_or("move_file: 'src' required")?;
    let dst = args["dst"].as_str().ok_or("move_file: 'dst' required")?;
    let find_refs = args["find_refs"].as_bool().unwrap_or(true);

    let src_path = PathBuf::from(src);
    let dst_path = PathBuf::from(dst);

    if !src_path.exists() {
        return Ok(ToolResult::error(format!(
            "move_file: source not found: {src}"
        )));
    }
    if dst_path.exists() {
        return Ok(ToolResult::error(format!(
            "move_file: destination already exists: {dst}"
        )));
    }

    // Create parent dirs of destination if needed
    if let Some(parent) = dst_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| format!("move_file: mkdir: {e}"))?;
        }
    }

    // Collect references before moving
    let refs: Vec<Value> = if find_refs {
        find_rust_references(src)
    } else {
        vec![]
    };

    std::fs::rename(&src_path, &dst_path).map_err(|e| format!("move_file: {e}"))?;

    Ok(ToolResult::json(&json!({
        "moved":       true,
        "src":         src,
        "dst":         dst,
        "ref_count":   refs.len(),
        "refs":        refs,
        "note":        if refs.is_empty() { "" } else { "refs may need updating" },
    })))
}

/// Scan .rs files in cwd for `mod` / `use` lines that mention the source
/// file's stem. Routes through the unified walker (respecting gitignore)
/// and only reads text files, so the old hand-rolled recursive walker
/// plus substring scan is gone.
fn find_rust_references(src: &str) -> Vec<Value> {
    let stem = PathBuf::from(src)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        return vec![];
    }

    let opts = WalkOptions {
        root: PathBuf::from("."),
        include: vec!["**/*.rs".to_owned()],
        respect_gitignore: true,
        respect_ferriteignore: true,
        ..WalkOptions::default()
    };
    let Ok(iter) = walk::walk_sequential(&opts) else {
        return vec![];
    };

    let mut out = Vec::new();
    for entry in iter {
        if !matches!(entry.file_type, FileType::File) {
            continue;
        }
        let Ok((content, _)) = text::read_all(&entry.path, &TextOptions::default()) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            let t = line.trim();
            if (t.starts_with("mod ")
                || t.starts_with("pub mod ")
                || t.starts_with("use ")
                || t.starts_with("pub use "))
                && t.contains(&stem)
            {
                out.push(json!({
                    "file": entry.path.display().to_string(),
                    "line": i + 1,
                    "content": t,
                }));
            }
        }
    }
    out
}

// ── mkdir ─────────────────────────────────────────────────────────────────────

/// Create a directory. With parents=true (default), creates all intermediate dirs.
pub fn make_dir(args: &Value) -> Result<ToolResult, String> {
    let path = args["path"].as_str().ok_or("mkdir: 'path' required")?;
    let parents = args["parents"].as_bool().unwrap_or(true);

    let p = PathBuf::from(path);
    if p.exists() {
        return Ok(ToolResult::json(&json!({
            "created": false,
            "path":    path,
            "note":    "already exists",
        })));
    }

    if parents {
        std::fs::create_dir_all(&p).map_err(|e| format!("mkdir: {e}"))?;
    } else {
        std::fs::create_dir(&p).map_err(|e| format!("mkdir: {e}"))?;
    }

    Ok(ToolResult::json(&json!({
        "created": true,
        "path":    path,
    })))
}

// ── delete_file ───────────────────────────────────────────────────────────────

/// Delete a single file. Refuses to delete directories (use exec for that).
pub fn delete_file(args: &Value) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("delete_file: 'path' required")?;
    let p = PathBuf::from(path);

    if !p.exists() {
        return Ok(ToolResult::error(format!("delete_file: not found: {path}")));
    }
    if p.is_dir() {
        return Ok(ToolResult::error(format!(
            "delete_file: '{path}' is a directory — use exec('rm -rf ...') explicitly"
        )));
    }

    std::fs::remove_file(&p).map_err(|e| format!("delete_file: {e}"))?;

    Ok(ToolResult::json(&json!({
        "deleted": true,
        "path":    path,
    })))
}
