//! Tiller git seam: the transactional model under the agent workflow.
//!
//! Core §3 rule 5 (PROJECT_CORE.md): *"Mechanizm transakcyjny jest w rdzeniu
//! (`git/`): worktree per zadanie, commit = checkpoint, niezmienny kandydat
//! review (id zmiany + commit bazowy), merge względem aktualnej bazy"* — one
//! worktree per task, commits as checkpoints, an immutable review candidate
//! (change id + base commit), merge against the current base. Enforcement
//! (gates, who may approve) belongs to the profile; this library only
//! provides the mechanics and refuses unsafe transitions.
//!
//! # Transactional contract
//!
//! A [`TaskWorkspace`] is one task's transaction:
//!
//! 1. **Isolation** — the agent works in `{repo}/.tiller/worktrees/{task_id}`
//!    on branch `tiller/task-{task_id}`; nothing it does is visible in the
//!    main tree until merge. `.tiller/` is excluded via `.git/info/exclude`,
//!    never via the user's `.gitignore`.
//! 2. **Checkpoints** — [`TaskWorkspace::checkpoint`] commits *everything*
//!    (modified, staged, deleted, untracked; `.gitignore` respected) as one
//!    commit on the task branch. A checkpoint is the unit of recovery.
//! 3. **Candidate** — [`TaskWorkspace::candidate`] snapshots the branch tip
//!    together with the base commit the task was created against. Candidates
//!    are immutable values: a new checkpoint yields a different candidate.
//! 4. **Approval + invalidation** — [`TaskWorkspace::approve`] records
//!    approval of one candidate; [`TaskWorkspace::approval_state`] is `Valid`
//!    iff that candidate is still the branch tip **and** the pinned target
//!    branch still points at exactly the base commit. Any new checkpoint →
//!    `StaleCandidate`; any commit landing on the target branch →
//!    `BaseMoved`. Approvals survive reopening (persisted in repo-local
//!    config) — as *data*; a reopened workspace recomputes validity from
//!    the current tips, it never revives a stale approval.
//! 5. **Merge against the current base** — [`TaskWorkspace::merge`] lands
//!    the approved candidate on the **pinned target branch** (the main-repo
//!    branch current at `create` time) as a `--no-ff` merge commit (one
//!    merge = one reviewed slice in history). The pin is deliberate: the
//!    main worktree may check out other branches meanwhile; base freshness
//!    and the merge always refer to the pinned branch, never to whatever
//!    HEAD happens to point at. If the target branch moved past the base
//!    (tip ≠ base) it returns [`MergeOutcome::NeedsRebase`] instead of
//!    merging; rebase is a conscious operation
//!    ([`TaskWorkspace::rebase_onto_new_base`]) whose new commit ids
//!    invalidate any prior approval, so the re-merged candidate must be
//!    approved again. Conflicts are surfaced, never auto-resolved.
//! 6. **Rollback** — [`TaskWorkspace::reset_to_base`] hard-resets the branch
//!    to base and removes untracked files (checkpoints stay recoverable in
//!    the branch reflog until gc); [`TaskWorkspace::discard`] removes the
//!    task entirely (worktree + branch + metadata).
//!
//! Stopping a writer that owns a worktree is orchestration (P6/P7), not this
//! library. One writer per worktree by contract; the type is not
//! synchronised.
//!
//! # Integrity boundary
//!
//! The invariants above (immutability of candidates, freshness checks,
//! pinned merge targets, refusal on dirty worktrees) protect against
//! **mistakes** — racing orchestrators, stale approvals, checkout-then-merge
//! mix-ups. They are *not* a security boundary: everything lives in plain
//! git (refs, reflogs, `.git/config`), and any process with shell access to
//! the repository — the agent itself included — can rewrite refs, forge
//! commits, or delete config keys. Guarding against a hostile process with
//! equal privileges is out of scope here and belongs to a future security
//! profile (filesystem/privilege separation); this library assumes its
//! caller is cooperative and verifies only what git itself can witness.
//!
//! # Decisions
//!
//! - **git2 with vendored libgit2, no openssl/ssh** — operations are strictly
//!   local (worktrees, commits, merge, rebase); this keeps the dependency
//!   tree free of `openssl-sys` and cross-checks clean.
//! - **Merge commit over fast-forward (`--no-ff`)** — slice boundaries stay
//!   visible in history (`git log` navigation per AGENTS.md).
//! - **Pinned target branch (P5 review, P1-2)** — the branch current at
//!   `create` time is recorded (`tiller.worktree.<id>.target-branch`) and is
//!   the sole authority for freshness, rebase destination and merge landing.
//!   A detached main HEAD is refused at `create`: there would be no branch to
//!   pin. Merge is allowed while the main worktree sits on another branch —
//!   it only advances the pinned ref and cannot clobber the main worktree;
//!   the outcome says whether the main worktree was touched.
//! - **Rebase demands a clean tracked worktree (P5 review, P1-3)** — the
//!   hard reset that lands a rebase is destructive, so uncommitted tracked
//!   changes and untracked paths present in the rebased tree are refused
//!   up front; non-colliding untracked files survive.
//! - **`remove` is total (no keep-branch option)** — a transaction is either
//!   merged before removal or intentionally dropped; `reset_to_base` covers
//!   the recoverable middle ground.
//! - **Task metadata lives in repo-local git config**
//!   (`tiller.worktree.<task_id>.*`) — inspectable with `git config`, shared
//!   between the main repo and its worktrees, removed with the task.

mod error;
mod workspace;

pub use error::TransactionError;
pub use workspace::{
    Approval, ApprovalState, CandidateDiff, ChangeType, CheckpointInfo, FileDiff, MergeOutcome,
    ReviewCandidate, TaskWorkspace, WorktreeEntry, WorktreeStatus, DIFF_FILE_BYTE_CAP,
    DIFF_TOTAL_BYTE_CAP, DIFF_TRUNCATION_MARKER,
};
