//! read_context and grep_code — code navigation tools.
//!
//! read_context: pull a window of lines around any file location.
//!               Called automatically after every build_check error.
//!
//! grep_code:    regex search across a file tree, returns matches with
//!               inline context. Uses ripgrep if available, pure-Rust fallback.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{json, Value};

use crate::protocol::ToolResult;

// ── read_context ──────────────────────────────────────────────────────────────

pub fn read_context(args: &Value) -> Result<ToolResult, String> {
    let file   = args["file"].as_str().ok_or("read_context: 'file' is required")?;
    let target = args["line"].as_u64().ok_or("read_context: 'line' is required")? as usize;
    let radius = args["radius"].as_u64().unwrap_or(10) as usize;

    let content = std::fs::read_to_string(file)
        .map_err(|e| format!("read_context: cannot read {file}: {e}"))?;

    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();

    // target is 1-indexed; clamp to valid range
    let target = target.max(1).min(total);
    let start  = target.saturating_sub(radius).max(1);
    let end    = (target + radius).min(total);

    let lines: Vec<Value> = (start..=end)
        .map(|n| json!({
            "num":       n,
            "content":   all_lines[n - 1],
            "is_target": n == target,
        }))
        .collect();

    Ok(ToolResult::json(&json!({
        "file":        file,
        "target_line": target,
        "start":       start,
        "end":         end,
        "total_lines": total,
        "lines":       lines,
    })))
}

// ── grep_code ─────────────────────────────────────────────────────────────────

pub fn grep_code(args: &Value) -> Result<ToolResult, String> {
    let pattern     = args["pattern"].as_str().ok_or("grep_code: 'pattern' is required")?;
    let root        = args["path"].as_str().unwrap_or(".");
    let file_glob   = args["glob"].as_str().unwrap_or("**/*.{rs,cu,c,cpp,h,cuh,py,toml}");
    let max_results = args["max_results"].as_u64().unwrap_or(50) as usize;
    let ctx_lines   = args["context_lines"].as_u64().unwrap_or(2) as usize;
    let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);

    // Try rg first — much faster on large trees
    if let Some(result) = try_ripgrep(pattern, root, file_glob, max_results, ctx_lines, case_insensitive) {
        return Ok(result);
    }

    // Pure-Rust fallback
    rust_grep(pattern, root, file_glob, max_results, ctx_lines, case_insensitive)
}

fn try_ripgrep(
    pattern: &str,
    root: &str,
    file_glob: &str,
    max_results: usize,
    ctx_lines: usize,
    case_insensitive: bool,
) -> Option<ToolResult> {
    // Check rg is available
    let rg = which_bin("rg")?;

    let mut cmd = std::process::Command::new(&rg);
    cmd.arg("--json")
       .arg("--max-count").arg("1")           // matches per file (rg counts differently)
       .arg("--glob").arg(file_glob)
       .arg("--context").arg(ctx_lines.to_string())
       .arg("--max-filesize").arg("1M");

    if case_insensitive { cmd.arg("--ignore-case"); }
    cmd.arg(pattern).arg(root);

    let out = cmd.output().ok()?;
    // rg exit 1 = no matches (not an error), exit 2 = real error
    if out.status.code() == Some(2) { return None; }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut matches: Vec<Value> = Vec::new();
    let mut truncated = false;

    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { continue };
        if msg["type"] == "match" {
            if matches.len() >= max_results {
                truncated = true;
                break;
            }
            let m = &msg["data"];
            let path = m["path"]["text"].as_str().unwrap_or("").to_owned();
            let line_num = m["line_number"].as_u64().unwrap_or(0);
            let content  = m["lines"]["text"].as_str().unwrap_or("").trim_end_matches('\n').to_owned();

            // Extract submatch column
            let col = m["submatches"][0]["start"].as_u64();

            matches.push(json!({
                "file":    path,
                "line":    line_num,
                "col":     col,
                "content": content,
            }));
        }
    }

    Some(ToolResult::json(&json!({
        "pattern":   pattern,
        "matches":   matches,
        "count":     matches.len(),
        "truncated": truncated,
        "engine":    "ripgrep",
    })))
}

fn rust_grep(
    pattern: &str,
    root: &str,
    file_glob: &str,
    max_results: usize,
    ctx_lines: usize,
    case_insensitive: bool,
) -> Result<ToolResult, String> {
    let re = {
        let pat = if case_insensitive {
            format!("(?i){pattern}")
        } else {
            pattern.to_owned()
        };
        Regex::new(&pat).map_err(|e| format!("grep_code: invalid regex: {e}"))?
    };

    // Build file list. The `glob` crate doesn't support {a,b} alternation,
    // so we walk the tree and filter by allowed extensions manually.
    let allowed_exts: Vec<&str> = file_glob
        .trim_start_matches("**/")
        .trim_start_matches("*.")
        .trim_matches('{')
        .trim_matches('}')
        .split(',')
        .map(str::trim)
        .collect();

    let search_root = if root.is_empty() || root == "." {
        PathBuf::from(".")
    } else {
        PathBuf::from(root)
    };

    let mut matches: Vec<Value> = Vec::new();
    let mut truncated = false;
    let mut files_searched = 0usize;

    let paths: Vec<PathBuf> = collect_files(&search_root, &allowed_exts);

    'outer: for path in &paths {
        files_searched += 1;

        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let all_lines: Vec<&str> = content.lines().collect();

        for (idx, line) in all_lines.iter().enumerate() {
            if let Some(m) = re.find(line) {
                if matches.len() >= max_results {
                    truncated = true;
                    break 'outer;
                }

                let line_num = idx + 1;
                let before: Vec<Value> = (idx.saturating_sub(ctx_lines)..idx)
                    .map(|i| json!({"num": i+1, "content": all_lines[i]}))
                    .collect();
                let after: Vec<Value> = ((idx+1)..(idx+1+ctx_lines).min(all_lines.len()))
                    .map(|i| json!({"num": i+1, "content": all_lines[i]}))
                    .collect();

                matches.push(json!({
                    "file":           path.display().to_string(),
                    "line":           line_num,
                    "col":            m.start() + 1,
                    "content":        line.trim_end(),
                    "context_before": before,
                    "context_after":  after,
                }));
            }
        }
    }

    Ok(ToolResult::json(&json!({
        "pattern":        pattern,
        "matches":        matches,
        "count":          matches.len(),
        "truncated":      truncated,
        "files_searched": files_searched,
        "engine":         "rust-builtin",
    })))
}

/// Walk a directory tree and collect files whose extensions match `allowed`.
fn collect_files(root: &Path, allowed: &[&str]) -> Vec<PathBuf> {
    let mut results = Vec::new();
    collect_recursive(root, allowed, &mut results, 0);
    results
}

fn collect_recursive(dir: &Path, allowed: &[&str], out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 20 { return; } // guard against deep symlink loops
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden dirs and common noise dirs
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" || name == "node_modules" { continue; }
            collect_recursive(&path, allowed, out, depth + 1);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            // If no extension filter or file_glob is generic, include all
            if allowed.is_empty() || allowed.contains(&ext) || allowed.iter().any(|a| *a == "*") {
                out.push(path);
            }
        }
    }
}

fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var.split(':').filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}
