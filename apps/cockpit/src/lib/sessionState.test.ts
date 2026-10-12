import { describe, expect, it } from 'vitest';
import {
  initialTerminalSessionState,
  sessionStatusLabel,
  terminalSessionReducer,
} from './sessionState';

describe('terminalSessionReducer', () => {
  it('moves idle → starting → running on spawn', () => {
    let state = terminalSessionReducer(initialTerminalSessionState, { type: 'spawnRequested' });
    expect(state.status).toBe('starting');
    expect(state.sessionId).toBeNull();
    state = terminalSessionReducer(state, { type: 'spawned', sessionId: 5 });
    expect(state).toEqual({
      status: 'running',
      sessionId: 5,
      exitCode: null,
      signal: null,
      error: null,
    });
  });

  it('spawnRequested clears the previous session identity (restart)', () => {
    let state = terminalSessionReducer(initialTerminalSessionState, { type: 'spawned', sessionId: 5 });
    state = terminalSessionReducer(state, { type: 'exited', sessionId: 5, exitCode: 7 });
    state = terminalSessionReducer(state, { type: 'spawnRequested' });
    expect(state).toEqual({ ...initialTerminalSessionState, status: 'starting' });
    // A late exit from the previous session cannot match during starting.
    state = terminalSessionReducer(state, { type: 'exited', sessionId: 5, exitCode: 7 });
    expect(state.status).toBe('starting');
  });

  it('records the exit code and signal for its own session', () => {
    let state = terminalSessionReducer(initialTerminalSessionState, { type: 'spawned', sessionId: 5 });
    state = terminalSessionReducer(state, {
      type: 'exited',
      sessionId: 5,
      exitCode: 7,
      signal: null,
    });
    expect(state.status).toBe('exited');
    expect(state.exitCode).toBe(7);
  });

  it('ignores exit events from other sessions', () => {
    let state = terminalSessionReducer(initialTerminalSessionState, { type: 'spawned', sessionId: 5 });
    state = terminalSessionReducer(state, {
      type: 'exited',
      sessionId: 99,
      exitCode: 0,
    });
    expect(state.status).toBe('running');
  });

  it('keeps errors without changing the status of a live session', () => {
    let state = terminalSessionReducer(initialTerminalSessionState, { type: 'spawned', sessionId: 1 });
    state = terminalSessionReducer(state, { type: 'failed', message: 'boom' });
    expect(state.status).toBe('running');
    expect(state.error).toBe('boom');
  });

  it('a failed spawn returns to idle so Start can be pressed again', () => {
    let state = terminalSessionReducer(initialTerminalSessionState, { type: 'spawnRequested' });
    state = terminalSessionReducer(state, { type: 'failed', message: 'not found' });
    expect(state).toMatchObject({ status: 'idle', error: 'not found' });
  });

  it('labels the status chip', () => {
    expect(sessionStatusLabel(initialTerminalSessionState)).toBe('idle');
    expect(
      sessionStatusLabel({
        status: 'exited',
        sessionId: 1,
        exitCode: 3,
        signal: null,
        error: null,
      }),
    ).toBe('exited (code 3)');
    expect(
      sessionStatusLabel({
        status: 'exited',
        sessionId: 1,
        exitCode: 1,
        signal: 'SIGKILL',
        error: null,
      }),
    ).toBe('exited (SIGKILL)');
  });
});
