//! Event vocabulary published by the core.
//!
//! Events are the read model for cockpit panels and agent adapters; the wire
//! format is JSON with an internal `type` tag so new variants stay additive.

use serde::{Deserialize, Serialize};

/// Something happened in the core that clients may want to observe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A tracked process (agent, gate, tool) changed its lifecycle state.
    ProcessStateChanged(ProcessStateChanged),
    /// A file in the workspace changed on disk.
    FileChanged(FileChanged),
    /// A verification gate (e.g. `cargo test`) progressed.
    GateProgress(GateProgress),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessStateChanged {
    pub process_id: u64,
    pub state: ProcessState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Exited,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileChanged {
    /// Workspace-relative path, kept as a plain string for transport-neutral JSON.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateProgress {
    pub gate: String,
    pub status: GateStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pending,
    Running,
    Passed,
    Failed,
}
