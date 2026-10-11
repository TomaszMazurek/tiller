// sanity-probe.mjs — one tiny prompt through the WS bridge; verifies that BOTH
// binary frames (PTY output) and text frames (host session-tail events) flow.
import WebSocket from "ws";

const t0 = Date.now();
const ws = new WebSocket("ws://127.0.0.1:8792");
ws.binaryType = "nodebuffer";
let binBytes = 0, textFrames = 0, sawUserEnd = false, sawAssistantEnd = false, echoed = false, replied = false;
const log = (m) => console.log(`[probe +${((Date.now() - t0) / 1000).toFixed(1)}s] ${m}`);

ws.on("message", (data, isBinary) => {
  if (isBinary) {
    binBytes += data.length;
    const s = data.toString("latin1");
    if (s.includes("SANITY-OK-PROMPT")) echoed = true;
    if (s.includes("SANITY-OK-REPLY")) replied = true;
    return;
  }
  textFrames++;
  try {
    const v = JSON.parse(data.toString("utf8"));
    if (v.t === "info") log(`info: ${v.msg ?? ""} ${v.path ?? ""}`);
    if (v.t === "ev") {
      const e = v.event ?? {};
      log(`ev ${e.type} ${e.role ?? e.toolName ?? ""} ${JSON.stringify(String(e.preview ?? "").slice(0, 80))}`);
      if (e.type === "message_end" && e.role === "user" && String(e.preview ?? "").includes("SANITY")) sawUserEnd = true;
      if (e.type === "message_end" && e.role === "assistant") sawAssistantEnd = true;
    }
  } catch { /* ignore */ }
});

ws.on("open", () => {
  log("connected");
  ws.send(JSON.stringify({ t: "hello", cols: 120, rows: 40 }));
  setTimeout(() => { log("typing prompt"); ws.send(Buffer.from("Reply with exactly this single token and nothing else: SANITY-OK-REPLY")); }, 2500);
  setTimeout(() => { log("pressing Enter"); ws.send(Buffer.from("\r")); }, 3500);
  setTimeout(() => {
    console.log(JSON.stringify({ binBytes, textFrames, echoed, replied, sawUserEnd, sawAssistantEnd }, null, 0));
    ws.close();
    process.exit(sawUserEnd && sawAssistantEnd && echoed ? 0 : 1);
  }, Number(process.argv[2] ?? 90000) + 4000);
});
ws.on("error", (e) => { console.log("WS ERROR", e.message); process.exit(1); });
