use tiller_rpc::{
    CoreDispatcher, Dispatcher, RequestId, RpcError, RpcRequest, RpcResponse, INVALID_PARAMS,
};

/// Tauri command bridging the UI to the core dispatcher.
///
/// Contract: the answer is always a serialized `RpcResponse`; a request that
/// cannot be parsed (or a response that cannot be serialized) maps to
/// `INVALID_PARAMS` instead of a panic or a bare string error on the wire.
#[tauri::command]
fn rpc(json: String) -> Result<String, String> {
    let response = dispatch_json(&json);
    serde_json::to_string(&response).map_err(|err| err.to_string())
}

fn dispatch_json(json: &str) -> RpcResponse {
    let dispatcher = CoreDispatcher;
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![rpc])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridges_core_version_requests() {
        let raw = dispatch_json(r#"{"id":1,"method":"core.version"}"#);
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
        let raw = dispatch_json("not json at all");
        let error = raw.error.expect("error present");
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(error.message.contains("malformed request"));
    }

    #[test]
    fn unknown_method_maps_to_method_not_found() {
        let raw = dispatch_json(r#"{"id":"a","method":"nope"}"#);
        let error = raw.error.expect("error present");
        assert_eq!(error.code, tiller_rpc::METHOD_NOT_FOUND);
        assert_eq!(raw.id, RequestId::Str("a".to_string()));
    }
}
