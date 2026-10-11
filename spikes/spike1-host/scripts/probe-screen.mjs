// probe-screen.mjs — connect to the WS bridge, resize, optionally send keys,
// then print the stripped screen text (or a context window around a keyword).
import WebSocket from "ws";

const keys = process.argv[2] ?? ""; // e.g. "\x0f" sent as literal via --keys
const keyword = process.argv[3];
const waitMs = Number(process.argv[4] ?? 5000);
const cols = Number(process.argv[5] ?? 120);
const rows = Number(process.argv[6] ?? 40);

const ws = new WebSocket("ws://127.0.0.1:8792");
ws.binaryType = "nodebuffer";
const chunks = [];
ws.on("message", (d, isB) => { if (isB) chunks.push(d); });
ws.on("open", () => {
  ws.send(JSON.stringify({ t: "hello", cols, rows }));
  if (keys) setTimeout(() => ws.send(Buffer.from(keys, "latin1")), Math.min(2500, waitMs / 2));
  setTimeout(() => {
    const t = Buffer.concat(chunks).toString("latin1").replace(
      /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|[\x00-\x09\x0b-\x1f\x7f]/g, "",
    );
    if (keyword) {
      const i = t.toLowerCase().indexOf(keyword.toLowerCase());
      console.log("context around", JSON.stringify(keyword), ":",
        i < 0 ? "NOT FOUND" : JSON.stringify(t.slice(Math.max(0, i - 250), i + 600)));
    } else {
      console.log(JSON.stringify(t.slice(-2500)));
    }
    ws.close();
    process.exit(0);
  }, waitMs);
});
ws.on("error", (e) => { console.log("WS ERROR", e.message); process.exit(1); });
