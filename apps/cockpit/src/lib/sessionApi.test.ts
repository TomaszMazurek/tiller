import { describe, expect, it, vi } from 'vitest';
import { sessionObserve, subscribeSessionEntry } from './sessionApi';
import type { RpcResponse } from './rpcClient';
import type { TermListenFn } from './termApi';

describe('sessionObserve', () => {
  it('sends the params and unwraps entries', async () => {
    const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
    const invokeFn = vi.fn(async (command: string, args?: Record<string, unknown>) => {
      calls.push({ command, args });
      const request = JSON.parse(String(args?.json ?? '{}')) as { method?: string };
      const response: RpcResponse = {
        id: 1,
        result:
          request.method === 'session.observe'
            ? {
                entries: [
                  {
                    kind: 'message',
                    id: '1',
                    role: 'user',
                    ts: 't',
                    chars: 3,
                    blocks: 1,
                  },
                ],
              }
            : {},
      };
      return JSON.stringify(response);
    });

    const entries = await sessionObserve(5, invokeFn);
    expect(entries).toHaveLength(1);
    expect(entries[0]).toEqual({
      kind: 'message',
      id: '1',
      role: 'user',
      ts: 't',
      chars: 3,
      blocks: 1,
    });
    const json = JSON.parse(String(calls[0]?.args?.json));
    expect(json.method).toBe('session.observe');
    expect(json.params).toEqual({ session_id: 5 });
  });

  it('rejects a result without an entries array', async () => {
    const invokeFn = vi.fn(async () => JSON.stringify({ id: 1, result: {} }));
    await expect(sessionObserve(1, invokeFn)).rejects.toThrow('entries');
  });
});

describe('subscribeSessionEntry', () => {
  it('listens on session:entry and unwraps the payload', async () => {
    const seen: Array<{ session_id: number; entry: unknown }> = [];
    const unlisten = vi.fn();
    const handlers = new Map<string, (event: { payload: unknown }) => void>();
    const listenFn: TermListenFn = (async <T,>(
      event: string,
      handler: (event: { payload: T }) => void,
    ) => {
      handlers.set(event, handler as (event: { payload: unknown }) => void);
      return unlisten;
    }) as TermListenFn;

    await subscribeSessionEntry((e) => seen.push(e), listenFn);
    expect([...handlers.keys()]).toEqual(['session:entry']);
    handlers.get('session:entry')?.({
      payload: {
        session_id: 2,
        entry: {
          kind: 'tool_call',
          id: 'c1',
          name: 'bash',
          ts: 't',
          tool_call_id: 'c1',
          args_chars: 21,
        },
      },
    });
    expect(seen).toEqual([
      {
        session_id: 2,
        entry: {
          kind: 'tool_call',
          id: 'c1',
          name: 'bash',
          ts: 't',
          tool_call_id: 'c1',
          args_chars: 21,
        },
      },
    ]);
  });
});
