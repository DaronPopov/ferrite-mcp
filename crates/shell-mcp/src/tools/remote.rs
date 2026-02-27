//! Remote SSH tools (Tier 2) — laptop↔desktop split.
//!
//! Tools:
//!   remote_exec   — run a command on a remote host via SSH
//!   remote_build  — trigger a build on a remote machine, stream via bg job
//!   sync_project  — rsync project between local and remote

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;
use crate::tools::execution::run;
use crate::tools::project::expand_tilde;

// ── SSH shared options ─────────────────────────────────────────────────────────

const SSH_OPTS: &str = "-o StrictHostKeyChecking=no -o BatchMode=yes -o ConnectTimeout=10";

// ── remote_exec ───────────────────────────────────────────────────────────────

pub fn remote_exec(args: &Value) -> Result<ToolResult, String> {
    let host    = args["host"].as_str().ok_or("remote_exec: 'host' is required")?;
    let cmd     = args["cmd"].as_str().ok_or("remote_exec: 'cmd' is required")?;
    let timeout = args["timeout_secs"].as_u64().unwrap_or(60);

    // Build the remote shell command
    let remote_cmd = if let Some(cwd) = args["cwd"].as_str() {
        format!("cd {} && {}", shell_escape(cwd), cmd)
    } else {
        cmd.to_owned()
    };

    // Inject extra env vars inline
    let env_prefix = args["env"].as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| format!("{}={}", k, shell_escape(s))))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let full_remote_cmd = if env_prefix.is_empty() {
        remote_cmd.clone()
    } else {
        format!("env {} {}", env_prefix, remote_cmd)
    };

    let ssh_cmd = format!("ssh {SSH_OPTS} {host} {}", shell_quote(&full_remote_cmd));

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let raw = run(&ssh_cmd, &cwd, &[], "", Duration::from_secs(timeout));

    let mut result = raw.as_object().cloned().unwrap_or_default();
    result.insert("host".to_owned(), json!(host));
    result.insert("remote_cmd".to_owned(), json!(remote_cmd));

    Ok(ToolResult::json(&Value::Object(result)))
}

// ── remote_build ──────────────────────────────────────────────────────────────

pub fn remote_build(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let host    = args["host"].as_str().ok_or("remote_build: 'host' is required")?;
    let project = args["project"].as_str().ok_or("remote_build: 'project' is required")?;

    // Auto-detect build command from project type if not supplied
    let build_cmd = if let Some(c) = args["build_cmd"].as_str() {
        c.to_owned()
    } else {
        detect_build_cmd(project)
    };

    let remote_path = format!("~/{project}");
    let full_cmd = format!("cd {remote_path} && {build_cmd}");

    let label = args["label"].as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("remote_build_{host}_{project}"));

    let ssh_cmd = format!("ssh {SSH_OPTS} {host} {}", shell_quote(&full_cmd));

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let job = store.spawn(&ssh_cmd, cwd, Some(&label), vec![])?;

    Ok(ToolResult::json(&json!({
        "job_id":     job.job_id,
        "host":       host,
        "project":    project,
        "remote_path": remote_path,
        "cmd":        build_cmd,
        "label":      label,
        "note":       "Build running in background — use bg_wait to block, bg_status/bg_tail to monitor",
    })))
}

fn detect_build_cmd(project: &str) -> String {
    // Heuristics based on known project names
    if project.starts_with("ferrite") || project.contains("cuda") {
        "cargo build --release 2>&1".to_owned()
    } else if project.contains("processor_lab") || project.contains("verilog") {
        "echo 'Use chip_build_pipeline for RTL projects' 2>&1".to_owned()
    } else if project.contains("rust") || std::path::Path::new(&format!("~/{project}/Cargo.toml")).exists() {
        "cargo build --release 2>&1".to_owned()
    } else if std::path::Path::new(&format!("~/{project}/package.json")).exists() {
        "npm run build 2>&1".to_owned()
    } else if std::path::Path::new(&format!("~/{project}/Makefile")).exists() {
        "make 2>&1".to_owned()
    } else {
        "cargo build --release 2>&1".to_owned()
    }
}

// ── sync_project ──────────────────────────────────────────────────────────────

pub fn sync_project(args: &Value) -> Result<ToolResult, String> {
    let project   = args["project"].as_str().ok_or("sync_project: 'project' is required")?;
    let host      = args["host"].as_str().ok_or("sync_project: 'host' is required")?;
    let direction = args["direction"].as_str().unwrap_or("push");
    let dry_run   = args["dry_run"].as_bool().unwrap_or(false);

    let default_excludes = [
        "target/",
        "*.bit",
        ".cache/",
        "*.runs/",
        "node_modules/",
        "*.ip_user_files/",
        "*.sim/",
        "__pycache__/",
        "*.pyc",
        ".git/",
        "*.jou",
        "*.log",
    ];

    let extra_excludes: Vec<String> = args["extra_excludes"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();

    let mut exclude_args = String::new();
    let extra_refs: Vec<&str> = extra_excludes.iter().map(|s| s.as_str()).collect();
    for exc in default_excludes.iter().copied().chain(extra_refs.iter().copied()) {
        exclude_args.push_str(&format!(" --exclude={}", shell_escape(exc)));
    }

    let dry_flag = if dry_run { " --dry-run" } else { "" };
    let local_path = expand_tilde(&format!("~/{project}"));
    let remote_path = format!("{host}:~/{project}/");

    let (src, dst) = match direction {
        "pull" => (remote_path.clone(), format!("{}/", local_path.display())),
        _      => (format!("{}/", local_path.display()), remote_path.clone()),
    };

    let cmd = format!("rsync -avz --progress{dry_flag}{exclude_args} {src} {dst}");

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let start = std::time::Instant::now();
    let raw = run(&cmd, &cwd, &[], "", Duration::from_secs(300));
    let duration_ms = start.elapsed().as_millis() as u64;

    let stdout = raw["stdout"].as_str().unwrap_or("").to_owned();
    let success = raw["success"].as_bool().unwrap_or(false);

    // Count transferred files from rsync output
    let files_transferred: usize = stdout.lines()
        .filter(|l| !l.starts_with("sending") && !l.starts_with("receiving")
                 && !l.starts_with("sent") && !l.starts_with("total")
                 && !l.is_empty() && !l.starts_with("building"))
        .count();

    // Extract bytes from last line "sent N bytes  received M bytes"
    let bytes_sent: u64 = stdout.lines().rev()
        .find(|l| l.contains("sent") && l.contains("bytes"))
        .and_then(|l| {
            l.split_whitespace().nth(1).and_then(|s| s.replace(',', "").parse().ok())
        })
        .unwrap_or(0);

    Ok(ToolResult::json(&json!({
        "success":            success,
        "direction":          direction,
        "project":            project,
        "src":                src,
        "dst":                dst,
        "dry_run":            dry_run,
        "files_transferred":  files_transferred,
        "bytes_sent":         bytes_sent,
        "duration_ms":        duration_ms,
        "stdout":             stdout,
        "stderr":             raw["stderr"],
    })))
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Wrap a string in single quotes, escaping any internal single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape special characters for use inline (not quoted).
fn shell_escape(s: &str) -> String {
    // Simple approach: quote the whole thing
    shell_quote(s)
}
