import { describe, expect, it } from 'vitest';
import { base64ToBytes, bytesToBase64 } from './base64';

describe('bytesToBase64 / base64ToBytes', () => {
  it('roundtrips every byte value 0..255 (binary-safe)', () => {
    const bytes = Uint8Array.from({ length: 256 }, (_, i) => i);
    const round = base64ToBytes(bytesToBase64(bytes));
    expect(Array.from(round)).toEqual(Array.from(bytes));
  });

  it('roundtrips a large buffer across chunk boundaries', () => {
    const bytes = new Uint8Array(100_000);
    for (let i = 0; i < bytes.length; i += 1) {
      bytes[i] = (i * 7 + 13) & 0xff;
    }
    const round = base64ToBytes(bytesToBase64(bytes));
    expect(round.length).toBe(bytes.length);
    expect(Array.from(round)).toEqual(Array.from(bytes));
  });

  it('handles the empty buffer', () => {
    expect(bytesToBase64(new Uint8Array(0))).toBe('');
    expect(base64ToBytes('').length).toBe(0);
  });

  it('matches known encodings', () => {
    expect(bytesToBase64(new TextEncoder().encode('hi!'))).toBe('aGkh');
    expect(Array.from(base64ToBytes('aGkh'))).toEqual([0x68, 0x69, 0x21]);
  });

  it('rejects invalid base64 input', () => {
    expect(() => base64ToBytes('not base64!')).toThrow();
  });
});
