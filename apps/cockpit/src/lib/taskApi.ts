/**
 * Typed helpers for the `task.*` rpc surface (review transactions over
 * tiller-git).
 *
 * Mirrors `termApi.ts`/`sessionApi.ts`: the generic call path is
 * `rpcCall`, these helpers add the wire types. Field names are the
 * backend's snake_case serde names — they mirror the structs in
 * `src-tauri/src/task_host.rs` and `crates/git/src/workspace.rs` verbatim.
 */

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { rpcCall, type InvokeFn } from './rpcClient';
import type { TermListenFn } from './termApi';

export interface TaskSummary {
  task_id: string;
  target_branch: string;
  /** The task worktree's directory — where a terminal session for the
   * task opens (P8a task↔terminal handoff). */
  worktree_path: string;
  base: string;
  tip: string;
}

export interface TaskCreated {
  task_id: string;
  worktree_path: string;
  target_branch: string;
}

/** `'no_checkpoints'` ⇔ the task branch still sits on its base. */
export type TaskPhase = 'no_checkpoints' | 'candidate';

/** Serde `ApprovalState` (snake_case); `null` ⇔ never approved. */
export type ApprovalState = 'valid' | 'stale_candidate' | 'base_moved';

export interface ReviewCandidate {
  task_id: string;
  candidate_commit: string;
  base_commit: string;
  created_at: number;
}

export interface TaskStatus {
  status: TaskPhase;
  candidate: ReviewCandidate | null;
  approval_state: ApprovalState | null;
  checkpoints_count: number;
}

export type ChangeType = 'added' | 'deleted' | 'modified' | 'renamed' | 'other';

export interface FileDiff {
  path: string;
  change_type: ChangeType;
  patch: string;
  binary: boolean;
  truncated: boolean;
}

export interface CandidateDiff {
  files: FileDiff[];
  truncated: boolean;
  total_bytes: number;
  byte_cap: number;
}

/** Serde `MergeOutcome` (externally tagged, snake_case). */
export type MergeOutcome =
  | { merged: { merge_commit: string; main_worktree_updated: boolean } }
  | { needs_rebase: { new_base: string } };

export interface TaskRebased {
  ok: boolean;
  new_tip: string;
}

// ------------------------------------------------------------------ gates

/** One gate's terminal status (`running` is event-only). */
export type GateStatus = 'passed' | 'failed' | 'timeout';

/** A gate entry as the UI shows it: live (event) or recorded (evidence). */
export interface GateEntry {
  id: string;
  label: string;
  status: 'running' | GateStatus;
  exit_code?: number | null;
  duration_ms?: number;
  log_tail?: string;
}

/** The persisted evidence record (`task.gates` → `evidence`). */
export interface GatesEvidence {
  gates: GateEntry[];
  dirty_at_start: boolean;
  /** A.2: the worktree was dirty when the run ended (changed or
   * untracked) — the record is annotated, not discarded; the UI treats
   * it as incomplete proof. `dirty_paths` rides along when true. */
  dirty_at_end?: boolean;
  dirty_paths?: string[];
  finished_at: number;
}

/** `task.gates` answer: `{running}` | `{running, stale}` | `{running,
 * candidate, evidence}` (absent fields are omitted by the backend). */
export interface GatesInfo {
  running?: boolean;
  candidate?: string;
  evidence?: GatesEvidence;
  stale?: boolean;
}

/** `task.run_gates` answer: what started, against which candidate. */
export interface GatesStarted {
  started: string[];
  candidate: string;
}

/** `task.gate` event payload (status change per gate). */
export interface TaskGateEvent {
  repo?: string;
  task_id: string;
  candidate: string;
  id: string;
  label: string;
  status: 'running' | GateStatus;
  duration_ms?: number;
}

function expectObject(method: string, result: unknown): Record<string, unknown> {
  if (result === null || typeof result !== 'object' || Array.isArray(result)) {
    throw new Error(`${method}: result is not an object`);
  }
  return result as Record<string, unknown>;
}

async function callTask<T>(
  method: string,
  params: Record<string, unknown>,
  invokeFn: InvokeFn,
): Promise<T> {
  return (await rpcCall(method, params, invokeFn)) as T;
}

export async function taskList(repo: string, invokeFn: InvokeFn = invoke): Promise<TaskSummary[]> {
  const result = await rpcCall('task.list', { repo }, invokeFn);
  if (!Array.isArray(result)) {
    throw new Error('task.list: result is missing a task array');
  }
  return result as TaskSummary[];
}

export async function taskCreate(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<TaskCreated> {
  const result = await callTask('task.create', { repo, task_id: taskId }, invokeFn);
  const obj = expectObject('task.create', result);
  if (typeof obj.task_id !== 'string' || typeof obj.worktree_path !== 'string') {
    throw new Error('task.create: result is missing "task_id"/"worktree_path" strings');
  }
  return result as TaskCreated;
}

export async function taskStatus(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<TaskStatus> {
  const result = await callTask('task.status', { repo, task_id: taskId }, invokeFn);
  const obj = expectObject('task.status', result);
  if (typeof obj.status !== 'string' || typeof obj.checkpoints_count !== 'number') {
    throw new Error('task.status: result is missing "status"/"checkpoints_count"');
  }
  return result as TaskStatus;
}

export async function taskCheckpoint(
  repo: string,
  taskId: string,
  message: string,
  invokeFn: InvokeFn = invoke,
): Promise<string> {
  const result = await callTask(
    'task.checkpoint',
    { repo, task_id: taskId, message },
    invokeFn,
  );
  const obj = expectObject('task.checkpoint', result);
  if (typeof obj.commit !== 'string') {
    throw new Error('task.checkpoint: result is missing a "commit" string');
  }
  return obj.commit;
}

export async function taskDiff(
  repo: string,
  taskId: string,
  path?: string,
  invokeFn: InvokeFn = invoke,
): Promise<CandidateDiff> {
  const params: Record<string, unknown> = { repo, task_id: taskId };
  if (path !== undefined) {
    params.path = path;
  }
  const result = await callTask('task.diff', params, invokeFn);
  const obj = expectObject('task.diff', result);
  if (!Array.isArray(obj.files)) {
    throw new Error('task.diff: result is missing a "files" array');
  }
  return result as CandidateDiff;
}

export async function taskApprove(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<string> {
  const result = await callTask('task.approve', { repo, task_id: taskId }, invokeFn);
  const obj = expectObject('task.approve', result);
  if (typeof obj.approved !== 'string') {
    throw new Error('task.approve: result is missing an "approved" string');
  }
  return obj.approved;
}

export async function taskMerge(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<MergeOutcome> {
  return callTask('task.merge', { repo, task_id: taskId }, invokeFn);
}

export async function taskRebase(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<TaskRebased> {
  const result = await callTask('task.rebase', { repo, task_id: taskId }, invokeFn);
  const obj = expectObject('task.rebase', result);
  if (typeof obj.new_tip !== 'string') {
    throw new Error('task.rebase: result is missing a "new_tip" string');
  }
  return result as TaskRebased;
}

export async function taskReset(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<void> {
  await rpcCall('task.reset', { repo, task_id: taskId }, invokeFn);
}

export async function taskDiscard(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<void> {
  await rpcCall('task.discard', { repo, task_id: taskId }, invokeFn);
}

/** Start the gates (default: both presets) — async; progress arrives as
 * `task.gate` events and the record lands via `task.gates`. */
export async function taskRunGates(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<GatesStarted> {
  const result = await callTask('task.run_gates', { repo, task_id: taskId }, invokeFn);
  const obj = expectObject('task.run_gates', result);
  if (!Array.isArray(obj.started) || typeof obj.candidate !== 'string') {
    throw new Error('task.run_gates: result is missing "started"/"candidate"');
  }
  return result as GatesStarted;
}

/** The evidence view for a task (fresh / stale / none + running). */
export async function taskGates(
  repo: string,
  taskId: string,
  invokeFn: InvokeFn = invoke,
): Promise<GatesInfo> {
  return callTask('task.gates', { repo, task_id: taskId }, invokeFn);
}

/** Subscribe to `task.gate` notifications (see `termApi.ts` for the
 * listen seam). */
export function subscribeTaskGate(
  handler: (event: TaskGateEvent) => void,
  listenFn: TermListenFn = listen,
): Promise<UnlistenFn> {
  return listenFn<TaskGateEvent>('task.gate', (event) => handler(event.payload));
}

/** Match a `task.gate` payload against a repo+task pair — the listen-side
 * filter the panel needs (`task.gate` is broadcast; `SessionEventGate`
 * would over-fit: it keys on a single session id adopted after spawn, this
 * is a plain two-field match). Events carry the repo string exactly as
 * the rpc caller passed it; a payload without `repo` matches any repo
 * (defensive — the backend always sets it). */
export function taskGateMatches(event: TaskGateEvent, repo: string, taskId: string): boolean {
  return event.task_id === taskId && (event.repo === undefined || event.repo === repo);
}
