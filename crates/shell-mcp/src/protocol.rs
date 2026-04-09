//! MCP JSON-RPC 2.0 wire types.
//!
//! Reference: https://modelcontextprotocol.io/specification

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Inbound ──────────────────────────────────────────────────────────────────

/// An incoming JSON-RPC message (request or notification).
#[derive(Debug, Deserialize)]
pub struct InboundMessage {
    pub jsonrpc: String,
    /// Absent on notifications.
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

impl InboundMessage {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

// ── Outbound ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

// ── Tool schema types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<ContentItem>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem {
                content_type: "text",
                text: s.into(),
            }],
            is_error: false,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem {
                content_type: "text",
                text: s.into(),
            }],
            is_error: true,
        }
    }

    pub fn json(v: &impl Serialize) -> Self {
        Self::text(serde_json::to_string_pretty(v).unwrap_or_else(|e| e.to_string()))
    }
}

#[derive(Debug, Serialize)]
pub struct ContentItem {
    #[serde(rename = "type")]
    pub content_type: &'static str,
    pub text: String,
}

// ── Standard RPC error codes ──────────────────────────────────────────────────

pub const ERR_PARSE: i32 = -32700;
pub const ERR_METHOD: i32 = -32601;
pub const ERR_INVALID: i32 = -32600;
pub const ERR_INTERNAL: i32 = -32603;
