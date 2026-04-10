//! Dynamic tool registry — register shell-backed tools at runtime without restarting.
//!
//! Tools are persisted to ~/.local/share/ferrite/dynamic_tools.json and reloaded on startup.
//! Each tool's command is a shell string with `{param}` placeholders; args are also passed
//! as JSON in the FERRITE_ARGS env var and on stdin for scripts that prefer that.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::protocol::ToolResult;

// ── DynamicTool ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicTool {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the tool's parameters.
    pub params: Value,
    /// Shell command template. Use `{param_name}` placeholders for substitution.
    /// Args are also available as JSON in $FERRITE_ARGS and on stdin.
    pub command: String,
    /// Optional timeout in seconds (default: 60).
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    60
}

impl DynamicTool {
    /// Serialize to the JSON shape MCP clients expect in a tools/list response.
    pub fn to_value(&self) -> Value {
        json!({
            "name":        self.name,
            "description": self.description,
            "inputSchema": self.params,
        })
    }

    /// Expand `{param}` placeholders in the command string.
    /// Values are single-quoted for shell safety.
    fn expand_command(&self, args: &Value) -> String {
        let mut cmd = self.command.clone();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let placeholder = format!("{{{k}}}");
                let raw = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                // Single-quote escape: replace ' with '\''
                let safe = raw.replace('\'', "'\\''");
                let quoted = format!("'{safe}'");
                cmd = cmd.replace(&placeholder, &quoted);
            }
        }
        cmd
    }

    /// Execute the tool and return a ToolResult.
    pub fn exec(&self, args: &Value) -> Result<ToolResult, String> {
        let cmd = self.expand_command(args);
        let args_json = args.to_string();

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env("FERRITE_ARGS", &args_json)
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;

        // Write args JSON to stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(args_json.as_bytes()).ok();
        }

        // Wait with timeout via a thread
        let timeout = std::time::Duration::from_secs(self.timeout_secs);
        let start = std::time::Instant::now();
        loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(status) => {
                    let out = child.wait_with_output().map_err(|e| e.to_string())?;
                    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                    return if status.success() {
                        let text = if stdout.trim().is_empty() && !stderr.trim().is_empty() {
                            stderr
                        } else {
                            stdout
                        };
                        Ok(ToolResult::text(text))
                    } else {
                        let code = status.code().unwrap_or(-1);
                        let msg = if stderr.trim().is_empty() {
                            &stdout
                        } else {
                            &stderr
                        };
                        Err(format!("exit {code}: {msg}"))
                    };
                }
                None => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        return Err(format!("timed out after {}s", self.timeout_secs));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
}

// ── DynamicRegistry ───────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct DynamicRegistry {
    pub tools: HashMap<String, DynamicTool>,
    path: PathBuf,
}

impl DynamicRegistry {
    /// Load from disk, falling back to empty registry if missing or corrupt.
    pub fn load() -> Self {
        let path = registry_path();
        let tools = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<HashMap<String, DynamicTool>>(&s).ok())
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        Self { tools, path }
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(&self.tools).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, json).map_err(|e| e.to_string())
    }

    pub fn register(&mut self, tool: DynamicTool) -> Result<(), String> {
        self.tools.insert(tool.name.clone(), tool);
        self.save()
    }

    pub fn unregister(&mut self, name: &str) -> Result<bool, String> {
        let removed = self.tools.remove(name).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }
}

fn registry_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".local/share/ferrite/dynamic_tools.json")
}
