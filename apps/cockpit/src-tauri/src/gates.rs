//! Verification gates (brief §5 rule 5): `cargo check` / `cargo test` run
//! **in the task worktree** against the review candidate — the authoritative
//! verify loop. The fast loop (LSP) is out of scope here (P7 boundary).
//!
//! Mechanics only, no policy: [`GATE_PRESETS`] hardcodes what a gate is
//! (same pattern as the terminal `TOOL_PRESETS` — no configuration), the
//! [`CommandRunner`] seam keeps process handling testable (production
//! [`ProcessRunner`], scripted fakes in tests), and orchestration
//! (preconditions, registry, evidence, events) lives in `task_host.rs`.
//!
//! Output discipline: gates capture stdout+stderr combined, but only the
//! last [`LOG_TAIL_BYTES`] ship as evidence (`log_tail`) — build scripts
//! can echo secrets, so nothing beyond the tail ever leaves the process.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

/// Tauri event name for one gate's status change (`running` → final).
pub const EVENT_GATE: &str = "task.gate";

/// Per-gate wall-clock budget. Generous on purpose: a cold `cargo test`
/// on a workspace can legitimately take minutes; the timeout exists to
/// reclaim a wedged run (deadlocked test, stuck linker), not to rush it.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(600);

/// How much combined stdout+stderr ships as `log_tail` (≈8 KiB): enough
/// for the failing compiler errors / test summary, capped so a chatty
/// build cannot flood the evidence store or the UI.
pub const LOG_TAIL_BYTES: usize = 8 * 1024;

/// Hardcoded gate definitions (brief §5; no configuration surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatePreset {
    pub id: &'static str,
    pub label: &'static str,
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// The two gates. `cargo check` stops at "does it compile" — no `-D
/// warnings`, warnings are a profile concern, not a compile gate; the
/// review (human or agent) reads them in the diff.
pub const GATE_PRESETS: &[GatePreset] = &[
    GatePreset {
        id: "cargo-check",
        label: "cargo check",
        program: "cargo",
        args: &["check", "--workspace", "--all-targets"],
    },
    GatePreset {
        id: "cargo-test",
        label: "cargo test",
        program: "cargo",
        args: &["test", "--workspace"],
    },
];

impl GateStatus {
    /// Wire/event name (matches the serde snake_case spelling).
    pub fn as_str(&self) -> &'static str {
        match self {
            GateStatus::Passed => "passed",
            GateStatus::Failed => "failed",
            GateStatus::Timeout => "timeout",
        }
    }
}

/// Find a preset by id (dispatch validates `task.run_gates` ids with it).
pub fn gate_preset(id: &str) -> Option<&'static GatePreset> {
    GATE_PRESETS.iter().find(|preset| preset.id == id)
}

/// Terminal status of one gate. A spawn failure (`cargo` not on PATH)
/// reports as [`GateStatus::Failed`] with the io error in `log_tail`:
/// from the operator's seat "the gate did not pass" is the honest answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Passed,
    Failed,
    Timeout,
}

/// One gate's finished result — this shape is both the evidence record
/// and the UI's badge source.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateResult {
    pub id: String,
    pub label: String,
    pub status: GateStatus,
    /// Process exit code, when the process ever exited (none on timeout
    /// or spawn failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Last [`LOG_TAIL_BYTES`] of combined stdout+stderr.
    pub log_tail: String,
}

/// Raw process outcome as produced by a [`CommandRunner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutcome {
    pub status: GateStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Full captured combined output (uncapped; the gate layer tails it).
    pub output: String,
}

/// The process seam: production runs real processes, tests script them.
/// `env` rides **on top of** the inherited environment.
pub trait CommandRunner: Send + Sync + 'static {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
    ) -> ProcessOutcome;
}

/// Keep the last `cap` bytes, marking the cut so the operator can tell a
/// tail from full output. Byte-oriented: output is opaque bytes on the
/// wire, and the cap is a byte budget — **including the marker** (P7
/// review MINOR-1): marker + bytes never exceed `cap`.
pub fn tail_bytes(output: &str, cap: usize) -> String {
    const MARKER: &str = "… [output tail]\n";
    let budget = cap.saturating_sub(MARKER.len());
    if output.len() <= budget {
        return output.to_owned();
    }
    // `char_indices` finds the first char boundary at/after the cut so we
    // never split a code point in half.
    let start = output
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= output.len() - budget)
        .unwrap_or(output.len() - budget);
    format!("{MARKER}{}", &output[start..])
}

// ------------------------------------------------------------ process impl

/// Poll cadence while waiting for the process (and for its exit after a
/// kill — `try_wait` is non-blocking, so this is the loop's sleep).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Grace for the process to actually die after a kill before the runner
/// gives up reaping and reports what it has (bounded, see below).
const KILL_GRACE: Duration = Duration::from_secs(10);

/// Grace for the output readers to hand in their buffers once the child
/// is gone (a straggler that slipped the tree kill can still hold the
/// pipe open — the drain is time-boxed so it cannot wedge the runner).
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Read chunk size for the bounded capture loop.
const READ_CHUNK: usize = 4096;

/// Drain a stream keeping only its **last `cap` bytes** (P7 review
/// MAJOR-4): reader memory is O(cap) however chatty the child — a build
/// script echoing gigabytes must not OOM the app. Raw bytes on purpose:
/// the head cut can land inside a multi-byte code point, and where to
/// cut is decided at extraction ([`utf8_tail`]), not while buffering.
fn drain_tail<R: Read + ?Sized>(pipe: &mut R, cap: usize) -> Vec<u8> {
    let mut tail: Vec<u8> = Vec::with_capacity(cap.min(READ_CHUNK));
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > cap {
                    // discard the head: only the end matters downstream
                    tail.drain(..tail.len() - cap);
                }
            }
            // EINTR is transient; any other error means the pipe is
            // unusable — keep what arrived (capture is best-effort
            // evidence, never a hard failure).
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    tail
}

/// Decode a captured byte tail as display text: skip leading
/// continuation bytes (0b10xxxxxx) so the ring's head cut never splits
/// a code point, then decode lossily — the child's output may not be
/// UTF-8 at all, and `log_tail` is operator text, not a byte protocol.
fn utf8_tail(bytes: Vec<u8>) -> String {
    let start = bytes
        .iter()
        .position(|byte| byte & 0xC0 != 0x80)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Production [`CommandRunner`]: `std::process` with inherited env plus
/// overrides, piped stdout/stderr drained by reader threads (a full pipe
/// would deadlock the child; each reader buffers only the last
/// [`LOG_TAIL_BYTES`] — see [`drain_tail`]), a poll-based timeout and
/// **tree-kill**.
///
/// Tree-kill per platform:
///
/// - **Windows:** `taskkill /PID <pid> /T /F` — `cargo` spawns
///   `rustc`/test binaries as children that must not be orphaned.
/// - **Unix:** the child is spawned as its own session leader
///   (`setsid` in `pre_exec`), so its pid doubles as the process-group
///   id and `killpg(-group)` reaches cargo *and* its rustc/test-binary
///   grandchildren with one signal (P7 review MAJOR-3).
#[derive(Debug, Default)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
    ) -> ProcessOutcome {
        let started = Instant::now();
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: `pre_exec` callbacks run in the forked child before
            // exec, where only async-signal-safe calls are allowed.
            // `setsid()` is a thin syscall wrapper — no allocation, no
            // locks, no libc/global state — squarely in the allowed
            // category. Making the child its own session leader puts it
            // and everything it spawns into a fresh process group, which
            // is what the timeout's `killpg` below needs to reach the
            // whole tree.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ProcessOutcome {
                    status: GateStatus::Failed,
                    exit_code: None,
                    duration_ms: started.elapsed().as_millis() as u64,
                    output: format!("failed to spawn `{program}`: {error}"),
                }
            }
        };

        // Reader threads drain both pipes so the child can never block on
        // a full one; each keeps only the last LOG_TAIL_BYTES (see
        // [`drain_tail`]) and sends its tail once at EOF.
        let (out_tx, out_rx) = mpsc::channel::<String>();
        let (err_tx, err_rx) = mpsc::channel::<String>();
        if let Some(mut pipe) = child.stdout.take() {
            thread::spawn(move || {
                let tail = utf8_tail(drain_tail(&mut pipe, LOG_TAIL_BYTES));
                let _ = out_tx.send(tail);
            });
        }
        if let Some(mut pipe) = child.stderr.take() {
            thread::spawn(move || {
                let tail = utf8_tail(drain_tail(&mut pipe, LOG_TAIL_BYTES));
                let _ = err_tx.send(tail);
            });
        }

        // Wait for exit, enforcing the timeout with a tree-kill.
        let deadline = started + timeout;
        let mut kill_issued_at = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if kill_issued_at.is_none() && Instant::now() >= deadline {
                        kill_tree(child.id());
                        kill_issued_at = Some(Instant::now());
                    }
                    if let Some(issued) = kill_issued_at {
                        if issued.elapsed() > KILL_GRACE {
                            // Did not die even after the kill — stop
                            // waiting; the report below says timeout and
                            // the drain is time-boxed, so nothing hangs.
                            break None;
                        }
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                Err(error) => {
                    return ProcessOutcome {
                        status: GateStatus::Failed,
                        exit_code: None,
                        duration_ms: started.elapsed().as_millis() as u64,
                        output: format!("failed to wait for `{program}`: {error}"),
                    }
                }
            }
        };

        let duration_ms = started.elapsed().as_millis() as u64;
        let mut output = String::new();
        for rx in [out_rx, err_rx] {
            match rx.recv_timeout(DRAIN_GRACE) {
                Ok(buf) => output.push_str(&buf),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    output.push_str("\n[tiller] output stream did not close in time; captured output may be incomplete\n");
                }
                // Disconnected without a send: the reader died before
                // EOF (never happens on a clean drain) — nothing to add.
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        }

        match (status, kill_issued_at) {
            // Clean exit before any kill: success decides pass/fail.
            (Some(status), None) => ProcessOutcome {
                status: if status.success() {
                    GateStatus::Passed
                } else {
                    GateStatus::Failed
                },
                exit_code: status.code(),
                duration_ms,
                output,
            },
            // The timeout fired: any exit after this point is *because of
            // the kill* (taskkill /F exits with a failure code) — the
            // operator-facing fact is the timeout, the post-kill exit code
            // would be noise.
            (_, Some(_)) => {
                let mut output = output;
                output.push_str("\n[tiller] gate timed out; process tree killed\n");
                ProcessOutcome {
                    status: GateStatus::Timeout,
                    exit_code: None,
                    duration_ms,
                    output,
                }
            }
            // A reap failure without a kill (cannot happen: the None break
            // only occurs after KILL_GRACE post-kill) — defensive branch,
            // honest output over a silent guess.
            (None, None) => {
                let mut output = output;
                output.push_str("\n[tiller] process exited without a waitable status\n");
                ProcessOutcome {
                    status: GateStatus::Failed,
                    exit_code: None,
                    duration_ms,
                    output,
                }
            }
        }
    }
}

/// Kill the whole process tree rooted at `pid`.
#[cfg(windows)]
fn kill_tree(pid: u32) {
    // /T walks the tree, /F forces (no graceful shutdown prompt games).
    // taskkill's own exit status is best-effort: if it fails there is
    // nothing left to try here, and the KILL_GRACE loop reports the miss.
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn kill_tree(pid: u32) {
    // The child runs as its own session leader (spawned with `setsid` in
    // `pre_exec`), so its pid doubles as the process-group id and the
    // negative address reaches every member — cargo itself plus its
    // rustc/test-binary grandchildren (P7 review MAJOR-3).
    // SAFETY: `killpg` signals a numeric process group; it touches no
    // Rust memory and takes no locks. It runs only while the child is
    // un-reaped (the poll loop kills solely after `try_wait` answered
    // `Ok(None)`), so the pid — and with it the group id — cannot have
    // been recycled; a failure (e.g. ESRCH once the group is empty) is
    // ignored because the KILL_GRACE loop reports the outcome.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted runner: hands out queued outcomes, records what ran.
    struct FakeRunner {
        outcomes: std::sync::Mutex<std::collections::VecDeque<ProcessOutcome>>,
        calls: std::sync::Mutex<Vec<(String, std::path::PathBuf)>>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<ProcessOutcome>) -> Self {
            Self {
                outcomes: std::sync::Mutex::new(outcomes.into()),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            cwd: &Path,
            _env: &[(String, String)],
            _timeout: Duration,
        ) -> ProcessOutcome {
            self.calls
                .lock()
                .unwrap()
                .push((format!("{program} {}", args.join(" ")), cwd.to_path_buf()));
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ProcessOutcome {
                    status: GateStatus::Passed,
                    exit_code: Some(0),
                    duration_ms: 1,
                    output: String::new(),
                })
        }
    }

    #[test]
    fn tail_bytes_caps_to_the_last_bytes_and_marks_the_cut() {
        let short = "abcdef";
        assert_eq!(tail_bytes(short, 64), short);
        let long = "x".repeat(100);
        let tailed = tail_bytes(&long, 32);
        assert!(tailed.starts_with("… [output tail]\n"));
        // the marker counts against the budget: marker + bytes = cap
        // exactly (P7 review MINOR-1), never over
        assert_eq!(tailed.len(), 32);
        assert!(tailed.ends_with("xxxxxxxxxxxxxx"));
        // never splits a code point
        let multibyte = "ąąąąąąąąąą"; // 10 × 2 bytes
        let tailed = tail_bytes(multibyte, 32);
        assert!(tailed.ends_with('ą'));
        assert!(tailed.starts_with("… [output tail]\n"));
        assert!(tailed.len() <= 32);
    }

    #[test]
    fn drain_tail_keeps_only_the_last_cap_bytes_of_any_stream() {
        use std::io::Cursor;
        // > one chunk (4 KiB reads): the ring must discard the head and
        // never grow past the cap however much arrives (P7 review
        // MAJOR-4: capture memory is O(cap), not O(output))
        let tail = drain_tail(&mut Cursor::new(vec![b'a'; 10_000]), 8 * 1024);
        assert_eq!(tail.len(), 8 * 1024);
        assert!(tail.iter().all(|&b| b == b'a'));
        // a short stream passes through whole
        let tail = drain_tail(&mut Cursor::new(b"short".to_vec()), 8 * 1024);
        assert_eq!(tail, b"short");
    }

    #[test]
    fn utf8_tail_never_splits_a_code_point_at_the_head_cut() {
        // 'ą' = 2 bytes; the ring's head cut lands inside the first
        // character — its leading continuation byte is dropped, the
        // remaining characters arrive whole
        let text = utf8_tail(vec![0x85, 0xC4, 0x85, 0xC4, 0x85]);
        assert_eq!(text, "ąą");
        // already on a boundary: untouched
        assert_eq!(utf8_tail("ok".as_bytes().to_vec()), "ok");
    }

    #[test]
    fn gate_presets_pin_the_verify_loop_commands() {
        assert_eq!(
            gate_preset("cargo-check").unwrap().args,
            &["check", "--workspace", "--all-targets"]
        );
        assert_eq!(
            gate_preset("cargo-test").unwrap().args,
            &["test", "--workspace"]
        );
        assert_eq!(gate_preset("cargo-check").unwrap().program, "cargo");
        assert!(gate_preset("nope").is_none());
        assert_eq!(GATE_PRESETS.len(), 2);
    }

    #[test]
    fn fake_runner_records_calls_and_returns_queued_outcomes() {
        let runner = FakeRunner::new(vec![ProcessOutcome {
            status: GateStatus::Failed,
            exit_code: Some(101),
            duration_ms: 5,
            output: "boom".into(),
        }]);
        let outcome = runner.run("cargo", &["check"], Path::new("/w"), &[], GATE_TIMEOUT);
        assert_eq!(outcome.status, GateStatus::Failed);
        assert_eq!(outcome.exit_code, Some(101));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0].0, "cargo check");
        assert_eq!(calls[0].1, Path::new("/w"));
    }
}
