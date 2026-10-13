/**
 * Terminal session state as a pure reducer — the panel component stays a
 * thin (untestable-in-jsdom) wrapper around this.
 */

import type { SessionId } from './termApi';

export type TerminalSessionStatus = 'idle' | 'starting' | 'running' | 'exited';

export interface TerminalSessionState {
  status: TerminalSessionStatus;
  sessionId: SessionId | null;
  exitCode: number | null;
  signal: string | null;
  error: string | null;
}

export type TerminalSessionAction =
  /** Start pressed: a spawn request is in flight. Stale session
   * identity and exit info clear here, so a restart never renders the
   * previous session's id/code and its late events cannot match. */
  | { type: 'spawnRequested' }
  /** The backend answered with a live session id. */
  | { type: 'spawned'; sessionId: SessionId }
  /** `term:exit` arrived for our session. */
  | { type: 'exited'; sessionId: SessionId; exitCode: number; signal?: string | null }
  /** An rpc call failed (spawn error, write error…). */
  | { type: 'failed'; message: string }
  // No `reset` action on purpose: a restart dispatches `spawnRequested`,
  // which already clears every session field (see its case).

export const initialTerminalSessionState: TerminalSessionState = {
  status: 'idle',
  sessionId: null,
  exitCode: null,
  signal: null,
  error: null,
};

export function terminalSessionReducer(
  state: TerminalSessionState,
  action: TerminalSessionAction,
): TerminalSessionState {
  switch (action.type) {
    case 'spawnRequested':
      return {
        status: 'starting',
        sessionId: null,
        exitCode: null,
        signal: null,
        error: null,
      };
    case 'spawned':
      return {
        status: 'running',
        sessionId: action.sessionId,
        exitCode: null,
        signal: null,
        error: null,
      };
    case 'exited':
      // Exit events carry their own session id: never let a stale event
      // from a previous session flip the current one.
      if (state.sessionId !== action.sessionId) {
        return state;
      }
      return {
        ...state,
        status: 'exited',
        exitCode: action.exitCode,
        signal: action.signal ?? null,
      };
    case 'failed':
      // A failed spawn attempt must unlock Start again (idle); an error
      // mid-session (write/resize) keeps the session status.
      if (state.status === 'starting') {
        return { ...state, status: 'idle', error: action.message };
      }
      return { ...state, error: action.message };
  }
}

/** Human label for the panel status chip. */
export function sessionStatusLabel(state: TerminalSessionState): string {
  switch (state.status) {
    case 'idle':
      return 'idle';
    case 'starting':
      return 'starting…';
    case 'running':
      return 'running';
    case 'exited':
      return state.signal
        ? `exited (${state.signal})`
        : `exited (code ${state.exitCode ?? '?'})`;
  }
}
