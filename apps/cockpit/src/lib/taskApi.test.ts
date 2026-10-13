import { describe, expect, it, vi } from 'vitest';
import {
  subscribeTaskGate,
  taskApprove,
  taskCheckpoint,
  taskCreate,
  taskDiff,
  taskDiscard,
  taskGates,
  taskList,
  taskMerge,
  taskRebase,
  taskReset,
  taskRunGates,
  taskStatus,
  taskGateMatches,
} from './taskApi';
import type { RpcResponse } from './rpcClient';

/** invoke mock answering a serialized RpcResponse per method. */
function invokeAnswering(results: Record<string, unknown>) {
  const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
  const invokeFn = vi.fn(async (command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args });
    const request = JSON.parse(String(args?.json ?? '{}')) as { method?: string };
    const response: RpcResponse = { id: 1, result: results[request.method ?? ''] ?? {} };
    return JSON.stringify(response);
  });
  return { invokeFn, calls };
}

function sentParams(calls: Array<{ command: string; args?: Record<string, unknown> }>) {
  return calls.map((call) => JSON.parse(String(call.args?.json)).params);
}

describe('taskList', () => {
  it('sends {repo} and unwraps the array (wire shape mirrors TaskSummary)', async () => {
    const entry = {
      task_id: 'fix-1',
      target_branch: 'master',
      worktree_path: 'C:/repo/.tiller/worktrees/fix-1',
      base: 'b',
      tip: 't',
    };
    const { invokeFn, calls } = invokeAnswering({ 'task.list': [entry] });
    const tasks = await taskList('C:/repo', invokeFn);
    expect(tasks).toEqual([entry]);
    expect(sentParams(calls)).toEqual([{ repo: 'C:/repo' }]);
  });

  it('rejects a non-array result', async () => {
    const { invokeFn } = invokeAnswering({ 'task.list': { tasks: [] } });
    await expect(taskList('r', invokeFn)).rejects.toThrow('task.list');
  });
});

describe('taskCreate / taskStatus / taskCheckpoint / taskApprove', () => {
  it('send snake_case params and validate the result shapes', async () => {
    const { invokeFn, calls } = invokeAnswering({
      'task.create': { task_id: 'fix-1', worktree_path: 'C:/repo/.tiller/worktrees/fix-1', target_branch: 'master' },
      'task.status': {
        status: 'candidate',
        candidate: { task_id: 'fix-1', candidate_commit: 'c', base_commit: 'b', created_at: 1 },
        approval_state: null,
        checkpoints_count: 1,
      },
      'task.checkpoint': { commit: 'abc123' },
      'task.approve': { approved: 'abc123' },
    });
    await taskCreate('C:/repo', 'fix-1', invokeFn);
    await taskStatus('C:/repo', 'fix-1', invokeFn);
    expect(await taskCheckpoint('C:/repo', 'fix-1', 'wip', invokeFn)).toBe('abc123');
    expect(await taskApprove('C:/repo', 'fix-1', invokeFn)).toBe('abc123');
    expect(sentParams(calls)).toEqual([
      { repo: 'C:/repo', task_id: 'fix-1' },
      { repo: 'C:/repo', task_id: 'fix-1' },
      { repo: 'C:/repo', task_id: 'fix-1', message: 'wip' },
      { repo: 'C:/repo', task_id: 'fix-1' },
    ]);
  });

  it('rejects results with missing keys', async () => {
    const missing: Array<[() => Promise<unknown>, string]> = [
      [() => taskCreate('r', 't', invokeAnswering({ 'task.create': {} }).invokeFn), 'task_id'],
      [() => taskStatus('r', 't', invokeAnswering({ 'task.status': {} }).invokeFn), 'status'],
      [
        () => taskCheckpoint('r', 't', 'm', invokeAnswering({ 'task.checkpoint': {} }).invokeFn),
        'commit',
      ],
      [() => taskApprove('r', 't', invokeAnswering({ 'task.approve': {} }).invokeFn), 'approved'],
    ];
    for (const [call, part] of missing) {
      await expect(call()).rejects.toThrow(part);
    }
  });
});

describe('taskDiff', () => {
  const diff = {
    files: [{ path: 'a.txt', change_type: 'modified', patch: '+x', binary: false, truncated: false }],
    truncated: false,
    total_bytes: 2,
    byte_cap: 262144,
  };

  it('omits path when not given, sends it when given', async () => {
    const { invokeFn, calls } = invokeAnswering({ 'task.diff': diff });
    await taskDiff('r', 't', undefined, invokeFn);
    await taskDiff('r', 't', 'a.txt', invokeFn);
    expect(sentParams(calls)).toEqual([
      { repo: 'r', task_id: 't' },
      { repo: 'r', task_id: 't', path: 'a.txt' },
    ]);
  });

  it('rejects a result without files', async () => {
    const { invokeFn } = invokeAnswering({ 'task.diff': {} });
    await expect(taskDiff('r', 't', undefined, invokeFn)).rejects.toThrow('files');
  });
});

describe('taskMerge / taskRebase / taskReset / taskDiscard', () => {
  it('pass MergeOutcome through and validate rebase shape', async () => {
    const { invokeFn } = invokeAnswering({
      'task.merge': { merged: { merge_commit: 'm1', main_worktree_updated: true } },
      'task.rebase': { ok: true, new_tip: 'n1' },
      'task.reset': {},
      'task.discard': {},
    });
    expect(await taskMerge('r', 't', invokeFn)).toEqual({
      merged: { merge_commit: 'm1', main_worktree_updated: true },
    });
    expect(await taskRebase('r', 't', invokeFn)).toEqual({ ok: true, new_tip: 'n1' });
    await expect(taskReset('r', 't', invokeFn)).resolves.toBeUndefined();
    await expect(taskDiscard('r', 't', invokeFn)).resolves.toBeUndefined();

    const bad = invokeAnswering({ 'task.rebase': { ok: true } }).invokeFn;
    await expect(taskRebase('r', 't', bad)).rejects.toThrow('new_tip');
  });
});

describe('taskRunGates / taskGates / task.gate events', () => {
  it('run_gates sends repo+task and validates the started shape', async () => {
    const { invokeFn, calls } = invokeAnswering({
      'task.run_gates': { started: ['cargo-check', 'cargo-test'], candidate: 'c1' },
    });
    const started = await taskRunGates('r', 't', invokeFn);
    expect(started).toEqual({ started: ['cargo-check', 'cargo-test'], candidate: 'c1' });
    expect(sentParams(calls)).toEqual([{ repo: 'r', task_id: 't' }]);

    const bad = invokeAnswering({ 'task.run_gates': { candidate: 'c1' } }).invokeFn;
    await expect(taskRunGates('r', 't', bad)).rejects.toThrow('started');
  });

  it('gates passes the evidence view through', async () => {
    const info = {
      running: false,
      candidate: 'c1',
      evidence: { gates: [{ id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 3 }], dirty_at_start: false, finished_at: 9 },
    };
    const { invokeFn } = invokeAnswering({ 'task.gates': info });
    expect(await taskGates('r', 't', invokeFn)).toEqual(info);
  });

  it('subscribeTaskGate listens on task.gate and taskGateMatches filters by repo+task', async () => {
    const seen: unknown[] = [];
    const listenFn = vi.fn(async (event: string, handler: (e: { payload: unknown }) => void) => {
      expect(event).toBe('task.gate');
      handler({ payload: { task_id: 't1', candidate: 'c1', id: 'cargo-check', label: 'cargo check', status: 'running' } });
      return () => undefined;
    });
    const unlisten = await subscribeTaskGate((event) => seen.push(event), listenFn as never);
    expect(unlisten).toBeTypeOf('function');
    expect(seen).toHaveLength(1);

    const base = { repo: 'r', task_id: 't1', candidate: 'c1', id: 'g', label: 'g', status: 'passed' as const };
    expect(taskGateMatches(base, 'r', 't1')).toBe(true);
    expect(taskGateMatches({ ...base, repo: 'other' }, 'r', 't1')).toBe(false);
    expect(taskGateMatches({ ...base, task_id: 't2' }, 'r', 't1')).toBe(false);
    // a payload without repo (defensive) still matches its task
    const { repo: _drop, ...noRepo } = base;
    expect(taskGateMatches(noRepo, 'r', 't1')).toBe(true);
  });
});
