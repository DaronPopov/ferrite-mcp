//! GitHub SSH tools — generic git@github.com helpers.
//!
//! Tools:
//!   gh_clone  — clone a repo from GitHub via SSH (owner resolved from args/env/git config)
//!   gh_sync   — pull, push, or fetch for a local git repo
//!   gh_status — git status across user-supplied paths (or cwd by default)
//!
//! ## Owner resolution
//!
//! `gh_clone` accepts `repo` as either:
//!   - `"name"` — owner is resolved from (in priority): `owner` arg, `$GITHUB_USER`,
//!     `$GH_USER`, `git config github.user`, else error.
//!   - `"owner/name"` — owner is taken from the slash-separated string.
//!
//! No identity is hardcoded. The tool will refuse to operate if no owner can be
//! resolved, rather than guessing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::project::expand_tilde;
use crate::tools::state::read_cwd;

/// Resolve a GitHub owner from explicit args, env, or `git config`.
/// Never falls back to a hardcoded identity.
fn resolve_github_owner(args: &Value, repo_arg: &str) -> Result<(String, String), String> {
    // 1. owner/repo embedded in the repo arg
    if let Some((owner, repo)) = repo_arg.split_once('/') {
        if !owner.is_empty() && !repo.is_empty() && !repo.contains('/') {
            return Ok((owner.to_owned(), repo.to_owned()));
        }
    }
    // 2. explicit owner arg
    if let Some(o) = args["owner"].as_str() {
        if !o.is_empty() {
            return Ok((o.to_owned(), repo_arg.to_owned()));
        }
    }
    // 3. env vars
    for var in ["GITHUB_USER", "GH_USER", "GITHUB_OWNER"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Ok((v, repo_arg.to_owned()));
            }
        }
    }
    // 4. git config github.user
    if let Ok(out) = Command::new("git")
        .args(["config", "--get", "github.user"])
        .output()
    {
        if out.status.success() {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if !v.is_empty() {
                return Ok((v, repo_arg.to_owned()));
            }
        }
    }
    Err(
        "gh_clone: no GitHub owner specified. Pass `repo: \"owner/name\"`, set `owner`, \
         or configure `git config --global github.user <name>` (or set $GITHUB_USER)."
            .to_owned(),
    )
}

/// Validate that a token (repo name, branch, owner) contains only safe chars.
/// GitHub allows `[A-Za-z0-9._-]` for repo names; refs allow `/`.
fn validate_repo_token(s: &str, kind: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("gh: empty {kind}"));
    }
    if s.len() > 256 {
        return Err(format!("gh: {kind} too long"));
    }
    let ok = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(format!(
            "gh: invalid {kind} '{s}' (allowed: A-Z a-z 0-9 . _ -)"
        ));
    }
    if s.starts_with('-') || s.starts_with('.') {
        return Err(format!("gh: {kind} cannot start with '-' or '.'"));
    }
    Ok(())
}

/// Validate a git ref name. Slightly looser than repo names — allows `/`.
fn validate_ref(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("gh: empty ref".to_owned());
    }
    if s.len() > 256 {
        return Err("gh: ref too long".to_owned());
    }
    let ok = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
    if !ok {
        return Err(format!("gh: invalid ref '{s}'"));
    }
    if s.starts_with('-') {
        return Err("gh: ref cannot start with '-'".to_owned());
    }
    Ok(())
}

/// Run a git command directly (no shell), capturing stdout/stderr.
fn git_exec(args: &[&str], cwd: &Path, timeout: Duration) -> Value {
    let start = std::time::Instant::now();
    let mut child = match Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "success": false,
                "stdout":  "",
                "stderr":  format!("git: {e}"),
                "duration_ms": 0_u64,
            });
        }
    };

    // Simple timeout poll loop — git operations are short
    let deadline = start + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                return json!({
                    "success": false,
                    "stdout":  "",
                    "stderr":  format!("git timed out after {}s", timeout.as_secs()),
                    "duration_ms": (std::time::Instant::now() - start).as_millis() as u64,
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                return json!({
                    "success": false,
                    "stdout":  "",
                    "stderr":  format!("git wait: {e}"),
                    "duration_ms": (std::time::Instant::now() - start).as_millis() as u64,
                });
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return json!({
                "success": false,
                "stdout":  "",
                "stderr":  format!("git output: {e}"),
                "duration_ms": (std::time::Instant::now() - start).as_millis() as u64,
            });
        }
    };
    json!({
        "success":     out.status.success(),
        "stdout":      String::from_utf8_lossy(&out.stdout).to_string(),
        "stderr":      String::from_utf8_lossy(&out.stderr).to_string(),
        "duration_ms": (std::time::Instant::now() - start).as_millis() as u64,
    })
}

// ── gh_clone ──────────────────────────────────────────────────────────────────

pub fn gh_clone(args: &Value) -> Result<ToolResult, String> {
    let repo_arg = args["repo"]
        .as_str()
        .ok_or("gh_clone: 'repo' is required")?;

    // Resolve owner + repo (handles "owner/name" form, or pulls from env/git config)
    let (owner, repo) = resolve_github_owner(args, repo_arg)?;
    validate_repo_token(&owner, "owner")?;
    validate_repo_token(&repo, "repo")?;

    let branch = args["branch"].as_str().unwrap_or("");
    if !branch.is_empty() {
        validate_ref(branch)?;
    }
    let shallow = args["shallow"].as_bool().unwrap_or(false);

    let local_path = args["dest"].as_str().map(expand_tilde).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
        PathBuf::from(home).join(&repo)
    });

    let ssh_url = format!("git@github.com:{owner}/{repo}.git");

    // Build args slice — no shell, no interpolation
    let mut git_args: Vec<String> = vec!["clone".to_owned()];
    if !branch.is_empty() {
        git_args.push("--branch".to_owned());
        git_args.push(branch.to_owned());
    }
    if shallow {
        git_args.push("--depth".to_owned());
        git_args.push("1".to_owned());
    }
    // Use `--` to ensure the URL and dest are never treated as flags
    git_args.push("--".to_owned());
    git_args.push(ssh_url.clone());
    git_args.push(local_path.display().to_string());

    let arg_refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
    let raw = git_exec(&arg_refs, &std::env::temp_dir(), Duration::from_secs(120));

    let success = raw["success"].as_bool().unwrap_or(false);
    let stdout = raw["stdout"].as_str().unwrap_or("").to_owned();
    let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();

    let actual_branch = if !branch.is_empty() {
        branch.to_owned()
    } else {
        detect_branch(&local_path).unwrap_or_else(|| "main".to_owned())
    };

    Ok(ToolResult::json(&json!({
        "success":    success,
        "owner":      owner,
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
    let op = args["op"].as_str().unwrap_or("pull");
    if !matches!(op, "push" | "pull" | "fetch") {
        return Err(format!("gh_sync: invalid op '{op}' (push|pull|fetch)"));
    }
    let branch = args["branch"].as_str().unwrap_or("");
    let remote = args["remote"].as_str().unwrap_or("origin");
    validate_repo_token(remote, "remote")?;
    if !branch.is_empty() {
        validate_ref(branch)?;
    }

    let path = resolve_work_path(args, state)?;
    let root = git_root_for(&path)?;

    // Determine current branch if not specified
    let actual_branch = if !branch.is_empty() {
        branch.to_owned()
    } else {
        detect_branch(&root).unwrap_or_else(|| "main".to_owned())
    };
    validate_ref(&actual_branch)?;

    // For push, optionally rewrite the remote URL to SSH form (in-process,
    // not interpolated into a shell command).
    let push_target = if op == "push" {
        resolve_push_target(&root, remote)
    } else {
        remote.to_owned()
    };
    // The SSH-rewritten target may contain `:` and `@` — that's fine for
    // Command::args, but we still validate the original remote name.

    let git_args: Vec<&str> = match op {
        "push" => vec!["push", &push_target, &actual_branch],
        "fetch" => vec!["fetch", remote],
        _ => vec!["pull", remote, &actual_branch],
    };

    let raw = git_exec(&git_args, &root, Duration::from_secs(120));

    let success = raw["success"].as_bool().unwrap_or(false);
    let stdout = raw["stdout"].as_str().unwrap_or("").to_owned();
    let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();
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
    // Resolution order:
    //   - explicit `paths` array  → use those only
    //   - explicit `path` string  → use that only
    //   - no args                  → server cwd (depth-2 shallow scan)
    //
    // No identity-specific paths are baked in.
    let paths: Vec<PathBuf> = if let Some(arr) = args["paths"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str())
            .map(expand_tilde)
            .collect()
    } else if let Some(p) = args["path"].as_str() {
        vec![expand_tilde(p)]
    } else {
        vec![read_cwd(state)]
    };

    let mut results = Vec::new();
    let mut seen = BTreeSet::new();

    for root in discover_repo_roots(&paths) {
        if !seen.insert(root.clone()) {
            continue;
        }

        let project = root
            .file_name()
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

fn resolve_push_target(root: &PathBuf, remote: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(root)
        .output();

    let Ok(out) = out else {
        return remote.to_owned();
    };
    if !out.status.success() {
        return remote.to_owned();
    }

    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    github_https_to_ssh(&url).unwrap_or_else(|| remote.to_owned())
}

fn github_https_to_ssh(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("git@github.com:{owner}/{repo}.git"))
}

fn detect_ahead_behind(root: &Path, branch: &str) -> (i64, i64) {
    // Detached HEAD → no meaningful upstream comparison
    if branch == "HEAD" || branch.is_empty() {
        return (0, 0);
    }
    if validate_ref(branch).is_err() {
        return (0, 0);
    }

    let upstream = format!("origin/{branch}");
    let range = format!("{upstream}...HEAD");

    // Check that origin/<branch> tracking ref exists before comparing
    let ref_check = git_exec(
        &["rev-parse", "--verify", &upstream],
        root,
        Duration::from_secs(4),
    );
    if !ref_check["success"].as_bool().unwrap_or(false) {
        return (0, 0);
    }

    let rev_list = git_exec(
        &["rev-list", "--left-right", "--count", &range],
        root,
        Duration::from_secs(8),
    );

    if rev_list["success"].as_bool().unwrap_or(false) {
        let s = rev_list["stdout"].as_str().unwrap_or("").trim().to_owned();
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() == 2 {
            let behind = parts[0].parse::<i64>().unwrap_or(0);
            let ahead = parts[1].parse::<i64>().unwrap_or(0);
            return (ahead, behind);
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
        Ok(read_cwd(state))
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
