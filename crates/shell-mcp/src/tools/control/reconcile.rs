use serde_json::json;

use super::schema::{ActualState, DesiredStateV0, ReconcileAction};

pub fn build_reconcile_plan(
    desired: &DesiredStateV0,
    _actual: &ActualState,
) -> Vec<ReconcileAction> {
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

    actions
}
