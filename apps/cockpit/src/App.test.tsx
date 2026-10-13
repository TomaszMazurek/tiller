import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { App } from './App';

/**
 * App-level regression for the P8a blocker: dockview-react captures the
 * panel component function once per panel (`ReactPanelContentPart`), so
 * props threaded through App's `components` map never update a panel that
 * is already mounted — the terminal mounted at startup kept a stale
 * `startRequest=null` no matter what Review clicked. The request now
 * travels through `TerminalRequestContext`, and the active session id
 * through `ActiveSessionContext` (P8a-2), both of which flow into
 * dockview's portals (they render inside <DockviewReact>'s React subtree).
 *
 * This is the full path with a real <DockviewReact> layout and the real
 * panels; only the outer seams are faked — the rpc bridge (jsdom has no
 * tauri), the session hook (jsdom has no canvas for xterm), and the
 * session rpc surface (the assertion seam is "the timeline observed id
 * N").
 */

const recorder = vi.hoisted(() => ({
  starts: [] as Array<{ program: string; cwd: string }>,
}));

const timeline = vi.hoisted(() => ({
  observed: [] as number[],
  subscribed: 0,
}));

vi.mock('./lib/useTerminalSession', async () => {
  const { useCallback, useState } = await import('react');
  const { initialTerminalSessionState } = await import('./lib/sessionState');
  return {
    // Minimal but *stateful*: `start` reports a session id upward the way
    // the real hook does (spawned → sessionId 7), so the lifted-id path
    // (TerminalPanel → App → timeline) is exercised for real.
    useTerminalSession: () => {
      const [sessionId, setSessionId] = useState<number | null>(null);
      const start = useCallback(async (program: string, cwd: string) => {
        recorder.starts.push({ program, cwd });
        setSessionId(7);
      }, []);
      return {
        state: { ...initialTerminalSessionState, sessionId },
        start,
        kill: () => {},
      };
    },
  };
});

vi.mock('./lib/sessionApi', () => ({
  sessionObserve: async (sessionId: number) => {
    timeline.observed.push(sessionId);
    return [];
  },
  subscribeSessionEntry: async () => {
    timeline.subscribed += 1;
    return () => {};
  },
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: async () => () => {},
}));

/** Fake rpc core: answers the wire protocol the real rpcClient speaks. */
vi.mock('@tauri-apps/api/core', () => ({
  invoke: async (_cmd: string, args?: { json?: string }) => {
    const request =
      args?.json !== undefined
        ? (JSON.parse(args.json) as { id: number | string; method: string })
        : { id: 0, method: '' };
    const reply = (result: unknown) => JSON.stringify({ id: request.id, result });
    switch (request.method) {
      case 'core.version':
        return reply({ version: 'test-core' });
      case 'task.list':
        return reply([
          {
            task_id: 'fix-1',
            target_branch: 'master',
            worktree_path: 'C:/repo/.tiller/worktrees/fix-1',
            base: 'aaaaaaa0',
            tip: '12345670',
          },
          {
            task_id: 'fix-2',
            target_branch: 'master',
            worktree_path: 'C:/repo/.tiller/worktrees/fix-2',
            base: 'aaaaaaa0',
            tip: '12345671',
          },
        ]);
      case 'task.status':
        return reply({
          status: 'candidate',
          candidate: {
            task_id: 'fix-1',
            candidate_commit: '1234567890abcdef',
            base_commit: 'abcdef1234567890',
            created_at: 0,
          },
          approval_state: null,
          checkpoints_count: 2,
        });
      case 'task.gates':
        // "no gate record yet" — the reducer accepts the empty answer.
        return reply({});
      case 'task.diff':
        return reply({
          files: [
            {
              path: 'a.txt',
              change_type: 'modified',
              patch: '@@ -1 +1 @@\n-old\n+new\n ctx',
              binary: false,
              truncated: false,
            },
          ],
          truncated: false,
          total_bytes: 30,
          byte_cap: 262144,
        });
      default:
        return reply(null);
    }
  },
}));

describe('App task→terminal handoff (P8a)', () => {
  beforeAll(() => {
    // dockview observes its containers; jsdom ships no ResizeObserver.
    class ResizeObserverStub {
      observe(): void {}
      unobserve(): void {}
      disconnect(): void {}
    }
    globalThis.ResizeObserver = ResizeObserverStub as unknown as typeof ResizeObserver;
  });

  afterEach(() => {
    cleanup();
    recorder.starts.length = 0;
    timeline.observed.length = 0;
    timeline.subscribed = 0;
  });

  it('a ">_" click in Review starts the session in the terminal mounted at startup', async () => {
    render(<App />);

    // The terminal panel has been mounted since App startup — the request
    // must reach THAT panel, not a newly instantiated one.
    expect(screen.getByTestId('terminal-tool')).toBeDefined();
    expect(recorder.starts).toEqual([]);

    fireEvent.change(screen.getByTestId('review-repo'), { target: { value: 'C:/repo' } });
    fireEvent.click(screen.getByTestId('review-load'));
    fireEvent.click((await screen.findAllByTestId('review-open-terminal'))[0]);

    await waitFor(() => {
      expect(recorder.starts).toEqual([
        { program: 'pi', cwd: 'C:/repo/.tiller/worktrees/fix-1' },
      ]);
    });
  });

  it('a second ">_" click after the first was handled reaches the same terminal', async () => {
    render(<App />);
    expect(screen.getByTestId('terminal-tool')).toBeDefined();

    fireEvent.change(screen.getByTestId('review-repo'), { target: { value: 'C:/repo' } });
    fireEvent.click(screen.getByTestId('review-load'));
    const openButtons = await screen.findAllByTestId('review-open-terminal');

    // each click is delivered and consumed — the channel serves repeat
    // handoffs, not just the first one
    fireEvent.click(openButtons[0]);
    await waitFor(() => {
      expect(recorder.starts).toEqual([
        { program: 'pi', cwd: 'C:/repo/.tiller/worktrees/fix-1' },
      ]);
    });

    fireEvent.click(openButtons[1]);
    await waitFor(() => {
      expect(recorder.starts).toEqual([
        { program: 'pi', cwd: 'C:/repo/.tiller/worktrees/fix-1' },
        { program: 'pi', cwd: 'C:/repo/.tiller/worktrees/fix-2' },
      ]);
    });
    // the terminal toolbar follows the latest request
    expect((screen.getByTestId('terminal-cwd') as HTMLInputElement).value).toBe(
      'C:/repo/.tiller/worktrees/fix-2'
    );
  });

  it('a session started in the terminal is observed by the timeline mounted at startup (P8a-2)', async () => {
    render(<App />);

    // The timeline panel has been mounted since App startup with the id
    // still null — the session id must reach THAT panel, not a newly
    // instantiated one (the old prop channel starved it at null forever).
    expect(screen.getByTestId('timeline-session').textContent).toBe('no active session');
    expect(timeline.observed).toEqual([]);

    // pi in the prefilled default cwd — Start is enabled out of the box.
    fireEvent.click(screen.getByTestId('terminal-start'));

    await waitFor(() => {
      expect(screen.getByTestId('timeline-session').textContent).toBe('session 7');
    });
    // exactly one observe, with the id of the session that just started
    expect(timeline.observed).toEqual([7]);
    expect(timeline.subscribed).toBeGreaterThan(0);
  });
});
