//! Command classification seam (types only, per core §3: `term` = PTY +
//! classification + audit).
//!
//! The boundary between "run" and "ask first" belongs to the terminal seam
//! because the PTY is where agent-issued commands surface. The engine
//! (allowlist, pattern rules, per-project policy) is a later slice; what is
//! fixed here is the decision vocabulary every engine must speak.

use serde::{Deserialize, Serialize};

/// What the host may do with a command line before it reaches the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandPolicy {
    /// Run without further confirmation.
    Allow,
    /// Requires an explicit human confirmation before running.
    Confirm,
    /// Never run.
    Deny,
}

/// One classification verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub policy: CommandPolicy,
    /// Why the policy applies (surfaced to the agent and the audit trail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Classifies a command line into a [`PolicyDecision`].
///
/// `Send + Sync` is load-bearing: classifiers sit behind trait objects
/// shared by the session host, the cockpit, and MCP adapters.
pub trait CommandClassifier: Send + Sync {
    fn classify(&self, command_line: &str) -> PolicyDecision;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_serde_uses_snake_case_tags() {
        assert_eq!(
            serde_json::to_string(&CommandPolicy::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&CommandPolicy::Confirm).unwrap(),
            "\"confirm\""
        );
        assert_eq!(
            serde_json::to_string(&CommandPolicy::Deny).unwrap(),
            "\"deny\""
        );
    }

    #[test]
    fn decision_serde_round_trip_and_optional_reason() {
        let d = PolicyDecision {
            policy: CommandPolicy::Deny,
            reason: Some("matches deny rule".into()),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<PolicyDecision>(&json).unwrap(), d);
        let bare = PolicyDecision {
            policy: CommandPolicy::Allow,
            reason: None,
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert!(
            !json.contains("reason"),
            "reason omitted when absent: {json}"
        );
        assert_eq!(serde_json::from_str::<PolicyDecision>(&json).unwrap(), bare);
    }
}
