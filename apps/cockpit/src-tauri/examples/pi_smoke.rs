//! Manual pi smoke for P3a: boots the *real* pi TUI through the cockpit
//! session host (sandbox included) and answers the P3 research question.
//!
//! An example, not an `#[ignore]`d test: it needs the operator's real pi
//! install and credentials, prints live observations, and must never run
//! in CI by accident (`cargo test -- --ignored` is too easy to flip on).
//!
//! Run from the repo root:
//!
//! ```text
//! cargo run -p cockpit --example pi_smoke [-- <cwd>]
//! ```
//!
//! Observations collected:
//!
//! - (a) DSR: does pi probe `ESC[6n` (session `dsr_responses_sent` >= 1)?
//! - (b) does pi render a TUI (ANSI sequences in the output stream)?
//! - (c) the key experiment: a 256 KiB input burst is written *immediately*
//!   after spawn, before the first render — does pi still boot and render
//!   (the P2 residual write window does not bite), or does the burst block
//!   it? Measures time-to-first-output and how long the burst write took.
//! - (d) does `kill` wind the process tree down (exit event arrives)?
//!
//! Output bytes are never dumped (only lengths/flags and a short escaped
//! preview) — auth material must not leak through logs.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cockpit_lib::{
    EventSink, PtySessionFactory, SessionManager, SpawnSpec, EVENT_EXIT, EVENT_OUTPUT,
};
use serde_json::Value;

/// Size of the immediate input burst: the P2 residual-window probe.
const BURST_BYTES: usize = 256 * 1024;

/// How many escaped characters of the first screen the preview keeps
/// (accumulated across chunks — the first chunk alone is just the
/// terminal title sequence).
const PREVIEW_CHARS: usize = 1024;

/// What the burst thread reports back: how long the 256 KiB write took
/// (or the session error it died on).
type BurstResult = Result<Duration, cockpit_lib::TermError>;
/// How long to wait for the first output before declaring pi blocked.
const FIRST_OUTPUT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to let the TUI render after the first output before killing.
const RENDER_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for the exit event after kill.
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);

struct SmokeState {
    spawn_at: Instant,
    first_output_at: Option<Duration>,
    total_bytes: usize,
    has_ansi: bool,
    first_preview: Option<String>,
    exit: Option<Value>,
    dsr_live: u64,
}

struct SmokeSink {
    state: Arc<Mutex<SmokeState>>,
}

impl EventSink for SmokeSink {
    fn emit(&self, event: &str, payload: Value) {
        let mut state = self.state.lock().unwrap();
        match event {
            EVENT_OUTPUT => {
                let bytes = decode(&payload);
                if state.first_output_at.is_none() {
                    state.first_output_at = Some(state.spawn_at.elapsed());
                }
                match state.first_preview.as_mut() {
                    Some(p) if p.chars().count() < PREVIEW_CHARS => {
                        p.push_str(&escaped_preview(&bytes, PREVIEW_CHARS));
                    }
                    Some(_) => {}
                    None => {
                        state.first_preview = Some(escaped_preview(&bytes, PREVIEW_CHARS));
                    }
                }
                state.total_bytes += bytes.len();
                if bytes.contains(&0x1b) {
                    state.has_ansi = true;
                }
            }
            EVENT_EXIT => {
                state.dsr_live = payload["dsr_responses_sent"].as_u64().unwrap_or(0);
                state.exit = Some(payload);
            }
            _ => {}
        }
    }
}

fn decode(payload: &Value) -> Vec<u8> {
    use base64::Engine;
    payload["data"]
        .as_str()
        .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
        .unwrap_or_default()
}

fn escaped_preview(bytes: &[u8], max: usize) -> String {
    let mut out = String::new();
    for &b in bytes.iter().take(max) {
        match b {
            0x1b => out.push_str("\\e"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

fn main() {
    let cwd: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Repos/tiller"));
    println!("[smoke] cwd = {}", cwd.display());
    println!("[smoke] burst = {BURST_BYTES} bytes written immediately after spawn");

    let state = Arc::new(Mutex::new(SmokeState {
        spawn_at: Instant::now(),
        first_output_at: None,
        total_bytes: 0,
        has_ansi: false,
        first_preview: None,
        exit: None,
        dsr_live: 0,
    }));
    let sink = Arc::new(SmokeSink {
        state: state.clone(),
    });
    let sandbox_root = std::env::temp_dir().join(format!("tiller-pi-smoke-{}", std::process::id()));
    let manager = SessionManager::new(
        sink.clone(),
        sandbox_root.clone(),
        Arc::new(PtySessionFactory),
    );

    let spec = SpawnSpec {
        program: "pi".to_string(),
        args: Vec::new(),
        // `PI_SMOKE_AGENT_DIR` opts out of the sandbox: point it at an
        // existing dir to probe which agent-home files pi actually needs
        // (the manager skips sandbox creation when the caller sets
        // PI_CODING_AGENT_DIR itself).
        env: match std::env::var_os("PI_SMOKE_AGENT_DIR") {
            Some(dir) => {
                vec![(
                    "PI_CODING_AGENT_DIR".to_string(),
                    dir.to_string_lossy().into_owned(),
                )]
            }
            None => Vec::new(),
        },
        cwd: Some(cwd.clone()),
        cols: 120,
        rows: 40,
    };
    let id = match manager.spawn_session(spec) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("[smoke] spawn failed: {error}");
            std::process::exit(1);
        }
    };
    println!(
        "[smoke] session {id} spawned; sandbox under {}",
        sandbox_root.display()
    );

    // The burst goes through its own thread, exactly like a UI rpc handler:
    // the actor keeps pumping output while the write drains.
    // `PI_SMOKE_NO_BURST` skips it — the baseline for the (c) comparison
    // (time-to-first-output with vs without the immediate burst).
    let burst_manager = manager.clone();
    let (done_tx, done_rx): (mpsc::Sender<BurstResult>, Receiver<BurstResult>) = mpsc::channel();
    if std::env::var_os("PI_SMOKE_NO_BURST").is_some() {
        println!("[smoke] PI_SMOKE_NO_BURST set — no burst, baseline run");
    } else {
        thread::spawn(move || {
            let payload = vec![b'a'; BURST_BYTES];
            let started = Instant::now();
            let result = burst_manager
                .write(id, &payload)
                .map(|()| started.elapsed());
            let _ = done_tx.send(result);
        });
    }

    // Watch for the first output while the burst is in flight.
    let deadline = Instant::now() + FIRST_OUTPUT_TIMEOUT;
    let mut last_stats = Instant::now() - Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(at) = state.lock().unwrap().first_output_at {
            println!("[smoke] first output after {at:?} (burst still draining or done)");
            break;
        }
        if let Ok(exit) = manager.stats(id) {
            if last_stats.elapsed() >= Duration::from_secs(2) {
                println!(
                    "[smoke] waiting for first output… dsr_responses_sent={}",
                    exit.dsr_responses_sent
                );
                last_stats = Instant::now();
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Let the TUI settle, then take the session down.
    thread::sleep(RENDER_GRACE);
    // Was pi still alive right before the kill? Distinguishes "burst
    // crashed pi" from "kill terminated a healthy pi" (both exit 1).
    println!(
        "[smoke] session alive at kill: {}",
        manager.stats(id).is_ok()
    );
    println!("[smoke] killing session {id}");
    match manager.kill(id) {
        Ok(()) => {}
        Err(error) => println!("[smoke] kill reported: {error}"),
    }

    let exit_deadline = Instant::now() + EXIT_TIMEOUT;
    while Instant::now() < exit_deadline && state.lock().unwrap().exit.is_none() {
        thread::sleep(Duration::from_millis(50));
    }

    let burst_result = done_rx
        .recv_timeout(Duration::from_secs(5))
        .ok()
        .and_then(|r| r.ok());

    let state = state.lock().unwrap();
    println!("---- pi smoke summary ----");
    println!("first output:  {:?}", state.first_output_at);
    println!("burst write:   {:?}", burst_result);
    println!("total output:  {} bytes", state.total_bytes);
    println!("ansi rendered: {}", state.has_ansi);
    println!("dsr answered:  {} (live) / {} (exit)", state.dsr_live, {
        state
            .exit
            .as_ref()
            .and_then(|e| e["dsr_responses_sent"].as_u64())
            .unwrap_or(0)
    });
    println!(
        "first preview: {}",
        state.first_preview.as_deref().unwrap_or("<none>")
    );
    match &state.exit {
        Some(exit) => println!(
            "exit:          code={} success={} signal={:?}",
            exit["exit_code"], exit["success"], exit["signal"]
        ),
        None => println!("exit:          <no exit event — kill failed?>"),
    }

    println!("---- conclusions ----");
    let dsr = state.dsr_live.max(
        state
            .exit
            .as_ref()
            .and_then(|e| e["dsr_responses_sent"].as_u64())
            .unwrap_or(0),
    );
    println!(
        "(a) DSR probe: {}",
        if dsr >= 1 {
            "YES — pi probed ESC[6n, host answered"
        } else {
            "NO"
        }
    );
    println!(
        "(b) TUI render: {}",
        if state.has_ansi {
            "YES — ANSI sequences present"
        } else {
            "NO"
        }
    );
    let boot_ok = state.first_output_at.is_some();
    println!(
        "(c) burst @spawn: {}",
        if boot_ok {
            "pi booted and rendered despite the immediate 256 KiB burst — residual P2 write window did not block the boot"
        } else {
            "NO first output within the window — the immediate burst blocked pi's boot (residual window REAL on pi)"
        }
    );
    println!(
        "(d) kill: {}",
        if state.exit.is_some() {
            "YES — exit event arrived"
        } else {
            "NO"
        }
    );
}
