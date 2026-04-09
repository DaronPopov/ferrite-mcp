//! ux_wizard — step-by-step interactive config flow.
//!
//! Current workflow: `role_limits`
//!   start  -> question 1
//!   answer -> stores answer, returns next question
//!   status -> current answers + pending question
//!   apply  -> writes/updates authz policy
//!   reset  -> clear wizard state

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::protocol::ToolResult;

#[derive(Default, Clone)]
struct WizardState {
    workflow: String,
    role: Option<String>,
    max_total_alloc_bytes: Option<u64>,
    max_concurrent_jobs: Option<u64>,
}

static WIZARDS: OnceLock<Mutex<HashMap<String, WizardState>>> = OnceLock::new();

fn wizards() -> &'static Mutex<HashMap<String, WizardState>> {
    WIZARDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn principal() -> String {
    std::env::var("FERRITE_PRINCIPAL").unwrap_or_else(|_| "default".to_owned())
}

pub fn ux_wizard(args: &Value) -> Result<ToolResult, String> {
    let op = args["op"].as_str().unwrap_or("status");
    let p = principal();
    match op {
        "start" => {
            let workflow = args["workflow"]
                .as_str()
                .unwrap_or("role_limits")
                .to_owned();
            if workflow != "role_limits" {
                return Err(format!("ux_wizard: unknown workflow '{workflow}'"));
            }
            let mut g = wizards()
                .lock()
                .map_err(|e| format!("wizard lock poisoned: {e}"))?;
            g.insert(
                p.clone(),
                WizardState {
                    workflow,
                    ..WizardState::default()
                },
            );
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": "start",
                "principal": p,
                "question": question_for(g.get(&p).expect("wizard must exist"))?
            })))
        }
        "answer" => {
            let question_id = args["question_id"]
                .as_str()
                .ok_or("ux_wizard answer: 'question_id' is required")?;
            let value = &args["value"];
            let mut g = wizards()
                .lock()
                .map_err(|e| format!("wizard lock poisoned: {e}"))?;
            let st = g
                .get_mut(&p)
                .ok_or("ux_wizard: no active wizard; call start first")?;
            match question_id {
                "role" => {
                    let role = value.as_str().ok_or("ux_wizard: role must be string")?;
                    if !matches!(role, "observer" | "operator" | "admin") {
                        return Err("ux_wizard: role must be observer/operator/admin".to_owned());
                    }
                    st.role = Some(role.to_owned());
                }
                "max_total_alloc_bytes" => {
                    let v = value
                        .as_u64()
                        .ok_or("ux_wizard: max_total_alloc_bytes must be integer")?;
                    st.max_total_alloc_bytes = Some(v);
                }
                "max_concurrent_jobs" => {
                    let v = value
                        .as_u64()
                        .ok_or("ux_wizard: max_concurrent_jobs must be integer")?;
                    st.max_concurrent_jobs = Some(v);
                }
                _ => return Err(format!("ux_wizard: unknown question_id '{question_id}'")),
            }

            let next = question_for(st)?;
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": "answer",
                "principal": p,
                "answers": answers_json(st),
                "question": next
            })))
        }
        "status" => {
            let g = wizards()
                .lock()
                .map_err(|e| format!("wizard lock poisoned: {e}"))?;
            let st = g.get(&p).ok_or("ux_wizard: no active wizard")?;
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": "status",
                "principal": p,
                "answers": answers_json(st),
                "question": question_for(st)?
            })))
        }
        "apply" => {
            let g = wizards()
                .lock()
                .map_err(|e| format!("wizard lock poisoned: {e}"))?;
            let st = g.get(&p).ok_or("ux_wizard: no active wizard")?;
            let role = st.role.clone().ok_or("ux_wizard: role not answered")?;
            let max_bytes = st
                .max_total_alloc_bytes
                .ok_or("ux_wizard: max_total_alloc_bytes not answered")?;
            let max_jobs = st
                .max_concurrent_jobs
                .ok_or("ux_wizard: max_concurrent_jobs not answered")?;
            drop(g);

            apply_role_limits(&role, max_bytes, max_jobs)?;
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": "apply",
                "principal": p,
                "role": role,
                "written_policy": policy_path().display().to_string(),
                "limits": {
                    "max_total_alloc_bytes": max_bytes,
                    "max_concurrent_jobs": max_jobs
                }
            })))
        }
        "reset" => {
            let mut g = wizards()
                .lock()
                .map_err(|e| format!("wizard lock poisoned: {e}"))?;
            let removed = g.remove(&p).is_some();
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": "reset",
                "principal": p,
                "removed": removed
            })))
        }
        _ => Err(format!("ux_wizard: unknown op '{op}'")),
    }
}

fn answers_json(st: &WizardState) -> Value {
    json!({
        "workflow": st.workflow,
        "role": st.role,
        "max_total_alloc_bytes": st.max_total_alloc_bytes,
        "max_concurrent_jobs": st.max_concurrent_jobs
    })
}

fn question_for(st: &WizardState) -> Result<Value, String> {
    if st.role.is_none() {
        return Ok(json!({
            "id": "role",
            "prompt": "Choose role to configure",
            "type": "enum",
            "options": ["observer", "operator", "admin"]
        }));
    }
    if st.max_total_alloc_bytes.is_none() {
        return Ok(json!({
            "id": "max_total_alloc_bytes",
            "prompt": "Set max total allocated bytes per session for this role",
            "type": "u64"
        }));
    }
    if st.max_concurrent_jobs.is_none() {
        return Ok(json!({
            "id": "max_concurrent_jobs",
            "prompt": "Set max concurrent jobs per session for this role",
            "type": "u64"
        }));
    }
    Ok(json!({
        "id": null,
        "prompt": "All questions answered. Call op=apply to write policy.",
        "type": "done"
    }))
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

fn apply_role_limits(role: &str, max_bytes: u64, max_jobs: u64) -> Result<(), String> {
    let path = policy_path();
    let mut root: toml::Value = if path.exists() {
        let txt =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        toml::from_str(&txt).map_err(|e| format!("parse {}: {e}", path.display()))?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };

    // Ensure [roles.<role>.limits]
    let table = root
        .as_table_mut()
        .ok_or("policy root must be a TOML table")?;
    if !table.contains_key("roles") {
        table.insert(
            "roles".to_owned(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }
    let roles = table
        .get_mut("roles")
        .and_then(|v| v.as_table_mut())
        .ok_or("policy roles must be table")?;
    if !roles.contains_key(role) {
        roles.insert(role.to_owned(), toml::Value::Table(toml::map::Map::new()));
    }
    let role_tbl = roles
        .get_mut(role)
        .and_then(|v| v.as_table_mut())
        .ok_or("policy role must be table")?;
    if !role_tbl.contains_key("limits") {
        role_tbl.insert(
            "limits".to_owned(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }
    let limits = role_tbl
        .get_mut("limits")
        .and_then(|v| v.as_table_mut())
        .ok_or("policy limits must be table")?;

    limits.insert(
        "max_total_alloc_bytes".to_owned(),
        toml::Value::Integer(max_bytes as i64),
    );
    limits.insert(
        "max_concurrent_jobs".to_owned(),
        toml::Value::Integer(max_jobs as i64),
    );

    // Ensure allow/deny keys exist minimally for role sanity.
    role_tbl.entry("allow".to_owned()).or_insert_with(|| {
        toml::Value::Array(vec![])
    });
    role_tbl
        .entry("deny".to_owned())
        .or_insert_with(|| toml::Value::Array(vec![]));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let out = toml::to_string_pretty(&root).map_err(|e| format!("serialize policy: {e}"))?;
    std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}
