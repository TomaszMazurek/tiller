//! The cockpit dispatcher: `core.*` delegated to [`CoreDispatcher`], the
//! `term.*` surface served by the session manager, `session.observe`
//! for the timeline snapshot, and the `task.*` review surface over
//! `tiller-git` (see [`CockpitDispatcher::dispatch_task`] for its wire
//! shapes and [`task_error`] for its error-code convention).
//!
//! Wire shapes (params always an object; unknown fields rejected so a typo
//! cannot silently drop a write):
//!
//! - `term.spawn` `{program, args?, cwd, cols?=80, rows?=24, env?}` →
//!   `{session_id}`
//! - `term.write` `{session_id, data}` → `{}` (`data` = standard base64)
//! - `term.resize` `{session_id, cols, rows}` → `{}`
//! - `term.kill` `{session_id}` → `{}`
//! - `term.stats` `{session_id}` → `{dsr_responses_sent}`
//! - `session.observe` `{session_id}` → `{entries: [TimelineEntry…]}` —
//!   the timeline snapshot so far; live entries keep flowing as
//!   `session:entry` notifications (the tailer runs from spawn, so
//!   observe is purely the catch-up read).
//!
//! Error codes: shape/decoding problems are `INVALID_PARAMS` (-32602);
//! unknown methods `METHOD_NOT_FOUND` (-32601); an id with no live session
//! is [`SESSION_NOT_FOUND`] — an application code in the JSON-RPC
//! *server-error* range (-32000..-32099), chosen over reusing
//! INVALID_PARAMS so the UI can tell "malformed request" apart from the
//! benign race "the program exited under you" without parsing messages.
//! `task.*` failures use the same range from -32010 down, one stable number
//! per `TransactionError` variant (see [`task_error`]).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tiller_git::TransactionError;
use tiller_rpc::{
    CoreDispatcher, Dispatcher, RpcError, RpcRequest, RpcResponse, INTERNAL_ERROR, INVALID_PARAMS,
    METHOD_NOT_FOUND,
};

use crate::gates::{gate_preset, GATE_PRESETS};
use crate::session_tail::TimelineEntry;
use crate::task_host::{GateStartError, GatesInfo, GatesStarted, TaskHost, TaskSummary};
use crate::term_host::{base64_decode, SessionId, SessionManager, SpawnSpec, TermError};

/// Application error: the session id is not (or no longer) live.
pub const SESSION_NOT_FOUND: i64 = -32001;

/// The dispatcher behind the cockpit's `rpc` command.
pub struct CockpitDispatcher {
    core: CoreDispatcher,
    terms: SessionManager,
    tasks: Arc<TaskHost>,
}

impl CockpitDispatcher {
    pub fn new(terms: SessionManager, tasks: Arc<TaskHost>) -> Self {
        Self {
            core: CoreDispatcher,
            terms,
            tasks,
        }
    }

    fn dispatch_term(&self, method: &str, req: RpcRequest) -> Result<RpcResponse, RpcError> {
        let id = req.id.clone();
        let answer = match method {
            "spawn" => self
                .term_spawn(req.params)
                .map(|session_id| serde_json::json!({ "session_id": session_id })),
            "write" => self.term_write(req.params).map(|()| serde_json::json!({})),
            "resize" => self.term_resize(req.params).map(|()| serde_json::json!({})),
            "kill" => self.term_kill(req.params).map(|()| serde_json::json!({})),
            "stats" => self
                .term_stats(req.params)
                .map(|stats| serde_json::json!({ "dsr_responses_sent": stats.dsr_responses_sent })),
            other => {
                return Err(RpcError::new(
                    METHOD_NOT_FOUND,
                    format!("unknown method: term.{other}"),
                ))
            }
        };
        answer.map(|result| RpcResponse::ok(id, result))
    }

    fn dispatch_session(&self, method: &str, req: RpcRequest) -> Result<RpcResponse, RpcError> {
        let id = req.id.clone();
        let answer = match method {
            "observe" => self
                .session_observe(req.params)
                .map(|entries| serde_json::json!({ "entries": entries })),
            other => {
                return Err(RpcError::new(
                    METHOD_NOT_FOUND,
                    format!("unknown method: session.{other}"),
                ))
            }
        };
        answer.map(|result| RpcResponse::ok(id, result))
    }

    /// The `task.*` surface: review transactions over `tiller-git`
    /// (per-repo serialized, open-per-call — see `task_host.rs`).
    ///
    /// Wire shapes (params always objects, unknown fields rejected):
    ///
    /// - `task.list {repo}` → `[{task_id, target_branch, worktree_path, base, tip}]`
    /// - `task.create {repo, task_id}` → `{task_id, worktree_path, target_branch}`
    /// - `task.status {repo, task_id}` → `{status, candidate?, approval_state?, checkpoints_count}`
    /// - `task.checkpoint {repo, task_id, message}` → `{commit}`
    /// - `task.diff {repo, task_id, path?}` → `CandidateDiff` (snake_case)
    /// - `task.approve {repo, task_id}` → `{approved}` (hex oid)
    /// - `task.merge {repo, task_id}` → `MergeOutcome` (snake_case);
    ///   **no auto-discard after `merged`** — the UI offers Cleanup
    ///   (`task.discard`) explicitly
    /// - `task.rebase {repo, task_id}` → `{ok, new_tip}`
    /// - `task.reset {repo, task_id}` → `{}`
    /// - `task.discard {repo, task_id}` → `{}`
    /// - `task.run_gates {repo, task_id, gates?}` → `{started: [ids],
    ///   candidate}` — **async**: the gates (default: both presets)
    ///   run on a worker thread in the task worktree; per-gate progress
    ///   arrives as `task:gate` events, the finished record via
    ///   `task.gates`. Refuses a dirty worktree (checkpoint first) and a
    ///   task without checkpoints.
    /// - `task.gates {repo, task_id}` → `{running, candidate?, evidence?}`
    ///   | `{running, stale: true}` | `{running}` — the evidence view.
    fn dispatch_task(&self, method: &str, req: RpcRequest) -> Result<RpcResponse, RpcError> {
        let id = req.id.clone();
        let answer = match method {
            "list" => self.task_list(req.params).and_then(|v| to_json(method, v)),
            "create" => self
                .task_create(req.params)
                .and_then(|v| to_json(method, v)),
            "status" => self
                .task_status(req.params)
                .and_then(|v| to_json(method, v)),
            "checkpoint" => self
                .task_checkpoint(req.params)
                .map(|commit| serde_json::json!({ "commit": commit })),
            "diff" => self.task_diff(req.params).and_then(|v| to_json(method, v)),
            "approve" => self
                .task_approve(req.params)
                .map(|approved| serde_json::json!({ "approved": approved })),
            "merge" => self.task_merge(req.params).and_then(|v| to_json(method, v)),
            "rebase" => self
                .task_rebase(req.params)
                .and_then(|v| to_json(method, v)),
            "reset" => self.task_reset(req.params).map(|()| serde_json::json!({})),
            "discard" => self
                .task_discard(req.params)
                .map(|()| serde_json::json!({})),
            "run_gates" => self
                .task_run_gates(req.params)
                .and_then(|v| to_json(method, v)),
            "gates" => self.task_gates(req.params).and_then(|v| to_json(method, v)),
            other => {
                return Err(RpcError::new(
                    METHOD_NOT_FOUND,
                    format!("unknown method: task.{other}"),
                ))
            }
        };
        answer.map(|result| RpcResponse::ok(id, result))
    }

    fn task_list(&self, params: serde_json::Value) -> Result<Vec<TaskSummary>, RpcError> {
        let params: RepoParams = parse_params("task.list", params)?;
        require_non_empty("task.list", "repo", &params.repo)?;
        self.tasks.list(&params.repo).map_err(task_error)
    }

    fn task_create(
        &self,
        params: serde_json::Value,
    ) -> Result<crate::task_host::TaskCreated, RpcError> {
        let params: TaskCreateParams = parse_params("task.create", params)?;
        require_non_empty("task.create", "repo", &params.repo)?;
        // task_id: no auto-generation on purpose — naming a task is the
        // UI's decision; charset validation is tiller-git's (InvalidTaskId).
        require_non_empty("task.create", "task_id", &params.task_id)?;
        self.tasks
            .create(&params.repo, &params.task_id)
            .map_err(task_error)
    }

    fn task_status(
        &self,
        params: serde_json::Value,
    ) -> Result<crate::task_host::TaskStatusInfo, RpcError> {
        let params: TaskRefParams = parse_params("task.status", params)?;
        require_non_empty("task.status", "repo", &params.repo)?;
        self.tasks
            .status(&params.repo, &params.task_id)
            .map_err(task_error)
    }

    fn task_checkpoint(&self, params: serde_json::Value) -> Result<String, RpcError> {
        let params: CheckpointParams = parse_params("task.checkpoint", params)?;
        require_non_empty("task.checkpoint", "repo", &params.repo)?;
        require_non_empty("task.checkpoint", "message", &params.message)?;
        self.tasks
            .checkpoint(&params.repo, &params.task_id, &params.message)
            .map_err(task_error)
    }

    fn task_diff(&self, params: serde_json::Value) -> Result<tiller_git::CandidateDiff, RpcError> {
        let params: DiffParams = parse_params("task.diff", params)?;
        require_non_empty("task.diff", "repo", &params.repo)?;
        self.tasks
            .diff(&params.repo, &params.task_id, params.path.as_deref())
            .map_err(task_error)
    }

    fn task_approve(&self, params: serde_json::Value) -> Result<String, RpcError> {
        let params: TaskRefParams = parse_params("task.approve", params)?;
        require_non_empty("task.approve", "repo", &params.repo)?;
        self.tasks
            .approve(&params.repo, &params.task_id)
            .map_err(task_error)
    }

    fn task_merge(&self, params: serde_json::Value) -> Result<tiller_git::MergeOutcome, RpcError> {
        let params: TaskRefParams = parse_params("task.merge", params)?;
        require_non_empty("task.merge", "repo", &params.repo)?;
        self.tasks
            .merge(&params.repo, &params.task_id)
            .map_err(task_error)
    }

    fn task_rebase(
        &self,
        params: serde_json::Value,
    ) -> Result<crate::task_host::TaskRebased, RpcError> {
        let params: TaskRefParams = parse_params("task.rebase", params)?;
        require_non_empty("task.rebase", "repo", &params.repo)?;
        self.tasks
            .rebase(&params.repo, &params.task_id)
            .map_err(task_error)
    }

    fn task_reset(&self, params: serde_json::Value) -> Result<(), RpcError> {
        let params: TaskRefParams = parse_params("task.reset", params)?;
        require_non_empty("task.reset", "repo", &params.repo)?;
        self.tasks
            .reset(&params.repo, &params.task_id)
            .map_err(task_error)
    }

    fn task_discard(&self, params: serde_json::Value) -> Result<(), RpcError> {
        let params: TaskRefParams = parse_params("task.discard", params)?;
        require_non_empty("task.discard", "repo", &params.repo)?;
        self.tasks
            .discard(&params.repo, &params.task_id)
            .map_err(task_error)
    }

    /// `task.run_gates`: validate the gate list (default: every preset in
    /// run order), then hand the async start to the task host. The answer
    /// is immediate; results live in `task:gate` events + `task.gates`.
    fn task_run_gates(&self, params: serde_json::Value) -> Result<GatesStarted, RpcError> {
        let params: RunGatesParams = parse_params("task.run_gates", params)?;
        require_non_empty("task.run_gates", "repo", &params.repo)?;
        let known = GATE_PRESETS.iter().map(|p| p.id).collect::<Vec<_>>();
        let gate_ids: Vec<String> = match params.gates {
            None => known.iter().map(|id| id.to_string()).collect(),
            Some(ids) => {
                if ids.is_empty() {
                    return Err(invalid_params(
                        "task.run_gates",
                        "`gates` must name at least one gate",
                    ));
                }
                for id in &ids {
                    if gate_preset(id).is_none() {
                        return Err(invalid_params(
                            "task.run_gates",
                            &format!("unknown gate id {id:?}; known: {}", known.join(", ")),
                        ));
                    }
                }
                ids
            }
        };
        let gate_ids: Vec<&str> = gate_ids.iter().map(String::as_str).collect();
        self.tasks
            .start_gates(&params.repo, &params.task_id, &gate_ids)
            .map_err(gate_start_error)
    }

    /// `task.gates`: the evidence view (fresh / stale / none + running).
    fn task_gates(&self, params: serde_json::Value) -> Result<GatesInfo, RpcError> {
        let params: TaskRefParams = parse_params("task.gates", params)?;
        require_non_empty("task.gates", "repo", &params.repo)?;
        self.tasks
            .gates(&params.repo, &params.task_id)
            .map_err(task_error)
    }

    fn session_observe(&self, params: serde_json::Value) -> Result<Vec<TimelineEntry>, RpcError> {
        let params: ObserveParams = parse_params("session.observe", params)?;
        self.terms
            .observe_entries(params.session_id)
            .map_err(term_error)
    }

    fn term_spawn(&self, params: serde_json::Value) -> Result<SessionId, RpcError> {
        let params: SpawnParams = parse_params("term.spawn", params)?;
        if params.program.trim().is_empty() {
            return Err(invalid_params("term.spawn", "`program` must not be empty"));
        }
        if params.cwd.trim().is_empty() {
            return Err(invalid_params("term.spawn", "`cwd` must not be empty"));
        }
        if params.cols == 0 || params.rows == 0 {
            return Err(invalid_params(
                "term.spawn",
                "`cols` and `rows` must be >= 1",
            ));
        }
        let spec = SpawnSpec {
            program: params.program,
            args: params.args,
            env: params.env.into_iter().collect(),
            cwd: Some(PathBuf::from(&params.cwd)),
            cols: params.cols,
            rows: params.rows,
        };
        self.terms.spawn_session(spec).map_err(term_error)
    }

    fn term_write(&self, params: serde_json::Value) -> Result<(), RpcError> {
        let params: WriteParams = parse_params("term.write", params)?;
        let bytes = decode_data("term.write", &params.data)?;
        self.terms
            .write(params.session_id, &bytes)
            .map_err(term_error)
    }

    fn term_resize(&self, params: serde_json::Value) -> Result<(), RpcError> {
        let params: ResizeParams = parse_params("term.resize", params)?;
        if params.cols == 0 || params.rows == 0 {
            return Err(invalid_params(
                "term.resize",
                "`cols` and `rows` must be >= 1",
            ));
        }
        self.terms
            .resize(params.session_id, params.cols, params.rows)
            .map_err(term_error)
    }

    fn term_kill(&self, params: serde_json::Value) -> Result<(), RpcError> {
        let params: KillParams = parse_params("term.kill", params)?;
        self.terms.kill(params.session_id).map_err(term_error)
    }

    fn term_stats(
        &self,
        params: serde_json::Value,
    ) -> Result<crate::term_host::SessionStats, RpcError> {
        let params: KillParams = parse_params("term.stats", params)?;
        self.terms.stats(params.session_id).map_err(term_error)
    }
}

impl Dispatcher for CockpitDispatcher {
    fn dispatch(&self, req: RpcRequest) -> Result<RpcResponse, RpcError> {
        match req.method.strip_prefix("term.").map(str::to_string) {
            // The whole term.* surface is ours…
            Some(rest) => self.dispatch_term(&rest, req),
            None => match req.method.strip_prefix("session.").map(str::to_string) {
                // …the session.* timeline surface likewise…
                Some(rest) => self.dispatch_session(&rest, req),
                None => match req.method.strip_prefix("task.").map(str::to_string) {
                    // …the task.* review surface over tiller-git…
                    Some(rest) => self.dispatch_task(&rest, req),
                    // …everything else (core.* today, later core surfaces) goes
                    // to the core dispatcher verbatim, including its
                    // METHOD_NOT_FOUND.
                    None => self.core.dispatch(req),
                },
            },
        }
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(
    method: &str,
    params: serde_json::Value,
) -> Result<T, RpcError> {
    if !params.is_object() {
        return Err(invalid_params(method, "params must be an object"));
    }
    serde_json::from_value(params).map_err(|e| invalid_params(method, &format!("bad params: {e}")))
}

fn invalid_params(method: &str, message: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, format!("{method}: {message}"))
}

fn decode_data(method: &str, data: &str) -> Result<Vec<u8>, RpcError> {
    base64_decode(data).map_err(|e| invalid_params(method, &format!("`data` is not base64: {e}")))
}

fn require_non_empty(method: &str, field: &str, value: &str) -> Result<(), RpcError> {
    if value.trim().is_empty() {
        return Err(invalid_params(
            method,
            &format!("`{field}` must not be empty"),
        ));
    }
    Ok(())
}

fn to_json<T: Serialize>(method: &str, value: T) -> Result<serde_json::Value, RpcError> {
    serde_json::to_value(value).map_err(|e| {
        RpcError::new(
            INTERNAL_ERROR,
            format!("{method}: serialization failed: {e}"),
        )
    })
}

fn term_error(error: TermError) -> RpcError {
    match error {
        TermError::SessionGone(id) => {
            RpcError::new(SESSION_NOT_FOUND, format!("no live session {id}"))
        }
        TermError::Io(message) => RpcError::new(
            tiller_rpc::INTERNAL_ERROR,
            format!("term operation failed: {message}"),
        ),
    }
}

/// Application error codes for `task.*`: one stable negative number per
/// [`TransactionError`] variant, in the JSON-RPC *server-error* range
/// (-32000..-32099) right after [`SESSION_NOT_FOUND`] (-32001).
///
/// Convention (P6): the numeric `code` pins the **variant** for generic
/// JSON-RPC tooling; the variant's stable string code
/// ([`TransactionError::code`]) rides in `data.code` and is what the UI
/// branches on; conflict detail paths (`rebase_conflict`, `dirty_worktree`)
/// ride in `data.paths`. Numbers follow the variant order in `error.rs`,
/// with the two internal classes (git2/io, -32031/-32032) last so the
/// -3201x/-3202x block stays purely semantic. Changing an assigned number
/// is a wire break — add new variants after the last one instead.
fn task_error(error: TransactionError) -> RpcError {
    let (code, str_code, paths): (i64, &str, Option<Vec<String>>) = match &error {
        TransactionError::InvalidTaskId(_) => (-32010, "invalid_task_id", None),
        TransactionError::BareRepository => (-32011, "bare_repository", None),
        TransactionError::NoHeadCommit => (-32012, "no_head_commit", None),
        TransactionError::AlreadyExists(_) => (-32013, "already_exists", None),
        TransactionError::UnknownTask(_) => (-32014, "unknown_task", None),
        TransactionError::NothingToCommit => (-32015, "nothing_to_commit", None),
        TransactionError::NoCheckpoints => (-32016, "no_checkpoints", None),
        TransactionError::NoApproval => (-32017, "no_approval", None),
        TransactionError::StaleApproval(_) => (-32018, "stale_approval", None),
        TransactionError::MainWorktreeDirty => (-32019, "main_worktree_dirty", None),
        TransactionError::MainDetachedHead => (-32020, "main_detached_head", None),
        TransactionError::DirtyWorktree { details } => {
            (-32021, "dirty_worktree", Some(details.clone()))
        }
        TransactionError::RebaseConflict { paths } => {
            (-32022, "rebase_conflict", Some(paths.clone()))
        }
        TransactionError::Git { op, source } => {
            (-32031, "git_error", Some(vec![format!("{op}: {source}")]))
        }
        TransactionError::Io { op, source } => {
            (-32032, "io_error", Some(vec![format!("{op}: {source}")]))
        }
    };
    let mut data = serde_json::json!({ "code": str_code });
    if let Some(paths) = paths {
        data["paths"] = serde_json::json!(paths);
    }
    let mut error = RpcError::new(code, format!("task: {error}"));
    error.data = Some(data);
    error
}

/// Gate-start preconditions map to their own stable codes, continuing the
/// `task.*` block (see [`task_error`]): a dirty worktree is not a git
/// failure, it is "checkpoint first" — and the UI needs to tell a
/// rejected double-start apart from both.
fn gate_start_error(error: GateStartError) -> RpcError {
    match error {
        GateStartError::Git(inner) => task_error(inner),
        GateStartError::Dirty { files } => {
            let mut rpc = RpcError::new(
                -32023,
                format!(
                    "task: gates need a clean worktree — commit first (Checkpoint); \
                     uncommitted/untracked changes: {}",
                    files.join(", ")
                ),
            );
            rpc.data = Some(serde_json::json!({
                "code": "gates_dirty_worktree",
                "paths": files,
            }));
            rpc
        }
        GateStartError::AlreadyRunning => {
            let mut rpc = RpcError::new(
                -32024,
                "task: a gate run is already active for this task".to_owned(),
            );
            rpc.data = Some(serde_json::json!({ "code": "gates_already_running" }));
            rpc
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnParams {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: String,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteParams {
    session_id: SessionId,
    data: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResizeParams {
    session_id: SessionId,
    cols: u16,
    rows: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KillParams {
    session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveParams {
    session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoParams {
    repo: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskCreateParams {
    repo: String,
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRefParams {
    repo: String,
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointParams {
    repo: String,
    task_id: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffParams {
    repo: String,
    task_id: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunGatesParams {
    repo: String,
    task_id: String,
    /// Gate ids in run order; default: every preset.
    #[serde(default)]
    gates: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term_host::test_util::{manager_with, FakeFactory};
    use tiller_rpc::RequestId;

    fn dispatcher() -> (CockpitDispatcher, std::sync::Arc<FakeFactory>) {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory.clone());
        (
            CockpitDispatcher::new(
                manager,
                std::sync::Arc::new(crate::task_host::TaskHost::new(
                    std::sync::Arc::new(crate::term_host::test_util::NoopSink),
                    std::sync::Arc::new(crate::gates::ProcessRunner),
                )),
            ),
            factory,
        )
    }

    fn dispatch_ok(
        dispatcher: &CockpitDispatcher,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let response = dispatcher
            .dispatch(RpcRequest {
                id: RequestId::Num(1),
                method: method.to_string(),
                params,
            })
            .expect("ok response");
        response.result.expect("result present")
    }

    fn dispatch_err(
        dispatcher: &CockpitDispatcher,
        method: &str,
        params: serde_json::Value,
    ) -> RpcError {
        dispatcher
            .dispatch(RpcRequest {
                id: RequestId::Num(1),
                method: method.to_string(),
                params,
            })
            .expect_err("error response")
    }

    fn spawn_params() -> serde_json::Value {
        serde_json::json!({
            "program": "fake",
            "cwd": "C:/Repos/tiller",
        })
    }

    #[test]
    fn spawn_defaults_and_result() {
        let (dispatcher, factory) = dispatcher();
        let result = dispatch_ok(&dispatcher, "term.spawn", spawn_params());
        assert_eq!(result["session_id"], 1);
        let specs = factory.specs.lock().unwrap();
        let spec = &specs[0];
        assert_eq!(spec.cols, 80);
        assert_eq!(spec.rows, 24);
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
        assert_eq!(spec.cwd, Some(PathBuf::from("C:/Repos/tiller")));
    }

    #[test]
    fn spawn_honors_explicit_fields() {
        let (dispatcher, factory) = dispatcher();
        let result = dispatch_ok(
            &dispatcher,
            "term.spawn",
            serde_json::json!({
                "program": "fake",
                "args": ["-a"],
                "cwd": "C:/tmp",
                "cols": 120,
                "rows": 40,
                "env": { "FOO": "bar" },
            }),
        );
        assert_eq!(result["session_id"], 1);
        let specs = factory.specs.lock().unwrap();
        let spec = &specs[0];
        assert_eq!(spec.cols, 120);
        assert_eq!(spec.rows, 40);
        assert_eq!(spec.args, vec!["-a".to_string()]);
        assert_eq!(spec.env, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn spawn_validation_errors() {
        let (dispatcher, _factory) = dispatcher();
        for (params, message_part) in [
            (serde_json::json!({ "cwd": "C:/tmp" }), "program"),
            (serde_json::json!({ "program": "fake" }), "cwd"),
            (serde_json::json!({ "program": "fake", "cwd": "" }), "cwd"),
            (
                serde_json::json!({ "program": "fake", "cwd": "   " }),
                "cwd",
            ),
            (
                serde_json::json!({ "program": "fake", "cwd": "C:/tmp", "cols": 0 }),
                "cols",
            ),
            (
                serde_json::json!({ "program": "fake", "cwd": "C:/tmp", "rows": 0 }),
                "rows",
            ),
            (
                serde_json::json!({ "program": "fake", "cwd": "C:/tmp", "env": [1, 2] }),
                "bad params",
            ),
            (
                serde_json::json!({ "program": "fake", "cwd": "C:/tmp", "extra": true }),
                "bad params",
            ),
            (
                serde_json::json!(["not", "an", "object"]),
                "params must be an object",
            ),
        ] {
            let error = dispatch_err(&dispatcher, "term.spawn", params);
            assert_eq!(error.code, INVALID_PARAMS, "{error:?}");
            assert!(
                error.message.contains(message_part),
                "message {:?} should mention {message_part}",
                error.message
            );
        }
    }

    #[test]
    fn write_decodes_base64_and_reports_gone_sessions() {
        let (dispatcher, factory) = dispatcher();
        dispatch_ok(&dispatcher, "term.spawn", spawn_params());
        // base64("hi!") = "aGkh"
        let result = dispatch_ok(
            &dispatcher,
            "term.write",
            serde_json::json!({ "session_id": 1, "data": "aGkh" }),
        );
        assert_eq!(result, serde_json::json!({}));
        let session = factory.session(0);
        assert_eq!(*session.writes.lock().unwrap(), vec![b"hi!".to_vec()]);

        let error = dispatch_err(
            &dispatcher,
            "term.write",
            serde_json::json!({ "session_id": 99, "data": "aGkh" }),
        );
        assert_eq!(error.code, SESSION_NOT_FOUND);
        assert!(error.message.contains("99"));

        let error = dispatch_err(
            &dispatcher,
            "term.write",
            serde_json::json!({ "session_id": 1, "data": "not base64!" }),
        );
        assert_eq!(error.code, INVALID_PARAMS);
    }

    #[test]
    fn resize_kill_stats_flow() {
        let (dispatcher, factory) = dispatcher();
        dispatch_ok(&dispatcher, "term.spawn", spawn_params());
        dispatch_ok(
            &dispatcher,
            "term.resize",
            serde_json::json!({ "session_id": 1, "cols": 100, "rows": 30 }),
        );
        dispatch_ok(
            &dispatcher,
            "term.kill",
            serde_json::json!({ "session_id": 1 }),
        );
        let session = factory.session(0);
        assert_eq!(*session.resizes.lock().unwrap(), vec![(100, 30)]);
        assert!(*session.killed.lock().unwrap());

        dispatch_ok(
            &dispatcher,
            "term.stats",
            serde_json::json!({ "session_id": 1 }),
        );
        for (method, params) in [
            (
                "term.resize",
                serde_json::json!({ "session_id": 9, "cols": 1, "rows": 1 }),
            ),
            ("term.kill", serde_json::json!({ "session_id": 9 })),
            ("term.stats", serde_json::json!({ "session_id": 9 })),
        ] {
            let error = dispatch_err(&dispatcher, method, params);
            assert_eq!(error.code, SESSION_NOT_FOUND, "{method}: {error:?}");
        }
    }

    #[test]
    fn resize_zero_is_invalid_params() {
        let (dispatcher, _factory) = dispatcher();
        let error = dispatch_err(
            &dispatcher,
            "term.resize",
            serde_json::json!({ "session_id": 1, "cols": 0, "rows": 10 }),
        );
        assert_eq!(error.code, INVALID_PARAMS);
    }

    #[test]
    fn unknown_term_method_is_method_not_found() {
        let (dispatcher, _factory) = dispatcher();
        let error = dispatch_err(&dispatcher, "term.nope", serde_json::json!({}));
        assert_eq!(error.code, METHOD_NOT_FOUND);
        assert!(error.message.contains("term.nope"));
    }

    #[test]
    fn session_observe_snapshots_the_session_file() {
        let (dispatcher, factory) = dispatcher();
        // A pi session (fake PTY) with a session file already written.
        dispatch_ok(
            &dispatcher,
            "term.spawn",
            serde_json::json!({ "program": "pi", "cwd": "C:/Repos/tiller" }),
        );
        let sandbox = factory.specs.lock().unwrap()[0]
            .env
            .iter()
            .find(|(k, _)| k == "PI_CODING_AGENT_DIR")
            .map(|(_, v)| std::path::PathBuf::from(v))
            .expect("sandbox env");
        let sessions = sandbox.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("s.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"u","timestamp":"t"}"#,
                "\n",
                r#"{"type":"message","id":"1","timestamp":"t1","message":{"role":"user","content":[{"type":"text","text":"snapshot me"}]}}"#,
                "\n",
            ),
        )
        .unwrap();

        let result = dispatch_ok(
            &dispatcher,
            "session.observe",
            serde_json::json!({ "session_id": 1 }),
        );
        assert_eq!(
            result["entries"],
            serde_json::json!([
                { "kind": "message", "id": "1", "role": "user", "ts": "t1", "chars": 11, "blocks": 1 }
            ])
        );
    }

    #[test]
    fn session_observe_empty_without_file_and_errors_for_foreign_ids() {
        let (dispatcher, _factory) = dispatcher();
        // No session file yet (pi creates it lazily): empty, not an error.
        dispatch_ok(
            &dispatcher,
            "term.spawn",
            serde_json::json!({ "program": "pi", "cwd": "C:/Repos/tiller" }),
        );
        let result = dispatch_ok(
            &dispatcher,
            "session.observe",
            serde_json::json!({ "session_id": 1 }),
        );
        assert_eq!(result["entries"], serde_json::json!([]));

        // A foreign id: SESSION_NOT_FOUND, consistently with term.*.
        let error = dispatch_err(
            &dispatcher,
            "session.observe",
            serde_json::json!({ "session_id": 99 }),
        );
        assert_eq!(error.code, SESSION_NOT_FOUND);
        assert!(error.message.contains("99"));
    }

    #[test]
    fn session_observe_validates_params() {
        let (dispatcher, _factory) = dispatcher();
        for (params, message_part) in [
            (serde_json::json!({}), "session_id"),
            (serde_json::json!({ "session_id": "one" }), "bad params"),
            (
                serde_json::json!({ "session_id": 1, "extra": true }),
                "bad params",
            ),
            (serde_json::json!([1]), "params must be an object"),
        ] {
            let error = dispatch_err(&dispatcher, "session.observe", params);
            assert_eq!(error.code, INVALID_PARAMS, "{error:?}");
            assert!(
                error.message.contains(message_part),
                "message {:?} should mention {message_part}",
                error.message
            );
        }
        let error = dispatch_err(&dispatcher, "session.nope", serde_json::json!({}));
        assert_eq!(error.code, METHOD_NOT_FOUND);
        assert!(error.message.contains("session.nope"));
    }

    #[test]
    fn core_methods_delegate_verbatim() {
        let (dispatcher, _factory) = dispatcher();
        let result = dispatch_ok(&dispatcher, "core.version", serde_json::Value::Null);
        assert!(result["version"].is_string());

        let error = dispatch_err(&dispatcher, "nope.what", serde_json::json!({}));
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }

    #[test]
    fn task_error_codes_are_stable_per_variant() {
        use tiller_git::TransactionError;

        // The numeric code pins the variant; data.code carries the stable
        // string code the UI branches on. Both halves of every mapping are
        // pinned here — changing a number is a wire break (see task_error).
        let cases: Vec<(TransactionError, i64, &str)> = vec![
            (
                TransactionError::InvalidTaskId("x".into()),
                -32010,
                "invalid_task_id",
            ),
            (TransactionError::BareRepository, -32011, "bare_repository"),
            (TransactionError::NoHeadCommit, -32012, "no_head_commit"),
            (
                TransactionError::AlreadyExists("x".into()),
                -32013,
                "already_exists",
            ),
            (
                TransactionError::UnknownTask("x".into()),
                -32014,
                "unknown_task",
            ),
            (
                TransactionError::NothingToCommit,
                -32015,
                "nothing_to_commit",
            ),
            (TransactionError::NoCheckpoints, -32016, "no_checkpoints"),
            (TransactionError::NoApproval, -32017, "no_approval"),
            (
                TransactionError::StaleApproval(tiller_git::ApprovalState::Valid),
                -32018,
                "stale_approval",
            ),
            (
                TransactionError::MainWorktreeDirty,
                -32019,
                "main_worktree_dirty",
            ),
            (
                TransactionError::MainDetachedHead,
                -32020,
                "main_detached_head",
            ),
            (
                TransactionError::DirtyWorktree {
                    details: vec!["uncommitted: x".into()],
                },
                -32021,
                "dirty_worktree",
            ),
            (
                TransactionError::RebaseConflict {
                    paths: vec!["a.txt".into()],
                },
                -32022,
                "rebase_conflict",
            ),
            (
                TransactionError::Git {
                    op: "op",
                    source: git2::Error::from_str("boom"),
                },
                -32031,
                "git_error",
            ),
            (
                TransactionError::Io {
                    op: "op",
                    source: std::io::Error::other("boom"),
                },
                -32032,
                "io_error",
            ),
        ];
        for (error, code, str_code) in cases {
            let rpc = task_error(error);
            assert_eq!(rpc.code, code, "code {code}");
            let data = rpc.data.clone().expect("data present");
            assert_eq!(data["code"], str_code, "data.code {code}");
        }
        // conflict detail paths ride in data.paths
        let rpc = task_error(TransactionError::RebaseConflict {
            paths: vec!["a.txt".into()],
        });
        assert_eq!(rpc.data.unwrap()["paths"], serde_json::json!(["a.txt"]));
    }

    #[test]
    fn task_surface_validates_params_and_reports_unknown_methods() {
        let (dispatcher, _factory) = dispatcher();
        let error = dispatch_err(&dispatcher, "task.nope", serde_json::json!({}));
        assert_eq!(error.code, METHOD_NOT_FOUND);
        assert!(error.message.contains("task.nope"));
        let error = dispatch_err(&dispatcher, "task.list", serde_json::json!({ "repo": "" }));
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(error.message.contains("repo"));
        // A task host exists in the dispatcher (per-repo serialization is
        // part of the construction contract, see task_host.rs).
        let _: &TaskHost = &dispatcher.tasks;
    }

    #[test]
    fn run_gates_validates_the_gate_list_before_starting_anything() {
        let (dispatcher, _factory) = dispatcher();
        // unknown id → INVALID_PARAMS, naming the id and the known ones
        let error = dispatch_err(
            &dispatcher,
            "task.run_gates",
            serde_json::json!({ "repo": "C:/not/here", "task_id": "t", "gates": ["cargo-check", "lint"] }),
        );
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(error.message.contains("lint"));
        assert!(error.message.contains("cargo-test"));
        // empty list → INVALID_PARAMS (naming zero gates is a client bug)
        let error = dispatch_err(
            &dispatcher,
            "task.run_gates",
            serde_json::json!({ "repo": "C:/not/here", "task_id": "t", "gates": [] }),
        );
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(error.message.contains("at least one"));
        // unknown fields rejected like everywhere else
        let error = dispatch_err(
            &dispatcher,
            "task.run_gates",
            serde_json::json!({ "repo": "C:/not/here", "task_id": "t", "extra": 1 }),
        );
        assert_eq!(error.code, INVALID_PARAMS);
        // a valid list reaches the host and fails on the real seam (no
        // such repo) — never on validation: the error is a task-level
        // code, not INVALID_PARAMS
        let error = dispatch_err(
            &dispatcher,
            "task.run_gates",
            serde_json::json!({ "repo": "C:/not/here", "task_id": "t", "gates": ["cargo-test"] }),
        );
        assert_ne!(error.code, INVALID_PARAMS, "{error:?}");
        assert!(error.code <= -32010, "a task.* app code: {error:?}");
        // gates view validates too
        let error = dispatch_err(
            &dispatcher,
            "task.gates",
            serde_json::json!({ "task_id": "t" }),
        );
        assert_eq!(error.code, INVALID_PARAMS);
    }
}
