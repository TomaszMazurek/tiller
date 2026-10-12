/**
 * Binary-safe base64 helpers for the terminal wire (`term.write` input,
 * `term.output` payloads are base64 strings of raw PTY bytes).
 *
 * `btoa`/`atob` alone are not binary-safe (they operate on DOM strings),
 * so byte arrays are carried through a chunked binary string. The chunk
 * size stays under the `String.fromCharCode(...spread)` argument limit.
 */

/** Max bytes per `String.fromCharCode.apply` call (stack-argument limit). */
const CHUNK_SIZE = 0x8000;

export function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < bytes.length; i += CHUNK_SIZE) {
    const chunk = bytes.subarray(i, Math.min(i + CHUNK_SIZE, bytes.length));
    binary += String.fromCharCode(...chunk);
  }
  return btoa(binary);
}

export function base64ToBytes(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
