//! git_commit — stage files and commit (the write side of git).
//!
//! Parameters:
//!   message  (required) — commit message
//!   path     — repo root (default: cwd)
//!   add      — "all" (-A), "tracked" (-u), or array of file paths; default "tracked"
//!   push     — push after commit (default: false)
//!   remote   — remote name (default: "origin")
//!   branch   — branch to push (default: current branch)
//!   author   — "Name <email>" override (default: uses git config or ferrite fallback)

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::tools::execution::run;
use crate::tools::project::expand_tilde;

pub fn git_commit(args: &Value) -> Result<ToolResult, String> {
    let message = args["message"].as_str().ok_or("git_commit: 'message' is required")?;
    let path_str = args["path"].as_str().unwrap_or(".");
    let root = resolve_git_root(&expand_tilde(path_str))?;
    let push   = args["push"].as_bool().unwrap_or(false);
    let remote = args["remote"].as_str().unwrap_or("origin");

    // ── Stage files ──────────────────────────────────────────────────────────
    let add_spec = &args["add"];
    let stage_cmd = if let Some(files) = add_spec.as_array() {
        // Specific file list
        let paths: Vec<&str> = files.iter().filter_map(|v| v.as_str()).collect();
        if paths.is_empty() {
            return Err("git_commit: 'add' array is empty".to_owned());
        }
        format!("git add -- {}", paths.join(" "))
    } else {
        match add_spec.as_str().unwrap_or("tracked") {
            "all"     => "git add -A".to_owned(),
            "tracked" => "git add -u".to_owned(),
            other     => format!("git add -- {other}"),
        }
    };

    let stage = run(&stage_cmd, &root, &[], "", Duration::from_secs(15));
    if !stage["success"].as_bool().unwrap_or(false) {
        return Err(format!("git add failed: {}", stage["stderr"].as_str().unwrap_or("")));
    }

    // Check there's something staged
    let diff_check = run(
        "git diff --cached --stat",
        &root, &[], "", Duration::from_secs(5),
    );
    let staged_stat = diff_check["stdout"].as_str().unwrap_or("").trim().to_owned();
    if staged_stat.is_empty() {
        return Ok(ToolResult::json(&json!({
            "committed": false,
            "reason":    "nothing to commit — working tree clean or no changes staged",
        })));
    }

    // ── Resolve author ───────────────────────────────────────────────────────
    let author_flag = if let Some(a) = args["author"].as_str() {
        format!(r#" --author="{a}""#)
    } else {
        // Check if git user is configured; if not, use a fallback
        let name_check = std::process::Command::new("git")
            .args(["config", "user.name"])
            .current_dir(&root)
            .output()
            .ok();
        let has_config = name_check
            .as_ref()
            .map(|o| o.status.success() && !o.stdout.is_empty())
            .unwrap_or(false);
        if has_config {
            String::new()
        } else {
            r#" -c user.name="ferrite" -c user.email="ferrite@local""#.to_owned()
        }
    };

    // ── Commit ───────────────────────────────────────────────────────────────
    // Escape the message for shell
    let safe_msg = message.replace('\'', "'\\''");
    let commit_cmd = format!("git{author_flag} commit -m '{safe_msg}'");

    let commit = run(&commit_cmd, &root, &[], "", Duration::from_secs(30));
    let committed = commit["success"].as_bool().unwrap_or(false);

    if !committed {
        return Err(format!("git commit failed: {}", commit["stderr"].as_str().unwrap_or("")));
    }

    // Parse the commit hash from output
    let commit_out = commit["stdout"].as_str().unwrap_or("");
    let hash = extract_hash(commit_out);

    // ── Optional push ────────────────────────────────────────────────────────
    let push_result = if push {
        let branch = if let Some(b) = args["branch"].as_str() {
            b.to_owned()
        } else {
            current_branch(&root).unwrap_or_else(|| "main".to_owned())
        };

        let push_cmd = format!("git push {remote} {branch}");
        let pr = run(&push_cmd, &root, &[], "", Duration::from_secs(60));
        json!({
            "pushed":   pr["success"].as_bool().unwrap_or(false),
            "remote":   remote,
            "branch":   branch,
            "stdout":   pr["stdout"].as_str().unwrap_or(""),
            "stderr":   pr["stderr"].as_str().unwrap_or(""),
        })
    } else {
        json!(null)
    };

    Ok(ToolResult::json(&json!({
        "committed":   committed,
        "hash":        hash,
        "message":     message,
        "staged_stat": staged_stat,
        "push":        push_result,
        "root":        root.display().to_string(),
    })))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn resolve_git_root(start: &PathBuf) -> Result<PathBuf, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("not a git repo: {}", start.display()));
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn current_branch(root: &PathBuf) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        None
    }
}

fn extract_hash(output: &str) -> String {
    // git commit output: "[main abc1234] message"
    for line in output.lines() {
        if line.starts_with('[') {
            if let Some(hash_part) = line.split_whitespace().nth(1) {
                let hash = hash_part.trim_end_matches(']');
                if hash.len() >= 6 {
                    return hash.to_owned();
                }
            }
        }
    }
    String::new()
}
