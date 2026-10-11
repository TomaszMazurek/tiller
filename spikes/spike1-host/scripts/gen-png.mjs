// Generates spool/spike1-icon.png — a deliberately simple, describable image:
// left half red, right half blue, white filled circle in the center, 2px black border.
// Pure Node (zlib), no image libraries. Run: node scripts/gen-png.mjs
import { deflateSync } from "node:zlib";
import { writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const W = 96, H = 96;
const RED = [224, 64, 64];
const BLUE = [64, 96, 224];
const WHITE = [255, 255, 255];
const BLACK = [0, 0, 0];

const cx = W / 2, cy = H / 2, r = 20;

function pixel(x, y) {
  const border = x < 2 || y < 2 || x >= W - 2 || y >= H - 2;
  const inCircle = (x - cx) ** 2 + (y - cy) ** 2 <= r * r;
  if (border) return BLACK;
  if (inCircle) return WHITE;
  return x < W / 2 ? RED : BLUE;
}

// raw scanlines: each row = filter byte 0 + RGB
const raw = Buffer.alloc(H * (1 + W * 3));
for (let y = 0; y < H; y++) {
  const row = y * (1 + W * 3);
  raw[row] = 0;
  for (let x = 0; x < W; x++) {
    const [R, G, B] = pixel(x, y);
    const o = row + 1 + x * 3;
    raw[o] = R; raw[o + 1] = G; raw[o + 2] = B;
  }
}

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(W, 0);
ihdr.writeUInt32BE(H, 4);
ihdr[8] = 8;  // bit depth
ihdr[9] = 2;  // color type: truecolor RGB
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);

const out = join(dirname(fileURLToPath(import.meta.url)), "..", "spool", "spike1-icon.png");
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, png);
console.log(`wrote ${out} (${png.length} bytes)`);
