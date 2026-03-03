//! Authorization + audit layer for MCP tool calls.
//!
//! Phase 1 scope:
//! - Enforce deny-by-default for `fercuda_runtime` actions.
//! - Leave non-feRcuda tools unchanged for compatibility.
//! - Emit audit records to ~/.local/share/ferrite/authz_audit.jsonl.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct AuthzPolicy {
    #[serde(default = "default_default_role")]
    pub default_role: String,
    #[serde(default)]
    pub principals: HashMap<String, String>,
    #[serde(default)]
    pub roles: HashMap<String, RolePolicy>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RolePolicy {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub limits: RuntimeLimits,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub allowed: bool,
    pub principal: String,
    pub role: String,
    pub action: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RuntimeLimits {
    pub max_session_mutable_bytes: Option<u64>,
    pub max_session_immutable_bytes: Option<u64>,
    pub max_total_alloc_bytes: Option<u64>,
    pub max_concurrent_jobs: Option<u64>,
}

fn default_default_role() -> String {
    "observer".to_owned()
}

pub fn authorize(tool: &str, args: &Value) -> Decision {
    let action = action_for(tool, args);
    let principal = principal();

    if tool != "fercuda_runtime" {
        return Decision {
            allowed: true,
            principal,
            role: "n/a".to_owned(),
            action,
            reason: "outside_authz_scope".to_owned(),
        };
    }

    // Always allow read-only bootstrap/discovery ops so setup is possible
    // even before an authz policy file exists.
    if matches!(args["op"].as_str(), Some("status" | "guide")) {
        return Decision {
            allowed: true,
            principal,
            role: "bootstrap".to_owned(),
            action,
            reason: "bootstrap_read_only_allow".to_owned(),
        };
    }

    let policy = match load_policy() {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Decision {
                allowed: false,
                principal,
                role: "unknown".to_owned(),
                action,
                reason: "policy_missing_deny_by_default".to_owned(),
            };
        }
        Err(e) => {
            return Decision {
                allowed: false,
                principal,
                role: "unknown".to_owned(),
                action,
                reason: format!("policy_parse_error:{e}"),
            };
        }
    };

    let role = policy
        .principals
        .get(&principal)
        .cloned()
        .unwrap_or_else(|| policy.default_role.clone());

    let role_policy = match policy.roles.get(&role) {
        Some(r) => r,
        None => {
            return Decision {
                allowed: false,
                principal,
                role,
                action,
                reason: "role_not_defined".to_owned(),
            };
        }
    };

    if role_policy.deny.iter().any(|pat| matches_action(pat, &action)) {
        return Decision {
            allowed: false,
            principal,
            role,
            action,
            reason: "explicit_deny".to_owned(),
        };
    }

    if role_policy.allow.iter().any(|pat| matches_action(pat, &action)) {
        return Decision {
            allowed: true,
            principal,
            role,
            action,
            reason: "explicit_allow".to_owned(),
        };
    }

    Decision {
        allowed: false,
        principal,
        role,
        action,
        reason: "no_matching_allow_rule".to_owned(),
    }
}

pub fn audit(decision: &Decision, tool: &str, args: &Value) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let args_hash = fnv1a64_hex(&args.to_string());
    let record = serde_json::json!({
        "ts": ts,
        "principal": decision.principal,
        "role": decision.role,
        "tool": tool,
        "action": decision.action,
        "allowed": decision.allowed,
        "reason": decision.reason,
        "args_hash": args_hash,
    });

    let line = match serde_json::to_string(&record) {
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

pub fn principal() -> String {
    std::env::var("FERRITE_PRINCIPAL").unwrap_or_else(|_| "default".to_owned())
}

pub fn runtime_limits_for_principal(principal: &str) -> Option<RuntimeLimits> {
    let policy = load_policy().ok().flatten()?;
    let role = policy
        .principals
        .get(principal)
        .cloned()
        .unwrap_or_else(|| policy.default_role.clone());
    let rp = policy.roles.get(&role)?;
    Some(rp.limits.clone())
}

fn load_policy() -> Result<Option<AuthzPolicy>, String> {
    let path = policy_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    toml::from_str::<AuthzPolicy>(&text).map(Some).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn action_for(tool: &str, args: &Value) -> String {
    if let Some(op) = args["op"].as_str() {
        format!("{tool}.{op}")
    } else {
        tool.to_owned()
    }
}

fn matches_action(pattern: &str, action: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        return action.starts_with(prefix) && action.len() > prefix.len();
    }
    pattern == action
}

fn policy_path() -> PathBuf {
    if let Ok(p) = std::env::var("FERRITE_AUTHZ_POLICY") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
            PathBuf::from(home).join(".config")
        });
    base.join("ferrite").join("authz_policy.toml")
}

fn audit_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("ferrite").join("authz_audit.jsonl")
}

fn fnv1a64_hex(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
