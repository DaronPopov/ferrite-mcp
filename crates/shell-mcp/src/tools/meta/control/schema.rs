use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredStateV0 {
    pub version: String,
    #[serde(default)]
    pub build: Option<BuildGoal>,
}

impl DesiredStateV0 {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != "v0" {
            return Err("desired state version must be 'v0'".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildGoal {
    pub cwd: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActualState {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileAction {
    pub id: String,
    pub action_type: String,
    pub status: String,
    pub reason: String,
    pub suggested_tool: String,
    pub suggested_arguments: serde_json::Value,
}
