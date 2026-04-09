//! Filesystem tools: glob, which, read_file, list_dir, move_file, mkdir, delete_file.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::state::{resolve_or_cwd, resolve_path};

// ── read_file ─────────────────────────────────────────────────────────────────

/// Hard cap: never return more than this many lines in one call.
const MAX_LINES: usize = 2_000;

pub fn read_file(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path_str = args["path"]
        .as_str()
        .ok_or("read_file: 'path' is required")?;
    let path_buf = resolve_path(state, path_str)?;
    let path = path_buf.to_str().unwrap_or(path_str);

    let raw = std::fs::read_to_string(&path_buf).map_err(|e| format!("read_file: {path}: {e}"))?;

    let all_lines: Vec<&str> = raw.lines().collect();
    let total_lines = all_lines.len();

    // 1-indexed, inclusive range — both optional
    let start = (args["start_line"].as_u64().unwrap_or(1) as usize).max(1);
    let end = (args["end_line"].as_u64().unwrap_or(total_lines as u64) as usize).min(total_lines);

    // Clamp window to MAX_LINES
    let end = end.min(start.saturating_add(MAX_LINES).saturating_sub(1));
    let truncated =
        end < (args["end_line"].as_u64().unwrap_or(total_lines as u64) as usize).min(total_lines);

    let content: String = if start <= end && start <= total_lines {
        all_lines[(start - 1)..end].join("\n")
    } else {
        String::new()
    };

    Ok(ToolResult::json(&json!({
        "path":        path,
        "total_lines": total_lines,
        "start":       start,
        "end":         end,
        "truncated":   truncated,
        "content":     content,
    })))
}

// ── list_dir ──────────────────────────────────────────────────────────────────

pub fn list_dir(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"].as_str();
    let depth = (args["depth"].as_u64().unwrap_or(1) as usize).min(5);
    let show_hidden = args["show_hidden"].as_bool().unwrap_or(false);
    let max_cap: Option<usize> = args["max_entries"].as_u64().map(|n| n as usize);
    let limit = max_cap.unwrap_or(usize::MAX);

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

    let mut count = 0usize;
    let mut truncated = false;
    let entries = list_recursive(
        &root,
        depth,
        show_hidden,
        0,
        limit,
        &mut count,
        &mut truncated,
    );
    let total = count_entries(&entries);

    Ok(ToolResult::json(&json!({
        "path":      canonical,
        "depth":     depth,
        "entries":   entries,
        "total":     total,
        "truncated": truncated,
    })))
}

fn list_recursive(
    dir: &std::path::Path,
    max_depth: usize,
    show_hidden: bool,
    current_depth: usize,
    max_entries: usize,
    count: &mut usize,
    truncated: &mut bool,
) -> Vec<Value> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return vec![];
    };

    let mut entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    // Sort: dirs first, then alphabetical
    entries.sort_by(|a, b| {
        let a_dir = a.path().is_dir();
        let b_dir = b.path().is_dir();
        b_dir.cmp(&a_dir).then(a.file_name().cmp(&b.file_name()))
    });

    let mut result = Vec::new();
    for entry in &entries {
        if *count >= max_entries {
            *truncated = true;
            break;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        *count += 1;
        let path = entry.path();
        let meta = path.metadata().ok();

        let kind = if path.is_symlink() {
            "symlink"
        } else if path.is_dir() {
            "dir"
        } else {
            "file"
        };

        let size_bytes = meta.as_ref().map(|m| m.len());
        let modified_secs = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        // Recurse into directories if depth allows
        let children = if kind == "dir" && current_depth < max_depth.saturating_sub(1) {
            // Skip noisy dirs
            if name != "target" && name != "node_modules" && name != ".git" {
                Some(list_recursive(
                    &path,
                    max_depth,
                    show_hidden,
                    current_depth + 1,
                    max_entries,
                    count,
                    truncated,
                ))
            } else {
                Some(vec![json!({ "note": format!("{name}/ skipped (noisy)") })])
            }
        } else {
            None
        };

        result.push(json!({
            "name":          name,
            "path":          path.display().to_string(),
            "type":          kind,
            "size_bytes":    size_bytes,
            "modified_secs": modified_secs,
            "children":      children,
        }));
    }
    result
}

fn count_entries(entries: &[Value]) -> usize {
    entries.iter().fold(0, |acc, e| {
        let children = e["children"]
            .as_array()
            .map(|c| count_entries(c))
            .unwrap_or(0);
        acc + 1 + children
    })
}

// ── glob ─────────────────────────────────────────────────────────────────────

pub fn glob_files(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or("glob: 'pattern' is required")?;
    let max = args["max_results"].as_u64().unwrap_or(200) as usize;

    // If a cwd is provided and the pattern is relative, prepend it.
    let cwd = resolve_or_cwd(state, args["cwd"].as_str())?;
    let resolved_pattern = if !pattern.starts_with('/') {
        format!("{}/{pattern}", cwd.display())
    } else {
        pattern.to_owned()
    };

    let mut matches: Vec<String> = Vec::new();
    let mut truncated = false;

    match glob::glob(&resolved_pattern) {
        Err(e) => return Ok(ToolResult::error(format!("invalid glob pattern: {e}"))),
        Ok(paths) => {
            for entry in paths {
                match entry {
                    Ok(path) => {
                        if matches.len() >= max {
                            truncated = true;
                            break;
                        }
                        matches.push(path.display().to_string());
                    }
                    Err(_) => {} // permission denied etc — skip
                }
            }
        }
    }

    Ok(ToolResult::json(&json!({
        "matches": matches,
        "count": matches.len(),
        "truncated": truncated,
        "pattern": resolved_pattern,
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
    let max = args["max_results"].as_u64().unwrap_or(100) as usize;

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

    let mut changed: Vec<Value> = Vec::new();
    let mut truncated = false;

    collect_changed(&root, since_secs, &mut changed, &mut truncated, max, 0);

    // Sort newest-first
    changed.sort_by(|a, b| {
        let ta = a["modified_secs"].as_u64().unwrap_or(0);
        let tb = b["modified_secs"].as_u64().unwrap_or(0);
        tb.cmp(&ta)
    });

    Ok(ToolResult::json(&json!({
        "path":        root.display().to_string(),
        "since_secs":  since_secs,
        "now_secs":    now_secs,
        "count":       changed.len(),
        "truncated":   truncated,
        "changed":     changed,
    })))
}

fn collect_changed(
    dir: &std::path::Path,
    since: u64,
    out: &mut Vec<Value>,
    truncated: &mut bool,
    max: usize,
    depth: usize,
) {
    if depth > 20 || *truncated {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(|e| e.ok()) {
        if *truncated {
            break;
        }
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();

        // Skip noise
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }

        if path.is_dir() {
            collect_changed(&path, since, out, truncated, max, depth + 1);
        } else if path.is_file() {
            let mtime = path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d: std::time::Duration| d.as_secs())
                .unwrap_or(0);

            if mtime >= since {
                if out.len() >= max {
                    *truncated = true;
                    break;
                }
                out.push(json!({
                    "path":          path.display().to_string(),
                    "modified_secs": mtime,
                    "size_bytes":    path.metadata().ok().map(|m| m.len()),
                }));
            }
        }
    }
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

/// Scan .rs files in cwd upwards for `mod` or `use` lines referencing the file stem.
fn find_rust_references(src: &str) -> Vec<Value> {
    let stem = PathBuf::from(src)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();

    if stem.is_empty() {
        return vec![];
    }

    let mut results = Vec::new();
    let search_root = PathBuf::from(".");

    search_rs_refs(&search_root, &stem, &mut results, 0);
    results
}

fn search_rs_refs(dir: &PathBuf, stem: &str, out: &mut Vec<Value>, depth: usize) {
    if depth > 10 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "target" || name == ".git" {
            continue;
        }

        if path.is_dir() {
            search_rs_refs(&path, stem, out, depth + 1);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (i, line) in content.lines().enumerate() {
                let t = line.trim();
                if (t.starts_with("mod ")
                    || t.starts_with("pub mod ")
                    || t.starts_with("use ")
                    || t.starts_with("pub use "))
                    && t.contains(stem)
                {
                    out.push(serde_json::json!({
                        "file": path.display().to_string(),
                        "line": i + 1,
                        "content": t,
                    }));
                }
            }
        }
    }
}

// ── mkdir ─────────────────────────────────────────────────────────────────────

/// Create a directory. With parents=true (default), creates all intermediate dirs.
pub fn make_dir(args: &Value) -> Result<ToolResult, String> {
    let path = args["path"].as_str().ok_or("mkdir: 'path' required")?;
    let parents = args["parents"].as_bool().unwrap_or(true);

    let p = PathBuf::from(path);
    if p.exists() {
        return Ok(ToolResult::json(&serde_json::json!({
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

    Ok(ToolResult::json(&serde_json::json!({
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

    Ok(ToolResult::json(&serde_json::json!({
        "deleted": true,
        "path":    path,
    })))
}
