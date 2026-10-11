//! Unix kill-escalation integration test (runs on the Linux/macOS CI legs;
//! empty on Windows where kill stays best-effort TerminateProcess).
//!
//! portable-pty's Unix killer only sends SIGHUP. A program that traps or
//! ignores HUP survives it, and the pre-fix session leaked on exactly
//! that: the reaper waited forever in `child.wait()`, the master stayed
//! open, the output pump never reached EOF. The session must escalate to
//! SIGKILL (untrappable) after the escalation grace and always wind down.

#![cfg(unix)]

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tiller_term::{PtySession, SpawnConfig};

/// One budget for the whole kill contract (exit status AND reader EOF):
/// the 2 s escalation grace plus reap/master-close slack, with ~5x margin
/// — generous for a kill, far below CI's per-test patience.
const KILL_BUDGET: Duration = Duration::from_secs(10);

/// Drain the output stream until the pump closes it (program exit, EOF).
fn drain_to_close(session: &PtySession, budget: Duration) -> Vec<u8> {
    let deadline = Instant::now() + budget;
    let mut acc = Vec::new();
    loop {
        let Some(remain) = deadline.checked_duration_since(Instant::now()) else {
            panic!(
                "timed out draining output; stream so far: {:?}",
                String::from_utf8_lossy(&acc)
            );
        };
        match session.output().recv_timeout(remain) {
            Ok(bytes) => acc.extend_from_slice(&bytes),
            Err(RecvTimeoutError::Disconnected) => return acc,
            Err(RecvTimeoutError::Timeout) => panic!(
                "timed out draining output; stream so far: {:?}",
                String::from_utf8_lossy(&acc)
            ),
        }
    }
}

/// Wait until the readiness marker appears in the output stream.
///
/// The marker can split across chunk boundaries (`TILLER-REA` + `DY`), so
/// matching happens on accumulated bytes, never per chunk. The deadline
/// bounds the wait; the accumulator keeps only a marker-sized tail, so a
/// chatty program cannot grow it without limit.
fn wait_for_ready(session: &PtySession, deadline: Instant) {
    const MARKER: &[u8] = b"TILLER-READY";
    let mut seen: Vec<u8> = Vec::new();
    loop {
        if seen.windows(MARKER.len()).any(|w| w == MARKER) {
            return;
        }
        match session.output().recv_timeout(Duration::from_millis(200)) {
            Ok(bytes) => {
                seen.extend_from_slice(&bytes);
                // Only a partial marker prefix can still matter; drop the
                // bytes that can no longer complete one.
                if seen.len() > 2 * MARKER.len() {
                    seen.drain(..seen.len() - (MARKER.len() - 1));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session closed before readiness"),
        }
        assert!(Instant::now() < deadline, "bash did not become ready");
    }
}

#[test]
fn kill_escalates_to_sigkill_when_sighup_is_ignored() {
    // `exec sleep` keeps one process (no orphaned child when bash dies);
    // an ignored-signal disposition survives exec, so the sleep ignores
    // the initial SIGHUP exactly like a misbehaving hosted program.
    let session = PtySession::spawn(SpawnConfig {
        program: "bash".into(),
        args: vec![
            "-c".into(),
            "trap '' HUP; echo TILLER-READY; exec sleep 600".into(),
        ],
        env: vec![],
        cwd: None,
        cols: 80,
        rows: 24,
    })
    .expect("spawn bash");
    // Wait for the trap to be installed, so the kill truly hits a
    // HUP-ignoring process instead of a still-booting shell. Accumulates
    // across chunks: the marker may split between reads.
    wait_for_ready(&session, Instant::now() + Duration::from_secs(30));

    session.kill().expect("kill (initial SIGHUP)");
    let killed_at = Instant::now();
    let status = loop {
        if let Some(status) = session.try_wait() {
            break status;
        }
        assert!(
            Instant::now() < killed_at + KILL_BUDGET,
            "SIGHUP-ignoring program did not exit; SIGKILL escalation missing?"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success(), "a killed program reports failure");
    // It ended in *our* SIGKILL, not an exit code the trap-installing bash
    // could have produced: portable-pty reports signal deaths with the
    // signal name (strsignal(9) == "Killed").
    assert_eq!(
        status.signal(),
        Some("Killed"),
        "escalation ended in SIGKILL, got: {status}"
    );
    // The reader must unblock: EOF on the output stream, same budget.
    let remain = (killed_at + KILL_BUDGET).saturating_duration_since(Instant::now());
    drain_to_close(&session, remain);
    assert!(
        killed_at.elapsed() < KILL_BUDGET,
        "status and EOF within the same kill budget"
    );
}
