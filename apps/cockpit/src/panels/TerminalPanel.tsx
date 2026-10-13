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

/**
 * Hardcoded tool dropdown entries (P6b): the operator picks a tool
 * instead of typing a program path — no configuration, no paths. `custom`
 * is the only way to a free-text program (it reveals the input field).
 */
interface ToolPreset {
  id: string;
  label: string;
  /** Spawn target; `null` for `custom` — the free-text field takes over. */
  program: string | null;
}

const TOOL_PRESETS: readonly ToolPreset[] = [
  // `pi` keeps the bare name: the backend's sandbox heuristic (P3a)
  // matches on it; cmd.exe/powershell.exe correctly get no sandbox.
  { id: 'pi', label: 'pi', program: 'pi' },
  { id: 'powershell', label: 'PowerShell', program: 'powershell.exe' },
  { id: 'cmd', label: 'Command Prompt', program: 'cmd.exe' },
  { id: 'custom', label: 'Custom…', program: null },
];

const DEFAULT_TOOL_ID = 'pi';

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
  /** Overrides the production session seams (tests fake them: xterm cannot
   *  mount in jsdom — it needs a canvas). */
  deps?: TerminalSessionDeps;
}

export function TerminalPanel({ onActiveSessionChange, deps: depsProp }: TerminalPanelProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [toolId, setToolId] = useState(DEFAULT_TOOL_ID);
  // Free-text program — only the `custom` preset reads it. Starts empty,
  // so Start stays disabled until the operator names the program.
  const [customProgram, setCustomProgram] = useState('');
  // No machine-specific default (a wrong cwd fails the spawn on any other
  // machine): the operator names the project directory; Start stays
  // disabled until the tool (for `custom`: a typed program) and cwd are set.
  const [cwd, setCwd] = useState('');
  const preset = TOOL_PRESETS.find((entry) => entry.id === toolId) ?? TOOL_PRESETS[0];
  const program = preset.program ?? customProgram;

  const productionDeps = useMemo<TerminalSessionDeps>(
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
  const deps = depsProp ?? productionDeps;
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
        <label className="terminal-panel__field terminal-panel__field--tool">
          tool
          <select
            value={toolId}
            onChange={(event) => setToolId(event.target.value)}
            disabled={running || busy}
            data-testid="terminal-tool"
          >
            {TOOL_PRESETS.map((entry) => (
              <option key={entry.id} value={entry.id}>
                {entry.label}
              </option>
            ))}
          </select>
        </label>
        {preset.program === null && (
          <label className="terminal-panel__field terminal-panel__field--program">
            program
            <input
              value={customProgram}
              onChange={(event) => setCustomProgram(event.target.value)}
              placeholder="custom program"
              spellCheck={false}
              disabled={running || busy}
              data-testid="terminal-program"
            />
          </label>
        )}
        <label className="terminal-panel__field terminal-panel__field--cwd">
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
