use tiller_core::{
    Event, GateProgress, GateStatus, ProcessState, ProcessStateChanged, Tool, ToolSpec,
    CORE_VERSION,
};

struct ProbeTool {
    name: &'static str,
    description: &'static str,
}

impl Tool for ProbeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_string(),
            description: self.description.to_string(),
        }
    }
}

#[test]
fn tools_compose_as_trait_objects_and_expose_specs() {
    // The tool seam must work behind `dyn Tool` (Vec, shared across threads),
    // because UI (via rpc) and agents (via MCP) only ever see trait objects.
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ProbeTool {
            name: "fs.read",
            description: "Read a file from the workspace",
        }),
        Box::new(ProbeTool {
            name: "term.run",
            description: "Run a classified command",
        }),
    ];

    let specs: Vec<ToolSpec> = tools.iter().map(|tool| tool.spec()).collect();
    assert_eq!(specs[0].name, "fs.read");
    assert_eq!(specs[0].description, "Read a file from the workspace");
    assert_eq!(specs[1].name, "term.run");
}

#[test]
fn tool_spec_is_serializable() {
    let spec = ToolSpec {
        name: "fs.read".to_string(),
        description: "Read a file".to_string(),
    };
    let json = serde_json::to_string(&spec).expect("ToolSpec must serialize");
    let back: ToolSpec = serde_json::from_str(&json).expect("ToolSpec must deserialize");
    assert_eq!(back, spec);
}

#[test]
fn core_version_is_exposed() {
    assert!(
        !CORE_VERSION.is_empty(),
        "CORE_VERSION must be set from the package version"
    );
}

#[test]
fn event_types_are_pub_usable_from_outside() {
    // Compile-level check that the event vocabulary is part of the public API.
    let events = [
        Event::ProcessStateChanged(ProcessStateChanged {
            process_id: 1,
            state: ProcessState::Running,
        }),
        Event::GateProgress(GateProgress {
            gate: "g".to_string(),
            status: GateStatus::Running,
        }),
    ];
    assert_eq!(events.len(), 2);
}
