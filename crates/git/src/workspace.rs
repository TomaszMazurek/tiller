//! Task workspaces and the review/merge lifecycle on top of git worktrees.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::build::CheckoutBuilder;
use git2::{
    BranchType, ErrorCode, IndexAddOption, MergeOptions, Oid, RebaseOptions, Repository, ResetType,
    Status, StatusOptions,
};
use serde::{Deserialize, Serialize};

use crate::error::TransactionError;

/// Root of Tiller-managed state inside the main worktree. Excluded from git
/// status via `.git/info/exclude` (pattern [`EXCLUDE_PATTERN`]), never via the
/// user's `.gitignore`.
const TILLER_DIR: &str = ".tiller";
const WORKTREES_SUBDIR: &str = "worktrees";
const EXCLUDE_PATTERN: &str = "/.tiller/";
const BRANCH_PREFIX: &str = "tiller/task-";
const AUTHOR_NAME: &str = "tiller";
const AUTHOR_EMAIL: &str = "tiller@local";

/// Total byte cap for one [`CandidateDiff`] response (≈256 KiB): enough for
/// a meaningful multi-file review read without shipping megabytes over the
/// rpc seam on every cockpit refresh.
pub const DIFF_TOTAL_BYTE_CAP: usize = 256 * 1024;
/// Per-file patch cap (≈64 KiB) inside a [`CandidateDiff`]: one huge
/// generated/locked file must not starve the rest of the review.
pub const DIFF_FILE_BYTE_CAP: usize = 64 * 1024;
/// Marker appended when a cap cut a patch short.
pub const DIFF_TRUNCATION_MARKER: &str = "… truncated …";

fn cfg_key_base(task_id: &str) -> String {
    format!("tiller.worktree.{task_id}.base")
}
fn cfg_key_target_branch(task_id: &str) -> String {
    format!("tiller.worktree.{task_id}.target-branch")
}
fn cfg_key_approval_candidate(task_id: &str) -> String {
    format!("tiller.worktree.{task_id}.approval.candidate")
}
fn cfg_key_approval_at(task_id: &str) -> String {
    format!("tiller.worktree.{task_id}.approval.at")
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// --------------------------------------------------------------- serde types

/// Hex-string serde for [`Oid`] (rpc-friendly: `"1a2b…"` full oid).
mod oid_serde {
    use super::Oid;
    use serde::Deserialize;

    pub fn serialize<S: serde::Serializer>(oid: &Oid, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&oid.to_string())
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Oid, D::Error> {
        let text = String::deserialize(d)?;
        Oid::from_str(&text)
            .map_err(|e| serde::de::Error::custom(format!("invalid git oid {text:?}: {e}")))
    }
}

/// Immutable review candidate: "what is being approved" — the current tip of
/// the task branch plus the base it was written against (core §3 rule 5).
///
/// A candidate is a value snapshot, never mutated: any new checkpoint on the
/// task branch produces a *different* candidate and invalidates approvals of
/// the previous one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCandidate {
    pub task_id: String,
    #[serde(with = "oid_serde")]
    pub candidate_commit: Oid,
    #[serde(with = "oid_serde")]
    pub base_commit: Oid,
    /// Unix epoch milliseconds.
    pub created_at: u64,
}

/// A recorded approval of one specific candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The candidate commit this approval covers.
    #[serde(with = "oid_serde")]
    pub candidate: Oid,
    /// Unix epoch milliseconds.
    pub approved_at: u64,
}

/// Result of [`TaskWorkspace::approval_state`].
///
/// Valid ⇔ the approved candidate is still the tip of the task branch AND
/// the pinned target branch still points at exactly the base commit. A new
/// checkpoint after approval → `StaleCandidate`; anything landing on the
/// target branch after branching → `BaseMoved`. The main worktree's current
/// checkout is irrelevant — freshness is judged against the pinned branch
/// recorded at create time (P1-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Valid,
    StaleCandidate,
    BaseMoved,
}

/// One checkpoint (commit) on the task branch, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInfo {
    #[serde(with = "oid_serde")]
    pub oid: Oid,
    /// Commit time, unix epoch seconds.
    pub ts: u64,
    /// The caller-supplied message (Tiller's bookkeeping trailer stripped).
    pub message: String,
}

/// Result of [`TaskWorkspace::merge`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeOutcome {
    /// The task landed on the pinned target branch as a merge commit.
    Merged {
        #[serde(with = "oid_serde")]
        merge_commit: Oid,
        /// Whether the main worktree was also checked out to the merge
        /// result. `false` when the main worktree was on a different branch
        /// (or detached) than the pinned target: the target ref advanced,
        /// the main worktree was deliberately left untouched — nothing was
        /// clobbered there.
        main_worktree_updated: bool,
    },
    /// The pinned target branch advanced past the candidate's base; the
    /// library never merges over a moved base. Rebase consciously
    /// ([`TaskWorkspace::rebase_onto_new_base`]), then approve the new
    /// candidate and merge again.
    NeedsRebase {
        #[serde(with = "oid_serde")]
        new_base: Oid,
    },
}

/// Working-tree status of the task worktree (for P6 diff/queue views).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    /// Tracked files modified/staged/deleted (paths relative to the worktree).
    pub changed: Vec<String>,
    /// Untracked files not ignored.
    pub untracked: Vec<String>,
}

/// How one file changed between the base and the candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    Added,
    Deleted,
    Modified,
    Renamed,
    /// Anything libgit2 can classify but Tiller has no dedicated name for
    /// (typechange, copied, ignored…); shown as-is, never silently dropped.
    Other,
}

/// One file's linear patch inside a [`CandidateDiff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// Path relative to the worktree; renames read `"old -> new"`.
    pub path: String,
    pub change_type: ChangeType,
    /// Unified patch (context 3) including the `diff --git`/`---`/`+++`
    /// header lines. Empty for binary files; a file past the total cap (or
    /// one where not even a single line fit) ships **metadata only** —
    /// empty patch, `truncated: true`; a cut-short patch ends with the
    /// `… truncated …` marker.
    pub patch: String,
    /// Binary delta: flagged, content never ships.
    pub binary: bool,
    /// The patch was cut by a cap (per-file or total): refine with the
    /// `path` filter, or accept that content was dropped.
    pub truncated: bool,
}

/// Linear diff base commit → review candidate with byte caps.
///
/// Caps keep one giant diff from flooding the rpc seam (and the cockpit)
/// on every refresh: [`DIFF_FILE_BYTE_CAP`] per file so a single huge
/// generated file cannot starve the rest of the review,
/// [`DIFF_TOTAL_BYTE_CAP`] for the whole payload. The caps are enforced
/// **during** the diff walk — a file's lines are collected only while
/// they fit its allowance, and once the total cap drains the walk stops;
/// deltas past that point ship as metadata-only entries (`path`,
/// `change_type`, `truncated: true`, no patch) so the file list stays
/// complete. `truncated` (at either level) means "refine with the `path`
/// filter", never "content is wrong": patches always start at the file's
/// first change and are cut only at the tail. Marker bytes are included
/// in `total_bytes`, so it can exceed `byte_cap` by at most one marker
/// per cut file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateDiff {
    pub files: Vec<FileDiff>,
    /// Any file was cut (per-file cap) or dropped content (total cap).
    pub truncated: bool,
    /// Sum of shipped patch bytes (after caps, markers included).
    pub total_bytes: usize,
    /// The total cap that governed this response ([`DIFF_TOTAL_BYTE_CAP`]).
    pub byte_cap: usize,
}

/// A task workspace discovered by [`TaskWorkspace::list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeEntry {
    pub task_id: String,
    /// The worktree's working directory.
    pub path: PathBuf,
    #[serde(with = "oid_serde")]
    pub base_commit: Oid,
}

// ------------------------------------------------------------------ helpers

fn git(op: &'static str) -> impl Fn(git2::Error) -> TransactionError {
    move |source| TransactionError::Git { op, source }
}

fn io(op: &'static str) -> impl Fn(std::io::Error) -> TransactionError {
    move |source| TransactionError::Io { op, source }
}

fn validate_task_id(task_id: &str) -> Result<(), TransactionError> {
    let valid = !task_id.is_empty()
        && task_id != "."
        && task_id != ".."
        && task_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(TransactionError::InvalidTaskId(task_id.to_owned()))
    }
}

fn branch_name(task_id: &str) -> String {
    format!("{BRANCH_PREFIX}{task_id}")
}

/// Display path for a diff delta: `"old -> new"` for renames, otherwise the
/// surviving (or removed) side. Distinct for every delta in one diff, so it
/// doubles as the bucket key while assembling per-file patches.
fn display_path(delta: &git2::DiffDelta<'_>) -> String {
    let old = delta
        .old_file()
        .path()
        .and_then(Path::to_str)
        .map(str::to_owned);
    let new = delta
        .new_file()
        .path()
        .and_then(Path::to_str)
        .map(str::to_owned);
    match (old, new) {
        (Some(old), Some(new)) if old != new => format!("{old} -> {new}"),
        (_, Some(new)) => new,
        (Some(old), None) => old,
        (None, None) => String::new(),
    }
}

fn change_type(delta: &git2::DiffDelta<'_>) -> ChangeType {
    match delta.status() {
        git2::Delta::Added => ChangeType::Added,
        git2::Delta::Deleted => ChangeType::Deleted,
        git2::Delta::Modified => ChangeType::Modified,
        git2::Delta::Renamed => ChangeType::Renamed,
        _ => ChangeType::Other,
    }
}

fn worktree_path(main_workdir: &Path, task_id: &str) -> PathBuf {
    main_workdir
        .join(TILLER_DIR)
        .join(WORKTREES_SUBDIR)
        .join(task_id)
}

fn set_config_str(repo: &Repository, name: &str, value: &str) -> Result<(), TransactionError> {
    let mut cfg = repo.config().map_err(git("config open"))?;
    cfg.set_str(name, value).map_err(git("config set"))
}

fn read_config_str(repo: &Repository, name: &str) -> Option<String> {
    repo.config().ok()?.get_string(name).ok()
}

/// Ensure `/.tiller/` is ignored through `.git/info/exclude` (created if
/// needed). The user's `.gitignore` is never touched. A missing exclude file
/// is simply treated as empty content — `.git/info` may exist without it.
fn exclude_tiller_dir(main: &Repository) -> Result<(), TransactionError> {
    let info = main.path().join("info");
    fs::create_dir_all(&info).map_err(io("create .git/info"))?;
    let exclude = info.join("exclude");
    let mut content = match fs::read_to_string(&exclude) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(TransactionError::Io {
                op: "read info/exclude",
                source: e,
            })
        }
    };
    if content.lines().any(|line| line.trim() == EXCLUDE_PATTERN) {
        return Ok(());
    }
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(EXCLUDE_PATTERN);
    content.push('\n');
    fs::write(&exclude, content).map_err(io("write info/exclude"))
}

/// Main worktree clean check for `merge`: non-ignored changes only (explicit
/// options — libgit2's NULL-option status *includes* ignored entries).
fn main_is_clean(main: &Repository) -> Result<bool, TransactionError> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    Ok(main
        .statuses(Some(&mut opts))
        .map_err(git("main status"))?
        .is_empty())
}

fn tiller_signature() -> Result<git2::Signature<'static>, TransactionError> {
    git2::Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).map_err(git("signature"))
}

/// Strip Tiller's bookkeeping trailer from a checkpoint message.
fn strip_checkpoint_trailer(message: &str) -> String {
    match message.rfind("\n\n[tiller checkpoint ") {
        Some(idx) => message[..idx].to_owned(),
        None => message.to_owned(),
    }
}

// ------------------------------------------------------------ TaskWorkspace

/// A git-worktree-backed workspace for one task: isolation for the agent,
/// checkpoints as commits, and the review/merge lifecycle on top.
///
/// One task = one worktree (`.tiller/worktrees/<task_id>`) + one branch
/// (`tiller/task-<task_id>`) + persisted metadata in the repo-local git
/// config (`tiller.worktree.<task_id>.*`): the base commit and any recorded
/// approval, so state survives reopen.
///
/// Single-writer per worktree by contract (AGENTS.md); this type is not
/// synchronised.
pub struct TaskWorkspace {
    /// Repository handle opened on the task worktree (HEAD = task branch).
    repo: Repository,
    /// The main repository's working directory.
    main_path: PathBuf,
    task_id: String,
    /// The main-repo branch pinned at create time — the transaction's merge
    /// target. Base freshness and merge always refer to THIS branch, never to
    /// whatever the main worktree currently has checked out (P1-2).
    target_branch: String,
    base_commit: Oid,
    approval: Option<Approval>,
}

impl std::fmt::Debug for TaskWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskWorkspace")
            .field("task_id", &self.task_id)
            .field("base_commit", &self.base_commit.to_string())
            .field("approval", &self.approval)
            .finish_non_exhaustive()
    }
}

impl TaskWorkspace {
    /// Create a workspace for `task_id`: a worktree under
    /// `{main}/.tiller/worktrees/{task_id}` on a new branch
    /// `tiller/task-{task_id}` from the current main HEAD. The main-repo
    /// branch HEAD points at is **pinned** as the workspace's target branch
    /// (`tiller.worktree.<id>.target-branch`): base freshness and merge
    /// always refer to it, regardless of what the main worktree checks out
    /// later. A detached main HEAD is refused — there would be no branch to
    /// pin, hence no well-defined merge target.
    pub fn create(main_repo: &Path, task_id: &str) -> Result<Self, TransactionError> {
        validate_task_id(task_id)?;
        let main = Repository::open(main_repo).map_err(git("open main repository"))?;
        let workdir = main
            .workdir()
            .ok_or(TransactionError::BareRepository)?
            .to_path_buf();

        if main.find_worktree(task_id).is_ok() || worktree_path(&workdir, task_id).exists() {
            return Err(TransactionError::AlreadyExists(task_id.to_owned()));
        }
        exclude_tiller_dir(&main)?;

        let head_ref = main.head().map_err(|_| TransactionError::NoHeadCommit)?;
        let target_branch = if main.head_detached().map_err(git("check main HEAD state"))? {
            return Err(TransactionError::MainDetachedHead);
        } else {
            head_ref
                .shorthand()
                .map(str::to_owned)
                .ok_or(TransactionError::MainDetachedHead)?
        };
        let head_oid = head_ref.target().ok_or(TransactionError::NoHeadCommit)?;
        let head_commit = main
            .find_commit(head_oid)
            .map_err(git("find HEAD commit"))?;

        let branch = main
            .branch(&branch_name(task_id), &head_commit, false)
            .map_err(|e| {
                if e.code() == ErrorCode::Exists {
                    TransactionError::AlreadyExists(task_id.to_owned())
                } else {
                    TransactionError::Git {
                        op: "create task branch",
                        source: e,
                    }
                }
            })?;
        let reference = branch.into_reference();

        let path = worktree_path(&workdir, task_id);
        fs::create_dir_all(path.parent().expect("worktree path has a parent"))
            .map_err(io("create .tiller/worktrees"))?;

        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        if let Err(e) = main.worktree(task_id, &path, Some(&opts)) {
            // don't leave an orphan branch behind
            let _ = main
                .find_branch(&branch_name(task_id), BranchType::Local)
                .and_then(|mut b| b.delete());
            return Err(TransactionError::Git {
                op: "git_worktree_add",
                source: e,
            });
        }

        set_config_str(&main, &cfg_key_base(task_id), &head_oid.to_string())?;
        set_config_str(&main, &cfg_key_target_branch(task_id), &target_branch)?;

        let repo = Repository::open(&path).map_err(git("open task worktree"))?;
        Ok(Self {
            repo,
            main_path: workdir,
            task_id: task_id.to_owned(),
            target_branch,
            base_commit: head_oid,
            approval: None,
        })
    }

    /// Reopen an existing workspace by task id (base and approval are
    /// restored from the repo-local config).
    pub fn open(main_repo: &Path, task_id: &str) -> Result<Self, TransactionError> {
        validate_task_id(task_id)?;
        let main = Repository::open(main_repo).map_err(git("open main repository"))?;
        let workdir = main
            .workdir()
            .ok_or(TransactionError::BareRepository)?
            .to_path_buf();

        let unknown = || TransactionError::UnknownTask(task_id.to_owned());
        let path = worktree_path(&workdir, task_id);
        if !path.is_dir() {
            return Err(unknown());
        }
        let base =
            Oid::from_str(&read_config_str(&main, &cfg_key_base(task_id)).ok_or_else(unknown)?)
                .map_err(git("parse base oid"))?;
        let target_branch =
            read_config_str(&main, &cfg_key_target_branch(task_id)).ok_or_else(unknown)?;
        if main
            .find_branch(&branch_name(task_id), BranchType::Local)
            .is_err()
        {
            return Err(unknown());
        }

        let approval = read_config_str(&main, &cfg_key_approval_candidate(task_id))
            .and_then(|oid| Oid::from_str(&oid).ok())
            .zip(read_config_str(&main, &cfg_key_approval_at(task_id)))
            .and_then(|(candidate, at)| {
                at.parse::<u64>().ok().map(|approved_at| Approval {
                    candidate,
                    approved_at,
                })
            });

        let repo = Repository::open(&path).map_err(git("open task worktree"))?;
        Ok(Self {
            repo,
            main_path: workdir,
            task_id: task_id.to_owned(),
            target_branch,
            base_commit: base,
            approval,
        })
    }

    /// List all known task worktrees (entries whose worktree dir is gone —
    /// e.g. removed outside Tiller — are skipped; call [`Self::discard`] to
    /// clean up properly).
    pub fn list(main_repo: &Path) -> Result<Vec<WorktreeEntry>, TransactionError> {
        let main = Repository::open(main_repo).map_err(git("open main repository"))?;
        let workdir = main.workdir().ok_or(TransactionError::BareRepository)?;
        let root = workdir.join(TILLER_DIR).join(WORKTREES_SUBDIR);
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&root).map_err(io("read .tiller/worktrees"))? {
            let entry = entry.map_err(io("read .tiller/worktrees entry"))?;
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(task_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_task_id(&task_id).is_err() {
                continue;
            }
            let Some(base) = read_config_str(&main, &cfg_key_base(&task_id))
                .and_then(|oid| Oid::from_str(&oid).ok())
            else {
                continue; // no recorded base → not a (consistent) Tiller task
            };
            entries.push(WorktreeEntry {
                task_id,
                path: entry.path(),
                base_commit: base,
            });
        }
        entries.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        Ok(entries)
    }

    /// Remove the task completely: worktree + branch + persisted metadata.
    /// (No keep-branch option on purpose: a transaction is either merged
    /// before removal or its history is intentionally dropped; half-removed
    /// state is the thing this library exists to prevent.)
    pub fn discard(self) -> Result<(), TransactionError> {
        let Self {
            repo,
            main_path,
            task_id,
            ..
        } = self;
        drop(repo); // close the worktree handle before deleting on Windows
        let main = Repository::open(&main_path).map_err(git("open main repository"))?;
        let workdir = main.workdir().ok_or(TransactionError::BareRepository)?;

        let path = worktree_path(workdir, &task_id);
        if path.exists() {
            fs::remove_dir_all(&path).map_err(io("remove worktree directory"))?;
        }
        if let Ok(worktree) = main.find_worktree(&task_id) {
            worktree.prune(None).map_err(git("prune worktree"))?;
        }
        if let Ok(mut branch) = main.find_branch(&branch_name(&task_id), BranchType::Local) {
            branch.delete().map_err(git("delete task branch"))?;
        }
        // Metadata cleanup failures are surfaced, not swallowed: a half-
        // cleaned config (task reappearing in `list`, stale base on `open`)
        // is exactly the inconsistent state this library exists to prevent.
        // Absent keys are fine (e.g. no approval was ever recorded).
        let mut cfg = main.config().map_err(git("config open"))?;
        for key in [
            cfg_key_base(&task_id),
            cfg_key_target_branch(&task_id),
            cfg_key_approval_candidate(&task_id),
            cfg_key_approval_at(&task_id),
        ] {
            match cfg.remove(&key) {
                Ok(()) => {}
                Err(e) if e.code() == ErrorCode::NotFound => {}
                Err(e) => {
                    return Err(TransactionError::Git {
                        op: "config remove",
                        source: e,
                    })
                }
            }
        }
        Ok(())
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// The task branch name (`tiller/task-{task_id}`).
    pub fn branch(&self) -> String {
        branch_name(&self.task_id)
    }

    /// The worktree's working directory.
    pub fn path(&self) -> &Path {
        self.repo
            .workdir()
            .expect("worktree repository has a workdir")
    }

    /// The main-repo branch pinned at create time: the transaction's merge
    /// target. Freshness (`approval_state`), `merge` and `rebase` all refer
    /// to this branch, not to the main worktree's current checkout.
    pub fn target_branch(&self) -> &str {
        &self.target_branch
    }

    /// The commit the pinned target branch pointed at when the task was
    /// created (updated by [`Self::rebase_onto_new_base`]).
    pub fn base_commit(&self) -> Oid {
        self.base_commit
    }

    /// The recorded approval, if any.
    pub fn approval(&self) -> Option<&Approval> {
        self.approval.as_ref()
    }

    /// The worktree's repository handle (HEAD = task branch).
    pub fn repository(&self) -> &Repository {
        &self.repo
    }

    fn main_repo(&self) -> Result<Repository, TransactionError> {
        Repository::open(&self.main_path).map_err(git("open main repository"))
    }

    /// Tip of the pinned target branch — the authority for base freshness
    /// and the landing ref for `merge`. Deliberately NOT the main worktree's
    /// current HEAD: the operator may have checked out another branch in the
    /// meantime (P1-2).
    fn pinned_tip(&self, main: &Repository) -> Result<Oid, TransactionError> {
        let branch = main
            .find_branch(&self.target_branch, BranchType::Local)
            .map_err(git("find target branch"))?;
        branch.get().target().ok_or_else(|| TransactionError::Git {
            op: "target branch points nowhere",
            source: git2::Error::from_str(&format!(
                "target branch `{}` has no direct commit target",
                self.target_branch
            )),
        })
    }

    /// Is the main worktree currently checked out on the pinned target
    /// branch (the only case where `merge` touches the main worktree)?
    fn main_worktree_on_target(&self, main: &Repository) -> Result<bool, TransactionError> {
        if main.head_detached().map_err(git("check main HEAD state"))? {
            return Ok(false);
        }
        let head_branch = main
            .head()
            .map_err(git("resolve main HEAD"))?
            .shorthand()
            .map(str::to_owned);
        Ok(head_branch.as_deref() == Some(self.target_branch.as_str()))
    }

    fn tip_commit(&self) -> Result<git2::Commit<'_>, TransactionError> {
        self.repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(git("resolve task branch tip"))
    }

    /// Changed and untracked files in the worktree (`.tiller/` is ignored
    /// repo-wide; `.gitignore` is respected).
    pub fn status(&self) -> Result<WorktreeStatus, TransactionError> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .exclude_submodules(true);
        let statuses = self.repo.statuses(Some(&mut opts)).map_err(git("status"))?;

        let mut changed = BTreeSet::new();
        let mut untracked = BTreeSet::new();
        let dirty = Status::all() - Status::WT_NEW - Status::IGNORED;
        for entry in statuses.iter() {
            let Some(path) = entry.path() else { continue };
            let flags = entry.status();
            if flags.contains(Status::WT_NEW) {
                untracked.insert(path.to_owned());
            } else if flags.intersects(dirty) {
                changed.insert(path.to_owned());
            }
        }
        Ok(WorktreeStatus {
            changed: changed.into_iter().collect(),
            untracked: untracked.into_iter().collect(),
        })
    }

    /// Commit *everything* — modified, staged, deleted and untracked files
    /// alike (`.gitignore` respected) — as a new checkpoint on the task
    /// branch. Author is `tiller`; the message gets a bookkeeping trailer
    /// with the wall-clock timestamp. Returns the new commit id.
    pub fn checkpoint(&mut self, message: &str) -> Result<Oid, TransactionError> {
        let status = self.status()?;
        if status.changed.is_empty() && status.untracked.is_empty() {
            return Err(TransactionError::NothingToCommit);
        }

        let mut index = self.repo.index().map_err(git("open index"))?;
        // `add_all` with the default pathspec is `git add -A`: stages new,
        // modified and deleted paths while honouring ignore rules. Deletion
        // staging included — verified against the vendored libgit2 for both
        // base-tracked and checkpoint-created files by
        // `checkpoint_removes_deleted_files_from_tree` (P5 review doubted
        // this; it holds, the test pins it).
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .map_err(git("index add_all"))?;
        index.write().map_err(git("index write"))?;
        let tree_id = index.write_tree().map_err(git("write tree"))?;
        let tree = self.repo.find_tree(tree_id).map_err(git("find tree"))?;

        let parent = self.tip_commit()?;
        let sig = tiller_signature()?;
        let full_message = format!("{message}\n\n[tiller checkpoint {}]", now_unix_ms());
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, &full_message, &tree, &[&parent])
            .map_err(git("commit checkpoint"))?;
        Ok(oid)
    }

    /// Checkpoints on the task branch since the base, oldest first.
    pub fn checkpoints(&self) -> Result<Vec<CheckpointInfo>, TransactionError> {
        let mut out = Vec::new();
        let mut current = self.tip_commit()?;
        while current.id() != self.base_commit {
            out.push(CheckpointInfo {
                oid: current.id(),
                ts: current.time().seconds().max(0) as u64,
                message: strip_checkpoint_trailer(current.message().unwrap_or_default()),
            });
            current = current.parent(0).map_err(git("walk checkpoint history"))?;
        }
        out.reverse();
        Ok(out)
    }

    /// The current review candidate: an immutable snapshot of the task-branch
    /// tip plus the base it was written against.
    pub fn candidate(&self) -> Result<ReviewCandidate, TransactionError> {
        let tip = self.tip_commit()?;
        if tip.id() == self.base_commit {
            return Err(TransactionError::NoCheckpoints);
        }
        Ok(ReviewCandidate {
            task_id: self.task_id.clone(),
            candidate_commit: tip.id(),
            base_commit: self.base_commit,
            created_at: now_unix_ms(),
        })
    }

    /// Linear diff of the review candidate against its base
    /// (base tree → candidate tree, unified, context 3), capped and
    /// filterable. **Pure object-database read**: it never touches the
    /// worktree, so it is safe (and meaningful) even while the worktree is
    /// dirty — uncommitted changes are simply not part of the candidate.
    ///
    /// `path` filters to a single pathspec (use it to refine a truncated
    /// response); `None` diffs everything. Renames are reported when
    /// libgit2's default rename detection finds them (path `"old -> new"`,
    /// [`ChangeType::Renamed`]) — no similarity tuning. Binary files ship
    /// without content (`binary: true`, empty patch). Byte caps and the
    /// `… truncated …` marker: see [`CandidateDiff`]. Without checkpoints
    /// there is no candidate to diff ([`TransactionError::NoCheckpoints`]).
    pub fn candidate_diff(&self, path: Option<&str>) -> Result<CandidateDiff, TransactionError> {
        let tip = self.tip_commit()?;
        if tip.id() == self.base_commit {
            return Err(TransactionError::NoCheckpoints);
        }
        let base_tree = self
            .repo
            .find_commit(self.base_commit)
            .and_then(|c| c.tree())
            .map_err(git("base tree"))?;
        let tip_tree = tip.tree().map_err(git("candidate tree"))?;

        let mut opts = git2::DiffOptions::new();
        opts.context_lines(3);
        if let Some(path) = path {
            opts.pathspec(path);
        }
        let mut diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&tip_tree), Some(&mut opts))
            .map_err(git("diff base..candidate"))?;
        // Default rename detection; a failure just leaves deltas un-renamed,
        // which is a valid (if less pretty) answer.
        let _ = diff.find_similar(None);

        // One bucket per file, opened on its first header line; later
        // header lines of the same delta (`index …`, `---`, `+++`) and all
        // content lines append to the open bucket. Binary deltas keep the
        // bucket (for `binary: true`) but never collect content.
        //
        // Budgets are enforced DURING the walk (P6 review MAJOR-2): a
        // file's lines are collected only while they fit its allowance
        // (per-file cap ∩ remaining total cap), and once the total cap is
        // drained the walk is aborted outright — full patches are never
        // materialised just to be cut afterwards. The header lines stay
        // in the shipped patch (a real unified patch needs
        // `diff --git`/`---`/`+++`, P6 review MAJOR-3) and count against
        // the allowance like any other line.
        struct OpenFile {
            path: String,
            change_type: ChangeType,
            binary: bool,
            text: String,
            /// Bytes this file may still ship (per-file ∩ remaining total).
            allowance: usize,
            /// Cap hit mid-file: stop collecting, append the truncation
            /// marker when the bucket is finalized.
            cut: bool,
        }

        /// Charge `used` (the marker ships, so it counts), append the
        /// marker when cut, push the entry.
        fn finalize(
            open: OpenFile,
            files: &mut Vec<FileDiff>,
            used: &mut usize,
            truncated: &mut bool,
        ) {
            if open.binary {
                files.push(FileDiff {
                    path: open.path,
                    change_type: open.change_type,
                    patch: String::new(),
                    binary: true,
                    truncated: false,
                });
                return;
            }
            let patch = if !open.cut || open.text.is_empty() {
                // Full patch — or a cap so small that not even one line fit:
                // metadata only, no marker noise (the `truncated` flag
                // carries the information).
                open.text
            } else if open.text.ends_with('\n') {
                format!("{}{}", open.text, DIFF_TRUNCATION_MARKER)
            } else {
                format!("{}\n{}", open.text, DIFF_TRUNCATION_MARKER)
            };
            *truncated |= open.cut;
            *used += patch.len();
            files.push(FileDiff {
                path: open.path,
                change_type: open.change_type,
                patch,
                binary: false,
                truncated: open.cut,
            });
        }

        let mut files: Vec<FileDiff> = Vec::new();
        let mut used = 0usize;
        let mut truncated = false;
        let mut open: Option<OpenFile> = None;
        // Deltas that already got a full entry (content or binary); the
        // metadata-only tail below starts after them.
        let mut pushed = 0usize;
        let mut aborted = false;

        let walk = diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            if line.origin_value() == git2::DiffLineType::FileHeader {
                let path = display_path(&delta);
                // Distinct display paths per delta tell "first header line
                // of a new file" (open a bucket) from the continuation
                // header lines of the file being walked (fall through to
                // the shared append below).
                if open.as_ref().is_none_or(|f| f.path != path) {
                    if let Some(done) = open.take() {
                        finalize(done, &mut files, &mut used, &mut truncated);
                        pushed += 1;
                    }
                    let allowance =
                        DIFF_FILE_BYTE_CAP.min(DIFF_TOTAL_BYTE_CAP.saturating_sub(used));
                    if allowance == 0 {
                        aborted = true;
                        return false; // total cap drained: stop the walk here
                    }
                    open = Some(OpenFile {
                        path,
                        change_type: change_type(&delta),
                        binary: delta.flags().is_binary(),
                        text: String::new(),
                        allowance,
                        cut: false,
                    });
                }
            }
            let Some(file) = open.as_mut() else {
                return true; // content before any header: nothing to attach to
            };
            if file.binary || file.cut {
                return true; // binary never ships; a cut file stops collecting
            }
            // `content()` carries no +/-/space prefix; rebuild it from the
            // line origin so the patch is a real unified diff ('='/<'/'>'
            // are the "\ No newline" lines, 'F' the header lines — all
            // arrive as-is, without a prefix).
            let prefix = match line.origin() {
                '+' => "+",
                '-' => "-",
                ' ' => " ",
                _ => "",
            };
            let mut piece = String::with_capacity(prefix.len() + line.content().len());
            piece.push_str(prefix);
            piece.push_str(&String::from_utf8_lossy(line.content()));
            if file.text.len() + piece.len() > file.allowance {
                file.cut = true; // this line would cross the allowance
            } else {
                file.text.push_str(&piece);
            }
            true
        });
        match walk {
            Ok(()) => {}
            // GIT_EUSER: the callback returned false — our own budget stop.
            Err(e) if e.code() == ErrorCode::User => {}
            Err(e) => {
                return Err(TransactionError::Git {
                    op: "print patch",
                    source: e,
                })
            }
        }
        if let Some(done) = open.take() {
            finalize(done, &mut files, &mut used, &mut truncated);
            pushed += 1;
        }
        if aborted {
            truncated = true;
            // Deltas past the aborted walk never got content — keep their
            // metadata (path, change type, `truncated: true`) so the file
            // list stays complete; nothing else ships for them.
            for delta in diff.deltas().skip(pushed) {
                files.push(FileDiff {
                    path: display_path(&delta),
                    change_type: change_type(&delta),
                    patch: String::new(),
                    binary: false,
                    truncated: true,
                });
            }
        }
        Ok(CandidateDiff {
            files,
            truncated,
            total_bytes: used,
            byte_cap: DIFF_TOTAL_BYTE_CAP,
        })
    }

    /// Record an approval of `candidate`. The approval is persisted with the
    /// workspace (repo-local config) and invalidated automatically as soon as
    /// the candidate or the base moves — see [`Self::approval_state`].
    pub fn approve(&mut self, candidate: Oid) -> Result<Approval, TransactionError> {
        let approval = Approval {
            candidate,
            approved_at: now_unix_ms(),
        };
        let main = self.main_repo()?;
        set_config_str(
            &main,
            &cfg_key_approval_candidate(&self.task_id),
            &candidate.to_string(),
        )?;
        set_config_str(
            &main,
            &cfg_key_approval_at(&self.task_id),
            &approval.approved_at.to_string(),
        )?;
        self.approval = Some(approval.clone());
        Ok(approval)
    }

    /// Valid ⇔ the approved candidate is still the task-branch tip AND the
    /// pinned target branch still points at exactly the base commit —
    /// nothing landed on it since the task was created. New checkpoint after
    /// approval → [`ApprovalState::StaleCandidate`]; any commit on the
    /// target branch → [`ApprovalState::BaseMoved`] (rebase first, then
    /// re-approve). The main worktree's *current checkout* is irrelevant
    /// here: freshness is judged against the pinned branch (P1-2), so
    /// checking out another branch in the main worktree neither validates
    /// nor invalidates an approval.
    ///
    /// Strict equality rather than plain ancestry on purpose: a linear
    /// commit on top of the base keeps the base an ancestor, yet merging
    /// then would silently skip the rebase-verify step the brief mandates
    /// ("merge względem aktualnej bazy").
    pub fn approval_state(&self) -> Result<ApprovalState, TransactionError> {
        let approval = self.approval.as_ref().ok_or(TransactionError::NoApproval)?;
        let tip = self.tip_commit()?;
        if approval.candidate != tip.id() {
            return Ok(ApprovalState::StaleCandidate);
        }
        let main = self.main_repo()?;
        let pinned_tip = self.pinned_tip(&main)?;
        Ok(if pinned_tip == self.base_commit {
            ApprovalState::Valid
        } else {
            ApprovalState::BaseMoved
        })
    }

    /// Merge the approved candidate onto the **pinned target branch** (the
    /// main-repo branch current at [`Self::create`] time; see
    /// [`Self::target_branch`]).
    ///
    /// * Precondition: `approval` matches the recorded approval and its
    ///   candidate is still the branch tip — otherwise
    ///   [`TransactionError::StaleApproval`].
    /// * If the pinned branch moved past the base (its tip ≠ base): **no
    ///   merge**, the outcome is [`MergeOutcome::NeedsRebase`] — rebase
    ///   consciously, then approve the new candidate and merge again.
    /// * Otherwise: a merge commit (`--no-ff`, two parents) whose first
    ///   parent is the pinned branch tip, written to the pinned branch ref.
    ///
    /// Main worktree handling: the merge is allowed even when the main
    /// worktree is checked out on a *different* branch (or detached) — it
    /// only advances the pinned branch ref and cannot clobber anything the
    /// main worktree holds; the outcome reports
    /// `main_worktree_updated: false` in that case. When the main worktree
    /// IS on the pinned branch, it must be clean
    /// ([`TransactionError::MainWorktreeDirty`] otherwise) and is checked
    /// out to the merge result (`main_worktree_updated: true`).
    ///
    /// Ordering (failure states, P2-1): the merge tree is computed in
    /// memory and the merge commit object is written *without* any ref
    /// update first; only then does the pinned branch ref move, and only
    /// after that is the main worktree checked out. Consequences:
    ///
    /// * a conflict or tree-write failure leaves **no** trace (unreferenced
    ///   objects only);
    /// * a ref-update failure leaves the pinned branch unmoved — only an
    ///   unreferenced commit object;
    /// * a `checkout_head` failure after the ref moved is the one residual
    ///   window: the merge *has landed* on the pinned branch while the main
    ///   worktree still holds pre-merge content, which then shows as
    ///   uncommitted changes there. Recovery: `git checkout -- .` in the
    ///   main worktree (or re-run merge after cleaning). git2 offers no
    ///   fault injection to test this window; it is kept as small as the
    ///   operation order allows and documented here.
    ///
    /// `--no-ff` on purpose: one merge commit = one reviewed slice in the
    /// history; log/blame navigation (AGENTS.md "historia = nawigacja") sees
    /// slice boundaries instead of interleaved linear history.
    ///
    /// After a successful merge the workspace is *not* removed; the caller
    /// decides (usually `discard`). Merging again without removing would
    /// report [`MergeOutcome::NeedsRebase`].
    pub fn merge(&self, approval: &Approval) -> Result<MergeOutcome, TransactionError> {
        let stored = self
            .approval
            .as_ref()
            .ok_or(TransactionError::StaleApproval(
                ApprovalState::StaleCandidate,
            ))?;
        if stored.candidate != approval.candidate {
            return Err(TransactionError::StaleApproval(
                ApprovalState::StaleCandidate,
            ));
        }
        let tip = self.tip_commit()?;
        if approval.candidate != tip.id() {
            return Err(TransactionError::StaleApproval(
                ApprovalState::StaleCandidate,
            ));
        }

        let main = self.main_repo()?;
        let pinned_tip = self.pinned_tip(&main)?;

        // Moved base (pinned branch tip ≠ base)? Report, don't merge over
        // it — rebase is a conscious operation.
        if pinned_tip != self.base_commit {
            return Ok(MergeOutcome::NeedsRebase {
                new_base: pinned_tip,
            });
        }

        // The main worktree is only touched when it sits on the pinned
        // branch; then it must be clean so the checkout can't clobber it.
        let main_on_pinned = self.main_worktree_on_target(&main)?;
        if main_on_pinned && !main_is_clean(&main)? {
            return Err(TransactionError::MainWorktreeDirty);
        }

        // Merge commit (--no-ff): base-tree 3-way merge of pinned tip ×
        // candidate (equal trees here — freshness pinned them together).
        let base_tree = main
            .find_commit(self.base_commit)
            .and_then(|c| c.tree())
            .map_err(git("base tree"))?;
        let ours_tree = main
            .find_commit(pinned_tip)
            .and_then(|c| c.tree())
            .map_err(git("target tree"))?;
        let theirs_tree = tip.tree().map_err(git("candidate tree"))?;
        let mut merge_index = main
            .merge_trees(
                &base_tree,
                &ours_tree,
                &theirs_tree,
                Some(&MergeOptions::new()),
            )
            .map_err(git("merge_trees"))?;
        if merge_index.has_conflicts() {
            // Unreachable while base == merge-base (candidate descends from
            // base); kept as a hard stop so it can never silently fast-path.
            return Err(TransactionError::RebaseConflict { paths: Vec::new() });
        }
        let tree_id = merge_index
            .write_tree_to(&main)
            .map_err(git("write merge tree"))?;
        let tree = main.find_tree(tree_id).map_err(git("find merge tree"))?;

        let sig = tiller_signature()?;
        let message = format!(
            "tiller: merge task '{}' (candidate {})",
            self.task_id,
            tip.id()
        );
        // Re-find the commits on the main handle: objects loaded through
        // the worktree repository are foreign to `main` and libgit2 silently
        // drops them from the parent list.
        let ours_on_main = main
            .find_commit(pinned_tip)
            .map_err(git("find target tip on main"))?;
        let tip_on_main = main
            .find_commit(tip.id())
            .map_err(git("find candidate on main"))?;
        // Commit object first, ref updates only afterwards (see doc comment):
        // every step above can still fail with zero visible side effects.
        let merge_commit = main
            .commit(
                None,
                &sig,
                &sig,
                &message,
                &tree,
                &[&ours_on_main, &tip_on_main],
            )
            .map_err(git("commit merge"))?;

        // Land on the pinned branch ref — this is the point of no return.
        main.reference(
            &format!("refs/heads/{}", self.target_branch),
            merge_commit,
            true,
            &format!("tiller: merge task '{}'", self.task_id),
        )
        .map_err(git("update target branch"))?;

        if main_on_pinned {
            main.checkout_head(Some(&mut CheckoutBuilder::new().force()))
                .map_err(git("checkout main after merge"))?;
        }
        Ok(MergeOutcome::Merged {
            merge_commit,
            main_worktree_updated: main_on_pinned,
        })
    }

    /// Rebase the task branch's checkpoints onto the current tip of the
    /// pinned target branch and update the workspace base. The worktree is
    /// hard-reset to the rebased tip. New commit ids ⇒ the previous
    /// candidate and its approval are stale ([`ApprovalState::StaleCandidate`])
    /// — merging requires a fresh approval of the new candidate.
    ///
    /// No-op when the pinned branch hasn't moved. On conflict:
    /// [`TransactionError::RebaseConflict`] with the conflicted paths; the
    /// library never resolves conflicts, and the branch/worktree stay on the
    /// pre-rebase state.
    ///
    /// Precondition (P1-3): the worktree must carry **no uncommitted tracked
    /// changes** (modified/staged/deleted — all of them would be destroyed
    /// by the hard reset) and **no untracked file that exists in the rebased
    /// tree** (the reset would clobber it). Non-colliding untracked files are
    /// fine and survive the rebase. Violations → [`TransactionError::DirtyWorktree`]
    /// listing the offending paths; nothing has been touched at that point.
    ///
    /// Ordering (failure states, P2-1): the rebase itself runs in memory —
    /// a conflict leaves zero trace. The landing sequence then is: write the
    /// new base to the repo config *first*, then hard-reset branch + worktree.
    /// If the reset fails, the config write is rolled back (best-effort
    /// restore of the previous base — it just succeeded, so failure there is
    /// practically impossible); if even the rollback failed, the residual
    /// state would be "config base ahead of the branch": `open()` restores
    /// the new base while the branch is still pre-rebase — recoverable by
    /// resetting the task branch manually. git2 has no fault injection to
    /// exercise this window in a test; the order keeps it as small as
    /// possible and this comment documents it.
    pub fn rebase_onto_new_base(&mut self) -> Result<(), TransactionError> {
        let main = self.main_repo()?;
        let pinned_tip = self.pinned_tip(&main)?;
        if pinned_tip == self.base_commit {
            return Ok(());
        }

        // P1-3: refuse before touching anything if tracked work is dirty —
        // the final hard reset would silently destroy it.
        let status = self.status()?;
        if !status.changed.is_empty() {
            return Err(TransactionError::DirtyWorktree {
                details: status
                    .changed
                    .into_iter()
                    .map(|p| format!("uncommitted: {p}"))
                    .collect(),
            });
        }

        let tip_id = self.tip_commit()?.id();

        // In-memory rebase: replay base..tip onto the pinned tip without
        // touching the worktree; we move the branch + reset only on full
        // success.
        let branch = main
            .find_annotated_commit(tip_id)
            .map_err(git("annotated commit (branch)"))?;
        let upstream = main
            .find_annotated_commit(self.base_commit)
            .map_err(git("annotated commit (upstream)"))?;
        let onto = main
            .find_annotated_commit(pinned_tip)
            .map_err(git("annotated commit (onto)"))?;
        let mut opts = RebaseOptions::new();
        opts.inmemory(true);
        let mut rebase = main
            .rebase(Some(&branch), Some(&upstream), Some(&onto), Some(&mut opts))
            .map_err(git("rebase init"))?;
        let committer = tiller_signature()?;

        let mut new_tip = None;
        while let Some(item) = rebase.next() {
            let op = item.map_err(git("rebase next"))?;
            let op_id = op.id();
            let index = rebase.inmemory_index().map_err(git("rebase index"))?;
            if index.has_conflicts() {
                let conflicts = index.conflicts().map_err(git("rebase conflicts"))?;
                let mut paths = BTreeSet::new();
                for conflict in conflicts {
                    let conflict = conflict.map_err(git("rebase conflict entry"))?;
                    for side in [conflict.ancestor, conflict.our, conflict.their]
                        .into_iter()
                        .flatten()
                    {
                        paths.insert(String::from_utf8_lossy(&side.path).into_owned());
                    }
                }
                return Err(TransactionError::RebaseConflict {
                    paths: paths.into_iter().collect(),
                });
            }
            let original = main.find_commit(op_id).map_err(git("find rebase commit"))?;
            let author = original.author();
            let oid = rebase
                .commit(Some(&author), &committer, None)
                .map_err(git("rebase commit"))?;
            new_tip = Some(oid);
        }
        let new_tip = new_tip.unwrap_or(pinned_tip);

        // P1-3: untracked files that exist in the rebased tree would be
        // clobbered by the hard reset — refuse them too. Untracked paths NOT
        // in the new tree are left alone by the reset and stay in place.
        let new_tree = self
            .repo
            .find_commit(new_tip)
            .and_then(|c| c.tree())
            .map_err(git("find rebased tree"))?;
        let colliding: Vec<String> = status
            .untracked
            .iter()
            .filter(|p| new_tree.get_path(Path::new(p.as_str())).is_ok())
            .map(|p| format!("untracked collides with rebased tree: {p}"))
            .collect();
        if !colliding.is_empty() {
            return Err(TransactionError::DirtyWorktree { details: colliding });
        }

        // Branch follows the rebased tip; worktree is hard-reset to it
        // (reset moves the checked-out branch, writes a reflog entry — the
        // pre-rebase checkpoints stay recoverable via reflog until gc).
        // P2-1: config first, reset second, best-effort rollback between.
        set_config_str(&main, &cfg_key_base(&self.task_id), &pinned_tip.to_string())?;
        let new_tip_commit = self
            .repo
            .find_commit(new_tip)
            .map_err(git("find rebased tip"))?;
        if let Err(e) = self
            .repo
            .reset(new_tip_commit.as_object(), ResetType::Hard, None)
        {
            let _ = set_config_str(
                &main,
                &cfg_key_base(&self.task_id),
                &self.base_commit.to_string(),
            );
            return Err(TransactionError::Git {
                op: "hard reset worktree to rebased tip",
                source: e,
            });
        }

        self.base_commit = pinned_tip;
        Ok(())
    }

    /// Soft rollback: hard-reset the task branch to the base and delete
    /// untracked files from the worktree (ignored files stay, like `git
    /// clean` without `-x`). The discarded checkpoints remain recoverable in
    /// the branch reflog until gc — use [`Self::discard`] to remove the task
    /// completely.
    pub fn reset_to_base(&mut self) -> Result<(), TransactionError> {
        let base = self
            .repo
            .find_commit(self.base_commit)
            .map_err(git("find base commit"))?;
        self.repo
            .reset(base.as_object(), ResetType::Hard, None)
            .map_err(git("hard reset to base"))?;
        self.clean_untracked()
    }

    fn clean_untracked(&self) -> Result<(), TransactionError> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let statuses = self.repo.statuses(Some(&mut opts)).map_err(git("status"))?;
        for entry in statuses.iter() {
            if !entry.status().contains(Status::WT_NEW) {
                continue;
            }
            let Some(path) = entry.path() else { continue };
            let file = self.path().join(path);
            if file.is_file() {
                fs::remove_file(&file).map_err(io("remove untracked file"))?;
            }
        }
        Ok(())
    }
}
