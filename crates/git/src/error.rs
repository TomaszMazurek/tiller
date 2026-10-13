//! Error surface of the transactional git seam.
//!
//! Every variant carries a stable [`TransactionError::code`] so the rpc layer
//! (P6/P7) can map failures to protocol errors without matching on prose.

use crate::workspace::ApprovalState;

/// Everything that can go wrong in a Tiller git transaction.
#[derive(Debug)]
pub enum TransactionError {
    /// A git2 operation failed; `op` names the call site for debugging.
    Git {
        op: &'static str,
        source: git2::Error,
    },
    /// Filesystem bookkeeping (exclude file, worktree dir) failed.
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// `task_id` is not a valid identifier (worktree name, branch suffix and
    /// git-config subsection all derive from it).
    InvalidTaskId(String),
    /// The main repository is bare; transactions need a working directory.
    BareRepository,
    /// The main repository has no commits yet — there is no base to branch from.
    NoHeadCommit,
    /// A worktree or branch for this task already exists.
    AlreadyExists(String),
    /// No task workspace exists under this id.
    UnknownTask(String),
    /// `checkpoint` found no changes (tracked or untracked) to commit.
    NothingToCommit,
    /// `candidate` was called before any checkpoint existed on the task branch.
    NoCheckpoints,
    /// `approval_state` was called before any approval was recorded.
    NoApproval,
    /// `merge` refused because the approval is not valid anymore; carries the
    /// computed [`ApprovalState`] for rpc clients.
    StaleApproval(ApprovalState),
    /// `merge` refuses to clobber uncommitted changes in the main worktree
    /// (the merge result is checked out there).
    MainWorktreeDirty,
    /// `create` refused because the main worktree is on a detached HEAD:
    /// there is no branch to pin as the transaction's merge target.
    MainDetachedHead,
    /// `rebase_onto_new_base` refused to touch the worktree because it holds
    /// uncommitted tracked changes, or untracked files that collide with the
    /// rebased tree — the hard reset would destroy them. `details` lists the
    /// offending paths, each prefixed with the reason.
    DirtyWorktree { details: Vec<String> },
    /// `rebase_onto_new_base` hit a conflict; the library never resolves
    /// conflicts. `paths` are the conflicted paths relative to the worktree.
    RebaseConflict { paths: Vec<String> },
}

impl TransactionError {
    /// Stable machine-readable code for rpc mapping (P6/P7).
    pub fn code(&self) -> &'static str {
        match self {
            TransactionError::Git { .. } => "git_error",
            TransactionError::Io { .. } => "io_error",
            TransactionError::InvalidTaskId(_) => "invalid_task_id",
            TransactionError::BareRepository => "bare_repository",
            TransactionError::NoHeadCommit => "no_head_commit",
            TransactionError::AlreadyExists(_) => "already_exists",
            TransactionError::UnknownTask(_) => "unknown_task",
            TransactionError::NothingToCommit => "nothing_to_commit",
            TransactionError::NoCheckpoints => "no_checkpoints",
            TransactionError::NoApproval => "no_approval",
            TransactionError::StaleApproval(_) => "stale_approval",
            TransactionError::MainWorktreeDirty => "main_worktree_dirty",
            TransactionError::MainDetachedHead => "main_detached_head",
            TransactionError::DirtyWorktree { .. } => "dirty_worktree",
            TransactionError::RebaseConflict { .. } => "rebase_conflict",
        }
    }
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransactionError::Git { op, source } => {
                write!(f, "git operation `{op}` failed: {source}")
            }
            TransactionError::Io { op, source } => {
                write!(f, "io operation `{op}` failed: {source}")
            }
            TransactionError::InvalidTaskId(id) => {
                write!(
                    f,
                    "invalid task id {id:?}: expected non-empty [A-Za-z0-9._-] and not `.`/`..`"
                )
            }
            TransactionError::BareRepository => {
                f.write_str("main repository is bare; a working directory is required")
            }
            TransactionError::NoHeadCommit => {
                f.write_str("main repository has no HEAD commit to branch from")
            }
            TransactionError::AlreadyExists(id) => write!(f, "task `{id}` already exists"),
            TransactionError::UnknownTask(id) => write!(f, "unknown task `{id}`"),
            TransactionError::NothingToCommit => {
                f.write_str("nothing to checkpoint: no tracked or untracked changes")
            }
            TransactionError::NoCheckpoints => {
                f.write_str("no checkpoints yet: the task branch is still at its base")
            }
            TransactionError::NoApproval => f.write_str("no approval recorded for this task"),
            TransactionError::StaleApproval(state) => write!(f, "approval is stale ({state:?})"),
            TransactionError::MainWorktreeDirty => {
                f.write_str("main worktree has uncommitted changes; merge would clobber them")
            }
            TransactionError::MainDetachedHead => f.write_str(
                "main worktree is on a detached HEAD; a branch is required to pin the \
                 transaction's merge target",
            ),
            TransactionError::DirtyWorktree { details } => write!(
                f,
                "worktree is dirty; rebase would destroy uncommitted work: {}",
                details.join(", ")
            ),
            TransactionError::RebaseConflict { paths } => {
                write!(f, "rebase conflict on {}", paths.join(", "))
            }
        }
    }
}

impl std::error::Error for TransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransactionError::Git { source, .. } => Some(source),
            TransactionError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
