//! Audit seam (types only): what gets recorded when a command decision is
//! made. The sink trait is the hook; storage engines (JSONL, SQLite, the
//! `orch` review queue) plug in later without touching the session code.

use serde::{Deserialize, Serialize};

use crate::policy::PolicyDecision;

/// One auditable command decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub command_line: String,
    pub decision: PolicyDecision,
}

/// Receives audit records. The session host calls this whenever a command
/// was classified; implementations decide persistence and fanout.
pub trait AuditSink: Send + Sync {
    fn record(&self, record: AuditRecord);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CommandPolicy, PolicyDecision};
    use std::sync::Mutex;

    struct Recording(Mutex<Vec<AuditRecord>>);

    impl AuditSink for Recording {
        fn record(&self, record: AuditRecord) {
            self.0.lock().unwrap().push(record);
        }
    }

    #[test]
    fn sink_trait_object_receives_records() {
        let sink = Recording(Mutex::new(Vec::new()));
        let dyn_sink: &dyn AuditSink = &sink;
        dyn_sink.record(AuditRecord {
            command_line: "cargo test --workspace".into(),
            decision: PolicyDecision {
                policy: CommandPolicy::Allow,
                reason: None,
            },
        });
        dyn_sink.record(AuditRecord {
            command_line: "rm -rf /".into(),
            decision: PolicyDecision {
                policy: CommandPolicy::Deny,
                reason: Some("destructive".into()),
            },
        });
        let got = sink.0.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].decision.policy, CommandPolicy::Deny);
    }

    #[test]
    fn record_serde_round_trip() {
        let r = AuditRecord {
            command_line: "git status".into(),
            decision: PolicyDecision {
                policy: CommandPolicy::Confirm,
                reason: Some("first run".into()),
            },
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<AuditRecord>(&json).unwrap(), r);
    }
}
