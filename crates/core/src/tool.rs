//! The tool seam: the common denominator every integration (UI via rpc,
//! agents via MCP, a potential future in-house engine) programs against.

use serde::{Deserialize, Serialize};

/// Human-readable identity of a tool exposed by the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
}

/// A capability the core exposes to its clients.
///
/// `Send + Sync` is load-bearing: tools live behind trait objects shared
/// across threads (Tauri IPC, MCP servers, worker pools).
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
}
