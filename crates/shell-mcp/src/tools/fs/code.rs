//! read_context and grep_code — code navigation tools.
//!
//! `read_context` pulls a window of lines around any file location and
//! routes through `tools::text` so encoding detection, binary rejection,
//! and byte budgets apply uniformly.
//!
//! `grep_code` uses the `ignore` walker under the hood for file discovery
//! (so `.gitignore` is respected), and `grep-searcher` + `grep-regex` for
//! the actual scan — the same engine ripgrep uses internally. An
//! external `rg` binary is still used opportunistically because it is
//! faster on large trees, but the fallback is no longer a hand-rolled
//! regex-per-line loop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::state::{resolve_or_cwd, resolve_path};
use crate::tools::text::{self, TextOptions};
use crate::tools::walk::{self, FileType, WalkOptions};

// ── read_context ──────────────────────────────────────────────────────────────

pub fn read_context(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let file = args["file"]
        .as_str()
        .ok_or("read_context: 'file' is required")?;
    let target = args["line"]
        .as_u64()
        .ok_or("read_context: 'line' is required")? as usize;
    let radius = args["radius"].as_u64().unwrap_or(10) as usize;
    let file_path = resolve_path(state, file)?;

    let (content, report) = match text::read_all(&file_path, &TextOptions::default()) {
        Ok(v) => v,
        Err(text::TextError::Binary { .. }) => {
            return Ok(ToolResult::error(format!(
                "read_context: {} is binary",
                file_path.display()
            )))
        }
        Err(text::TextError::TooLarge { size, limit, .. }) => {
            return Ok(ToolResult::error(format!(
                "read_context: {} is {size} bytes (limit {limit})",
                file_path.display()
            )))
        }
        Err(e) => return Ok(ToolResult::error(format!("read_context: {e}"))),
    };

    let all_lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total = report.line_count;
    if total == 0 {
        return Ok(ToolResult::json(&json!({
            "file":          file,
            "resolved_path": file_path.display().to_string(),
            "target_line":   target,
            "start":         0,
            "end":           0,
            "total_lines":   0,
            "encoding":      report.encoding,
            "lines":         [],
        })));
    }

    // target is 1-indexed; clamp to valid range
    let target = target.max(1).min(total);
    let start = target.saturating_sub(radius).max(1);
    let end = (target + radius).min(total);

    let lines: Vec<Value> = (start..=end)
        .map(|n| {
            let raw = all_lines.get(n - 1).copied().unwrap_or("");
            let trimmed = raw.strip_suffix('\n').unwrap_or(raw);
            let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
            json!({
                "num":       n,
                "content":   trimmed,
                "is_target": n == target,
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "file":          file,
        "resolved_path": file_path.display().to_string(),
        "target_line":   target,
        "start":         start,
        "end":           end,
        "total_lines":   total,
        "encoding":      report.encoding,
        "lines":         lines,
    })))
}

// ── grep_code ─────────────────────────────────────────────────────────────────

/// Default glob for `grep_code`. Broad, language-neutral — matches text
/// source files across the major ecosystems without the old
/// CUDA-flavoured default that silently excluded most of a typical repo.
const DEFAULT_GLOB: &str = "**/*.{rs,toml,md,txt,c,cc,cpp,cxx,h,hh,hpp,hxx,cu,cuh,py,pyi,js,jsx,mjs,cjs,ts,tsx,go,java,kt,kts,swift,rb,php,cs,scala,sh,bash,zsh,fish,yaml,yml,json,xml,html,css,scss,sass,sql,lua,r,jl,nim,zig,vim}";

pub fn grep_code(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or("grep_code: 'pattern' is required")?;
    let root = resolve_or_cwd(state, args["path"].as_str())?;
    let file_glob = args["glob"].as_str().unwrap_or(DEFAULT_GLOB);
    let max_results = args["max_results"].as_u64().unwrap_or(200) as usize;
    let ctx_lines = args["context_lines"].as_u64().unwrap_or(2) as usize;
    let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);

    // Try rg first — it's markedly faster on large trees.
    if let Some(result) = try_ripgrep(
        pattern,
        &root,
        file_glob,
        max_results,
        ctx_lines,
        case_insensitive,
    ) {
        return Ok(result);
    }

    // Fallback: walk + grep-searcher.
    rust_grep(
        pattern,
        &root,
        file_glob,
        max_results,
        ctx_lines,
        case_insensitive,
    )
}

fn try_ripgrep(
    pattern: &str,
    root: &Path,
    file_glob: &str,
    max_results: usize,
    ctx_lines: usize,
    case_insensitive: bool,
) -> Option<ToolResult> {
    // Check rg is available
    let rg = which_rg()?;

    let mut cmd = std::process::Command::new(&rg);
    cmd.arg("--json")
        .arg("--glob")
        .arg(file_glob)
        .arg("--context")
        .arg(ctx_lines.to_string());
    // Note: no --max-count (which would cap per-file, not total) and no
    // --max-filesize (which would silently skip large files). The old
    // code did both.

    if case_insensitive {
        cmd.arg("--ignore-case");
    }
    cmd.arg(pattern).arg(root);

    let out = cmd.output().ok()?;
    // rg exit 1 = no matches (not an error), exit 2 = real error
    if out.status.code() == Some(2) {
        return None;
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut matches: Vec<Value> = Vec::new();
    let mut truncated = false;

    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if msg["type"] == "match" {
            if matches.len() >= max_results {
                truncated = true;
                break;
            }
            let m = &msg["data"];
            let path = m["path"]["text"].as_str().unwrap_or("").to_owned();
            let line_num = m["line_number"].as_u64().unwrap_or(0);
            let content = m["lines"]["text"]
                .as_str()
                .unwrap_or("")
                .trim_end_matches('\n')
                .to_owned();

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

    let count = matches.len();
    Some(ToolResult::json(&json!({
        "pattern":   pattern,
        "matches":   matches,
        "count":     count,
        "truncated": truncated,
        "engine":    "ripgrep",
    })))
}

fn rust_grep(
    pattern: &str,
    root: &Path,
    file_glob: &str,
    max_results: usize,
    ctx_lines: usize,
    case_insensitive: bool,
) -> Result<ToolResult, String> {
    let matcher = RegexMatcher::new_line_matcher(&apply_case_flag(pattern, case_insensitive))
        .map_err(|e| format!("grep_code: invalid regex: {e}"))?;

    // Use the unified walker with the glob as an include filter.
    // This respects .gitignore and supports full brace expansion.
    let walk_opts = WalkOptions {
        root: root.to_path_buf(),
        include: vec![file_glob.to_owned()],
        respect_gitignore: true,
        respect_ferriteignore: true,
        ..WalkOptions::default()
    };
    let iter =
        walk::walk_sequential(&walk_opts).map_err(|e| format!("grep_code: walker error: {e:?}"))?;

    let files: Vec<PathBuf> = iter
        .filter(|e| matches!(e.file_type, FileType::File))
        .map(|e| e.path)
        .collect();

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .before_context(ctx_lines)
        .after_context(ctx_lines)
        .line_number(true)
        .build();

    let mut matches: Vec<Value> = Vec::new();
    let mut truncated = false;
    let files_searched = files.len();

    'outer: for path in &files {
        if matches.len() >= max_results {
            truncated = true;
            break;
        }
        let mut per_file: Vec<(u64, String, Option<u64>)> = Vec::new();
        let search_result = searcher.search_path(
            &matcher,
            path,
            UTF8(|lnum, line| {
                let col = matcher
                    .find(line.as_bytes())
                    .ok()
                    .flatten()
                    .map(|m| m.start() as u64);
                per_file.push((lnum, line.trim_end_matches('\n').to_owned(), col));
                Ok(true)
            }),
        );
        if search_result.is_err() {
            continue;
        }
        for (line_num, content, col) in per_file {
            if matches.len() >= max_results {
                truncated = true;
                break 'outer;
            }
            if col.is_some() {
                matches.push(json!({
                    "file":    path.display().to_string(),
                    "line":    line_num,
                    "col":     col,
                    "content": content,
                }));
            }
        }
    }

    let count = matches.len();
    Ok(ToolResult::json(&json!({
        "pattern":        pattern,
        "matches":        matches,
        "count":          count,
        "truncated":      truncated,
        "files_searched": files_searched,
        "engine":         "grep-searcher",
    })))
}

fn apply_case_flag(pattern: &str, case_insensitive: bool) -> String {
    if case_insensitive && !pattern.starts_with("(?i)") {
        format!("(?i){pattern}")
    } else {
        pattern.to_owned()
    }
}

fn which_rg() -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join("rg"))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}
