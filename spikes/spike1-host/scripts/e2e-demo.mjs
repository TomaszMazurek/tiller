// e2e-demo.mjs — automated end-to-end driver for spike1-host (criteria A–E).
//
// Prereq: spike1-host is running (spawned pi in a PTY) and the WS bridge
// listens on 127.0.0.1:8792. The script acts as a "headless browser terminal":
// it sends keystrokes over the WS, collects PTY output (binary frames) and the
// host's session-tail events (text frames), then asserts each criterion.
//
// Usage: node scripts/e2e-demo.mjs [--ws-port 8792]
// Writes .pi-run/e2e-result.json and .pi-run/e2e-tui-output.bin

import WebSocket from "ws";
import { writeFileSync, appendFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { firstTurnFinalAssistantEnd, makeDsrDetector } from "./e2e-lib.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const runDir = join(root, ".pi-run");
mkdirSync(runDir, { recursive: true });

const args = process.argv.slice(2);
const portIdx = args.indexOf("--ws-port");
const wsPort = portIdx >= 0 ? Number(args[portIdx + 1]) : 8792;

const t0 = Date.now();
const results = [];
const tuiChunks = []; // raw PTY output bytes
const events = [];    // session-tail events (parsed JSON)
let evSeq = 0;

const stripAnsi = (s) => s.replace(
  /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)|[\x00-\x09\x0b-\x1f\x7f]/g,
  "",
);

function tuiText() {
  return stripAnsi(Buffer.concat(tuiChunks).toString("utf8"));
}

function log(msg) {
  const line = `[e2e +${((Date.now() - t0) / 1000).toFixed(1)}s] ${msg}`;
  console.log(line);
  appendFileSync(join(runDir, "e2e.log"), line + "\n");
}

function record(id, name, pass, evidence) {
  results.push({ id, name, pass, evidence, ts: new Date().toISOString(), elapsedMs: Date.now() - t0 });
  log(`${pass ? "PASS" : "FAIL"} — ${id}: ${name}`);
}

const ws = new WebSocket(`ws://127.0.0.1:${wsPort}`);
ws.binaryType = "nodebuffer";

let firstByteAt = null;
let ctlResize = null; // last {"t":"ctl","msg":"resize"} ack from the host
const waiters = [];
function notifyWaiters() {
  for (let i = waiters.length - 1; i >= 0; i--) {
    const w = waiters[i];
    try {
      if (w.check()) {
        waiters.splice(i, 1);
        w.resolve();
      }
    } catch (e) {
      waiters.splice(i, 1);
      w.reject(e);
    }
  }
}
function waitFor(desc, check, timeoutMs) {
  return new Promise((resolve, reject) => {
    const w = { desc, check, resolve, reject };
    waiters.push(w);
    const timer = setTimeout(() => {
      const i = waiters.indexOf(w);
      if (i >= 0) {
        waiters.splice(i, 1);
        reject(new Error(`timeout waiting for: ${desc}`));
      }
    }, timeoutMs);
    const origResolve = w.resolve;
    w.resolve = () => { clearTimeout(timer); origResolve(); };
    notifyWaiters();
  });
}

ws.on("message", (data, isBinary) => {
  if (isBinary) {
    if (firstByteAt === null) {
      firstByteAt = Date.now();
      log(`first PTY output byte received (${data.length} bytes)`);
    }
    tuiChunks.push(data);
    // xterm.js parity: answer the terminal's DSR cursor-position probe, in case
    // pi re-probes while we are attached (the host leaves a probe to the
    // emulators only when every fragment of it was delivered to a client).
    // The detector's tail buffer catches a probe split across WS frames
    // (makeDsrDetector in e2e-lib.mjs; latin1 keeps bytes 1:1).
    if (dsrProbe(data.toString("latin1"))) {
      ws.send(Buffer.from("\x1b[1;1R", "utf8"));
      log("answered DSR probe ESC[1;1R");
    }
    notifyWaiters();
  } else {
    try {
      const v = JSON.parse(data.toString("utf8"));
      if (v.t === "ev") {
        v.seq = ++evSeq;
        v.recvMs = Date.now() - t0;
        events.push(v);
        log(`ev#${v.seq} ${v.event?.type} ${v.event?.toolName ?? v.event?.role ?? ""} ${v.event?.preview ? JSON.stringify(String(v.event.preview).slice(0, 90)) : ""}`);
        notifyWaiters();
      } else if (v.t === "ctl") {
        // Control acks from the host (currently: result of pty_master.resize()).
        if (v.msg === "resize") {
          ctlResize = v;
          log(`ctl: resize ${v.cols}x${v.rows} ok=${v.ok}${v.err ? ` err=${String(v.err)}` : ""}`);
          notifyWaiters();
        }
      } else if (v.t === "info") {
        log(`info: ${v.msg ?? ""} ${v.path ?? ""}`);
      }
    } catch { /* not JSON */ }
  }
});

// xterm.js parity: long single-burst writes into pi's composer are sometimes
// dropped; type long strings in small chunks (see README pitfall 9).
const type = async (text) => {
  if (text.length <= 32) { ws.send(Buffer.from(text, "utf8")); return; }
  for (let i = 0; i < text.length; i += 16) {
    ws.send(Buffer.from(text.slice(i, i + 16), "utf8"));
    await sleep(40);
  }
};
const enter = () => type("\r");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const dsrProbe = makeDsrDetector();

let lastReply = { markerSeen: false, preview: "" };
// Exchanges that completed AFTER the mid-session resize WITH the marker
// confirmed in the turn-final reply (hard assertion) — the honest A2
// evidence ("resize accepted by the PTY [ctl:ok]; session survived"),
// collected as the run progresses instead of being recorded unconditionally.
const postResizeExchanges = [];

// Marker-as-hard-assertion (review round 2): a B/C/D exchange whose turn-final
// reply lacks the expected marker is recorded as FAIL for that exchange AND
// excluded from postResizeExchanges — A2 can no longer pass on unconfirmed
// replies. (E stays best-effort: its marker can fall out of the truncated
// 200-char event preview; the full session JSONL is the authoritative check
// there, as documented in the README.)
const confirmExchange = (id, marker, stepMs) => {
  const ok = lastReply.markerSeen;
  record(id, `turn-final reply contains marker ${marker} (hard assertion)`, ok,
    ok ? `marker confirmed in the turn-final reply (stopReason=${lastReply.stopReason}); step ${stepMs} ms`
       : `marker MISSING from the turn-final reply (stopReason=${lastReply.stopReason}); preview may be truncated to 200 chars — inspect the session JSONL; step ${stepMs} ms; preview: ${JSON.stringify(lastReply.preview)}`);
  if (ok) postResizeExchanges.push(id);
  return ok;
};

const sendPrompt = async (marker, prompt, timeoutMs = 240000) => {
  const sentAt = Date.now();
  const promptTail = prompt.slice(-24);
  const assistantEnds = () =>
    events.filter((e) => e.event?.type === "message_end" && e.event?.role === "assistant");
  const before = assistantEnds().length;
  // Type-and-verify: keystrokes sent right after a resize (or while pi is
  // mid-turn) can be dropped while the TUI re-lays out. Verify the prompt was
  // echoed into the composer, retry once.
  let typed = false;
  for (let attempt = 1; attempt <= 3 && !typed; attempt++) {
    type(prompt);
    await sleep(attempt === 1 ? 600 : 1200);
    typed = tuiText().includes(promptTail);
    if (!typed) {
      log(`prompt not echoed in composer (attempt ${attempt}); re-typing`);
      type("\x15"); // Ctrl+U: kill line
      await sleep(400);
    }
  }
  if (!typed) throw new Error(`prompt never echoed into composer: ${promptTail}`);
  const submittedAt = Date.now();
  enter();
  // Sync point: the TURN-FINAL assistant message_end — stopReason != "toolUse"
  // (usually "stop"). A "toolUse" message_end only announces tool calls and
  // can even carry text, so accepting any new non-empty message_end could
  // mistake it for the final reply (evidence host-stdout-final-ABCDE.log: a
  // toolUse end lands between the prompt and the real answer). The expected
  // marker is checked separately below, as an assertion — never as the wait
  // condition (matching it in the echoed prompt or the user bubble would be
  // a false positive; the wait is on the authoritative session-tail event).
  let reply = null;
  await waitFor(
    `turn-final assistant message_end (stopReason != toolUse) after prompt containing ${marker}`,
    () => {
      reply = firstTurnFinalAssistantEnd(assistantEnds().slice(before));
      return !!reply;
    },
    timeoutMs,
  );
  lastReply = {
    markerSeen: String(reply?.event?.preview ?? "").includes(marker),
    stopReason: reply?.event?.stopReason ?? null,
    preview: String(reply?.event?.preview ?? "").slice(0, 300),
    replyMs: Date.now() - submittedAt,
  };
  if (!lastReply.markerSeen) {
    log(`ASSERTION: marker ${JSON.stringify(marker)} NOT in the turn-final reply preview (stopReason=${lastReply.stopReason}; host truncates previews to 200 chars) — inspect the session JSONL for the full text`);
  }
  log(`assistant turn-final reply (${lastReply.replyMs} ms, stopReason=${lastReply.stopReason}, marker=${lastReply.markerSeen}): ${JSON.stringify(lastReply.preview.slice(0, 120))}`);
  return Date.now() - sentAt;
};

async function main() {
  log(`connecting ws://127.0.0.1:${wsPort} ...`);
  await new Promise((res, rej) => { ws.once("open", res); ws.once("error", rej); });
  log("ws connected");

  // Mimic the browser client (web/index.html sends hello+resize on open).
  // pi's TUI paints only after a window-size event, so without this the
  // first-PTY-output wait below would time out even though pi is alive.
  ws.send(JSON.stringify({ t: "hello", cols: 120, rows: 40 }));
  log("sent hello 120x40 (browser parity)");

  // ---------- A: PTY hosting ----------
  await waitFor("first PTY output (pi banner)", () => firstByteAt !== null, 30000);
  const connectToFirstByteMs = firstByteAt - t0;
  // give the TUI a moment to finish drawing, then resize mid-session
  await sleep(3000);
  ctlResize = null; // the hello-triggered ctl ack must not satisfy the check below
  ws.send(JSON.stringify({ t: "resize", cols: 100, rows: 30 }));
  log("sent resize 100x30 mid-session");
  // A2, part 1: the host acks every resize with a {"t":"ctl"} message carrying
  // the real result of pty_master.resize(). Assert it — non-fatally: the
  // outcome lands in the A2 record after D1.
  try {
    await waitFor("host ctl ack for resize (t:ctl, msg:resize, cols:100, rows:30, ok:true)",
      () => ctlResize?.ok === true && ctlResize?.cols === 100 && ctlResize?.rows === 30, 10000);
  } catch (e) {
    log(`WARN: ${e.message}`);
  }
  await sleep(2500); // let the resize re-layout settle before typing (input-race fix)

  const readyMs = await sendPrompt(
    "SPIKE1-READY",
    "Reply with exactly this single token and nothing else: SPIKE1-READY",
  );
  // A1 claim kept honest: this driver has NO browser — it proves the WS-level
  // roundtrip (keystrokes echoed into the PTY stream and verified before
  // Enter, plus the authoritative session-tail reply event). The PTY-text
  // check below is deliberately weak (first word of the reply searched
  // anywhere in the accumulated stream, the echoed prompt included) and is
  // reported as supplementary evidence, never as the pass condition.
  const a1FirstWord = lastReply.preview.slice(0, 20).trim().split(/\s+/)[0] ?? "###";
  const a1PtyTextWeak = lastReply.preview.length > 0 && tuiText().includes(a1FirstWord);
  record("A1", "keystrokes over WS reach pi; assistant reply observed via session-tail (turn-final message_end)", lastReply.markerSeen,
    `submit->reply ${lastReply.replyMs} ms; typing->reply ${readyMs} ms; driver-start->record ${Date.now() - t0} ms; connect->first byte ${connectToFirstByteMs} ms; reply preview: ${JSON.stringify(lastReply.preview.slice(0, 100))}; marker in final reply: ${lastReply.markerSeen}; weak PTY-text check (first word anywhere in stream, echoed prompt included — not proof of rendering): ${a1PtyTextWeak}`);
  // A2 is NOT recorded here: the claim ("resize accepted by the PTY [ctl:ok];
  // session survived") is proven by the ctl ack asserted above plus the
  // marker-confirmed exchanges that complete AFTER the resize — see the A2
  // record following D1 below.

  // ---------- B: session observation ----------
  const bMs = await sendPrompt(
    "PIENV-DONE",
    "Use the bash tool to run exactly: env | grep ^PI_SESSION | sort — then reply with the value of PI_SESSION_FILE and the token PIENV-DONE",
  );
  const sawMessageEnd = events.some((e) => e.event?.type === "message_end");
  const sawToolStart = events.some((e) => e.event?.type === "tool_execution_start");
  const sawToolEnd = events.some((e) => e.event?.type === "tool_execution_end");
  const sessionFileEchoed = events.some(
    (e) => e.event?.type === "tool_execution_end" &&
      String(e.event?.preview ?? "").includes(".pi-run") && String(e.event?.preview ?? "").includes(".jsonl"),
  ) || tuiText().includes(".pi-run") && tuiText().includes(".jsonl");
  record("B1", "message_end events observed live", sawMessageEnd,
    `${events.filter((e) => e.event?.type === "message_end").length} message_end events received over WS`);
  record("B2", "tool_execution_start/end observed live", sawToolStart && sawToolEnd,
    `start=${events.filter((e) => e.event?.type === "tool_execution_start").length} end=${events.filter((e) => e.event?.type === "tool_execution_end").length}`);
  record("B3", "PI_SESSION_FILE/PI_SESSION_ID read from spawned process env", sessionFileEchoed,
    `bash tool child env echoed PI_SESSION_FILE pointing into .pi-run/sessions; step ${bMs} ms`);
  confirmExchange("B4", "PIENV-DONE", bMs);

  // ---------- C: MCP tool seam ----------
  const cMs = await sendPrompt(
    "MCP-DONE",
    "Call the MCP tool spike1_ping (it may appear as mcp__spike1-tools__spike1_ping). Then reply with the tool result and the token MCP-DONE",
  );
  const mcpEv = events.filter((e) => String(e.event?.toolName ?? "").includes("spike1_ping"));
  const mcpOk = mcpEv.some((e) => e.event?.type === "tool_execution_end" && !e.event?.isError &&
    String(e.event?.preview ?? "").includes("pong"));
  const cDetail = mcpEv.length
    ? `events: ${mcpEv.map((e) => e.event.type + (e.event.isError ? "(err)" : "")).join(",")}; previews contain "pong"`
    : "no spike1_ping tool events";
  record("C1", "pi called spike1_ping over MCP; result returned to agent", mcpOk,
    `${cDetail}; step ${cMs} ms`);
  confirmExchange("C2", "MCP-DONE", cMs);

  // ---------- D: permission gate ----------
  const dMs = await sendPrompt(
    "GATE-DONE",
    "Use the bash tool to run exactly this command: echo spike1-deny-me — then reply with what happened and the token GATE-DONE",
  );
  const denyEv = events.find(
    (e) => e.event?.type === "tool_execution_end" && e.event?.isError === true &&
      /spike1-gate|spike1-deny-me/.test(String(e.event?.preview ?? "")),
  );
  record("D1", "bash command containing spike1-deny-me blocked with visible reason", !!denyEv,
    denyEv ? `blocked toolResult preview: ${JSON.stringify(String(denyEv.event.preview).slice(0, 160))}; step ${dMs} ms`
           : `no blocked toolResult event; step ${dMs} ms`);
  confirmExchange("D2", "GATE-DONE", dMs);

  // A2, recorded here rather than at resize time: the ctl ack asserted right
  // after the resize proves the PTY itself accepted it (result of
  // pty_master.resize(), reported by the host as {"t":"ctl"}); reaching this
  // line with >=3 marker-confirmed exchanges proves the session survived it.
  // No browser is involved in this driver, so no claim about browser
  // rendering is made (manual observation only — see README).
  const ctlOk = ctlResize?.ok === true && ctlResize?.cols === 100 && ctlResize?.rows === 30;
  record("A2", "resize sent over WS and accepted by the PTY (host ctl:ok from pty_master.resize()); session survived (marker-confirmed post-resize exchanges)",
    ctlOk && postResizeExchanges.length >= 3,
    `resize 100x30 sent mid-session (+3.0 s after first byte); host ctl ack: ${ctlResize ? `cols=${ctlResize.cols} rows=${ctlResize.rows} ok=${ctlResize.ok}${ctlResize.err ? ` err=${String(ctlResize.err)}` : ""}` : "NOT RECEIVED"}; marker-confirmed post-resize exchanges: ${postResizeExchanges.length ? postResizeExchanges.join(", ") : "none"} (exchanges with a missing marker are recorded as FAIL and excluded; E is best-effort and has its own driver)`);

  // ---------- E: visual path ----------
  const eMs = await sendPrompt(
    "VISION-DONE",
    "Use the read tool on spool/spike1-icon.png and describe exactly the colors and shapes you see in it. Then use read on spool/spike1-diagram.svg and name the three boxes. End with the token VISION-DONE",
  );
  const assistantTexts = events
    .filter((e) => e.event?.type === "message_end" && e.event?.role === "assistant")
    .map((e) => String(e.event?.preview ?? ""))
    .join("\n")
    .toLowerCase();
  const sawRed = assistantTexts.includes("red") || assistantTexts.includes("czerw");
  const sawBlue = assistantTexts.includes("blue") || assistantTexts.includes("niebiesk");
  const sawCircle = assistantTexts.includes("circle") || assistantTexts.includes("koł") || assistantTexts.includes("kolo") || assistantTexts.includes("okr");
  const sawBoxes = /browser/.test(assistantTexts) && /host/.test(assistantTexts) && /\bpi\b/.test(assistantTexts);
  const visionOk = sawRed && sawBlue && sawCircle;
  record("E1", "agent references PNG image content (vision)", visionOk,
    visionOk ? `reply mentions red+blue+circle; step ${eMs} ms` :
    `honest result: reply did not reference colors/shapes (red=${sawRed} blue=${sawBlue} circle=${sawCircle}); model may lack vision; step ${eMs} ms — inspect session JSONL for image content blocks`);
  record("E2", "agent references SVG diagram content (text read)", sawBoxes,
    `reply names browser/host/pi boxes: ${sawBoxes}`);

  // graceful-ish shutdown of pi through its own TUI
  type("\x03"); // Ctrl+C
  await sleep(1000);
  type("\x03");
  await sleep(1000);

  writeFileSync(join(runDir, "e2e-result.json"), JSON.stringify({ startedAt: new Date(t0).toISOString(), wsPort, results, totalMs: Date.now() - t0 }, null, 2));
  writeFileSync(join(runDir, "e2e-tui-output.bin"), Buffer.concat(tuiChunks));
  log(`done: ${results.filter((r) => r.pass).length}/${results.length} checks passed`);
  const failed = results.filter((r) => !r.pass);
  ws.close();
  process.exitCode = failed.length ? 1 : 0;
}

main().catch((e) => {
  log(`FATAL: ${e.message}`);
  log(`STACK: ${e.stack ?? ""}`);
  try {
    writeFileSync(join(runDir, "e2e-result.json"), JSON.stringify({ startedAt: new Date(t0).toISOString(), wsPort, results, fatal: String(e), totalMs: Date.now() - t0 }, null, 2));
    writeFileSync(join(runDir, "e2e-tui-output.bin"), Buffer.concat(tuiChunks));
  } catch {}
  process.exitCode = 1;
  try { ws.close(); } catch {}
});
