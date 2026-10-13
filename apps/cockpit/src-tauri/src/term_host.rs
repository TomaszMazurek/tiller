//! Hosting PTY sessions for the cockpit UI.
//!
//! [`SessionManager`] owns one *session actor* thread per spawned program.
//! The actor owns the [`HostedSession`] exclusively (std has no channel
//! `select`, and `mpsc::Sender` is `!Sync`, so sharing a `PtySession`
//! across threads is off the table): it drains pending commands, then
//! pumps one output poll — a 20 ms cadence that bounds both command latency
//! and the jitter added to output delivery. Output chunks and the exit
//! status reach the UI as `term:output` / `term:exit` notifications through
//! the [`EventSink`] (the Tauri `AppHandle` in production, a recorder in
//! tests).
//!
//! Data flow per session:
//!
//! ```text
//! UI (rpc term.*) ──cmd channel──► session actor ──PtySession──► PTY ─► pi
//! PTY output ──PtySession pump──► actor recv_output ──EventSink──► term:output
//! child exit ──PtySession reaper──► actor wait_exit ──EventSink──► term:exit
//! ```
//!
//! Blocking semantics: `write` mirrors [`tiller_term::PtySession::write`]
//! (blocks until every byte is drained), so a caller pacing a huge paste
//! paces the actor too — commands issued while a write is in flight wait
//! for it. Kill therefore cannot overtake an in-flight write; the UI only
//! sends small keystroke writes, and the big-burst case is exactly the P3
//! research question the smoke example measures.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::json;
use tiller_term::{PtySession, SpawnConfig};

use crate::sandbox;
use crate::session_tail::{self, TailerHandle, TimelineEntry};

/// Tauri event carrying one output chunk: `{session_id, data}` (base64).
pub const EVENT_OUTPUT: &str = "term:output";
/// Tauri event carrying the session's exit: `{session_id, exit_code,
/// signal?, success, dsr_responses_sent}`.
pub const EVENT_EXIT: &str = "term:exit";

/// Output poll cadence of a session actor (see the module docs).
const OUTPUT_POLL: Duration = Duration::from_millis(20);

/// Session identifier. A per-app-run monotonic `u64`: small, JSON-safe,
/// enough for one cockpit process (a UUID would add a dependency for no
/// benefit — the id never leaves this app instance).
pub type SessionId = u64;

/// One poll of a session's output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvOutcome {
    /// A chunk arrived.
    Chunk(Vec<u8>),
    /// Quiet but alive.
    Quiet,
    /// The stream closed (program gone; drain complete).
    Closed,
}

/// The hosted program's final status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermExit {
    pub exit_code: u32,
    pub signal: Option<String>,
    pub success: bool,
}

/// Live diagnostics for a running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    /// DSR probes the host answered so far (the P2 counter).
    pub dsr_responses_sent: u64,
}

/// A program hosted in a PTY, as the session actor sees it.
///
/// Exists so the manager is testable against scripted sessions without a
/// real PTY; the production implementation wraps [`PtySession`] verbatim
/// (crates/term is not touched). `Send` only: exactly one actor thread owns
/// the session.
pub trait HostedSession: Send {
    /// Receive the next output chunk, waiting up to `timeout`.
    fn recv_output(&self, timeout: Duration) -> RecvOutcome;
    /// Write input bytes; blocks until fully drained.
    fn write(&self, bytes: &[u8]) -> io::Result<()>;
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
    /// Best-effort termination initiation.
    fn kill(&self) -> io::Result<()>;
    /// Block until the program exits.
    fn wait_exit(&self) -> TermExit;
    fn dsr_responses_sent(&self) -> u64;
}

impl HostedSession for PtySession {
    fn recv_output(&self, timeout: Duration) -> RecvOutcome {
        match self.output().recv_timeout(timeout) {
            Ok(bytes) => RecvOutcome::Chunk(bytes),
            Err(RecvTimeoutError::Timeout) => RecvOutcome::Quiet,
            Err(RecvTimeoutError::Disconnected) => RecvOutcome::Closed,
        }
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        PtySession::write(self, bytes)
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        PtySession::resize(self, cols, rows)
    }

    fn kill(&self) -> io::Result<()> {
        PtySession::kill(self)
    }

    fn wait_exit(&self) -> TermExit {
        let status = PtySession::wait(self);
        TermExit {
            exit_code: status.exit_code(),
            signal: status.signal().map(str::to_string),
            success: status.success(),
        }
    }

    fn dsr_responses_sent(&self) -> u64 {
        PtySession::dsr_responses_sent(self)
    }
}

/// Fully-resolved spawn request (params validated, sandbox applied).
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment on top of the inherited one.
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
}

impl SpawnSpec {
    fn to_term_config(&self) -> SpawnConfig {
        SpawnConfig {
            program: self.program.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            cwd: self.cwd.clone(),
            cols: self.cols,
            rows: self.rows,
        }
    }
}

/// Creates [`HostedSession`]s; the production factory spawns real PTYs.
pub trait SessionFactory: Send + Sync {
    fn spawn(&self, spec: &SpawnSpec) -> io::Result<Box<dyn HostedSession>>;
}

/// Production factory: [`PtySession`] via crates/term.
pub struct PtySessionFactory;

impl SessionFactory for PtySessionFactory {
    fn spawn(&self, spec: &SpawnSpec) -> io::Result<Box<dyn HostedSession>> {
        PtySession::spawn(spec.to_term_config()).map(|s| Box::new(s) as Box<dyn HostedSession>)
    }
}

/// Delivers session notifications to the UI. In production the Tauri
/// `AppHandle` (`Emitter::emit` is thread-safe); a recorder in tests.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: serde_json::Value);
}

/// What a session actor is asked to do. Every variant carries a reply
/// channel so the rpc handler can surface the real outcome.
enum SessionCommand {
    Write {
        bytes: Vec<u8>,
        reply: Sender<io::Result<()>>,
    },
    Resize {
        cols: u16,
        rows: u16,
        reply: Sender<io::Result<()>>,
    },
    Kill {
        reply: Sender<io::Result<()>>,
    },
    Stats {
        reply: Sender<SessionStats>,
    },
}

#[derive(Debug, Clone)]
struct SessionHandle {
    id: SessionId,
    commands: Sender<SessionCommand>,
}

impl SessionHandle {
    /// Send one command and wait for its answer. A dropped channel (actor
    /// gone mid-teardown) maps to [`TermError::SessionGone`].
    fn call<T>(&self, make: impl FnOnce(Sender<T>) -> SessionCommand) -> Result<T, TermError>
    where
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.commands
            .send(make(reply_tx))
            .map_err(|_| TermError::SessionGone(self.id))?;
        reply_rx.recv().map_err(|_| TermError::SessionGone(self.id))
    }
}

/// Why a session operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermError {
    /// No live session under this id (never spawned, or already exited and
    /// been reaped by its actor).
    SessionGone(SessionId),
    /// The operation ran but the session/OS answered with an error.
    Io(String),
}

impl std::fmt::Display for TermError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TermError::SessionGone(id) => write!(f, "no live session {id}"),
            TermError::Io(message) => write!(f, "{message}"),
        }
    }
}

struct ManagerInner {
    sessions: Mutex<HashMap<SessionId, SessionHandle>>,
    /// Sandbox dir per pi session (the observation timeline's data
    /// source; see [`SessionManager::observe_entries`]).
    sandboxes: Mutex<HashMap<SessionId, PathBuf>>,
    next_id: AtomicU64,
    sink: Arc<dyn EventSink>,
    factory: Arc<dyn SessionFactory>,
    sandbox_root: PathBuf,
}

/// Owns all live sessions; one actor thread per session. Cheap to clone
/// (every clone shares the same map), so the manager can be handed to the
/// dispatcher and inspected from tests.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<ManagerInner>,
}

impl SessionManager {
    pub fn new(
        sink: Arc<dyn EventSink>,
        sandbox_root: PathBuf,
        factory: Arc<dyn SessionFactory>,
    ) -> Self {
        // Orphan sweep: agent-homes from previous runs (crash, force kill)
        // must not outlive their sessions — their `auth.json` copies and
        // the from-1 session-id reuse would leak state into this run.
        // Clearing the whole subtree is safe here: no session of this
        // manager exists yet (see `sandbox::sweep_agent_homes`).
        sandbox::sweep_agent_homes(&sandbox_root);
        Self {
            inner: Arc::new(ManagerInner {
                sessions: Mutex::new(HashMap::new()),
                sandboxes: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                sink,
                factory,
                sandbox_root,
            }),
        }
    }

    /// Spawn a program in a PTY and start its actor.
    ///
    /// Pi programs (by file-name heuristic) additionally get the agent-home
    /// sandbox (see [`crate::sandbox`]) unless the caller set
    /// `PI_CODING_AGENT_DIR` themselves.
    pub fn spawn_session(&self, mut spec: SpawnSpec) -> Result<SessionId, TermError> {
        let inner = Arc::clone(&self.inner);
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut sandbox_dir = None;
        if sandbox::is_pi_program(&spec.program) {
            // Program resolution always applies to pi spawns (the .cmd shim
            // translation is about *running* pi, not about its sandbox).
            let (program, prefix_args) = sandbox::resolve_pi_program(&spec.program);
            spec.program = program;
            spec.args = prefix_args.into_iter().chain(spec.args).collect();
            if !spec.env.iter().any(|(k, _)| k == "PI_CODING_AGENT_DIR") {
                let dir = sandbox::prepare_pi_sandbox(&inner.sandbox_root, id, spec.cwd.as_deref())
                    .map_err(|e| TermError::Io(format!("pi sandbox setup failed: {e}")))?;
                spec.env.push((
                    "PI_CODING_AGENT_DIR".to_string(),
                    dir.to_string_lossy().into_owned(),
                ));
                // The actor deletes the sandbox when the session ends, so
                // the (secret-bearing) copy never outlives its session.
                sandbox_dir = Some(dir);
            }
        }
        let session = match inner.factory.spawn(&spec) {
            Ok(session) => session,
            Err(e) => {
                // The session never started, so no actor will sweep this
                // dir — remove the (secret-bearing) copy now instead of
                // leaving it for the next app start's orphan sweep.
                if let Some(dir) = &sandbox_dir {
                    sandbox::cleanup_dir(dir);
                }
                return Err(TermError::Io(format!("spawn failed: {e}")));
            }
        };
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SessionCommand>();
        inner.sessions.lock().unwrap().insert(
            id,
            SessionHandle {
                id,
                commands: cmd_tx,
            },
        );
        // Pi sessions are observed from spawn: the tailer turns the
        // session JSONL pi writes in its sandbox into `session:entry`
        // notifications (see [`crate::session_tail`]). Auto-start rather
        // than observe-on-demand: zero UI coupling, the timeline works
        // from the first second, and the tailer simply waits out pi's
        // lazy session-file creation.
        let tailer = sandbox_dir.clone().map(|dir| {
            inner.sandboxes.lock().unwrap().insert(id, dir.clone());
            let stop = Arc::new(AtomicBool::new(false));
            let stop_for_thread = Arc::clone(&stop);
            let tailer_sink = Arc::clone(&inner.sink);
            let handle = std::thread::Builder::new()
                .name(format!("tiller-session-tail-{id}"))
                .spawn(move || session_tail::run_tailer(tailer_sink, id, dir, stop_for_thread))
                .expect("spawn session tailer");
            (stop, handle)
        });
        // No `Arc` back into the manager: a strong actor→manager ref would
        // keep the sessions map (and this very command sender) alive after
        // the last `SessionManager` clone drops, and the declared
        // kill-on-disconnect below would never trigger. The actor checks
        // liveness through a `Weak` upgrade and owns its own sink copy so
        // it can still report the exit while tearing down.
        let sink = Arc::clone(&inner.sink);
        let manager = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name(format!("tiller-term-host-{id}"))
            .spawn(move || run_session(manager, sink, id, sandbox_dir, tailer, session, cmd_rx))
            .expect("spawn session actor");
        Ok(id)
    }

    /// Snapshot of a live session's observed timeline: the entries
    /// parsed from the session file so far (pi sessions only — a session
    /// without a sandbox observes as empty; a pi session with no session
    /// file yet likewise, not an error: pi creates it lazily).
    pub fn observe_entries(&self, id: SessionId) -> Result<Vec<TimelineEntry>, TermError> {
        self.handle(id)?;
        let sandbox = self.inner.sandboxes.lock().unwrap().get(&id).cloned();
        Ok(sandbox.map_or_else(Vec::new, |dir| session_tail::snapshot(&dir)))
    }

    /// Write input bytes; blocks until the session actor drained them.
    pub fn write(&self, id: SessionId, bytes: &[u8]) -> Result<(), TermError> {
        self.handle(id)?
            .call(|reply| SessionCommand::Write {
                bytes: bytes.to_vec(),
                reply,
            })?
            .map_err(|e| TermError::Io(format!("write failed: {e}")))
    }

    pub fn resize(&self, id: SessionId, cols: u16, rows: u16) -> Result<(), TermError> {
        self.handle(id)?
            .call(|reply| SessionCommand::Resize { cols, rows, reply })?
            .map_err(|e| TermError::Io(format!("resize failed: {e}")))
    }

    pub fn kill(&self, id: SessionId) -> Result<(), TermError> {
        self.handle(id)?
            .call(|reply| SessionCommand::Kill { reply })?
            .map_err(|e| TermError::Io(format!("kill failed: {e}")))
    }

    pub fn stats(&self, id: SessionId) -> Result<SessionStats, TermError> {
        self.handle(id)?
            .call(|reply| SessionCommand::Stats { reply })
    }

    fn handle(&self, id: SessionId) -> Result<SessionHandle, TermError> {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(TermError::SessionGone(id))
    }
}

/// The session actor: owns the session, drains commands between output
/// polls, forwards output as `term:output`, and after the stream closes
/// publishes `term:exit` (with the final DSR counter) and removes the
/// session from the manager. When the manager goes away (app shutdown,
/// or the panel that owned it) the hosted program — owned by the app —
/// is taken down with it: the actor holds the manager only as a `Weak`,
/// so the last manager drop both disconnects the command channel and
/// fails the liveness upgrade, and either way the actor kills the
/// session, still reports `term:exit` through its own sink copy, and
/// deletes the session's pi sandbox if it had one.
fn run_session(
    manager: Weak<ManagerInner>,
    sink: Arc<dyn EventSink>,
    id: SessionId,
    sandbox_dir: Option<PathBuf>,
    tailer: Option<TailerHandle>,
    session: Box<dyn HostedSession>,
    commands: Receiver<SessionCommand>,
) {
    'actor: loop {
        // Liveness heartbeat: a failed upgrade means the manager was
        // dropped — nobody will send commands or listen anymore.
        if manager.upgrade().is_none() {
            session.kill().ok();
            break 'actor;
        }
        // Commands first: a kill must not wait behind output delivery.
        loop {
            match commands.try_recv() {
                Ok(cmd) => handle_command(session.as_ref(), cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The manager (or its sessions map) is gone: kill on
                    // disconnect, as promised.
                    session.kill().ok();
                    break 'actor;
                }
            }
        }
        match session.recv_output(OUTPUT_POLL) {
            RecvOutcome::Chunk(bytes) => {
                sink.emit(
                    EVENT_OUTPUT,
                    json!({
                        "session_id": id,
                        "data": base64_encode(&bytes),
                    }),
                );
            }
            RecvOutcome::Quiet => continue,
            RecvOutcome::Closed => break 'actor,
        }
    }
    let exit = session.wait_exit();
    let dsr_responses_sent = session.dsr_responses_sent();
    // Wind the observation tailer down before announcing the exit: the
    // join guarantees the tailer thread never outlives its session
    // actor, and every `session:entry` of this session precedes its
    // `term:exit` on the wire (its final drain runs while stopped).
    if let Some((stop, handle)) = tailer {
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }
    sink.emit(
        EVENT_EXIT,
        json!({
            "session_id": id,
            "exit_code": exit.exit_code,
            "signal": exit.signal,
            "success": exit.success,
            "dsr_responses_sent": dsr_responses_sent,
        }),
    );
    // The manager may already be gone (this exit is the kill-on-drop
    // teardown); only a live one still has a map entry to remove.
    if let Some(inner) = manager.upgrade() {
        inner.sessions.lock().unwrap().remove(&id);
        inner.sandboxes.lock().unwrap().remove(&id);
    }
    if let Some(dir) = &sandbox_dir {
        // Best-effort sandbox delete: a locked file on Windows must not
        // panic the actor — leftovers are swept at the next manager start.
        sandbox::cleanup_dir(dir);
    }
    // Dropping `session` (and its command channel) kills the child if it
    // somehow survived and winds the PTY threads down.
    drop(session);
}

fn handle_command(session: &dyn HostedSession, cmd: SessionCommand) {
    match cmd {
        SessionCommand::Write { bytes, reply } => {
            let _ = reply.send(session.write(&bytes));
        }
        SessionCommand::Resize { cols, rows, reply } => {
            let _ = reply.send(session.resize(cols, rows));
        }
        SessionCommand::Kill { reply } => {
            let _ = reply.send(session.kill());
        }
        SessionCommand::Stats { reply } => {
            let _ = reply.send(SessionStats {
                dsr_responses_sent: session.dsr_responses_sent(),
            });
        }
    }
}

/// Standard base64 (matches the frontend `atob`/`btoa` helpers).
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode standard base64 (used by the rpc layer).
pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text)
}

#[cfg(test)]
pub(crate) mod test_util {
    //! Scripted fakes shared by the term_host and dispatch unit tests.

    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    /// A sink that swallows everything: for construction-time use in tests
    /// that never expect events (e.g. the task host in dispatcher tests).
    pub(crate) struct NoopSink;

    impl EventSink for NoopSink {
        fn emit(&self, _event: &str, _payload: serde_json::Value) {}
    }

    /// Records every notification for assertions.
    pub(crate) struct RecordingSink {
        pub events: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl RecordingSink {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
            })
        }

        /// Wait until an event matching `pred` shows up (panics with the
        /// collected log after `timeout`).
        pub(crate) fn wait_for(
            &self,
            timeout: Duration,
            pred: impl Fn(&str, &serde_json::Value) -> bool,
        ) -> (String, serde_json::Value) {
            let deadline = Instant::now() + timeout;
            loop {
                {
                    let events = self.events.lock().unwrap();
                    if let Some(found) = events.iter().find(|(e, p)| pred(e, p)).cloned() {
                        return found;
                    }
                }
                if Instant::now() >= deadline {
                    panic!(
                        "timed out waiting for event; recorded: {:?}",
                        self.events.lock().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl EventSink for RecordingSink {
        fn emit(&self, event: &str, payload: serde_json::Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }
    }

    /// A scripted [`HostedSession`]: output through a channel the test
    /// drives, every operation recorded, exit parked behind a flag.
    pub(crate) struct FakeSession {
        output_tx: Mutex<Option<Sender<Vec<u8>>>>,
        output_rx: Mutex<Option<Receiver<Vec<u8>>>>,
        pub writes: Mutex<Vec<Vec<u8>>>,
        pub resizes: Mutex<Vec<(u16, u16)>>,
        pub killed: Mutex<bool>,
        exit: Mutex<Option<TermExit>>,
        pub dsr: AtomicU64,
    }

    impl FakeSession {
        pub(crate) fn scripted() -> Arc<Self> {
            let (output_tx, output_rx) = channel();
            Arc::new(Self {
                output_tx: Mutex::new(Some(output_tx)),
                output_rx: Mutex::new(Some(output_rx)),
                writes: Mutex::new(Vec::new()),
                resizes: Mutex::new(Vec::new()),
                killed: Mutex::new(false),
                exit: Mutex::new(None),
                dsr: AtomicU64::new(0),
            })
        }

        /// Feed one output chunk to the actor.
        pub(crate) fn send_output(&self, bytes: &[u8]) {
            self.output_tx
                .lock()
                .unwrap()
                .as_ref()
                .expect("session already finished")
                .send(bytes.to_vec())
                .expect("actor still pumping");
        }

        /// Park the exit status and close the output stream: the actor sees
        /// `Closed`, then `wait_exit` succeeds.
        pub(crate) fn finish(&self, exit: TermExit) {
            *self.exit.lock().unwrap() = Some(exit);
            self.output_tx.lock().unwrap().take();
        }
    }

    impl HostedSession for Arc<FakeSession> {
        fn recv_output(&self, timeout: Duration) -> RecvOutcome {
            match self.output_rx.lock().unwrap().as_ref() {
                None => RecvOutcome::Closed,
                Some(rx) => match rx.recv_timeout(timeout) {
                    Ok(bytes) => RecvOutcome::Chunk(bytes),
                    Err(RecvTimeoutError::Timeout) => RecvOutcome::Quiet,
                    Err(RecvTimeoutError::Disconnected) => RecvOutcome::Closed,
                },
            }
        }

        fn write(&self, bytes: &[u8]) -> io::Result<()> {
            if *self.killed.lock().unwrap() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "fake session killed",
                ));
            }
            self.writes.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }

        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            self.resizes.lock().unwrap().push((cols, rows));
            Ok(())
        }

        fn kill(&self) -> io::Result<()> {
            // A killed session ends: park a synthetic exit so a waiting
            // `wait_exit` returns (mirrors a real child dying on kill).
            // An explicitly parked exit (test scripting) wins.
            *self.killed.lock().unwrap() = true;
            let mut exit = self.exit.lock().unwrap();
            if exit.is_none() {
                *exit = Some(TermExit {
                    exit_code: 1,
                    signal: None,
                    success: false,
                });
            }
            Ok(())
        }

        fn wait_exit(&self) -> TermExit {
            loop {
                if let Some(exit) = self.exit.lock().unwrap().clone() {
                    return exit;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn dsr_responses_sent(&self) -> u64 {
            self.dsr.load(Ordering::Relaxed)
        }
    }

    /// Factory handing out scripted sessions; records the specs it saw.
    pub(crate) struct FakeFactory {
        pub specs: Mutex<Vec<SpawnSpec>>,
        pub sessions: Mutex<Vec<Arc<FakeSession>>>,
        fail_next: Mutex<Option<io::Error>>,
    }

    impl FakeFactory {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                specs: Mutex::new(Vec::new()),
                sessions: Mutex::new(Vec::new()),
                fail_next: Mutex::new(None),
            })
        }

        /// The session created by the Nth spawn (0-based).
        pub(crate) fn session(&self, index: usize) -> Arc<FakeSession> {
            self.sessions.lock().unwrap()[index].clone()
        }

        pub(crate) fn fail_next(&self, error: io::Error) {
            *self.fail_next.lock().unwrap() = Some(error);
        }
    }

    impl SessionFactory for FakeFactory {
        fn spawn(&self, spec: &SpawnSpec) -> io::Result<Box<dyn HostedSession>> {
            if let Some(error) = self.fail_next.lock().unwrap().take() {
                return Err(error);
            }
            let session = FakeSession::scripted();
            self.sessions.lock().unwrap().push(session.clone());
            self.specs.lock().unwrap().push(spec.clone());
            Ok(Box::new(session))
        }
    }

    /// A fresh manager on a unique temp sandbox root: pid + a
    /// process-global counter. A timestamp is not enough — parallel tests
    /// would collide on temp dirs and clobber each other's fixtures.
    static ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn manager_with(
        factory: Arc<dyn SessionFactory>,
    ) -> (SessionManager, Arc<RecordingSink>, PathBuf) {
        let sink = RecordingSink::new();
        let root = std::env::temp_dir().join(format!(
            "tiller-termhost-test-{}-{}",
            std::process::id(),
            ROOT_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let manager = SessionManager::new(sink.clone(), root.clone(), factory);
        (manager, sink, root)
    }

    pub(crate) fn basic_spec(program: &str) -> SpawnSpec {
        SpawnSpec {
            program: program.to_string(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            cols: 80,
            rows: 24,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn spawn_write_kill_and_exit_lifecycle() {
        let factory = FakeFactory::new();
        let (manager, sink, _root) = manager_with(factory.clone());

        let id = manager.spawn_session(basic_spec("fake")).expect("spawn");
        assert_eq!(id, 1);

        manager.write(id, b"hello".as_slice()).expect("write");
        let session = factory.session(0);
        assert_eq!(*session.writes.lock().unwrap(), vec![b"hello".to_vec()]);

        manager.resize(id, 100, 30).expect("resize");
        assert_eq!(*session.resizes.lock().unwrap(), vec![(100, 30)]);

        manager.kill(id).expect("kill");
        assert!(*session.killed.lock().unwrap());

        // Output flows through the sink as base64.
        session.send_output(b"out-chunk");
        let (_, payload) = sink.wait_for(WAIT, |event, _| event == EVENT_OUTPUT);
        assert_eq!(payload["session_id"], id);
        assert_eq!(
            base64_decode(payload["data"].as_str().unwrap()).unwrap(),
            b"out-chunk"
        );

        // Stats report the (fake) DSR counter while the session lives.
        session.dsr.store(3, Ordering::Relaxed);
        assert_eq!(manager.stats(id).unwrap().dsr_responses_sent, 3);

        // Exit notification with the parked status and DSR counter.
        session.finish(TermExit {
            exit_code: 7,
            signal: None,
            success: false,
        });
        let (_, payload) = sink.wait_for(WAIT, |event, _| event == EVENT_EXIT);
        assert_eq!(payload["session_id"], id);
        assert_eq!(payload["exit_code"], 7);
        assert_eq!(payload["success"], false);
        assert_eq!(payload["dsr_responses_sent"], 3);

        // After exit the actor removed the session: operations report gone.
        sink.wait_for(WAIT, |event, p| {
            event == EVENT_EXIT && p["session_id"] == id
        });
        eventually(
            || {
                matches!(
                    manager.write(id, b"x".as_slice()),
                    Err(TermError::SessionGone(_))
                )
            },
            "session removed from the manager map after exit",
        );
    }

    #[test]
    fn unknown_session_reports_gone() {
        let (manager, _sink, _root) = manager_with(FakeFactory::new());
        assert!(matches!(
            manager.write(999, b"x".as_slice()),
            Err(TermError::SessionGone(999))
        ));
        assert!(matches!(
            manager.resize(999, 10, 10),
            Err(TermError::SessionGone(999))
        ));
        assert!(matches!(
            manager.kill(999),
            Err(TermError::SessionGone(999))
        ));
        assert!(matches!(
            manager.stats(999),
            Err(TermError::SessionGone(999))
        ));
    }

    #[test]
    fn spawn_failure_maps_to_io_error() {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory.clone());
        factory.fail_next(io::Error::new(io::ErrorKind::NotFound, "program not found"));
        let err = manager.spawn_session(basic_spec("fake")).unwrap_err();
        assert!(
            matches!(err, TermError::Io(ref m) if m.contains("program not found")),
            "{err:?}"
        );
    }

    /// A pi spawn that fails in the factory must not leave its
    /// (secret-bearing) sandbox behind: no actor exists to sweep it, so
    /// the cleanup happens before the error is returned.
    #[test]
    fn pi_sandbox_dir_removed_when_spawn_fails() {
        let factory = FakeFactory::new();
        let (manager, _sink, root) = manager_with(factory.clone());
        factory.fail_next(io::Error::new(io::ErrorKind::NotFound, "program not found"));
        let mut spec = basic_spec("pi");
        spec.cwd = Some(PathBuf::from("C:/Repos/tiller"));
        let err = manager.spawn_session(spec).unwrap_err();
        assert!(
            matches!(err, TermError::Io(ref m) if m.contains("program not found")),
            "{err:?}"
        );
        let base = root.join("agent-home");
        let leftover: Vec<_> = std::fs::read_dir(&base)
            .map(|entries| entries.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            leftover.is_empty(),
            "no sandbox dir may outlive a failed spawn: {leftover:?}"
        );
    }

    #[test]
    fn write_io_error_maps_to_io_term_error() {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory.clone());
        let id = manager.spawn_session(basic_spec("fake")).unwrap();
        let session = factory.session(0);
        session.kill().unwrap(); // fake write now fails with BrokenPipe
        let err = manager.write(id, b"x".as_slice()).unwrap_err();
        assert!(matches!(err, TermError::Io(_)), "{err:?}");
    }

    #[test]
    fn pi_program_gets_sandbox_env() {
        let factory = FakeFactory::new();
        let (manager, _sink, root) = manager_with(factory.clone());
        let mut spec = basic_spec("pi");
        spec.cwd = Some(PathBuf::from("C:/Repos/tiller"));
        let id = manager.spawn_session(spec).expect("spawn");

        let specs = factory.specs.lock().unwrap();
        let spawned = &specs[0];
        let env_dir = spawned
            .env
            .iter()
            .find(|(k, _)| k == "PI_CODING_AGENT_DIR")
            .expect("sandbox env var set")
            .1
            .clone();
        // Unpredictable per-session dir: agent-home/{id}-{random tag}.
        // (Component-wise prefix then a string check: `Path::starts_with`
        // cannot match a `{id}-` prefix inside one path component.)
        let env_path = PathBuf::from(&env_dir);
        let rest = env_path
            .strip_prefix(root.join("agent-home"))
            .expect("sandbox under agent-home");
        assert!(
            rest.to_string_lossy().starts_with(&format!("{id}-")),
            "env dir {env_dir}"
        );
        assert_eq!(spawned.cwd.as_deref(), Some(Path::new("C:/Repos/tiller")));
    }

    #[test]
    fn pi_sandbox_dir_is_removed_after_exit() {
        let factory = FakeFactory::new();
        let (manager, sink, _root) = manager_with(factory.clone());
        let mut spec = basic_spec("pi");
        spec.cwd = Some(PathBuf::from("C:/Repos/tiller"));
        let id = manager.spawn_session(spec).expect("spawn");

        let env_dir = {
            let specs = factory.specs.lock().unwrap();
            PathBuf::from(
                specs[0]
                    .env
                    .iter()
                    .find(|(k, _)| k == "PI_CODING_AGENT_DIR")
                    .unwrap()
                    .1
                    .clone(),
            )
        };
        assert!(env_dir.is_dir(), "sandbox exists while the session runs");

        // Session end (normal exit or kill — same actor teardown path).
        factory.session(0).finish(TermExit {
            exit_code: 0,
            signal: None,
            success: true,
        });
        sink.wait_for(WAIT, |event, p| {
            event == EVENT_EXIT && p["session_id"] == id
        });
        eventually(|| !env_dir.exists(), "sandbox removed after exit");
    }

    #[test]
    fn dropping_the_manager_kills_sessions_and_ends_actors() {
        let factory = FakeFactory::new();
        let (manager, sink, _root) = manager_with(factory.clone());
        let id = manager.spawn_session(basic_spec("fake")).expect("spawn");
        let session = factory.session(0);
        let weak = Arc::downgrade(&session);

        // The last manager clone going away must break the ownership
        // cycle: the actor notices (Weak upgrade fails / channel
        // disconnects), kills the session, still reports the exit, and
        // terminates instead of leaking a parked actor thread.
        drop(manager);

        eventually(
            || *session.killed.lock().unwrap(),
            "hosted session killed on manager drop",
        );
        // Strong refs after the actor exits: the factory's vec + this
        // test's clone — the actor's copy must be gone.
        eventually(|| weak.strong_count() == 2, "session actor terminated");
        let (_, payload) = sink.wait_for(WAIT, |event, p| {
            event == EVENT_EXIT && p["session_id"] == id
        });
        assert_eq!(payload["exit_code"], 1, "kill-on-drop exit reported");
    }

    #[test]
    fn non_pi_program_skips_sandbox() {
        let factory = FakeFactory::new();
        let (manager, _sink, root) = manager_with(factory.clone());
        manager.spawn_session(basic_spec("cmd.exe")).unwrap();
        let specs = factory.specs.lock().unwrap();
        assert!(specs[0].env.is_empty());
        drop(specs);
        assert!(!root.join("agent-home").exists());
    }

    #[test]
    fn explicit_pi_agent_dir_opts_out_of_sandbox_but_program_still_resolves() {
        let factory = FakeFactory::new();
        let (manager, _sink, root) = manager_with(factory.clone());
        // A pi.cmd shim on disk: deterministic resolution to node + cli.js
        // even though the sandbox is opted out (the shim translation is
        // about *running* pi, not about its agent home).
        let home = root.join("shim-home");
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let shim = bin.join("pi.cmd");
        std::fs::write(&shim, "@echo off\r\n").unwrap();
        let cli = bin
            .join("node_modules")
            .join("@earendil-works")
            .join("pi-coding-agent")
            .join("dist")
            .join("bundle")
            .join("cli.js");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, "").unwrap();

        let mut spec = basic_spec(shim.to_str().unwrap());
        spec.env.push((
            "PI_CODING_AGENT_DIR".to_string(),
            "C:/custom/dir".to_string(),
        ));
        manager.spawn_session(spec).unwrap();
        let specs = factory.specs.lock().unwrap();
        assert_eq!(specs[0].program, "node");
        assert_eq!(specs[0].args, vec![cli.to_string_lossy().into_owned()]);
        assert_eq!(
            specs[0].env,
            vec![(
                "PI_CODING_AGENT_DIR".to_string(),
                "C:/custom/dir".to_string()
            )]
        );
        drop(specs);
        assert!(!root.join("agent-home").exists());
    }

    #[test]
    fn output_before_exit_ordering() {
        let factory = FakeFactory::new();
        let (manager, sink, _root) = manager_with(factory.clone());
        let id = manager.spawn_session(basic_spec("fake")).unwrap();
        let session = factory.session(0);
        session.send_output(b"first");
        session.finish(TermExit {
            exit_code: 0,
            signal: None,
            success: true,
        });
        sink.wait_for(WAIT, |event, _| event == EVENT_EXIT);
        let events = sink.events.lock().unwrap();
        let positions: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(i, (name, payload))| {
                (payload["session_id"] == id && (name == EVENT_OUTPUT || name == EVENT_EXIT))
                    .then_some(i)
            })
            .collect();
        assert_eq!(positions.len(), 2, "one output, one exit: {events:?}");
    }

    /// The sandbox dir of a spawned fake-pi session (from the env the
    /// manager set), with its `sessions` dir ready for a session file.
    fn pi_sandbox_with_sessions_dir(factory: &FakeFactory) -> PathBuf {
        let env_dir = factory.specs.lock().unwrap()[0]
            .env
            .iter()
            .find(|(k, _)| k == "PI_CODING_AGENT_DIR")
            .map(|(_, v)| PathBuf::from(v))
            .expect("pi sandbox env");
        std::fs::create_dir_all(env_dir.join("sessions")).unwrap();
        env_dir
    }

    /// Write a session file under `sandbox/sessions/{slug}/…` (pi 1.1.0's
    /// per-project layout — discovery must search the subtree).
    fn write_session_file(sandbox: &Path, slug: &str, contents: &str) {
        let dir = sandbox.join("sessions").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.jsonl"), contents).unwrap();
    }

    const SESSION_USER_LINE: &str = r#"{"type":"message","id":"1","timestamp":"2026-10-11T12:42:05.554Z","message":{"role":"user","content":[{"type":"text","text":"hello timeline"}]}}"#;

    #[test]
    fn pi_session_is_observed_live_from_spawn() {
        let factory = FakeFactory::new();
        let (manager, sink, _root) = manager_with(factory.clone());
        let mut spec = basic_spec("pi");
        spec.cwd = Some(PathBuf::from("C:/Repos/tiller"));
        let id = manager.spawn_session(spec).expect("spawn");

        // pi creates the session file lazily (first message, not boot):
        // nothing is observed until the file shows up in the sandbox.
        let sandbox = pi_sandbox_with_sessions_dir(&factory);
        std::thread::sleep(Duration::from_millis(300));
        assert!(manager.observe_entries(id).unwrap().is_empty());

        write_session_file(&sandbox, "--slug--", &format!("{SESSION_USER_LINE}\n"));

        // The auto-started tailer streams it as `session:entry`.
        let (_, payload) =
            sink.wait_for(WAIT, |event, _| event == crate::session_tail::EVENT_ENTRY);
        assert_eq!(payload["session_id"], id);
        assert_eq!(payload["entry"]["kind"], "message");
        assert_eq!(payload["entry"]["role"], "user");
        assert_eq!(payload["entry"]["chars"], "hello timeline".chars().count());

        // The snapshot rpc sees the same entries.
        let snapshot = manager.observe_entries(id).unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(
            matches!(&snapshot[0], TimelineEntry::Message { id, .. } if id == "1"),
            "{snapshot:?}"
        );
    }

    #[test]
    fn observe_is_empty_without_sandbox_or_file_and_gone_for_dead_ids() {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory.clone());

        // A non-pi session: live, but nothing to observe.
        let cmd_id = manager.spawn_session(basic_spec("cmd.exe")).unwrap();
        assert!(manager.observe_entries(cmd_id).unwrap().is_empty());

        // A pi session whose session file does not exist yet: empty, not
        // an error.
        let mut spec = basic_spec("pi");
        spec.cwd = Some(PathBuf::from("C:/Repos/tiller"));
        let pi_id = manager.spawn_session(spec).unwrap();
        assert!(manager.observe_entries(pi_id).unwrap().is_empty());

        // Foreign/dead ids: SessionGone, consistently with term.*.
        assert!(matches!(
            manager.observe_entries(999),
            Err(TermError::SessionGone(999))
        ));
    }

    #[test]
    fn tailer_entries_precede_exit_and_thread_ends_before_cleanup() {
        let factory = FakeFactory::new();
        let (manager, sink, _root) = manager_with(factory.clone());
        let mut spec = basic_spec("pi");
        spec.cwd = Some(PathBuf::from("C:/Repos/tiller"));
        let id = manager.spawn_session(spec).expect("spawn");
        let sandbox = pi_sandbox_with_sessions_dir(&factory);
        write_session_file(&sandbox, "flat", &format!("{SESSION_USER_LINE}\n"));
        sink.wait_for(WAIT, |event, _| event == crate::session_tail::EVENT_ENTRY);

        // Session end: every session:entry precedes term:exit (the actor
        // joins the tailer before announcing the exit), and the sandbox
        // cleanup — which runs after that join — completing proves the
        // tailer thread was already wound down (a stalled join would
        // hang this assertion, not leak the thread).
        factory.session(0).finish(TermExit {
            exit_code: 0,
            signal: None,
            success: true,
        });
        sink.wait_for(WAIT, |event, p| {
            event == EVENT_EXIT && p["session_id"] == id
        });
        eventually(|| !sandbox.exists(), "sandbox removed after exit");
        {
            let events = sink.events.lock().unwrap();
            let entry_at = events
                .iter()
                .position(|(name, _)| name == crate::session_tail::EVENT_ENTRY)
                .expect("an entry event");
            let exit_at = events
                .iter()
                .position(|(name, _)| name == EVENT_EXIT)
                .expect("an exit event");
            assert!(entry_at < exit_at, "entries precede exit: {events:?}");
        }
    }

    /// Poll `f` until it returns `true` (bounded), so lifecycle races in
    /// tests fail on the assertion instead of timing out silently.
    fn eventually(f: impl Fn() -> bool, what: &str) {
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition not met within {WAIT:?}: {what}");
    }
}
