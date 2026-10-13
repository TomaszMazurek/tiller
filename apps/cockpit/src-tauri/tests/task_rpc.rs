//! End-to-end integration suite for the `task.*` rpc surface (P6):
//! dispatcher → TaskHost → tiller-git on real temp repositories. The
//! term host is never exercised here (a no-op sink satisfies the
//! constructor); these tests pin wire shapes, validation and the
//! application error-code mapping.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use cockpit_lib::{CockpitDispatcher, EventSink, PtySessionFactory, SessionManager, TaskHost};
use git2::Repository;
use tiller_rpc::{Dispatcher, RequestId, RpcRequest, INVALID_PARAMS, METHOD_NOT_FOUND};

/// Application error codes for task.* (see `dispatch::task_error`).
const CODE_INVALID_TASK_ID: i64 = -32010;
const CODE_UNKNOWN_TASK: i64 = -32014;
const CODE_NOTHING_TO_COMMIT: i64 = -32015;
const CODE_NO_CHECKPOINTS: i64 = -32016;
const CODE_NO_APPROVAL: i64 = -32017;
const CODE_STALE_APPROVAL: i64 = -32018;
const CODE_REBASE_CONFLICT: i64 = -32022;

struct NoopSink;
impl EventSink for NoopSink {
    fn emit(&self, _event: &str, _payload: serde_json::Value) {}
}

fn dispatcher() -> CockpitDispatcher {
    let sandbox_root = std::env::temp_dir().join(format!("tiller-taskrpc-{}", std::process::id()));
    let manager = SessionManager::new(
        Arc::new(NoopSink),
        sandbox_root,
        Arc::new(PtySessionFactory),
    );
    CockpitDispatcher::new(manager, TaskHost::new())
}

fn call(d: &CockpitDispatcher, method: &str, params: serde_json::Value) -> serde_json::Value {
    d.dispatch(RpcRequest {
        id: RequestId::Num(1),
        method: method.to_string(),
        params,
    })
    .expect("ok response")
    .result
    .expect("result present")
}

fn call_err(
    d: &CockpitDispatcher,
    method: &str,
    params: serde_json::Value,
) -> tiller_rpc::RpcError {
    d.dispatch(RpcRequest {
        id: RequestId::Num(1),
        method: method.to_string(),
        params,
    })
    .expect_err("error response")
}

// ---------------------------------------------------------------- harness

struct Repo {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn commit_all(repo: &Repository, message: &str) {
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
        .unwrap();
}

/// A main repo with one base commit (`a.txt`, `b.txt`), autocrlf pinned
/// for byte-identical assertions on Windows.
fn init_repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.config()
        .unwrap()
        .set_str("core.autocrlf", "false")
        .unwrap();
    fs::write(dir.path().join("a.txt"), "base-a\n").unwrap();
    fs::write(dir.path().join("b.txt"), "base-b\n").unwrap();
    commit_all(&repo, "base");
    Repo {
        _dir: dir,
        path: repo.workdir().unwrap().to_path_buf(),
    }
}

fn repo_param(repo: &Repo) -> serde_json::Value {
    serde_json::json!({ "repo": repo.path.to_string_lossy() })
}

fn task_param(repo: &Repo, task_id: &str) -> serde_json::Value {
    serde_json::json!({ "repo": repo.path.to_string_lossy(), "task_id": task_id })
}

/// create + checkpoint editing `a.txt` and adding `c.txt`; returns the
/// checkpoint commit hex.
fn task_with_checkpoint(d: &CockpitDispatcher, repo: &Repo, task_id: &str) -> String {
    let created = call(d, "task.create", task_param(repo, task_id));
    let worktree = PathBuf::from(created["worktree_path"].as_str().unwrap());
    fs::write(worktree.join("a.txt"), "task-a\n").unwrap();
    fs::write(worktree.join("c.txt"), "task-c\n").unwrap();
    call(
        d,
        "task.checkpoint",
        serde_json::json!({
            "repo": repo.path.to_string_lossy(),
            "task_id": task_id,
            "message": "first checkpoint",
        }),
    )["commit"]
        .as_str()
        .unwrap()
        .to_owned()
}

// ------------------------------------------------------------------ tests

#[test]
fn happy_path_create_checkpoint_diff_approve_merge() {
    let d = dispatcher();
    let repo = init_repo();
    let commit = task_with_checkpoint(&d, &repo, "fix-1");
    assert_eq!(commit.len(), 40);

    // status: candidate exists, not approved yet, one checkpoint
    let status = call(&d, "task.status", task_param(&repo, "fix-1"));
    assert_eq!(status["status"], "candidate");
    assert_eq!(status["candidate"]["candidate_commit"], commit);
    assert!(status["candidate"]["base_commit"].is_string());
    assert!(status["candidate"]["created_at"].is_u64());
    assert_eq!(status["approval_state"], serde_json::Value::Null);
    assert_eq!(status["checkpoints_count"], 1);

    // diff: two files, patches with unified +/- lines, snake_case types
    let diff = call(&d, "task.diff", task_param(&repo, "fix-1"));
    assert_eq!(diff["files"].as_array().unwrap().len(), 2);
    assert_eq!(diff["files"][0]["path"], "a.txt");
    assert_eq!(diff["files"][0]["change_type"], "modified");
    let patch = diff["files"][0]["patch"].as_str().unwrap();
    assert!(patch.contains("-base-a"), "{patch}");
    assert!(patch.contains("+task-a"), "{patch}");
    assert_eq!(diff["truncated"], false);
    assert_eq!(diff["byte_cap"], 256 * 1024);
    // path filter narrows the diff
    let one = call(
        &d,
        "task.diff",
        serde_json::json!({
            "repo": repo.path.to_string_lossy(),
            "task_id": "fix-1",
            "path": "c.txt",
        }),
    );
    assert_eq!(one["files"].as_array().unwrap().len(), 1);

    // approve → valid → merge lands on the pinned branch
    let approved = call(&d, "task.approve", task_param(&repo, "fix-1"));
    assert_eq!(approved["approved"], commit);
    let status = call(&d, "task.status", task_param(&repo, "fix-1"));
    assert_eq!(status["approval_state"], "valid");

    let merged = call(&d, "task.merge", task_param(&repo, "fix-1"));
    assert!(merged["merged"].is_object(), "{merged}");
    assert!(merged["merged"]["merge_commit"].is_string());
    assert_eq!(merged["merged"]["main_worktree_updated"], true);

    // the merge landed in the main worktree's files…
    assert_eq!(
        fs::read_to_string(repo.path.join("a.txt")).unwrap(),
        "task-a\n"
    );
    assert_eq!(
        fs::read_to_string(repo.path.join("c.txt")).unwrap(),
        "task-c\n"
    );
    // …and the task is NOT auto-discarded: the UI offers Cleanup.
    let list = call(&d, "task.list", repo_param(&repo));
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["task_id"], "fix-1");
    assert_eq!(list[0]["target_branch"], "master");
    assert_eq!(list[0]["base"], list[0]["base"]); // hex fields present
    assert!(list[0]["tip"].as_str().unwrap().len() == 40);
}

#[test]
fn needs_rebase_flow_rebase_approve_merge() {
    let d = dispatcher();
    let repo = init_repo();
    task_with_checkpoint(&d, &repo, "fix-2");

    // someone lands on the pinned branch while the task is open
    fs::write(repo.path.join("b.txt"), "moved-on-main\n").unwrap();
    commit_all(&Repository::open(&repo.path).unwrap(), "main moved");

    call(&d, "task.approve", task_param(&repo, "fix-2"));
    let status = call(&d, "task.status", task_param(&repo, "fix-2"));
    assert_eq!(status["approval_state"], "base_moved");

    // merge refuses over a moved base…
    let outcome = call(&d, "task.merge", task_param(&repo, "fix-2"));
    assert!(outcome["needs_rebase"].is_object(), "{outcome}");
    assert!(outcome["needs_rebase"]["new_base"].is_string());

    // …rebase is conscious, new ids invalidate the old approval…
    let rebased = call(&d, "task.rebase", task_param(&repo, "fix-2"));
    assert_eq!(rebased["ok"], true);
    let new_tip = rebased["new_tip"].as_str().unwrap().to_owned();
    assert!(new_tip.len() == 40);
    call(&d, "task.approve", task_param(&repo, "fix-2"));
    let status = call(&d, "task.status", task_param(&repo, "fix-2"));
    assert_eq!(status["approval_state"], "valid");
    assert_eq!(status["candidate"]["candidate_commit"], new_tip);

    // …and the re-approval merges
    let merged = call(&d, "task.merge", task_param(&repo, "fix-2"));
    assert!(merged["merged"].is_object(), "{merged}");
    assert_eq!(
        fs::read_to_string(repo.path.join("a.txt")).unwrap(),
        "task-a\n"
    );
    assert_eq!(
        fs::read_to_string(repo.path.join("b.txt")).unwrap(),
        "moved-on-main\n"
    );
}

#[test]
fn rebase_conflict_maps_paths_into_error_data() {
    let d = dispatcher();
    let repo = init_repo();
    let created = call(&d, "task.create", task_param(&repo, "fix-3"));
    let worktree = PathBuf::from(created["worktree_path"].as_str().unwrap());
    fs::write(worktree.join("a.txt"), "task-a\n").unwrap();
    call(
        &d,
        "task.checkpoint",
        serde_json::json!({
            "repo": repo.path.to_string_lossy(),
            "task_id": "fix-3",
            "message": "touch a",
        }),
    );

    // conflicting edit of the same line on the pinned branch
    fs::write(repo.path.join("a.txt"), "main-a\n").unwrap();
    commit_all(&Repository::open(&repo.path).unwrap(), "main conflict");

    let error = call_err(&d, "task.rebase", task_param(&repo, "fix-3"));
    assert_eq!(error.code, CODE_REBASE_CONFLICT);
    assert_eq!(error.data.as_ref().unwrap()["code"], "rebase_conflict");
    let paths = error.data.as_ref().unwrap()["paths"].as_array().unwrap();
    assert!(paths.contains(&serde_json::json!("a.txt")), "{paths:?}");
}

#[test]
fn stale_approval_after_extra_checkpoint_is_refused_at_merge() {
    let d = dispatcher();
    let repo = init_repo();
    task_with_checkpoint(&d, &repo, "fix-4");
    call(&d, "task.approve", task_param(&repo, "fix-4"));

    // a new checkpoint after approval invalidates the candidate
    let worktree = repo.path.join(".tiller").join("worktrees").join("fix-4");
    fs::write(worktree.join("d.txt"), "late\n").unwrap();
    call(
        &d,
        "task.checkpoint",
        serde_json::json!({
            "repo": repo.path.to_string_lossy(),
            "task_id": "fix-4",
            "message": "late addition",
        }),
    );
    let status = call(&d, "task.status", task_param(&repo, "fix-4"));
    assert_eq!(status["approval_state"], "stale_candidate");

    let error = call_err(&d, "task.merge", task_param(&repo, "fix-4"));
    assert_eq!(error.code, CODE_STALE_APPROVAL);
    assert_eq!(error.data.as_ref().unwrap()["code"], "stale_approval");
}

#[test]
fn validation_and_error_mapping() {
    let d = dispatcher();
    let repo = init_repo();

    // params must be objects with exactly the known fields
    for (method, params) in [
        ("task.list", serde_json::json!({ "repo": "" })),
        ("task.list", serde_json::json!([])),
        ("task.create", serde_json::json!({ "repo": "x" })),
        (
            "task.create",
            serde_json::json!({ "repo": "x", "task_id": "", "extra": 1 }),
        ),
        (
            "task.checkpoint",
            serde_json::json!({ "repo": "x", "task_id": "t", "message": "" }),
        ),
        (
            "task.diff",
            serde_json::json!({ "repo": "x", "task_id": "t", "path": 7 }),
        ),
    ] {
        let error = call_err(&d, method, params);
        assert_eq!(error.code, INVALID_PARAMS, "{method}: {error:?}");
    }

    // invalid task id charset: app code from tiller-git, string code in data
    let error = call_err(
        &d,
        "task.create",
        serde_json::json!({ "repo": repo.path.to_string_lossy(), "task_id": "bad id!" }),
    );
    assert_eq!(error.code, CODE_INVALID_TASK_ID);
    assert_eq!(error.data.as_ref().unwrap()["code"], "invalid_task_id");

    // unknown task on every repo-scoped method
    for method in [
        "task.status",
        "task.checkpoint",
        "task.diff",
        "task.approve",
    ] {
        let params = if method == "task.checkpoint" {
            serde_json::json!({
                "repo": repo.path.to_string_lossy(),
                "task_id": "ghost",
                "message": "m",
            })
        } else {
            task_param(&repo, "ghost")
        };
        let error = call_err(&d, method, params);
        assert_eq!(error.code, CODE_UNKNOWN_TASK, "{method}: {error:?}");
        assert!(error.message.contains("ghost"), "{error:?}");
    }

    // no changes → nothing_to_commit; no candidate → no_checkpoints;
    // merge without approval → no_approval
    call(&d, "task.create", task_param(&repo, "empty-1"));
    let error = call_err(
        &d,
        "task.checkpoint",
        serde_json::json!({
            "repo": repo.path.to_string_lossy(),
            "task_id": "empty-1",
            "message": "nothing here",
        }),
    );
    assert_eq!(error.code, CODE_NOTHING_TO_COMMIT);
    for method in ["task.diff", "task.approve"] {
        let error = call_err(&d, method, task_param(&repo, "empty-1"));
        assert_eq!(error.code, CODE_NO_CHECKPOINTS, "{method}: {error:?}");
    }
    let error = call_err(&d, "task.merge", task_param(&repo, "empty-1"));
    assert_eq!(error.code, CODE_NO_APPROVAL);

    // unknown method stays METHOD_NOT_FOUND
    let error = call_err(&d, "task.nope", serde_json::json!({}));
    assert_eq!(error.code, METHOD_NOT_FOUND);
    assert!(error.message.contains("task.nope"));
}

#[test]
fn reset_and_discard_round_out_the_lifecycle() {
    let d = dispatcher();
    let repo = init_repo();
    task_with_checkpoint(&d, &repo, "fix-5");

    // reset: soft rollback to base
    call(&d, "task.reset", task_param(&repo, "fix-5"));
    let status = call(&d, "task.status", task_param(&repo, "fix-5"));
    assert_eq!(status["status"], "no_checkpoints");
    assert_eq!(status["checkpoints_count"], 0);
    assert!(status["candidate"].is_null());

    // discard: task gone from the queue and from disk
    call(&d, "task.discard", task_param(&repo, "fix-5"));
    let list = call(&d, "task.list", repo_param(&repo));
    assert_eq!(list.as_array().unwrap().len(), 0);
    assert!(!repo
        .path
        .join(".tiller")
        .join("worktrees")
        .join("fix-5")
        .exists());
    let error = call_err(&d, "task.status", task_param(&repo, "fix-5"));
    assert_eq!(error.code, CODE_UNKNOWN_TASK);
}

#[test]
fn diff_is_a_pure_object_read_available_while_worktree_dirty() {
    let d = dispatcher();
    let repo = init_repo();
    task_with_checkpoint(&d, &repo, "fix-6");

    // uncommitted worktree change must not leak into the candidate diff
    let worktree = repo.path.join(".tiller").join("worktrees").join("fix-6");
    fs::write(worktree.join("a.txt"), "uncommitted\n").unwrap();
    let diff = call(&d, "task.diff", task_param(&repo, "fix-6"));
    let patches = diff["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["patch"].as_str().unwrap())
        .collect::<String>();
    assert!(!patches.contains("uncommitted"), "{patches}");
}
