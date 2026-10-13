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
