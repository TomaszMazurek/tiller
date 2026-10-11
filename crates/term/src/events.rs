//! Session lifecycle in the core event vocabulary.
//!
//! P4 will publish these on the event bus; nothing here needs new core
//! variants. Mapping note: the core's [`tiller_core::ProcessState`] has no
//! "killed" state — a killed program reports a non-zero exit status and maps
//! to `Failed`. Revisiting that vocabulary is a core decision, not a term
//! one.

use portable_pty::ExitStatus;
use tiller_core::{Event, ProcessState, ProcessStateChanged};

/// Map an exit status to the core process-state vocabulary.
pub fn process_state(status: &ExitStatus) -> ProcessState {
    if status.success() {
        ProcessState::Exited
    } else {
        ProcessState::Failed
    }
}

/// Build the core event for a session that exited with `status`.
pub fn exit_event(process_id: u64, status: &ExitStatus) -> Event {
    Event::ProcessStateChanged(ProcessStateChanged {
        process_id,
        state: process_state(status),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_maps_to_exited_failure_to_failed() {
        assert_eq!(
            process_state(&ExitStatus::with_exit_code(0)),
            ProcessState::Exited
        );
        assert_eq!(
            process_state(&ExitStatus::with_exit_code(7)),
            ProcessState::Failed
        );
        assert_eq!(
            process_state(&ExitStatus::with_signal("SIGKILL")),
            ProcessState::Failed
        );
    }

    #[test]
    fn exit_event_is_wire_compatible_with_core_events() {
        let ev = exit_event(42, &ExitStatus::with_exit_code(0));
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
        assert!(json.contains("\"process_id\":42"));
        assert!(json.contains("process_state_changed"));
        assert!(json.contains("\"state\":\"exited\""));
    }
}
