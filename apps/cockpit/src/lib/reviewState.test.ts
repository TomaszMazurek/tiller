import { describe, expect, it } from 'vitest';
import {
  approvalBadge,
  boundMergeOutcome,
  changeTypeIcon,
  diffLineKind,
  initialReviewState,
  reviewReducer,
  selectedFileDiff,
  selectedStatus,
  selectedTask,
  shortOid,
  splitPatch,
  type ReviewAction,
  type ReviewState,
} from './reviewState';
import type { CandidateDiff, TaskStatus, TaskSummary } from './taskApi';

function task(id: string): TaskSummary {
  return { task_id: id, target_branch: 'master', base: 'b', tip: 't' };
}

function status(overrides: Partial<TaskStatus> = {}): TaskStatus {
  return {
    status: 'candidate',
    candidate: { task_id: 't1', candidate_commit: 'c', base_commit: 'b', created_at: 0 },
    approval_state: null,
    checkpoints_count: 1,
    ...overrides,
  };
}

function diff(paths: string[]): CandidateDiff {
  return {
    files: paths.map((path) => ({
      path,
      change_type: 'modified' as const,
      patch: `+${path}`,
      binary: false,
      truncated: false,
    })),
    truncated: false,
    total_bytes: 8,
    byte_cap: 262144,
  };
}

describe('reviewReducer loads', () => {
  it('first load adopts the first task and keeps null diff until diffLoaded', () => {
    let state = reviewReducer(initialReviewState, { type: 'loadStarted' });
    expect(state.busy).toBe(true);
    state = reviewReducer(state, {
      type: 'loaded',
      repo: 'C:/repo',
      tasks: [task('t1'), task('t2')],
      statuses: { t1: status() },
    });
    expect(state.repo).toBe('C:/repo');
    expect(state.selectedTaskId).toBe('t1');
    expect(state.busy).toBe(false);
    expect(state.diff).toBeNull();
    expect(state.diffEpoch).toBe(1);

    state = reviewReducer(state, { type: 'diffLoaded', diff: diff(['a.txt', 'b.txt']) });
    expect(state.selectedFile).toBe('a.txt');
    expect(selectedFileDiff(state)?.path).toBe('a.txt');
  });

  it('refresh keeps the selection when the task survives, drops it when gone', () => {
    let state: ReviewState = {
      ...initialReviewState,
      repo: 'C:/repo',
      tasks: [task('t1'), task('t2')],
      selectedTaskId: 't2',
      diff: diff(['x.txt']),
      selectedFile: 'x.txt',
    };
    state = reviewReducer(state, {
      type: 'loaded',
      repo: 'C:/repo',
      tasks: [task('t1'), task('t3')],
      statuses: {},
    });
    expect(state.selectedTaskId).toBe('t1');
    expect(state.diff).toBeNull();
    expect(state.selectedFile).toBeNull();

    // surviving selection keeps the diff view (panel refreshes it next)
    state = reviewReducer(state, {
      type: 'loaded',
      repo: 'C:/repo',
      tasks: [task('t1')],
      statuses: {},
    });
    expect(state.selectedTaskId).toBe('t1');
    expect(state.diffEpoch).toBe(2);
  });

  it('empty queue clears selection and diff', () => {
    let state = reviewReducer(initialReviewState, {
      type: 'loaded',
      repo: 'C:/repo',
      tasks: [task('t1')],
      statuses: {},
    });
    state = reviewReducer(state, { type: 'diffLoaded', diff: diff(['a.txt']) });
    state = reviewReducer(state, { type: 'loaded', repo: 'C:/repo', tasks: [], statuses: {} });
    expect(state.selectedTaskId).toBeNull();
    expect(state.diff).toBeNull();
  });

  it('loadFailed records the error and unlocks', () => {
    const state = reviewReducer(initialReviewState, { type: 'loadStarted' });
    const failed = reviewReducer(state, { type: 'loadFailed', message: 'boom' });
    expect(failed.busy).toBe(false);
    expect(failed.error).toBe('boom');
  });
});

describe('reviewReducer selection and diff view', () => {
  it('taskSelected resets the previous diff; same selection is a no-op', () => {
    let state: ReviewState = {
      ...initialReviewState,
      tasks: [task('t1'), task('t2')],
      selectedTaskId: 't1',
      diff: diff(['a.txt']),
      selectedFile: 'a.txt',
    };
    const same = reviewReducer(state, { type: 'taskSelected', taskId: 't1' });
    expect(same).toBe(state);
    state = reviewReducer(state, { type: 'taskSelected', taskId: 't2' });
    expect(state.selectedTaskId).toBe('t2');
    expect(state.diff).toBeNull();
  });

  it('fileSelected switches the open file without touching the diff', () => {
    let state: ReviewState = {
      ...initialReviewState,
      diff: diff(['a.txt', 'b.txt']),
      selectedFile: 'a.txt',
    };
    state = reviewReducer(state, { type: 'fileSelected', path: 'b.txt' });
    expect(state.selectedFile).toBe('b.txt');
    expect(state.diff?.files.length).toBe(2);
  });

  it('diffLoaded keeps the open file when it survives, else first file', () => {
    let state: ReviewState = { ...initialReviewState, selectedFile: 'b.txt' };
    state = reviewReducer(state, { type: 'diffLoaded', diff: diff(['a.txt', 'b.txt']) });
    expect(state.selectedFile).toBe('b.txt');
    state = reviewReducer(state, { type: 'diffLoaded', diff: diff(['a.txt']) });
    expect(state.selectedFile).toBe('a.txt');
    state = reviewReducer(state, { type: 'diffLoaded', diff: { files: [], truncated: false, total_bytes: 0, byte_cap: 262144 } });
    expect(state.selectedFile).toBeNull();
    expect(selectedFileDiff(state)).toBeNull();
  });
});

describe('reviewReducer actions', () => {
  it('actionStarted guards, actionFailed records, actionSettled unlocks', () => {
    let state = reviewReducer(initialReviewState, { type: 'actionStarted' });
    expect(state.busy).toBe(true);
    expect(state.error).toBeNull();
    state = reviewReducer(state, { type: 'actionFailed', message: 'stale approval' });
    expect(state.busy).toBe(false);
    expect(state.error).toBe('stale approval');
    state = reviewReducer(state, { type: 'actionStarted' });
    state = reviewReducer(state, { type: 'actionSettled' });
    expect(state.busy).toBe(false);
  });

  it('mergeFinished binds the outcome to repo+task; cleanupDone clears it', () => {
    const outcome = { merged: { merge_commit: 'm', main_worktree_updated: true } } as const;
    let state: ReviewState = {
      ...initialReviewState,
      repo: 'C:/repo',
      selectedTaskId: 't1',
    };
    state = reviewReducer(state, { type: 'mergeFinished', repo: 'C:/repo', taskId: 't1', outcome });
    expect(state.lastMerge).toEqual({ repo: 'C:/repo', task_id: 't1', outcome });
    expect(boundMergeOutcome(state)).toBe(outcome);
    state = reviewReducer(state, { type: 'cleanupDone' });
    expect(state.lastMerge).toBeNull();
    expect(boundMergeOutcome(state)).toBeNull();
  });

  it('boundMergeOutcome follows the selection and the repo (Cleanup safety)', () => {
    // P6 review BLOCKER: after merging A, selecting B must NOT offer
    // Cleanup for B — the outcome stays bound to the merged task.
    const outcome = { merged: { merge_commit: 'm', main_worktree_updated: true } } as const;
    let state: ReviewState = {
      ...initialReviewState,
      repo: 'C:/repo',
      tasks: [task('t1'), task('t2')],
      selectedTaskId: 't1',
      lastMerge: { repo: 'C:/repo', task_id: 't1', outcome },
    };
    state = reviewReducer(state, { type: 'taskSelected', taskId: 't2' });
    expect(boundMergeOutcome(state)).toBeNull();
    state = reviewReducer(state, { type: 'taskSelected', taskId: 't1' });
    expect(boundMergeOutcome(state)).toBe(outcome);
    // a different repo is a different queue — never bound either
    state = reviewReducer(state, { type: 'loaded', repo: 'C:/other', tasks: [], statuses: {} });
    expect(boundMergeOutcome(state)).toBeNull();
  });

  it('diffCleared empties the diff view; diffFailed also records the error', () => {
    let state: ReviewState = {
      ...initialReviewState,
      diff: diff(['a.txt']),
      selectedFile: 'a.txt',
    };
    state = reviewReducer(state, { type: 'diffCleared' });
    expect(state.diff).toBeNull();
    expect(state.selectedFile).toBeNull();
    expect(state.error).toBeNull();

    // a failed diff fetch must not leave the previous patch on screen
    state = reviewReducer(state, { type: 'diffLoaded', diff: diff(['a.txt']) });
    state = reviewReducer(state, { type: 'diffFailed', message: 'diff rpc exploded' });
    expect(state.diff).toBeNull();
    expect(state.selectedFile).toBeNull();
    expect(state.error).toBe('diff rpc exploded');
  });
});

describe('selectors and view helpers', () => {
  it('selectedTask/selectedStatus resolve or null out', () => {
    const state: ReviewState = {
      ...initialReviewState,
      tasks: [task('t1')],
      statuses: { t1: status() },
      selectedTaskId: 't1',
    };
    expect(selectedTask(state)?.task_id).toBe('t1');
    expect(selectedStatus(state)?.checkpoints_count).toBe(1);
    expect(selectedTask({ ...state, selectedTaskId: 'gone' })).toBeNull();
    expect(selectedStatus({ ...state, selectedTaskId: 'gone' })).toBeNull();
  });

  it('approvalBadge maps every backend state', () => {
    expect(approvalBadge(null)).toBe('no-checkpoints');
    expect(approvalBadge(status({ status: 'no_checkpoints', candidate: null, checkpoints_count: 0 }))).toBe('no-checkpoints');
    expect(approvalBadge(status())).toBe('no-approval');
    expect(approvalBadge(status({ approval_state: 'valid' }))).toBe('valid');
    expect(approvalBadge(status({ approval_state: 'stale_candidate' }))).toBe('stale-candidate');
    expect(approvalBadge(status({ approval_state: 'base_moved' }))).toBe('base-moved');
  });

  it('shortOid cuts to 7 chars', () => {
    expect(shortOid('1a2b3c4d5e6f7890')).toBe('1a2b3c4');
  });

  it('diffLineKind classifies header/hunk/add/del/context in order', () => {
    expect(diffLineKind('diff --git a/x b/x')).toBe('meta');
    expect(diffLineKind('index 123..456 100644')).toBe('meta');
    expect(diffLineKind('--- a/x')).toBe('meta');
    expect(diffLineKind('+++ b/x')).toBe('meta'); // before '+', not 'add'
    expect(diffLineKind('@@ -1 +1 @@')).toBe('hunk');
    expect(diffLineKind('+added')).toBe('add');
    expect(diffLineKind('-removed')).toBe('del');
    expect(diffLineKind(' context')).toBe('ctx');
    expect(diffLineKind('')).toBe('ctx');
  });

  it('splitPatch drops the trailing empty line', () => {
    expect(splitPatch('+a\n+b\n')).toEqual(['+a', '+b']);
    expect(splitPatch('')).toEqual([]);
  });

  it('changeTypeIcon maps all change types', () => {
    expect(changeTypeIcon('added')).toBe('A');
    expect(changeTypeIcon('deleted')).toBe('D');
    expect(changeTypeIcon('modified')).toBe('M');
    expect(changeTypeIcon('renamed')).toBe('R');
    expect(changeTypeIcon('other')).toBe('?');
  });
});

// ------------------------------------------------------------- gates (P7)

function gatesStateOf(state: ReviewState) {
  return state.gates;
}

describe('reviewReducer gates', () => {
  it('gatesLoaded adopts fresh evidence and derives staleness against the current candidate', () => {
    const evidenceAction = {
      type: 'gatesLoaded',
      info: {
        running: false,
        candidate: 'c1',
        evidence: {
          gates: [
            { id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 1000, log_tail: 'ok' },
            { id: 'cargo-test', label: 'cargo test', status: 'failed', exit_code: 101, duration_ms: 2000, log_tail: 'boom' },
          ],
          dirty_at_start: false,
          finished_at: 5,
        },
      },
      currentCandidate: 'c1',
    } satisfies ReviewAction;
    const loaded = reviewReducer(initialReviewState, evidenceAction);
    expect(gatesStateOf(loaded)).toEqual({
      candidate: 'c1',
      entries: loaded.gates.entries,
      running: false,
      stale: false,
      loaded: true,
      dirtyAtEnd: false,
      awaitingEvidence: false,
      evidenceLag: false,
    });
    // candidate moved → the queue refresh derives staleness from the
    // task's fresh candidate (evidence still bound to c1)
    let queue = reviewReducer(initialReviewState, {
      type: 'loaded',
      repo: 'r',
      tasks: [task('t1')],
      statuses: { t1: status() },
    });
    queue = reviewReducer(queue, evidenceAction);
    const derived = reviewReducer(queue, {
      type: 'loaded',
      repo: 'r',
      tasks: [task('t1')],
      statuses: { t1: status({ candidate: { task_id: 't1', candidate_commit: 'c2', base_commit: 'b', created_at: 0 } }) },
    });
    expect(derived.gates.stale).toBe(true); // derived on queue refresh
    expect(derived.gates.candidate).toBe('c1');
  });

  it('gatesLoaded with stale flag keeps the seen entries; empty info resets', () => {
    const started = reviewReducer(initialReviewState, {
      type: 'gatesStarted',
      gates: ['cargo-check', 'cargo-test'],
      candidate: 'c1',
    });
    expect(started.gates).toEqual({
      candidate: 'c1',
      entries: [
        { id: 'cargo-check', label: 'cargo-check', status: 'running' },
        { id: 'cargo-test', label: 'cargo-test', status: 'running' },
      ],
      running: true,
      stale: false,
      loaded: true,
      dirtyAtEnd: false,
      // a live run is underway: the record is fetched at the end (MAJOR-2)
      awaitingEvidence: true,
      evidenceLag: false,
    });
    const stale = reviewReducer(started, {
      type: 'gatesLoaded',
      info: { running: false, stale: true },
      currentCandidate: 'c2',
    });
    expect(stale.gates.stale).toBe(true);
    expect(stale.gates.running).toBe(false);
    expect(stale.gates.entries).toHaveLength(2);
    expect(stale.gates.awaitingEvidence).toBe(false);

    const none = reviewReducer(stale, { type: 'gatesLoaded', info: {}, currentCandidate: null });
    expect(none.gates).toEqual({ candidate: null, entries: [], running: false, stale: false, loaded: false, dirtyAtEnd: false, awaitingEvidence: false, evidenceLag: false });
  });

  it('gateEvent upserts entries, resets on a new candidate, and flips running off at the end', () => {
    let state = reviewReducer(initialReviewState, {
      type: 'gatesStarted',
      gates: ['cargo-check'],
      candidate: 'c1',
    });
    state = reviewReducer(state, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c1', id: 'cargo-check', label: 'cargo check', status: 'failed', duration_ms: 42 },
      currentCandidate: 'c1',
    });
    expect(state.gates.entries[0]).toEqual({
      id: 'cargo-check',
      label: 'cargo check',
      status: 'failed',
      duration_ms: 42,
    });
    expect(state.gates.running).toBe(false);

    // an event for another candidate resets the view under that candidate
    state = reviewReducer(state, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c9', id: 'cargo-test', label: 'cargo test', status: 'running' },
      currentCandidate: 'c9',
    });
    expect(state.gates.candidate).toBe('c9');
    expect(state.gates.entries).toEqual([
      { id: 'cargo-test', label: 'cargo test', status: 'running' },
    ]);
    expect(state.gates.running).toBe(true);

    // a live run whose candidate is no longer current is stale
    state = reviewReducer(state, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c9', id: 'cargo-test', label: 'cargo test', status: 'passed', duration_ms: 1 },
      currentCandidate: 'c10',
    });
    expect(state.gates.stale).toBe(true);
  });

  it('selection change resets the gates view with the diff', () => {
    let state = reviewReducer(initialReviewState, {
      type: 'gatesLoaded',
      info: { running: false, candidate: 'c1', evidence: { gates: [], dirty_at_start: false, finished_at: 1 } },
      currentCandidate: 'c1',
    });
    state = reviewReducer(state, { type: 'loaded', repo: 'r', tasks: [task('t1'), task('t2')], statuses: { t1: status(), t2: status() } });
    state = reviewReducer(state, { type: 'taskSelected', taskId: 't2' });
    expect(state.gates).toEqual({ candidate: null, entries: [], running: false, stale: false, loaded: false, dirtyAtEnd: false, awaitingEvidence: false, evidenceLag: false });
  });

  it('a repo switch resets the gates even when the task id survives (MINOR-2)', () => {
    let state = reviewReducer(initialReviewState, {
      type: 'gatesLoaded',
      info: { running: false, candidate: 'c1', evidence: { gates: [], dirty_at_start: false, finished_at: 1 } },
      currentCandidate: 'c1',
    });
    // same task_id under another repo: the evidence belongs to the OLD
    // repo's worktree — keeping it would show another repo's proof
    state = reviewReducer(state, { type: 'loaded', repo: 'r2', tasks: [task('t1')], statuses: { t1: status() } });
    expect(state.gates).toEqual({ candidate: null, entries: [], running: false, stale: false, loaded: false, dirtyAtEnd: false, awaitingEvidence: false, evidenceLag: false });
    // a refresh of the SAME repo keeps the view
    state = reviewReducer(state, {
      type: 'gatesLoaded',
      info: { running: false, candidate: 'c1', evidence: { gates: [], dirty_at_start: false, finished_at: 1 } },
      currentCandidate: 'c1',
    });
    state = reviewReducer(state, { type: 'loaded', repo: 'r2', tasks: [task('t1')], statuses: { t1: status() } });
    expect(state.gates.loaded).toBe(true);
  });

  // MAJOR-2: the recorded evidence (with the log tails) is fetched after
  // the run's last event — the reducer arms that fetch and never lets a
  // late `started` answer resurrect a finished run.

  it('normal order: events arm the evidence fetch once every entry settles', () => {
    let state = reviewReducer(initialReviewState, {
      type: 'gatesStarted',
      gates: ['cargo-check', 'cargo-test'],
      candidate: 'c1',
    });
    state = reviewReducer(state, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c1', id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 1 },
      currentCandidate: 'c1',
    });
    // one gate done, one still live: still fetching-not-yet
    expect(state.gates.awaitingEvidence).toBe(true);
    state = reviewReducer(state, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c1', id: 'cargo-test', label: 'cargo test', status: 'failed', duration_ms: 2 },
      currentCandidate: 'c1',
    });
    // the LAST event (running:false in the state) still wants the record
    expect(state.gates.running).toBe(false);
    expect(state.gates.awaitingEvidence).toBe(true);
    // the record lands: fetch discharged, log tails now renderable
    state = reviewReducer(state, {
      type: 'gatesLoaded',
      info: {
        running: false,
        candidate: 'c1',
        evidence: { gates: [{ id: 'cargo-test', label: 'cargo test', status: 'failed', log_tail: 'boom' }], dirty_at_start: false, finished_at: 9 },
      },
      currentCandidate: 'c1',
    });
    expect(state.gates.awaitingEvidence).toBe(false);
    expect(state.gates.entries[0].log_tail).toBe('boom');
  });

  it('retry exhaustion (evidencePending) keeps entries, flags the lag; a later load clears it', () => {
    let state = reviewReducer(initialReviewState, {
      type: 'gatesStarted',
      gates: ['cargo-check'],
      candidate: 'c1',
    });
    state = reviewReducer(state, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c1', id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 5 },
      currentCandidate: 'c1',
    });
    // the fetch gives up while the backend still says running
    state = reviewReducer(state, { type: 'evidencePending' });
    expect(state.gates.awaitingEvidence).toBe(false);
    expect(state.gates.evidenceLag).toBe(true);
    expect(state.gates.entries.map((entry) => entry.status)).toEqual(['passed']);
    // the record finally arrives on the next refresh
    state = reviewReducer(state, {
      type: 'gatesLoaded',
      info: {
        running: false,
        candidate: 'c1',
        evidence: { gates: [{ id: 'cargo-check', label: 'cargo check', status: 'passed', log_tail: 'ok-tail' }], dirty_at_start: false, finished_at: 9 },
      },
      currentCandidate: 'c1',
    });
    expect(state.gates.evidenceLag).toBe(false);
    expect(state.gates.entries[0].log_tail).toBe('ok-tail');
  });

  it('a mid-run Refresh (gatesLoaded running:true, no record) keeps the event entries; a settled empty answer resets', () => {
    let state = reviewReducer(initialReviewState, {
      type: 'gatesStarted',
      gates: ['cargo-check'],
      candidate: 'c1',
    });
    state = reviewReducer(state, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c1', id: 'cargo-check', label: 'cargo check', status: 'running' },
      currentCandidate: 'c1',
    });
    // Refresh while the run is still in flight: nothing recorded yet,
    // the running answer must not wipe what the events showed
    state = reviewReducer(state, {
      type: 'gatesLoaded',
      info: { running: true },
      currentCandidate: 'c1',
    });
    expect(state.gates.entries.map((entry) => entry.id)).toEqual(['cargo-check']);
    expect(state.gates.running).toBe(true);
    // a settled no-record answer (run gone, nothing written) resets
    state = reviewReducer(state, {
      type: 'gatesLoaded',
      info: {},
      currentCandidate: 'c1',
    });
    expect(state.gates.entries).toEqual([]);
    expect(state.gates.running).toBe(false);
  });

  it('fast order: a late started answer cannot resurrect the finished run', () => {
    // the whole run beat the rpc answer: every entry already final
    let state = reviewReducer(initialReviewState, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c1', id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 1 },
      currentCandidate: 'c1',
    });
    let finished = reviewReducer(state, {
      type: 'gateEvent',
      event: { task_id: 't1', candidate: 'c1', id: 'cargo-test', label: 'cargo test', status: 'passed', duration_ms: 2 },
      currentCandidate: 'c1',
    });
    expect(finished.gates.running).toBe(false);
    expect(finished.gates.awaitingEvidence).toBe(true);

    // the rpc answer arrives LAST and must not reset to running badges
    const late = reviewReducer(finished, {
      type: 'gatesStarted',
      gates: ['cargo-check', 'cargo-test'],
      candidate: 'c1',
    });
    expect(late).toBe(finished); // dropped wholesale, state untouched

    // but a started answer for a NEW candidate still resets (a real new run)
    const fresh = reviewReducer(finished, {
      type: 'gatesStarted',
      gates: ['cargo-check'],
      candidate: 'c2',
    });
    expect(fresh.gates.candidate).toBe('c2');
    expect(fresh.gates.running).toBe(true);
  });

  it('dirty_at_end evidence marks the run incomplete-proof (A.2)', () => {
    let state = reviewReducer(initialReviewState, {
      type: 'gatesLoaded',
      info: {
        running: false,
        candidate: 'c1',
        evidence: {
          gates: [{ id: 'cargo-check', label: 'cargo check', status: 'passed' }],
          dirty_at_start: false,
          dirty_at_end: true,
          dirty_paths: ['a.txt'],
          finished_at: 9,
        },
      },
      currentCandidate: 'c1',
    });
    expect(state.gates.dirtyAtEnd).toBe(true);
    // a new run clears the annotation until fresh evidence says otherwise
    state = reviewReducer(state, { type: 'gatesStarted', gates: ['cargo-check'], candidate: 'c2' });
    expect(state.gates.dirtyAtEnd).toBe(false);
  });
});

describe('mergeNeedsWarning', () => {
  it('warns on failed/timeout gates, stale evidence and dirty_at_end; silence when nothing ran', async () => {
    const { mergeNeedsWarning, mergeGateProblems } = await import('./reviewState');
    const empty = { candidate: null, entries: [], running: false, stale: false, loaded: false, dirtyAtEnd: false, awaitingEvidence: false, evidenceLag: false };
    expect(mergeNeedsWarning(empty)).toBe(false); // no evidence → hint, not warn

    const passed = { ...empty, loaded: true, candidate: 'c1', entries: [{ id: 'cargo-check', label: 'cargo check', status: 'passed' as const }] };
    expect(mergeNeedsWarning(passed)).toBe(false);

    const failed = { ...passed, entries: [...passed.entries, { id: 'cargo-test', label: 'cargo test', status: 'failed' as const }] };
    expect(mergeNeedsWarning(failed)).toBe(true);
    expect(mergeGateProblems(failed)).toEqual(['gate cargo test failed']);

    const timedOut = { ...passed, entries: [{ id: 'cargo-test', label: 'cargo test', status: 'timeout' as const }] };
    expect(mergeGateProblems(timedOut)).toEqual(['gate cargo test timeout']);

    const stale = { ...passed, stale: true };
    expect(mergeGateProblems(stale)).toEqual(['gate evidence is stale — the candidate moved after the gates ran; re-run gates']);

    // A.2: the worktree changed during the run — the record is annotated
    // dirt, the merge must warn like a failed gate
    const dirty = { ...passed, dirtyAtEnd: true };
    expect(mergeGateProblems(dirty)).toEqual([
      'worktree changed during gates — evidence incomplete; re-run gates',
    ]);
    expect(mergeNeedsWarning(dirty)).toBe(true);
  });
});
