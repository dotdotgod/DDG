#![cfg(unix)]

mod common;

use common::{HostSandbox, eventually};
use ddg_agents_proto::host::TabRuntimeState;
use ddg_agents_proto::terminal::{PtyListResult, PtySpawnResult};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn v1_reset_kills_legacy_pty_and_reconnect_keeps_v2_identity() {
    let mut sandbox = HostSandbox::new("v1");
    let source = br#"{"schema_version":1,"host_id":"legacy","next_room_id":2,"next_tab_id":2,"rooms":[{"id":1,"tabs":[{"id":1}]}]}"#;
    fs::write(sandbox.state.join("catalog.json"), source).unwrap();
    sandbox.start_core();
    let orphan: PtySpawnResult = sandbox
        .terminal()
        .call(
            "pty.spawn",
            json!({"cols": 80, "rows": 24, "argv": ["/bin/cat"]}),
        )
        .unwrap();
    assert!(
        sandbox
            .terminal()
            .call::<PtyListResult>("pty.list", json!({}))
            .unwrap()
            .ptys
            .iter()
            .any(|pty| pty.pty_id == orphan.pty_id && pty.session_id.is_empty())
    );

    sandbox.start_host();
    let mut host = sandbox.connect();
    assert!(host.hello.get("reset_notice").is_none());
    let subscription = host.catalog();
    assert!(subscription.catalog.rooms.is_empty());
    let notice = subscription.reset_notice.unwrap();
    assert!(notice.backup_file.starts_with("catalog.json.v1.bak."));
    assert!(!notice.backup_file.contains('/'));
    let file = sandbox.state.join(&notice.backup_file);
    assert_eq!(fs::read(&file).unwrap(), source);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        sandbox
            .terminal()
            .call::<PtyListResult>("pty.list", json!({}))
            .unwrap()
            .ptys
            .is_empty()
    );
    assert_eq!(
        sandbox.connect().catalog().reset_notice.as_ref(),
        Some(&notice)
    );

    drop(host);
    sandbox.stop_host();
    sandbox.start_host();
    let mut host = sandbox.connect();
    assert_eq!(host.catalog().reset_notice, Some(notice));
    let visible = host.visible();
    let tab = &visible.room.tabs[0];
    assert!(host.catalog().reset_notice.is_none());
    let second_orphan: PtySpawnResult = sandbox
        .terminal()
        .call(
            "pty.spawn",
            json!({"cols": 80, "rows": 24, "argv": ["/bin/cat"]}),
        )
        .unwrap();
    sandbox.crash_core();
    eventually(|| host.call::<Value>("ping", json!({})).ok());
    let ptys: PtyListResult = sandbox.terminal().call("pty.list", json!({})).unwrap();
    assert!(!ptys.ptys.iter().any(|p| p.pty_id == second_orphan.pty_id));
    let restored = host.catalog();
    assert_eq!(restored.catalog.rooms[0].tabs[0].id, tab.id);
    assert_eq!(
        restored.catalog.rooms[0].tabs[0].run_generation,
        tab.run_generation
    );
    assert!(matches!(
        restored.catalog.rooms[0].tabs[0].runtime,
        TabRuntimeState::Running { .. }
    ));
}
