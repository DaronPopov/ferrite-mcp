use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredStateV0 {
    pub version: String,
    #[serde(default)]
    pub build: Option<BuildGoal>,
    #[serde(default)]
    pub fercuda: Option<FercudaGoal>,
}

impl DesiredStateV0 {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != "v0" {
            return Err("desired state version must be 'v0'".to_owned());
        }
        if let Some(goal) = &self.fercuda {
            if goal.min_sessions > goal.max_sessions {
                return Err("fercuda.min_sessions cannot be greater than fercuda.max_sessions".to_owned());
            }
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
pub struct FercudaGoal {
    pub min_sessions: u64,
    pub max_sessions: u64,
    #[serde(default)]
    pub session_create: Option<FercudaSessionCreate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FercudaSessionCreate {
    #[serde(default = "default_device")]
    pub device: i64,
    #[serde(default = "default_mutable_bytes")]
    pub mutable_bytes: u64,
    #[serde(default = "default_immutable_bytes")]
    pub immutable_bytes: u64,
}

fn default_device() -> i64 { 0 }
fn default_mutable_bytes() -> u64 { 512u64 << 20 }
fn default_immutable_bytes() -> u64 { 2u64 << 30 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActualState {
    #[serde(default)]
    pub fercuda_owned_sessions: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileAction {
    pub id: String,
    pub action_type: String,
    pub status: String,
    pub reason: String,
    pub suggested_tool: String,
    pub suggested_arguments: serde_json::Value,
}

