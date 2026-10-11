// inspect-tui.mjs — strip ANSI from a raw TUI capture and print the last screen text.
import { readFileSync } from "node:fs";
const file = process.argv[2] ?? ".pi-run/e2e-tui-output.bin";
const buf = readFileSync(file);
const s = buf.toString("latin1");
const strip = (x) =>
  x.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|[\x00-\x09\x0b-\x1f\x7f]/g, "");
const t = strip(s);
console.log("file:", file, "bytes:", buf.length, "stripped len:", t.length);
const tailLen = Number(process.argv[3] ?? 1500);
console.log("---- last", tailLen, "chars ----");
console.log(JSON.stringify(t.slice(-tailLen)));
