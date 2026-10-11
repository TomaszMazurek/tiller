use tiller_core::{
    Event, FileChanged, GateProgress, GateStatus, ProcessState, ProcessStateChanged,
};

fn assert_roundtrip(event: Event, expected_tag: &str) {
    let serialized = serde_json::to_string(&event).expect("Event must serialize");
    assert!(
        serialized.contains(&format!("\"type\":\"{expected_tag}\"")),
        "expected tag `{expected_tag}` in {serialized}"
    );
    let deserialized: Event = serde_json::from_str(&serialized).expect("Event must deserialize");
    assert_eq!(deserialized, event);
}

#[test]
fn every_variant_roundtrips_through_json() {
    // Exhaustive check over all variants, so adding one without a tag/roundtrip
    // behavior that works fails here.
    let variants = [
        (
            Event::ProcessStateChanged(ProcessStateChanged {
                process_id: 7,
                state: ProcessState::Running,
            }),
            "process_state_changed",
        ),
        (
            Event::FileChanged(FileChanged {
                path: "crates/core/src/lib.rs".to_string(),
            }),
            "file_changed",
        ),
        (
            Event::GateProgress(GateProgress {
                gate: "cargo-test".to_string(),
                status: GateStatus::Passed,
            }),
            "gate_progress",
        ),
    ];
    assert_eq!(variants.len(), 3, "update this test when Event grows");
    for (event, tag) in variants {
        assert_roundtrip(event, tag);
    }
}

#[test]
fn process_state_serializes_snake_case() {
    let event = Event::ProcessStateChanged(ProcessStateChanged {
        process_id: 1,
        state: ProcessState::Failed,
    });
    let value = serde_json::to_value(&event).expect("serialization");
    assert_eq!(value["type"], "process_state_changed");
    assert_eq!(value["process_id"], 1);
    assert_eq!(value["state"], "failed");
}

#[test]
fn gate_status_serializes_snake_case() {
    let event = Event::GateProgress(GateProgress {
        gate: "cargo-check".to_string(),
        status: GateStatus::Pending,
    });
    let value = serde_json::to_value(&event).expect("serialization");
    assert_eq!(value["type"], "gate_progress");
    assert_eq!(value["gate"], "cargo-check");
    assert_eq!(value["status"], "pending");
}

#[test]
fn file_changed_carries_path() {
    let event = Event::FileChanged(FileChanged {
        path: "apps/cockpit/src/App.tsx".to_string(),
    });
    let value = serde_json::to_value(&event).expect("serialization");
    assert_eq!(value["type"], "file_changed");
    assert_eq!(value["path"], "apps/cockpit/src/App.tsx");
}
