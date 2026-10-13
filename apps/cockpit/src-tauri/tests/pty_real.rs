//! ConPTY integration tests for the cockpit term host (Windows only).
//!
//! The unit suites cover parsing, validation and the fake-session
//! lifecycle; these tests drive the *real* seam end to end: dispatcher →
//! SessionManager → PtySession → ConPTY, with output observed through the
//! notification sink exactly like the UI would. Mirrors
//! `crates/term/tests/conpty.rs`: every test bounds its runtime and kills
//! its child, so a failing assertion cannot leak processes.

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cockpit_lib::{
    CockpitDispatcher, EventSink, PtySessionFactory, SessionManager, SpawnSpec, TaskHost,
    SESSION_NOT_FOUND,
};
use tiller_rpc::{Dispatcher, RequestId, RpcRequest};

/// Sink that accumulates events plus the raw output bytes per session
/// (decoded once here so assertions read plain bytes).
struct RecordingSink {
    events: Mutex<Vec<(String, serde_json::Value)>>,
    output: Mutex<Vec<u8>>,
    dsr: AtomicU64,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            output: Mutex::new(Vec::new()),
            dsr: AtomicU64::new(0),
        })
    }

    /// Wait until `pred` matches a recorded event (panics with the log).
    fn wait_for(
        &self,
        timeout: Duration,
        pred: impl Fn(&str, &serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let events = self.events.lock().unwrap();
                if let Some((_, payload)) = events.iter().find(|(e, p)| pred(e, p)) {
                    return payload.clone();
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for event; recorded {:?}",
                    self.events.lock().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the accumulated output contains `needle`.
    fn wait_for_output(&self, needle: &[u8], timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self
                .output
                .lock()
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "timed out waiting for {:?}; output so far: {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&self.output.lock().unwrap())
        );
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        if event == "term.output" {
            use base64::Engine;
            let data = payload["data"].as_str().unwrap_or_default();
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                self.output.lock().unwrap().extend_from_slice(&bytes);
            }
        }
        if event == "term.exit" {
            if let Some(dsr) = payload["dsr_responses_sent"].as_u64() {
                self.dsr.store(dsr, Ordering::Relaxed);
            }
        }
        self.events
            .lock()
            .unwrap()
            .push((event.to_string(), payload));
    }
}

fn dispatcher() -> (CockpitDispatcher, Arc<RecordingSink>) {
    let sink = RecordingSink::new();
    let sandbox_root = std::env::temp_dir().join(format!(
        "tiller-ptyreal-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let manager = SessionManager::new(sink.clone(), sandbox_root, Arc::new(PtySessionFactory));
    (CockpitDispatcher::new(manager, TaskHost::new()), sink)
}

fn call(
    dispatcher: &CockpitDispatcher,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, tiller_rpc::RpcError> {
    dispatcher
        .dispatch(RpcRequest {
            id: RequestId::Num(1),
            method: method.to_string(),
            params,
        })
        .map(|response| response.result.expect("result present"))
}

fn ok(
    dispatcher: &CockpitDispatcher,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    call(dispatcher, method, params).expect("ok response")
}

fn spawn(dispatcher: &CockpitDispatcher, program: &str, args: &[&str]) -> u64 {
    let result = ok(
        dispatcher,
        "term.spawn",
        serde_json::json!({
            "program": program,
            "args": args,
            "cwd": repo_root().to_string_lossy(),
        }),
    );
    result["session_id"].as_u64().expect("session_id")
}

/// The repo root (this test binary lives in `<repo>/target/debug/deps`).
fn repo_root() -> PathBuf {
    let mut dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .unwrap()
        .to_path_buf();
    for _ in 0..6 {
        if dir.join("Cargo.toml").is_file() && dir.join("crates").is_dir() {
            return dir;
        }
        dir = dir.parent().expect("walk up to repo root").to_path_buf();
    }
    panic!(
        "repo root not found above {}",
        std::env::current_exe().unwrap().display()
    );
}

const WAIT: Duration = Duration::from_secs(15);

#[test]
fn short_program_flow_spawn_output_exit() {
    let (dispatcher, sink) = dispatcher();
    let id = spawn(&dispatcher, "cmd.exe", &["/c", "echo pty-real-hello"]);

    sink.wait_for_output(b"pty-real-hello", WAIT);
    let exit = sink.wait_for(WAIT, |event, p| {
        event == "term.exit" && p["session_id"] == id
    });
    assert_eq!(exit["exit_code"], 0);
    assert_eq!(exit["success"], true);

    // The exited session is gone: the next write reports SESSION_NOT_FOUND.
    let error = call(
        &dispatcher,
        "term.write",
        serde_json::json!({ "session_id": id, "data": "" }),
    )
    .expect_err("session gone");
    assert_eq!(error.code, SESSION_NOT_FOUND);
}

#[test]
fn interactive_session_resize_write_kill() {
    let (dispatcher, sink) = dispatcher();
    let id = spawn(&dispatcher, "cmd.exe", &[]);

    // cmd prints a banner promptly; that proves the actor pumps output.
    sink.wait_for_output(b"Microsoft Windows", WAIT);

    // Resize must not disturb the session.
    ok(
        &dispatcher,
        "term.resize",
        serde_json::json!({ "session_id": id, "cols": 120, "rows": 40 }),
    );

    // Input roundtrip through base64: `echo` a unique marker.
    use base64::Engine;
    let input = base64::engine::general_purpose::STANDARD.encode(b"echo resize-survives-1234\r\n");
    ok(
        &dispatcher,
        "term.write",
        serde_json::json!({ "session_id": id, "data": input }),
    );
    sink.wait_for_output(b"resize-survives-1234", WAIT);

    // Stats answer while the session is live.
    ok(
        &dispatcher,
        "term.stats",
        serde_json::json!({ "session_id": id }),
    );

    ok(
        &dispatcher,
        "term.kill",
        serde_json::json!({ "session_id": id }),
    );
    let exit = sink.wait_for(WAIT, |event, p| {
        event == "term.exit" && p["session_id"] == id
    });
    assert!(exit["exit_code"].is_u64(), "exit status reported: {exit}");
}

#[test]
fn manager_drop_kills_long_running_process() {
    // Kill-on-disconnect with a real ConPTY child: `pause` never exits on
    // its own, so an arriving `term.exit` after `drop(manager)` proves the
    // actor noticed the abandoned manager, killed the child and reaped it
    // — all inside the test budget.
    let sink = RecordingSink::new();
    let sandbox_root = std::env::temp_dir().join(format!(
        "tiller-ptyreal-drop-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let manager = SessionManager::new(sink.clone(), sandbox_root, Arc::new(PtySessionFactory));
    let id = manager
        .spawn_session(SpawnSpec {
            program: "cmd.exe".to_string(),
            args: vec!["/c".to_string(), "pause".to_string()],
            env: Vec::new(),
            cwd: Some(repo_root()),
            cols: 80,
            rows: 24,
        })
        .expect("spawn");

    // The child is alive and the actor pumps its output.
    sink.wait_for_output(b"Press any key", WAIT);

    drop(manager);

    let exit = sink.wait_for(WAIT, |event, p| {
        event == "term.exit" && p["session_id"] == id
    });
    assert!(exit["exit_code"].is_u64(), "exit status reported: {exit}");
}
