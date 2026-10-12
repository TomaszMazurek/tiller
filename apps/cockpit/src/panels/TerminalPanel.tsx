import { useEffect, useMemo, useRef, useState } from 'react';
import type { IDockviewPanelProps } from 'dockview-react';
import { FitAddon } from '@xterm/addon-fit';
import { Terminal } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
import {
  sessionStatusLabel,
} from '../lib/sessionState';
import {
  useTerminalSession,
  type TerminalSessionDeps,
  type TerminalSurface,
} from '../lib/useTerminalSession';
import {
  subscribeTermExit,
  subscribeTermOutput,
  termKill,
  termResize,
  termSpawn,
  termWrite,
} from '../lib/termApi';

const DEFAULT_PROGRAM = 'pi';

/**
 * The xterm-backed surface: the production implementation of the hook's
 * `TerminalSurface` seam (tests fake it — xterm cannot mount in jsdom).
 */
function createXtermSurface(container: HTMLElement): TerminalSurface {
  const terminal = new Terminal({ convertEol: false });
  const fit = new FitAddon();
  terminal.loadAddon(fit);
  terminal.open(container);
  return {
    get cols() {
      return terminal.cols;
    },
    get rows() {
      return terminal.rows;
    },
    onData: (handler) => {
      terminal.onData(handler);
    },
    fit: () => {
      fit.fit();
    },
    write: (data) => {
      terminal.write(data);
    },
    dispose: () => {
      terminal.dispose();
    },
  };
}

/**
 * Terminal panel: hosts a program (pi by default) in a real PTY through
 * the `term.*` rpc seam. This component is deliberately a thin wrapper —
 * the whole session lifecycle (spawn/kill/restart, listen-before-spawn,
 * unmount rules, resize scheduling) lives in `useTerminalSession`, which
 * unit tests drive with injected fakes because xterm.js cannot mount
 * under jsdom (it needs a canvas).
 */
export interface TerminalPanelProps extends IDockviewPanelProps {
  /** Reports the live session id upward (the timeline observes it). */
  onActiveSessionChange?: (sessionId: import('../lib/termApi').SessionId | null) => void;
}

export function TerminalPanel({ onActiveSessionChange }: TerminalPanelProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [program, setProgram] = useState(DEFAULT_PROGRAM);
  // No machine-specific default (a wrong cwd fails the spawn on any other
  // machine): the operator names the project directory; Start stays
  // disabled until both fields are filled.
  const [cwd, setCwd] = useState('');

  const deps = useMemo<TerminalSessionDeps>(
    () => ({
      spawn: termSpawn,
      write: termWrite,
      resize: termResize,
      kill: termKill,
      listenOutput: subscribeTermOutput,
      listenExit: subscribeTermExit,
      createSurface: createXtermSurface,
      observeResize: (target, callback) => {
        const observer = new ResizeObserver(callback);
        observer.observe(target);
        return () => observer.disconnect();
      },
    }),
    [],
  );
  const { state, start, kill } = useTerminalSession(containerRef, deps);

  // The lifted session id (App state): the reducer already nulls it on a
  // restart request, so the timeline resets with the terminal.
  useEffect(() => {
    onActiveSessionChange?.(state.sessionId);
  }, [state.sessionId, onActiveSessionChange]);

  const running = state.status === 'running';
  const busy = state.status === 'starting';
  const canStart =
    !running && !busy && program.trim() !== '' && cwd.trim() !== '';

  return (
    <div className="terminal-panel">
      <div className="terminal-panel__toolbar">
        <label className="terminal-panel__field">
          program
          <input
            value={program}
            onChange={(event) => setProgram(event.target.value)}
            spellCheck={false}
            disabled={running || busy}
            data-testid="terminal-program"
          />
        </label>
        <label className="terminal-panel__field">
          cwd
          <input
            value={cwd}
            onChange={(event) => setCwd(event.target.value)}
            placeholder="project directory"
            spellCheck={false}
            disabled={running || busy}
            data-testid="terminal-cwd"
          />
        </label>
        <button
          type="button"
          onClick={() => void start(program, cwd)}
          disabled={!canStart}
          data-testid="terminal-start"
        >
          Start
        </button>
        <button type="button" onClick={kill} disabled={!running} data-testid="terminal-kill">
          Kill
        </button>
        <span
          className={`terminal-panel__status terminal-panel__status--${state.status}`}
          data-testid="terminal-status"
        >
          {sessionStatusLabel(state)}
        </span>
        {state.error !== null && (
          <span className="terminal-panel__error" data-testid="terminal-error">
            {state.error}
          </span>
        )}
      </div>
      <div className="terminal-panel__screen" ref={containerRef} data-testid="terminal-screen" />
    </div>
  );
}
