//! Task host: the `task.*` rpc surface over `tiller-git` transactions.
//!
//! One [`TaskHost`] lives in the dispatcher for the whole app. Its single
//! job beyond forwarding to [`TaskWorkspace`] is **per-repo serialization**:
//! the task flows are open→act→answer sequences over shared git state
//! (refs, config, index), so two concurrent rpc calls for the same repo
//! could interleave (e.g. a checkpoint racing a rebase's hard reset). A
//! coarse `Mutex<()>` per repository path is the deliberate simple answer
//! — fighting git2's own threading model would buy nothing at this scale.
//! Repositories are opened per call (`TaskWorkspace::open`) and dropped
//! with the call; no repository handle is held across requests.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use git2::Oid;
use serde::Serialize;
use tiller_git::{
    ApprovalState, CandidateDiff, MergeOutcome, ReviewCandidate, TaskWorkspace, TransactionError,
};

use crate::gates::{
    gate_preset, tail_bytes, CommandRunner, GatePreset, GateResult, EVENT_GATE, GATE_TIMEOUT,
    LOG_TAIL_BYTES,
};
use crate::term_host::EventSink;

/// `task.list` entry: one task workspace with its pinned merge target and
/// the two ends of its review range (hex oids).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub target_branch: String,
    pub base: String,
    pub tip: String,
}

/// `task.create` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskCreated {
    pub task_id: String,
    /// The worktree's working directory (where the agent should be pointed).
    pub worktree_path: PathBuf,
    pub target_branch: String,
}

/// Task phase for `task.status`: `no_checkpoints` ⇔ the task branch still
/// sits on its base (nothing reviewable yet); `candidate` ⇔ at least one
/// checkpoint exists and [`TaskStatusInfo::candidate`] names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    NoCheckpoints,
    Candidate,
}

/// `task.status` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskStatusInfo {
    pub status: TaskPhase,
    /// The current review candidate, once any checkpoint exists.
    pub candidate: Option<ReviewCandidate>,
    /// Recorded approval's validity (`null` ⇔ never approved).
    pub approval_state: Option<ApprovalState>,
    pub checkpoints_count: usize,
}

/// `task.rebase` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskRebased {
    pub ok: bool,
    /// Task-branch tip after the rebase (hex oid); new ids ⇒ old approvals
    /// are stale, a fresh approve is required before merge.
    pub new_tip: String,
}

/// Why a gate run could not start. Git-level failures reuse the stable
/// `TransactionError` codes; the gate-specific preconditions get their own
/// app-level codes in `dispatch.rs` (the -3202x block, after rebase).
#[derive(Debug)]
pub enum GateStartError {
    Git(TransactionError),
    /// The worktree has uncommitted tracked changes **or untracked
    /// files** — the gates would verify a tree that is not the candidate
    /// (`cargo` compiles untracked sources too). Checkpoint first.
    Dirty {
        files: Vec<String>,
    },
    /// A gate run is already active for this repo+task.
    AlreadyRunning,
}

impl From<TransactionError> for GateStartError {
    fn from(error: TransactionError) -> Self {
        GateStartError::Git(error)
    }
}

/// `task.run_gates` answer: what is now running, against which candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatesStarted {
    /// Gate ids that were started, in run order.
    pub started: Vec<String>,
    /// The candidate (task-branch tip) the gates run against; results
    /// land as `task.gate` events and then in `task.gates` evidence.
    pub candidate: String,
}

/// `task.gates` answer: the evidence view. Field presence is the wire
/// shape: `{running}` ⇔ nothing recorded; `{stale: true, running}` ⇔ a
/// run finished under a candidate that is no longer the tip (the record
/// itself reads back as stale by definition); `{candidate, evidence,
/// running}` ⇔ fresh evidence for the current tip.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GatesInfo {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Per-task cargo target dir for gate runs. Gate builds must not dirty
/// the worktree they verify: without this, the first run leaves an
/// untracked `target/` (unless the repo's `.gitignore` covers it),
/// which the next start would refuse as dirty and the post-run
/// re-check would annotate as `dirty_at_end`. A per-(repo, task) dir
/// under the OS temp keeps incremental caching across runs of one task;
/// different repos never share it (stable repo hash in the name).
fn gate_target_dir(repo: &str, task_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    // Stable across runs (DefaultHasher::new() has fixed keys, unlike
    // RandomState): the cache must survive restarts for incremental
    // builds. Distinct repos with the same task_id never share the dir
    // (P7 review note); collisions of 64-bit SipHash over a handful of
    // paths are not a concern in practice.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut hasher);
    let repo_tag = format!("{:016x}", hasher.finish());
    std::env::temp_dir()
        .join("tiller-gates-target")
        .join(format!("{repo_tag}-{task_id}"))
        .to_string_lossy()
        .into_owned()
}

/// Serves the `task.*` rpc surface. See the module docs for the locking
/// contract; the type itself is `Send + Sync` through interior mutability.
pub struct TaskHost {
    /// Repo path (canonicalized best-effort) → its operation lock. Entries
    /// are never removed: a handful of live repos, locks are one `Mutex`
    /// each — removing them would only reopen the race the map exists to
    /// close.
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Gate seams + in-memory run bookkeeping (P7).
    gates: GateHost,
}

/// Gate machinery owned by the host: the injected command seam and event
/// sink, plus the run registry. The registry mutex is deliberately NOT the
/// per-repo lock — a gate run (up to 2×[`GATE_TIMEOUT`]) must never hold
/// the repo lock; it is taken only for the short open→act→answer steps
/// (preconditions, evidence write) around it.
struct GateHost {
    runner: Arc<dyn CommandRunner>,
    sink: Arc<dyn EventSink>,
    /// (canonical repo, task_id) with a live run.
    running: Mutex<HashSet<(PathBuf, String)>>,
    /// Last candidate each (repo, task) successfully wrote evidence
    /// under — lets `task.gates` answer `{stale: true}` for a run whose
    /// candidate has since been superseded (in-memory only: after an app
    /// restart a superseded record is simply absent, which is the same
    /// "no evidence" answer by definition).
    last_run: Mutex<HashMap<(PathBuf, String), String>>,
}

impl TaskHost {
    pub fn new(sink: Arc<dyn EventSink>, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
            gates: GateHost {
                runner,
                sink,
                running: Mutex::new(HashSet::new()),
                last_run: Mutex::new(HashMap::new()),
            },
        }
    }

    fn gate_key(&self, repo: &str, task_id: &str) -> (PathBuf, String) {
        (
            std::fs::canonicalize(repo).unwrap_or_else(|_| PathBuf::from(repo)),
            task_id.to_owned(),
        )
    }

    fn lock_repo(&self, repo: &str) -> Arc<Mutex<()>> {
        // Canonicalize best-effort: same directory under two spellings
        // would get two locks. Whatever canonicalize cannot resolve is
        // still a consistent key for the string the UI keeps sending.
        let key = std::fs::canonicalize(repo).unwrap_or_else(|_| PathBuf::from(repo));
        let mut locks = self.locks.lock().expect("task lock map poisoned");
        locks.entry(key).or_default().clone()
    }

    /// Open a task under its repo's lock and run `f` on it. The lock is
    /// held for the whole call: open→act→answer is one serialized step,
    /// which is the entire point of the host.
    fn with_task<R>(
        &self,
        repo: &str,
        task_id: &str,
        f: impl FnOnce(TaskWorkspace) -> Result<R, TransactionError>,
    ) -> Result<R, TransactionError> {
        let lock = self.lock_repo(repo);
        let _guard = lock.lock().expect("task lock poisoned");
        let ws = TaskWorkspace::open(Path::new(repo), task_id)?;
        f(ws)
    }

    /// All task workspaces of a repo. Entries that fail to open are
    /// skipped, not fatal: `list` is a *view*, and a task removed while
    /// listing must not blank the whole queue ([`TaskWorkspace::list`]
    /// already skips dirs gone the same way).
    pub fn list(&self, repo: &str) -> Result<Vec<TaskSummary>, TransactionError> {
        let lock = self.lock_repo(repo);
        let _guard = lock.lock().expect("task lock poisoned");
        let mut out = Vec::new();
        for entry in TaskWorkspace::list(Path::new(repo))? {
            let Ok(ws) = TaskWorkspace::open(Path::new(repo), &entry.task_id) else {
                continue;
            };
            let tip = ws
                .repository()
                .head()
                .and_then(|head| head.peel_to_commit())
                .map(|commit| commit.id().to_string())
                .map_err(|source| TransactionError::Git {
                    op: "resolve task tip",
                    source,
                })?;
            out.push(TaskSummary {
                task_id: entry.task_id,
                target_branch: ws.target_branch().to_owned(),
                base: entry.base_commit.to_string(),
                tip,
            });
        }
        Ok(out)
    }

    /// Create a workspace. The task id is always caller-supplied and
    /// validated by `tiller-git` — no auto-generation here: naming a task
    /// is the UI's decision, the backend just refuses invalid ids
    /// ([`TransactionError::InvalidTaskId`]).
    pub fn create(&self, repo: &str, task_id: &str) -> Result<TaskCreated, TransactionError> {
        let lock = self.lock_repo(repo);
        let _guard = lock.lock().expect("task lock poisoned");
        let ws = TaskWorkspace::create(Path::new(repo), task_id)?;
        Ok(TaskCreated {
            task_id: ws.task_id().to_owned(),
            worktree_path: ws.path().to_path_buf(),
            target_branch: ws.target_branch().to_owned(),
        })
    }

    pub fn status(&self, repo: &str, task_id: &str) -> Result<TaskStatusInfo, TransactionError> {
        self.with_task(repo, task_id, |ws| {
            let checkpoints_count = ws.checkpoints()?.len();
            match ws.candidate() {
                Ok(candidate) => Ok(TaskStatusInfo {
                    status: TaskPhase::Candidate,
                    candidate: Some(candidate),
                    // approval_state() only fails with NoApproval here —
                    // "never approved" is the answer, not an error.
                    approval_state: ws.approval_state().ok(),
                    checkpoints_count,
                }),
                Err(TransactionError::NoCheckpoints) => Ok(TaskStatusInfo {
                    status: TaskPhase::NoCheckpoints,
                    candidate: None,
                    approval_state: None,
                    checkpoints_count,
                }),
                Err(other) => Err(other),
            }
        })
    }

    pub fn checkpoint(
        &self,
        repo: &str,
        task_id: &str,
        message: &str,
    ) -> Result<String, TransactionError> {
        self.with_task(repo, task_id, |mut ws| {
            ws.checkpoint(message).map(|oid| oid.to_string())
        })
    }

    /// Linear diff of the task's candidate (see
    /// [`TaskWorkspace::candidate_diff`]); reads git objects only, never
    /// the worktree.
    pub fn diff(
        &self,
        repo: &str,
        task_id: &str,
        path: Option<&str>,
    ) -> Result<CandidateDiff, TransactionError> {
        self.with_task(repo, task_id, |ws| ws.candidate_diff(path))
    }

    /// Approve the *current* candidate: the tip at the moment of the call.
    /// Any later checkpoint invalidates this approval (StaleCandidate).
    pub fn approve(&self, repo: &str, task_id: &str) -> Result<String, TransactionError> {
        self.with_task(repo, task_id, |mut ws| {
            let candidate = ws.candidate()?;
            ws.approve(candidate.candidate_commit)?;
            Ok(candidate.candidate_commit.to_string())
        })
    }

    /// Merge the recorded approval. On [`MergeOutcome::Merged`] the task is
    /// **not** discarded — the answer goes to the UI, which offers an
    /// explicit cleanup (`task.discard`).
    pub fn merge(&self, repo: &str, task_id: &str) -> Result<MergeOutcome, TransactionError> {
        self.with_task(repo, task_id, |ws| {
            let approval = ws.approval().cloned().ok_or(TransactionError::NoApproval)?;
            ws.merge(&approval)
        })
    }

    /// Rebase onto the pinned target's current tip. Conflicts come back as
    /// [`TransactionError::RebaseConflict`] with the conflicted paths.
    pub fn rebase(&self, repo: &str, task_id: &str) -> Result<TaskRebased, TransactionError> {
        self.with_task(repo, task_id, |mut ws| {
            ws.rebase_onto_new_base()?;
            let new_tip = ws
                .repository()
                .head()
                .and_then(|head| head.peel_to_commit())
                .map(|commit| commit.id().to_string())
                .map_err(|source| TransactionError::Git {
                    op: "resolve rebased tip",
                    source,
                })?;
            Ok(TaskRebased { ok: true, new_tip })
        })
    }

    /// Soft rollback: drop all checkpoints, back to the base
    /// ([`TaskWorkspace::reset_to_base`]).
    pub fn reset(&self, repo: &str, task_id: &str) -> Result<(), TransactionError> {
        self.with_task(repo, task_id, |mut ws| ws.reset_to_base())
    }

    /// Remove the task completely (worktree + branch + metadata).
    pub fn discard(&self, repo: &str, task_id: &str) -> Result<(), TransactionError> {
        self.with_task(repo, task_id, |ws| ws.discard())
    }

    // ------------------------------------------------------------- gates

    /// Start the verification gates for a task (P7): `cargo check` /
    /// `cargo test` in the task worktree against the current candidate.
    ///
    /// Preconditions (checked here, under the repo lock): the worktree
    /// exists, has **no uncommitted tracked changes and no untracked
    /// files** (gates verify the candidate — a dirty tree would be a
    /// different program; checkpoint first) and has a candidate (≥1
    /// checkpoint). The candidate oid is snapshotted here and is what
    /// the evidence will be bound to.
    ///
    /// The run itself is **async**: a worker thread executes the gates
    /// sequentially (check → test), emits `task.gate` events on every
    /// status change, and on completion writes the evidence via
    /// [`TaskWorkspace::set_evidence`] — under the repo lock again, so a
    /// concurrent checkpoint can interleave safely: the record is bound to
    /// the snapshotted oid and simply reads back stale if the tip moved.
    ///
    /// One run per (repo, task) at a time ([`GateStartError::AlreadyRunning`]).
    pub fn start_gates(
        self: &Arc<Self>,
        repo: &str,
        task_id: &str,
        gate_ids: &[&str],
    ) -> Result<GatesStarted, GateStartError> {
        let presets: Vec<&'static GatePreset> = gate_ids
            .iter()
            .map(|id| gate_preset(id).expect("dispatch validates gate ids"))
            .collect();

        // Claim the run slot first: check-and-insert on the registry is
        // atomic, so two concurrent starts cannot both win.
        let key = self.gate_key(repo, task_id);
        if !self
            .gates
            .running
            .lock()
            .expect("gate registry poisoned")
            .insert(key.clone())
        {
            return Err(GateStartError::AlreadyRunning);
        }

        // Preconditions under the per-repo lock (open→check→answer). The
        // checks need the gate-level error type, so this block acquires the
        // lock directly instead of going through `with_task` (whose error
        // channel is `TransactionError` by design).
        let lock = self.lock_repo(repo);
        let guard = lock.lock().expect("task lock poisoned");
        let preconditions = (|| -> Result<(PathBuf, Oid), GateStartError> {
            let ws = TaskWorkspace::open(Path::new(repo), task_id).map_err(GateStartError::Git)?;
            let status = ws.status().map_err(GateStartError::Git)?;
            // Untracked files are dirt here too (P7 review MAJOR-1):
            // `cargo` would compile files that are not part of the
            // candidate, so the refusal lists both kinds of paths —
            // checkpoint first.
            if !status.changed.is_empty() || !status.untracked.is_empty() {
                let mut files = status.changed;
                files.extend(status.untracked);
                return Err(GateStartError::Dirty { files });
            }
            let candidate = ws.candidate().map_err(GateStartError::Git)?;
            Ok((ws.path().to_path_buf(), candidate.candidate_commit))
        })();
        drop(guard);
        let (workdir, candidate) = match preconditions {
            Ok(value) => value,
            Err(error) => {
                self.gates
                    .running
                    .lock()
                    .expect("gate registry poisoned")
                    .remove(&key);
                return Err(error);
            }
        };

        let host = Arc::clone(self);
        let repo_owned = repo.to_owned();
        let task_owned = task_id.to_owned();
        thread::Builder::new()
            .name(format!("tiller-gates-{task_id}"))
            .spawn(move || {
                host.run_gates_to_evidence(&repo_owned, &task_owned, &workdir, candidate, presets)
            })
            .expect("gate worker thread spawns");

        Ok(GatesStarted {
            started: gate_ids.iter().map(|id| id.to_string()).collect(),
            candidate: candidate.to_string(),
        })
    }

    /// The worker body: run the gates, emit events, write the evidence.
    /// Split from [`Self::start_gates`] so the whole flow (events,
    /// evidence) is drivable synchronously in tests without racing the
    /// thread spawn.
    fn run_gates_to_evidence(
        &self,
        repo: &str,
        task_id: &str,
        workdir: &Path,
        candidate: Oid,
        presets: Vec<&'static GatePreset>,
    ) {
        let candidate_hex = candidate.to_string();
        let env = [
            ("CARGO_TERM_COLOR".to_owned(), "never".to_owned()),
            (
                "CARGO_TARGET_DIR".to_owned(),
                gate_target_dir(repo, task_id),
            ),
        ];
        let mut results = Vec::new();
        for preset in presets {
            let event = |status: &str, duration_ms: Option<u64>| {
                let mut payload = serde_json::json!({
                    "repo": repo,
                    "task_id": task_id,
                    "candidate": candidate_hex,
                    "id": preset.id,
                    "label": preset.label,
                    "status": status,
                });
                if let Some(ms) = duration_ms {
                    payload["duration_ms"] = ms.into();
                }
                payload
            };
            self.gates.sink.emit(EVENT_GATE, event("running", None));
            let outcome =
                self.gates
                    .runner
                    .run(preset.program, preset.args, workdir, &env, GATE_TIMEOUT);
            let result = GateResult {
                id: preset.id.to_owned(),
                label: preset.label.to_owned(),
                status: outcome.status,
                exit_code: outcome.exit_code,
                duration_ms: outcome.duration_ms,
                log_tail: tail_bytes(&outcome.output, LOG_TAIL_BYTES),
            };
            self.gates.sink.emit(
                EVENT_GATE,
                event(result.status.as_str(), Some(result.duration_ms)),
            );
            results.push(result);
        }

        // Evidence write under the repo lock, through the same seam every
        // other task mutation uses. A task discarded mid-run makes this a
        // no-op (UnknownTask): the per-gate events already carried the
        // results, and a record for a discarded task would be orphaned
        // anyway — the error is swallowed deliberately.
        //
        // Review A.2 — post-run cleanliness: the worktree is re-checked
        // (changed + untracked) in the same locked step as the write; a
        // dirty end **annotates** the evidence (`dirty_at_end` +
        // `dirty_paths`) instead of discarding the run — incomplete
        // proof, not a lost one. Semantic boundary, stated on purpose:
        // this is NOT full atomicity — a change made *and reverted*
        // inside the run window is undetectable; the annotation is a
        // best-effort invalidation marker, consciously bounded. A
        // status read that fails leaves the run unannotated (evidence
        // is advisory bookkeeping; a git-level failure here means the
        // task is likely gone and `set_evidence` fails with it).
        let written = self
            .with_task(repo, task_id, |ws| {
                let dirty_paths = ws
                    .status()
                    .map(|status| {
                        let mut paths = status.changed;
                        paths.extend(status.untracked);
                        paths
                    })
                    .unwrap_or_default();
                let mut evidence = serde_json::json!({
                    "gates": results,
                    "dirty_at_start": false,
                    "dirty_at_end": !dirty_paths.is_empty(),
                    "finished_at": now_unix_ms(),
                });
                if !dirty_paths.is_empty() {
                    evidence["dirty_paths"] = dirty_paths.into();
                }
                ws.set_evidence(candidate, evidence)
            })
            .is_ok();
        let key = self.gate_key(repo, task_id);
        self.gates
            .running
            .lock()
            .expect("gate registry poisoned")
            .remove(&key);
        if written {
            self.gates
                .last_run
                .lock()
                .expect("gate registry poisoned")
                .insert(key, candidate_hex);
        }
    }

    /// The evidence view for `task.gates`: fresh evidence if present,
    /// `stale` when a finished run's candidate was superseded, and the
    /// live-run flag from the registry. Reads the git state under the
    /// repo lock; the registry under its own (never both at once).
    pub fn gates(&self, repo: &str, task_id: &str) -> Result<GatesInfo, TransactionError> {
        let running = self
            .gates
            .running
            .lock()
            .expect("gate registry poisoned")
            .contains(&self.gate_key(repo, task_id));
        let evidence = self.with_task(repo, task_id, |ws| ws.evidence())?;
        Ok(match evidence {
            Some((oid, value)) => GatesInfo {
                running,
                candidate: Some(oid.to_string()),
                evidence: Some(value),
                stale: false,
            },
            None => {
                let last_run = self
                    .gates
                    .last_run
                    .lock()
                    .expect("gate registry poisoned")
                    .get(&self.gate_key(repo, task_id))
                    .cloned();
                GatesInfo {
                    running,
                    candidate: None,
                    evidence: None,
                    // A finished run exists but its candidate is no longer
                    // readable as fresh — superseded (or the task reset to
                    // base and re-checkpointed): either way, re-run against
                    // the new candidate.
                    stale: last_run.is_some(),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_locks_are_shared_per_path_and_distinct_across_paths() {
        let host = TaskHost::new(
            Arc::new(crate::term_host::test_util::NoopSink),
            Arc::new(crate::gates::ProcessRunner),
        );
        let a1 = host.lock_repo("C:/definitely/not/here");
        let a2 = host.lock_repo("C:/definitely/not/here");
        // Same key → the very same lock object: per-repo serialization.
        assert!(Arc::ptr_eq(&a1, &a2));
        let b = host.lock_repo("C:/definitely/not/elsewhere");
        assert!(!Arc::ptr_eq(&a1, &b));
    }

    #[test]
    fn gate_target_dirs_are_stable_and_repo_scoped() {
        // Same (repo, task): stable across calls — the incremental cache
        // must survive restarts (DefaultHasher has fixed keys).
        assert_eq!(
            gate_target_dir("C:/repos/alpha", "fix-1"),
            gate_target_dir("C:/repos/alpha", "fix-1")
        );
        // Different repos, same task_id: never a shared dir (P7 review
        // note — two repos must not fight over one cargo target lock).
        assert_ne!(
            gate_target_dir("C:/repos/alpha", "fix-1"),
            gate_target_dir("C:/repos/beta", "fix-1")
        );
    }
}
