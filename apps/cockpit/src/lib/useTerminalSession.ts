/**
 * Terminal session lifecycle for the terminal panel — every decision the
 * panel used to make inline, as one testable hook.
 *
 * Owns the full session cycle: surface (xterm) creation/disposal, resize
 * scheduler + observer, spawn/kill/write/resize rpc wiring, notification
 * routing and the unmount/exit rules:
 *
 * - Listen **before** spawn: subscriptions go up before `term.spawn` is
 *   called, and [`SessionEventGate`] buffers events whose session id is
 *   not known yet — a short-lived program can emit its whole output and
 *   exit before the spawn rpc resolves (the old code lost that race).
 * - Restart after exit works: `start` tears the previous session's
 *   surface, observer, scheduler and listeners down before building the
 *   new one.
 * - Closing the panel kills the hosted program (fire-and-forget: a
 *   SESSION_NOT_FOUND from an already-exited session is fine).
 * - A spawn that resolves after unmount kills its session immediately.
 *
 * The panel stays a thin JSX wrapper because every dependency (rpc
 * helpers, listener factory, surface factory, resize observer) is
 * injected: jsdom has no canvas, so xterm itself cannot mount in tests.
 */

import { useCallback, useEffect, useReducer, useRef, type RefObject } from 'react';
import { createResizeScheduler, type ResizeScheduler } from './resizeScheduler';
import { SessionEventGate } from './sessionEventGate';
import {
  applyTermOutput,
  type SessionId,
  type TermExitEvent,
  type TermOutputEvent,
  type TermSpawnParams,
} from './termApi';
import {
  initialTerminalSessionState,
  terminalSessionReducer,
  type TerminalSessionState,
} from './sessionState';

/**
 * The terminal emulator surface the hook drives. Implemented by xterm in
 * the panel; faked in tests. `write` accepts raw bytes (output chunks).
 */
export interface TerminalSurface {
  readonly cols: number;
  readonly rows: number;
  onData(handler: (data: string) => void): void;
  fit(): void;
  write(data: Uint8Array): void;
  dispose(): void;
}

/** Injected seams of {@link useTerminalSession} (defaults live in the panel). */
export interface TerminalSessionDeps {
  spawn(params: TermSpawnParams): Promise<SessionId>;
  write(sessionId: SessionId, data: Uint8Array): Promise<void>;
  resize(sessionId: SessionId, cols: number, rows: number): Promise<void>;
  kill(sessionId: SessionId): Promise<void>;
  listenOutput(handler: (event: TermOutputEvent) => void): Promise<() => void>;
  listenExit(handler: (event: TermExitEvent) => void): Promise<() => void>;
  /** Create the emulator surface mounted in `container`. */
  createSurface(container: HTMLElement): TerminalSurface;
  /** Observe `target`'s size; return the disconnect function. */
  observeResize(target: Element, callback: () => void): () => void;
}

export interface UseTerminalSession {
  state: TerminalSessionState;
  /** Start (or restart after exit) a session; resolves when wired up. */
  start(program: string, cwd: string): Promise<void>;
  /** Ask the backend to kill the live session (exit arrives as event). */
  kill(): void;
}

export function useTerminalSession(
  containerRef: RefObject<HTMLElement | null>,
  deps: TerminalSessionDeps,
): UseTerminalSession {
  const [state, dispatch] = useReducer(terminalSessionReducer, initialTerminalSessionState);

  // Control-flow truth for async code: React state is stale inside the
  // async start() chain, refs are not. 'exited' permits a restart.
  const phaseRef = useRef<'idle' | 'starting' | 'running' | 'exited'>('idle');
  const sessionIdRef = useRef<SessionId | null>(null);
  const unmountedRef = useRef(false);
  const surfaceRef = useRef<TerminalSurface | null>(null);
  const schedulerRef = useRef<ResizeScheduler | null>(null);
  const disconnectResizeRef = useRef<(() => void) | null>(null);
  const unlistenRef = useRef<(() => void) | null>(null);
  const gatesRef = useRef<{
    output: SessionEventGate<TermOutputEvent>;
    exit: SessionEventGate<TermExitEvent>;
  } | null>(null);

  /** Dispose the previous session's surface, observer, scheduler and listeners. */
  const teardownSession = useCallback(() => {
    unlistenRef.current?.();
    unlistenRef.current = null;
    disconnectResizeRef.current?.();
    disconnectResizeRef.current = null;
    schedulerRef.current?.cancel();
    schedulerRef.current = null;
    surfaceRef.current?.dispose();
    surfaceRef.current = null;
    gatesRef.current = null;
  }, []);

  const start = useCallback(
    async (program: string, cwd: string): Promise<void> => {
      if (phaseRef.current === 'starting' || phaseRef.current === 'running') {
        return;
      }
      if (program.trim() === '' || cwd.trim() === '') {
        // The panel disables Start on empty fields; keep the seam safe.
        return;
      }
      const container = containerRef.current;
      if (container === null) {
        return;
      }
      phaseRef.current = 'starting';
      dispatch({ type: 'spawnRequested' });
      // A restart must not build on the dead session's leftovers.
      teardownSession();

      const surface = deps.createSurface(container);
      surfaceRef.current = surface;
      try {
        surface.fit();
      } catch {
        // container not laid out yet: spawn at the default size
      }

      const outputGate = new SessionEventGate<TermOutputEvent>((event) => {
        const current = surfaceRef.current;
        if (current !== null) {
          applyTermOutput(current, event);
        }
      });
      const exitGate = new SessionEventGate<TermExitEvent>((event) => {
        phaseRef.current = 'exited';
        sessionIdRef.current = null;
        dispatch({
          type: 'exited',
          sessionId: event.session_id,
          exitCode: event.exit_code,
          signal: event.signal,
        });
      });
      gatesRef.current = { output: outputGate, exit: exitGate };
      outputGate.arm();
      exitGate.arm();

      try {
        // Subscribe BEFORE spawning: the backend may emit this session's
        // output/exit before the spawn rpc resolves, and the armed gates
        // buffer events until the id is adopted (foreign ids drop).
        const [unlistenOutput, unlistenExit] = await Promise.all([
          deps.listenOutput((event) => gatesRef.current?.output.onEvent(event)),
          deps.listenExit((event) => gatesRef.current?.exit.onEvent(event)),
        ]);
        if (unmountedRef.current) {
          unlistenOutput();
          unlistenExit();
          return;
        }
        unlistenRef.current = () => {
          unlistenOutput();
          unlistenExit();
        };

        const sessionId = await deps.spawn({
          program,
          cwd,
          cols: surface.cols,
          rows: surface.rows,
        });
        if (unmountedRef.current) {
          // The panel went away while the rpc was in flight: the backend
          // session exists but nobody will ever drive it — kill it now.
          void deps.kill(sessionId).catch(() => undefined);
          return;
        }
        sessionIdRef.current = sessionId;
        phaseRef.current = 'running';
        dispatch({ type: 'spawned', sessionId });
        // Adopt only after the spawned state landed: a buffered exit
        // (race) must flip running → exited, not be overwritten by it.
        outputGate.adopt(sessionId);
        exitGate.adopt(sessionId);

        const encoder = new TextEncoder();
        surface.onData((data) => {
          const id = sessionIdRef.current;
          if (id !== null) {
            // The backend reports gone sessions via term:exit; a lost
            // keystroke at teardown is not worth a UI error.
            void deps.write(id, encoder.encode(data)).catch(() => undefined);
          }
        });

        const scheduler = createResizeScheduler(() => {
          const current = surfaceRef.current;
          const id = sessionIdRef.current;
          if (current === null || id === null) {
            return;
          }
          // Sizes observed at schedule time are stale by the quiet
          // window's end: fit first, then send the emulator's size.
          current.fit();
          void deps.resize(id, current.cols, current.rows).catch(() => undefined);
        });
        schedulerRef.current = scheduler;
        disconnectResizeRef.current = deps.observeResize(container, () => {
          const current = surfaceRef.current;
          if (current !== null) {
            schedulerRef.current?.schedule(current.cols, current.rows);
          }
        });
      } catch (cause) {
        phaseRef.current = 'idle';
        outputGate.disarm();
        exitGate.disarm();
        const message = cause instanceof Error ? cause.message : String(cause);
        dispatch({ type: 'failed', message });
        teardownSession();
      }
    },
    [containerRef, deps, teardownSession],
  );

  const kill = useCallback(() => {
    const id = sessionIdRef.current;
    if (id !== null) {
      void deps.kill(id).catch(() => undefined);
    }
  }, [deps]);

  // Unmount: the hosted program belongs to the app, so kill it instead of
  // leaking a headless PTY (fire-and-forget: SESSION_NOT_FOUND is fine —
  // the program may have exited already).
  useEffect(() => {
    unmountedRef.current = false;
    return () => {
      unmountedRef.current = true;
      const id = sessionIdRef.current;
      sessionIdRef.current = null;
      if (id !== null) {
        void deps.kill(id).catch(() => undefined);
      }
      teardownSession();
    };
  }, [deps, teardownSession]);

  return { state, start, kill };
}
