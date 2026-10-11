//! Tiller terminal seam: production PTY sessions for hosting TUI agents and
//! user shells, ported from Spike 1 (`spikes/spike1-host`).
//!
//! # What this crate owns (core §3: `term` = PTY + classification + audit)
//!
//! - [`PtySession`] — a spawned program in a pseudo-terminal (ConPTY on
//!   Windows, openpty elsewhere) with a dedicated output pump and a single
//!   writer-owner thread. The writer owner serializes input writes with
//!   resizes through one command queue, closing the resize↔input race found
//!   in Spike 1 (keystrokes sent right after a resize were lost while the
//!   hosted TUI re-laid-out).
//! - [`DsrResponder`] — the host speaks for the terminal on Device Status
//!   Report probes (`ESC[6n`). Spike 1 proved that a TUI can block forever in
//!   `read()` waiting for the cursor-position answer, so the session answers
//!   every probe it sees and consumes the probe bytes from the forwarded
//!   stream. That keeps exactly one responder per probe (atomicity): a real
//!   emulator attached later never sees the probe and cannot double-answer.
//! - [`CommandClassifier`] / [`AuditSink`] — type-only starters for the
//!   command classification and audit seams; the engine and allowlist arrive
//!   in a later slice.
//!
//! # Decisions
//!
//! - **No logging framework yet.** Logging is a separate, undecided slice.
//!   Until then: synchronous operations return their real result to the
//!   caller ([`PtySession::write`], [`PtySession::resize`] — the resize
//!   result *is* the control ack), the session exposes queryable
//!   diagnostics ([`PtySession::output_error`]) and counters
//!   ([`PtySession::dsr_responses_sent`]).
//! - **Blocking I/O on dedicated threads**, not async: portable-pty hands out
//!   blocking reader/writer handles by design (Spike 1 used the same shape).
//!   P3 bridges these channels into its own transport without dragging an
//!   async runtime into the crate.
//! - **Master lifetime = child lifetime.** ConPTY keeps the output pipe open
//!   as long as the pseudoconsole lives, so a reader parked in `read()` never
//!   sees EOF after the program exits on its own. The session's reaper
//!   therefore closes the master right after `child.wait()` parks the exit
//!   status — that close is what delivers EOF to the output pump (bisect:
//!   without it the stream stalls forever; with it EOF lands ~20ms after
//!   exit and buffered output survives).
//! - **DSR replies take the priority lane.** The probe's sender may be the
//!   only party that can drain pending input (it blocks in `read()` until
//!   the answer arrives), so a reply queued behind a multi-megabyte paste
//!   starves both sides. The writer owner answers all pending replies
//!   before any input/resize and between 64 KiB input chunks, bounding a
//!   reply's delay to one in-flight chunk plus what the input pipe already
//!   absorbed.
//! - **Bounded output buffer.** The pump forwards into a ~4 MiB byte-capped
//!   buffer ([`OutputReceiver`]); a full buffer blocks the pump, which makes
//!   the child block in its own writes — natural terminal backpressure.
//!   No-loss holds, memory is O(cap), and a consumer that stops draining
//!   stalls the child by design.
//! - **Kill escalates (Unix), the reaper decides.** portable-pty's Unix
//!   killer only sends SIGHUP; a program that ignores it survives and leaks
//!   the session threads and the master. Two seconds after the initial
//!   signal the *reaper thread* SIGKILLs: it is the single escalation
//!   decider and confirms liveness (`try_wait`) in the same breath as the
//!   signal, so a pid the OS has recycled can never be hit — the child is
//!   at worst an un-reaped zombie that still owns its pid until the reaper
//!   itself collects it. kill/drop always leads to an exit status and EOF.
//!   Windows keeps best-effort TerminateProcess with the reaper as the
//!   authority.
//! - **Events**: session lifecycle maps onto the existing
//!   [`tiller_core::Event`] vocabulary ([`exit_event`]); no new core variants
//!   are needed. P4 will emit them on the event bus.

mod audit;
mod dsr;
mod events;
mod outbuf;
mod policy;
mod session;
mod writer;

pub use audit::{AuditRecord, AuditSink};
pub use dsr::{DsrResponder, ProcessedOutput};
pub use events::{exit_event, process_state};
pub use outbuf::OutputReceiver;
pub use policy::{CommandClassifier, CommandPolicy, PolicyDecision};
pub use session::{PtySession, SpawnConfig};
