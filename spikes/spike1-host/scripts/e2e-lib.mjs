// e2e-lib.mjs — pure helpers for the e2e drivers (unit-tested, no I/O).
//
// Turn-final sync rule (used by e2e-demo.mjs sendPrompt): an assistant
// `message_end` with stopReason "toolUse" is NOT the reply to wait for — it
// only announces tool calls and can even carry text, so accepting any new
// non-empty message_end could mistake it for the final answer (the archived
// host-stdout-final-ABCDE.log shows a toolUse end landing between the prompt
// and the real reply). The wait condition is the turn-final event; the
// expected marker is a separate assertion, never the wait condition.

/// True for a turn-final assistant event: message_end whose stopReason is
/// anything except "toolUse" (usually "stop"; a missing stopReason is treated
/// as final — only toolUse explicitly means "more of this turn is coming").
export const isTurnFinalAssistantEnd = (e) =>
  e?.event?.type === "message_end" &&
  e?.event?.role === "assistant" &&
  e?.event?.stopReason !== "toolUse";

/// First turn-final assistant event in a list of session-tail events, or null.
export const firstTurnFinalAssistantEnd = (events) => {
  for (const e of events) {
    if (isTurnFinalAssistantEnd(e)) return e;
  }
  return null;
};

/// Incremental DSR-probe (`ESC[6n`) detector for WS binary frames. The WS
/// framing can split the 4-byte probe across frames, so — like the host's
/// pump (src/dsr.rs) — keep a 3-byte tail and scan tail+frame. Returns true
/// when this frame completes a probe (the caller answers `ESC[1;1R`).
/// `frame` must be decoded byte-faithfully (latin1); the probe is pure ASCII,
/// so multibyte splits elsewhere in the frame cannot create or mask it.
export const makeDsrDetector = () => {
  const PROBE = "\x1b[6n";
  let tail = "";
  return (frame) => {
    const scan = tail + frame;
    const hit = scan.includes(PROBE);
    tail = scan.slice(-3);
    return hit;
  };
};
