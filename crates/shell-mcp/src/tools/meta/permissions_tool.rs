//! `pre_validate` and `permissions_setup` MCP tool handlers.

use serde_json::{json, Value};

use crate::permissions::{self, BlockerKind, SudoersStatus};
use crate::protocol::ToolResult;

// ── pre_validate ──────────────────────────────────────────────────────────────

pub fn pre_validate(args: &Value) -> Result<ToolResult, String> {
    let cmd = args["cmd"]
        .as_str()
        .ok_or("pre_validate: 'cmd' is required")?;

    let analysis = permissions::analyze(cmd);

    let sudo_status_str = match &analysis.sudo_status {
        SudoersStatus::Active => "active",
        SudoersStatus::StaleUser => "stale_user",
        SudoersStatus::Missing => "missing",
    };

    let blockers_json: Vec<Value> = analysis
        .blockers
        .iter()
        .map(|b| {
            json!({
                "kind": match b.kind {
                    BlockerKind::SudoNotCovered      => "sudo_not_covered",
                    BlockerKind::InteractivePrompt   => "interactive_prompt",
                    BlockerKind::ManualRequired      => "manual_required",
                },
                "description":   b.description,
                "auto_resolved": b.auto_resolved,
                "resolution":    b.resolution,
            })
        })
        .collect();

    let env_json: Value = analysis
        .injected_env
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect::<serde_json::Map<_, _>>()
        .into();

    let changed = analysis.original != analysis.rewritten;

    Ok(ToolResult::json(&json!({
        "ok":           analysis.all_clear,
        "all_clear":    analysis.all_clear,
        "original_cmd": analysis.original,
        "rewritten_cmd": analysis.rewritten,
        "cmd_changed":  changed,
        "injected_env": env_json,
        "blockers":     blockers_json,
        "blocker_count": analysis.blockers.len(),
        "unresolved":   analysis.blockers.iter().filter(|b| !b.auto_resolved).count(),
        "sudo_status":  sudo_status_str,
        "sudoers_path": permissions::sudoers_path(),
        "note": if analysis.all_clear {
            if changed {
                "Command auto-rewritten to non-interactive form. Safe to run via exec."
            } else {
                "Command is already non-interactive. Safe to run."
            }
        } else {
            "Unresolved blockers found. Check 'blockers' for details before running."
        },
    })))
}

// ── permissions_setup ─────────────────────────────────────────────────────────

/// Report the current sudoers status.  With `install: true`, attempt to
/// self-install the NOPASSWD entry without requiring a TTY — tries a cached
/// sudo credential first, then falls back to a graphical Polkit (pkexec) dialog.
pub fn permissions_setup(args: &Value) -> Result<ToolResult, String> {
    let show_entry = args["show_entry"].as_bool().unwrap_or(false);
    let install = args["install"].as_bool().unwrap_or(false);
    let path = permissions::sudoers_path();

    // ── Optional self-install ─────────────────────────────────────────────────
    let install_result: Option<String> = if install {
        match permissions::sudoers_status() {
            SudoersStatus::Active => Some("already_active".to_owned()),
            _ => match permissions::try_install_sudoers() {
                Ok(()) => Some("installed".to_owned()),
                Err(msg) => Some(format!("failed: {msg}")),
            },
        }
    } else {
        None
    };

    // ── Re-read status (may have just changed) ────────────────────────────────
    let status = permissions::sudoers_status();
    let entry = permissions::sudoers_entry_content();

    let status_str = match &status {
        SudoersStatus::Active => "active",
        SudoersStatus::StaleUser => "stale_user",
        SudoersStatus::Missing => "missing",
    };

    let advice = match &status {
        SudoersStatus::Active => {
            "Sudoers entry is active. All whitelisted privileged commands will run without a password prompt.".to_owned()
        }
        SudoersStatus::StaleUser => {
            format!(
                "Sudoers file exists at {path} but NOPASSWD probe failed — wrong user or broken entry. \
                 Re-run install.sh to regenerate, or call permissions_setup with install:true."
            )
        }
        SudoersStatus::Missing => {
            "Sudoers entry not installed. Call permissions_setup with install:true to attempt \
                 auto-install (uses cached sudo token or graphical Polkit prompt), or run install.sh \
                 manually for a one-time interactive sudo prompt.".to_string()
        }
    };

    let mut result = json!({
        "sudoers_status": status_str,
        "sudoers_path":   path,
        "advice":         advice,
    });

    if let Some(r) = install_result {
        result["install_result"] = Value::String(r);
    }

    if show_entry {
        result["entry_content"] = Value::String(entry);
    }

    Ok(ToolResult::json(&result))
}
