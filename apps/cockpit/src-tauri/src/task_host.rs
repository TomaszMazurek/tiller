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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tiller_git::{
    ApprovalState, CandidateDiff, MergeOutcome, ReviewCandidate, TaskWorkspace, TransactionError,
};

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

/// Serves the `task.*` rpc surface. See the module docs for the locking
/// contract; the type itself is `Send + Sync` through interior mutability.
#[derive(Debug, Default)]
pub struct TaskHost {
    /// Repo path (canonicalized best-effort) → its operation lock. Entries
    /// are never removed: a handful of live repos, locks are one `Mutex`
    /// each — removing them would only reopen the race the map exists to
    /// close.
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl TaskHost {
    pub fn new() -> Self {
        Self::default()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_locks_are_shared_per_path_and_distinct_across_paths() {
        let host = TaskHost::new();
        let a1 = host.lock_repo("C:/definitely/not/here");
        let a2 = host.lock_repo("C:/definitely/not/here");
        // Same key → the very same lock object: per-repo serialization.
        assert!(Arc::ptr_eq(&a1, &a2));
        let b = host.lock_repo("C:/definitely/not/elsewhere");
        assert!(!Arc::ptr_eq(&a1, &b));
    }
}
