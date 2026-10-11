//! Bounded output buffer: byte-capacity backpressure between the output
//! pump and the session's single consumer.
//!
//! An unbounded channel here means a chatty child plus a consumer that
//! stops reading exhausts memory (P2 review, major). The buffer caps
//! *queued bytes*: when full, the pump blocks in [`OutputSender::push`],
//! which in turn makes the child block in its own `write()` once the PTY
//! buffers fill — the terminal's natural backpressure. No bytes are
//! dropped (the no-loss contract holds); memory is O(cap). A consumer
//! that stops draining stalls the child by design; a consumer that goes
//! away (session dropped) unblocks the pump so the session's threads
//! always wind down. Empty chunks are absorbed rather than queued: a
//! probe-only read forwards an empty `Vec`, and queueing those would grow
//! the entry count forever without ever tripping the byte cap.
//!
//! Backpressure is confined to the output stream: the pump's *other*
//! action — queueing DSR replies to the writer owner — uses an
//! unbounded command channel, so output pressure can never deadlock the
//! input path.

use std::collections::VecDeque;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Queued-byte capacity of a session's output buffer.
///
/// ~4 MiB: several screens of scrollback for a UI consumer at rest, a
/// rounding error of the process footprint, and only a ceiling — the pump
/// forwards in 16 KiB read chunks, so the cap never gates a single
/// message (an empty buffer always accepts one chunk).
pub(crate) const OUTPUT_BUFFER_CAP: usize = 4 * 1024 * 1024;

struct State {
    chunks: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    /// The pump finished (EOF or read error): drain what is left, then
    /// disconnect the consumer.
    closed: bool,
    /// The consumer is gone (session dropped): stop buffering and unblock
    /// the pump. Bytes still queued die with the buffer — no-loss applies
    /// to live consumers only.
    abandoned: bool,
}

struct Shared {
    cap: usize,
    state: Mutex<State>,
    has_space: Condvar,
    has_data: Condvar,
}

pub(crate) fn output_buffer(cap: usize) -> (OutputSender, OutputReceiver) {
    let shared = Arc::new(Shared {
        cap,
        state: Mutex::new(State {
            chunks: VecDeque::new(),
            queued_bytes: 0,
            closed: false,
            abandoned: false,
        }),
        has_space: Condvar::new(),
        has_data: Condvar::new(),
    });
    (
        OutputSender {
            shared: Arc::clone(&shared),
        },
        OutputReceiver { shared },
    )
}

/// Pump-side handle: [`OutputSender::push`] blocks while the buffer sits at
/// capacity, returning `false` only when the consumer is gone.
pub struct OutputSender {
    shared: Arc<Shared>,
}

impl OutputSender {
    /// Queue one chunk, blocking (backpressure) while the buffer is full.
    /// Returns `false` when the consumer was dropped — the pump should
    /// stop, not spin. An empty buffer always accepts the chunk, even one
    /// larger than the cap: the cap bounds memory, it is not a frame size.
    /// An *empty* chunk is not queued at all — nothing to deliver, nothing
    /// to wake the consumer for — so a run of probe-only reads cannot
    /// inflate the queue; it only reports the consumer's liveness.
    pub(crate) fn push(&self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() {
            let state = self.shared.state.lock().unwrap();
            return !state.abandoned;
        }
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if state.abandoned {
                return false;
            }
            if state.queued_bytes == 0 || state.queued_bytes + bytes.len() <= self.shared.cap {
                state.queued_bytes += bytes.len();
                state.chunks.push_back(bytes);
                drop(state);
                self.shared.has_data.notify_one();
                return true;
            }
            state = self.shared.has_space.wait(state).unwrap();
        }
    }
}

impl Drop for OutputSender {
    fn drop(&mut self) {
        // The pump finished: close so the consumer drains the remainder
        // and then sees `Disconnected` (same observable as the old
        // channel's sender-drop).
        let mut state = self.shared.state.lock().unwrap();
        state.closed = true;
        drop(state);
        self.shared.has_data.notify_all();
    }
}

/// Consumer-side handle: `recv_timeout` with the shape of
/// `std::sync::mpsc::Receiver` so callers keep one idiom across the seam.
///
/// Single consumer by convention (P3 fans out above this seam); the
/// internal lock keeps concurrent calls safe, and a consumer that lives
/// shorter than the session is dropped with it, which unblocks the pump.
pub struct OutputReceiver {
    shared: Arc<Shared>,
}

impl OutputReceiver {
    /// Receive the next chunk, waiting up to `timeout`. `Timeout` means
    /// quiet-but-alive; `Disconnected` means the pump finished and the
    /// buffer is drained.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some(chunk) = state.chunks.pop_front() {
                state.queued_bytes -= chunk.len();
                drop(state);
                self.shared.has_space.notify_one();
                return Ok(chunk);
            }
            if state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let Some(remain) = deadline.checked_duration_since(Instant::now()) else {
                return Err(RecvTimeoutError::Timeout);
            };
            if remain.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            let (guard, _) = self.shared.has_data.wait_timeout(state, remain).unwrap();
            state = guard;
        }
    }
}

impl Drop for OutputReceiver {
    fn drop(&mut self) {
        // The consumer is gone: unblock a pump parked on a full buffer so
        // the session's threads always wind down after a drop.
        let mut state = self.shared.state.lock().unwrap();
        state.abandoned = true;
        drop(state);
        self.shared.has_space.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// Backpressure: pushing past the cap without a consumer blocks the
    /// pusher at exactly the cap (data intact, nothing dropped), and a
    /// consumer that then drains receives everything in order.
    #[test]
    fn push_blocks_at_capacity_until_drained() {
        let (tx, rx) = output_buffer(1024);
        let pusher = thread::spawn(move || {
            for i in 0..3u8 {
                assert!(tx.push(vec![i; 512]), "consumer alive, push accepted");
            }
        });
        // Let the pusher run into the cap (2×512 fits, the 3rd blocks).
        thread::sleep(Duration::from_millis(200));
        assert!(
            !pusher.is_finished(),
            "push past the cap must block while nobody drains"
        );
        {
            let state = rx.shared.state.lock().unwrap();
            assert_eq!(state.queued_bytes, 1024, "buffer sits exactly at cap");
            assert_eq!(state.chunks.len(), 2);
        }
        // Drain: the blocked third push lands, everything arrives in order.
        let mut got = Vec::new();
        while got.len() < 3 * 512 {
            let chunk = rx.recv_timeout(Duration::from_secs(5)).expect("chunk");
            got.extend_from_slice(&chunk);
        }
        pusher.join().expect("pusher clean exit");
        let expected: Vec<u8> = (0..3u8).flat_map(|i| vec![i; 512]).collect();
        assert_eq!(got, expected, "no byte lost, order preserved");
    }

    /// The cap is a memory ceiling, not a frame size: an empty buffer
    /// accepts a chunk larger than the cap instead of deadlocking.
    #[test]
    fn oversize_chunk_passes_on_empty_buffer() {
        let (tx, rx) = output_buffer(4);
        assert!(tx.push(vec![7u8; 10]));
        let chunk = rx.recv_timeout(Duration::from_secs(5)).expect("chunk");
        assert_eq!(chunk, vec![7u8; 10]);
    }

    /// Empty chunks are absorbed instead of queued (P1 review): probe-only
    /// reads forward an empty `Vec`, and queueing those would grow the
    /// entry count forever without ever tripping the byte cap — an
    /// unbounded leak hiding behind a bounded buffer.
    #[test]
    fn empty_pushes_are_not_queued() {
        let (tx, rx) = output_buffer(64);
        for _ in 0..100_000 {
            assert!(tx.push(Vec::new()), "empty push accepted (consumer alive)");
        }
        {
            let state = rx.shared.state.lock().unwrap();
            assert_eq!(state.queued_bytes, 0, "empty pushes carry no bytes");
            assert_eq!(state.chunks.len(), 0, "empty pushes must not queue entries");
        }
        // The stream is unaffected: a real chunk still arrives in order.
        assert!(tx.push(b"after-probes".to_vec()));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            b"after-probes"
        );
    }

    /// Teardown liveness: a consumer dropped mid-backpressure must unblock
    /// the pusher (which reports the abandonment) — the pump can never be
    /// parked forever on a session that is gone.
    #[test]
    fn dropped_consumer_unblocks_pusher() {
        let (tx, rx) = output_buffer(8);
        assert!(tx.push(vec![1u8; 8]));
        let pusher = thread::spawn(move || !tx.push(vec![2u8; 8]));
        thread::sleep(Duration::from_millis(200));
        assert!(!pusher.is_finished(), "pusher parked on the full buffer");
        drop(rx);
        let abandoned = pusher.join().expect("pusher clean exit");
        assert!(abandoned, "push reports the consumer is gone");
    }

    /// Close semantics: after the pump drops its sender, the consumer
    /// drains the remainder and only then sees `Disconnected`.
    #[test]
    fn receiver_drains_then_disconnects_after_close() {
        let (tx, rx) = output_buffer(1024);
        tx.push(b"hello".to_vec());
        tx.push(b"world".to_vec());
        drop(tx);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), b"hello");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), b"world");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    /// A zero timeout must answer immediately instead of busy-waiting.
    #[test]
    fn zero_timeout_returns_timeout_when_empty() {
        let (_tx, rx) = output_buffer(64);
        let start = Instant::now();
        assert_eq!(
            rx.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Timeout)
        );
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
