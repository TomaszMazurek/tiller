//! The single writer owner: burst-safe input writes, DSR-priority
//! replies, and serialized resizes.
//!
//! Spike 1 lost input in two ways: keystrokes sent right after a resize were
//! dropped while the hosted TUI re-laid-out (resize↔input race), and large
//! single writes to the composer were sometimes swallowed. Both are fixed by
//! construction here:
//!
//! - one owner thread consumes a command queue and is the *only* code that
//!   touches the PTY write end and master, so a resize can never interleave
//!   with a half-drained input burst;
//! - [`drain`] advances through partial writes, retries `WouldBlock` and
//!   `Interrupted`, and never drops the tail of a burst.
//!
//! # DSR replies take the priority lane
//!
//! A program that sends `ESC[6n` may block in `read()` until the answer
//! arrives — while the answer sits in the same input stream as user input.
//! If a multi-megabyte paste is queued ahead of the reply, the paste can
//! only drain as fast as the program reads, and the program reads nothing
//! until its reply: mutual starvation (P2 review blocker). The owner
//! therefore keeps two lanes: DSR replies on a priority channel, input and
//! resizes on the command channel. The owner answers *all* pending replies
//! before starting any command and between the chunks of a large input, so
//! a reply is delayed by at most one in-flight chunk ([`INPUT_CHUNK`]) plus
//! whatever the platform's input pipe already absorbed. Replies never tear
//! a write: they are injected only at chunk/command boundaries, which is
//! the same granularity the child sees between separate input commands.

use std::io::{self, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use portable_pty::{MasterPty, PtySize};

/// Bulk-input chunk size: the owner checks the DSR priority lane between
/// chunks, so a probe arriving mid-paste is answered after at most one
/// 64 KiB chunk instead of after the whole burst. 64 KiB is far above any
/// realistic keystroke burst (so normal input still lands in one write)
/// and small enough that one chunk drains out of a nearly-full input pipe
/// in bounded time.
const INPUT_CHUNK: usize = 64 * 1024;

/// Commands the single writer owner executes, strictly one at a time.
pub(crate) enum OwnerCommand {
    /// Drain these bytes into the PTY input. When `reply` is set, the owner
    /// reports the outcome there (synchronous write).
    Input {
        bytes: Vec<u8>,
        reply: Option<std::sync::mpsc::Sender<io::Result<()>>>,
    },
    /// Resize the PTY. The reply carries the real outcome of the underlying
    /// resize — this is the control ack callers assert on.
    Resize {
        cols: u16,
        rows: u16,
        reply: std::sync::mpsc::Sender<io::Result<()>>,
    },
    /// Wake-up sentinel from the output pump: a DSR reply may be waiting on
    /// the priority lane. The pump queues the reply *before* sending this,
    /// so handling it always observes the reply; the belt-and-braces drains
    /// (before every command and between chunks) make ordering harmless
    /// either way.
    CheckPriority,
}

/// The PTY-side capabilities the writer owner needs. Behind one trait so the
/// write/resize serialization is testable without a real PTY (mock implements
/// both and records the order it saw).
pub(crate) trait WriteEnd: Send {
    fn write_input(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()>;
}

/// Drain `bytes` into `w` without losing any part of the burst.
///
/// Partial writes advance the slice; `Interrupted` and `WouldBlock` are
/// retried (with a short yield so a would-block writer still makes progress
/// instead of spinning). Blocking is the documented semantics: the call
/// returns only when every byte is written and flushed.
pub(crate) fn drain<W: Write + ?Sized>(w: &mut W, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match w.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pty writer accepted 0 bytes of a non-empty burst",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Defensive: portable-pty writers are blocking, but a
                // would-block must not lose the burst — yield and retry.
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    w.flush()
}

/// The writer-owner loop: serializes input writes and resizes through one
/// queue (a resize can never interleave with a half-drained input burst)
/// while keeping the DSR priority lane drained ahead of both. Exits when
/// the command channel closes (the pump, its last sender, is gone).
pub(crate) fn run_owner<W: WriteEnd>(
    mut end: W,
    priority: Receiver<Vec<u8>>,
    commands: Receiver<OwnerCommand>,
) {
    while let Ok(command) = commands.recv() {
        match command {
            OwnerCommand::Input { bytes, reply } => {
                let result = write_input(&mut end, &priority, &bytes);
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            OwnerCommand::Resize { cols, rows, reply } => {
                flush_priority(&mut end, &priority);
                let result = end.resize(cols, rows);
                let _ = reply.send(result);
            }
            OwnerCommand::CheckPriority => {
                flush_priority(&mut end, &priority);
            }
        }
    }
}

/// Write every pending DSR reply before anything else. Non-blocking: an
/// empty lane returns immediately. A write failure (program gone) stops
/// the drain — the reaper teardown owns that state.
fn flush_priority<W: WriteEnd>(end: &mut W, priority: &Receiver<Vec<u8>>) {
    while let Ok(reply) = priority.try_recv() {
        if end.write_input(&reply).is_err() {
            return;
        }
    }
}

/// Bulk input in [`INPUT_CHUNK`] chunks with the priority lane checked
/// before each chunk: a DSR reply queued mid-paste overtakes everything
/// not yet written instead of waiting out the whole burst.
fn write_input<W: WriteEnd>(
    end: &mut W,
    priority: &Receiver<Vec<u8>>,
    bytes: &[u8],
) -> io::Result<()> {
    for chunk in bytes.chunks(INPUT_CHUNK) {
        flush_priority(end, priority);
        end.write_input(chunk)?;
    }
    Ok(())
}

/// Production [`WriteEnd`]: the PTY write end plus the shared master slot.
///
/// The master is *not* owned here: its lifetime belongs to the hosted
/// program. The reaper takes it out of the [`MasterSlot`] once the child is
/// reaped, and dropping the master is what delivers EOF to the output pump
/// on ConPTY (`ClosePseudoConsole` tears down the conhost end of the output
/// pipe; while the pseudoconsole lives, a read never returns). Resizes
/// served after that fail with [`io::ErrorKind::NotConnected`] instead of
/// touching a closed console.
pub(crate) type MasterSlot = Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>;

pub(crate) struct MasterWriteEnd {
    writer: Box<dyn Write + Send>,
    master: MasterSlot,
}

impl MasterWriteEnd {
    pub(crate) fn new(writer: Box<dyn Write + Send>, master: MasterSlot) -> Self {
        Self { writer, master }
    }
}

impl WriteEnd for MasterWriteEnd {
    fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        drain(self.writer.as_mut(), bytes)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let guard = self.master.lock().unwrap();
        let Some(master) = guard.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "pty master closed (program exited)",
            ));
        };
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;

    use super::*;

    /// Mock PTY writer that accepts at most `chunk` bytes per call and
    /// returns `WouldBlock` on every `would_block_every`-th call.
    struct FlakyWriter {
        chunk: usize,
        would_block_every: usize,
        calls: usize,
        accepted: Vec<u8>,
    }

    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.would_block_every > 0 && self.calls.is_multiple_of(self.would_block_every) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "try again"));
            }
            let n = self.chunk.min(buf.len());
            self.accepted.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn burst(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn drain_survives_partial_writes_and_would_block() {
        let input = burst(100_000);
        let mut w = FlakyWriter {
            chunk: 7,
            would_block_every: 4,
            calls: 0,
            accepted: Vec::new(),
        };
        drain(&mut w, &input).expect("drain completes");
        assert_eq!(w.accepted, input, "no byte lost, order preserved");
        assert!(w.calls > 1, "partial writes actually exercised");
    }

    #[test]
    fn drain_large_burst_small_chunks() {
        let input = burst(1 << 20);
        let mut w = FlakyWriter {
            chunk: 1,
            would_block_every: 0,
            calls: 0,
            accepted: Vec::new(),
        };
        let start = std::time::Instant::now();
        drain(&mut w, &input).expect("drain completes");
        assert_eq!(w.accepted, input);
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "1 MiB in 1-byte chunks is still fast"
        );
    }

    #[test]
    fn drain_reports_write_zero() {
        struct ZeroWriter;
        impl Write for ZeroWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(0) // accepted nothing, claimed success: treat as fatal
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let err = drain(&mut ZeroWriter, b"payload").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
    }

    #[test]
    fn drain_propagates_hard_errors() {
        struct DeniedWriter(usize);
        impl Write for DeniedWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0 += 1;
                if self.0 == 1 {
                    Ok(buf.len().min(3))
                } else {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "gone"))
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let err = drain(&mut DeniedWriter(0), b"payload").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn drain_flushes_once_at_the_end() {
        struct CountingFlush {
            flushes: usize,
        }
        impl Write for CountingFlush {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let mut w = CountingFlush { flushes: 0 };
        drain(&mut w, b"abc").expect("ok");
        assert_eq!(w.flushes, 1);
    }

    /// After the reaper took the master (program exited), a resize must fail
    /// with `NotConnected` instead of panicking or touching a closed console.
    /// The slot is `None` in exactly that state, so no real PTY is needed.
    #[test]
    fn resize_after_master_closed_reports_not_connected() {
        let mut end = MasterWriteEnd {
            writer: Box::new(Vec::new()),
            master: Arc::new(Mutex::new(None)),
        };
        let err = end.resize(80, 24).expect_err("closed master has no resize");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    }

    /// Recording mock for the serialization test: every operation lands in
    /// the log as one complete entry, in the order the owner executed it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Op {
        W(Vec<u8>),
        R(u16, u16),
    }

    struct MockEnd {
        log: Arc<Mutex<Vec<Op>>>,
    }

    impl WriteEnd for MockEnd {
        fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.log.lock().unwrap().push(Op::W(bytes.to_vec()));
            Ok(())
        }
        fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
            self.log.lock().unwrap().push(Op::R(cols, rows));
            Ok(())
        }
    }

    /// The resize↔input race fix, locked in: many threads hammer writes and
    /// resizes through one command queue; the single owner must execute every
    /// operation completely and in per-thread submission order, with every
    /// write visible in the log before its synchronous reply returns.
    #[test]
    fn owner_serializes_writes_and_resizes() {
        const THREADS: usize = 8;
        const ITERS: usize = 25;

        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel::<OwnerCommand>();
        let (priority_tx, priority_rx) = mpsc::channel::<Vec<u8>>();
        drop(priority_tx); // no DSR traffic in this test; a closed lane is fine
        let owner_log = log.clone();
        let owner = thread::Builder::new()
            .name("test-owner".into())
            .spawn(move || run_owner(MockEnd { log: owner_log }, priority_rx, rx))
            .expect("spawn owner");

        let senders: Vec<_> = (0..THREADS)
            .map(|t| {
                let tx = tx.clone();
                let log = log.clone();
                thread::spawn(move || {
                    for i in 0..ITERS {
                        let payload = vec![(t % 251) as u8; 64];
                        let (rtx, rrx) = mpsc::channel();
                        tx.send(OwnerCommand::Input {
                            bytes: payload.clone(),
                            reply: Some(rtx),
                        })
                        .expect("queue write");
                        rrx.recv_timeout(Duration::from_secs(10))
                            .expect("write reply")
                            .expect("write ok");
                        // Synchronous semantics: the completed write is
                        // already in the log when write() returns.
                        assert!(
                            log.lock().unwrap().contains(&Op::W(payload)),
                            "write visible after reply"
                        );

                        let (rtx, rrx) = mpsc::channel();
                        tx.send(OwnerCommand::Resize {
                            cols: 80 + t as u16,
                            rows: 24 + i as u16,
                            reply: rtx,
                        })
                        .expect("queue resize");
                        rrx.recv_timeout(Duration::from_secs(10))
                            .expect("resize reply")
                            .expect("resize ok");
                    }
                })
            })
            .collect();
        drop(tx);

        for h in senders {
            h.join().expect("hammer thread");
        }
        // All senders dropped -> the owner exits and the log is final.
        owner.join().expect("owner clean exit");

        let log = log.lock().unwrap();
        assert_eq!(
            log.len(),
            THREADS * ITERS * 2,
            "every operation executed exactly once"
        );
        let expected_writes = THREADS * ITERS;
        let actual_writes = log.iter().filter(|op| matches!(op, Op::W(_))).count();
        assert_eq!(actual_writes, expected_writes);
        for t in 0..THREADS {
            let write = Op::W(vec![(t % 251) as u8; 64]);
            let positions: Vec<usize> = log
                .iter()
                .enumerate()
                .filter(|(_, op)| **op == write)
                .map(|(i, _)| i)
                .collect();
            assert_eq!(positions.len(), ITERS, "thread {t} writes present");
            let mut sorted = positions.clone();
            sorted.sort_unstable();
            assert_eq!(positions, sorted, "per-thread FIFO order preserved");
        }
    }

    /// WriteEnd that pauses before its second and later writes until a gate
    /// opens — stands in for a PTY input pipe that backs up mid-burst so the
    /// test can queue a DSR reply *while* the owner is inside a bulk input.
    struct GatedEnd {
        log: Arc<Mutex<Vec<Op>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
        writes: std::cell::Cell<usize>,
    }

    impl WriteEnd for GatedEnd {
        fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
            if self.writes.get() > 0 {
                let mut open = self.gate.0.lock().unwrap();
                while !*open {
                    open = self.gate.1.wait(open).unwrap();
                }
            }
            self.writes.set(self.writes.get() + 1);
            self.log.lock().unwrap().push(Op::W(bytes.to_vec()));
            Ok(())
        }
        fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
            Ok(())
        }
    }

    /// The starvation fix, locked in: a DSR reply queued while the owner is
    /// mid-way through a bulk input must overtake the *remaining* chunks —
    /// it is written between chunk N and chunk N+1, not after the burst.
    #[test]
    fn dsr_reply_overtakes_remaining_input_chunks() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel::<OwnerCommand>();
        let (priority_tx, priority_rx) = mpsc::channel::<Vec<u8>>();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let end = GatedEnd {
            log: log.clone(),
            gate: Arc::clone(&gate),
            writes: std::cell::Cell::new(0),
        };

        let owner = thread::spawn(move || run_owner(end, priority_rx, rx));

        // A bulk input of 3 chunks (192 KiB at 64 KiB); the owner completes
        // chunk 1 un-gated, then parks in chunk 2 behind the gate.
        let bulk = vec![b'A'; 3 * super::INPUT_CHUNK];
        let (rtx, rrx) = mpsc::channel();
        tx.send(OwnerCommand::Input {
            bytes: bulk.clone(),
            reply: Some(rtx),
        })
        .expect("queue bulk input");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while log.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "chunk 1 never landed");
            thread::sleep(Duration::from_millis(5));
        }

        // The probe lands while the burst is in flight...
        priority_tx
            .send(b"\x1b[1;1R".to_vec())
            .expect("queue reply");
        tx.send(OwnerCommand::CheckPriority).expect("wake sentinel");
        thread::sleep(Duration::from_millis(50)); // let the owner park in chunk 2

        // ...and the pipe frees up: chunk 2, then the reply MUST be flushed
        // before chunk 3.
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        rrx.recv_timeout(Duration::from_secs(10))
            .expect("bulk reply")
            .expect("bulk input written");
        drop(tx);
        owner.join().expect("owner clean exit");

        let log = log.lock().unwrap();
        let chunk =
            |i: usize| Op::W(bulk[i * super::INPUT_CHUNK..(i + 1) * super::INPUT_CHUNK].to_vec());
        let reply = Op::W(b"\x1b[1;1R".to_vec());
        let expected = [chunk(0), chunk(1), reply.clone(), chunk(2)];
        assert_eq!(
            log.iter().take(4).cloned().collect::<Vec<_>>(),
            expected,
            "reply written between chunks, before the burst's tail"
        );
        assert_eq!(log.len(), 4, "nothing else written");
    }

    /// A reply that lands before a bulk input is queued must be written
    /// first even though it was not there when the owner last checked.
    #[test]
    fn dsr_reply_queued_before_input_writes_first() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel::<OwnerCommand>();
        let (priority_tx, priority_rx) = mpsc::channel::<Vec<u8>>();
        let owner_log = log.clone();
        let owner = thread::spawn(move || run_owner(MockEnd { log: owner_log }, priority_rx, rx));

        priority_tx
            .send(b"\x1b[1;1R".to_vec())
            .expect("queue reply");
        tx.send(OwnerCommand::CheckPriority).expect("sentinel");
        let bulk = vec![b'P'; 1024];
        tx.send(OwnerCommand::Input {
            bytes: bulk.clone(),
            reply: None,
        })
        .expect("queue input");
        drop(tx);
        owner.join().expect("owner clean exit");

        let log = log.lock().unwrap();
        assert_eq!(
            log.as_slice(),
            &[Op::W(b"\x1b[1;1R".to_vec()), Op::W(bulk)],
            "the reply precedes the input queued after it"
        );
    }
}
