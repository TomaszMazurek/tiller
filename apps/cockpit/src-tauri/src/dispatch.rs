//! The cockpit dispatcher: `core.*` delegated to [`CoreDispatcher`], the
//! `term.*` surface served by the session manager.
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
//!
//! Error codes: shape/decoding problems are `INVALID_PARAMS` (-32602);
//! unknown methods `METHOD_NOT_FOUND` (-32601); an id with no live session
//! is [`SESSION_NOT_FOUND`] — an application code in the JSON-RPC
//! *server-error* range (-32000..-32099), chosen over reusing
//! INVALID_PARAMS so the UI can tell "malformed request" apart from the
//! benign race "the program exited under you" without parsing messages.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use tiller_rpc::{
    CoreDispatcher, Dispatcher, RpcError, RpcRequest, RpcResponse, INVALID_PARAMS, METHOD_NOT_FOUND,
};

use crate::term_host::{base64_decode, SessionId, SessionManager, SpawnSpec, TermError};

/// Application error: the session id is not (or no longer) live.
pub const SESSION_NOT_FOUND: i64 = -32001;

/// The dispatcher behind the cockpit's `rpc` command.
pub struct CockpitDispatcher {
    core: CoreDispatcher,
    terms: SessionManager,
}

impl CockpitDispatcher {
    pub fn new(terms: SessionManager) -> Self {
        Self {
            core: CoreDispatcher,
            terms,
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
            // …everything else (core.* today, later core surfaces) goes to
            // the core dispatcher verbatim, including its METHOD_NOT_FOUND.
            None => self.core.dispatch(req),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term_host::test_util::{manager_with, FakeFactory};
    use tiller_rpc::RequestId;

    fn dispatcher() -> (CockpitDispatcher, std::sync::Arc<FakeFactory>) {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory.clone());
        (CockpitDispatcher::new(manager), factory)
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
    fn core_methods_delegate_verbatim() {
        let (dispatcher, _factory) = dispatcher();
        let result = dispatch_ok(&dispatcher, "core.version", serde_json::Value::Null);
        assert!(result["version"].is_string());

        let error = dispatch_err(&dispatcher, "nope.what", serde_json::json!({}));
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }
}
