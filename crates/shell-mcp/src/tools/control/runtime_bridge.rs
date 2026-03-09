use serde_json::Value;

use super::schema::ReconcileAction;

pub trait RuntimeApplyAdapter {
    fn apply(action: &ReconcileAction) -> Result<Option<Value>, String>;
}

pub struct NoopRuntimeApplyAdapter;

impl RuntimeApplyAdapter for NoopRuntimeApplyAdapter {
    fn apply(_action: &ReconcileAction) -> Result<Option<Value>, String> {
        Ok(None)
    }
}

pub fn apply_runtime_action(action: &ReconcileAction) -> Result<Option<Value>, String> {
    NoopRuntimeApplyAdapter::apply(action)
}
