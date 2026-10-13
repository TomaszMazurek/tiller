import { describe, expect, it, vi } from 'vitest';
import {
  taskApprove,
  taskCheckpoint,
  taskCreate,
  taskDiff,
  taskDiscard,
  taskList,
  taskMerge,
  taskRebase,
  taskReset,
  taskStatus,
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
  it('sends {repo} and unwraps the array', async () => {
    const { invokeFn, calls } = invokeAnswering({
      'task.list': [{ task_id: 'fix-1', target_branch: 'master', base: 'b', tip: 't' }],
    });
    const tasks = await taskList('C:/repo', invokeFn);
    expect(tasks).toEqual([{ task_id: 'fix-1', target_branch: 'master', base: 'b', tip: 't' }]);
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
