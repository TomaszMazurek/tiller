import { useEffect, useReducer, useRef, useState } from 'react';
import type { IDockviewPanelProps } from 'dockview-react';
import { sessionObserve, subscribeSessionEntry } from '../lib/sessionApi';
import { SessionEventGate } from '../lib/sessionEventGate';
import type { SessionId } from '../lib/termApi';
import {
  initialTimelineState,
  relativeTime,
  timelineReducer,
  type SessionEntryEvent,
  type TimelineEntry,
} from '../lib/timeline';

/** Injected seams (tests fake the tauri bridge). */
export interface TimelineDeps {
  observe(sessionId: SessionId): Promise<TimelineEntry[]>;
  listenEntry(handler: (event: SessionEntryEvent) => void): Promise<() => void>;
}

const defaultDeps: TimelineDeps = {
  observe: sessionObserve,
  listenEntry: subscribeSessionEntry,
};

/** How often relative ages ("42 s") refresh. */
const TIME_TICK_MS = 15_000;

export interface TimelinePanelProps extends IDockviewPanelProps {
  /** The terminal session whose session file is observed. */
  activeSessionId: SessionId | null;
  deps?: TimelineDeps;
}

/**
 * Timeline panel (brief §5 rule 2): a derived status/event view of what
 * the hosted agent is doing, observed from the session JSONL pi writes.
 * Thin by design — the state machine lives in `timeline.ts` (reducer) and
 * the backend (`session.observe` snapshot + `session.entry` stream).
 *
 * Two races are closed structurally:
 *
 * - Live events can arrive before `activeSessionId` is known (a short
 *   session may exit before the spawn rpc resolves) — they route
 *   through a [`SessionEventGate`] that buffers until the id is adopted.
 * - The snapshot rpc races event delivery; entries arriving while it is
 *   in flight are buffered by the reducer and merged after (see
 *   `timeline.ts`).
 */
export function TimelinePanel({ activeSessionId, deps = defaultDeps }: TimelinePanelProps) {
  const [state, dispatch] = useReducer(timelineReducer, initialTimelineState);
  const gateRef = useRef<SessionEventGate<SessionEntryEvent> | null>(null);

  // The live subscription outlives session switches: it is armed once
  // (on mount) and the gate matches events against the adopted id.
  useEffect(() => {
    const gate = new SessionEventGate<SessionEntryEvent>((event) => {
      dispatch({ type: 'append', entry: event.entry });
    });
    gate.arm();
    gateRef.current = gate;
    let disposed = false;
    let unlisten: (() => void) | null = null;
    deps
      .listenEntry((event) => gateRef.current?.onEvent(event))
      .then((fn) => {
        if (disposed) {
          fn();
        } else {
          unlisten = fn;
        }
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
      gateRef.current = null;
    };
  }, [deps]);

  useEffect(() => {
    const gate = gateRef.current;
    if (activeSessionId === null) {
      // No session: clear the view but stay armed — early events of
      // the *next* session must still buffer until its id is adopted.
      // (release+arm, not a bare release: release disarms and would
      // drop everything emitted while no id is known.)
      gate?.release();
      gate?.arm();
      dispatch({ type: 'reset' });
      return;
    }
    // Adopting replays the events buffered before the id was known —
    // after the reset, so they land in a clean view, in order.
    dispatch({ type: 'reset' });
    gate?.adopt(activeSessionId);
    let cancelled = false;
    dispatch({ type: 'observeStart' });
    deps
      .observe(activeSessionId)
      .then((entries) => {
        if (!cancelled) {
          dispatch({ type: 'snapshot', entries });
        }
      })
      // A session that exited under us (observe → SESSION_NOT_FOUND):
      // keep the collected entries visible — the terminal already shows
      // the exit status.
      .catch(() => {
        if (!cancelled) {
          dispatch({ type: 'observeFailed' });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [activeSessionId, deps]);

  // Relative ages tick: entries render "just now" → "42 s" → "5 min".
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), TIME_TICK_MS);
    return () => clearInterval(timer);
  }, []);

  // Auto-scroll while pinned to the bottom; scrolling up detaches.
  const listRef = useRef<HTMLUListElement | null>(null);
  const pinnedRef = useRef(true);
  useEffect(() => {
    const list = listRef.current;
    if (list !== null && pinnedRef.current) {
      list.scrollTop = list.scrollHeight;
    }
  }, [state.entries]);

  return (
    <div className="timeline-panel">
      <div className="timeline-panel__head">
        <span data-testid="timeline-session">
          {activeSessionId === null ? 'no active session' : `session ${activeSessionId}`}
        </span>
      </div>
      <ul
        className="timeline-panel__list"
        ref={listRef}
        data-testid="timeline-list"
        onScroll={(event) => {
          const el = event.currentTarget;
          pinnedRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
      >
        {state.droppedOlder > 0 && (
          <li className="timeline-panel__note" data-testid="timeline-note">
            … {state.droppedOlder} older
          </li>
        )}
        {state.entries.map((entry) => (
          <TimelineRow key={entry.id} entry={entry} now={now} />
        ))}
      </ul>
    </div>
  );
}

/** One kind marker per entry — stats rows, never content (§5 rule 2). */
function entryIcon(entry: TimelineEntry): string {
  switch (entry.kind) {
    case 'message':
      return entry.role === 'user' ? '❯' : '✦';
    case 'tool_call':
      return '⚙';
    case 'tool_result':
      return entry.ok ? '✓' : '✕';
  }
}

function TimelineRow({ entry, now }: { entry: TimelineEntry; now: number }) {
  const age = relativeTime(entry.ts, now);
  const badge =
    entry.kind === 'message'
      ? entry.role
      : entry.kind === 'tool_call'
        ? entry.name
        : entry.ok
          ? 'ok'
          : 'error';
  const text =
    entry.kind === 'message'
      ? `${entry.chars} chars${entry.blocks > 1 ? ` · ${entry.blocks} blocks` : ''}`
      : entry.kind === 'tool_call'
        ? `args ${entry.args_chars} chars`
        : `${entry.tool_name} · ${entry.duration_ms} ms`;
  const okClass = entry.kind === 'tool_result' ? ` timeline-panel__entry--${entry.ok ? 'ok' : 'error'}` : '';
  return (
    <li
      className={`timeline-panel__entry timeline-panel__entry--${entry.kind}${okClass}`}
      data-testid="timeline-entry"
    >
      <span className="timeline-panel__icon" aria-hidden>
        {entryIcon(entry)}
      </span>
      <span className={`timeline-panel__badge timeline-panel__badge--${entry.kind}`}>{badge}</span>
      <span className="timeline-panel__text" data-testid="timeline-entry-text">
        {text}
      </span>
      {age !== '' && <span className="timeline-panel__age">{age}</span>}
    </li>
  );
}
