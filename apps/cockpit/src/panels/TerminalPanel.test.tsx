import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useCallback, useEffect, useMemo, useState } from 'react';
import type { TerminalSessionDeps, TerminalSurface } from '../lib/useTerminalSession';
import type { SessionId, TermExitEvent, TermSpawnParams } from '../lib/termApi';
import {
  TerminalRequestContext,
  type TerminalRequestChannel,
  type TerminalStartRequest,
} from '../lib/terminalRequest';
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
  const kill = vi.fn(async () => {});
  const deps: TerminalSessionDeps = {
    spawn,
    kill,
    write: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    listenOutput: async () => () => {},
    listenExit: async () => () => {},
    createSurface: () => fakeSurface(),
    observeResize: () => () => {},
  };
  return { deps, spawn, kill };
}

/** Deps whose `listenExit` captures the handler, so tests can end a
 * session the way the backend does — with a `term:exit` event. */
function createDepsWithExitCapture() {
  const { deps, spawn, kill } = createDeps();
  let exitHandler: ((event: TermExitEvent) => void) | null = null;
  const withExit: TerminalSessionDeps = {
    ...deps,
    listenExit: async (handler) => {
      exitHandler = handler;
      return () => {};
    },
  };
  return {
    deps: withExit,
    spawn,
    kill,
    emitExit: (event: TermExitEvent) => exitHandler!(event),
  };
}

/** Mirrors App's wiring (P8a): the request lives in the provider host, the
 * panel consumes it over the context, the host clears it on consume —
 * exactly one handling per request. */
function RequestHost({
  deps,
  request,
  mounted = true,
}: {
  deps: TerminalSessionDeps;
  request: TerminalStartRequest | null;
  /** Lets tests close/reopen the panel the way dockview does. */
  mounted?: boolean;
}) {
  const [current, setCurrent] = useState<TerminalStartRequest | null>(request);
  useEffect(() => {
    if (request !== null) {
      setCurrent(request);
    }
  }, [request]);
  const consume = useCallback(() => setCurrent(null), []);
  const channel = useMemo(() => ({ request: current, consume }), [current, consume]);
  return (
    <TerminalRequestContext.Provider value={channel}>
      {mounted && <TerminalPanel {...({ deps } as Parameters<typeof TerminalPanel>[0])} />}
    </TerminalRequestContext.Provider>
  );
}

/** Reproduces dockview's frozen factory (the P8a blocker root cause):
 * `ReactPanelContentPart` captures the component function when the panel
 * is created, so a mounted panel's props never update again. The element
 * below is likewise created exactly once (useMemo) with fixed props — the
 * request must still arrive, because it rides the context, which flows
 * through dockview's portals regardless of the frozen factory. */
function FrozenFactoryHost({
  deps,
  request,
}: {
  deps: TerminalSessionDeps;
  request: TerminalStartRequest | null;
}) {
  const [current, setCurrent] = useState<TerminalStartRequest | null>(request);
  useEffect(() => {
    if (request !== null) {
      setCurrent(request);
    }
  }, [request]);
  const consume = useCallback(() => setCurrent(null), []);
  const channel = useMemo(() => ({ request: current, consume }), [current, consume]);
  const frozenPanel = useMemo(
    () => <TerminalPanel {...({ deps } as Parameters<typeof TerminalPanel>[0])} />,
    [deps],
  );
  return (
    <TerminalRequestContext.Provider value={channel}>{frozenPanel}</TerminalRequestContext.Provider>
  );
}

/** Host that issues two requests in one tick (one event handler = one
 * batched commit, like two `requestTerminal` calls in a single Review
 * action): the pending slot keeps only the latest request. */
function DoubleRequestHost({
  deps,
  first,
  second,
}: {
  deps: TerminalSessionDeps;
  first: TerminalStartRequest;
  second: TerminalStartRequest;
}) {
  const [current, setCurrent] = useState<TerminalStartRequest | null>(null);
  const consume = useCallback(() => setCurrent(null), []);
  const channel = useMemo(() => ({ request: current, consume }), [current, consume]);
  return (
    <TerminalRequestContext.Provider value={channel}>
      <button
        type="button"
        data-testid="issue-two"
        onClick={() => {
          setCurrent(first);
          setCurrent(second);
        }}
      />
      <TerminalPanel {...({ deps } as Parameters<typeof TerminalPanel>[0])} />
    </TerminalRequestContext.Provider>
  );
}

/** dockview hands panels its props; the terminal panel uses none of them. */
const NO_REQUEST: TerminalRequestChannel = { request: null, consume: () => {} };

function mountPanel(deps: TerminalSessionDeps) {
  const props = { deps } as Parameters<typeof TerminalPanel>[0];
  render(
    <TerminalRequestContext.Provider value={NO_REQUEST}>
      <TerminalPanel {...props} />
    </TerminalRequestContext.Provider>,
  );
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

  it('cwd comes prefilled with the Tiller project default (operator request)', async () => {
    const { deps, spawn } = createDeps();
    mountPanel(deps);

    // No fillCwd: the prefill alone must satisfy the validation and spawn
    // pi in the default project directory.
    expect((screen.getByTestId('terminal-cwd') as HTMLInputElement).value).toBe(
      'C:/Repos/tiller'
    );
    fireEvent.click(screen.getByTestId('terminal-start'));

    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].cwd).toBe('C:/Repos/tiller');
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

// ---------------------------------------------- start requests (P8a)

describe('TerminalPanel start requests', () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it('a request on an idle terminal auto-starts it with the requested program and cwd (once)', async () => {
    const { deps, spawn } = createDeps();
    render(
      <RequestHost
        deps={deps}
        request={{ program: 'pi', cwd: 'C:/repo/.tiller/worktrees/fix-1' }}
      />,
    );

    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].program).toBe('pi');
    expect(spawn.mock.calls[0][0].cwd).toBe('C:/repo/.tiller/worktrees/fix-1');
    // the toolbar reflects the request
    expect((screen.getByTestId('terminal-tool') as HTMLSelectElement).value).toBe('pi');
    expect((screen.getByTestId('terminal-cwd') as HTMLInputElement).value).toBe(
      'C:/repo/.tiller/worktrees/fix-1'
    );

    // consumed once, never replayed: no second spawn after the consume
    // settles and the panel re-renders
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(spawn).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId('terminal-status').textContent).toBe('running');
  });

  it('a request while running only prefills the fields and hints — no kill, no start', async () => {
    const { deps, spawn, kill, emitExit } = createDepsWithExitCapture();
    const view = render(<RequestHost deps={deps} request={null} />);

    // a live session on another tool and directory
    selectTool('cmd');
    fillCwd('C:/tmp/elsewhere');
    fireEvent.click(screen.getByTestId('terminal-start'));
    await waitFor(() => {
      expect(screen.getByTestId('terminal-status').textContent).toBe('running');
    });
    expect(spawn).toHaveBeenCalledTimes(1);

    view.rerender(
      <RequestHost deps={deps} request={{ program: 'pi', cwd: 'C:/repo/.tiller/worktrees/fix-1' }} />
    );

    // fields follow the request, tool back on pi — but the session is
    // untouched: exactly the one spawn from before, no kill either
    await waitFor(() => {
      expect((screen.getByTestId('terminal-cwd') as HTMLInputElement).value).toBe(
        'C:/repo/.tiller/worktrees/fix-1'
      );
    });
    expect((screen.getByTestId('terminal-tool') as HTMLSelectElement).value).toBe('pi');
    expect(spawn).toHaveBeenCalledTimes(1);
    expect(kill).not.toHaveBeenCalled();
    expect(screen.getByTestId('terminal-hint').textContent).toBe(
      'Terminal busy — Kill first, then Start'
    );

    // the hint retires once nothing is running (exit event lands)
    emitExit({
      session_id: 7,
      exit_code: 0,
      signal: null,
      success: true,
      dsr_responses_sent: 0,
    });
    await waitFor(() => {
      expect(screen.queryByTestId('terminal-hint')).toBeNull();
    });
  });

  it('a request after the session exited restarts with the requested cwd', async () => {
    const { deps, spawn, emitExit } = createDepsWithExitCapture();
    const view = render(<RequestHost deps={deps} request={null} />);

    fillCwd('C:/tmp/first');
    fireEvent.click(screen.getByTestId('terminal-start'));
    await waitFor(() => {
      expect(screen.getByTestId('terminal-status').textContent).toBe('running');
    });
    emitExit({
      session_id: 7,
      exit_code: 0,
      signal: null,
      success: true,
      dsr_responses_sent: 0,
    });
    await waitFor(() => {
      expect(screen.getByTestId('terminal-status').textContent).toContain('exited');
    });

    view.rerender(<RequestHost deps={deps} request={{ program: 'pi', cwd: 'C:/w/task-9' }} />);
    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(2);
    });
    expect(spawn.mock.calls[1][0].program).toBe('pi');
    expect(spawn.mock.calls[1][0].cwd).toBe('C:/w/task-9');
    // restart-after-exit is a clean second session, not a busy case
    expect(screen.queryByTestId('terminal-hint')).toBeNull();
  });

  it('a request reaches a panel whose factory was frozen before the request existed (dockview repro)', async () => {
    // The P8a blocker in miniature: the panel is mounted through a
    // factory that never re-renders it with new props (FrozenFactoryHost
    // memoizes the element), exactly like dockview's ReactPanelContentPart.
    // The old prop channel starved such a panel at `startRequest=null`;
    // the context channel still delivers.
    const { deps, spawn } = createDeps();
    const view = render(<FrozenFactoryHost deps={deps} request={null} />);
    expect(spawn).not.toHaveBeenCalled();

    view.rerender(
      <FrozenFactoryHost deps={deps} request={{ program: 'pi', cwd: 'C:/repo/.tiller/worktrees/fix-1' }} />
    );

    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].program).toBe('pi');
    expect(spawn.mock.calls[0][0].cwd).toBe('C:/repo/.tiller/worktrees/fix-1');
  });

  it('two requests issued in one tick: only the last one runs (no queue)', async () => {
    const { deps, spawn } = createDeps();
    render(
      <DoubleRequestHost
        deps={deps}
        first={{ program: 'pi', cwd: 'C:/w/fix-1' }}
        second={{ program: 'pi', cwd: 'C:/w/fix-2' }}
      />,
    );

    fireEvent.click(screen.getByTestId('issue-two'));

    // one batched commit carried only the second request — the first was
    // overwritten before any panel saw it
    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].cwd).toBe('C:/w/fix-2');
  });

  it('a request issued while the panel is closed is handled by the next mounted terminal (no act warnings)', async () => {
    const { deps, spawn } = createDeps();
    const pending: TerminalStartRequest = { program: 'pi', cwd: 'C:/w/fix-1' };
    const consoleErrors: string[] = [];
    vi.spyOn(console, 'error').mockImplementation((...args: unknown[]) => {
      consoleErrors.push(args.map(String).join(' '));
    });

    const view = render(<RequestHost deps={deps} request={null} />);
    // the request lands while no terminal is mounted — it stays pending
    // (nothing consumed it; no setState on an unmounted panel happened)
    view.rerender(<RequestHost deps={deps} request={pending} mounted={false} />);
    await Promise.resolve();
    expect(spawn).not.toHaveBeenCalled();

    // dockview reopens the terminal panel: the still-pending request
    // auto-starts there — the handoff survives the close/reopen cycle
    view.rerender(<RequestHost deps={deps} request={pending} />);
    await waitFor(() => {
      expect(spawn).toHaveBeenCalledTimes(1);
    });
    expect(spawn.mock.calls[0][0].cwd).toBe('C:/w/fix-1');

    // closing a panel (again) with everything settled raises no act /
    // update-on-unmounted warnings
    view.rerender(<RequestHost deps={deps} request={pending} mounted={false} />);
    await Promise.resolve();
    expect(
      consoleErrors.filter((line) => line.includes('act(') || line.includes('not wrapped in act')),
    ).toEqual([]);
  });
});
