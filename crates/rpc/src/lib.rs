//! Narrow JSON-RPC protocol between the cockpit UI and the Tiller core.
//!
//! This is deliberately our own minimal protocol, not a general-purpose
//! JSON-RPC framework: a fixed request/response/notification shape, a small
//! error-code set, and one dispatcher trait. The transport is swappable
//! (Tauri IPC today, WebSocket for the daemon profile later).

use serde::{Deserialize, Serialize};
use tiller_core::CORE_VERSION;

/// Standard-ish error codes for this protocol.
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// Request identifier: numeric or string, opaque to the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(u64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: RequestId,
    pub method: String,
    /// Missing `params` on the wire deserializes to `Value::Null`.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A message from the core that carries no id and expects no reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Exactly one of `result` / `error` is present on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: RequestId, error: RpcError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// Routes an incoming request to a handler.
pub trait Dispatcher: Send + Sync {
    fn dispatch(&self, req: RpcRequest) -> Result<RpcResponse, RpcError>;
}

/// Minimal dispatcher exposing core introspection methods.
///
/// Stateful core surfaces (sessions, worktrees, tools) plug in as additional
/// methods in later slices; the protocol itself should stay narrow.
pub struct CoreDispatcher;

impl Dispatcher for CoreDispatcher {
    fn dispatch(&self, req: RpcRequest) -> Result<RpcResponse, RpcError> {
        if !req.params.is_object() && !req.params.is_null() {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!("params for `{}` must be an object or null", req.method),
            ));
        }
        match req.method.as_str() {
            "core.version" => Ok(RpcResponse::ok(
                req.id,
                serde_json::json!({ "version": CORE_VERSION }),
            )),
            method => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("unknown method: {method}"),
            )),
        }
    }
}
