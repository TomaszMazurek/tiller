import { describe, expect, it, vi } from 'vitest';
import { SessionEventGate } from './sessionEventGate';
import type { TermExitEvent, TermOutputEvent } from './termApi';
import { bytesToBase64 } from './base64';

function output(session_id: number, bytes: Uint8Array): TermOutputEvent {
  return { session_id, data: bytesToBase64(bytes) };
}

function exit(session_id: number, exit_code: number): TermExitEvent {
  return { session_id, exit_code, success: exit_code === 0, dsr_responses_sent: 0 };
}

describe('SessionEventGate', () => {
  it('buffers while armed and replays matching events in order after adopt', () => {
    const applied: TermOutputEvent[] = [];
    const gate = new SessionEventGate<TermOutputEvent>((event) => applied.push(event));
    gate.arm();

    gate.onEvent(output(7, Uint8Array.of(1)));
    gate.onEvent(output(99, Uint8Array.of(2))); // foreign id: buffered, then dropped
    gate.onEvent(output(7, Uint8Array.of(3)));
    expect(applied).toEqual([]);

    gate.adopt(7);
    expect(applied).toEqual([output(7, Uint8Array.of(1)), output(7, Uint8Array.of(3))]);
  });

  it('applies exit that arrived before the id was known', () => {
    const apply = vi.fn();
    const gate = new SessionEventGate<TermExitEvent>(apply);
    gate.arm();
    gate.onEvent(exit(5, 3));
    expect(apply).not.toHaveBeenCalled();

    gate.adopt(5);
    expect(apply).toHaveBeenCalledTimes(1);
    expect(apply).toHaveBeenCalledWith(exit(5, 3));
  });

  it('drops foreign ids after adoption and matching ones still apply', () => {
    const applied: number[] = [];
    const gate = new SessionEventGate<TermExitEvent>((event) => applied.push(event.exit_code));
    gate.arm();
    gate.adopt(5);

    gate.onEvent(exit(99, 1));
    gate.onEvent(exit(5, 2));
    expect(applied).toEqual([2]);
  });

  it('drops everything when idle (not armed, not adopted)', () => {
    const apply = vi.fn();
    const gate = new SessionEventGate<TermExitEvent>(apply);
    gate.onEvent(exit(5, 0));
    expect(apply).not.toHaveBeenCalled();
  });

  it('disarm drops the buffered events of a failed spawn', () => {
    const apply = vi.fn();
    const gate = new SessionEventGate<TermExitEvent>(apply);
    gate.arm();
    gate.onEvent(exit(5, 0));
    gate.disarm();
    gate.adopt(5);
    expect(apply).not.toHaveBeenCalled();
  });

  it('release stops applying events from the adopted session', () => {
    const apply = vi.fn();
    const gate = new SessionEventGate<TermExitEvent>(apply);
    gate.arm();
    gate.adopt(5);
    gate.release();
    gate.onEvent(exit(5, 0));
    expect(apply).not.toHaveBeenCalled();
  });
});
