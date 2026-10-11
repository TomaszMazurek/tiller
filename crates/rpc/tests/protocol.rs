use serde::{Deserialize, Serialize};
use tiller_core::{Event, ProcessState, ProcessStateChanged};
use tiller_rpc::{
    Notification, RequestId, RpcError, RpcRequest, RpcResponse, INVALID_PARAMS, METHOD_NOT_FOUND,
};

fn assert_roundtrip<T>(value: T)
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(&value).expect("serialization");
    let back: T = serde_json::from_str(&json).expect("deserialization");
    assert_eq!(back, value);
}

#[test]
fn request_roundtrips_with_numeric_id() {
    let request = RpcRequest {
        id: RequestId::Num(42),
        method: "core.version".to_string(),
        params: serde_json::json!({ "verbose": false }),
    };
    let json = serde_json::to_string(&request).expect("serialization");
    assert_eq!(
        json,
        r#"{"id":42,"method":"core.version","params":{"verbose":false}}"#
    );
    assert_roundtrip(request);
}

#[test]
fn request_roundtrips_with_string_id() {
    let request = RpcRequest {
        id: RequestId::Str("abc-1".to_string()),
        method: "core.version".to_string(),
        params: serde_json::Value::Null,
    };
    let json = serde_json::to_string(&request).expect("serialization");
    assert_eq!(
        json,
        r#"{"id":"abc-1","method":"core.version","params":null}"#
    );
    assert_roundtrip(request);
}

#[test]
fn request_params_are_optional_on_the_wire() {
    let parsed: RpcRequest =
        serde_json::from_str(r#"{"id":1,"method":"core.version"}"#).expect("deserialization");
    assert_eq!(parsed.id, RequestId::Num(1));
    assert_eq!(parsed.method, "core.version");
    assert_eq!(parsed.params, serde_json::Value::Null);
}

#[test]
fn response_roundtrips_success() {
    let response = RpcResponse {
        id: RequestId::Num(1),
        result: Some(serde_json::json!({ "version": "0.1.0" })),
        error: None,
    };
    let json = serde_json::to_string(&response).expect("serialization");
    assert_eq!(json, r#"{"id":1,"result":{"version":"0.1.0"}}"#);
    assert_roundtrip(response);
}

#[test]
fn response_roundtrips_error_with_data() {
    let response = RpcResponse {
        id: RequestId::Str("req-7".to_string()),
        result: None,
        error: Some(RpcError {
            code: METHOD_NOT_FOUND,
            message: "unknown method: nope".to_string(),
            data: Some(serde_json::json!({ "method": "nope" })),
        }),
    };
    let json = serde_json::to_string(&response).expect("serialization");
    assert_eq!(
        json,
        r#"{"id":"req-7","error":{"code":-32601,"message":"unknown method: nope","data":{"method":"nope"}}}"#
    );
    assert_roundtrip(response);
}

#[test]
fn error_omits_data_when_absent() {
    let error = RpcError {
        code: INVALID_PARAMS,
        message: "params must be an object".to_string(),
        data: None,
    };
    let json = serde_json::to_string(&error).expect("serialization");
    assert_eq!(
        json,
        r#"{"code":-32602,"message":"params must be an object"}"#
    );
    assert_roundtrip(error);
}

#[test]
fn notification_roundtrips() {
    // The wire convention for event payloads comes from `tiller_core::Event`
    // serialization (snake_case), so build the params from a real event
    // instead of hand-written JSON with a different casing.
    let event = Event::ProcessStateChanged(ProcessStateChanged {
        process_id: 1,
        state: ProcessState::Running,
    });
    let notification = Notification {
        method: "event.process_state_changed".to_string(),
        params: serde_json::to_value(&event).expect("event serialization"),
    };
    assert_roundtrip(notification);

    let parsed: Notification =
        serde_json::from_str(r#"{"method":"ping"}"#).expect("deserialization");
    assert_eq!(parsed.method, "ping");
    assert_eq!(parsed.params, serde_json::Value::Null);
}

#[test]
fn response_constructors_keep_the_wire_shape() {
    let ok = RpcResponse::ok(RequestId::Num(3), serde_json::json!({ "ok": true }));
    assert_eq!(
        serde_json::to_string(&ok).expect("serialization"),
        r#"{"id":3,"result":{"ok":true}}"#
    );

    let err = RpcResponse::error(
        RequestId::Num(3),
        RpcError::new(METHOD_NOT_FOUND, "unknown method: x"),
    );
    assert_eq!(
        serde_json::to_string(&err).expect("serialization"),
        r#"{"id":3,"error":{"code":-32601,"message":"unknown method: x"}}"#
    );
}
