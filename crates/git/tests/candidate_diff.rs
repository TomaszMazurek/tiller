//! Integration suite for the candidate diff API (P6): linear diff
//! base→candidate with per-file and total byte caps, binary detection and
//! path filtering — all on real temp repositories, never on files inside
//! this repo (same harness contract as `transactions.rs`).

use std::fs;
use std::path::{Path, PathBuf};

use git2::Repository;
use tiller_git::{CandidateDiff, ChangeType, TaskWorkspace, TransactionError};

// ---------------------------------------------------------------- harness

struct MainRepo {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn write_bytes(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn commit_all(repo: &Repository, message: &str) -> git2::Oid {
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

/// A main repo with a base commit containing `a.txt` and `b.txt`
/// (`core.autocrlf=false` pins checkout byte-for-byte cross-platform).
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

/// create + one checkpoint modifying `a.txt` and adding `c.txt`.
fn task_with_candidate(main: &MainRepo, task_id: &str) -> TaskWorkspace {
    let mut ws = TaskWorkspace::create(&main.path, task_id).unwrap();
    let dir = worktree_dir(&main.path, task_id);
    write_file(&dir, "a.txt", "task-a\nmore\n");
    write_file(&dir, "c.txt", "task-c\n");
    ws.checkpoint("first checkpoint").unwrap();
    ws
}

// ------------------------------------------------------------------ tests

#[test]
fn small_diff_is_complete_with_serde_shape() {
    let main = init_main();
    let ws = task_with_candidate(&main, "diff-1");

    let diff: CandidateDiff = ws.candidate_diff(None).unwrap();
    assert!(!diff.truncated);
    assert_eq!(diff.byte_cap, 256 * 1024);
    assert!(diff.total_bytes > 0);

    let by_path: Vec<&tiller_git::FileDiff> = diff
        .files
        .iter()
        .map(|f| (f.path.as_str(), f))
        .map(|(_, f)| f)
        .collect();
    assert_eq!(diff.files.len(), 2, "{by_path:?}");
    assert_eq!(diff.files[0].path, "a.txt");
    assert_eq!(diff.files[0].change_type, ChangeType::Modified);
    assert!(!diff.files[0].binary);
    assert!(!diff.files[0].truncated);
    assert!(diff.files[0].patch.contains("-base-a"));
    assert!(diff.files[0].patch.contains("+task-a"));
    assert!(diff.files[0].patch.contains("@@"));
    assert_eq!(diff.files[1].path, "c.txt");
    assert_eq!(diff.files[1].change_type, ChangeType::Added);

    // snake_case serde shape on the wire (rpc mirror in the cockpit).
    let value = serde_json::to_value(&diff).unwrap();
    assert_eq!(value["files"][0]["change_type"], "modified");
    assert_eq!(value["byte_cap"], (256 * 1024) as u64);
}

#[test]
fn large_file_patch_is_truncated_with_marker() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "diff-2").unwrap();
    let dir = worktree_dir(&main.path, "diff-2");
    // ~400 KiB of text: above both the per-file (64 KiB) and total (256 KiB) caps.
    let mut big = String::new();
    for i in 0..20_000 {
        big.push_str(&format!("line {i} of a very generated file\n"));
    }
    assert!(big.len() > 256 * 1024);
    write_file(&dir, "big.txt", &big);
    ws.checkpoint("big checkpoint").unwrap();

    let diff = ws.candidate_diff(None).unwrap();
    assert!(diff.truncated, "{diff:?}");
    let file = &diff.files[0];
    assert_eq!(file.path, "big.txt");
    assert!(file.truncated);
    assert!(
        file.patch.ends_with("… truncated …"),
        "patch should end with the marker, got tail {:?}",
        &file.patch[file.patch.len().saturating_sub(60)..]
    );
    // cut + marker only: never more than one marker's worth over the cap.
    assert!(file.patch.len() <= 64 * 1024 + 64);
    assert!(diff.total_bytes <= 256 * 1024 + 64);
}

#[test]
fn total_cap_truncates_later_files_too() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "diff-3").unwrap();
    let dir = worktree_dir(&main.path, "diff-3");
    // Four files × ~100 KiB each: the first eats the per-file cap, the
    // remaining ones only see a drained total budget.
    for n in 0..4 {
        let mut content = String::new();
        for i in 0..5_000 {
            content.push_str(&format!("file {n} line {i} with some padding text\n"));
        }
        write_file(&dir, &format!("f{n}.txt"), &content);
    }
    ws.checkpoint("wide checkpoint").unwrap();

    let diff = ws.candidate_diff(None).unwrap();
    assert!(diff.truncated);
    assert_eq!(diff.files.len(), 4);
    assert!(diff.files.iter().all(|f| f.truncated));
    assert!(diff.total_bytes <= 256 * 1024 + 4 * 64);
}

#[test]
fn binary_file_is_flagged_without_content() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "diff-4").unwrap();
    let dir = worktree_dir(&main.path, "diff-4");
    write_bytes(&dir, "blob.bin", b"BIN\x00\x01\x02\x03data");
    ws.checkpoint("binary checkpoint").unwrap();

    let diff = ws.candidate_diff(None).unwrap();
    let file = &diff.files[0];
    assert_eq!(file.path, "blob.bin");
    assert!(file.binary);
    assert!(file.patch.is_empty(), "no binary content on the wire");
    assert!(!file.truncated);
}

#[test]
fn path_filter_returns_only_the_requested_file() {
    let main = init_main();
    let ws = task_with_candidate(&main, "diff-5");

    let diff = ws.candidate_diff(Some("c.txt")).unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "c.txt");
    assert_eq!(diff.files[0].change_type, ChangeType::Added);

    // filtering by a path that did not change yields an empty diff, not an error
    let empty = ws.candidate_diff(Some("b.txt")).unwrap();
    assert!(empty.files.is_empty());
    assert!(!empty.truncated);
}

#[test]
fn deleted_file_patch_removes_content() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "diff-6").unwrap();
    let dir = worktree_dir(&main.path, "diff-6");
    fs::remove_file(dir.join("b.txt")).unwrap();
    ws.checkpoint("delete b").unwrap();

    let diff = ws.candidate_diff(None).unwrap();
    let file = &diff.files[0];
    assert_eq!(file.path, "b.txt");
    assert_eq!(file.change_type, ChangeType::Deleted);
    assert!(file.patch.contains("-base-b"));
}

#[test]
fn renamed_file_is_reported_as_old_arrow_new() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "diff-7").unwrap();
    let dir = worktree_dir(&main.path, "diff-7");
    // identical content → certain rename detection (no similarity tuning)
    fs::rename(dir.join("a.txt"), dir.join("renamed.txt")).unwrap();
    ws.checkpoint("rename").unwrap();

    let diff = ws.candidate_diff(None).unwrap();
    assert_eq!(diff.files.len(), 1, "{:?}", diff.files);
    assert_eq!(diff.files[0].path, "a.txt -> renamed.txt");
    assert_eq!(diff.files[0].change_type, ChangeType::Renamed);
}

#[test]
fn diff_without_checkpoints_is_no_checkpoints() {
    let main = init_main();
    let ws = TaskWorkspace::create(&main.path, "diff-8").unwrap();
    let err = ws.candidate_diff(None).unwrap_err();
    assert_eq!(err.code(), "no_checkpoints");
    assert!(matches!(err, TransactionError::NoCheckpoints));
}

#[test]
fn patch_keeps_the_unified_file_header() {
    let main = init_main();
    let ws = task_with_candidate(&main, "diff-10");

    // A shipped patch must be a full unified patch (P6 review MAJOR-3):
    // the `diff --git` line plus the `---`/`+++` file specs, counted
    // against the caps like any other line.
    let diff = ws.candidate_diff(None).unwrap();
    let a = &diff.files[0];
    assert_eq!(a.path, "a.txt");
    assert!(
        a.patch.contains("diff --git a/a.txt b/a.txt"),
        "modified file patch is missing its `diff --git` header: {a:?}"
    );
    assert!(
        a.patch.contains("--- a/a.txt") && a.patch.contains("+++ b/a.txt"),
        "modified file patch is missing its ---/+++ specs: {a:?}"
    );
    let c = &diff.files[1];
    assert_eq!(c.path, "c.txt");
    assert!(
        c.patch.contains("diff --git a/c.txt b/c.txt") && c.patch.contains("+++ b/c.txt"),
        "added file patch is missing its header: {c:?}"
    );
    assert!(
        c.patch.contains("--- /dev/null"),
        "added file patch must say it came from /dev/null: {c:?}"
    );
}

#[test]
fn total_budget_exhaustion_keeps_later_files_as_metadata() {
    let main = init_main();
    let mut ws = TaskWorkspace::create(&main.path, "diff-11").unwrap();
    let dir = worktree_dir(&main.path, "diff-11");
    // Five ~200 KiB files: four eat the whole 256 KiB total budget under
    // their per-file caps, the fifth is announced after the budget is
    // gone — it must still appear (metadata only, no content) and the
    // walk must stop instead of materialising the remaining patches
    // (P6 review MAJOR-2).
    for n in 0..5 {
        let mut content = String::new();
        for i in 0..5_000 {
            content.push_str(&format!("file {n} line {i} with some padding text\n"));
        }
        write_file(&dir, &format!("f{n}.txt"), &content);
    }
    ws.checkpoint("wide checkpoint").unwrap();

    let diff = ws.candidate_diff(None).unwrap();
    assert!(diff.truncated);
    assert_eq!(diff.files.len(), 5, "{:?}", diff.files);
    for file in &diff.files[..4] {
        assert!(file.truncated);
        assert!(!file.patch.is_empty(), "capped files still ship content");
    }
    let overflow = &diff.files[4];
    assert_eq!(overflow.path, "f4.txt");
    assert_eq!(overflow.change_type, ChangeType::Added);
    assert!(overflow.truncated);
    assert!(
        overflow.patch.is_empty(),
        "post-budget file is metadata only, no content ships: {overflow:?}"
    );
    // marker bytes ship → they count: at most one marker over budget per file
    assert!(diff.total_bytes <= 256 * 1024 + 4 * 64);
}

#[test]
fn diff_reads_objects_not_the_worktree() {
    let main = init_main();
    let ws = task_with_candidate(&main, "diff-9");
    let before = ws.candidate_diff(None).unwrap();

    // dirty the worktree *after* the checkpoint: the candidate diff is a
    // pure object-database read (base tree → candidate tree) and must not
    // see uncommitted changes.
    write_file(
        &worktree_dir(&main.path, "diff-9"),
        "a.txt",
        "uncommitted\n",
    );
    let after = ws.candidate_diff(None).unwrap();
    assert_eq!(before, after);
    assert!(!after.files.iter().any(|f| f.patch.contains("uncommitted")));
}
