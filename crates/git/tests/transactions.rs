//! Integration suite for the transactional contract (P5): task worktrees,
//! checkpoints, immutable review candidates, approval invalidation,
//! rebase-aware merge, and rollback — all on real temp repositories, never on
//! files inside this repo. Each test gets a unique temp root, so the suite is
//! collision-free under `cargo test` parallelism.

use std::fs;
use std::path::{Path, PathBuf};

use git2::{Oid, Repository};
use tiller_git::{ApprovalState, MergeOutcome, ReviewCandidate, TaskWorkspace, TransactionError};

// ---------------------------------------------------------------- harness

struct MainRepo {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl MainRepo {
    fn repo(&self) -> Repository {
        Repository::open(&self.path).unwrap()
    }

    fn head(&self) -> Oid {
        self.repo().head().unwrap().target().unwrap()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path.join(rel)).unwrap()
    }
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn commit_all(repo: &Repository, message: &str) -> Oid {
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("test", "test@tiller.local").unwrap();
    let head = repo.head().ok().and_then(|h| h.target());
    let parents: Vec<git2::Commit> = head
        .map(|oid| repo.find_commit(oid).unwrap())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .unwrap()
}

/// A main repo with a base commit containing `a.txt` and `b.txt`.
/// `core.autocrlf=false` pins checkout byte-for-byte so assertions are
/// identical on Windows (libgit2 defaults autocrlf=true there) and Linux.
fn init_main() -> MainRepo {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("core.autocrlf", "false").unwrap();
    }
    write_file(dir.path(), "a.txt", "base-a\n");
    write_file(dir.path(), "b.txt", "base-b\n");
    commit_all(&repo, "base");
    MainRepo {
        _dir: dir,
        path: repo.workdir().unwrap().to_path_buf(),
    }
}

fn worktree_dir(main: &Path, task_id: &str) -> PathBuf {
    main.join(".tiller").join("worktrees").join(task_id)
}

/// Clean check that ignores ignored entries (libgit2's NULL-option status
/// *includes* them, unlike `git status`).
fn is_clean(repo: &Repository) -> bool {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    repo.statuses(Some(&mut opts)).unwrap().is_empty()
}

/// create + one checkpoint touching `a.txt` and adding `c.txt`.
fn task_with_checkpoint(main: &MainRepo, task_id: &str) -> (TaskWorkspace, Oid) {
    let mut ws = TaskWorkspace::create(&main.path, task_id).unwrap();
    let dir = worktree_dir(&main.path, task_id);
    write_file(&dir, "a.txt", "task-a\n");
    write_file(&dir, "c.txt", "task-c\n");
    let cp = ws.checkpoint("first checkpoint").unwrap();
    (ws, cp)
}

// --------------------------------------------------- 1. worktree creation

#[test]
fn create_worktree_branches_from_head_and_excludes_tiller_dir() {
    let main = init_main();
    let ws = TaskWorkspace::create(&main.path, "fix-1").unwrap();

    assert_eq!(ws.task_id(), "fix-1");
    assert_eq!(ws.base_commit(), main.head());
    assert!(worktree_dir(&main.path, "fix-1").is_dir());

    // branch tiller/task-fix-1 exists and points at base
    let repo = main.repo();
    let branch = repo
        .find_branch("tiller/task-fix-1", git2::BranchType::Local)
        .unwrap();
    assert_eq!(branch.get().target().unwrap(), main.head());

    // .tiller/ is excluded via .git/info/exclude (user .gitignore untouched);
    // the exclusion itself shows up as IGNORED under NULL-option status, so
    // assert on non-ignored cleanliness instead
    let exclude = fs::read_to_string(main.path.join(".git/info/exclude")).unwrap();
    assert!(exclude.contains("/.tiller/"));
    assert!(is_clean(&main.repo()));
    let repo = main.repo();
    let raw = repo.statuses(None).unwrap();
    assert!(raw
        .iter()
        .all(|e| e.status().contains(git2::Status::IGNORED)));
}

#[test]
fn create_rejects_invalid_ids_and_duplicates() {
    let main = init_main();
    for bad in ["", "a/b", "..", "a b", "a\\b"] {
        let err = TaskWorkspace::create(&main.path, bad).unwrap_err();
        assert!(
            matches!(err, TransactionError::InvalidTaskId(_)),
            "expected InvalidTaskId for {bad:?}, got {err:?}"
        );
    }
    TaskWorkspace::create(&main.path, "ok_1.x").unwrap();
    let err = TaskWorkspace::create(&main.path, "ok_1.x").unwrap_err();
    assert!(matches!(err, TransactionError::AlreadyExists(_)));
}

// ------------------------------------------------------- 2. list and open

#[test]
fn list_and_reopen_preserve_base() {
    let main = init_main();
    {
        let _ws = TaskWorkspace::create(&main.path, "task-a").unwrap();
    }
    let listed = TaskWorkspace::list(&main.path).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].task_id, "task-a");
    assert_eq!(listed[0].base_commit, main.head());
    assert_eq!(listed[0].path, worktree_dir(&main.path, "task-a"));

    let reopened = TaskWorkspace::open(&main.path, "task-a").unwrap();
    assert_eq!(reopened.base_commit(), main.head());
    // the pinned target branch survives reopen (P1-2)
    let head_branch = main.repo().head().unwrap().shorthand().unwrap().to_owned();
    assert_eq!(reopened.target_branch(), head_branch);
    assert_eq!(reopened.branch(), "tiller/task-task-a");

    let err = TaskWorkspace::open(&main.path, "ghost").unwrap_err();
    assert!(matches!(err, TransactionError::UnknownTask(_)));
}

// --------------------------------------------------- 3. status, checkpoints

#[test]
fn checkpoint_commits_everything_including_untracked_respecting_gitignore() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "cp-all").unwrap();
    let dir = worktree_dir(&main.path, "cp-all");

    // nothing changed yet → no empty checkpoints
    assert!(matches!(
        ws.checkpoint("empty").unwrap_err(),
        TransactionError::NothingToCommit
    ));

    write_file(&dir, "a.txt", "changed\n");
    write_file(&dir, "new/nested.txt", "nested\n");
    write_file(&dir, ".gitignore", "ignored.txt\n");
    write_file(&dir, "ignored.txt", "should not be committed\n");

    let status = ws.status().unwrap();
    assert_eq!(status.changed, vec!["a.txt"]);
    assert!(status
        .untracked
        .iter()
        .any(|p| p == ".gitignore" || p == "new/nested.txt"));
    assert!(status.untracked.iter().all(|p| p != "ignored.txt"));

    let first = ws.checkpoint("first").unwrap();
    let cps = ws.checkpoints().unwrap();
    assert_eq!(cps.len(), 1);
    assert_eq!(cps[0].oid, first);
    assert_eq!(cps[0].message, "first");
    assert!(cps[0].ts > 0);

    // everything tracked landed in the commit; ignored file did not
    {
        let repo = ws.repository();
        let commit = repo.find_commit(first).unwrap();
        let tree = commit.tree().unwrap();
        assert!(tree.get_path(Path::new("new/nested.txt")).is_ok());
        assert!(tree.get_path(Path::new("ignored.txt")).is_err());
        let blob = tree
            .get_path(Path::new("a.txt"))
            .unwrap()
            .to_object(repo)
            .unwrap()
            .peel(git2::ObjectType::Blob)
            .unwrap();
        assert_eq!(blob.as_blob().unwrap().content(), b"changed\n");
    }

    // second checkpoint → ordered history, oldest first
    write_file(&dir, "b.txt", "also changed\n");
    let second = ws.checkpoint("second").unwrap();
    let cps = ws.checkpoints().unwrap();
    assert_eq!(
        cps.iter().map(|c| c.oid).collect::<Vec<_>>(),
        vec![first, second]
    );
    assert_eq!(cps[1].message, "second");
}

/// P1-1: checkpoint must have full `git add -A` semantics — deleting a
/// tracked file and checkpointing must REMOVE it from the committed tree,
/// not just from the working directory.
#[test]
fn checkpoint_removes_deleted_files_from_tree() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "cp-del").unwrap();
    let dir = worktree_dir(&main.path, "cp-del");

    write_file(&dir, "new.txt", "first\n");
    let first = ws.checkpoint("first").unwrap();
    {
        let tree = ws.repository().find_commit(first).unwrap().tree().unwrap();
        assert!(tree.get_path(Path::new("b.txt")).is_ok());
    }

    // delete a tracked file (b.txt comes from the base commit)
    fs::remove_file(dir.join("b.txt")).unwrap();
    let status = ws.status().unwrap();
    assert!(status.changed.iter().any(|p| p == "b.txt"));

    let second = ws.checkpoint("delete b.txt").unwrap();
    {
        let repo = ws.repository();
        let tree = repo.find_commit(second).unwrap().tree().unwrap();
        assert!(
            tree.get_path(Path::new("b.txt")).is_err(),
            "deleted tracked file must be absent from the checkpoint tree"
        );
        assert!(tree.get_path(Path::new("new.txt")).is_ok());
    }

    // same for a file that only ever existed because a *checkpoint* added it
    fs::remove_file(dir.join("new.txt")).unwrap();
    let third = ws.checkpoint("delete new.txt").unwrap();
    {
        let repo = ws.repository();
        let tree = repo.find_commit(third).unwrap().tree().unwrap();
        assert!(tree.get_path(Path::new("new.txt")).is_err());
    }

    // full add -A semantics: after the checkpoint the worktree is clean
    let status = ws.status().unwrap();
    assert!(status.changed.is_empty() && status.untracked.is_empty());
}

#[test]
fn worktree_is_isolated_from_main_tree() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "iso").unwrap();
    let dir = worktree_dir(&main.path, "iso");

    write_file(&dir, "a.txt", "task-only change\n");
    write_file(&dir, "only-in-worktree.txt", "x\n");
    let _ = ws.checkpoint("isolate").unwrap();

    // main worktree and its HEAD are untouched
    assert_eq!(main.read("a.txt"), "base-a\n");
    assert!(!main.path.join("only-in-worktree.txt").exists());
    assert_eq!(main.head(), ws.base_commit());
}

// ------------------------------------------- 4. candidate + approval state

#[test]
fn candidate_is_branch_tip_and_approval_invalidates_on_new_checkpoint() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "review-1");

    let candidate: ReviewCandidate = ws.candidate().unwrap();
    assert_eq!(candidate.task_id, "review-1");
    assert_eq!(candidate.candidate_commit, cp);
    assert_eq!(candidate.base_commit, main.head());
    assert!(candidate.created_at > 0);

    ws.approve(candidate.candidate_commit).unwrap();
    assert!(matches!(ws.approval_state().unwrap(), ApprovalState::Valid));

    // a new checkpoint after approval → StaleCandidate
    write_file(&worktree_dir(&main.path, "review-1"), "a.txt", "task-a-2\n");
    ws.checkpoint("post-approval change").unwrap();
    assert!(matches!(
        ws.approval_state().unwrap(),
        ApprovalState::StaleCandidate
    ));
}

#[test]
fn approval_becomes_base_moved_when_main_advances() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "review-2");
    ws.approve(cp).unwrap();
    assert!(matches!(ws.approval_state().unwrap(), ApprovalState::Valid));

    // base moves: unrelated commit lands on main
    write_file(&main.path, "b.txt", "main moved\n");
    commit_all(&main.repo(), "main advance");

    assert!(matches!(
        ws.approval_state().unwrap(),
        ApprovalState::BaseMoved
    ));
}

#[test]
fn approval_state_requires_an_approval_and_candidate_requires_a_checkpoint() {
    let main = init_main();
    let ws = TaskWorkspace::create(&main.path, "bare").unwrap();

    assert!(matches!(
        ws.approval_state().unwrap_err(),
        TransactionError::NoApproval
    ));
    assert!(matches!(
        ws.candidate().unwrap_err(),
        TransactionError::NoCheckpoints
    ));
}

// ------------------------------------------------------------- 5. merge

#[test]
fn merge_with_valid_approval_creates_merge_commit_and_updates_main_worktree() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "merge-1");
    let approval = ws.approve(cp).unwrap();
    let outcome = ws.merge(&approval).unwrap();
    let MergeOutcome::Merged {
        merge_commit,
        main_worktree_updated,
    } = outcome
    else {
        panic!("expected Merged, got {outcome:?}");
    };
    // main worktree sat on the pinned branch → it was checked out to the
    // merge result
    assert!(main_worktree_updated);

    // merge-commit (--no-ff), not a fast-forward: two parents
    let repo = main.repo();
    let head = repo.find_commit(merge_commit).unwrap();
    assert_eq!(main.head(), merge_commit);
    assert_eq!(head.parent_count(), 2);
    assert_eq!(head.parent_id(0).unwrap(), ws.base_commit());
    assert_eq!(head.parent_id(1).unwrap(), cp);

    // main worktree materialized the merged content
    assert_eq!(main.read("a.txt"), "task-a\n");
    assert_eq!(main.read("c.txt"), "task-c\n");
}

#[test]
fn merge_without_valid_approval_is_rejected() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "merge-stale");
    let approval = ws.approve(cp).unwrap();

    // checkpoint after approval invalidates → merge refuses
    write_file(&worktree_dir(&main.path, "merge-stale"), "a.txt", "more\n");
    ws.checkpoint("sneaky").unwrap();
    match ws.merge(&approval).unwrap_err() {
        TransactionError::StaleApproval(state) => {
            assert!(matches!(state, ApprovalState::StaleCandidate))
        }
        e => panic!("expected StaleApproval, got {e:?}"),
    }
}

#[test]
fn moved_base_requires_rebase_then_reapproval_before_merge() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "moved-1");
    let approval = ws.approve(cp).unwrap();

    write_file(&main.path, "b.txt", "main moved\n");
    commit_all(&main.repo(), "main advance");

    // base moved → merge refuses to merge over it and reports the new base
    // (approval_state() itself reports BaseMoved; the merge precondition is
    // candidate freshness)
    let outcome = ws.merge(&approval).unwrap();
    let MergeOutcome::NeedsRebase { new_base } = outcome else {
        panic!("expected NeedsRebase, got {outcome:?}");
    };
    assert_eq!(new_base, main.head());

    // rebase onto the new base; approval expired (candidate ids changed)
    let new_base = main.head();
    ws.rebase_onto_new_base().unwrap();
    assert_eq!(ws.base_commit(), new_base);
    assert!(matches!(
        ws.approval_state().unwrap(),
        ApprovalState::StaleCandidate
    ));

    // checkpoints survived the rebase, now rooted at the new base
    let cps = ws.checkpoints().unwrap();
    assert_eq!(cps.len(), 1);
    let repo = ws.repository();
    assert_eq!(
        repo.find_commit(cps[0].oid).unwrap().parent_id(0).unwrap(),
        new_base
    );
    // worktree followed the rebase: main's b.txt is visible in the worktree
    assert_eq!(
        fs::read_to_string(worktree_dir(&main.path, "moved-1").join("b.txt")).unwrap(),
        "main moved\n"
    );

    // re-approve the new candidate → merge succeeds
    let candidate = ws.candidate().unwrap();
    let approval = ws.approve(candidate.candidate_commit).unwrap();
    let MergeOutcome::Merged {
        merge_commit,
        main_worktree_updated,
    } = ws.merge(&approval).unwrap()
    else {
        panic!("expected Merged");
    };
    assert!(main_worktree_updated);
    let repo = main.repo();
    let head = repo.find_commit(merge_commit).unwrap();
    assert_eq!(head.parent_id(1).unwrap(), candidate.candidate_commit);
    assert_eq!(main.read("a.txt"), "task-a\n");
    assert_eq!(main.read("b.txt"), "main moved\n");
}

/// P1-3: rebase must refuse to hard-reset over uncommitted tracked work.
#[test]
fn rebase_refuses_uncommitted_tracked_changes() {
    let main = init_main();
    let (mut ws, _cp) = task_with_checkpoint(&main, "rebase-dirty");
    let base_before = ws.base_commit();

    // the pinned branch moves so a rebase would actually run
    write_file(&main.path, "b.txt", "main moved\n");
    commit_all(&main.repo(), "main advance");

    write_file(
        &worktree_dir(&main.path, "rebase-dirty"),
        "a.txt",
        "uncommitted\n",
    );
    match ws.rebase_onto_new_base().unwrap_err() {
        TransactionError::DirtyWorktree { details } => {
            assert!(
                details.iter().any(|d| d.ends_with("a.txt")),
                "expected a.txt in {details:?}"
            );
        }
        e => panic!("expected DirtyWorktree, got {e:?}"),
    }

    // refusal happened before anything destructive: the modification, the
    // branch, the base — in memory AND persisted — all survive untouched
    // (P2-1: checks precede the config write and the reset entirely)
    let dir = worktree_dir(&main.path, "rebase-dirty");
    assert_eq!(
        fs::read_to_string(dir.join("a.txt")).unwrap(),
        "uncommitted\n"
    );
    assert_eq!(ws.base_commit(), base_before);
    assert_eq!(ws.checkpoints().unwrap().len(), 1);
    let persisted_base = main
        .repo()
        .config()
        .unwrap()
        .get_string("tiller.worktree.rebase-dirty.base")
        .unwrap();
    assert_eq!(persisted_base, base_before.to_string());
}

/// P1-3: untracked files that exist in the rebased tree would be clobbered
/// by the hard reset — refuse them too.
#[test]
fn rebase_refuses_untracked_files_colliding_with_rebased_tree() {
    let main = init_main();
    let (mut ws, _cp) = task_with_checkpoint(&main, "rebase-collide");
    let base_before = ws.base_commit();

    // main introduces d.txt; the rebased tree will contain it
    write_file(&main.path, "d.txt", "from main\n");
    commit_all(&main.repo(), "main adds d.txt");

    // untracked d.txt in the task worktree collides with that tree
    let dir = worktree_dir(&main.path, "rebase-collide");
    write_file(&dir, "d.txt", "local scratch\n");
    match ws.rebase_onto_new_base().unwrap_err() {
        TransactionError::DirtyWorktree { details } => {
            assert!(
                details.iter().any(|d| d.ends_with("d.txt")),
                "expected d.txt in {details:?}"
            );
        }
        e => panic!("expected DirtyWorktree, got {e:?}"),
    }

    // the colliding file survived the refusal
    assert_eq!(
        fs::read_to_string(dir.join("d.txt")).unwrap(),
        "local scratch\n"
    );
    assert_eq!(ws.base_commit(), base_before);
}

/// P1-3: a clean worktree with non-colliding untracked files rebases fine
/// and the untracked files survive the hard reset.
#[test]
fn rebase_allows_noncolliding_untracked_files() {
    let main = init_main();
    let (mut ws, _cp) = task_with_checkpoint(&main, "rebase-clean");

    write_file(&main.path, "b.txt", "main moved\n");
    commit_all(&main.repo(), "main advance");

    let dir = worktree_dir(&main.path, "rebase-clean");
    write_file(&dir, "scratch/notes.txt", "keep me\n");
    ws.rebase_onto_new_base().unwrap();

    assert_eq!(
        fs::read_to_string(dir.join("scratch/notes.txt")).unwrap(),
        "keep me\n"
    );
    // checkpoints were replayed onto the new base
    assert_eq!(ws.checkpoints().unwrap().len(), 1);
    assert_eq!(ws.base_commit(), main.head());
}

#[test]
fn rebase_conflict_reports_conflicting_paths() {
    let main = init_main();
    let (mut ws, _cp) = task_with_checkpoint(&main, "conflict");

    // main edits the same line the task edited → genuine 3-way conflict
    write_file(&main.path, "a.txt", "main-wins?\n");
    commit_all(&main.repo(), "main conflicting edit");

    match ws.rebase_onto_new_base().unwrap_err() {
        TransactionError::RebaseConflict { paths } => {
            assert_eq!(paths, vec!["a.txt"]);
        }
        e => panic!("expected RebaseConflict, got {e:?}"),
    }
    // branch and worktree stay on the pre-rebase state
    assert_eq!(ws.checkpoints().unwrap().len(), 1);
}

/// P1-2: base/freshness/merge target is the branch pinned at create time —
/// not whatever the main worktree happens to have checked out later.
#[test]
fn merge_lands_on_pinned_branch_not_current_head() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "pinned-1");
    let approval = ws.approve(cp).unwrap();

    // remember the branch the task was created against
    let repo = main.repo();
    let pinned = repo.head().unwrap().shorthand().unwrap().to_owned();
    let pinned_before = main.head();
    assert_eq!(ws.target_branch(), pinned);

    // main worktree switches to another branch at the SAME commit
    let head_commit = repo.find_commit(main.head()).unwrap();
    repo.branch("side", &head_commit, false).unwrap();
    repo.set_head("refs/heads/side").unwrap();

    // freshness is judged against the pinned branch: still Valid here
    assert!(matches!(ws.approval_state().unwrap(), ApprovalState::Valid));

    // a commit on the merely-checked-out branch does not move the base
    write_file(&main.path, "a.txt", "side edit\n");
    commit_all(&repo, "work on side");
    assert!(matches!(ws.approval_state().unwrap(), ApprovalState::Valid));
    let side_tip = main.head();

    // merge: lands on the PINNED branch ref; the main worktree (on `side`,
    // dirty by normal standards) is deliberately untouched
    let outcome = ws.merge(&approval).unwrap();
    let MergeOutcome::Merged {
        merge_commit,
        main_worktree_updated,
    } = outcome
    else {
        panic!("expected Merged, got {outcome:?}");
    };
    assert!(!main_worktree_updated);

    let repo = main.repo();
    let pinned_ref = repo.find_branch(&pinned, git2::BranchType::Local).unwrap();
    assert_eq!(pinned_ref.get().target().unwrap(), merge_commit);
    let mc = repo.find_commit(merge_commit).unwrap();
    assert_eq!(mc.parent_count(), 2);
    assert_eq!(mc.parent_id(0).unwrap(), pinned_before);
    assert_eq!(mc.parent_id(1).unwrap(), cp);

    // `side` — merely checked out — keeps its own tip, the main worktree
    // still shows its content
    let side = repo.find_branch("side", git2::BranchType::Local).unwrap();
    assert_eq!(side.get().target().unwrap(), side_tip);
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "side");
    assert_eq!(main.read("a.txt"), "side edit\n");
}

/// P1-2: create refuses a detached main HEAD — there is no branch to pin as
/// the transaction's merge target — and the refusal leaves no trace.
#[test]
fn create_refuses_detached_main_head() {
    let main = init_main();
    let repo = main.repo();
    repo.set_head_detached(main.head()).unwrap();

    let err = TaskWorkspace::create(&main.path, "detached").unwrap_err();
    assert!(matches!(err, TransactionError::MainDetachedHead));

    // no leftovers: no branch, no worktree dir, no config keys
    assert!(repo
        .find_branch("tiller/task-detached", git2::BranchType::Local)
        .is_err());
    assert!(!worktree_dir(&main.path, "detached").exists());
    let cfg = repo.config().unwrap();
    for key in [
        "tiller.worktree.detached.base",
        "tiller.worktree.detached.target-branch",
    ] {
        assert!(cfg.get_string(key).is_err(), "{key} must not be set");
    }
}

/// P5 review follow-up: a reopened workspace recomputes approval validity
/// from the current tips — persisting the approval never *revives* it.
#[test]
fn reopen_does_not_revive_a_stale_approval() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "reopen-stale");
    ws.approve(cp).unwrap();

    // tip moves after the approval, then the workspace is reopened
    write_file(
        &worktree_dir(&main.path, "reopen-stale"),
        "a.txt",
        "later\n",
    );
    ws.checkpoint("post-approval").unwrap();
    let reopened = TaskWorkspace::open(&main.path, "reopen-stale").unwrap();
    assert!(matches!(
        reopened.approval_state().unwrap(),
        ApprovalState::StaleCandidate
    ));
}

#[test]
fn reopen_reports_a_moved_base() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "reopen-moved");
    ws.approve(cp).unwrap();

    write_file(&main.path, "b.txt", "main moved\n");
    commit_all(&main.repo(), "main advance");

    let reopened = TaskWorkspace::open(&main.path, "reopen-moved").unwrap();
    assert!(matches!(
        reopened.approval_state().unwrap(),
        ApprovalState::BaseMoved
    ));
}

#[test]
fn merge_rejects_dirty_main_worktree() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "dirty-main");
    let approval = ws.approve(cp).unwrap();

    write_file(&main.path, "b.txt", "uncommitted on main\n");
    match ws.merge(&approval).unwrap_err() {
        TransactionError::MainWorktreeDirty => {}
        e => panic!("expected MainWorktreeDirty, got {e:?}"),
    }
}

/// P2-2: a missing `.git/info/exclude` is recreated (not an error), and the
/// exclusion pattern is appended exactly once across workspaces.
#[test]
fn exclude_missing_is_recreated_and_append_is_idempotent() {
    let main = init_main();
    fs::remove_file(main.path.join(".git/info/exclude")).unwrap();

    TaskWorkspace::create(&main.path, "ex-1").unwrap();
    let content = fs::read_to_string(main.path.join(".git/info/exclude")).unwrap();
    assert!(content.contains("/.tiller/"));

    TaskWorkspace::create(&main.path, "ex-2").unwrap();
    let content = fs::read_to_string(main.path.join(".git/info/exclude")).unwrap();
    assert_eq!(content.matches("/.tiller/").count(), 1);
}

// ------------------------------------------------------------ 6. rollback

#[test]
fn reset_to_base_discards_changes_and_untracked_but_keeps_recoverable_history() {
    let main = init_main();
    let (mut ws, _cp) = task_with_checkpoint(&main, "rollback-1");
    let dir = worktree_dir(&main.path, "rollback-1");

    write_file(&dir, "scratch.txt", "untracked\n");
    ws.reset_to_base().unwrap();

    // worktree content back to base, untracked gone
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "base-a\n");
    assert!(!dir.join("c.txt").exists());
    assert!(!dir.join("scratch.txt").exists());
    assert_eq!(ws.checkpoints().unwrap().len(), 0);

    // worktree still usable: a fresh checkpoint advances from base again
    write_file(&dir, "a.txt", "after rollback\n");
    let cp = ws.checkpoint("fresh start").unwrap();
    assert_eq!(ws.checkpoints().unwrap().len(), 1);
    assert_eq!(cp, ws.repository().head().unwrap().target().unwrap());
}

#[test]
fn discard_removes_everything_and_allows_recreating_the_same_task() {
    let main = init_main();
    {
        let (mut ws, cp) = task_with_checkpoint(&main, "discard-1");
        ws.approve(cp).unwrap(); // approval keys exist → removal is exercised
        ws.discard().unwrap();
    }
    let dir = worktree_dir(&main.path, "discard-1");
    assert!(!dir.exists());
    assert!(main
        .repo()
        .find_branch("tiller/task-discard-1", git2::BranchType::Local)
        .is_err());
    assert!(TaskWorkspace::list(&main.path).unwrap().is_empty());
    assert!(matches!(
        TaskWorkspace::open(&main.path, "discard-1").unwrap_err(),
        TransactionError::UnknownTask(_)
    ));

    // P2-3: metadata keys are gone from the repo-local config, not just the
    // worktree dir and branch — a fresh config read (not a cached handle)
    // must not find them
    let cfg = main.repo().config().unwrap();
    for key in [
        "tiller.worktree.discard-1.base",
        "tiller.worktree.discard-1.target-branch",
        "tiller.worktree.discard-1.approval.candidate",
        "tiller.worktree.discard-1.approval.at",
    ] {
        assert!(cfg.get_string(key).is_err(), "{key} should be gone");
    }

    // same id can be reused after a discard
    let ws = TaskWorkspace::create(&main.path, "discard-1").unwrap();
    assert_eq!(ws.base_commit(), main.head());
}

// -------------------------------------------------------------- 7. serde

#[test]
fn serde_types_round_trip_with_snake_case_hex_oids() {
    let main = init_main();
    let (mut ws, cp) = task_with_checkpoint(&main, "serde-1");
    let candidate = ws.candidate().unwrap();
    ws.approve(cp).unwrap();

    let json = serde_json::to_value(&candidate).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "task_id": "serde-1",
            "candidate_commit": cp.to_string(),
            "base_commit": ws.base_commit().to_string(),
            "created_at": candidate.created_at,
        })
    );
    let back: ReviewCandidate = serde_json::from_value(json).unwrap();
    assert_eq!(back.candidate_commit, candidate.candidate_commit);

    let state: ApprovalState = serde_json::from_str("\"stale_candidate\"").unwrap();
    assert!(matches!(state, ApprovalState::StaleCandidate));
    assert_eq!(
        serde_json::to_value(ApprovalState::BaseMoved).unwrap(),
        "base_moved"
    );

    let outcome = MergeOutcome::NeedsRebase { new_base: cp };
    let json = serde_json::to_value(&outcome).unwrap();
    assert_eq!(json["needs_rebase"]["new_base"], cp.to_string());
    let back: MergeOutcome = serde_json::from_value(json).unwrap();
    assert!(matches!(back, MergeOutcome::NeedsRebase { .. }));

    let outcome = MergeOutcome::Merged {
        merge_commit: cp,
        main_worktree_updated: false,
    };
    let json = serde_json::to_value(&outcome).unwrap();
    assert_eq!(json["merged"]["merge_commit"], cp.to_string());
    assert_eq!(json["merged"]["main_worktree_updated"], false);
    let back: MergeOutcome = serde_json::from_value(json).unwrap();
    assert!(matches!(
        back,
        MergeOutcome::Merged {
            main_worktree_updated: false,
            ..
        }
    ));
}
