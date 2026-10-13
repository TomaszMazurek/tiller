/**
 * Review state as a pure reducer — the panel component stays a thin
 * wrapper around this (same contract as `sessionState.ts`/`timeline.ts`).
 *
 * The panel orchestrates the rpc calls (load → list + statuses → diff of
 * the selected task, auto-refresh after every action); this module owns
 * every state transition and every derived value the panel renders.
 */

import type {
  ApprovalState,
  CandidateDiff,
  FileDiff,
  GatesInfo,
  GateEntry,
  MergeOutcome,
  TaskGateEvent,
  TaskStatus,
  TaskSummary,
} from './taskApi';

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

/** Gates view for the **selected** task: the live/recorded entries plus
 * the freshness bookkeeping the merge warning needs. `stale` is true when
 * the evidence (or the live run) belongs to a candidate that is no longer
 * the task's candidate — either the backend said so (`stale: true`) or the
 * queue refresh moved the candidate under it. */
export interface GatesState {
  /** Candidate the current entries/evidence belong to (null ⇔ none). */
  candidate: string | null;
  entries: GateEntry[];
  running: boolean;
  stale: boolean;
  /** Any evidence or run was ever seen (drives warn vs. hint). */
  loaded: boolean;
  /** The recorded evidence says `dirty_at_end` (A.2): the worktree
   * changed during the run — incomplete proof, warned like a failed
   * gate at merge. */
  dirtyAtEnd: boolean;
  /** A live run was observed (started answer or events) whose recorded
   * evidence has not been fetched yet: arms the panel's post-run fetch
   * once every entry settles (P7 review MAJOR-2 — the record carries
   * the log tails the events do not). */
  awaitingEvidence: boolean;
  /** The post-run fetch gave up while the backend still said `running`:
   * the event-derived entries stay (they are not wiped by an empty
   * record — P7 review MAJOR-2), and the UI shows an explicit note;
   * the next refresh/action re-attempts the load. */
  evidenceLag: boolean;
}

export const emptyGates: GatesState = {
  candidate: null,
  entries: [],
  running: false,
  stale: false,
  loaded: false,
  dirtyAtEnd: false,
  awaitingEvidence: false,
  evidenceLag: false,
};

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
  /** Gates view of the selected task (P7). */
  gates: GatesState;
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
  | { type: 'cleanupDone' }
  /** `task.gates` answered for the selected task; `currentCandidate` is
   * the fresh candidate from `task.status` (staleness is derived). */
  | { type: 'gatesLoaded'; info: GatesInfo; currentCandidate: string | null }
  /** Run Gates answered: the named gates are running against `candidate`. */
  | { type: 'gatesStarted'; gates: string[]; candidate: string }
  /** The post-run evidence fetch gave up (backend still `running` after
   * retries): keep the event-derived entries, surface the lag. */
  | { type: 'evidencePending' }
  /** A `task:gate` event for the selected task (the panel pre-filters by
   * repo+task; `currentCandidate` maintains staleness). */
  | { type: 'gateEvent'; event: TaskGateEvent; currentCandidate: string | null };

export const initialReviewState: ReviewState = {
  repo: null,
  tasks: [],
  statuses: {},
  selectedTaskId: null,
  diff: null,
  selectedFile: null,
  diffEpoch: 0,
  gates: emptyGates,
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
      // A repo switch resets the gates even when the task id survives:
      // the evidence belongs to the OLD repo's worktree (P7 review
      // MINOR-2) — same shape as the selection-change reset above.
      const repoChanged = action.repo !== state.repo;
      // The selected task's fresh candidate — the authority the gates'
      // staleness is derived from on every refresh.
      const currentCandidate =
        selectedTaskId !== null
          ? (action.statuses[selectedTaskId]?.candidate?.candidate_commit ?? null)
          : null;
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
        gates: selectedChanged || repoChanged
          ? emptyGates
          : deriveGateStaleness(state.gates, currentCandidate),
        error: null,
      };
    }
    case 'loadFailed':
      return { ...state, busy: false, error: action.message };
    case 'taskSelected':
      if (action.taskId === state.selectedTaskId) {
        return state;
      }
      return {
        ...state,
        selectedTaskId: action.taskId,
        diff: null,
        selectedFile: null,
        gates: emptyGates,
      };
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
    case 'gatesLoaded': {
      const info = action.info;
      if (info.evidence !== undefined && info.candidate !== undefined) {
        // Fresh record for a specific candidate — staleness against the
        // task's *current* candidate is derived here, never trusted.
        return {
          ...state,
          gates: deriveGateStaleness(
            {
              candidate: info.candidate,
              entries: info.evidence.gates,
              running: info.running ?? false,
              stale: info.stale ?? false,
              loaded: true,
              dirtyAtEnd: info.evidence.dirty_at_end === true,
              awaitingEvidence: false,
              evidenceLag: false,
            },
            action.currentCandidate,
          ),
        };
      }
      if (info.stale === true) {
        // Backend remembers a finished run under a superseded candidate —
        // keep whatever entries the events already showed, mark stale.
        return {
          ...state,
          gates: {
            ...state.gates,
            running: info.running ?? false,
            stale: true,
            loaded: true,
            awaitingEvidence: false,
            evidenceLag: false,
          },
        };
      }
      // Nothing recorded. If the backend still says `running` and the
      // events already showed entries, KEEP them (the live run's record
      // is simply not written yet — a Refresh mid-run must not wipe the
      // view; P7 review MAJOR-2 exhaustion/Refresh path). Only a settled
      // no-record answer legitimately resets to empty.
      if ((info.running ?? false) === true && state.gates.entries.length > 0) {
        return {
          ...state,
          gates: { ...state.gates, running: true, loaded: true },
        };
      }
      return {
        ...state,
        gates: { ...emptyGates, running: info.running ?? false },
      };
    }
    case 'evidencePending':
      return {
        ...state,
        gates: { ...state.gates, awaitingEvidence: false, evidenceLag: true },
      };
    case 'gatesStarted': {
      // A late `started` answer (the run finished before the rpc
      // returned) must not resurrect the running badges over a view that
      // already watched this same run end (P7 review MAJOR-2): every
      // entry final for the same candidate ⇔ done — drop the answer. A
      // started answer under a different candidate is a real new run and
      // still resets.
      const alreadySettled =
        state.gates.candidate === action.candidate &&
        !state.gates.running &&
        state.gates.entries.length > 0 &&
        state.gates.entries.every((entry) => entry.status !== 'running');
      if (alreadySettled) {
        return state;
      }
      return {
        ...state,
        gates: {
          candidate: action.candidate,
          entries: action.gates.map((id) => ({ id, label: id, status: 'running' as const })),
          running: true,
          stale: false,
          loaded: true,
          dirtyAtEnd: false,
          awaitingEvidence: true,
          evidenceLag: false,
        },
      };
    }
    case 'gateEvent': {
      const event = action.event;
      const gates = state.gates;
      // A run starting under a new candidate resets the view (old entries
      // belong to the superseded candidate — and so does any dirt note).
      const base =
        gates.candidate === event.candidate
          ? gates
          : {
              ...gates,
              candidate: event.candidate,
              entries: [],
              stale: false,
              loaded: true,
              dirtyAtEnd: false,
              evidenceLag: false,
            };
      const entries = base.entries.some((entry) => entry.id === event.id)
        ? base.entries.map((entry) =>
            entry.id === event.id
              ? {
                  ...entry,
                  label: event.label,
                  status: event.status,
                  duration_ms: event.duration_ms,
                }
              : entry,
          )
        : [
            ...base.entries,
            {
              id: event.id,
              label: event.label,
              status: event.status,
              duration_ms: event.duration_ms,
            },
          ];
      // Every event proves a live run — its record is wanted at the end.
      return {
        ...state,
        gates: deriveGateStaleness(
          { ...base, entries, running: event.status === 'running', awaitingEvidence: true },
          action.currentCandidate,
        ),
      };
    }
  }
}

/** Staleness is derived, never stored as truth: evidence (or a live run)
 * under a candidate that is not the task's current candidate is stale —
 * a fresh record clears the flag again. */
function deriveGateStaleness(gates: GatesState, currentCandidate: string | null): GatesState {
  if (gates.candidate === null || currentCandidate === null) {
    return gates;
  }
  return { ...gates, stale: gates.candidate !== currentCandidate };
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

/** The problems a merge would sail past, as operator-readable lines —
 * the confirm() body (P7): a failed/timed-out gate, or evidence (a run
 * included) that no longer matches the current candidate. Empty array ⇔
 * nothing to warn about (no evidence at all is a *hint*, not a warn). */
export function mergeGateProblems(gates: GatesState): string[] {
  if (!gates.loaded) {
    return [];
  }
  const problems: string[] = [];
  if (gates.stale) {
    problems.push('gate evidence is stale — the candidate moved after the gates ran; re-run gates');
  }
  if (gates.dirtyAtEnd) {
    problems.push('worktree changed during gates — evidence incomplete; re-run gates');
  }
  for (const entry of gates.entries) {
    if (entry.status === 'failed' || entry.status === 'timeout') {
      problems.push(`gate ${entry.label} ${entry.status}`);
    }
  }
  return problems;
}

/** Derived merge warning: evidence exists ∧ (a failed/timed-out gate ∨ the
 * candidate moved under the evidence). Never blocks — confirm() only. */
export function mergeNeedsWarning(gates: GatesState): boolean {
  return mergeGateProblems(gates).length > 0;
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
