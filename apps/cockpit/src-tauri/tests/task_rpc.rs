//! End-to-end integration suite for the `task.*` rpc surface (P6):
//! dispatcher → TaskHost → tiller-git on real temp repositories. The
//! term host is never exercised here (a no-op sink satisfies the
//! constructor); these tests pin wire shapes, validation and the
//! application error-code mapping.

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use cockpit_lib::{
    CockpitDispatcher, CommandRunner, EventSink, ProcessOutcome, ProcessRunner, PtySessionFactory,
    SessionManager, TaskHost,
};
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

/// Records every notification (gate events) for assertions.
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<(String, serde_json::Value)>>,
}

impl EventSink for RecordingSink {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        self.events
            .lock()
            .unwrap()
            .push((event.to_string(), payload));
    }
}

/// Poll `pred` until it holds or the deadline passes (async gate worker
/// threads finish on their own schedule; bounded waiting, no sleeps).
fn wait_until(timeout_ms: u64, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A scripted [`CommandRunner`]: every invocation announces itself on
/// `arrivals` and then parks on `release` until the test lets it proceed,
/// answering with the queued outcome. That makes the async worker's
/// progress test-drivable: the mid-run `running` state is observable.
struct ScriptedRunner {
    outcomes: Mutex<std::collections::VecDeque<ProcessOutcome>>,
    arrivals: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl CommandRunner for ScriptedRunner {
    fn run(
        &self,
        _program: &str,
        _args: &[&str],
        _cwd: &std::path::Path,
        _env: &[(String, String)],
        _timeout: std::time::Duration,
    ) -> ProcessOutcome {
        self.arrivals.send(()).expect("test listens");
        self.release.lock().unwrap().recv().expect("test releases");
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("an outcome is queued for every gate")
    }
}

fn dispatcher() -> CockpitDispatcher {
    let sandbox_root = std::env::temp_dir().join(format!("tiller-taskrpc-{}", std::process::id()));
    let manager = SessionManager::new(
        Arc::new(NoopSink),
        sandbox_root,
        Arc::new(PtySessionFactory),
    );
    CockpitDispatcher::new(
        manager,
        Arc::new(TaskHost::new(Arc::new(NoopSink), Arc::new(ProcessRunner))),
    )
}

fn gates_dispatcher(
    outcomes: Vec<ProcessOutcome>,
) -> (
    CockpitDispatcher,
    Arc<RecordingSink>,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let sink = Arc::new(RecordingSink::default());
    let (arrive_tx, arrive_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let runner = Arc::new(ScriptedRunner {
        outcomes: Mutex::new(outcomes.into()),
        arrivals: arrive_tx,
        release: Mutex::new(release_rx),
    });
    let sandbox_root = std::env::temp_dir().join(format!("tiller-taskrpc-{}", std::process::id()));
    let manager = SessionManager::new(sink.clone(), sandbox_root, Arc::new(PtySessionFactory));
    (
        CockpitDispatcher::new(manager, Arc::new(TaskHost::new(sink.clone(), runner))),
        sink,
        arrive_rx,
        release_tx,
    )
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

/// The repo HEAD at task-create time, hex — the expected `base` in list
/// answers (nothing lands on the pinned branch before the capture).
fn head_hex(repo: &Repo) -> String {
    Repository::open(&repo.path)
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap()
        .to_string()
}

// ------------------------------------------------------------------ tests

#[test]
fn happy_path_create_checkpoint_diff_approve_merge() {
    let d = dispatcher();
    let repo = init_repo();
    let base = head_hex(&repo);
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
    // P8a: the entry carries the worktree path for the task↔terminal
    // handoff ("open in terminal" reads it straight from the queue)
    assert_eq!(
        list[0]["worktree_path"],
        serde_json::json!(repo.path.join(".tiller").join("worktrees").join("fix-1"))
    );
    assert_eq!(list[0]["base"], base);
    assert_eq!(list[0]["tip"], commit);
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

// ------------------------------------------------------------ gates (P7)

fn outcome(status: &str, exit_code: Option<i32>, output: &str) -> ProcessOutcome {
    use cockpit_lib::GateStatus;
    ProcessOutcome {
        status: match status {
            "passed" => GateStatus::Passed,
            "failed" => GateStatus::Failed,
            _ => GateStatus::Timeout,
        },
        exit_code,
        duration_ms: 7,
        output: output.to_owned(),
    }
}

fn gate_events(sink: &RecordingSink) -> Vec<(String, serde_json::Value)> {
    sink.events.lock().unwrap().clone()
}

/// The whole async flow: started answer, `task:gate` events (running →
/// final per gate), the running flag mid-run, and the finished evidence
/// bound to the candidate with log tails capped.
#[test]
fn run_gates_full_flow_events_running_flag_and_evidence() {
    let long_output = "x".repeat(64 * 1024);
    let (d, sink, arrivals, release) = gates_dispatcher(vec![
        outcome("passed", Some(0), "checking...\n"),
        outcome(
            "failed",
            Some(101),
            &format!("{long_output}\ntests fail at the end"),
        ),
    ]);
    let repo = init_repo();
    let commit = task_with_checkpoint(&d, &repo, "gates-1");

    // default gates = both presets, in run order
    let started = call(&d, "task.run_gates", task_param(&repo, "gates-1"));
    assert_eq!(
        started["started"],
        serde_json::json!(["cargo-check", "cargo-test"])
    );
    assert_eq!(started["candidate"], commit);

    // mid-run: the first gate is parked on the script → running=true,
    // no evidence yet
    assert!(arrivals
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_ok());
    let gates = call(&d, "task.gates", task_param(&repo, "gates-1"));
    assert_eq!(gates["running"], true);
    assert!(gates.get("evidence").is_none());

    // running event for gate 1 precedes its final answer
    assert!(wait_until(2000, || !gate_events(&sink).is_empty()));
    let (_, payload) = gate_events(&sink)[0].clone();
    assert_eq!(payload["task_id"], "gates-1");
    assert_eq!(payload["candidate"], commit);
    assert_eq!(payload["id"], "cargo-check");
    assert_eq!(payload["status"], "running");
    assert!(payload.get("duration_ms").is_none());

    // release gate 1; gate 2 arrives parked — and its `running` event is
    // already out (events precede the runner call by design)
    release.send(()).unwrap();
    assert!(arrivals
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_ok());
    let events = gate_events(&sink);
    assert_eq!(events.len(), 3);
    assert_eq!(events[1].1["id"], "cargo-check");
    assert_eq!(events[1].1["status"], "passed");
    assert_eq!(events[1].1["duration_ms"], 7);
    assert_eq!(events[2].1["id"], "cargo-test");
    assert_eq!(events[2].1["status"], "running");

    // second start while a run is live → stable app error
    let again = call_err(&d, "task.run_gates", task_param(&repo, "gates-1"));
    assert_eq!(again.code, -32024);
    assert_eq!(again.data.unwrap()["code"], "gates_already_running");

    release.send(()).unwrap();
    // evidence lands bound to the candidate, log tails capped
    assert!(wait_until(5000, || {
        call(&d, "task.gates", task_param(&repo, "gates-1"))
            .get("evidence")
            .is_some()
    }));
    let gates = call(&d, "task.gates", task_param(&repo, "gates-1"));
    assert_eq!(gates["running"], false);
    assert_eq!(gates["candidate"], commit);
    let evidence = &gates["evidence"];
    assert_eq!(evidence["dirty_at_start"], false);
    // A clean end (nothing changed during the run): no annotation
    assert_eq!(evidence["dirty_at_end"], false);
    assert!(evidence.get("dirty_paths").is_none());
    assert!(evidence["finished_at"].is_u64());
    assert_eq!(evidence["gates"][0]["id"], "cargo-check");
    assert_eq!(evidence["gates"][0]["status"], "passed");
    assert_eq!(evidence["gates"][0]["exit_code"], 0);
    assert_eq!(evidence["gates"][1]["id"], "cargo-test");
    assert_eq!(evidence["gates"][1]["status"], "failed");
    assert_eq!(evidence["gates"][1]["exit_code"], 101);
    let tail = evidence["gates"][1]["log_tail"].as_str().unwrap();
    // the tail keeps the END: the closing marker survives, the 64 KiB of
    // filler does not
    assert!(tail.contains("tests fail at the end"), "{tail}");
    assert!(tail.starts_with("… [output tail]"));
    // the marker counts against the budget: marker + bytes ≤ 8 KiB total
    // (P7 review MINOR-1)
    assert!(
        tail.len() <= 8 * 1024,
        "log tail capped to 8 KiB including the marker: {}",
        tail.len()
    );

    // all four events: running+final per gate, in order
    let events = gate_events(&sink);
    assert_eq!(events.len(), 4);
    assert!(events.iter().all(|(name, _)| name == "task:gate"));
    assert_eq!(events[2].1["id"], "cargo-test");
    assert_eq!(events[2].1["status"], "running");
    assert_eq!(events[3].1["status"], "failed");
}

/// A subset of gates runs only what was asked for.
#[test]
fn run_gates_accepts_a_subset_of_presets() {
    let (d, _sink, arrivals, release) = gates_dispatcher(vec![outcome("passed", Some(0), "ok")]);
    let repo = init_repo();
    task_with_checkpoint(&d, &repo, "gates-sub");
    let started = call(
        &d,
        "task.run_gates",
        serde_json::json!({
            "repo": repo.path.to_string_lossy(),
            "task_id": "gates-sub",
            "gates": ["cargo-test"],
        }),
    );
    assert_eq!(started["started"], serde_json::json!(["cargo-test"]));
    assert!(arrivals
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_ok());
    release.send(()).unwrap();
    assert!(wait_until(5000, || {
        call(&d, "task.gates", task_param(&repo, "gates-sub"))
            .get("evidence")
            .is_some()
    }));
    let gates = call(&d, "task.gates", task_param(&repo, "gates-sub"));
    assert_eq!(gates["evidence"]["gates"].as_array().unwrap().len(), 1);
}

/// Untracked files are dirt too (P7 review MAJOR-1): `cargo` would
/// compile files that are not part of the candidate, so the start
/// refuses them with the path in the error — checkpoint first.
#[test]
fn run_gates_refuses_untracked_files() {
    let (d, _sink, _arrivals, _release) = gates_dispatcher(vec![]);
    let repo = init_repo();
    task_with_checkpoint(&d, &repo, "gates-untracked");

    // a clean tracked tree but a new untracked file: still refused
    let worktree = repo
        .path
        .join(".tiller")
        .join("worktrees")
        .join("gates-untracked");
    fs::write(worktree.join("stray.rs"), "fn main() {}\n").unwrap();
    let error = call_err(&d, "task.run_gates", task_param(&repo, "gates-untracked"));
    assert_eq!(error.code, -32023);
    let data = error.data.clone().unwrap();
    assert_eq!(data["code"], "gates_dirty_worktree");
    assert_eq!(data["paths"], serde_json::json!(["stray.rs"]));
    assert!(error.message.contains("stray.rs"), "{error:?}");

    // both kinds land in one refusal, tracked first
    fs::write(worktree.join("a.txt"), "also modified\n").unwrap();
    let error = call_err(&d, "task.run_gates", task_param(&repo, "gates-untracked"));
    assert_eq!(
        error.data.unwrap()["paths"],
        serde_json::json!(["a.txt", "stray.rs"])
    );
}

/// Review directive A.2: the worktree is re-checked right before the
/// record is written; a run that ended dirty is **annotated**
/// (`dirty_at_end` + `dirty_paths`), never discarded — incomplete proof,
/// not a lost one. A clean run records `dirty_at_end: false`.
#[test]
fn run_gates_annotates_dirty_at_end_when_the_worktree_changed_during_the_run() {
    let (d, _sink, arrivals, release) = gates_dispatcher(vec![
        outcome("passed", Some(0), "ok"),
        outcome("passed", Some(0), "ok"),
    ]);
    let repo = init_repo();
    let commit = task_with_checkpoint(&d, &repo, "gates-dirty-end");
    call(&d, "task.run_gates", task_param(&repo, "gates-dirty-end"));

    // mid-run (first gate parked on the script): the operator edits the
    // worktree — the run itself proceeds and passes
    assert!(arrivals
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_ok());
    let worktree = repo
        .path
        .join(".tiller")
        .join("worktrees")
        .join("gates-dirty-end");
    fs::write(worktree.join("a.txt"), "changed mid-run\n").unwrap();
    fs::write(worktree.join("late.txt"), "untracked mid-run\n").unwrap();
    release.send(()).unwrap();
    assert!(arrivals
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_ok());
    release.send(()).unwrap();

    assert!(wait_until(5000, || {
        call(&d, "task.gates", task_param(&repo, "gates-dirty-end"))
            .get("evidence")
            .is_some()
    }));
    let gates = call(&d, "task.gates", task_param(&repo, "gates-dirty-end"));
    assert_eq!(gates["candidate"], commit);
    let evidence = &gates["evidence"];
    assert_eq!(evidence["dirty_at_end"], true);
    // tracked change and untracked file alike, both listed
    assert_eq!(
        evidence["dirty_paths"],
        serde_json::json!(["a.txt", "late.txt"])
    );
    // the run itself is recorded — annotated, not discarded
    assert_eq!(evidence["gates"][0]["status"], "passed");
    assert_eq!(evidence["gates"][1]["status"], "passed");
}

/// Preconditions refuse: dirty worktree (checkpoint first, files listed),
/// no checkpoints yet, unknown task.
#[test]
fn run_gates_refuses_dirty_worktree_and_tasks_without_checkpoints() {
    let (d, _sink, _arrivals, _release) = gates_dispatcher(vec![]);
    let repo = init_repo();
    task_with_checkpoint(&d, &repo, "gates-dirty");

    // dirty: tracked change without a checkpoint
    let worktree = repo
        .path
        .join(".tiller")
        .join("worktrees")
        .join("gates-dirty");
    fs::write(worktree.join("a.txt"), "uncommitted\n").unwrap();
    let error = call_err(&d, "task.run_gates", task_param(&repo, "gates-dirty"));
    assert_eq!(error.code, -32023);
    let data = error.data.unwrap();
    assert_eq!(data["code"], "gates_dirty_worktree");
    assert_eq!(data["paths"], serde_json::json!(["a.txt"]));
    assert!(error.message.contains("commit first"));

    // no checkpoints: nothing reviewable to gate
    call(&d, "task.create", task_param(&repo, "gates-empty"));
    let error = call_err(&d, "task.run_gates", task_param(&repo, "gates-empty"));
    assert_eq!(error.code, CODE_NO_CHECKPOINTS);

    // unknown task
    let error = call_err(&d, "task.run_gates", task_param(&repo, "ghost"));
    assert_eq!(error.code, CODE_UNKNOWN_TASK);
}

/// After a new checkpoint the recorded evidence reads back stale — the
/// wire says `{stale: true}` until gates run against the new candidate.
#[test]
fn gates_go_stale_when_the_candidate_moves() {
    let (d, _sink, arrivals, release) = gates_dispatcher(vec![
        outcome("passed", Some(0), "ok"),
        outcome("passed", Some(0), "ok"),
    ]);
    let repo = init_repo();
    let commit = task_with_checkpoint(&d, &repo, "gates-stale");
    call(&d, "task.run_gates", task_param(&repo, "gates-stale"));
    for _ in 0..2 {
        assert!(arrivals
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok());
        release.send(()).unwrap();
    }
    assert!(wait_until(5000, || {
        call(&d, "task.gates", task_param(&repo, "gates-stale"))
            .get("evidence")
            .is_some()
    }));

    // a fresh checkpoint supersedes the candidate → stale
    let worktree = repo
        .path
        .join(".tiller")
        .join("worktrees")
        .join("gates-stale");
    fs::write(worktree.join("a.txt"), "moved on\n").unwrap();
    let cp2 = call(
        &d,
        "task.checkpoint",
        serde_json::json!({
            "repo": repo.path.to_string_lossy(),
            "task_id": "gates-stale",
            "message": "post-gates",
        }),
    )["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(cp2, commit);
    let gates = call(&d, "task.gates", task_param(&repo, "gates-stale"));
    assert_eq!(gates["stale"], true);
    assert_eq!(gates["running"], false);
    assert!(gates.get("evidence").is_none());

    // and a task that never ran gates answers the bare `{running: false}`
    task_with_checkpoint(&d, &repo, "gates-none");
    let gates = call(&d, "task.gates", task_param(&repo, "gates-none"));
    assert_eq!(gates, serde_json::json!({ "running": false }));
}

// ------------------------------------------- real process behavior (win)

#[cfg(windows)]
mod process_runner {
    use super::*;
    use cockpit_lib::ProcessRunner;

    fn run(program: &str, args: &[&str], timeout: std::time::Duration) -> ProcessOutcome {
        ProcessRunner.run(program, args, &std::env::temp_dir(), &[], timeout)
    }

    #[test]
    fn exit_codes_map_to_passed_and_failed() {
        let ok = run("cmd", &["/c", "exit 0"], std::time::Duration::from_secs(30));
        assert_eq!(ok.status, cockpit_lib::GateStatus::Passed);
        assert_eq!(ok.exit_code, Some(0));

        let bad = run("cmd", &["/c", "exit 1"], std::time::Duration::from_secs(30));
        assert_eq!(bad.status, cockpit_lib::GateStatus::Failed);
        assert_eq!(bad.exit_code, Some(1));
    }

    #[test]
    fn combined_output_is_captured() {
        let out = run(
            "cmd",
            &[
                "/c",
                "echo gate-stdout-marker & echo gate-stderr-marker 1>&2",
            ],
            std::time::Duration::from_secs(30),
        );
        assert!(out.output.contains("gate-stdout-marker"), "{}", out.output);
        assert!(out.output.contains("gate-stderr-marker"), "{}", out.output);
    }

    #[test]
    fn timeout_kills_the_tree_and_reports_timeout_fast() {
        // `ping -n 60` runs ~60 s; the 2 s budget must kill the tree and
        // return promptly — the tree-kill smoke (no orphan check: pid
        // reuse makes that flaky; promptness is the observable contract).
        let started = std::time::Instant::now();
        let out = run(
            "cmd",
            &["/c", "ping -n 60 127.0.0.1"],
            std::time::Duration::from_secs(2),
        );
        assert_eq!(out.status, cockpit_lib::GateStatus::Timeout);
        assert_eq!(out.exit_code, None);
        assert!(out.output.contains("timed out"));
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
    }

    #[test]
    fn missing_program_reports_failed_with_the_reason() {
        let out = run(
            "definitely-not-a-program-tiller",
            &[],
            std::time::Duration::from_secs(5),
        );
        assert_eq!(out.status, cockpit_lib::GateStatus::Failed);
        assert!(out.output.contains("failed to spawn"));
    }

    #[test]
    fn megabyte_of_output_is_tailed_not_hoarded() {
        // ~1 MiB through the real pipes: memory is O(cap) while reading
        // (not directly assertable — P7 review MAJOR-4), so the contract
        // checked here is: prompt return, tail capped to the reader
        // budget, ending preserved.
        let started = std::time::Instant::now();
        let out = run(
            "powershell",
            &["-NoProfile", "-Command", "$s = 'x' * 1048576; $s"],
            std::time::Duration::from_secs(60),
        );
        assert_eq!(out.status, cockpit_lib::GateStatus::Passed);
        assert!(out.output.ends_with('\n'), "{}", out.output);
        assert!(out.output.len() <= 2 * 8192, "{}", out.output.len());
        assert!(out.output.trim_end().chars().all(|c| c == 'x'));
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
    }
}

// -------------------------------------------- real process behavior (unix)

#[cfg(unix)]
mod process_runner_unix {
    use super::*;
    use std::time::{Duration, Instant};

    fn run(program: &str, args: &[&str], timeout: Duration) -> ProcessOutcome {
        ProcessRunner.run(program, args, &std::env::temp_dir(), &[], timeout)
    }

    /// `/proc/<pid>/stat` exists and is not a zombie ⇔ the process is
    /// alive (Linux; the CI target — macOS has no /proc and skips this
    /// module's process probes).
    #[cfg(target_os = "linux")]
    fn process_alive(pid: i32) -> bool {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // after the "(comm)" field the state char follows; Z = zombie
        // (dead, waiting to be reaped) counts as gone
        match stat.rsplit(')').next() {
            Some(rest) => !rest.trim_start().starts_with('Z'),
            None => true,
        }
    }

    /// A timeout must kill the whole process **group** (P7 review
    /// MAJOR-3): the child runs as its own session leader (spawned with
    /// `setsid`), so `killpg` reaches it *and* its grandchildren —
    /// `cargo`'s rustc/test binaries — with one signal.
    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_kills_the_process_group_including_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        // `sh` waits on a backgrounded `sleep`: child + grandchild both
        // alive until the group kill; the grandchild's pid lands in a
        // file the test can read back.
        let script = "sleep 300 & echo $! > grandchild.pid; wait";
        let started = Instant::now();
        let out = ProcessRunner.run(
            "sh",
            &["-c", script],
            dir.path(),
            &[],
            Duration::from_secs(1),
        );
        assert_eq!(out.status, cockpit_lib::GateStatus::Timeout);
        assert!(out.output.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(30));

        let pid: i32 = fs::read_to_string(dir.path().join("grandchild.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "grandchild {pid} survived the group kill"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn megabyte_of_output_is_tailed_not_hoarded() {
        // `yes | head -c` produces exactly 1 MiB; see the Windows twin
        // for the rationale (P7 review MAJOR-4).
        let started = Instant::now();
        let out = run(
            "sh",
            &[
                "-c",
                "yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -c 1048576",
            ],
            Duration::from_secs(60),
        );
        assert_eq!(out.status, cockpit_lib::GateStatus::Passed);
        assert!(out.output.ends_with('\n'), "{}", out.output);
        assert!(out.output.len() <= 2 * 8192, "{}", out.output.len());
        assert!(out.output.trim_end().chars().all(|c| c == 'x'));
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}
