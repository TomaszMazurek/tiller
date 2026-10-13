import { act, cleanup, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useEffect, useMemo, useState } from 'react';
import { TimelinePanel, type TimelineDeps } from './TimelinePanel';
import { ActiveSessionContext } from '../lib/activeSession';
import type { SessionId } from '../lib/termApi';
import type { SessionEntryEvent, TimelineEntry } from '../lib/timeline';

function message(id: string, chars = 7): TimelineEntry {
  return { kind: 'message', id, role: 'user', ts: '2026-10-11T12:00:00.000Z', chars, blocks: 1 };
}

/** A deferred `session.observe` — resolves when the test says so (P1-1). */
function deferredObserve() {
  let resolve!: (entries: TimelineEntry[]) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<TimelineEntry[]>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function fakeDeps(snapshot: () => Promise<TimelineEntry[]>) {
  const unlisten = vi.fn();
  let emit: ((event: SessionEntryEvent) => void) | null = null;
  const deps: TimelineDeps = {
    observe: vi.fn(snapshot),
    listenEntry: vi.fn(async (handler) => {
      emit = handler;
      return unlisten;
    }),
  };
  return {
    deps,
    observe: deps.observe as ReturnType<typeof vi.fn>,
    getEmit: () => emit,
    unlisten,
  };
}

// dockview hands panels its props; the timeline panel only needs the
// context-provided session id seam plus a minimal stand-in for the rest.
const panelProps = { api: {}, params: {} } as unknown as Parameters<typeof TimelinePanel>[0];

/** Mirrors App's wiring: the session id lives in the host (App) state and
 *  reaches the panel through ActiveSessionContext — not panel props. */
function SessionHost({ sessionId, deps }: { sessionId: SessionId | null; deps: TimelineDeps }) {
  const session = useMemo(() => ({ sessionId }), [sessionId]);
  return (
    <ActiveSessionContext.Provider value={session}>
      <TimelinePanel {...panelProps} deps={deps} />
    </ActiveSessionContext.Provider>
  );
}

/** Reproduces dockview's frozen factory (the P8a blocker, same pattern as
 *  TerminalPanel.test): `ReactPanelContentPart` captures the component
 *  function when the panel is created, so a mounted panel's props never
 *  update again. The element below is likewise created exactly once
 *  (useMemo) with fixed props — the session id must still arrive, because
 *  it rides the context, which flows through dockview's portals regardless
 *  of the frozen factory. */
function FrozenFactoryHost({
  deps,
  sessionId,
}: {
  deps: TimelineDeps;
  sessionId: SessionId | null;
}) {
  const [current, setCurrent] = useState<SessionId | null>(sessionId);
  // The id changes the way App's state does: the host re-renders with a
  // new value, but the panel element below stays frozen — only the
  // provider value changes.
  useEffect(() => {
    setCurrent(sessionId);
  }, [sessionId]);
  const session = useMemo(() => ({ sessionId: current }), [current]);
  const frozenPanel = useMemo(() => <TimelinePanel {...panelProps} deps={deps} />, [deps]);
  return <ActiveSessionContext.Provider value={session}>{frozenPanel}</ActiveSessionContext.Provider>;
}

describe('TimelinePanel', () => {
  // No `globals: true` in the vitest config, so RTL's auto-cleanup is
  // off — panels from earlier tests would otherwise leak into screen.
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it('fails fast without the provider (a missing provider is a dead timeline)', () => {
    // The P8a-2 bug class this channel exists to prevent would otherwise
    // stay silent: a timeline reading a default id forever renders "no
    // active session" while a live session runs. Silence it in the log —
    // the throw itself is the assertion.
    vi.spyOn(console, 'error').mockImplementation(() => {});
    expect(() =>
      render(<TimelinePanel {...panelProps} deps={fakeDeps(async () => []).deps} />),
    ).toThrow(/ActiveSessionContext\.Provider/);
  });

  it('shows the empty state without an active session', () => {
    render(<SessionHost sessionId={null} deps={fakeDeps(async () => []).deps} />);
    expect(screen.getByTestId('timeline-session').textContent).toBe('no active session');
  });

  it('renders role + chars for messages, name + args for calls, ok/error + tool + duration for results', async () => {
    const entries: TimelineEntry[] = [
      { kind: 'message', id: 'm1', role: 'user', ts: '2026-10-11T12:00:00.000Z', chars: 42, blocks: 1 },
      {
        kind: 'tool_call',
        id: 'c1',
        name: 'bash',
        ts: '2026-10-11T12:00:01.000Z',
        tool_call_id: 'c1',
        args_chars: 128,
      },
      {
        kind: 'tool_result',
        id: 'r1',
        ts: '2026-10-11T12:00:02.000Z',
        tool_call_id: 'c1|fc_1',
        tool_name: 'bash',
        ok: true,
        duration_ms: 92,
      },
      {
        kind: 'tool_result',
        id: 'r2',
        ts: '2026-10-11T12:00:03.000Z',
        tool_call_id: 'c2|fc_2',
        tool_name: 'read',
        ok: false,
        duration_ms: 5,
      },
    ];
    render(<SessionHost sessionId={7} deps={fakeDeps(async () => entries).deps} />);
    // Stats only — no content ever renders.
    expect(await screen.findByText('42 chars')).toBeDefined();
    expect(screen.getByText('user')).toBeDefined();
    expect(screen.getByText('bash')).toBeDefined();
    expect(screen.getByText('args 128 chars')).toBeDefined();
    expect(screen.getByText('ok')).toBeDefined();
    expect(screen.getByText('bash · 92 ms')).toBeDefined();
    expect(screen.getByText('error')).toBeDefined();
    expect(screen.getByText('read · 5 ms')).toBeDefined();
  });

  it('keeps live entries that raced the snapshot read (P1-1)', async () => {
    const observe = deferredObserve();
    const { deps, getEmit } = fakeDeps(() => observe.promise);
    render(<SessionHost sessionId={7} deps={deps} />);

    // The rpc is in flight; a live entry arrives for our session — it
    // must buffer, not vanish under the snapshot's replace.
    const emit = getEmit();
    expect(emit).not.toBeNull();
    await act(async () => {
      emit!({ session_id: 7, entry: message('live', 11) });
    });
    // Not shown yet (the snapshot is pending)…
    expect(screen.queryByText('11 chars')).toBeNull();

    // The snapshot (read before the live entry was written) lands.
    await act(async () => {
      observe.resolve([message('snap', 21)]);
    });
    expect(await screen.findByText('21 chars')).toBeDefined();
    expect(screen.getByText('11 chars')).toBeDefined();
    expect(screen.queryAllByTestId('timeline-entry')).toHaveLength(2);
  });

  it('shows events that predate the session id when the session exits fast (NOTE)', async () => {
    const observe = deferredObserve();
    const { deps, getEmit } = fakeDeps(() => observe.promise);
    // No session id yet: the gate buffers instead of dropping.
    const view = render(<SessionHost sessionId={null} deps={deps} />);
    const emit = getEmit();
    expect(emit).not.toBeNull();
    await act(async () => {
      emit!({ session_id: 4, entry: message('early', 33) });
    });
    expect(screen.queryByText('33 chars')).toBeNull();

    // The id arrives — but the session already exited, so observe
    // rejects with SESSION_NOT_FOUND. The buffered entries stay.
    view.rerender(<SessionHost sessionId={4} deps={deps} />);
    await act(async () => {
      observe.reject(new Error('session not found'));
    });
    expect(await screen.findByText('33 chars')).toBeDefined();
  });

  it('renders the snapshot, then live entries — foreign sessions dropped, duplicates deduped', async () => {
    const { deps, getEmit } = fakeDeps(async () => [message('1', 15)]);
    render(<SessionHost sessionId={7} deps={deps} />);

    // Snapshot landed.
    expect(await screen.findByText('15 chars')).toBeDefined();
    expect(screen.getByTestId('timeline-session').textContent).toBe('session 7');

    const emit = getEmit();
    expect(emit).not.toBeNull();

    // A foreign session's entry is dropped.
    await act(async () => {
      emit!({ session_id: 99, entry: message('99', 5) });
    });
    expect(screen.queryByText('5 chars')).toBeNull();

    // Our session's entry is appended…
    await act(async () => {
      emit!({ session_id: 7, entry: message('2', 9) });
    });
    expect(screen.getByText('9 chars')).toBeDefined();

    // …and a live duplicate of a snapshot id is not rendered twice.
    await act(async () => {
      emit!({ session_id: 7, entry: message('1', 15) });
    });
    expect(screen.getAllByText('15 chars')).toHaveLength(1);
  });

  it('a session id change reaches a panel whose factory was frozen before the id existed (dockview repro)', async () => {
    // The P8a-2 blocker in miniature: the panel is mounted through a
    // factory that never re-renders it with new props (FrozenFactoryHost
    // memoizes the element), exactly like dockview's ReactPanelContentPart.
    // The old prop channel starved such a panel at `activeSessionId=null`
    // forever; the context channel still delivers.
    const { deps, observe } = fakeDeps(async () => []);
    const view = render(<FrozenFactoryHost deps={deps} sessionId={null} />);
    expect(observe).not.toHaveBeenCalled();

    view.rerender(<FrozenFactoryHost deps={deps} sessionId={9} />);

    await waitFor(() => {
      expect(observe).toHaveBeenCalledWith(9);
    });
    expect(await screen.findByText('session 9')).toBeDefined();
  });

  it('clears the view when the active session goes away (exit/kill → id null)', async () => {
    // Existing semantics, unchanged: a null id resets the view and stays
    // armed for the next session (see the gate comment in the panel).
    const { deps } = fakeDeps(async () => [message('1', 15)]);
    const view = render(<SessionHost sessionId={7} deps={deps} />);
    expect(await screen.findByText('15 chars')).toBeDefined();

    view.rerender(<SessionHost sessionId={null} deps={deps} />);

    expect(await screen.findByText('no active session')).toBeDefined();
    expect(screen.queryByText('15 chars')).toBeNull();
  });

  it('unsubscribes on unmount', async () => {
    const { deps, unlisten } = fakeDeps(async () => []);
    const view = render(<SessionHost sessionId={1} deps={deps} />);
    await screen.findByText('session 1');
    view.unmount();
    expect(unlisten).toHaveBeenCalled();
  });

  it('keeps the view when observe fails (session exited under us)', async () => {
    const deps: TimelineDeps = {
      observe: vi.fn(async () => {
        throw new Error('session not found');
      }),
      listenEntry: vi.fn(async () => () => undefined),
    };
    render(<SessionHost sessionId={3} deps={deps} />);
    expect(await screen.findByTestId('timeline-list')).toBeDefined();
    expect(screen.queryAllByTestId('timeline-entry')).toHaveLength(0);
  });
});
