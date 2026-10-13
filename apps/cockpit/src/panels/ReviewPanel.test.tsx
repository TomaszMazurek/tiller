import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcClientError } from '../lib/rpcClient';
import { ReviewPanel, type ReviewDeps } from './ReviewPanel';
import type { CandidateDiff, MergeOutcome, TaskStatus, TaskSummary } from '../lib/taskApi';

function task(id: string): TaskSummary {
  return { task_id: id, target_branch: 'master', base: 'aaaaaaa0', tip: '12345670' };
}

function candidateStatus(approval: TaskStatus['approval_state'] = null): TaskStatus {
  return {
    status: 'candidate',
    candidate: {
      task_id: 'fix-1',
      candidate_commit: '1234567890abcdef',
      base_commit: 'abcdef1234567890',
      created_at: 0,
    },
    approval_state: approval,
    checkpoints_count: 2,
  };
}

function diffFixture(): CandidateDiff {
  return {
    files: [
      {
        path: 'a.txt',
        change_type: 'modified',
        patch: '@@ -1 +1 @@\n-old line\n+new line\n context',
        binary: false,
        truncated: false,
      },
      {
        path: 'blob.bin',
        change_type: 'added',
        patch: '',
        binary: true,
        truncated: false,
      },
    ],
    truncated: false,
    total_bytes: 40,
    byte_cap: 262144,
  };
}

/** A deps bundle where every method is a spy with per-test overrides. */
function fakeDeps(overrides: Partial<ReviewDeps> = {}) {
  const deps: ReviewDeps = {
    list: vi.fn(async () => [task('fix-1'), task('fix-2')]),
    create: vi.fn(async () => ({ task_id: 'new', worktree_path: 'C:/w', target_branch: 'master' })),
    status: vi.fn(async (_repo: string, taskId: string): Promise<TaskStatus> =>
      taskId === 'fix-1'
        ? candidateStatus()
        : { ...candidateStatus(), status: 'no_checkpoints', candidate: null, checkpoints_count: 0 },
    ),
    diff: vi.fn(async () => diffFixture()),
    checkpoint: vi.fn(async () => 'c0ffee'),
    approve: vi.fn(async () => '1234567890abcdef'),
    merge: vi.fn(async () => ({ merged: { merge_commit: 'deadbee', main_worktree_updated: true } })),
    rebase: vi.fn(async () => ({ ok: true, new_tip: '999aaa' })),
    reset: vi.fn(async () => undefined),
    discard: vi.fn(async () => undefined),
    ...overrides,
  };
  return deps;
}

// dockview hands panels its props; the review panel uses none of them.
const panelProps = {} as Parameters<typeof ReviewPanel>[0];

function loadRepo(deps: ReviewDeps, repo = 'C:/repo') {
  render(<ReviewPanel {...panelProps} deps={deps} />);
  fireEvent.change(screen.getByTestId('review-repo'), { target: { value: repo } });
  fireEvent.click(screen.getByTestId('review-load'));
}

describe('ReviewPanel', () => {
  // No `globals: true` in the vitest config: RTL auto-cleanup is off.
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it('shows the hint before any repo is loaded', () => {
    render(<ReviewPanel {...panelProps} deps={fakeDeps()} />);
    expect(screen.getByTestId('review-hint').textContent).toContain('Load');
  });

  it('loads the queue with badges, candidate line, file list and colored patch', async () => {
    const deps = fakeDeps();
    loadRepo(deps);

    expect(await screen.findByText('fix-1')).toBeDefined();
    expect(deps.list).toHaveBeenCalledWith('C:/repo');
    expect(deps.status).toHaveBeenCalledWith('C:/repo', 'fix-1');
    expect(deps.status).toHaveBeenCalledWith('C:/repo', 'fix-2');
    // badges: fix-1 has a candidate but no approval; fix-2 has none yet
    const badges = screen.getAllByTestId('review-task').map((el) => el.textContent);
    expect(badges[0]).toContain('no-approval');
    expect(badges[1]).toContain('no-checkpoints');
    // candidate line: short oids, checkpoints count, branch
    expect(screen.getByTestId('review-candidate').textContent).toBe('1234567 ← abcdef1');
    expect(screen.getByTestId('review-approval-badge').textContent).toBe('no-approval');
    expect(screen.getByText('2 checkpoints')).toBeDefined();
    expect(screen.getByText(/master · 1234567/)).toBeDefined();
    // diff loads asynchronously after the queue — wait for it, then assert
    // the file list with change-type marker, binary flag; patch colored
    const files = (await screen.findAllByTestId('review-file')).map((el) => el.textContent);
    expect(files[0]).toContain('Ma.txt');
    expect(files[1]).toContain('Ablob.bin');
    expect(files[1]).toContain('bin');
    const patch = screen.getByTestId('review-patch');
    expect(patch.textContent).toContain('+new line');
    expect(patch.querySelector('.review-diff__line--add')?.textContent).toBe('+new line');
    expect(patch.querySelector('.review-diff__line--del')?.textContent).toBe('-old line');
    expect(patch.querySelector('.review-diff__line--ctx')?.textContent).toContain('context');
  });

  it('switches the patch when another file is selected', async () => {
    const deps = fakeDeps();
    loadRepo(deps);
    const files = await screen.findAllByTestId('review-file');
    fireEvent.click(files[1]);
    expect(screen.getByTestId('review-patch').textContent).toContain('binary file');
  });

  it('loads the selected task diff on selection change', async () => {
    const deps = fakeDeps({
      // both tasks carry a candidate here: a no-checkpoints task must NOT
      // trigger a diff rpc at all (see the dedicated MAJOR-1 test below)
      status: vi.fn(async () => candidateStatus()),
    });
    loadRepo(deps);
    const tasks = await screen.findAllByTestId('review-task');
    fireEvent.click(tasks[1]);
    // selection fix-2 → diff rpc with that task id (auto after click)
    await waitFor(() => {
      expect(deps.diff).toHaveBeenCalledWith('C:/repo', 'fix-2');
    });
  });

  it('no-checkpoints task: no diff rpc, no error, empty diff view', async () => {
    // P6 review MAJOR-1: `task.diff` on a task without checkpoints only
    // answers `no_checkpoints` — a false error the toolbar would show.
    const deps = fakeDeps();
    loadRepo(deps);
    expect(await screen.findByTestId('review-patch')).toBeDefined(); // fix-1 diff loaded
    fireEvent.click(screen.getAllByTestId('review-task')[1]); // fix-2: no checkpoints
    await waitFor(() => {
      expect(screen.getByTestId('review-approval-badge').textContent).toBe('no-checkpoints');
    });
    expect(screen.queryByTestId('review-patch')).toBeNull();
    expect(screen.queryByTestId('review-file')).toBeNull();
    expect(screen.queryByTestId('review-error')).toBeNull();
    expect(deps.diff).not.toHaveBeenCalledWith('C:/repo', 'fix-2');
  });

  it('reset back to no-checkpoints clears the diff without a fetch or an error', async () => {
    let resetDone = false;
    const deps = fakeDeps({
      status: vi.fn(
        async (_repo: string, taskId: string): Promise<TaskStatus> =>
          taskId === 'fix-1' && !resetDone
            ? candidateStatus()
            : {
                ...candidateStatus(),
                status: 'no_checkpoints',
                candidate: null,
                approval_state: null,
                checkpoints_count: 0,
              },
      ),
      reset: vi.fn(async () => {
        resetDone = true;
      }),
    });
    loadRepo(deps);
    expect(await screen.findByTestId('review-patch')).toBeDefined(); // fix-1 diff loaded
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    fireEvent.click(screen.getByTestId('review-reset'));
    await waitFor(() => {
      expect(deps.reset).toHaveBeenCalledWith('C:/repo', 'fix-1');
    });
    // the refreshed status says no_checkpoints → the old patch must be
    // gone, without a second diff rpc and without an error
    await waitFor(() => {
      expect(screen.getByTestId('review-approval-badge').textContent).toBe('no-checkpoints');
    });
    await waitFor(() => {
      expect(screen.queryByTestId('review-patch')).toBeNull();
    });
    expect(deps.diff).toHaveBeenCalledTimes(1);
    expect(screen.queryByTestId('review-error')).toBeNull();
  });

  it('a failed diff fetch shows the error and never leaves stale patch content', async () => {
    let calls = 0;
    const deps = fakeDeps({
      diff: vi.fn(async () => {
        calls += 1;
        if (calls === 1) {
          return diffFixture();
        }
        throw new Error('diff rpc exploded');
      }),
    });
    loadRepo(deps);
    expect(await screen.findByTestId('review-patch')).toBeDefined();
    fireEvent.click(screen.getByTestId('review-refresh'));
    await waitFor(() => {
      expect(screen.getByTestId('review-error').textContent).toContain('diff rpc exploded');
    });
    expect(screen.queryByTestId('review-patch')).toBeNull();
  });

  it('truncation note is honest about the missing per-file filter', async () => {
    // P6 review MINOR: the panel has no path filter yet — the note must
    // not promise one.
    const deps = fakeDeps({
      diff: vi.fn(async () => ({ ...diffFixture(), truncated: true })),
    });
    loadRepo(deps);
    const note = await screen.findByTestId('review-diff-note');
    expect(note.textContent).toContain('truncated');
    expect(note.textContent).not.toContain('path filter');
    expect(note.textContent).toContain('per-file filtering');
  });

  it('checkpoints with the typed message and auto-refreshes', async () => {
    const deps = fakeDeps();
    loadRepo(deps);
    await screen.findByTestId('review-checkpoint');
    fireEvent.change(screen.getByTestId('review-checkpoint-message'), {
      target: { value: 'wip' },
    });
    fireEvent.click(screen.getByTestId('review-checkpoint'));
    await waitFor(() => {
      expect(deps.checkpoint).toHaveBeenCalledWith('C:/repo', 'fix-1', 'wip');
    });
    // auto-refresh: list called again after the action
    await waitFor(() => {
      expect(deps.list).toHaveBeenCalledTimes(2);
    });
    // the message input cleared
    expect((screen.getByTestId('review-checkpoint-message') as HTMLInputElement).value).toBe('');
  });

  it('approve & merge shows the merge note and Cleanup discards', async () => {
    const deps = fakeDeps();
    loadRepo(deps);
    await screen.findByTestId('review-merge');
    fireEvent.click(screen.getByTestId('review-merge'));
    expect(await screen.findByTestId('review-merge-note')).toBeDefined();
    expect(screen.getByTestId('review-merge-note').textContent).toContain('deadbee');
    expect(screen.getByTestId('review-cleanup')).toBeDefined();

    vi.spyOn(window, 'confirm').mockReturnValue(true);
    fireEvent.click(screen.getByTestId('review-cleanup'));
    await waitFor(() => {
      expect(deps.discard).toHaveBeenCalledWith('C:/repo', 'fix-1');
    });
    // cleanup done → the merge note (and with it Cleanup) disappears
    await waitFor(() => {
      expect(screen.queryByTestId('review-cleanup')).toBeNull();
    });
  });

  it('Cleanup stays bound to the merged task — never the current selection', async () => {
    // P6 review BLOCKER: merge A → select B → Cleanup must disappear;
    // selecting A back must offer Cleanup that discards A, not B.
    const deps = fakeDeps();
    loadRepo(deps);
    await screen.findByTestId('review-merge');
    fireEvent.click(screen.getByTestId('review-merge')); // merges fix-1 (selected)
    expect(await screen.findByTestId('review-cleanup')).toBeDefined();
    await waitFor(() => {
      expect((screen.getAllByTestId('review-task')[1] as HTMLButtonElement).disabled).toBe(false);
    });

    fireEvent.click(screen.getAllByTestId('review-task')[1]); // select fix-2
    await waitFor(() => {
      expect(screen.queryByTestId('review-cleanup')).toBeNull();
    });
    expect(screen.queryByTestId('review-merge-note')).toBeNull();

    fireEvent.click(screen.getAllByTestId('review-task')[0]); // back to fix-1
    expect(await screen.findByTestId('review-cleanup')).toBeDefined();
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    fireEvent.click(screen.getByTestId('review-cleanup'));
    await waitFor(() => {
      expect(deps.discard).toHaveBeenCalledWith('C:/repo', 'fix-1');
    });
  });

  it('needs_rebase outcome surfaces the new base and keeps Rebase available', async () => {
    const outcome: MergeOutcome = { needs_rebase: { new_base: 'feedface1234567890' } };
    const deps = fakeDeps({ merge: vi.fn(async () => outcome) });
    loadRepo(deps);
    await screen.findByTestId('review-merge');
    fireEvent.click(screen.getByTestId('review-merge'));
    expect(await screen.findByTestId('review-merge-note')).toBeDefined();
    expect(screen.getByTestId('review-merge-note').textContent).toContain('feedfac');
    expect(screen.queryByTestId('review-cleanup')).toBeNull();
  });

  it('rebase action calls the rpc and refreshes', async () => {
    const deps = fakeDeps();
    loadRepo(deps);
    await screen.findByTestId('review-rebase');
    fireEvent.click(screen.getByTestId('review-rebase'));
    await waitFor(() => {
      expect(deps.rebase).toHaveBeenCalledWith('C:/repo', 'fix-1');
      expect(deps.list).toHaveBeenCalledTimes(2);
    });
  });

  it('reset and discard ask for confirmation', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false);
    const deps = fakeDeps();
    loadRepo(deps);
    await screen.findByTestId('review-reset');
    fireEvent.click(screen.getByTestId('review-reset'));
    fireEvent.click(screen.getByTestId('review-discard'));
    expect(confirmSpy).toHaveBeenCalledTimes(2);
    expect(deps.reset).not.toHaveBeenCalled();
    expect(deps.discard).not.toHaveBeenCalled();
  });

  it('creates a task with the explicit id', async () => {
    const deps = fakeDeps();
    loadRepo(deps);
    await screen.findByTestId('review-create');
    fireEvent.change(screen.getByTestId('review-new-task'), { target: { value: 'fix-3' } });
    fireEvent.click(screen.getByTestId('review-create'));
    await waitFor(() => {
      expect(deps.create).toHaveBeenCalledWith('C:/repo', 'fix-3');
    });
  });

  it('shows rpc failures as the toolbar error', async () => {
    const deps = fakeDeps({ list: vi.fn(async () => { throw new Error('repo not found'); }) });
    loadRepo(deps);
    expect(await screen.findByTestId('review-error')).toBeDefined();
    expect(screen.getByTestId('review-error').textContent).toContain('repo not found');
  });

  it('appends the stable string code from RpcClientError data', async () => {
    const error = new RpcClientError({
      code: -32014,
      message: 'task: unknown task `ghost`',
      data: { code: 'unknown_task' },
    });
    const deps = fakeDeps({ list: vi.fn(async () => { throw error; }) });
    loadRepo(deps);
    expect(await screen.findByTestId('review-error')).toBeDefined();
    expect(screen.getByTestId('review-error').textContent).toContain('[unknown_task]');
  });
});
