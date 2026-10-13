import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcClientError } from '../lib/rpcClient';
import { ReviewPanel, type ReviewDeps } from './ReviewPanel';
import type { CandidateDiff, MergeOutcome, TaskStatus, TaskSummary } from '../lib/taskApi';
import type { UnlistenFn } from '@tauri-apps/api/event';

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
    gates: vi.fn(async () => ({})),
    runGates: vi.fn(async () => ({ started: ['cargo-check', 'cargo-test'], candidate: '1234567890abcdef' })),
    listenGate: vi.fn(async () => (() => undefined) as UnlistenFn),
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

// ------------------------------------------------------------- gates (P7)

import type { GatesInfo, GatesStarted, TaskGateEvent } from '../lib/taskApi';

function gatesEvidence(
  status: 'passed' | 'failed' | 'timeout',
  logTail = '',
  dirtyAtEnd = false,
): GatesInfo {
  return {
    running: false,
    candidate: '1234567890abcdef',
    evidence: {
      gates: [
        { id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 1200 },
        { id: 'cargo-test', label: 'cargo test', status, exit_code: 101, duration_ms: 3400, log_tail: logTail },
      ],
      dirty_at_start: false,
      ...(dirtyAtEnd ? { dirty_at_end: true, dirty_paths: ['a.txt'] } : {}),
      finished_at: 42,
    },
  };
}

/** Deps whose listenGate captures the handler so tests can push events. */
function gateDeps(
  overrides: Partial<ReviewDeps> = {},
  gatesAnswer: () => Promise<GatesInfo> = async () => ({}),
) {
  let handler: ((event: TaskGateEvent) => void) | null = null;
  const deps = fakeDeps({
    gates: vi.fn(gatesAnswer),
    runGates: vi.fn(async (): Promise<GatesStarted> => ({
      started: ['cargo-check', 'cargo-test'],
      candidate: '1234567890abcdef',
    })),
    listenGate: vi.fn(async (h: (event: TaskGateEvent) => void) => {
      handler = h;
      return (() => undefined) as UnlistenFn;
    }),
    ...overrides,
  });
  return {
    deps,
    emit: (event: TaskGateEvent) => handler!(event),
  };
}

describe('ReviewPanel gates', () => {
  afterEach(() => cleanup());

  it('Run Gates → live badges from task.gate events → settled badges', async () => {
    const { deps, emit } = gateDeps();
    loadRepo(deps);
    const button = await screen.findByTestId('review-run-gates');
    fireEvent.click(button);
    await waitFor(() => {
      expect(deps.runGates).toHaveBeenCalledWith('C:/repo', 'fix-1');
    });
    // started: both entries running
    expect((await screen.findAllByTestId('review-gate-badge')).map((el) => el.textContent)).toEqual([
      'running',
      'running',
    ]);
    // foreign task events are filtered out by the listener helper
    emit({ task_id: 'fix-2', candidate: 'x', id: 'cargo-check', label: 'cargo check', status: 'passed' });
    emit({ repo: 'other', task_id: 'fix-1', candidate: 'x', id: 'cargo-check', label: 'cargo check', status: 'passed' });
    expect(screen.getAllByTestId('review-gate-badge').every((el) => el.textContent === 'running')).toBe(true);

    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 1200 });
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-test', label: 'cargo test', status: 'failed', duration_ms: 3400 });
    await waitFor(() => {
      expect(screen.getAllByTestId('review-gate-badge').map((el) => el.textContent)).toEqual(['passed', 'failed']);
    });
    expect(screen.getByText('1.2s')).toBeDefined();
    expect(screen.getByText('3.4s')).toBeDefined();
  });

  it('evidence log tails render in an expandable details block', async () => {
    const { deps } = gateDeps({}, async () => gatesEvidence('failed', 'error[E0308]: mismatched types'));
    loadRepo(deps);
    const details = await screen.findByTestId('review-gate-log');
    expect(details.textContent).toContain('log tail');
    expect(details.querySelector('pre')?.textContent).toContain('error[E0308]');
  });

  it('a failed gate warns at merge: confirm lists the problem, decline stops the merge', async () => {
    const { deps } = gateDeps({}, async () => gatesEvidence('failed'));
    loadRepo(deps);
    await screen.findByTestId('review-gate-warn');
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false);
    fireEvent.click(await screen.findByTestId('review-merge'));
    expect(confirmSpy).toHaveBeenCalledTimes(1);
    expect(confirmSpy.mock.calls[0][0]).toContain('gate cargo test failed');
    expect(deps.approve).not.toHaveBeenCalled();

    confirmSpy.mockReturnValue(true);
    fireEvent.click(screen.getByTestId('review-merge'));
    await waitFor(() => {
      expect(deps.approve).toHaveBeenCalledWith('C:/repo', 'fix-1');
    });
  });

  it('all-passed gates merge without any confirm', async () => {
    const { deps } = gateDeps({}, async () => gatesEvidence('passed'));
    loadRepo(deps);
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false);
    await screen.findByTestId('review-gates');
    fireEvent.click(screen.getByTestId('review-merge'));
    await waitFor(() => {
      expect(deps.approve).toHaveBeenCalledWith('C:/repo', 'fix-1');
    });
    expect(confirmSpy).not.toHaveBeenCalled();
  });

  it('stale evidence shows the STALE marker and warns at merge', async () => {
    // evidence bound to an older candidate → stale by candidate comparison
    const staleInfo = async (): Promise<GatesInfo> => ({
      running: false,
      candidate: 'aaaabbbbccccdddd',
      evidence: gatesEvidence('passed').evidence,
    });
    const { deps } = gateDeps({}, staleInfo);
    loadRepo(deps);
    expect(await screen.findByTestId('review-gate-stale')).toBeDefined();
    expect(screen.getByTestId('review-gate-stale').textContent).toContain('re-run');
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false);
    fireEvent.click(screen.getByTestId('review-merge'));
    expect(confirmSpy.mock.calls[0][0]).toContain('stale');
    expect(deps.approve).not.toHaveBeenCalled();
  });

  it('no evidence at all → subtle hint, never a warn; Run Gates disabled without checkpoints', async () => {
    const { deps } = gateDeps();
    loadRepo(deps);
    expect(await screen.findByTestId('review-gate-hint')).toBeDefined();
    expect(screen.queryByTestId('review-gate-warn')).toBeNull();
    // a no-checkpoints task disables Run Gates
    fireEvent.click(screen.getAllByTestId('review-task')[1]);
    await waitFor(() => {
      expect((screen.getByTestId('review-run-gates') as HTMLButtonElement).disabled).toBe(true);
    });
  });

  it('after the last event the recorded evidence is fetched (retrying while the backend still says running)', async () => {
    // the queue load's own fetch first, then the post-run attempts:
    // #1 lands before the record is written (running), #2 delivers it
    const gates = vi
      .fn()
      .mockImplementationOnce(async () => ({}) as GatesInfo)
      .mockImplementationOnce(async () => ({ running: true }) as GatesInfo)
      .mockImplementationOnce(async () => gatesEvidence('failed', 'error[E0308]: boom'));
    const { deps, emit } = gateDeps({ gates });
    loadRepo(deps);
    fireEvent.click(await screen.findByTestId('review-run-gates'));
    // the whole run arrives via events (running:false in state after the
    // last one — the fetch must not depend on it, P7 review MAJOR-2)
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-check', label: 'cargo check', status: 'running' });
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 1200 });
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-test', label: 'cargo test', status: 'running' });
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-test', label: 'cargo test', status: 'failed', duration_ms: 3400 });
    // the record's log tail renders — proof the evidence landed
    const log = await screen.findByTestId('review-gate-log', {}, { timeout: 4000 });
    expect(log.querySelector('pre')?.textContent).toContain('error[E0308]');
    expect(screen.getAllByTestId('review-gate-badge').map((el) => el.textContent)).toEqual(['passed', 'failed']);
    expect(gates.mock.calls.length).toBeGreaterThanOrEqual(3);
  });

  it('retry exhaustion keeps the event-derived entries and surfaces the lag (P7 review MAJOR-2)', async () => {
    // the record is never written while the backend keeps saying running
    const gates = vi.fn().mockImplementation(async () => ({ running: true }) as GatesInfo);
    const { deps, emit } = gateDeps({ gates });
    loadRepo(deps);
    fireEvent.click(await screen.findByTestId('review-run-gates'));
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-check', label: 'cargo check', status: 'running' });
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 900 });
    // attempts: #1 (t≈250ms), #2 (≈500), #3 (≈750) — all `running:true`;
    // the view must NOT be wiped by an empty record and the lag shows
    const lag = await screen.findByTestId('review-gate-lag', {}, { timeout: 4000 });
    expect(lag.textContent).toContain('evidence still recording');
    expect(screen.getAllByTestId('review-gate-badge').map((el) => el.textContent)).toEqual(['passed']);
    expect(gates.mock.calls.length).toBeGreaterThanOrEqual(3);
  });

  it('a run that beats its rpc answer stays finished — the late started answer is dropped', async () => {
    // gates always answer with the finished record: whenever the
    // post-run fetch happens, the view is already settled
    const { deps, emit } = gateDeps({}, async () => gatesEvidence('failed', 'boom-tail'));
    let release!: (value: GatesStarted) => void;
    const started = new Promise<GatesStarted>((resolve) => {
      release = resolve;
    });
    (deps.runGates as ReturnType<typeof vi.fn>).mockImplementation(() => started);
    loadRepo(deps);
    fireEvent.click(await screen.findByTestId('review-run-gates'));
    // every event lands before the rpc answer resolves
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-check', label: 'cargo check', status: 'passed', duration_ms: 1200 });
    emit({ repo: 'C:/repo', task_id: 'fix-1', candidate: '1234567890abcdef', id: 'cargo-test', label: 'cargo test', status: 'failed', duration_ms: 3400 });
    await waitFor(() => {
      expect(screen.getAllByTestId('review-gate-badge').map((el) => el.textContent)).toEqual(['passed', 'failed']);
    });
    // now the started answer arrives late — it must not resurrect running
    release({ started: ['cargo-check', 'cargo-test'], candidate: '1234567890abcdef' });
    await waitFor(() => {
      expect(deps.runGates).toHaveBeenCalled();
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(screen.getAllByTestId('review-gate-badge').map((el) => el.textContent)).toEqual(['passed', 'failed']);
    expect((screen.getByTestId('review-run-gates') as HTMLButtonElement).disabled).toBe(false);
  });

  it('dirty_at_end evidence shows the annotation and warns at merge (A.2)', async () => {
    const { deps } = gateDeps({}, async () => gatesEvidence('passed', '', true));
    loadRepo(deps);
    expect(await screen.findByTestId('review-gate-dirty')).toBeDefined();
    expect(screen.getByTestId('review-gate-dirty').textContent).toContain('changed during gates');
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false);
    fireEvent.click(screen.getByTestId('review-merge'));
    expect(confirmSpy.mock.calls[0][0]).toContain('changed during gates');
    expect(deps.approve).not.toHaveBeenCalled();
  });
});
