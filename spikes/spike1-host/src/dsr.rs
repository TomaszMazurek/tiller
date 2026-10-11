//! DSR probe answer-ownership for the PTY output pump.
//!
//! pi's TUI blocks in read() until SOMETHING answers its cursor-position probe
//! `ESC[6n`. The probe can split across PTY read boundaries (`ESC[6` + `n`),
//! so ownership cannot be decided from the delivery of the current chunk
//! alone: a client attaching between fragments never saw the probe start and
//! will not answer it.
//!
//! Ownership rule (ONE rule, decided atomically with the delivery of each
//! chunk): a probe belongs to the attached terminal emulators only if EVERY
//! chunk carrying any of its bytes was broadcast to at least one live WS
//! client; when the probe completes, the host itself answers `ESC[1;1R` if
//! any chunk of it reached nobody (no client attached yet, a client detached
//! mid-probe, or the broadcast failed). State is per probe: it resets on the
//! decision, and a byte stream that diverges from the probe prefix voids the
//! accumulated flag (an undelivered chunk that turns out not to contain probe
//! bytes must not make the host answer a later, fully delivered probe).

/// Incremental decision state for "who answers the DSR probe": the host or
/// the attached emulators. Feed each PTY output chunk together with the
/// number of clients the broadcast delivered it to.
pub struct DsrOwnership {
    /// The stream currently ends with `PROBE[..partial]` (a probe may be in
    /// flight). 0 = no probe prefix pending.
    partial: usize,
    /// Some chunk carrying bytes of the in-flight probe reached no client.
    undelivered: bool,
}

const PROBE: &[u8; 4] = b"\x1b[6n";

impl DsrOwnership {
    pub fn new() -> Self {
        Self { partial: 0, undelivered: false }
    }

    /// Observe one PTY output chunk and how many WS clients the broadcast
    /// delivered it to (a failed send counts as 0 — nothing was delivered).
    /// Returns true exactly when a probe just COMPLETED with at least one of
    /// its chunks undelivered: the host must answer it now.
    pub fn observe(&mut self, chunk: &[u8], delivered: usize) -> bool {
        let mut answer_now = false;
        for &b in chunk {
            if self.partial < PROBE.len() && b == PROBE[self.partial] {
                // Byte extends the in-flight (or brand new) probe prefix.
                self.partial += 1;
                if self.partial == PROBE.len() {
                    // Probe complete: decide ownership with THIS chunk's
                    // delivery included. Any earlier fragment that reached
                    // nobody means no client saw the whole probe.
                    answer_now |= self.undelivered || delivered == 0;
                    self.partial = 0;
                    self.undelivered = false;
                }
            } else if b == PROBE[0] {
                // Mismatch, but a new probe may start at this byte; the dead
                // prefix never became a probe, so its flag is void.
                self.partial = 1;
                self.undelivered = false;
            } else {
                // Stream diverged from the probe prefix.
                self.partial = 0;
                self.undelivered = false;
            }
        }
        // A partial probe still in flight at the end of this chunk: if the
        // chunk reached nobody, the probe-so-far has an undelivered fragment.
        if self.partial > 0 && delivered == 0 {
            self.undelivered = true;
        }
        answer_now
    }
}

impl Default for DsrOwnership {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_probe_undelivered_host_answers() {
        let mut o = DsrOwnership::new();
        assert!(o.observe(b"\x1b[6n", 0));
    }

    #[test]
    fn whole_probe_delivered_emulator_owns_it() {
        let mut o = DsrOwnership::new();
        assert!(!o.observe(b"\x1b[6n", 1));
    }

    /// Reviewer scenario (forced split with attach between reads): the first
    /// fragment `ESC[6` is broadcast to zero recipients, a client attaches,
    /// and the final fragment `n` IS delivered — the client never saw the
    /// probe start, so the host must answer once the probe completes.
    #[test]
    fn split_probe_attach_between_reads_host_answers() {
        let mut o = DsrOwnership::new();
        assert!(!o.observe(b"\x1b[6", 0));
        assert!(o.observe(b"n", 1));
    }

    /// Mirror of the reviewer scenario: detach between fragments. The first
    /// fragment was delivered, the completing one was not — the host answers.
    #[test]
    fn split_probe_detach_between_reads_host_answers() {
        let mut o = DsrOwnership::new();
        assert!(!o.observe(b"\x1b[6", 1));
        assert!(o.observe(b"n", 0));
    }

    #[test]
    fn three_way_split_attach_only_at_the_end_host_answers() {
        let mut o = DsrOwnership::new();
        assert!(!o.observe(b"\x1b", 0));
        assert!(!o.observe(b"[6", 0));
        assert!(o.observe(b"n", 1));
    }

    #[test]
    fn split_probe_fully_delivered_host_stays_silent() {
        let mut o = DsrOwnership::new();
        assert!(!o.observe(b"\x1b[6", 1));
        assert!(!o.observe(b"n", 1));
    }

    /// An undelivered chunk whose bytes looked like a probe start but then
    /// diverged (`ESC[6` + `J…` = some other CSI sequence) must not poison a
    /// LATER probe that was delivered in full.
    #[test]
    fn diverged_prefix_does_not_poison_later_probes() {
        let mut o = DsrOwnership::new();
        assert!(!o.observe(b"\x1b[6", 0)); // partial that will diverge
        assert!(!o.observe(b"J junk", 1)); // not a probe -> flag voided
        assert!(!o.observe(b"\x1b[6n", 1)); // fully delivered: emulator owns it
    }

    #[test]
    fn undelivered_unrelated_csi_does_not_poison_later_probes() {
        let mut o = DsrOwnership::new();
        assert!(!o.observe(b"repaint \x1b[2J", 0));
        assert!(!o.observe(b"\x1b[6n", 1));
    }

    /// The probe state resets after a decision: a host-answered probe must not
    /// silence the next one, and vice versa.
    #[test]
    fn state_resets_after_decision() {
        let mut o = DsrOwnership::new();
        assert!(o.observe(b"\x1b[6n", 0)); // host answered this one
        assert!(!o.observe(b"\x1b[6n", 1)); // next one fully delivered
        assert!(o.observe(b"junk \x1b[6n", 0)); // and undelivered again
    }
}
