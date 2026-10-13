import { describe, expect, it, vi } from 'vitest';
import { bytesToBase64 } from './base64';
import {
  applyTermOutput,
  subscribeTermExit,
  subscribeTermOutput,
  termKill,
  termResize,
  termSpawn,
  termStats,
  termWrite,
  type TermListenFn,
} from './termApi';
import type { RpcResponse } from './rpcClient';

/** invoke mock answering a serialized RpcResponse per method. */
function invokeAnswering(results: Record<string, unknown>) {
  const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
  const invokeFn = vi.fn(async (command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args });
    const request = JSON.parse(String(args?.json ?? '{}')) as { method?: string };
    const response: RpcResponse = { id: 1, result: results[request.method ?? ''] ?? {} };
    return JSON.stringify(response);
  });
  return { invokeFn, calls };
}

describe('termSpawn', () => {
  it('sends the params and unwraps session_id', async () => {
    const { invokeFn, calls } = invokeAnswering({ 'term.spawn': { session_id: 7 } });
    const id = await termSpawn({ program: 'pi', cwd: 'C:/Repos/tiller' }, invokeFn);
    expect(id).toBe(7);
    const json = JSON.parse(String(calls[0]?.args?.json));
    expect(json.method).toBe('term.spawn');
    expect(json.params).toEqual({ program: 'pi', cwd: 'C:/Repos/tiller' });
  });

  it('rejects a result without session_id', async () => {
    const { invokeFn } = invokeAnswering({ 'term.spawn': {} });
    await expect(termSpawn({ program: 'pi', cwd: 'x' }, invokeFn)).rejects.toThrow('session_id');
  });
});

describe('termWrite', () => {
  it('base64-encodes the bytes on the wire', async () => {
    const { invokeFn, calls } = invokeAnswering({ 'term.write': {} });
    await termWrite(3, new TextEncoder().encode('hi!'), invokeFn);
    const json = JSON.parse(String(calls[0]?.args?.json));
    expect(json.method).toBe('term.write');
    expect(json.params).toEqual({ session_id: 3, data: 'aGkh' });
  });
});

describe('termResize / termKill / termStats', () => {
  it('send their shapes', async () => {
    const { invokeFn, calls } = invokeAnswering({
      'term.resize': {},
      'term.kill': {},
      'term.stats': { dsr_responses_sent: 2 },
    });
    await termResize(3, 120, 40, invokeFn);
    await termKill(3, invokeFn);
    expect(await termStats(3, invokeFn)).toEqual({ dsr_responses_sent: 2 });
    const shapes = calls.map((call) => {
      const json = JSON.parse(String(call.args?.json));
      return { method: json.method, params: json.params };
    });
    expect(shapes).toEqual([
      { method: 'term.resize', params: { session_id: 3, cols: 120, rows: 40 } },
      { method: 'term.kill', params: { session_id: 3 } },
      { method: 'term.stats', params: { session_id: 3 } },
    ]);
  });
});

describe('applyTermOutput', () => {
  it('decodes the payload and writes the bytes', () => {
    const target = { write: vi.fn() };
    applyTermOutput(target, { session_id: 1, data: bytesToBase64(Uint8Array.of(1, 2, 0xff)) });
    expect(target.write).toHaveBeenCalledWith(Uint8Array.of(1, 2, 0xff));
  });
});

describe('subscribeTermOutput / subscribeTermExit', () => {
  it('listens on the right event and unwraps payloads', async () => {
    const handlers = new Map<string, (event: { payload: unknown }) => void>();
    const unlisten = vi.fn();
    const listenFn: TermListenFn = (async <T,>(
      event: string,
      handler: (event: { payload: T }) => void,
    ) => {
      handlers.set(event, handler as (event: { payload: unknown }) => void);
      return unlisten;
    }) as TermListenFn;

    const outputs: unknown[] = [];
    const exits: unknown[] = [];
    const unlistenOutput = await subscribeTermOutput((e) => outputs.push(e), listenFn);
    const unlistenExit = await subscribeTermExit((e) => exits.push(e), listenFn);

    expect([...handlers.keys()].sort()).toEqual(['term:exit', 'term:output']);
    handlers.get('term:output')?.({ payload: { session_id: 1, data: 'aGkh' } });
    handlers.get('term:exit')?.({
      payload: { session_id: 1, exit_code: 0, success: true, dsr_responses_sent: 4 },
    });
    expect(outputs).toEqual([{ session_id: 1, data: 'aGkh' }]);
    expect(exits).toEqual([{ session_id: 1, exit_code: 0, success: true, dsr_responses_sent: 4 }]);

    unlistenOutput();
    unlistenExit();
    expect(unlisten).toHaveBeenCalledTimes(2);
  });
});
