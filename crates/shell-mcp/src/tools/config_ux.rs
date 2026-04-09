//! config_ux — MCP-facing config list/get/set with consistent JSON responses.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::{config_path, FerriteConfig};
use crate::protocol::ToolResult;

pub fn config_ux(args: &Value) -> Result<ToolResult, String> {
    let op = args["op"].as_str().unwrap_or("list");
    match op {
        "list" => {
            let cfg = FerriteConfig::load();
            let mut map = serde_json::Map::new();
            for (k, v) in cfg.list() {
                map.insert(k, Value::String(v));
            }
            map.insert(
                "authz.policy_path".to_owned(),
                Value::String(authz_policy_path().display().to_string()),
            );
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": "list",
                "config_path": config_path().display().to_string(),
                "values": map
            })))
        }
        "get" => {
            let key = args["key"]
                .as_str()
                .ok_or("config_ux get: 'key' is required")?;
            if key == "authz.policy_path" {
                return Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": "get",
                    "key": key,
                    "value": authz_policy_path().display().to_string()
                })));
            }
            let cfg = FerriteConfig::load();
            match cfg.get(key) {
                Some(v) => Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": "get",
                    "key": key,
                    "value": v
                }))),
                None => Ok(ToolResult::json(&json!({
                    "ok": false,
                    "op": "get",
                    "key": key,
                    "error": "unknown key"
                }))),
            }
        }
        "set" => {
            let key = args["key"]
                .as_str()
                .ok_or("config_ux set: 'key' is required")?;
            let value = args["value"]
                .as_str()
                .ok_or("config_ux set: 'value' must be string")?;
            if key == "authz.policy_path" {
                return Ok(ToolResult::json(&json!({
                    "ok": false,
                    "op": "set",
                    "key": key,
                    "error": "authz.policy_path is derived from env and cannot be set here"
                })));
            }
            let mut cfg = FerriteConfig::load();
            cfg.set(key, value)?;
            cfg.save()?;
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": "set",
                "key": key,
                "value": value,
                "config_path": config_path().display().to_string()
            })))
        }
        _ => Err(format!("config_ux: unknown op '{op}'")),
    }
}

fn authz_policy_path() -> PathBuf {
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
