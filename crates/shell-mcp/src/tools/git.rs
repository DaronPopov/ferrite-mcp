//! Git tools: structured project history, diff, and workspace state.
//!
//! Tools:
//!   git_log    — commit history with author/date/subject
//!   git_diff   — uncommitted changes as structured file + hunk data
//!   git_status — staged / unstaged / untracked + branch info

use std::path::PathBuf;
use serde_json::{json, Value};
use crate::protocol::ToolResult;

// ── git_log ───────────────────────────────────────────────────────────────────

pub fn git_log(args: &Value) -> Result<ToolResult, String> {
    let path   = args["path"].as_str().unwrap_or(".");
    let limit  = args["limit"].as_u64().unwrap_or(20) as usize;
    let author = args["author"].as_str().unwrap_or("");
    let since  = args["since"].as_str().unwrap_or("");
    let branch = args["branch"].as_str().unwrap_or("HEAD");
    let file   = args["file"].as_str().unwrap_or("");

    let root = git_root(path)?;

    // Format: hash|short|author|email|date(iso)|subject
    let fmt = "%H%x00%h%x00%an%x00%ae%x00%ai%x00%s";

    let mut cmd = std::process::Command::new("git");
    cmd.arg("log")
       .arg(format!("--format={fmt}"))
       .arg(format!("-{limit}"))
       .arg(branch)
       .current_dir(&root);

    if !author.is_empty() { cmd.arg(format!("--author={author}")); }
    if !since.is_empty()  { cmd.arg(format!("--since={since}")); }
    if !file.is_empty()   { cmd.args(["--", file]); }

    let out = cmd.output().map_err(|e| format!("git log: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Ok(ToolResult::error(format!("git log: {err}")));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let commits: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(6, '\x00').collect();
            if parts.len() < 6 { return None; }
            Some(json!({
                "hash":    parts[0],
                "short":   parts[1],
                "author":  parts[2],
                "email":   parts[3],
                "date":    parts[4],
                "subject": parts[5],
            }))
        })
        .collect();

    let branch_name = current_branch(&root).unwrap_or_else(|_| "unknown".into());

    Ok(ToolResult::json(&json!({
        "branch":  branch_name,
        "count":   commits.len(),
        "commits": commits,
    })))
}

// ── git_diff ──────────────────────────────────────────────────────────────────

pub fn git_diff(args: &Value) -> Result<ToolResult, String> {
    let path   = args["path"].as_str().unwrap_or(".");
    let staged = args["staged"].as_bool().unwrap_or(false);
    let file   = args["file"].as_str().unwrap_or("");
    let commit = args["commit"].as_str().unwrap_or("");

    let root = git_root(path)?;

    let mut cmd = std::process::Command::new("git");
    cmd.arg("diff").current_dir(&root);

    if staged  { cmd.arg("--staged"); }
    if !commit.is_empty() { cmd.arg(commit); }
    if !file.is_empty()   { cmd.args(["--", file]); }

    let out = cmd.output().map_err(|e| format!("git diff: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    if stdout.is_empty() {
        return Ok(ToolResult::json(&json!({
            "clean":             true,
            "total_additions":   0,
            "total_deletions":   0,
            "files":             [],
        })));
    }

    let files = parse_unified_diff(&stdout);
    let total_add: i64 = files.iter()
        .map(|f| f["additions"].as_i64().unwrap_or(0)).sum();
    let total_del: i64 = files.iter()
        .map(|f| f["deletions"].as_i64().unwrap_or(0)).sum();

    Ok(ToolResult::json(&json!({
        "clean":           false,
        "total_additions": total_add,
        "total_deletions": total_del,
        "file_count":      files.len(),
        "files":           files,
    })))
}

fn parse_unified_diff(diff: &str) -> Vec<Value> {
    let mut files: Vec<Value> = Vec::new();
    let mut cur_path  = String::new();
    let mut cur_hunks: Vec<Value> = Vec::new();
    let mut cur_hunk_header = String::new();
    let mut cur_hunk_lines: Vec<String> = Vec::new();
    let mut additions: i64 = 0;
    let mut deletions: i64 = 0;
    let mut in_hunk = false;

    let flush_hunk = |header: &str, lines: &[String], hunks: &mut Vec<Value>| {
        if !header.is_empty() {
            hunks.push(json!({ "header": header, "lines": lines }));
        }
    };

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            // Save previous file
            if !cur_path.is_empty() {
                if in_hunk {
                    flush_hunk(&cur_hunk_header, &cur_hunk_lines, &mut cur_hunks);
                }
                files.push(json!({
                    "path":      cur_path,
                    "additions": additions,
                    "deletions": deletions,
                    "hunks":     cur_hunks,
                }));
            }
            // Parse new path from "diff --git a/PATH b/PATH"
            cur_path = line.splitn(4, ' ')
                .nth(3)
                .and_then(|s| s.strip_prefix("b/"))
                .unwrap_or("")
                .to_owned();
            cur_hunks  = Vec::new();
            additions  = 0;
            deletions  = 0;
            in_hunk    = false;
            cur_hunk_header = String::new();
            cur_hunk_lines  = Vec::new();
            continue;
        }

        if line.starts_with("+++ ") || line.starts_with("--- ")
           || line.starts_with("index ") || line.starts_with("new file")
           || line.starts_with("deleted file") || line.starts_with("Binary")
        {
            continue;
        }

        if line.starts_with("@@ ") {
            if in_hunk {
                flush_hunk(&cur_hunk_header, &cur_hunk_lines, &mut cur_hunks);
            }
            cur_hunk_header = line.to_owned();
            cur_hunk_lines  = Vec::new();
            in_hunk = true;
            continue;
        }

        if in_hunk {
            if line.starts_with('+') && !line.starts_with("+++") {
                additions += 1;
                cur_hunk_lines.push(line.to_owned());
            } else if line.starts_with('-') && !line.starts_with("---") {
                deletions += 1;
                cur_hunk_lines.push(line.to_owned());
            } else {
                cur_hunk_lines.push(line.to_owned());
            }
        }
    }

    // Flush last file
    if !cur_path.is_empty() {
        if in_hunk {
            flush_hunk(&cur_hunk_header, &cur_hunk_lines, &mut cur_hunks);
        }
        files.push(json!({
            "path":      cur_path,
            "additions": additions,
            "deletions": deletions,
            "hunks":     cur_hunks,
        }));
    }

    files
}

// ── git_status ────────────────────────────────────────────────────────────────

pub fn git_status(args: &Value) -> Result<ToolResult, String> {
    let path = args["path"].as_str().unwrap_or(".");
    let root = git_root(path)?;

    // --porcelain=v1 -b gives branch header + XY status lines
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-b", "--untracked-files=all"])
        .current_dir(&root)
        .output()
        .map_err(|e| format!("git status: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Ok(ToolResult::error(format!("git status: {err}")));
    }

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let mut staged:    Vec<Value> = Vec::new();
    let mut unstaged:  Vec<Value> = Vec::new();
    let mut untracked: Vec<Value> = Vec::new();
    let mut branch = String::new();
    let mut ahead  = 0i64;
    let mut behind = 0i64;

    for line in stdout.lines() {
        if line.starts_with("## ") {
            // "## main...origin/main [ahead 1, behind 2]"
            let rest = &line[3..];
            if let Some((b, tracking)) = rest.split_once("...") {
                branch = b.to_owned();
                if tracking.contains('[') {
                    if let Some(bracket) = tracking.find('[') {
                        let info = &tracking[bracket+1..tracking.rfind(']').unwrap_or(tracking.len())];
                        for part in info.split(',') {
                            let p = part.trim();
                            if let Some(n) = p.strip_prefix("ahead ") {
                                ahead = n.trim().parse().unwrap_or(0);
                            } else if let Some(n) = p.strip_prefix("behind ") {
                                behind = n.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                }
            } else {
                branch = rest.split_whitespace().next().unwrap_or("").to_owned();
            }
            continue;
        }

        if line.len() < 3 { continue; }
        let x = &line[..1]; // staged status
        let y = &line[1..2]; // unstaged status
        let file_path = line[3..].to_owned();

        match (x, y) {
            ("?", "?") => untracked.push(json!({ "path": file_path })),
            _ => {
                if x != " " && x != "?" {
                    staged.push(json!({ "status": x, "path": file_path }));
                }
                if y != " " && y != "?" {
                    unstaged.push(json!({ "status": y, "path": file_path }));
                }
            }
        }
    }

    let clean = staged.is_empty() && unstaged.is_empty() && untracked.is_empty();

    Ok(ToolResult::json(&json!({
        "branch":    branch,
        "ahead":     ahead,
        "behind":    behind,
        "clean":     clean,
        "staged":    staged,
        "unstaged":  unstaged,
        "untracked": untracked,
    })))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn git_root(from: &str) -> Result<PathBuf, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(from)
        .output()
        .map_err(|e| format!("git: {e}"))?;

    if !out.status.success() {
        return Err(format!("git: '{from}' is not inside a git repository"));
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn current_branch(root: &PathBuf) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("git branch: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
