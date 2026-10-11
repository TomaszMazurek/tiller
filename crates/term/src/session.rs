//! The PTY session: spawn, output pump, serialized write/resize with a
//! DSR-priority lane, bounded output buffer, kill with escalation, exit.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, ExitStatus, PtySize};

use crate::dsr::DsrResponder;
use crate::outbuf::{output_buffer, OutputReceiver, OUTPUT_BUFFER_CAP};
use crate::writer::{run_owner, MasterSlot, MasterWriteEnd, OwnerCommand};

/// Grace period between the initial kill signal and the SIGKILL escalation
/// (Unix): a well-behaved program gets two seconds to honor the polite
/// signal before the session stops asking.
#[cfg(unix)]
const KILL_ESCALATION_GRACE: Duration = Duration::from_secs(2);

/// Reaper poll cadence (Unix): the reaper cannot block in `wait()` and
/// still decide the SIGKILL escalation, so it polls `try_wait()` (WNOHANG
/// underneath) at this interval. The cadence bounds both the exit-status
/// latency after a normal exit and the extra delay added on top of
/// [`KILL_ESCALATION_GRACE`] to a single interval.
#[cfg(unix)]
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Configuration for [`PtySession::spawn`].
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Program to run (resolved by the caller; `.cmd` shims need translation
    /// to `node <cli.js>` — Spike 1 pitfall 2, handled by callers that host
    /// npm-based agents).
    pub program: String,
    pub args: Vec<String>,
    /// Environment entries set on top of the inherited environment. When no
    /// `TERM` entry is present the session sets `xterm-256color`, matching
    /// the terminal class the DSR responder speaks for.
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
}

/// Where a session thread park's the child's exit status once reaped.
#[derive(Default)]
struct ExitSlot {
    status: Mutex<Option<ExitStatus>>,
    ready: Condvar,
}

impl ExitSlot {
    fn set(&self, status: ExitStatus) {
        let mut guard = self.status.lock().unwrap();
        *guard = Some(status);
        self.ready.notify_all();
    }

    fn get(&self) -> Option<ExitStatus> {
        self.status.lock().unwrap().clone()
    }

    fn wait(&self) -> ExitStatus {
        let mut guard = self.status.lock().unwrap();
        loop {
            if let Some(status) = guard.clone() {
                return status;
            }
            guard = self.ready.wait(guard).unwrap();
        }
    }
}

/// A program hosted in a pseudo-terminal.
///
/// Threading layout (all owned by [`PtySession::spawn`], released on drop):
///
/// - **output pump** — the only reader of the PTY; feeds each chunk through
///   the [`DsrResponder`], queues DSR replies to the writer owner's
///   *priority lane* (ahead of any bulk input still being written), and
///   forwards the remaining bytes into the bounded [`OutputReceiver`]
///   (backpressure, no-loss, memory O(cap));
/// - **writer owner** — the only code touching the PTY write end; serializes
///   input writes with resizes (Spike 1 resize↔input race), answers all
///   pending DSR replies before and between 64 KiB input chunks so a reply
///   is never starved behind a burst. Resizes reach the master through a
///   shared [`MasterSlot`].
/// - **reaper** — waits for the hosted program to exit, parks the exit
///   status, then *closes the master*. That last step is load-bearing on
///   ConPTY: the output pipe stays open while the pseudoconsole lives, so
///   without the close a reader parked in `read()` never sees EOF after
///   the program exits (bisect: control and writer-close hang; master-close
///   EOFs ~20ms after exit with the buffered output intact). On Unix the
///   reaper polls `try_wait()` instead of blocking so that it — and only
///   it — can also serve as the kill-escalation decider (next bullet);
///   elsewhere it simply blocks in `child.wait()`.
/// - **kill escalation** (Unix) — armed by `kill()`/drop, decided by the
///   *reaper thread*: once [`KILL_ESCALATION_GRACE`] has passed since the
///   polite signal, the reaper confirms liveness with a fresh `try_wait()`
///   and sends the SIGKILL inside that same window. `try_wait() == Ok(None)`
///   on the only thread that reaps means the process is running and
///   un-reaped, and an un-reaped child keeps its pid reserved (even as a
///   zombie) — so the signal can never land on a pid the OS has already
///   recycled into an unrelated process. A probe *error* (`Err`) proves
///   nothing about the pid and never opens the signal window; the loop
///   defers to the blocking `wait()` instead. The reaper then blocks in
///   `wait()` for the status it just forced, so kill/drop always winds
///   down the session threads and the master.
pub struct PtySession {
    output: OutputReceiver,
    owner: Sender<OwnerCommand>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    exit: Arc<ExitSlot>,
    dsr_responses_sent: Arc<AtomicU64>,
    output_error: Arc<Mutex<Option<String>>>,
    process_id: Option<u32>,
    /// When the polite kill was requested (Unix): anchors the reaper's
    /// escalation deadline to the *first* request — a repeat `kill()` does
    /// not extend the grace. Read by the reaper thread (the single
    /// escalation decider) while it polls.
    #[cfg(unix)]
    kill_request: Arc<Mutex<Option<Instant>>>,
}

impl PtySession {
    /// Spawn a program in a fresh PTY with the given size and environment.
    pub fn spawn(cfg: SpawnConfig) -> io::Result<PtySession> {
        let pty_system = native_pty_system();
        let size = PtySize {
            rows: cfg.rows,
            cols: cfg.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let mut cmd = CommandBuilder::new(&cfg.program);
        cmd.args(cfg.args.iter().map(String::as_str));
        if let Some(cwd) = &cfg.cwd {
            cmd.cwd(cwd);
        }
        let mut has_term = false;
        for (key, value) in &cfg.env {
            cmd.env(key, value);
            has_term |= key == "TERM";
        }
        if !has_term {
            cmd.env("TERM", "xterm-256color");
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| io::Error::other(e.to_string()))?;
        drop(pair.slave); // close the slave so it cannot keep the PTY alive
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let killer = child.clone_killer();
        let process_id = child.process_id();

        // The master's lifetime belongs to the hosted program: the writer
        // owner borrows it for resizes, the reaper takes it away when the
        // program exits (which is what delivers EOF to the output pump).
        let master: MasterSlot = Arc::new(Mutex::new(Some(pair.master)));

        let (out_tx, out_rx) = output_buffer(OUTPUT_BUFFER_CAP);
        let (owner_tx, owner_rx) = channel();
        let (priority_tx, priority_rx) = channel::<Vec<u8>>();
        let exit = Arc::new(ExitSlot::default());
        let dsr_responses_sent = Arc::new(AtomicU64::new(0));
        let output_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        // Unix: when the polite kill was requested — read by the reaper,
        // the single escalation decider (see [`reap_child`]).
        #[cfg(unix)]
        let kill_request: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

        // Writer owner: the only code touching the PTY write end. The DSR
        // priority lane is drained ahead of input/resize commands and
        // between input chunks (see the writer module docs).
        let write_end = MasterWriteEnd::new(writer, Arc::clone(&master));
        thread::Builder::new()
            .name("tiller-term-owner".into())
            .spawn(move || run_owner(write_end, priority_rx, owner_rx))
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Reaper: parks the exit status once the child terminates, then
        // closes the master so the pump's reader reaches EOF. Order matters:
        // the status is published first, so `wait()` callers never depend on
        // the close. On Unix the reaper polls `try_wait()` instead of
        // blocking so that it can also decide the SIGKILL escalation — see
        // [`reap_child`]. `take()` moves the Box out under the lock and drops it
        // after releasing it, so a resize in flight on the owner thread is
        // never dropped mid-call — it either completes or reports
        // `NotConnected` on the next attempt.
        let exit_slot = Arc::clone(&exit);
        let reaper_master = Arc::clone(&master);
        #[cfg(unix)]
        let reaper_kill_request = Arc::clone(&kill_request);
        #[cfg(unix)]
        let reaper_pid = process_id;
        thread::Builder::new()
            .name("tiller-term-reaper".into())
            .spawn(move || {
                // A wait error is exotic (child already reaped elsewhere);
                // it still has to unblock `wait()` with a failed status.
                #[cfg(unix)]
                let status = reap_child(child, &reaper_kill_request, reaper_pid);
                #[cfg(not(unix))]
                let status = reap_child(child);
                exit_slot.set(status);
                drop(reaper_master.lock().unwrap().take());
            })
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Output pump: the only reader of the PTY. Feeds chunks through the
        // DSR responder, queues replies on the owner's *priority* lane (an
        // unanswered probe can be the thing keeping the input pipe from
        // draining, so replies must not queue behind bulk input), forwards
        // the rest into the bounded output buffer. A dropped consumer does
        // not stop DSR answering — the session is the terminal regardless
        // of who is watching — but it does end the pump (the buffer reports
        // abandonment, threads wind down).
        let pump_owner = owner_tx.clone();
        let pump_priority = priority_tx;
        let pump_dsr = Arc::clone(&dsr_responses_sent);
        let pump_error = Arc::clone(&output_error);
        thread::Builder::new()
            .name("tiller-term-pump".into())
            .spawn(move || {
                let mut responder = DsrResponder::new();
                let mut buf = [0u8; 16384];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF: the program side is gone
                        Ok(n) => {
                            let out = responder.feed(&buf[..n]);
                            if out.responses > 0 {
                                pump_dsr.fetch_add(out.responses as u64, Ordering::Relaxed);
                                // Reply first, then the wake-up sentinel: the
                                // sentinel handler (and every between-chunk
                                // drain) then always observes the reply.
                                let _ = pump_priority.send(out.reply);
                                let _ = pump_owner.send(OwnerCommand::CheckPriority);
                            }
                            if !out_tx.push(out.forward) {
                                break; // consumer gone: stop pumping
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            *pump_error.lock().unwrap() = Some(e.to_string());
                            break;
                        }
                    }
                }
            })
            .map_err(|e| io::Error::other(e.to_string()))?;

        Ok(PtySession {
            output: out_rx,
            owner: owner_tx,
            killer: Mutex::new(killer),
            exit,
            dsr_responses_sent,
            output_error,
            process_id,
            #[cfg(unix)]
            kill_request,
        })
    }

    /// The session's output stream (bytes with DSR probes consumed).
    ///
    /// Bounded at ~4 MiB of queued bytes: a consumer must keep draining or
    /// the hosted program eventually stalls in its own writes (intended
    /// backpressure — no data is lost). A single consumer by design; P3
    /// fans out to UI and observers above this seam.
    pub fn output(&self) -> &OutputReceiver {
        &self.output
    }

    /// Process id of the hosted program, if the platform reports one.
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    /// Write input bytes to the PTY, fully drained.
    ///
    /// Blocks until every byte is written and flushed (partial writes and
    /// `WouldBlock` are retried; nothing is dropped under backpressure).
    /// Large inputs land in 64 KiB chunks with pending DSR replies flushed
    /// between chunks (see the writer module docs). Returns an error when
    /// the writer owner is gone (session torn down).
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let (reply_tx, reply_rx) = channel();
        self.owner
            .send(OwnerCommand::Input {
                bytes: bytes.to_vec(),
                reply: Some(reply_tx),
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pty writer owner stopped"))?;
        reply_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pty writer owner stopped"))?
    }

    /// Resize the PTY. Returns the real outcome of the underlying resize —
    /// the control ack callers assert on (Spike 1 e2e asserted
    /// `{"t":"ctl","ok":true}`; here the ack is the function result, and
    /// correlating it with a request id across a transport is P3).
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.owner
            .send(OwnerCommand::Resize {
                cols,
                rows,
                reply: reply_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pty writer owner stopped"))?;
        reply_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pty writer owner stopped"))?
    }

    /// Terminate the hosted program.
    ///
    /// Contract — best-effort *initiation* plus a reaper-decided escalation
    /// on Unix: the initial signal is the platform killer's (SIGHUP through
    /// portable-pty's Unix killer; TerminateProcess on Windows, where 0.9's
    /// killer inverts `TerminateProcess`'s success check so its `Result` is
    /// untrustworthy — best-effort there, unchanged). A program that
    /// survives the initial signal (Unix: traps/ignores HUP) is SIGKILLed
    /// by the *reaper thread* [`KILL_ESCALATION_GRACE`] after this request:
    /// the reaper is the single escalation decider, and it confirms
    /// liveness with `try_wait()` in the same breath as the signal, so the
    /// SIGKILL can only ever hit the hosted child — a pid the OS has
    /// already recycled cannot be signaled through this path (see the
    /// session docs). SIGKILL cannot be trapped, so after `kill()`/drop the
    /// session always eventually winds down: the reaper parks the status
    /// and closes the master, giving any remaining output reader EOF. The
    /// authoritative outcome on every platform is the reaper: `wait()` /
    /// `try_wait()` report the program's actual exit status. A child the
    /// reaper already reaped is not signaled at all — it is dead, and its
    /// pid may have been recycled by then.
    pub fn kill(&self) -> io::Result<()> {
        // A reaped child's pid may already belong to an unrelated process:
        // once the reaper published a status, no signal goes out at all.
        if self.exit.get().is_some() {
            return Ok(());
        }
        let result = self.killer.lock().unwrap().kill();
        #[cfg(unix)]
        self.note_kill_request();
        if cfg!(windows) {
            Ok(())
        } else {
            result
        }
    }

    /// Anchor the escalation deadline to this request (Unix): the reaper
    /// thread reads it while polling. The first request wins — a repeat
    /// `kill()` never extends the grace.
    #[cfg(unix)]
    fn note_kill_request(&self) {
        let mut requested = self.kill_request.lock().unwrap();
        requested.get_or_insert_with(Instant::now);
    }

    /// Block until the hosted program exits; safe to call repeatedly.
    pub fn wait(&self) -> ExitStatus {
        self.exit.wait()
    }

    /// Exit status if the program has already terminated.
    pub fn try_wait(&self) -> Option<ExitStatus> {
        self.exit.get()
    }

    /// How many DSR probe replies this session injected (diagnostic counter).
    pub fn dsr_responses_sent(&self) -> u64 {
        self.dsr_responses_sent.load(Ordering::Relaxed)
    }

    /// Why the output pump stopped, if it stopped on a read error. A clean
    /// stop (program exit, EOF) surfaces as the output channel disconnecting
    /// instead.
    pub fn output_error(&self) -> Option<String> {
        self.output_error.lock().unwrap().clone()
    }
}

/// The reaper's view of the hosted child (Unix): the wait operations the
/// escalation decision needs, split from portable-pty's `Child` so the
/// decision loop ([`run_reap_loop`]) is unit-testable against a fake
/// child and clock.
#[cfg(unix)]
trait ReapChild {
    /// Non-blocking liveness/status check (WNOHANG underneath).
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    /// Blocking wait — used only right after a SIGKILL, which cannot be
    /// trapped, so it is guaranteed to return.
    fn wait(&mut self) -> io::Result<ExitStatus>;
    /// SIGKILL the child by pid. `false` when no pid is known: nothing to
    /// escalate with, the caller keeps polling instead of parking.
    fn sigkill(&mut self) -> bool;
}

/// Production [`ReapChild`]: the real PTY child plus its pid.
#[cfg(unix)]
struct PtyChild {
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
}

#[cfg(unix)]
impl ReapChild for PtyChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    fn sigkill(&mut self) -> bool {
        let Some(pid) = self.pid else {
            return false;
        };
        // SAFETY: libc::kill signals a numeric pid; it touches no Rust
        // memory and takes no locks. The call happens only in the
        // liveness-confirmed window (see [`run_reap_loop`]), where the pid
        // provably belongs to our un-reaped child; the result is ignored
        // because the reaper stays the authority for the exit status.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        true
    }
}

/// Reaper body on Unix: poll instead of block, and be the *single* SIGKILL
/// escalation decider.
///
/// Why one decider: a watchdog that signals by pid after inspecting a
/// *published* status races the reaper — the child can die, be reaped (its
/// pid recyclable) and be replaced by an unrelated process between the
/// watchdog's check and its `kill(pid)`. Here the check and the signal
/// happen on the one thread that reaps, with nothing in between that could
/// reap the child: `try_wait() == Ok(None)` means the process is running, and
/// until this thread reaps it the pid stays reserved (an exited-but-
/// un-reaped child is a zombie that keeps it), so a SIGKILL sent in that
/// window can only ever hit our own child — there is no reuse window at
/// all.
#[cfg(unix)]
fn reap_child(
    child: Box<dyn Child + Send + Sync>,
    kill_request: &Mutex<Option<Instant>>,
    pid: Option<u32>,
) -> ExitStatus {
    let mut child = PtyChild { child, pid };
    run_reap_loop(&mut child, kill_request, Instant::now, thread::sleep)
}

/// The decision loop, parameterized over child and clock so the escalation
/// state machine is unit-testable (fake child, fake time; production: real
/// child, `Instant::now`, `thread::sleep`).
#[cfg(unix)]
fn run_reap_loop<C, Now, Sleep>(
    child: &mut C,
    kill_request: &Mutex<Option<Instant>>,
    mut now: Now,
    mut sleep: Sleep,
) -> ExitStatus
where
    C: ReapChild,
    Now: FnMut() -> Instant,
    Sleep: FnMut(Duration),
{
    loop {
        // One probe per iteration: its result is the single source the
        // escalation decision and the park/sleep match below both read —
        // a status seen once is parked exactly once, never probed twice
        // and lost.
        let probe = child.try_wait();
        if let Some(requested_at) = *kill_request.lock().unwrap() {
            if now().saturating_duration_since(requested_at) >= KILL_ESCALATION_GRACE {
                match &probe {
                    // The signal window opens ONLY on confirmed liveness.
                    // An Err proves nothing about the pid (the child may be
                    // reaped elsewhere and the pid recycled) — never signal
                    // on it.
                    Ok(None) => {
                        if child.sigkill() {
                            // SIGKILL cannot be trapped: the blocking wait
                            // returns the status we just forced.
                            return child
                                .wait()
                                .unwrap_or_else(|_| ExitStatus::with_exit_code(1));
                        }
                        // No pid known: nothing to escalate with — poll on.
                    }
                    Ok(Some(status)) => return status.clone(),
                    // Exotic (reaped elsewhere): let the blocking wait
                    // arbitrate, exactly like the pre-escalation reaper did.
                    Err(_) => {
                        return child
                            .wait()
                            .unwrap_or_else(|_| ExitStatus::with_exit_code(1))
                    }
                }
            }
        }
        match &probe {
            Ok(Some(status)) => return status.clone(),
            Ok(None) => sleep(REAP_POLL_INTERVAL),
            Err(_) => {
                return child
                    .wait()
                    .unwrap_or_else(|_| ExitStatus::with_exit_code(1))
            }
        }
    }
}

/// Reaper body elsewhere: block for the status. No escalation exists
/// outside Unix (Windows: TerminateProcess, reaper stays the authority).
#[cfg(not(unix))]
fn reap_child(mut child: Box<dyn Child + Send + Sync>) -> ExitStatus {
    child
        .wait()
        .unwrap_or_else(|_| ExitStatus::with_exit_code(1))
}

impl Drop for PtySession {
    /// Teardown contract: best-effort terminate ([`PtySession::kill`]), with
    /// the reaper escalating to SIGKILL after the grace on Unix. After a
    /// drop the session always winds down in bounded time: the reaper parks
    /// the status and closes the master (EOF for any remaining reader), the
    /// pump unblocks once the output consumer is gone, and the owner exits
    /// when the pump's command-sender drops.
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_clone_and_debug() {
        let cfg = SpawnConfig {
            program: "cmd.exe".into(),
            args: vec!["/c".into()],
            env: vec![("FOO".into(), "bar".into())],
            cwd: None,
            cols: 80,
            rows: 24,
        };
        let clone = cfg.clone();
        assert_eq!(clone.program, "cmd.exe");
        assert!(format!("{cfg:?}").contains("SpawnConfig"));
    }

    /// Fake child for the escalation state machine: counts try_wait polls,
    /// records every SIGKILL attempt, either self-exits after a chosen
    /// number of polls or runs until killed.
    #[cfg(unix)]
    struct FakeChild {
        /// Poll count at which the process self-exits; `None` = runs until
        /// killed.
        self_exit_after_polls: Option<usize>,
        /// Poll count at which probing starts failing; `None` = never.
        probe_err_after_polls: Option<usize>,
        polls: usize,
        sigkills: usize,
        /// When `false`, `sigkill` reports "no pid known".
        has_pid: bool,
        status: Option<ExitStatus>,
    }

    #[cfg(unix)]
    impl FakeChild {
        fn new(has_pid: bool, self_exit_after_polls: Option<usize>) -> Self {
            Self {
                self_exit_after_polls,
                probe_err_after_polls: None,
                polls: 0,
                sigkills: 0,
                has_pid,
                status: None,
            }
        }
    }

    #[cfg(unix)]
    impl ReapChild for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.polls += 1;
            if self
                .probe_err_after_polls
                .is_some_and(|after| self.polls >= after)
            {
                // ECHILD-style: the probe fails, proving nothing about the
                // pid (e.g. reaped elsewhere).
                return Err(io::Error::from_raw_os_error(10));
            }
            if self
                .self_exit_after_polls
                .is_some_and(|after| self.polls >= after)
            {
                let status = ExitStatus::with_exit_code(5);
                self.status = Some(status.clone());
                Ok(Some(status))
            } else {
                Ok(None)
            }
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(self
                .status
                .clone()
                // Distinctive fallback: tests can tell this blocking-wait
                // path from a parked or forced status.
                .unwrap_or_else(|| ExitStatus::with_exit_code(7)))
        }

        fn sigkill(&mut self) -> bool {
            self.sigkills += 1;
            if !self.has_pid {
                return false;
            }
            self.status = Some(ExitStatus::with_signal("Killed"));
            true
        }
    }

    /// A clock that advances one second per call: with the 2 s grace,
    /// escalation is due on the second poll — deterministic without real
    /// sleeping.
    #[cfg(unix)]
    fn stepping_clock() -> impl FnMut() -> Instant {
        let mut tick = 0u64;
        move || {
            tick += 1;
            Instant::now() + Duration::from_secs(tick)
        }
    }

    /// The core guarantee (P1 review): SIGKILL is sent exactly once, only
    /// after the grace, only while a fresh try_wait just confirmed the
    /// child alive (`Ok(None)` — a probe error never opens the window),
    /// child alive — and the returned status is the one the blocking wait
    /// produced after *our* signal.
    #[cfg(unix)]
    #[test]
    fn escalation_sigkills_once_after_grace_while_alive() {
        let request = Mutex::new(Some(Instant::now()));
        let mut child = FakeChild::new(true, None);
        let status = run_reap_loop(&mut child, &request, stepping_clock(), |_| {});
        assert_eq!(child.sigkills, 1, "exactly one SIGKILL, then wait");
        assert_eq!(
            status.signal(),
            Some("Killed"),
            "status is the forced death"
        );
        // Poll 1: pre-grace check. Poll 2: the confirming try_wait that
        // immediately precedes the signal. Nothing runs after the wait.
        assert_eq!(child.polls, 2, "one pre-grace poll, one confirming poll");
    }

    /// No kill request, a self-exiting child: the loop only parks the
    /// status — no escalation is ever consulted.
    #[cfg(unix)]
    #[test]
    fn reaper_parks_self_exit_without_escalation() {
        let request = Mutex::new(None);
        let mut child = FakeChild::new(true, Some(2));
        let status = run_reap_loop(&mut child, &request, stepping_clock(), |_| {});
        assert_eq!(child.sigkills, 0, "no request -> no escalation");
        assert_eq!(status.exit_code(), 5, "the self-exit status is parked");
    }

    /// A child that dies inside the grace window is never signaled: by the
    /// time the grace would elapse, try_wait reports it gone.
    #[cfg(unix)]
    #[test]
    fn child_exiting_during_grace_is_never_sigkilled() {
        let request = Mutex::new(Some(Instant::now()));
        let mut child = FakeChild::new(true, Some(1));
        // A clock frozen at +1 s: the 2 s grace never elapses.
        let status = run_reap_loop(
            &mut child,
            &request,
            || Instant::now() + Duration::from_secs(1),
            |_| {},
        );
        assert_eq!(child.sigkills, 0, "grace not elapsed -> no signal");
        assert_eq!(status.exit_code(), 5);
    }

    /// No pid known: the escalation has nothing to signal, so the loop
    /// keeps polling (and still parks a later self-exit) instead of
    /// parking in a blocking wait forever.
    #[cfg(unix)]
    #[test]
    fn escalation_without_pid_keeps_polling_until_exit() {
        let request = Mutex::new(Some(Instant::now()));
        let mut child = FakeChild::new(false, Some(6));
        // A clock always past the grace: every iteration takes the
        // decision branch, tries to signal, cannot, and polls on. With one
        // probe per iteration and a self-exit at poll 6, the signal is
        // attempted on polls 1–5; poll 6 parks the status.
        let status = run_reap_loop(
            &mut child,
            &request,
            || Instant::now() + Duration::from_secs(5),
            |_| {},
        );
        assert_eq!(
            child.sigkills, 5,
            "escalation attempted on every post-grace poll before the exit"
        );
        assert_eq!(child.polls, 6);
        assert_eq!(status.exit_code(), 5, "the later self-exit is still reaped");
    }

    /// A probe error must never open the signal window: `Err` proves
    /// nothing about the pid (the child may be reaped elsewhere and the
    /// pid recycled) — the loop defers to the blocking wait without
    /// signaling, and that wait's status is what comes back.
    #[cfg(unix)]
    #[test]
    fn probe_error_never_signals_and_defers_to_wait() {
        let request = Mutex::new(Some(Instant::now()));
        let mut child = FakeChild::new(true, None);
        child.probe_err_after_polls = Some(1);
        let status = run_reap_loop(
            &mut child,
            &request,
            || Instant::now() + Duration::from_secs(5),
            |_| {},
        );
        assert_eq!(child.sigkills, 0, "Err must not open the signal window");
        assert_eq!(child.polls, 1, "single probe, then the blocking wait");
        assert_eq!(status.exit_code(), 7, "status comes from the blocking wait");
    }
}
