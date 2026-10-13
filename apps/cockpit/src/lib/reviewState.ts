/**
 * Review state as a pure reducer — the panel component stays a thin
 * wrapper around this (same contract as `sessionState.ts`/`timeline.ts`).
 *
 * The panel orchestrates the rpc calls (load → list + statuses → diff of
 * the selected task, auto-refresh after every action); this module owns
 * every state transition and every derived value the panel renders.
 */

import type { ApprovalState, CandidateDiff, FileDiff, MergeOutcome, TaskStatus, TaskSummary } from './taskApi';

/** Badge label for a task's approval/candidate state. */
export type ApprovalBadge =
  | 'no-checkpoints'
  | 'no-approval'
  | 'valid'
  | 'stale-candidate'
  | 'base-moved';

/** The last merge, bound to the repo and task it belongs to. Cleanup
 * (and the merge note) are offered only while that exact task is
 * selected — see [`boundMergeOutcome`]. */
export interface LastMerge {
  repo: string;
  task_id: string;
  outcome: MergeOutcome;
}

export interface ReviewState {
  /** The repo whose task queue is loaded (null before the first Load). */
  repo: string | null;
  tasks: TaskSummary[];
  /** Latest `task.status` per task id (drives the list badges). */
  statuses: Record<string, TaskStatus>;
  selectedTaskId: string | null;
  /** Candidate diff of the selected task. */
  diff: CandidateDiff | null;
  /** Selected file path inside `diff` (a `FileDiff.path`, renames incl. ` -> `). */
  selectedFile: string | null;
  /** Bumped by every completed queue load; the panel refetches the diff
   * when it changes (post-action freshness) and on selection change. */
  diffEpoch: number;
  /** Last merge outcome, bound to its repo+task (see [`LastMerge`]). */
  lastMerge: LastMerge | null;
  /** An rpc action is in flight (buttons disabled). */
  busy: boolean;
  error: string | null;
}

export type ReviewAction =
  /** Load/Refresh pressed: list+statuses+diff in flight. */
  | { type: 'loadStarted' }
  /** The queue snapshot arrived; selection survives when the task does. */
  | { type: 'loaded'; repo: string; tasks: TaskSummary[]; statuses: Record<string, TaskStatus> }
  | { type: 'loadFailed'; message: string }
  | { type: 'taskSelected'; taskId: string }
  | { type: 'fileSelected'; path: string }
  | { type: 'diffLoaded'; diff: CandidateDiff }
  /** No diff to load (e.g. the selected task has no checkpoints yet) —
   * clear the view instead of fetching into a guaranteed error
   * (P6 review MAJOR-1). */
  | { type: 'diffCleared' }
  /** A diff fetch failed: record the error and drop any stale patch
   * (P6 review MAJOR-1). */
  | { type: 'diffFailed'; message: string }
  /** An action rpc (checkpoint/approve/merge/…) is in flight. */
  | { type: 'actionStarted' }
  | { type: 'actionFailed'; message: string }
  /** Any action succeeded and its refresh finished (busy off). */
  | { type: 'actionSettled' }
  /** Merge answered — the outcome is recorded bound to `repo`+`taskId`;
   * `merged` arms Cleanup for that task only (P6 review BLOCKER). */
  | { type: 'mergeFinished'; repo: string; taskId: string; outcome: MergeOutcome }
  /** Cleanup (discard) done after a merge. */
  | { type: 'cleanupDone' };

export const initialReviewState: ReviewState = {
  repo: null,
  tasks: [],
  statuses: {},
  selectedTaskId: null,
  diff: null,
  selectedFile: null,
  diffEpoch: 0,
  lastMerge: null,
  busy: false,
  error: null,
};

export function reviewReducer(state: ReviewState, action: ReviewAction): ReviewState {
  switch (action.type) {
    case 'loadStarted':
      return { ...state, busy: true, error: null };
    case 'loaded': {
      // Selection survives a refresh when the task still exists; otherwise
      // the first task (if any) gets it so the diff pane is never stale.
      const selectedTaskId = state.selectedTaskId !== null &&
        action.tasks.some((t) => t.task_id === state.selectedTaskId)
        ? state.selectedTaskId
        : (action.tasks[0]?.task_id ?? null);
      const selectedChanged = selectedTaskId !== state.selectedTaskId;
      return {
        ...state,
        repo: action.repo,
        tasks: action.tasks,
        statuses: action.statuses,
        selectedTaskId,
        busy: false,
        diffEpoch: state.diffEpoch + 1,
        // New selection → previous diff is stale; drop it (and the file
        // choice) until the panel loads the new one.
        diff: selectedChanged ? null : state.diff,
        selectedFile: selectedChanged ? null : state.selectedFile,
        error: null,
      };
    }
    case 'loadFailed':
      return { ...state, busy: false, error: action.message };
    case 'taskSelected':
      if (action.taskId === state.selectedTaskId) {
        return state;
      }
      return { ...state, selectedTaskId: action.taskId, diff: null, selectedFile: null };
    case 'fileSelected':
      return { ...state, selectedFile: action.path };
    case 'diffLoaded': {
      // Keep the open file when it survived the refresh (re-checkpoint of
      // another file must not jump the view); else take the first file.
      const selectedFile =
        state.selectedFile !== null && action.diff.files.some((f) => f.path === state.selectedFile)
          ? state.selectedFile
          : (action.diff.files[0]?.path ?? null);
      return { ...state, diff: action.diff, selectedFile };
    }
    case 'diffCleared':
      if (state.diff === null && state.selectedFile === null) {
        return state;
      }
      return { ...state, diff: null, selectedFile: null };
    case 'diffFailed':
      return { ...state, diff: null, selectedFile: null, error: action.message };
    case 'actionStarted':
      return { ...state, busy: true, error: null };
    case 'actionFailed':
      return { ...state, busy: false, error: action.message };
    case 'actionSettled':
      return { ...state, busy: false };
    case 'mergeFinished':
      return {
        ...state,
        lastMerge: { repo: action.repo, task_id: action.taskId, outcome: action.outcome },
        busy: false,
      };
    case 'cleanupDone':
      return { ...state, lastMerge: null };
  }
}

// ------------------------------------------------------------- selectors

export function selectedTask(state: ReviewState): TaskSummary | null {
  return state.tasks.find((t) => t.task_id === state.selectedTaskId) ?? null;
}

/** The last merge outcome **iff** it belongs to the current repo and the
 * currently selected task — `null` otherwise. Cleanup (and the merge
 * note) must only ever be offered through this selector: a free-floating
 * `lastMerge` made Cleanup discard whatever task happened to be selected
 * after a merge (P6 review BLOCKER). */
export function boundMergeOutcome(state: ReviewState): MergeOutcome | null {
  const last = state.lastMerge;
  if (last === null || last.repo !== state.repo || last.task_id !== state.selectedTaskId) {
    return null;
  }
  return last.outcome;
}

export function selectedStatus(state: ReviewState): TaskStatus | null {
  return state.selectedTaskId !== null ? (state.statuses[state.selectedTaskId] ?? null) : null;
}

export function selectedFileDiff(state: ReviewState): FileDiff | null {
  if (state.diff === null || state.selectedFile === null) {
    return null;
  }
  return state.diff.files.find((f) => f.path === state.selectedFile) ?? null;
}

export function approvalBadge(status: TaskStatus | null): ApprovalBadge {
  if (status === null || status.status === 'no_checkpoints') {
    return 'no-checkpoints';
  }
  return approvalBadgeForState(status.approval_state);
}

function approvalBadgeForState(state: ApprovalState | null): ApprovalBadge {
  switch (state) {
    case null:
      return 'no-approval';
    case 'valid':
      return 'valid';
    case 'stale_candidate':
      return 'stale-candidate';
    case 'base_moved':
      return 'base-moved';
  }
}

/** Short hex oid for display (`1a2b3c4`), full value in `title`. */
export function shortOid(hex: string): string {
  return hex.slice(0, 7);
}

// ------------------------------------------------------------ patch view

export type DiffLineKind = 'add' | 'del' | 'hunk' | 'meta' | 'ctx';

/**
 * Classify one patch line for coloring. File headers (`diff --git`,
 * `index`, `---`, `+++`) are `meta` — note the order: `+++`/`---` are
 * checked before `+`/`-` so header lines are never misread as content.
 */
export function diffLineKind(line: string): DiffLineKind {
  if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff --git') || line.startsWith('index ')) {
    return 'meta';
  }
  if (line.startsWith('@@')) {
    return 'hunk';
  }
  if (line.startsWith('+')) {
    return 'add';
  }
  if (line.startsWith('-')) {
    return 'del';
  }
  return 'ctx';
}

/** Patch text → renderable lines (empty trailing line dropped). */
export function splitPatch(patch: string): string[] {
  const lines = patch.split('\n');
  if (lines.length > 0 && lines[lines.length - 1] === '') {
    lines.pop();
  }
  return lines;
}

/** One-character marker per change type for the file list. */
export function changeTypeIcon(change: FileDiff['change_type']): string {
  switch (change) {
    case 'added':
      return 'A';
    case 'deleted':
      return 'D';
    case 'modified':
      return 'M';
    case 'renamed':
      return 'R';
    case 'other':
      return '?';
  }
}
