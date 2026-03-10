//! GitHub SSH tools — operate on DaronPopov repos via git@github.com.
//!
//! Tools:
//!   gh_clone  — clone a repo from DaronPopov GitHub via SSH
//!   gh_sync   — pull or push for a local git repo
//!   gh_status — git status across all known project roots

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::execution::run;
use crate::tools::project::expand_tilde;

const GITHUB_USER: &str = "DaronPopov";

// ── gh_clone ──────────────────────────────────────────────────────────────────

pub fn gh_clone(args: &Value) -> Result<ToolResult, String> {
    let repo = args["repo"].as_str().ok_or("gh_clone: 'repo' is required")?;
    let branch = args["branch"].as_str().unwrap_or("");
    let shallow = args["shallow"].as_bool().unwrap_or(false);

    let local_path = args["dest"].as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
            PathBuf::from(home).join(repo)
        });

    let ssh_url = format!("git@github.com:{GITHUB_USER}/{repo}.git");

    let mut cmd = format!("git clone");
    if !branch.is_empty() {
        cmd.push_str(&format!(" --branch {branch}"));
    }
    if shallow {
        cmd.push_str(" --depth 1");
    }
    cmd.push_str(&format!(" {} {}", ssh_url, local_path.display()));

    let cwd = std::env::temp_dir();
    let raw = run(&cmd, &cwd, &[], "", Duration::from_secs(120));

    let success = raw["success"].as_bool().unwrap_or(false);
    let stdout  = raw["stdout"].as_str().unwrap_or("").to_owned();
    let stderr  = raw["stderr"].as_str().unwrap_or("").to_owned();

    // Detect actual branch cloned
    let actual_branch = if !branch.is_empty() {
        branch.to_owned()
    } else {
        detect_branch(&local_path).unwrap_or_else(|| "main".to_owned())
    };

    Ok(ToolResult::json(&json!({
        "success":    success,
        "repo":       repo,
        "ssh_url":    ssh_url,
        "local_path": local_path.display().to_string(),
        "branch":     actual_branch,
        "stdout":     stdout,
        "stderr":     stderr,
        "duration_ms": raw["duration_ms"],
    })))
}

// ── gh_sync ───────────────────────────────────────────────────────────────────

pub fn gh_sync(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let op       = args["op"].as_str().unwrap_or("pull");
    let branch   = args["branch"].as_str().unwrap_or("");
    let remote   = args["remote"].as_str().unwrap_or("origin");

    let path = resolve_work_path(args, state)?;
    let root = git_root_for(&path)?;

    // Determine current branch if not specified
    let actual_branch = if !branch.is_empty() {
        branch.to_owned()
    } else {
        detect_branch(&root).unwrap_or_else(|| "main".to_owned())
    };

    let cmd = match op {
        "push"  => format!("git push {remote} {actual_branch}"),
        "fetch" => format!("git fetch {remote}"),
        _       => format!("git pull {remote} {actual_branch}"),
    };

    let raw = run(&cmd, &root, &[], "", Duration::from_secs(120));

    let success = raw["success"].as_bool().unwrap_or(false);
    let stdout  = raw["stdout"].as_str().unwrap_or("").to_owned();
    let stderr  = raw["stderr"].as_str().unwrap_or("").to_owned();
    let combined = format!("{stdout}{stderr}");

    let fast_forward = combined.contains("Fast-forward") || combined.contains("fast-forward");

    Ok(ToolResult::json(&json!({
        "success":      success,
        "op":           op,
        "path":         root.display().to_string(),
        "remote":       remote,
        "branch":       actual_branch,
        "fast_forward": fast_forward,
        "stdout":       stdout,
        "stderr":       stderr,
        "duration_ms":  raw["duration_ms"],
    })))
}

// ── gh_status ─────────────────────────────────────────────────────────────────

pub fn gh_status(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let default_paths = [
        "~/processor_lab",
        "~/verilogchill",
        "~/ferrite-os-clean",
        "~/cpp_importable_test",
        "~/rust_shell",
        "~/aws_tool",
    ];

    let mut paths: Vec<PathBuf> = if let Some(arr) = args["paths"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str())
            .map(expand_tilde)
            .collect()
    } else {
        vec![resolve_work_path(args, state)?]
    };
    if args["paths"].as_array().is_none() {
        paths.extend(default_paths.iter().map(|p| expand_tilde(p)));
    }

    let mut results = Vec::new();
    let mut seen = BTreeSet::new();

    for root in discover_repo_roots(&paths) {
        if !seen.insert(root.clone()) {
            continue;
        }

        let project = root.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

        // Branch
        let branch = detect_branch(&root).unwrap_or_else(|| "unknown".to_owned());

        // Ahead/behind
        let (ahead, behind) = detect_ahead_behind(&root, &branch);

        // Dirty check
        let dirty = is_dirty(&root);

        // Last commit
        let last_commit = last_commit_info(&root);

        results.push(json!({
            "path":        root.display().to_string(),
            "project":     project,
            "branch":      branch,
            "ahead":       ahead,
            "behind":      behind,
            "dirty":       dirty,
            "last_commit": last_commit,
        }));
    }

    Ok(ToolResult::json(&json!({
        "count":   results.len(),
        "repos":   results,
    })))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn git_root_for(path: &PathBuf) -> Result<PathBuf, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .output()
        .map_err(|e| format!("git: {e}"))?;

    if !out.status.success() {
        return Err(format!("not a git repo: {}", path.display()));
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn detect_branch(root: &PathBuf) -> Option<String> {
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

fn detect_ahead_behind(root: &PathBuf, branch: &str) -> (i64, i64) {
    // Fetch remote info first (quiet)
    let _ = std::process::Command::new("git")
        .args(["fetch", "--quiet", "--no-tags"])
        .current_dir(root)
        .output();

    let out = std::process::Command::new("git")
        .args(["rev-list", "--left-right", "--count",
               &format!("origin/{branch}...HEAD")])
        .current_dir(root)
        .output();

    if let Ok(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout);
            let parts: Vec<&str> = s.trim().split_whitespace().collect();
            if parts.len() == 2 {
                let behind = parts[0].parse::<i64>().unwrap_or(0);
                let ahead  = parts[1].parse::<i64>().unwrap_or(0);
                return (ahead, behind);
            }
        }
    }
    (0, 0)
}

fn is_dirty(root: &PathBuf) -> bool {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output();
    match out {
        Ok(o) => !o.stdout.is_empty(),
        Err(_) => false,
    }
}

fn last_commit_info(root: &PathBuf) -> Value {
    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%h\x00%s\x00%ai", "HEAD"])
        .current_dir(root)
        .output();

    if let Ok(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout);
            let parts: Vec<&str> = s.trim().splitn(3, '\x00').collect();
            if parts.len() == 3 {
                return json!({
                    "hash":    parts[0],
                    "subject": parts[1],
                    "date":    parts[2],
                });
            }
        }
    }
    json!(null)
}

fn resolve_work_path(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<PathBuf, String> {
    if let Some(path) = args["path"].as_str() {
        Ok(expand_tilde(path))
    } else {
        Ok(state.lock()
            .map_err(|e| format!("state lock poisoned: {e}"))?
            .cwd
            .clone())
    }
}

fn discover_repo_roots(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for path in paths {
        collect_repo_roots(path, 2, &mut roots);
    }
    roots
}

fn collect_repo_roots(path: &PathBuf, depth: usize, roots: &mut Vec<PathBuf>) {
    if !path.exists() {
        return;
    }
    if let Ok(root) = git_root_for(path) {
        roots.push(root);
        return;
    }
    if depth == 0 || !path.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if !child.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        collect_repo_roots(&child, depth - 1, roots);
    }
}
