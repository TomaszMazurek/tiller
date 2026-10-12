import { act, renderHook } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import {
  useTerminalSession,
  type TerminalSessionDeps,
  type TerminalSurface,
} from './useTerminalSession';
import type { SessionId, TermExitEvent, TermOutputEvent, TermSpawnParams } from './termApi';
import { bytesToBase64 } from './base64';

/** Recorder stand-in for the xterm surface (xterm cannot mount in jsdom). */
interface FakeSurface extends TerminalSurface {
  writes: Uint8Array[];
  disposed: boolean;
  dataHandler: ((data: string) => void) | null;
}

function fakeSurface(): FakeSurface {
  const surface: FakeSurface = {
    cols: 97,
    rows: 31,
    writes: [],
    disposed: false,
    dataHandler: null,
    onData: (handler) => {
      surface.dataHandler = handler;
    },
    fit: () => {},
    write: (data) => {
      surface.writes.push(data);
    },
    dispose: () => {
      surface.disposed = true;
    },
  };
  return surface;
}

function outputEvent(session_id: SessionId, bytes: Uint8Array): TermOutputEvent {
  return { session_id, data: bytesToBase64(bytes) };
}

function exitEvent(session_id: SessionId, exit_code: number): TermExitEvent {
  return { session_id, exit_code, success: exit_code === 0, dsr_responses_sent: 0 };
}

function createHarness() {
  const container = document.createElement('div');
  document.body.appendChild(container);
  const surfaces: FakeSurface[] = [];
  const outputHandlers: Array<(event: TermOutputEvent) => void> = [];
  const exitHandlers: Array<(event: TermExitEvent) => void> = [];
  const spawn = vi.fn(async (_params: TermSpawnParams): Promise<SessionId> => 7);
  const kill = vi.fn(async () => {});
  const write = vi.fn(async () => {});
  const resize = vi.fn(async () => {});
  const deps: TerminalSessionDeps = {
    spawn,
    kill,
    write,
    resize,
    listenOutput: async (handler) => {
      outputHandlers.push(handler);
      return () => {
        const index = outputHandlers.indexOf(handler);
        if (index >= 0) {
          outputHandlers.splice(index, 1);
        }
      };
    },
    listenExit: async (handler) => {
      exitHandlers.push(handler);
      return () => {
        const index = exitHandlers.indexOf(handler);
        if (index >= 0) {
          exitHandlers.splice(index, 1);
        }
      };
    },
    createSurface: () => {
      const surface = fakeSurface();
      surfaces.push(surface);
      return surface;
    },
    observeResize: () => () => {},
  };
  const utils = renderHook(
    ({ d }: { d: TerminalSessionDeps }) => useTerminalSession({ current: container }, d),
    { initialProps: { d: deps } },
  );
  const emitOutput = (event: TermOutputEvent) =>
    act(() => {
      for (const handler of outputHandlers) {
        handler(event);
      }
    });
  const emitExit = (event: TermExitEvent) =>
    act(() => {
      for (const handler of exitHandlers) {
        handler(event);
      }
    });
  const emitRawOutput = (event: TermOutputEvent) => {
    for (const handler of outputHandlers) {
      handler(event);
    }
  };
  const emitRawExit = (event: TermExitEvent) => {
    for (const handler of exitHandlers) {
      handler(event);
    }
  };
  const start = () => act(async () => {
    await utils.result.current.start('pi', 'C:/tmp/project');
  });
  return {
    ...utils,
    surfaces,
    spawn,
    kill,
    write,
    resize,
    emitOutput,
    emitExit,
    emitRawOutput,
    emitRawExit,
    start,
  };
}

describe('useTerminalSession', () => {
  it('spawns with the surface size, routes output and input', async () => {
    const h = createHarness();
    await h.start();

    expect(h.spawn).toHaveBeenCalledTimes(1);
    const params = h.spawn.mock.calls[0]?.[0];
    expect(params).toMatchObject({ program: 'pi', cwd: 'C:/tmp/project', cols: 97, rows: 31 });
    expect(h.result.current.state).toMatchObject({ status: 'running', sessionId: 7 });

    // Output of our session lands in the surface; foreign ids do not.
    await h.emitOutput(outputEvent(7, Uint8Array.of(65)));
    await h.emitOutput(outputEvent(99, Uint8Array.of(66)));
    expect(h.surfaces[0]?.writes).toEqual([Uint8Array.of(65)]);

    // Typing flows back as a base64 write rpc.
    h.surfaces[0]?.dataHandler?.('a');
    expect(h.write).toHaveBeenCalledWith(7, new TextEncoder().encode('a'));

    await h.emitExit(exitEvent(7, 3));
    expect(h.result.current.state).toMatchObject({
      status: 'exited',
      exitCode: 3,
      sessionId: 7,
    });
  });

  it('restarts after exit: fresh surface, old session ignored, new one live', async () => {
    const h = createHarness();
    h.spawn.mockResolvedValueOnce(7).mockResolvedValueOnce(8);
    await h.start();
    await h.emitExit(exitEvent(7, 0));
    expect(h.result.current.state.status).toBe('exited');

    // Start again — the same button that used to be dead after an exit.
    await h.start();

    expect(h.result.current.state).toMatchObject({ status: 'running', sessionId: 8 });
    expect(h.surfaces).toHaveLength(2);
    expect(h.surfaces[0]?.disposed).toBe(true);
    expect(h.surfaces[1]?.disposed).toBe(false);

    // Old session's events must not reach the new surface.
    await h.emitOutput(outputEvent(7, Uint8Array.of(1)));
    expect(h.surfaces[1]?.writes).toEqual([]);
    await h.emitOutput(outputEvent(8, Uint8Array.of(2)));
    expect(h.surfaces[1]?.writes).toEqual([Uint8Array.of(2)]);
  });

  it('keeps events that raced ahead of the spawn resolution (listen-before-spawn)', async () => {
    const h = createHarness();
    const spawnResolvers: Array<(id: SessionId) => void> = [];
    h.spawn.mockImplementation(
      () =>
        new Promise<SessionId>((resolve) => {
          spawnResolvers.push(resolve);
        }),
    );
    // One act scope for the whole flow: while the spawn rpc is pending
    // the gates only buffer (no state updates), so the raw emissions are
    // safe un-wrapped; the dispatches happen on adopt inside this act.
    await act(async () => {
      const starting = h.result.current.start('pi', 'C:/tmp/project');
      // The backend emits output and exit while the spawn rpc is pending.
      h.emitRawOutput(outputEvent(5, Uint8Array.of(9)));
      h.emitRawExit(exitEvent(5, 2));
      // Bounded microtask drain: listen promises settle, then spawn fires.
      // try/finally guarantees the resolver runs even if the assertion
      // fails, so the start promise always settles and the run exits.
      for (let i = 0; i < 10 && spawnResolvers.length === 0; i += 1) {
        await Promise.resolve();
      }
      try {
        expect(spawnResolvers).toHaveLength(1);
      } finally {
        spawnResolvers[0]?.(5);
      }
      await starting;
    });

    expect(h.result.current.state).toMatchObject({ status: 'exited', exitCode: 2 });
    expect(h.surfaces[0]?.writes).toEqual([Uint8Array.of(9)]);
  });

  it('kills the live session when the panel unmounts', async () => {
    const h = createHarness();
    await h.start();
    h.unmount();

    expect(h.kill).toHaveBeenCalledWith(7);
    expect(h.surfaces[0]?.disposed).toBe(true);
  });

  it('kills the session when the spawn resolves after unmount', async () => {
    const h = createHarness();
    const spawnResolvers: Array<(id: SessionId) => void> = [];
    h.spawn.mockImplementation(
      () =>
        new Promise<SessionId>((resolve) => {
          spawnResolvers.push(resolve);
        }),
    );
    await act(async () => {
      const starting = h.result.current.start('pi', 'C:/tmp/project');
      // Bounded drain: the spawn rpc must be in flight before unmount;
      // try/finally keeps the start promise settling on failure too.
      for (let i = 0; i < 10 && spawnResolvers.length === 0; i += 1) {
        await Promise.resolve();
      }
      try {
        expect(spawnResolvers).toHaveLength(1);
      } finally {
        spawnResolvers[0]?.(9);
      }
      h.unmount();
      await starting;
    });

    expect(h.kill).toHaveBeenCalledWith(9);
  });

  it('does not spawn with an empty program or cwd', async () => {
    const h = createHarness();
    await h.result.current.start('  ', 'C:/tmp');
    await h.result.current.start('pi', '');
    expect(h.spawn).not.toHaveBeenCalled();
    expect(h.result.current.state.status).toBe('idle');
  });

  it('reports a failed spawn and allows a retry', async () => {
    const h = createHarness();
    h.spawn.mockRejectedValueOnce(new Error('program not found'));
    await h.start();
    expect(h.result.current.state).toMatchObject({ status: 'idle', error: 'program not found' });

    await h.start();
    expect(h.result.current.state).toMatchObject({ status: 'running', sessionId: 7 });
  });
});
