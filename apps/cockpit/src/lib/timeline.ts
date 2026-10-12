/**
 * Timeline state as a pure reducer — the panel component stays a thin
 * wrapper around this (brief §5 rule 2: the timeline is a derived
 * observation view, not a chat transcript).
 *
 * The lifecycle across the snapshot/live seam:
 *
 * - `observeStart` marks the `session.observe` rpc in flight: live
 *   entries arriving while it runs are *buffered*, not shown. The rpc
 *   reads the session file, and an entry written after that read would
 *   be lost to the snapshot's `replace` — the merge below closes that
 *   race (P1-1).
 * - `snapshot` merges the answer with the buffered entries and whatever
 *   was already shown, deduped by `id` (a live event can duplicate a
 *   line the snapshot already contains), ordered by `ts` then `id`.
 * - `observeFailed` keeps everything visible: the session may have
 *   exited before observe answered (SESSION_NOT_FOUND) — the terminal
 *   already shows the exit status, the collected entries stay.
 * - `append` adds one live `session.entry` (deduped by `id` — unique
 *   per session: pi's JSONL entry id, or the tool-call id).
 * - The list is capped ([`TIMELINE_CAP`]); entries falling off the head
 *   are counted in `droppedOlder` and surfaced as "… N older".
 */

import type { SessionId } from './termApi';

/**
 * One `session.entry` payload entry (mirrors the Rust `TimelineEntry`).
 * Stats only — the wire never carries message text, tool arguments or
 * result content (privacy by structure, brief §5 rule 2).
 */
export type TimelineEntry =
  | {
      kind: 'message';
      id: string;
      role: 'user' | 'assistant';
      ts: string;
      /** Total content length in chars (all text blocks summed). */
      chars: number;
      /** Number of content blocks (text, toolCall, image, …). */
      blocks: number;
    }
  | {
      kind: 'tool_call';
      id: string;
      name: string;
      ts: string;
      /** Correlation key: echoed by the matching `tool_result`. */
      tool_call_id: string;
      /** Length of the arguments' compact JSON, in chars. */
      args_chars: number;
    }
  | {
      kind: 'tool_result';
      id: string;
      ts: string;
      tool_call_id: string;
      tool_name: string;
      ok: boolean;
      duration_ms: number;
    };

/** `session.entry` notification payload. */
export interface SessionEntryEvent {
  session_id: SessionId;
  entry: TimelineEntry;
}

export const TIMELINE_CAP = 1000;

export interface TimelineState {
  entries: TimelineEntry[];
  /** Entries dropped from the head by the cap (0 = nothing dropped). */
  droppedOlder: number;
  /** Live entries buffered while the snapshot request is in flight. */
  pending: TimelineEntry[];
  /** A `session.observe` request is in flight (appends buffer). */
  observing: boolean;
}

export type TimelineAction =
  /** No session (yet): clear everything. */
  | { type: 'reset' }
  /** `session.observe` was sent: buffer live entries until it answers. */
  | { type: 'observeStart' }
  /** `session.observe` answered: merge the snapshot with the buffer. */
  | { type: 'snapshot'; entries: TimelineEntry[] }
  /** `session.observe` rejected (session gone): keep what we have. */
  | { type: 'observeFailed' }
  /** One live `session.entry` arrived. */
  | { type: 'append'; entry: TimelineEntry };

export const initialTimelineState: TimelineState = {
  entries: [],
  droppedOlder: 0,
  pending: [],
  observing: false,
};

export function timelineReducer(state: TimelineState, action: TimelineAction): TimelineState {
  switch (action.type) {
    case 'reset':
      return initialTimelineState;
    case 'observeStart':
      return { ...state, observing: true, pending: [] };
    case 'append': {
      const target = state.observing ? 'pending' : 'entries';
      const appended = dedupeAppend(state[target], action.entry);
      return state.observing
        ? { ...state, pending: appended }
        : { ...state, ...cap(appended, state.droppedOlder) };
    }
    case 'snapshot': {
      // Merge (never replace): the snapshot, entries already shown
      // (gated early events), and the live buffer — dedup by id.
      const merged = cap(
        mergeByTsId(state.entries, action.entries, state.pending),
        state.droppedOlder,
      );
      return { entries: merged.entries, droppedOlder: merged.droppedOlder, pending: [], observing: false };
    }
    case 'observeFailed': {
      const merged = cap(mergeByTsId(state.entries, state.pending), state.droppedOlder);
      return { entries: merged.entries, droppedOlder: merged.droppedOlder, pending: [], observing: false };
    }
  }
}

/** Append unless an entry with the same id is already there (empty ids never dedupe). */
function dedupeAppend(entries: TimelineEntry[], entry: TimelineEntry): TimelineEntry[] {
  if (entry.id !== '' && entries.some((e) => e.id === entry.id)) {
    return entries;
  }
  return [...entries, entry];
}

/**
 * Concatenate the lists, drop id duplicates (first occurrence wins), and
 * order the result by `ts` (ISO-8601 sorts lexicographically) with `id`
 * breaking ties. The sort is stable, so equal keys keep arrival order.
 */
function mergeByTsId(...lists: TimelineEntry[][]): TimelineEntry[] {
  const merged: TimelineEntry[] = [];
  const seen = new Set<string>();
  for (const list of lists) {
    for (const entry of list) {
      if (entry.id !== '') {
        if (seen.has(entry.id)) {
          continue;
        }
        seen.add(entry.id);
      }
      merged.push(entry);
    }
  }
  return merged.sort((a, b) => {
    if (a.ts !== b.ts) {
      return a.ts < b.ts ? -1 : 1;
    }
    return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  });
}

function cap(entries: TimelineEntry[], droppedOlder: number): Pick<TimelineState, 'entries' | 'droppedOlder'> {
  if (entries.length <= TIMELINE_CAP) {
    return { entries, droppedOlder };
  }
  const cut = entries.length - TIMELINE_CAP;
  return { entries: entries.slice(cut), droppedOlder: droppedOlder + cut };
}

/**
 * Short relative age of an ISO timestamp ("now", "42 s", "5 min", "3 h").
 * An unparseable timestamp renders as '' (never a NaN).
 */
export function relativeTime(ts: string, now: number): string {
  const at = Date.parse(ts);
  if (Number.isNaN(at)) {
    return '';
  }
  const seconds = Math.max(0, Math.round((now - at) / 1000));
  if (seconds < 5) {
    return 'now';
  }
  if (seconds < 60) {
    return `${seconds} s`;
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min`;
  }
  return `${Math.round(minutes / 60)} h`;
}
