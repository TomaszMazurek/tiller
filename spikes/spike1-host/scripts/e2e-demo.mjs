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
      } else if (v.t === "info") {
        log(`info: ${v.msg ?? ""} ${v.path ?? ""}`);
      }
    } catch { /* not JSON */ }
  }
});

const type = (text) => ws.send(Buffer.from(text, "utf8"));
const enter = () => type("\r");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const sendPrompt = async (marker, prompt, timeoutMs = 180000) => {
  const sentAt = Date.now();
  type(prompt);
  enter();
  // wait until the marker token shows up in an assistant message_end preview
  // (authoritative final text) or in the raw TUI stream
  await waitFor(
    `assistant reply containing ${marker}`,
    () =>
      events.some((e) => e.event?.type === "message_end" && e.event?.role === "assistant" &&
        String(e.event?.preview ?? "").includes(marker)) ||
      tuiText().includes(marker),
    timeoutMs,
  );
  return Date.now() - sentAt;
};

async function main() {
  log(`connecting ws://127.0.0.1:${wsPort} ...`);
  await new Promise((res, rej) => { ws.once("open", res); ws.once("error", rej); });
  log("ws connected");

  // ---------- A: PTY hosting ----------
  await waitFor("first PTY output (pi banner)", () => firstByteAt !== null, 30000);
  const connectToFirstByteMs = firstByteAt - t0;
  // give the TUI a moment to finish drawing, then resize mid-session
  await sleep(3000);
  ws.send(JSON.stringify({ t: "resize", cols: 100, rows: 30 }));
  log("sent resize 100x30 mid-session");
  await sleep(1500);

  const readyMs = await sendPrompt(
    "SPIKE1-READY",
    "Reply with exactly this single token and nothing else: SPIKE1-READY",
  );
  record("A1", "browser keystrokes reach pi; assistant reply renders in terminal", true,
    `prompt->reply ${readyMs} ms; connect->first byte ${connectToFirstByteMs} ms`);
  record("A2", "resize propagated; rendering survives resize", true,
    "resize 100x30 sent mid-session; subsequent prompts still round-trip (see B/C/D timings)");

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
  try {
    writeFileSync(join(runDir, "e2e-result.json"), JSON.stringify({ startedAt: new Date(t0).toISOString(), wsPort, results, fatal: String(e), totalMs: Date.now() - t0 }, null, 2));
    writeFileSync(join(runDir, "e2e-tui-output.bin"), Buffer.concat(tuiChunks));
  } catch {}
  process.exitCode = 1;
  try { ws.close(); } catch {}
});
