//! git write tools.
//!
//! `git_checkpoint`: one call for stage/commit/push plus before/after status.
//! `git_commit`: backward-compatible wrapper retained for existing clients.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::project::expand_tilde;
use crate::tools::state::read_cwd;

#[derive(Clone)]
enum AddSpec {
    All,
    Tracked,
    Paths(Vec<String>),
}

#[derive(Default, Clone)]
struct Provenance {
    agent: Option<String>,
    session_id: Option<String>,
    reason: Option<String>,
    tags: Vec<String>,
}

#[derive(Default, Clone)]
struct StatusSummary {
    branch: String,
    ahead: i64,
    behind: i64,
    staged: usize,
    unstaged: usize,
    untracked: usize,
}

struct CheckpointRequest {
    root: PathBuf,
    add: AddSpec,
    stage: bool,
    commit: bool,
    allow_empty: bool,
    message: Option<String>,
    author: Option<String>,
    provenance: Provenance,
    push: bool,
    remote: String,
    branch: Option<String>,
}

struct CheckpointOutcome {
    root: PathBuf,
    before: StatusSummary,
    after: StatusSummary,
    staged_stat: String,
    commit_attempted: bool,
    committed: bool,
    commit_hash: String,
    commit_message: Option<String>,
    commit_stdout: String,
    commit_stderr: String,
    push_attempted: bool,
    push_ok: bool,
    push_remote: Option<String>,
    push_branch: Option<String>,
    push_stdout: String,
    push_stderr: String,
}

pub fn git_checkpoint(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let req = parse_checkpoint_request(args, state, false)?;
    let out = run_checkpoint(req)?;
    Ok(ToolResult::json(&json!({
        "ok": true,
        "root": out.root.display().to_string(),
        "before": summary_json(&out.before),
        "after": summary_json(&out.after),
        "staged_stat": out.staged_stat,
        "commit": {
            "attempted": out.commit_attempted,
            "committed": out.committed,
            "hash": out.commit_hash,
            "message": out.commit_message,
            "stdout": out.commit_stdout,
            "stderr": out.commit_stderr,
        },
        "push": {
            "attempted": out.push_attempted,
            "pushed": out.push_ok,
            "remote": out.push_remote,
            "branch": out.push_branch,
            "stdout": out.push_stdout,
            "stderr": out.push_stderr,
        }
    })))
}

pub fn git_commit(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let req = parse_checkpoint_request(args, state, true)?;
    let out = run_checkpoint(req)?;

    if !out.commit_attempted || !out.committed {
        return Ok(ToolResult::json(&json!({
            "committed": false,
            "reason": "nothing to commit — working tree clean or no changes staged",
            "root": out.root.display().to_string(),
        })));
    }

    let push_value = if out.push_attempted {
        json!({
            "pushed": out.push_ok,
            "remote": out.push_remote,
            "branch": out.push_branch,
            "stdout": out.push_stdout,
            "stderr": out.push_stderr,
        })
    } else {
        json!(null)
    };

    Ok(ToolResult::json(&json!({
        "committed": true,
        "hash": out.commit_hash,
        "message": out.commit_message.unwrap_or_default(),
        "staged_stat": out.staged_stat,
        "push": push_value,
        "root": out.root.display().to_string(),
    })))
}

fn parse_checkpoint_request(
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
    require_message: bool,
) -> Result<CheckpointRequest, String> {
    let base_path = if let Some(path_str) = args["path"].as_str() {
        expand_tilde(path_str)
    } else {
        read_cwd(state)
    };
    let root = resolve_git_root(&base_path)?;
    let push = args["push"].as_bool().unwrap_or(false);
    let remote = args["remote"].as_str().unwrap_or("origin").to_owned();
    let branch = args["branch"].as_str().map(ToOwned::to_owned);
    let author = args["author"].as_str().map(ToOwned::to_owned);
    let stage = args["stage"].as_bool().unwrap_or(true);
    let commit = if require_message {
        true
    } else {
        args["commit"].as_bool().unwrap_or(true)
    };
    let allow_empty = args["allow_empty"].as_bool().unwrap_or(false);
    let add = parse_add_spec(&args["add"])?;
    let message = args["message"].as_str().map(ToOwned::to_owned);

    if require_message && message.is_none() {
        return Err("git_commit: 'message' is required".to_owned());
    }

    let provenance = parse_provenance(&args["provenance"]);

    Ok(CheckpointRequest {
        root,
        add,
        stage,
        commit,
        allow_empty,
        message,
        author,
        provenance,
        push,
        remote,
        branch,
    })
}

fn parse_add_spec(value: &Value) -> Result<AddSpec, String> {
    if let Some(arr) = value.as_array() {
        let mut files = Vec::new();
        for item in arr {
            if let Some(path) = item.as_str() {
                if !path.trim().is_empty() {
                    files.push(path.to_owned());
                }
            }
        }
        if files.is_empty() {
            return Err("git_checkpoint: 'add' array is empty".to_owned());
        }
        return Ok(AddSpec::Paths(files));
    }

    match value.as_str().unwrap_or("tracked") {
        "all" => Ok(AddSpec::All),
        "tracked" => Ok(AddSpec::Tracked),
        "none" => Ok(AddSpec::Paths(Vec::new())),
        other => Ok(AddSpec::Paths(vec![other.to_owned()])),
    }
}

fn parse_provenance(value: &Value) -> Provenance {
    let mut p = Provenance::default();
    let Some(obj) = value.as_object() else {
        return p;
    };
    p.agent = obj
        .get("agent")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    p.session_id = obj
        .get("session_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    p.reason = obj
        .get("reason")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(tags) = obj.get("tags").and_then(Value::as_array) {
        p.tags = tags
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    p
}

fn run_checkpoint(req: CheckpointRequest) -> Result<CheckpointOutcome, String> {
    let before = collect_status(&req.root)?;

    if req.stage {
        stage_changes(&req.root, &req.add)?;
    }

    let staged_stat = staged_diff_stat(&req.root)?;
    let mut commit_attempted = false;
    let mut committed = false;
    let mut commit_hash = String::new();
    let mut commit_stdout = String::new();
    let mut commit_stderr = String::new();
    let mut final_message = None;

    if req.commit {
        if !staged_stat.is_empty() || req.allow_empty {
            commit_attempted = true;
            let base = req
                .message
                .unwrap_or_else(|| default_checkpoint_message(&before));
            let msg = decorate_commit_message(base, &req.provenance);
            let out = commit_changes(&req.root, &msg, req.author.as_deref(), req.allow_empty)?;
            committed = out.status.success();
            commit_stdout = stdout_trimmed(&out);
            commit_stderr = stderr_trimmed(&out);
            if committed {
                commit_hash = short_head_hash(&req.root).unwrap_or_default();
                final_message = Some(msg);
            } else {
                return Err(format!("git commit failed: {}", stderr_trimmed(&out)));
            }
        }
    }

    let mut push_attempted = false;
    let mut push_ok = false;
    let mut push_branch = None;
    let mut push_stdout = String::new();
    let mut push_stderr = String::new();
    let push_remote = if req.push {
        Some(req.remote.clone())
    } else {
        None
    };

    if req.push {
        push_attempted = true;
        let branch = req
            .branch
            .clone()
            .or_else(|| current_branch(&req.root))
            .unwrap_or_else(|| "main".to_owned());
        let push_target = resolve_push_target(&req.root, req.remote.as_str())?;
        let out = run_git(&req.root, &["push", push_target.as_str(), branch.as_str()])?;
        push_ok = out.status.success();
        push_stdout = stdout_trimmed(&out);
        push_stderr = stderr_trimmed(&out);
        push_branch = Some(branch);
    }

    let after = collect_status(&req.root)?;

    Ok(CheckpointOutcome {
        root: req.root,
        before,
        after,
        staged_stat,
        commit_attempted,
        committed,
        commit_hash,
        commit_message: final_message,
        commit_stdout,
        commit_stderr,
        push_attempted,
        push_ok,
        push_remote,
        push_branch,
        push_stdout,
        push_stderr,
    })
}

fn stage_changes(root: &Path, add: &AddSpec) -> Result<(), String> {
    let out = match add {
        AddSpec::All => run_git(root, &["add", "-A"])?,
        AddSpec::Tracked => run_git(root, &["add", "-u"])?,
        AddSpec::Paths(paths) if paths.is_empty() => return Ok(()),
        AddSpec::Paths(paths) => {
            let mut args = vec!["add", "--"];
            for p in paths {
                args.push(p.as_str());
            }
            run_git(root, &args)?
        }
    };
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("git add failed: {}", stderr_trimmed(&out)))
    }
}

fn collect_status(root: &Path) -> Result<StatusSummary, String> {
    let branch = current_branch(root).unwrap_or_else(|| "HEAD".to_owned());
    let (ahead, behind) = ahead_behind(root);
    let porcelain = run_git(root, &["status", "--porcelain"])?;
    if !porcelain.status.success() {
        return Err(format!("git status failed: {}", stderr_trimmed(&porcelain)));
    }
    let mut staged = 0usize;
    let mut unstaged = 0usize;
    let mut untracked = 0usize;
    for line in stdout_trimmed(&porcelain).lines() {
        if line.starts_with("??") {
            untracked += 1;
            continue;
        }
        let mut chars = line.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        if x != ' ' && x != '?' {
            staged += 1;
        }
        if y != ' ' {
            unstaged += 1;
        }
    }
    Ok(StatusSummary {
        branch,
        ahead,
        behind,
        staged,
        unstaged,
        untracked,
    })
}

fn ahead_behind(root: &Path) -> (i64, i64) {
    let Ok(out) = run_git(
        root,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    ) else {
        return (0, 0);
    };
    if !out.status.success() {
        return (0, 0);
    }
    let txt = stdout_trimmed(&out);
    let mut it = txt.split_whitespace();
    let behind = it.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let ahead = it.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    (ahead, behind)
}

fn staged_diff_stat(root: &Path) -> Result<String, String> {
    let out = run_git(root, &["diff", "--cached", "--stat"])?;
    if !out.status.success() {
        return Err(format!(
            "git diff --cached failed: {}",
            stderr_trimmed(&out)
        ));
    }
    Ok(stdout_trimmed(&out))
}

fn commit_changes(
    root: &Path,
    message: &str,
    author: Option<&str>,
    allow_empty: bool,
) -> Result<Output, String> {
    let mut args: Vec<String> = Vec::new();

    if !has_local_identity(root) {
        args.extend([
            "-c".into(),
            "user.name=ferrite".into(),
            "-c".into(),
            "user.email=ferrite@local".into(),
        ]);
    }

    args.push("commit".into());
    args.push("-m".into());
    args.push(message.into());
    if allow_empty {
        args.push("--allow-empty".into());
    }
    if let Some(a) = author {
        args.push(format!("--author={a}"));
    }

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git_timeout(root, &arg_refs, GIT_TIMEOUT)
}

fn has_local_identity(root: &Path) -> bool {
    let Ok(name) = run_git(root, &["config", "user.name"]) else {
        return false;
    };
    if !name.status.success() || stdout_trimmed(&name).is_empty() {
        return false;
    }
    let Ok(email) = run_git(root, &["config", "user.email"]) else {
        return false;
    };
    email.status.success() && !stdout_trimmed(&email).is_empty()
}

fn default_checkpoint_message(before: &StatusSummary) -> String {
    format!(
        "checkpoint: staged={} unstaged={} untracked={}",
        before.staged, before.unstaged, before.untracked
    )
}

fn decorate_commit_message(base: String, p: &Provenance) -> String {
    let mut lines = Vec::new();
    lines.push(base);
    lines.push(String::new());
    lines.push("Ferrite-Checkpoint: true".to_owned());
    if let Some(agent) = &p.agent {
        lines.push(format!("Ferrite-Agent: {agent}"));
    }
    if let Some(session_id) = &p.session_id {
        lines.push(format!("Ferrite-Session: {session_id}"));
    }
    if let Some(reason) = &p.reason {
        lines.push(format!("Ferrite-Reason: {reason}"));
    }
    if !p.tags.is_empty() {
        lines.push(format!("Ferrite-Tags: {}", p.tags.join(",")));
    }
    lines.join("\n")
}

fn short_head_hash(root: &Path) -> Option<String> {
    let out = run_git(root, &["rev-parse", "--short", "HEAD"]).ok()?;
    if out.status.success() {
        let h = stdout_trimmed(&out);
        if h.is_empty() {
            None
        } else {
            Some(h)
        }
    } else {
        None
    }
}

/// Default timeout for git write operations.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

fn run_git(root: &Path, args: &[&str]) -> Result<Output, String> {
    run_git_timeout(root, args, GIT_TIMEOUT)
}

/// Run a git command with a timeout. Kills the child if it exceeds the deadline.
fn run_git_timeout(cwd: &Path, args: &[&str], timeout: Duration) -> Result<Output, String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|e| format!("git {}: {e}", args.join(" ")))
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "git {}: timed out after {}s",
                        args.join(" "),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("git {}: {e}", args.join(" "))),
        }
    }
}

fn resolve_git_root(start: &Path) -> Result<PathBuf, String> {
    let out = run_git_timeout(
        start,
        &["rev-parse", "--show-toplevel"],
        Duration::from_secs(5),
    )?;
    if !out.status.success() {
        return Err(format!("not a git repo: {}", start.display()));
    }
    Ok(PathBuf::from(stdout_trimmed(&out)))
}

fn current_branch(root: &Path) -> Option<String> {
    let out = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = stdout_trimmed(&out);
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

fn resolve_push_target(root: &Path, remote: &str) -> Result<String, String> {
    let out = run_git(root, &["remote", "get-url", remote])?;
    if !out.status.success() {
        return Ok(remote.to_owned());
    }
    Ok(github_https_to_ssh(&stdout_trimmed(&out)).unwrap_or_else(|| remote.to_owned()))
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

fn summary_json(s: &StatusSummary) -> Value {
    json!({
        "branch": s.branch,
        "ahead": s.ahead,
        "behind": s.behind,
        "staged": s.staged,
        "unstaged": s.unstaged,
        "untracked": s.untracked,
        "dirty": s.staged > 0 || s.unstaged > 0 || s.untracked > 0
    })
}

fn stdout_trimmed(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn stderr_trimmed(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_owned()
}
