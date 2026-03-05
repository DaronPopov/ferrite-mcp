use serde_json::Value;

use super::schema::ReconcileAction;

pub trait RuntimeApplyAdapter {
    fn apply(action: &ReconcileAction) -> Result<Option<Value>, String>;
}

#[cfg(not(feature = "fercuda-runtime-apply"))]
pub struct NoopRuntimeApplyAdapter;

#[cfg(not(feature = "fercuda-runtime-apply"))]
impl RuntimeApplyAdapter for NoopRuntimeApplyAdapter {
    fn apply(_action: &ReconcileAction) -> Result<Option<Value>, String> {
        Ok(None)
    }
}

#[cfg(feature = "fercuda-runtime-apply")]
pub struct FercudaRuntimeApplyAdapter;

#[cfg(feature = "fercuda-runtime-apply")]
impl RuntimeApplyAdapter for FercudaRuntimeApplyAdapter {
    fn apply(action: &ReconcileAction) -> Result<Option<Value>, String> {
        if action.suggested_tool != "fercuda_runtime" {
            return Ok(None);
        }
        let tr = crate::tools::fercuda::runtime(&action.suggested_arguments)?;
        let txt = tr
            .content
            .first()
            .map(|c| c.text.as_str())
            .ok_or("empty fercuda tool result".to_owned())?;
        let v = serde_json::from_str::<Value>(txt)
            .map_err(|e| format!("failed to parse fercuda tool result: {e}"))?;
        Ok(Some(v))
    }
}

pub fn apply_runtime_action(action: &ReconcileAction) -> Result<Option<Value>, String> {
    #[cfg(feature = "fercuda-runtime-apply")]
    {
        return FercudaRuntimeApplyAdapter::apply(action);
    }
    #[cfg(not(feature = "fercuda-runtime-apply"))]
    {
        NoopRuntimeApplyAdapter::apply(action)
    }
}
