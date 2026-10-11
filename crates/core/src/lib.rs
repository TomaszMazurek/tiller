//! Tiller core: the workspace model shared by every client of the IDE.
//!
//! The core knows nothing about Tauri, the UI, or any transport. Cockpit talks
//! to it through `tiller-rpc`, agents through MCP adapters — both built on the
//! tool seam ([`Tool`]) and the event vocabulary ([`Event`]) defined here.

mod event;
mod tool;

pub use event::{Event, FileChanged, GateProgress, GateStatus, ProcessState, ProcessStateChanged};
pub use tool::{Tool, ToolSpec};

/// Version of the core workspace, surfaced to clients (e.g. `core.version`).
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
