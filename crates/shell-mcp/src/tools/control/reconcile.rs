use serde_json::json;

use super::schema::{ActualState, DesiredStateV0, ReconcileAction};

pub fn build_reconcile_plan(desired: &DesiredStateV0, actual: &ActualState) -> Vec<ReconcileAction> {
    let mut actions = Vec::new();

    if let Some(build) = &desired.build {
        actions.push(ReconcileAction {
            id: "build.ensure".to_owned(),
            action_type: "build".to_owned(),
            status: "needed".to_owned(),
            reason: "build goal requested; execution policy is manual-apply for now".to_owned(),
            suggested_tool: "exec".to_owned(),
            suggested_arguments: json!({
                "cmd": build.command,
                "cwd": build.cwd
            }),
        });
    }

    if let Some(fercuda) = &desired.fercuda {
        let owned = actual.fercuda_owned_sessions.len() as u64;
        let create_cfg = fercuda.session_create.clone();
        if owned < fercuda.min_sessions {
            let to_add = fercuda.min_sessions - owned;
            for idx in 0..to_add {
                let (device, mutable_bytes, immutable_bytes) = if let Some(cfg) = &create_cfg {
                    (cfg.device, cfg.mutable_bytes, cfg.immutable_bytes)
                } else {
                    (0, 512u64 << 20, 2u64 << 30)
                };
                actions.push(ReconcileAction {
                    id: format!("fercuda.session_create.{}", idx + 1),
                    action_type: "fercuda_session_create".to_owned(),
                    status: "needed".to_owned(),
                    reason: format!("owned sessions={} below min_sessions={}", owned, fercuda.min_sessions),
                    suggested_tool: "fercuda_runtime".to_owned(),
                    suggested_arguments: json!({
                        "op": "session_create",
                        "device": device,
                        "mutable_bytes": mutable_bytes,
                        "immutable_bytes": immutable_bytes
                    }),
                });
            }
        } else if owned > fercuda.max_sessions {
            let to_remove = owned - fercuda.max_sessions;
            for (idx, sid) in actual.fercuda_owned_sessions.iter().take(to_remove as usize).enumerate() {
                actions.push(ReconcileAction {
                    id: format!("fercuda.session_destroy.{}", idx + 1),
                    action_type: "fercuda_session_destroy".to_owned(),
                    status: "needed".to_owned(),
                    reason: format!("owned sessions={} above max_sessions={}", owned, fercuda.max_sessions),
                    suggested_tool: "fercuda_runtime".to_owned(),
                    suggested_arguments: json!({
                        "op": "session_destroy",
                        "session_id": sid
                    }),
                });
            }
        } else {
            actions.push(ReconcileAction {
                id: "fercuda.sessions.in_range".to_owned(),
                action_type: "fercuda_session".to_owned(),
                status: "noop".to_owned(),
                reason: format!(
                    "owned sessions={} in target range [{}..={}]",
                    owned, fercuda.min_sessions, fercuda.max_sessions
                ),
                suggested_tool: "fercuda_runtime".to_owned(),
                suggested_arguments: json!({ "op": "status" }),
            });
        }
    }

    actions
}

