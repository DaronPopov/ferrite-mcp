//! Internal pre-tool git checkpoint guard.
//!
//! Runs a lightweight auto-checkpoint before selected mutating tools to keep
//! an auditable rollback trail for autonomous control-plane actions.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::config::GitConfig;
use crate::server::ServerState;
use crate::tools::git_write;
use crate::tools::project::expand_tilde;
use crate::tools::state::{read_config, read_cwd};

pub fn maybe_auto_checkpoint(
    tool: &str,
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
) -> Result<(), String> {
    if skip_by_tool(tool) || args["no_checkpoint"].as_bool().unwrap_or(false) {
        return Ok(());
    }

    let Some(class) = classify(tool, args) else {
        return Ok(());
    };

    let cfg = read_config(state).git.clone();
    let cwd = read_cwd(state);

    if !cfg.auto_checkpoint || !class_enabled(&cfg, class) {
        return Ok(());
    }

    let repo_path = args["path"].as_str().map(expand_tilde).unwrap_or(cwd);

    let add_mode = if cfg.add_mode == "all" {
        "all"
    } else {
        "tracked"
    };
    let cp_args = json!({
        "path": repo_path.display().to_string(),
        "stage": true,
        "add": add_mode,
        "commit": true,
        "push": false,
        "provenance": {
            "agent": "ferrite-mcp",
            "reason": format!("pre_{class}:{tool}"),
            "tags": ["auto-checkpoint", class, tool],
        },
        "message": format!("checkpoint: before {class} tool '{tool}'")
    });

    match git_write::git_checkpoint(&cp_args, state) {
        Ok(_) => {
            write_audit(tool, class, "committed", None, &repo_path);
            Ok(())
        }
        Err(e) => {
            if e.contains("not a git repo") {
                write_audit(tool, class, "skipped_non_repo", Some(&e), &repo_path);
                return Ok(());
            }
            write_audit(tool, class, "checkpoint_failed", Some(&e), &repo_path);
            if cfg.strict {
                Err(format!("auto-checkpoint failed before '{tool}': {e}"))
            } else {
                Ok(())
            }
        }
    }
}

fn classify(tool: &str, args: &Value) -> Option<&'static str> {
    if matches!(tool, "move_file" | "mkdir" | "delete_file" | "project_new") {
        return Some("write");
    }
    if matches!(
        tool,
        "build_check"
            | "test_run"
            | "cocotb_run"
            | "verilog_sim"
            | "vivado_tcl"
            | "chip_build_pipeline"
    ) {
        return Some("build");
    }
    if matches!(tool, "fpga_program" | "sync_project" | "remote_build") {
        return Some("deploy");
    }
    if tool == "gh_sync" && args["op"].as_str().unwrap_or("pull") == "push" {
        return Some("deploy");
    }
    None
}

fn class_enabled(cfg: &GitConfig, class: &str) -> bool {
    match class {
        "write" => cfg.before_write,
        "build" => cfg.before_build,
        "deploy" => cfg.before_deploy,
        _ => false,
    }
}

fn skip_by_tool(tool: &str) -> bool {
    matches!(tool, "git_checkpoint" | "git_commit")
}

fn write_audit(tool: &str, class: &str, status: &str, error: Option<&str>, repo_path: &PathBuf) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rec = json!({
        "ts": ts,
        "tool": tool,
        "class": class,
        "status": status,
        "repo_path": repo_path.display().to_string(),
        "error": error,
    });
    let line = match serde_json::to_string(&rec) {
        Ok(s) => s + "\n",
        Err(_) => return,
    };
    let path = audit_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

fn audit_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("ferrite").join("git_checkpoint_hook.jsonl")
}
