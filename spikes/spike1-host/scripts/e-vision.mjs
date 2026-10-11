// e-vision.mjs — criterion E only: PNG/SVG read by pi, chunked typing
// (xterm.js parity: keystroke-by-keystroke; long single-burst writes into the
// composer were observed to be dropped).
import WebSocket from "ws";
import { writeFileSync, appendFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const runDir = join(root, ".pi-run");
const t0 = Date.now();
const log = (m) => { const l = `[e-vis +${((Date.now() - t0) / 1000).toFixed(1)}s] ${m}`; console.log(l); appendFileSync(join(runDir, "e-vision.log"), l + "\n"); };

const PROMPT = "Use the read tool on spool/spike1-icon.png and describe exactly the colors and shapes you see in it. Then use read on spool/spike1-diagram.svg and name the three boxes. End with the token VISION-DONE";
const MARKER = "VISION-DONE";

const ws = new WebSocket("ws://127.0.0.1:8792");
ws.binaryType = "nodebuffer";
const chunks = [];
const events = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const tuiText = () => Buffer.concat(chunks).toString("utf8");

ws.on("message", (d, isB) => {
  if (isB) { chunks.push(d); return; }
  try {
    const v = JSON.parse(d.toString("utf8"));
    if (v.t === "ev") { events.push(v); log(`ev ${v.event?.type} ${v.event?.role ?? v.event?.toolName ?? ""} ${JSON.stringify(String(v.event?.preview ?? "").slice(0, 70))}`); }
  } catch { /* ignore */ }
});

const typeChunked = async (text, size = 16, delayMs = 40) => {
  for (let i = 0; i < text.length; i += size) {
    ws.send(Buffer.from(text.slice(i, i + size), "utf8"));
    await sleep(delayMs);
  }
};

ws.on("open", async () => {
  log("connected");
  ws.send(JSON.stringify({ t: "hello", cols: 120, rows: 40 }));
  await sleep(2500); // settle after attach
  let typed = false;
  for (let attempt = 1; attempt <= 3 && !typed; attempt++) {
    await typeChunked(PROMPT);
    await sleep(700);
    typed = tuiText().includes(MARKER);
    log(`echo attempt ${attempt}: ${typed}`);
    if (!typed) { ws.send(Buffer.from("\x15")); await sleep(400); } // Ctrl+U kill line
  }
  if (!typed) { log("FATAL: prompt never echoed"); writeFileSync(join(runDir, "e-vision-result.json"), JSON.stringify({ typed: false }, null, 2)); process.exit(1); }
  ws.send(Buffer.from("\r"));
  log("submitted; waiting for assistant message_end…");
  const deadline = Date.now() + 300000;
  while (Date.now() < deadline) {
    await sleep(1000);
    const ends = events.filter((e) => e.event?.type === "message_end" && e.event?.role === "assistant");
    const reads = events.filter((e) => String(e.event?.toolName ?? "").includes("read"));
    const done = ends.some((e) => String(e.event?.preview ?? "").includes(MARKER));
    if (done || (ends.length >= 2 && reads.length >= 2 && ends.at(-1) !== ends.at(-2))) {
      await sleep(1500); // let the final message_end land
      break;
    }
  }
  const assistantTexts = events
    .filter((e) => e.event?.type === "message_end" && e.event?.role === "assistant")
    .map((e) => String(e.event?.preview ?? ""))
    .join("\n")
    .toLowerCase();
  const sawRed = /red|czerw/.test(assistantTexts);
  const sawBlue = /blue|niebiesk/.test(assistantTexts);
  const sawCircle = /circle|koł|kolo|okr/.test(assistantTexts);
  const sawBoxes = /browser/.test(assistantTexts) && /host/.test(assistantTexts) && /\bpi\b/.test(assistantTexts);
  const result = {
    typedEcho: true,
    readToolCalls: events.filter((e) => String(e.event?.toolName ?? "").includes("read")).length,
    markerSeen: assistantTexts.includes(MARKER.toLowerCase()),
    png: { sawRed, sawBlue, sawCircle, pass: sawRed && sawBlue && sawCircle },
    svg: { sawBoxes, pass: sawBoxes },
    assistantPreview: events.filter((e) => e.event?.type === "message_end" && e.event?.role === "assistant").map((e) => String(e.event?.preview ?? "").slice(0, 400)),
  };
  writeFileSync(join(runDir, "e-vision-result.json"), JSON.stringify(result, null, 2));
  log(`RESULT: png.pass=${result.png.pass} svg.pass=${result.svg.pass} marker=${result.markerSeen} readCalls=${result.readToolCalls}`);
  ws.close();
  process.exit(0);
});
ws.on("error", (e) => { log(`WS ERROR ${e.message}`); process.exit(1); });
