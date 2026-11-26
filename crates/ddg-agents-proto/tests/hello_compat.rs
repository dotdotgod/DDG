use ddg_agents_proto::{host::HostHelloResult, rpc::error_code, terminal::HelloResult};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

fn check_hello<T: DeserializeOwned + Serialize>(legacy: Value, metadata: Value) {
    let round_trip = |wire| json!(serde_json::from_value::<T>(wire).unwrap());
    assert_eq!(round_trip(legacy.clone()), legacy);

    let mut extended = legacy.clone();
    extended["future_field"] = json!({"unknown": true});
    assert_eq!(round_trip(extended.clone()), legacy);

    let object = extended.as_object_mut().unwrap();
    object.remove("future_field");
    object.extend(metadata.as_object().unwrap().clone());
    for field in metadata.as_object().unwrap().keys() {
        let mut null = extended.clone();
        null[field] = Value::Null;
        assert!(round_trip(null).get(field).is_none(), "{field}");
    }
    assert_eq!(round_trip(extended.clone()), extended);
}

#[test]
fn hello_metadata_is_wire_compatible() {
    check_hello::<HelloResult>(
        json!({
            "protocol_version": 1, "server_id": "terminal", "boot_generation": 2,
            "conn_id": 3, "features": ["credit_v1"]
        }),
        json!({
            "app_version": "0.2.0", "platform": "macos-arm64", "holder_protocol_version": 1,
            "holder_instance": "instance", "active_pty_count": 0
        }),
    );
    check_hello::<HostHelloResult>(
        json!({
            "protocol_version": 1, "host_id": "host", "boot_generation": 2,
            "conn_id": 3, "features": ["credit_v1"]
        }),
        json!({
            "app_version": "0.2.0", "platform": "macos-arm64", "terminal_server_version": "0.2.0",
            "holder_protocol_version": 1, "active_pty_count": 0, "rollback_version": "0.1.0"
        }),
    );
}

#[test]
fn protocol_incompatible_error_code_is_stable() {
    assert_eq!(error_code::PROTOCOL_INCOMPATIBLE, "protocol_incompatible");
}
