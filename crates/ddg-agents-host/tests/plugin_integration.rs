#![cfg(unix)]

mod common;

use common::HostSandbox;
use ddg_agents_proto::rpc::ErrorKind;
use serde_json::{Value, json};
use std::path::Path;

#[test]
fn plugin_discovery_capability_sandbox_and_crash_isolation() {
    let mut sb = HostSandbox::new("plugins");
    let dir = sb.root.join("plugins/demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo_plugin.py"),
        dir.join("demo_plugin.py"),
    )
    .unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        r#"
name = "demo"
version = "0.1.0"
command = ["python3", "./demo_plugin.py"]
capabilities = ["host_read"]
timeout_ms = 5000
"#,
    )
    .unwrap();
    sb.start_host();
    let mut c = sb.connect();

    let disc: Value = c.call("rpc.discover", json!({})).unwrap();
    let methods = disc["methods"].as_array().unwrap();
    assert!(methods.iter().any(|m| m["name"] == "host.room_create"));
    assert!(methods.iter().any(|m| m["name"] == "ext.demo.echo"));
    let echo: Value = c
        .call("ext.demo.echo", json!({ "text": "hello plugin" }))
        .unwrap();
    assert_eq!(echo["echo"], "hello plugin");

    let created = c.visible();
    let tab_id = &created.room.tabs[0].id;
    c.hidden(&sb.get_token(tab_id).0, "exec cat");
    let count: Value = c.call("ext.demo.room_count", json!({})).unwrap();
    assert_eq!(count["rooms"], 1);
    assert_eq!(count["room_ids"], json!([created.room.id]));

    let denied = c
        .call::<Value>("ext.demo.try_forbidden_write", json!({}))
        .unwrap_err();
    assert_eq!(denied.data.kind, ErrorKind::PermissionDenied);
    let crashed = c.call::<Value>("ext.demo.crash", json!({})).unwrap_err();
    assert_eq!(crashed.data.kind, ErrorKind::PluginUnavailable);
    let ping: Value = c.call("ping", json!({})).unwrap();
    assert_eq!(ping["ok"], true);
    let _: Value = c
        .call("host.tab_close", json!({ "tab_id": tab_id }))
        .unwrap();
}
