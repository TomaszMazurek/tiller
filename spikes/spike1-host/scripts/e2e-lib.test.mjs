// e2e-lib.test.mjs — unit tests for the turn-final/marker split logic of the
// e2e driver (review finding P1: a stopReason:"toolUse" message_end must never
// satisfy the wait for the final reply; the marker is a separate assertion).
import test from "node:test";
import assert from "node:assert/strict";
import { isTurnFinalAssistantEnd, firstTurnFinalAssistantEnd, makeDsrDetector } from "./e2e-lib.mjs";

const ev = (over = {}) => ({
  t: "ev",
  event: { type: "message_end", role: "assistant", stopReason: "toolUse", preview: "", ...over },
});

test("toolUse message_end is not turn-final, even when it carries text", () => {
  assert.equal(isTurnFinalAssistantEnd(ev({ preview: "I will run it." })), false);
  assert.equal(isTurnFinalAssistantEnd(ev({ stopReason: "toolUse", preview: "" })), false);
});

test("stop message_end is turn-final", () => {
  assert.equal(isTurnFinalAssistantEnd(ev({ stopReason: "stop", preview: "SPIKE1-READY" })), true);
});

test("missing stopReason is treated as turn-final (only toolUse means 'not final')", () => {
  assert.equal(isTurnFinalAssistantEnd(ev({ stopReason: null, preview: "x" })), true);
  assert.equal(isTurnFinalAssistantEnd({ t: "ev", event: { type: "message_end", role: "assistant", preview: "x" } }), true);
});

test("non-assistant message_end is never turn-final", () => {
  assert.equal(isTurnFinalAssistantEnd({ t: "ev", event: { type: "message_end", role: "user", preview: "prompt" } }), false);
  assert.equal(isTurnFinalAssistantEnd({ t: "ev", event: { type: "tool_execution_end", toolName: "bash" } }), false);
});

test("firstTurnFinalAssistantEnd skips toolUse ends and picks the stop end", () => {
  // Shape from evidence host-stdout-final-ABCDE.log: prompt -> (empty) toolUse
  // end -> tool events -> stop end carrying the marker.
  const list = [
    { t: "ev", event: { type: "message_end", role: "user", preview: "… PIENV-DONE" } },
    ev(),
    { t: "ev", event: { type: "tool_execution_start", toolName: "bash" } },
    { t: "ev", event: { type: "tool_execution_end", toolName: "bash", preview: "PI_SESSION_FILE=…" } },
    ev({ stopReason: "stop", preview: "C:\\…\\.jsonl\nPIENV-DONE" }),
  ];
  const r = firstTurnFinalAssistantEnd(list);
  assert.ok(r);
  assert.equal(r.event.stopReason, "stop");
  assert.equal(String(r.event.preview).includes("PIENV-DONE"), true);
});

test("a toolUse end with text does not satisfy the sync, and the marker stays a separate assertion", () => {
  // Regression for the reviewed bug: the old wait accepted the FIRST new
  // non-empty message_end, so a toolUse end carrying text (or, with empty
  // toolUse previews, nothing at all — it just kept waiting) could be taken
  // for the final reply.
  const marker = "MCP-DONE";
  const list = [
    ev({ preview: "Let me call the tool." }),
    ev({ stopReason: "stop", preview: "tool result follows; marker missing here" }),
  ];
  const r = firstTurnFinalAssistantEnd(list);
  assert.ok(r);
  assert.equal(r.event.stopReason, "stop");
  assert.equal(String(r.event.preview).includes(marker), false); // assertion fails, but the sync point is well-defined
});

test("no turn-final end yields null (wait keeps polling)", () => {
  assert.equal(firstTurnFinalAssistantEnd([ev(), ev({ preview: "text" })]), null);
  assert.equal(firstTurnFinalAssistantEnd([]), null);
});

// ---- DSR probe detection across WS frame splits (review finding P2: the
// driver's old `data.includes("\x1b[6n")` saw only the current frame, so a
// probe split by the WS framing was never answered — same class of bug as the
// host-side split-probe race in src/dsr.rs). ----

test("DSR detector: whole probe in one frame fires once", () => {
  const d = makeDsrDetector();
  assert.equal(d("repaint \u001b[6n tail"), true);
  assert.equal(d("more output"), false);
});

test("DSR detector: probe split across frames is still detected (ESC[6 | n)", () => {
  const d = makeDsrDetector();
  assert.equal(d("\u001b[6"), false);
  assert.equal(d("n"), true); // the driver must answer here — old code never did
});

test("DSR detector: three-way split is detected", () => {
  const d = makeDsrDetector();
  assert.equal(d("junk \u001b"), false);
  assert.equal(d("[6"), false);
  assert.equal(d("n more junk"), true);
});

test("DSR detector: unrelated CSI sequence split across frames does not fire", () => {
  const d = makeDsrDetector();
  assert.equal(d("\u001b[2"), false);
  assert.equal(d("J cleared"), false);
  assert.equal(d("\u001b[6n"), true); // a later real probe still works
});

test("DSR detector: adjacent text cannot form a false probe", () => {
  const d = makeDsrDetector();
  // "6n" without the ESC[ prefix, then an ESC[ that belongs to another sequence.
  assert.equal(d("value 6n done \u001b["), false);
  assert.equal(d("2J"), false);
});
