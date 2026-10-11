//! Device Status Report (DSR) responder: the host speaks for the terminal.
//!
//! Spike 1 (`spikes/spike1-host`) found that a hosted TUI blocks in `read()`
//! until *something* answers its cursor-position probe `ESC[6n`. The probe
//! can split across PTY read boundaries (`ESC[6` + `n`), so detection has to
//! be incremental across chunks. This module ports the spike's matcher as a
//! pure feed function: probes are consumed from the forwarded stream and
//! answered by the host, which keeps a single responder per probe — a real
//! emulator attached later (P3) never sees the probe bytes and cannot
//! double-answer.

/// The cursor-position report request we answer: `ESC[6n`.
pub const PROBE: &[u8; 4] = b"\x1b[6n";

/// What [`DsrResponder::feed`] produced from one output chunk.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProcessedOutput {
    /// Bytes to pass through to the emulator/UI; probe bytes are consumed.
    pub forward: Vec<u8>,
    /// Reply bytes to inject into the PTY input: one `ESC[r;cR` per probe,
    /// concatenated. Injected as a single write so replies stay atomic
    /// relative to user input.
    pub reply: Vec<u8>,
    /// Number of probes completed by this chunk (the diagnostic counter
    /// behind `dsr_responses_sent`).
    pub responses: usize,
}

/// Incremental matcher/answerer for DSR probes in the PTY output stream.
///
/// Pure with respect to I/O: feed it chunks, get passthrough bytes and reply
/// bytes. The session's output pump owns one instance so split sequences
/// carry over between reads.
#[derive(Debug, Clone)]
pub struct DsrResponder {
    /// The stream currently ends with `PROBE[..partial]` (a probe may be in
    /// flight across chunk boundaries). 0 = no probe prefix pending.
    partial: usize,
    /// Position the host reports for the cursor (row, col). The host is the
    /// terminal, so it answers for the cursor it maintains; home position
    /// worked for the whole Spike 1 e2e and is deterministic under resize.
    row: u16,
    col: u16,
}

impl DsrResponder {
    /// Responder reporting the home position `ESC[1;1R`.
    pub fn new() -> Self {
        Self {
            partial: 0,
            row: 1,
            col: 1,
        }
    }

    /// Responder reporting an explicit cursor position.
    pub fn with_position(row: u16, col: u16) -> Self {
        Self {
            partial: 0,
            row,
            col,
        }
    }

    fn reply_bytes(&self) -> Vec<u8> {
        format!("\x1b[{};{}R", self.row, self.col).into_bytes()
    }

    /// Feed one output chunk. Completed probes are answered once each and
    /// consumed; everything else is forwarded byte-exact.
    ///
    /// Matching logic ports the spike's `DsrOwnership` state machine: a byte
    /// either extends the in-flight probe prefix, starts a new candidate
    /// (`ESC`), or diverges — in which case the withheld prefix was ordinary
    /// data and is flushed to the forwarded stream, so an undelivered
    /// lookalike cannot poison a later real probe.
    pub fn feed(&mut self, chunk: &[u8]) -> ProcessedOutput {
        let mut forward = Vec::with_capacity(chunk.len());
        let mut reply = Vec::new();
        let mut responses = 0usize;
        for &b in chunk {
            if self.partial < PROBE.len() && b == PROBE[self.partial] {
                self.partial += 1;
                if self.partial == PROBE.len() {
                    // Probe complete: consumed (not forwarded) and answered.
                    responses += 1;
                    reply.extend_from_slice(&self.reply_bytes());
                    self.partial = 0;
                }
            } else if b == PROBE[0] {
                // Diverged, but this byte may start a new probe candidate.
                forward.extend_from_slice(&PROBE[..self.partial]);
                self.partial = 1;
            } else {
                forward.extend_from_slice(&PROBE[..self.partial]);
                forward.push(b);
                self.partial = 0;
            }
        }
        // A partial prefix stays pending across the chunk boundary; those
        // bytes are withheld from `forward` until they resolve.
        ProcessedOutput {
            forward,
            reply,
            responses,
        }
    }
}

impl Default for DsrResponder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_probe_is_consumed_and_answered_once() {
        let mut r = DsrResponder::new();
        let out = r.feed(b"\x1b[6n");
        assert_eq!(out.forward, b"");
        assert_eq!(out.reply, b"\x1b[1;1R");
        assert_eq!(out.responses, 1);
        // State resets: a probe right after is answered too.
        let out = r.feed(b"\x1b[6n");
        assert_eq!(out.responses, 1);
    }

    #[test]
    fn multiple_probes_in_one_chunk_each_get_a_reply() {
        // Spike 1 review R3, deferred item (a): two probes in one chunk must
        // not collapse into a single answer.
        let mut r = DsrResponder::new();
        let out = r.feed(b"\x1b[6n\x1b[6n junk \x1b[6n");
        assert_eq!(out.forward, b" junk ");
        assert_eq!(out.reply, b"\x1b[1;1R\x1b[1;1R\x1b[1;1R");
        assert_eq!(out.responses, 3);
    }

    #[test]
    fn probe_split_across_chunks_is_still_answered() {
        let mut r = DsrResponder::new();
        let out = r.feed(b"prefix \x1b[6");
        assert_eq!(out.forward, b"prefix ");
        assert_eq!(out.reply, b"");
        assert_eq!(out.responses, 0);
        let out = r.feed(b"n suffix");
        assert_eq!(out.forward, b" suffix");
        assert_eq!(out.reply, b"\x1b[1;1R");
        assert_eq!(out.responses, 1);
    }

    #[test]
    fn three_way_split_answered() {
        let mut r = DsrResponder::new();
        assert_eq!(r.feed(b"a\x1b").forward, b"a");
        assert_eq!(r.feed(b"[").forward, b"");
        assert_eq!(r.feed(b"6").forward, b"");
        let out = r.feed(b"n");
        assert_eq!(out.reply, b"\x1b[1;1R");
        assert_eq!(out.responses, 1);
    }

    #[test]
    fn byte_at_a_time_feeding_matches_single_chunk() {
        let mut once = DsrResponder::new();
        let once = once.feed(b"head \x1b[6n tail \x1b[6n end");
        let mut bytewise = DsrResponder::new();
        let mut acc = ProcessedOutput::default();
        for b in b"head \x1b[6n tail \x1b[6n end" {
            let o = bytewise.feed(std::slice::from_ref(b));
            acc.forward.extend(o.forward);
            acc.reply.extend(o.reply);
            acc.responses += o.responses;
        }
        assert_eq!(once, acc);
    }

    #[test]
    fn diverged_prefix_is_forwarded_and_does_not_poison_later_probes() {
        let mut r = DsrResponder::new();
        // `ESC[6` then divergence: the withheld bytes were ordinary data.
        let out = r.feed(b"\x1b[6J");
        assert_eq!(out.forward, b"\x1b[6J");
        assert_eq!(out.responses, 0);
        // A later real probe still gets answered.
        let out = r.feed(b"\x1b[6n");
        assert_eq!(out.responses, 1);
    }

    #[test]
    fn no_false_positive_on_lookalike_sequences() {
        let mut r = DsrResponder::new();
        for chunk in [
            b"\x1b[66n".as_slice(), // different DSR parameter
            b"\x1b[?6n".as_slice(), // private-mode query, not a probe
            b"\x1b[6x".as_slice(),  // diverges before the final byte
            b"[6n".as_slice(),      // no escape byte
        ] {
            let out = r.feed(chunk);
            assert_eq!(out.forward, chunk, "forwarded byte-exact: {chunk:?}");
            assert_eq!(out.responses, 0, "not a probe: {chunk:?}");
        }
    }

    #[test]
    fn stray_escape_before_a_real_probe_is_data_probe_still_answered() {
        // `ESC ESC [6n`: the stray ESC is data, the rest is a genuine probe.
        let mut r = DsrResponder::new();
        let out = r.feed(b"\x1b\x1b[6n");
        assert_eq!(out.forward, b"\x1b");
        assert_eq!(out.reply, b"\x1b[1;1R");
        assert_eq!(out.responses, 1);
    }

    #[test]
    fn binary_payload_flows_through_untouched() {
        // A chunk of arbitrary bytes (with ESC-like noise) forwards intact.
        let payload: Vec<u8> = (0u16..600).map(|i| (i % 251) as u8).collect();
        let mut r = DsrResponder::new();
        let out = r.feed(&payload);
        assert_eq!(out.forward, payload);
        assert_eq!(out.reply, b"");
        assert_eq!(out.responses, 0);
    }

    #[test]
    fn reply_reports_the_configured_position() {
        let mut r = DsrResponder::with_position(12, 34);
        let out = r.feed(b"\x1b[6n");
        assert_eq!(out.reply, b"\x1b[12;34R");
    }
}
