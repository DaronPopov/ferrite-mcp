use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::execution;

mod reconcile;
mod runtime_bridge;
mod schema;

use reconcile::build_reconcile_plan;
use runtime_bridge::apply_runtime_action;
use schema::{ActualState, DesiredStateV0, ReconcileAction};

#[derive(Debug, Default)]
struct ControlStore {
    desired: Option<DesiredStateV0>,
    last_tick_actions: Vec<ReconcileAction>,
}

static STORE: OnceLock<Mutex<ControlStore>> = OnceLock::new();

fn store() -> &'static Mutex<ControlStore> {
    STORE.get_or_init(|| Mutex::new(ControlStore::default()))
}

fn collect_actual_state(args: &Value, desired: &DesiredStateV0) -> Result<ActualState, String> {
    if !args["actual"].is_null() {
        return serde_json::from_value(args["actual"].clone())
            .map_err(|e| format!("invalid actual schema: {e}"));
    }
    let actual = ActualState {
        fercuda_owned_sessions: Vec::new(),
    };
    if desired.fercuda.is_some() {
        // Standalone install: no feRcuda runtime probe from this crate.
        // Caller can pass `actual.fercuda_owned_sessions` when available.
    }
    Ok(actual)
}

fn apply_actions(
    actions: &[ReconcileAction],
    state: &Arc<Mutex<ServerState>>,
    enable_apply_runtime: bool,
) -> Result<Vec<Value>, String> {
    let mut applied: Vec<Value> = Vec::new();
    for a in actions {
        if a.status != "needed" {
            continue;
        }
        if !enable_apply_runtime {
            applied.push(json!({
                "action_id": a.id,
                "applied": false,
                "reason": "enable_apply_runtime=false",
                "suggested_tool": a.suggested_tool,
                "suggested_arguments": a.suggested_arguments
            }));
            continue;
        }
        if a.suggested_tool == "exec" {
            let tr = execution::exec_cmd(&a.suggested_arguments, state)?;
            let result = serde_json::from_str::<Value>(
                tr.content.first().map(|c| c.text.as_str()).unwrap_or("{}")
            ).unwrap_or_else(|_| json!({"raw": tr.content.first().map(|c| c.text.clone()).unwrap_or_default()}));
            applied.push(json!({
                "action_id": a.id,
                "applied": true,
                "result": result
            }));
            continue;
        }
        if let Some(v) = apply_runtime_action(a)? {
            applied.push(json!({
                "action_id": a.id,
                "applied": true,
                "result": v
            }));
            continue;
        }
        applied.push(json!({
            "action_id": a.id,
            "applied": false,
            "reason": "apply bridge not available for this tool in current module",
            "suggested_tool": a.suggested_tool,
            "suggested_arguments": a.suggested_arguments
        }));
    }
    Ok(applied)
}

pub fn control_reconcile(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let op = args["op"].as_str().unwrap_or("status");
    match op {
        "set_desired" => {
            let desired_v = args["desired"]
                .clone();
            if desired_v.is_null() {
                return Err("control_reconcile: 'desired' is required for set_desired".to_owned());
            }
            let desired: DesiredStateV0 = serde_json::from_value(desired_v)
                .map_err(|e| format!("invalid desired schema: {e}"))?;
            desired.validate()?;
            let mut guard = store().lock().map_err(|e| format!("control lock poisoned: {e}"))?;
            guard.desired = Some(desired.clone());
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": op,
                "desired": desired
            })))
        }
        "get_desired" => {
            let guard = store().lock().map_err(|e| format!("control lock poisoned: {e}"))?;
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": op,
                "desired": guard.desired
            })))
        }
        "clear" => {
            let mut guard = store().lock().map_err(|e| format!("control lock poisoned: {e}"))?;
            guard.desired = None;
            guard.last_tick_actions.clear();
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": op,
                "cleared": true
            })))
        }
        "tick" => {
            let apply = args["apply"].as_bool().unwrap_or(false);
            let enable_apply_runtime = args["enable_apply_runtime"].as_bool().unwrap_or(false);
            let desired = if args["desired"].is_null() {
                let guard = store().lock().map_err(|e| format!("control lock poisoned: {e}"))?;
                guard.desired.clone().ok_or("no desired state configured".to_owned())?
            } else {
                let d: DesiredStateV0 = serde_json::from_value(args["desired"].clone())
                    .map_err(|e| format!("invalid desired schema: {e}"))?;
                d.validate()?;
                d
            };
            let actual = collect_actual_state(args, &desired)?;
            let actions = build_reconcile_plan(&desired, &actual);
            let applied = if apply { apply_actions(&actions, state, enable_apply_runtime)? } else { Vec::new() };

            {
                let mut guard = store().lock().map_err(|e| format!("control lock poisoned: {e}"))?;
                guard.desired = Some(desired.clone());
                guard.last_tick_actions = actions.clone();
            }

            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": op,
                "apply": apply,
                "enable_apply_runtime": enable_apply_runtime,
                "desired": desired,
                "actual": actual,
                "plan": actions,
                "applied": applied
            })))
        }
        "status" => {
            let guard = store().lock().map_err(|e| format!("control lock poisoned: {e}"))?;
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": op,
                "has_desired": guard.desired.is_some(),
                "last_tick_action_count": guard.last_tick_actions.len(),
                "last_tick_actions": guard.last_tick_actions
            })))
        }
        _ => Err(format!("control_reconcile: unknown op '{op}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> Arc<Mutex<ServerState>> {
        Arc::new(Mutex::new(ServerState::default()))
    }

    fn parse_tool_json(tr: &ToolResult) -> Value {
        let txt = tr.content.first().map(|c| c.text.as_str()).unwrap_or("{}");
        serde_json::from_str::<Value>(txt).unwrap_or_else(|_| json!({}))
    }

    #[test]
    fn set_and_get_desired_roundtrip() {
        let desired = json!({
            "version": "v0",
            "fercuda": {
                "min_sessions": 1,
                "max_sessions": 2
            }
        });
        let st = state();
        let set = control_reconcile(&json!({
            "op": "set_desired",
            "desired": desired
        }), &st).expect("set_desired");
        let set_v = parse_tool_json(&set);
        assert_eq!(set_v["ok"], json!(true));

        let get = control_reconcile(&json!({
            "op": "get_desired"
        }), &st).expect("get_desired");
        let get_v = parse_tool_json(&get);
        assert_eq!(get_v["desired"]["version"], json!("v0"));
        assert_eq!(get_v["desired"]["fercuda"]["min_sessions"], json!(1));
    }

    #[test]
    fn tick_build_plan_deferred_when_runtime_apply_disabled() {
        let st = state();
        let tr = control_reconcile(&json!({
            "op": "tick",
            "apply": true,
            "enable_apply_runtime": false,
            "desired": {
                "version": "v0",
                "build": {
                    "cwd": ".",
                    "command": "echo hello"
                }
            }
        }), &st).expect("tick");
        let v = parse_tool_json(&tr);
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["apply"], json!(true));
        assert_eq!(v["enable_apply_runtime"], json!(false));
        assert!(v["plan"].as_array().map(|a| !a.is_empty()).unwrap_or(false));
        let applied = v["applied"].as_array().cloned().unwrap_or_default();
        assert!(!applied.is_empty());
        assert_eq!(applied[0]["applied"], json!(false));
        assert_eq!(applied[0]["reason"], json!("enable_apply_runtime=false"));
    }

    #[test]
    fn tick_fercuda_range_noop_with_explicit_actual() {
        let st = state();
        let tr = control_reconcile(&json!({
            "op": "tick",
            "desired": {
                "version": "v0",
                "fercuda": {
                    "min_sessions": 1,
                    "max_sessions": 2
                }
            },
            "actual": {
                "fercuda_owned_sessions": [42]
            }
        }), &st).expect("tick");
        let v = parse_tool_json(&tr);
        let plan = v["plan"].as_array().cloned().unwrap_or_default();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0]["status"], json!("noop"));
    }
}
