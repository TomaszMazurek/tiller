import { describe, expect, it } from 'vitest';
import {
  initialTimelineState,
  relativeTime,
  TIMELINE_CAP,
  timelineReducer,
  type TimelineEntry,
} from './timeline';

function message(id: string, ts = '2026-10-11T12:00:00.000Z', chars = 3): TimelineEntry {
  return { kind: 'message', id, role: 'user', ts, chars, blocks: 1 };
}

describe('timelineReducer', () => {
  it('starts empty and resets', () => {
    const filled = timelineReducer(initialTimelineState, {
      type: 'snapshot',
      entries: [message('1')],
    });
    expect(filled.entries).toHaveLength(1);
    expect(timelineReducer(filled, { type: 'reset' })).toEqual(initialTimelineState);
  });

  it('append keeps arrival order', () => {
    let state = timelineReducer(initialTimelineState, { type: 'append', entry: message('1') });
    state = timelineReducer(state, { type: 'append', entry: message('2') });
    expect(state.entries.map((e) => e.id)).toEqual(['1', '2']);
  });

  it('append drops an entry whose id the list already had', () => {
    // The race: a live event delivered after the snapshot read the same line.
    let state = timelineReducer(initialTimelineState, {
      type: 'snapshot',
      entries: [message('1'), message('2')],
    });
    state = timelineReducer(state, { type: 'append', entry: message('2') });
    expect(state.entries.map((e) => e.id)).toEqual(['1', '2']);
  });

  it('append never dedupes on an empty id', () => {
    let state = timelineReducer(initialTimelineState, { type: 'append', entry: message('') });
    state = timelineReducer(state, { type: 'append', entry: message('') });
    expect(state.entries).toHaveLength(2);
  });

  it('buffers live entries while the snapshot request is in flight and merges them after (P1-1)', () => {
    // The race the fix targets: the snapshot rpc reads the file, an entry
    // is written *after* that read, the rpc response lands, the live
    // event for the new entry arrives before the response is applied.
    // Buffering during the request + merging after = no loss, no dupes.
    let state = timelineReducer(initialTimelineState, { type: 'observeStart' });
    state = timelineReducer(state, {
      type: 'append',
      entry: message('1', '2026-10-11T12:00:01.000Z'),
    });
    state = timelineReducer(state, {
      type: 'append',
      entry: message('2', '2026-10-11T12:00:02.000Z'),
    });
    // Nothing shows while observing — the view never flickers half a
    // list that the snapshot is about to replace.
    expect(state.entries).toEqual([]);
    expect(state.observing).toBe(true);

    // The snapshot contains 0 and 1 (1 raced the read and also arrived
    // live): after the merge every entry appears exactly once.
    state = timelineReducer(state, {
      type: 'snapshot',
      entries: [
        message('0', '2026-10-11T12:00:00.000Z'),
        message('1', '2026-10-11T12:00:01.000Z'),
      ],
    });
    expect(state.entries.map((e) => e.id)).toEqual(['0', '1', '2']);
    expect(state.observing).toBe(false);
    expect(state.pending).toEqual([]);
  });

  it('merges the snapshot ordered by ts, then id on ties', () => {
    let state = timelineReducer(initialTimelineState, { type: 'observeStart' });
    state = timelineReducer(state, {
      type: 'append',
      entry: message('b', '2026-10-11T12:00:01.000Z'),
    });
    state = timelineReducer(state, {
      type: 'snapshot',
      entries: [message('z', '2026-10-11T12:00:02.000Z'), message('a', '2026-10-11T12:00:01.000Z')],
    });
    expect(state.entries.map((e) => e.id)).toEqual(['a', 'b', 'z']);
  });

  it('observeFailed keeps the collected entries visible (short session, NOTE)', () => {
    // A session that exits before observe answers rejects with
    // SESSION_NOT_FOUND: the buffered live entries stay in the view —
    // the terminal already shows the exit status.
    let state = timelineReducer(initialTimelineState, { type: 'observeStart' });
    state = timelineReducer(state, {
      type: 'append',
      entry: message('1', '2026-10-11T12:00:01.000Z'),
    });
    state = timelineReducer(state, { type: 'observeFailed' });
    expect(state.entries.map((e) => e.id)).toEqual(['1']);
    expect(state.observing).toBe(false);
    expect(state.pending).toEqual([]);
    // Live appends work again after the failure.
    state = timelineReducer(state, { type: 'append', entry: message('2', '2026-10-11T12:00:02.000Z') });
    expect(state.entries.map((e) => e.id)).toEqual(['1', '2']);
  });

  it('caps the list and counts the dropped head as older', () => {
    // Zero-padded ids: the ts/id merge order then matches the numeric
    // order the assertions walk.
    const many: TimelineEntry[] = [];
    for (let i = 0; i < TIMELINE_CAP + 50; i += 1) {
      many.push(message(String(i).padStart(5, '0')));
    }
    const state = timelineReducer(initialTimelineState, { type: 'snapshot', entries: many });
    expect(state.entries).toHaveLength(TIMELINE_CAP);
    expect(state.droppedOlder).toBe(50);
    // The newest end survives…
    expect(state.entries[state.entries.length - 1]?.id).toBe(
      String(TIMELINE_CAP + 49).padStart(5, '0'),
    );
    // …and appends keep rolling the window forward.
    const next = timelineReducer(state, {
      type: 'append',
      entry: message('zzz-fresh'),
    });
    expect(next.entries).toHaveLength(TIMELINE_CAP);
    expect(next.droppedOlder).toBe(51);
    expect(next.entries[next.entries.length - 1]?.id).toBe('zzz-fresh');
  });

  it('caps the merged list across the snapshot/live seam too', () => {
    let state = timelineReducer(initialTimelineState, { type: 'observeStart' });
    for (let i = 0; i < TIMELINE_CAP + 10; i += 1) {
      state = timelineReducer(state, {
        type: 'append',
        entry: message(`live-${i}`, `2026-10-11T12:${String(i % 60).padStart(2, '0')}:00.000Z`),
      });
    }
    state = timelineReducer(state, { type: 'snapshot', entries: [] });
    expect(state.entries).toHaveLength(TIMELINE_CAP);
    expect(state.droppedOlder).toBe(10);
  });
});

describe('relativeTime', () => {
  const now = Date.parse('2026-10-11T12:01:00.000Z');

  it('labels coarse ages', () => {
    expect(relativeTime('2026-10-11T12:00:58.000Z', now)).toBe('now');
    expect(relativeTime('2026-10-11T12:00:20.000Z', now)).toBe('40 s');
    expect(relativeTime('2026-10-11T11:56:00.000Z', now)).toBe('5 min');
    expect(relativeTime('2026-10-11T09:00:00.000Z', now)).toBe('3 h');
  });

  it('tolerates garbage and the future', () => {
    expect(relativeTime('not-a-date', now)).toBe('');
    expect(relativeTime('2026-10-11T12:05:00.000Z', now)).toBe('now');
  });
});
