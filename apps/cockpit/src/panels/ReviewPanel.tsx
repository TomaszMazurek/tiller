import { useCallback, useEffect, useReducer, useRef, useState } from 'react';
import type { IDockviewPanelProps } from 'dockview-react';
import { RpcClientError } from '../lib/rpcClient';
import {
  approvalBadge,
  boundMergeOutcome,
  changeTypeIcon,
  diffLineKind,
  initialReviewState,
  mergeGateProblems,
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
  taskGates,
  taskList,
  taskMerge,
  taskRebase,
  taskReset,
  taskRunGates,
  taskStatus,
  taskCheckpoint,
  taskGateMatches,
  subscribeTaskGate,
  type CandidateDiff,
  type GatesInfo,
  type GatesStarted,
  type MergeOutcome,
  type TaskCreated,
  type TaskGateEvent,
  type TaskStatus,
  type TaskSummary,
} from '../lib/taskApi';
import type { UnlistenFn } from '@tauri-apps/api/event';

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
  gates(repo: string, taskId: string): Promise<GatesInfo>;
  runGates(repo: string, taskId: string): Promise<GatesStarted>;
  listenGate(handler: (event: TaskGateEvent) => void): Promise<UnlistenFn>;
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
  gates: taskGates,
  runGates: taskRunGates,
  listenGate: subscribeTaskGate,
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

  /** The selected task's current candidate (the authority the gates'
   * staleness is derived from). */
  const currentCandidate =
    selectedTaskId !== null
      ? (state.statuses[selectedTaskId]?.candidate?.candidate_commit ?? null)
      : null;

  /** Single gates load path, same triggers as the diff (selection change
   * or a completed queue load). A no-checkpoints task has nothing to
   * gate: clear instead of fetching into a guaranteed error. */
  const gates = state.gates;
  useEffect(() => {
    if (repo === null || selectedTaskId === null) {
      return;
    }
    if (phase === 'no_checkpoints') {
      dispatch({ type: 'gatesLoaded', info: {}, currentCandidate });
      return;
    }
    let cancelled = false;
    deps
      .gates(repo, selectedTaskId)
      .then((info) => {
        if (!cancelled) {
          dispatch({ type: 'gatesLoaded', info, currentCandidate });
        }
      })
      .catch(() => {
        // gates are advisory bookkeeping: a failed fetch never blocks
        // the review — it just leaves the previous view in place.
      });
    return () => {
      cancelled = true;
    };
    // currentCandidate flows through the action, not the fetch key
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repo, selectedTaskId, diffEpoch, phase, deps]);

  /** The run's every entry settled: fetch the recorded evidence — it
   * lands after the last event (the backend flips `running` false once
   * written), so retry briefly while it still says running (bounded,
   * ~3 attempts; on give-up the event-derived entries stay and only the
   * log tails are missing). Guarded by `awaitingEvidence` so a plain
   * evidence load cannot loop on itself (P7 review MAJOR-2). */
  useEffect(() => {
    if (
      !gates.awaitingEvidence ||
      gates.entries.length === 0 ||
      !gates.entries.every((entry) => entry.status !== 'running')
    ) {
      return;
    }
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const attempt = (retriesLeft: number) => {
      if (cancelled || repo === null || selectedTaskId === null) return;
      deps
        .gates(repo, selectedTaskId)
        .then((info) => {
          if (cancelled) return;
          if (info.running === true) {
            if (retriesLeft > 0) {
              timer = setTimeout(() => attempt(retriesLeft - 1), 250);
              return;
            }
            // Retry exhausted while the backend still says `running`:
            // dispatching this answer would wipe the event-derived
            // entries (P7 review MAJOR-2). Keep them and surface the
            // lag explicitly instead.
            dispatch({ type: 'evidencePending' });
            return;
          }
          dispatch({ type: 'gatesLoaded', info, currentCandidate });
        })
        .catch(() => undefined);
    };
    timer = setTimeout(() => attempt(2), 250);
    return () => {
      cancelled = true;
      if (timer !== null) clearTimeout(timer);
    };
    // gates is the loop's own state; refetch only when it changes
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [gates, repo, selectedTaskId, deps]);

  /** `task.gate` events for the selected task (repo+task filter; refs so
   * the subscription is mounted once but always sees fresh ids). */
  const repoRef = useRef(repo);
  repoRef.current = repo;
  const taskRef = useRef(selectedTaskId);
  taskRef.current = selectedTaskId;
  const candidateRef = useRef(currentCandidate);
  candidateRef.current = currentCandidate;
  useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    void deps
      .listenGate((event) => {
        const currentRepo = repoRef.current;
        const taskId = taskRef.current;
        if (currentRepo === null || taskId === null) {
          return;
        }
        if (!taskGateMatches(event, currentRepo, taskId)) {
          return;
        }
        dispatch({ type: 'gateEvent', event, currentCandidate: candidateRef.current });
      })
      .then((fn) => {
        if (cancelled) {
          fn();
        } else {
          unlisten = fn;
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [deps]);

  /** Run Gates: async by contract — no busy flag, the entries go live via
   * the started answer and then the `task.gate` events. */
  const onRunGates = () => {
    const currentRepo = state.repo;
    const taskId = state.selectedTaskId;
    if (currentRepo === null || taskId === null || gates.running) return;
    void (async () => {
      try {
        const started = await deps.runGates(currentRepo, taskId);
        dispatch({ type: 'gatesStarted', gates: started.started, candidate: started.candidate });
      } catch (error) {
        dispatch({ type: 'actionFailed', message: messageOf(error) });
      }
    })();
  };

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
    const currentRepo = state.repo;
    const taskId = state.selectedTaskId;
    if (currentRepo === null || taskId === null) return;
    // Gates warn (P7): confirm, never block — the operator owns the call.
    const problems = mergeGateProblems(state.gates);
    if (
      problems.length > 0 &&
      !confirm(
        `Merge ${taskId} despite gate problems?\n\n- ${problems.join('\n- ')}\n\n(tiller will merge anyway after Confirm)`,
      )
    ) {
      return;
    }
    dispatch({ type: 'actionStarted' });
    void (async () => {
      try {
        await deps.approve(currentRepo, taskId);
        const outcome = await deps.merge(currentRepo, taskId);
        dispatch({ type: 'mergeFinished', repo: currentRepo, taskId, outcome });
      } catch (error) {
        dispatch({ type: 'actionFailed', message: messageOf(error) });
        return;
      }
      await refresh(currentRepo);
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
  const gatesState = state.gates;
  const gateProblems = mergeGateProblems(gatesState);

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
                {gateProblems.length > 0 ? (
                  <span className="review-panel__gate-warn" data-testid="review-gate-warn">
                    gates: {gateProblems.length} problem{gateProblems.length > 1 ? 's' : ''}
                  </span>
                ) : !gatesState.loaded && phase === 'candidate' ? (
                  <span className="review-panel__gate-hint" data-testid="review-gate-hint">
                    no gate evidence — consider Run Gates first
                  </span>
                ) : null}
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

              <div className="review-panel__gates" data-testid="review-gates">
                <div className="review-panel__gates-head">
                  <span className="review-panel__gates-title">Gates</span>
                  <button
                    type="button"
                    onClick={onRunGates}
                    disabled={gatesState.running || phase === 'no_checkpoints'}
                    data-testid="review-run-gates"
                  >
                    Run Gates
                  </button>
                  {gatesState.stale && (
                    <span className="review-panel__gate-stale" data-testid="review-gate-stale">
                      STALE — re-run against the new candidate
                    </span>
                  )}
                  {gatesState.dirtyAtEnd && (
                    <span className="review-panel__gate-dirty" data-testid="review-gate-dirty">
                      worktree changed during gates — re-run
                    </span>
                  )}
                  {gatesState.evidenceLag && (
                    <span
                      className="review-panel__gate-dirty"
                      data-testid="review-gate-lag"
                    >
                      evidence still recording — Refresh to load log tails
                    </span>
                  )}
                </div>
                {gatesState.entries.length === 0 ? (
                  <p className="review-panel__hint">no gate results yet</p>
                ) : (
                  <ul className="review-panel__gate-list" data-testid="review-gate-list">
                    {gatesState.entries.map((entry) => (
                      <li key={entry.id}>
                        <span
                          className={`review-badge review-badge--gate-${entry.status}`}
                          data-testid="review-gate-badge"
                        >
                          {entry.status}
                        </span>
                        <span className="review-panel__gate-label">{entry.label}</span>
                        {entry.duration_ms !== undefined && (
                          <span className="review-panel__gate-duration">
                            {(entry.duration_ms / 1000).toFixed(1)}s
                          </span>
                        )}
                        {entry.log_tail !== undefined && entry.log_tail !== '' && (
                          <details className="review-panel__gate-log" data-testid="review-gate-log">
                            <summary>log tail</summary>
                            <pre>{entry.log_tail}</pre>
                          </details>
                        )}
                      </li>
                    ))}
                  </ul>
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
