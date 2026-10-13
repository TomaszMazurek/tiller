import { useCallback, useEffect, useReducer, useState } from 'react';
import type { IDockviewPanelProps } from 'dockview-react';
import { RpcClientError } from '../lib/rpcClient';
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
} from '../lib/reviewState';
import {
  taskApprove,
  taskCreate,
  taskDiff,
  taskDiscard,
  taskList,
  taskMerge,
  taskRebase,
  taskReset,
  taskStatus,
  taskCheckpoint,
  type CandidateDiff,
  type MergeOutcome,
  type TaskCreated,
  type TaskStatus,
  type TaskSummary,
} from '../lib/taskApi';

/** Injected seams (tests fake the tauri bridge; see taskApi for defaults). */
export interface ReviewDeps {
  list(repo: string): Promise<TaskSummary[]>;
  create(repo: string, taskId: string): Promise<TaskCreated>;
  status(repo: string, taskId: string): Promise<TaskStatus>;
  diff(repo: string, taskId: string): Promise<CandidateDiff>;
  checkpoint(repo: string, taskId: string, message: string): Promise<string>;
  approve(repo: string, taskId: string): Promise<string>;
  merge(repo: string, taskId: string): Promise<MergeOutcome>;
  rebase(repo: string, taskId: string): Promise<{ ok: boolean; new_tip: string }>;
  reset(repo: string, taskId: string): Promise<void>;
  discard(repo: string, taskId: string): Promise<void>;
}

const defaultDeps: ReviewDeps = {
  list: taskList,
  create: taskCreate,
  status: taskStatus,
  diff: (repo, taskId) => taskDiff(repo, taskId),
  checkpoint: taskCheckpoint,
  approve: taskApprove,
  merge: taskMerge,
  rebase: taskRebase,
  reset: taskReset,
  discard: taskDiscard,
};

/** Operator-facing rpc error: message plus the stable string code. */
function messageOf(error: unknown): string {
  if (error instanceof RpcClientError) {
    const code = (error.data as { code?: string } | undefined)?.code;
    return code !== undefined ? `${error.message} [${code}]` : error.message;
  }
  return error instanceof Error ? error.message : String(error);
}

/** dockview hands panels their props; `deps` is the test seam. */
export interface ReviewPanelProps extends IDockviewPanelProps {
  deps?: ReviewDeps;
}

/**
 * Review panel (brief §6): the operator's view of the task queue —
 * candidate diff (base→candidate), approval state and the actions
 * checkpoint / approve & merge / rebase / reset / discard / cleanup.
 * Thin by design: every state transition lives in `reviewState.ts`
 * (reducer), every rpc shape in `taskApi.ts`; this component only
 * orchestrates load → (list + statuses) → diff of the selected task and
 * refreshes the whole view after every action.
 */
export function ReviewPanel({ deps = defaultDeps }: ReviewPanelProps) {
  const [state, dispatch] = useReducer(reviewReducer, initialReviewState);

  const [repoInput, setRepoInput] = useState('');
  const [newTaskId, setNewTaskId] = useState('');
  const [checkpointMessage, setCheckpointMessage] = useState('');

  /** Load the queue: list + per-task statuses. The selected task's diff
   * follows from the `diffEpoch` effect below (single load path). */
  const refresh = useCallback(
    async (repo: string) => {
      dispatch({ type: 'loadStarted' });
      let tasks: TaskSummary[];
      let statuses: Record<string, TaskStatus>;
      try {
        tasks = await deps.list(repo);
        statuses = {};
        await Promise.all(
          tasks.map(async (task) => {
            statuses[task.task_id] = await deps.status(repo, task.task_id);
          }),
        );
      } catch (error) {
        dispatch({ type: 'loadFailed', message: messageOf(error) });
        return;
      }
      dispatch({ type: 'loaded', repo, tasks, statuses });
    },
    [deps],
  );

  /** Single diff load path: selection change or a completed queue load
   * (the epoch) both land here — no second fetcher anywhere else. A task
   * without checkpoints has no candidate to diff: `task.diff` would only
   * answer `no_checkpoints` (a false toolbar error, P6 review MAJOR-1),
   * so the view is cleared instead of fetching. */
  const repo = state.repo;
  const selectedTaskId = state.selectedTaskId;
  const diffEpoch = state.diffEpoch;
  const phase =
    selectedTaskId !== null ? (state.statuses[selectedTaskId]?.status ?? null) : null;
  useEffect(() => {
    if (repo === null || selectedTaskId === null) {
      return;
    }
    if (phase === 'no_checkpoints') {
      dispatch({ type: 'diffCleared' });
      return;
    }
    let cancelled = false;
    deps
      .diff(repo, selectedTaskId)
      .then((diff) => {
        if (!cancelled) {
          dispatch({ type: 'diffLoaded', diff });
        }
      })
      .catch((error) => {
        // a failed fetch must not leave the previous task's patch on
        // screen (P6 review MAJOR-1)
        if (!cancelled) {
          dispatch({ type: 'diffFailed', message: messageOf(error) });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [repo, selectedTaskId, diffEpoch, phase, deps]);

  /** One rpc action: guard → call → auto-refresh the whole view. */
  const runAction = useCallback(
    async (repo: string, fn: () => Promise<unknown>) => {
      dispatch({ type: 'actionStarted' });
      try {
        await fn();
      } catch (error) {
        dispatch({ type: 'actionFailed', message: messageOf(error) });
        return;
      }
      await refresh(repo);
    },
    [refresh],
  );

  const onLoad = () => {
    const repo = repoInput.trim();
    if (repo !== '') {
      void refresh(repo);
    }
  };

  const onCreate = () => {
    const repo = state.repo;
    const taskId = newTaskId.trim();
    if (repo === null || taskId === '') return;
    setNewTaskId('');
    void runAction(repo, () => deps.create(repo, taskId));
  };

  const onCheckpoint = () => {
    const repo = state.repo;
    const taskId = state.selectedTaskId;
    const message = checkpointMessage.trim();
    if (repo === null || taskId === null || message === '') return;
    setCheckpointMessage('');
    void runAction(repo, () => deps.checkpoint(repo, taskId, message));
  };

  const onApproveAndMerge = () => {
    const repo = state.repo;
    const taskId = state.selectedTaskId;
    if (repo === null || taskId === null) return;
    dispatch({ type: 'actionStarted' });
    void (async () => {
      try {
        await deps.approve(repo, taskId);
        const outcome = await deps.merge(repo, taskId);
        dispatch({ type: 'mergeFinished', repo, taskId, outcome });
      } catch (error) {
        dispatch({ type: 'actionFailed', message: messageOf(error) });
        return;
      }
      await refresh(repo);
    })();
  };

  const onRebase = () => {
    const repo = state.repo;
    const taskId = state.selectedTaskId;
    if (repo === null || taskId === null) return;
    void runAction(repo, () => deps.rebase(repo, taskId));
  };

  const onReset = () => {
    const repo = state.repo;
    const taskId = state.selectedTaskId;
    if (repo === null || taskId === null) return;
    if (!confirm(`Reset task ${taskId}: drop ALL checkpoints back to base?`)) return;
    void runAction(repo, () => deps.reset(repo, taskId));
  };

  const onDiscard = () => {
    const repo = state.repo;
    const taskId = state.selectedTaskId;
    if (repo === null || taskId === null) return;
    if (!confirm(`Discard task ${taskId} completely (worktree + branch + metadata)?`)) return;
    void runAction(repo, () => deps.discard(repo, taskId));
  };

  /** Cleanup is only ever offered for the task that was merged — the
   * guard re-checks the binding at click time so it can never discard a
   * task that merely happens to be selected (P6 review BLOCKER). */
  const onCleanup = () => {
    const last = state.lastMerge;
    if (last === null || last.repo !== state.repo || last.task_id !== state.selectedTaskId) {
      return;
    }
    if (!confirm(`Merged — remove task ${last.task_id} (worktree + branch)?`)) return;
    dispatch({ type: 'actionStarted' });
    void (async () => {
      try {
        await deps.discard(last.repo, last.task_id);
        dispatch({ type: 'cleanupDone' });
      } catch (error) {
        dispatch({ type: 'actionFailed', message: messageOf(error) });
        return;
      }
      await refresh(last.repo);
    })();
  };

  const task = selectedTask(state);
  const status = selectedStatus(state);
  const badge = approvalBadge(status);
  const file = selectedFileDiff(state);
  const boundMerge = boundMergeOutcome(state);
  const merged = boundMerge !== null && 'merged' in boundMerge;
  const busy = state.busy;

  return (
    <div className="review-panel">
      <div className="review-panel__toolbar">
        <label className="review-panel__field">
          repo
          <input
            value={repoInput}
            onChange={(event) => setRepoInput(event.target.value)}
            placeholder="repository directory"
            spellCheck={false}
            data-testid="review-repo"
            onKeyDown={(event) => {
              if (event.key === 'Enter') onLoad();
            }}
          />
        </label>
        <button type="button" onClick={onLoad} data-testid="review-load">
          Load
        </button>
        <label className="review-panel__field">
          new task
          <input
            value={newTaskId}
            onChange={(event) => setNewTaskId(event.target.value)}
            placeholder="task id"
            spellCheck={false}
            data-testid="review-new-task"
          />
        </label>
        <button
          type="button"
          onClick={onCreate}
          disabled={busy || state.repo === null || newTaskId.trim() === ''}
          data-testid="review-create"
        >
          Create
        </button>
        <button
          type="button"
          onClick={() => state.repo !== null && void refresh(state.repo)}
          disabled={state.repo === null || busy}
          data-testid="review-refresh"
        >
          Refresh
        </button>
        {state.error !== null && (
          <span className="review-panel__error" data-testid="review-error">
            {state.error}
          </span>
        )}
      </div>

      <div className="review-panel__body">
        <aside className="review-panel__queue">
          {state.repo === null ? (
            <p className="review-panel__hint" data-testid="review-hint">
              podaj ścieżkę repo i wciśnij Load
            </p>
          ) : state.tasks.length === 0 ? (
            <p className="review-panel__hint">brak zadań — utwórz nowe</p>
          ) : (
            <ul data-testid="review-task-list">
              {state.tasks.map((entry) => {
                const entryStatus = state.statuses[entry.task_id] ?? null;
                const entryBadge = approvalBadge(entryStatus);
                return (
                  <li key={entry.task_id}>
                    <button
                      type="button"
                      className={`review-task${entry.task_id === state.selectedTaskId ? ' review-task--selected' : ''}`}
                      onClick={() => dispatch({ type: 'taskSelected', taskId: entry.task_id })}
                      disabled={busy}
                      data-testid="review-task"
                    >
                      <span className={`review-badge review-badge--${entryBadge}`}>
                        {entryBadge}
                      </span>
                      <span className="review-task__id">{entry.task_id}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </aside>

        <section className="review-panel__main">
          {task === null ? (
            <p className="review-panel__hint">wybierz zadanie z kolejki</p>
          ) : (
            <>
              <div className="review-panel__candidate">
                <span className={`review-badge review-badge--${badge}`} data-testid="review-approval-badge">
                  {badge}
                </span>
                {status?.candidate !== null && status?.candidate !== undefined ? (
                  <span data-testid="review-candidate">
                    {shortOid(status.candidate.candidate_commit)} ← {shortOid(status.candidate.base_commit)}
                  </span>
                ) : (
                  <span data-testid="review-candidate">no checkpoints</span>
                )}
                <span>{status?.checkpoints_count ?? 0} checkpoints</span>
                <span className="review-panel__branch">
                  {task.target_branch} · {shortOid(task.tip)}
                </span>
                {boundMerge !== null && (
                  <span className="review-panel__merge-note" data-testid="review-merge-note">
                    {'merged' in boundMerge
                      ? `merged ${shortOid(boundMerge.merged.merge_commit)}`
                      : `needs rebase onto ${shortOid(boundMerge.needs_rebase.new_base)}`}
                  </span>
                )}              </div>

              <div className="review-panel__actions">
                <input
                  value={checkpointMessage}
                  onChange={(event) => setCheckpointMessage(event.target.value)}
                  placeholder="checkpoint message"
                  data-testid="review-checkpoint-message"
                />
                <button
                  type="button"
                  onClick={onCheckpoint}
                  disabled={busy || checkpointMessage.trim() === ''}
                  data-testid="review-checkpoint"
                >
                  Checkpoint
                </button>
                <button type="button" onClick={onApproveAndMerge} disabled={busy} data-testid="review-merge">
                  Approve &amp; Merge
                </button>
                <button type="button" onClick={onRebase} disabled={busy} data-testid="review-rebase">
                  Rebase
                </button>
                <button type="button" onClick={onReset} disabled={busy} data-testid="review-reset">
                  Reset
                </button>
                <button type="button" onClick={onDiscard} disabled={busy} data-testid="review-discard">
                  Discard
                </button>
                {merged && (
                  <button type="button" onClick={onCleanup} disabled={busy} data-testid="review-cleanup">
                    Cleanup
                  </button>
                )}
              </div>

              <div className="review-panel__diff">
                <div className="review-diff__view">
                  <ul className="review-diff__files" data-testid="review-file-list">
                    {state.diff?.files.map((entry) => (
                      <li key={entry.path}>
                        <button
                          type="button"
                          className={`review-file${entry.path === state.selectedFile ? ' review-file--selected' : ''}`}
                          onClick={() => dispatch({ type: 'fileSelected', path: entry.path })}
                          data-testid="review-file"
                        >
                          <span className={`review-file__type review-file__type--${entry.change_type}`}>
                            {changeTypeIcon(entry.change_type)}
                          </span>
                          <span className="review-file__path">{entry.path}</span>
                          {entry.binary && <span className="review-file__flag">bin</span>}
                          {entry.truncated && <span className="review-file__flag">…</span>}
                        </button>
                      </li>
                    ))}
                  </ul>
                  {file === null ? (
                    <p className="review-panel__hint">
                      {state.diff !== null && state.diff.files.length === 0
                        ? 'brak zmian w kandydacie'
                        : 'wybierz plik'}
                    </p>
                  ) : file.binary ? (
                    <pre className="review-diff__patch" data-testid="review-patch">
                      binary file (content not shipped)
                    </pre>
                  ) : (
                    <pre className="review-diff__patch" data-testid="review-patch">
                      {splitPatch(file.patch).map((line, index) => (
                        <div key={index} className={`review-diff__line review-diff__line--${diffLineKind(line)}`}>
                          {line === '' ? ' ' : line}
                        </div>
                      ))}
                    </pre>
                  )}
                </div>
                {state.diff?.truncated === true && (
                  <p className="review-diff__note" data-testid="review-diff-note">
                    diff truncated at {Math.round(state.diff.byte_cap / 1024)} KiB — full candidate diff; per-file filtering lands with the file tree
                  </p>
                )}
              </div>
            </>
          )}
        </section>
      </div>
    </div>
  );
}
