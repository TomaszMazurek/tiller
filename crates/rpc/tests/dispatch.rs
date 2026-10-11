use tiller_core::CORE_VERSION;
use tiller_rpc::{
    CoreDispatcher, Dispatcher, RequestId, RpcError, RpcRequest, INVALID_PARAMS, METHOD_NOT_FOUND,
};

fn request(method: &str, params: serde_json::Value) -> RpcRequest {
    RpcRequest {
        id: RequestId::Num(1),
        method: method.to_string(),
        params,
    }
}

#[test]
fn core_version_returns_workspace_version() {
    let response = CoreDispatcher
        .dispatch(request("core.version", serde_json::Value::Null))
        .expect("core.version must succeed");

    assert_eq!(response.id, RequestId::Num(1));
    assert_eq!(response.error, None);
    assert_eq!(
        response.result,
        Some(serde_json::json!({ "version": CORE_VERSION }))
    );
}

#[test]
fn core_version_accepts_object_params() {
    let response = CoreDispatcher
        .dispatch(request("core.version", serde_json::json!({})))
        .expect("object params are valid");
    assert_eq!(
        response.result.expect("result present")["version"],
        CORE_VERSION
    );
}

#[test]
fn unknown_method_yields_method_not_found() {
    let error = CoreDispatcher
        .dispatch(request("nope.nope", serde_json::Value::Null))
        .expect_err("unknown method must fail");

    assert_eq!(error.code, METHOD_NOT_FOUND);
    assert!(
        error.message.contains("nope.nope"),
        "message should name the method: {error:?}"
    );
    assert_eq!(error.data, None);
}

#[test]
fn non_object_params_yield_invalid_params() {
    for params in [
        serde_json::json!([1, 2]),
        serde_json::json!("str"),
        serde_json::json!(7),
    ] {
        let error = CoreDispatcher
            .dispatch(request("core.version", params))
            .expect_err("non-object params must fail");
        assert_eq!(error.code, INVALID_PARAMS);
    }
}

#[test]
fn dispatcher_trait_is_object_safe_and_shareable() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CoreDispatcher>();

    let dispatcher: Box<dyn Dispatcher> = Box::new(CoreDispatcher);
    let response = dispatcher
        .dispatch(request("core.version", serde_json::Value::Null))
        .expect("dispatch through trait object");
    assert_eq!(
        response.result.expect("result present")["version"],
        CORE_VERSION
    );
}

#[test]
fn rpc_error_new_sets_code_and_message() {
    let error = RpcError::new(INVALID_PARAMS, "bad");
    assert_eq!(error.code, INVALID_PARAMS);
    assert_eq!(error.message, "bad");
    assert_eq!(error.data, None);
}
