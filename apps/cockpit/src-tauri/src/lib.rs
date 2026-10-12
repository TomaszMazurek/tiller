//! Cockpit shell: Tauri commands bridging the UI to the core over the
//! narrow JSON-RPC seam, plus the PTY session host behind `term.*`.

mod dispatch;
mod sandbox;
mod term_host;

use std::sync::Arc;

use tiller_rpc::{Dispatcher, RequestId, RpcError, RpcRequest, RpcResponse, INVALID_PARAMS};

// Re-exported for the integration tests and the `pi_smoke` example.
pub use dispatch::{CockpitDispatcher, SESSION_NOT_FOUND};
pub use term_host::{
    EventSink, PtySessionFactory, SessionId, SessionManager, SessionStats, SpawnSpec, TermError,
    TermExit, EVENT_EXIT, EVENT_OUTPUT,
};

/// Tauri command bridging the UI to the cockpit dispatcher.
///
/// Contract: the answer is always a serialized `RpcResponse`; a request that
/// cannot be parsed (or a response that cannot be serialized) maps to
/// `INVALID_PARAMS` instead of a panic or a bare string error on the wire.
#[tauri::command]
fn rpc(state: tauri::State<'_, Arc<CockpitDispatcher>>, json: String) -> Result<String, String> {
    let response = dispatch_json(state.inner().as_ref(), &json);
    serde_json::to_string(&response).map_err(|err| err.to_string())
}

fn dispatch_json<D: Dispatcher>(dispatcher: &D, json: &str) -> RpcResponse {
    match serde_json::from_str::<RpcRequest>(json) {
        Ok(request) => {
            let id = request.id.clone();
            match dispatcher.dispatch(request) {
                Ok(response) => response,
                Err(error) => RpcResponse::error(id, error),
            }
        }
        Err(error) => RpcResponse::error(
            RequestId::Num(0),
            RpcError::new(INVALID_PARAMS, format!("malformed request: {error}")),
        ),
    }
}

/// The production [`EventSink`]: Tauri events. `Emitter::emit` is
/// thread-safe, so the session actor threads call it directly.
struct AppEventSink {
    app: tauri::AppHandle,
}

impl EventSink for AppEventSink {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        use tauri::Emitter;
        // A failed emit means no webview listener matched (window closing):
        // the session actor must keep draining either way, so the error is
        // swallowed.
        let _ = self.app.emit(event, payload);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            use tauri::Manager;
            let cache_dir = app
                .path()
                .app_cache_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("tiller-cockpit"));
            let sink = Arc::new(AppEventSink {
                app: app.handle().clone(),
            });
            let manager = SessionManager::new(sink, cache_dir, Arc::new(PtySessionFactory));
            app.manage(Arc::new(CockpitDispatcher::new(manager)));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![rpc])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term_host::test_util::{manager_with, FakeFactory};
    use serde_json::json;
    use tiller_rpc::CoreDispatcher;

    #[test]
    fn bridges_core_version_requests() {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory);
        let dispatcher = Arc::new(CockpitDispatcher::new(manager));
        let raw = dispatch_json(dispatcher.as_ref(), r#"{"id":1,"method":"core.version"}"#);
        assert_eq!(raw.error, None);
        // The bridge must forward the dispatcher's answer verbatim; the
        // expected value comes from `CoreDispatcher` itself, so this crate
        // needs no direct `tiller-core` dependency.
        let expected = CoreDispatcher
            .dispatch(
                serde_json::from_str::<RpcRequest>(r#"{"id":1,"method":"core.version"}"#)
                    .expect("valid request"),
            )
            .expect("core.version dispatch")
            .result;
        assert_eq!(raw.result, expected);
    }

    #[test]
    fn malformed_json_maps_to_invalid_params() {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory);
        let dispatcher = Arc::new(CockpitDispatcher::new(manager));
        let raw = dispatch_json(dispatcher.as_ref(), "not json at all");
        let error = raw.error.expect("error present");
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(error.message.contains("malformed request"));
    }

    #[test]
    fn unknown_method_maps_to_method_not_found() {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory);
        let dispatcher = Arc::new(CockpitDispatcher::new(manager));
        let raw = dispatch_json(dispatcher.as_ref(), r#"{"id":"a","method":"nope"}"#);
        let error = raw.error.expect("error present");
        assert_eq!(error.code, tiller_rpc::METHOD_NOT_FOUND);
        assert_eq!(raw.id, RequestId::Str("a".to_string()));
    }

    #[test]
    fn bridges_term_methods_with_base64_payloads() {
        let factory = FakeFactory::new();
        let (manager, _sink, _root) = manager_with(factory);
        let dispatcher = Arc::new(CockpitDispatcher::new(manager));
        let spawn = dispatch_json(
            dispatcher.as_ref(),
            r#"{"id":1,"method":"term.spawn","params":{"program":"fake","cwd":"C:/tmp"}}"#,
        );
        assert_eq!(spawn.result, Some(json!({ "session_id": 1 })));
        let write = dispatch_json(
            dispatcher.as_ref(),
            r#"{"id":2,"method":"term.write","params":{"session_id":1,"data":"aGkh"}}"#,
        );
        assert_eq!(write.result, Some(json!({})));
        let gone = dispatch_json(
            dispatcher.as_ref(),
            r#"{"id":3,"method":"term.kill","params":{"session_id":9}}"#,
        );
        let error = gone.error.expect("session gone");
        assert_eq!(error.code, SESSION_NOT_FOUND);
    }
}
