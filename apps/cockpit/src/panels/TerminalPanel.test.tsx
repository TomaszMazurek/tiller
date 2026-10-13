import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { TerminalSessionDeps, TerminalSurface } from '../lib/useTerminalSession';
import type { SessionId, TermSpawnParams } from '../lib/termApi';
import { TerminalPanel } from './TerminalPanel';

/**
 * Panel-level tests for the hardcoded tool dropdown (P6b): the operator
 * picks a tool instead of typing a program path. The session machinery
 * itself is covered by useTerminalSession.test — here we only check what
 * the toolbar does with the chosen preset, so every seam is faked (xterm
 * cannot mount in jsdom).
 */

/** Recorder stand-in for the xterm surface (xterm cannot mount in jsdom). */
function fakeSurface(): TerminalSurface {
  return {
    cols: 97,
    rows: 31,
    onData: () => {},
    fit: () => {},
    write: () => {},
    dispose: () => {},
  };
}

function createDeps() {
  const spawn = vi.fn(async (_params: TermSpawnParams): Promise<SessionId> => 7);
  const deps: TerminalSessionDeps = {
    spawn,
    kill: vi.fn(async () => {}),
    write: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    listenOutput: async () => () => {},
    listenExit: async () => () => {},
    createSurface: () => fakeSurface(),
    observeResize: () => () => {},
  };
  return { deps, spawn };
}

/** dockview hands panels its props; the terminal panel uses none of them. */
function mountPanel(deps: TerminalSessionDeps) {
  const props = { deps } as Parameters<typeof TerminalPanel>[0];
  render(<TerminalPanel {...props} />);
}

function selectTool(id: string) {
  fireEvent.change(screen.getByTestId('terminal-tool'), { target: { value: id } });
}

function fillCwd(value: string) {
  fireEvent.change(screen.getByTestId('terminal-cwd'), { target: { value } });
}

const OPTION_LABELS = ['pi', 'PowerShell', 'Command Prompt', 'Custom…'];

describe('TerminalPanel tool dropdown', () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it('renders the tool select: three presets + custom, pi default, program field hidden', () => {
    mountPanel(createDeps().deps);

    const select = screen.getByTestId('terminal-tool') as HTMLSelectElement;
    expect(select.value).toBe('pi');
    expect(Array.from(select.options).map((option) => option.textContent)).toEqual(OPTION_LABELS);
    // no preset needs the free-text program field — custom is the only way in
    expect(screen.queryByTestId('terminal-program')).toBeNull();
  });

  it('default pi preset starts the session with program pi', async () => {
    const { deps, spawn } = createDeps();
    mountPanel(deps);
    fillCwd('C:/tmp/project');

    fireEvent.click(screen.getByTestId('terminal-start'));

    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].program).toBe('pi');
    expect(spawn.mock.calls[0][0].cwd).toBe('C:/tmp/project');
  });

  it('cmd preset spawns cmd.exe and locks the toolbar while running', async () => {
    const { deps, spawn } = createDeps();
    mountPanel(deps);

    selectTool('cmd');
    expect(screen.queryByTestId('terminal-program')).toBeNull();
    fillCwd('C:/tmp/project');
    fireEvent.click(screen.getByTestId('terminal-start'));

    await waitFor(() => {
      expect(screen.getByTestId('terminal-status').textContent).toBe('running');
    });
    expect(spawn).toHaveBeenCalledTimes(1);
    expect(spawn.mock.calls[0][0].program).toBe('cmd.exe');
    expect((screen.getByTestId('terminal-tool') as HTMLSelectElement).disabled).toBe(true);
  });

  it('powershell preset spawns powershell.exe', async () => {
    const { deps, spawn } = createDeps();
    mountPanel(deps);

    selectTool('powershell');
    fillCwd('C:/tmp/project');
    fireEvent.click(screen.getByTestId('terminal-start'));

    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].program).toBe('powershell.exe');
  });

  it('custom reveals the program field: empty keeps Start disabled, typed value reaches spawn', async () => {
    const { deps, spawn } = createDeps();
    mountPanel(deps);

    selectTool('custom');
    const program = screen.getByTestId('terminal-program') as HTMLInputElement;
    fillCwd('C:/tmp/project');
    // the existing validation holds: empty program → Start disabled
    expect((screen.getByTestId('terminal-start') as HTMLButtonElement).disabled).toBe(true);

    fireEvent.change(program, { target: { value: 'node' } });
    fireEvent.click(screen.getByTestId('terminal-start'));

    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].program).toBe('node');
  });

  it('switching from custom back to a preset hides the program field and spawns the preset', async () => {
    const { deps, spawn } = createDeps();
    mountPanel(deps);

    selectTool('custom');
    fireEvent.change(screen.getByTestId('terminal-program'), { target: { value: 'node' } });
    selectTool('pi');
    expect(screen.queryByTestId('terminal-program')).toBeNull();
    fillCwd('C:/tmp/project');
    fireEvent.click(screen.getByTestId('terminal-start'));

    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].program).toBe('pi');
  });
});
