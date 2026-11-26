//! Shared catalog, control isolation, and post-reboot restoration via the connect bridge.
#![cfg(unix)]

mod common;

use common::HostSandbox;
use ddg_agents_proto::host::{
    CatalogSubscribeResult, ExecutorSpec, RoomCreateParams, RoomCreateResult, RoomRenameParams,
    RoomRenameResult, TabAttachParams, TabAttachResult, TabCloseParams, TabCloseResult,
    TabRestartParams, TabRestartResult, TabRuntimeState,
};
use ddg_agents_proto::terminal::ControlMode;
use ddg_agents_vt::Vt;
use serde_json::json;

#[test]
fn plugin_redaction_and_hub_boundary_host_pid() {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    let mut sb = HostSandbox::new("pid");
    let pid_file = sb.run.join("host.pid");
    let target = sb.run.join("untouched");
    fs::write(&target, "untouched").unwrap();
    symlink(&target, &pid_file).unwrap();
    sb.start_host();
    let meta = fs::symlink_metadata(&pid_file).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let text = fs::read_to_string(&pid_file).unwrap();
    let pid: i32 = text.strip_suffix('\n').unwrap().parse().unwrap();
    assert!(pid > 1);
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
    let client = sb.connect();
    assert!(
        client.hello["features"]
            .as_array()
            .unwrap()
            .contains(&json!("agent_activity_v1"))
    );
    sb.stop_host();
    assert!(!pid_file.exists());
}

#[test]
fn two_clients_share_rooms_control_isolation_and_reboot_restore() {
    let mut sb = HostSandbox::new("shared");
    sb.start_core();
    sb.start_host();

    let mut c1 = sb.bridge();
    let created: RoomCreateResult = c1
        .call(
            "host.room_create",
            json!(RoomCreateParams {
                request_id: Some("req-r1".into()),
                agent_profile: None,
                name: None,
                space_cwd: Some(sb.space.to_string_lossy().into_owned()),
                executor: ExecutorSpec::Command {
                    argv: vec!["/bin/sh".into()],
                    env: Default::default(),
                },
                cols: 80,
                rows: 24,
            }),
        )
        .unwrap();
    assert_eq!(created.room.auto_name, "my-space");

    // Idempotent retry with the same request_id returns the exact same room!
    let retry: RoomCreateResult = c1
        .call(
            "host.room_create",
            json!(RoomCreateParams {
                request_id: Some("req-r1".into()),
                agent_profile: None,
                name: None,
                space_cwd: Some(sb.space.to_string_lossy().into_owned()),
                executor: ExecutorSpec::DefaultShell,
                cols: 80,
                rows: 24,
            }),
        )
        .unwrap();
    assert_eq!(retry.room.id, created.room.id);

    let tab_id = created.room.tabs[0].id.clone();
    let renamed: RoomRenameResult = c1
        .call(
            "host.room_rename",
            json!(RoomRenameParams {
                room_id: created.room.id,
                name: Some("백엔드".into()),
            }),
        )
        .unwrap();
    assert_eq!(renamed.room.name.as_deref(), Some("백엔드"));

    // Attach client 1 (gets Control) and run a command in space_cwd.
    let a1: TabAttachResult = c1
        .call(
            "host.tab_attach",
            json!(TabAttachParams {
                tab_id: tab_id.clone(),
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 100,
            }),
        )
        .unwrap();
    assert_eq!(a1.mode, ControlMode::Control);
    assert_eq!(a1.run_generation, 1);
    let mut vt1 = Vt::new(80, 24).unwrap();
    c1.send_input(a1.channel, b"pwd\n");
    c1.read_until(a1.channel, &mut vt1, "my-space");

    // Second client sees the exact same room/tab catalog and attaches Readonly!
    let mut c2 = sb.bridge();
    let sub2: CatalogSubscribeResult = c2.call("host.catalog_subscribe", json!({})).unwrap();
    assert_eq!(sub2.catalog.rooms.len(), 1);
    assert_eq!(sub2.catalog.rooms[0].name.as_deref(), Some("백엔드"));
    let a2: TabAttachResult = c2
        .call(
            "host.tab_attach",
            json!(TabAttachParams {
                tab_id: tab_id.clone(),
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 100,
            }),
        )
        .unwrap();
    assert_eq!(a2.mode, ControlMode::Readonly);
    let mut vt2 = Vt::new(80, 24).unwrap();
    c2.read_until(a2.channel, &mut vt2, "my-space");

    drop(c1);
    drop(c2);

    // Simulate host reboot: stop both host and terminal-server/holder, then restart!
    sb.stop_host_and_terminal();
    sb.start_core();
    sb.start_host();

    let mut c3 = sb.bridge();
    let sub3: CatalogSubscribeResult = c3.call("host.catalog_subscribe", json!({})).unwrap();
    assert_eq!(sub3.catalog.rooms.len(), 1);
    assert_eq!(sub3.catalog.rooms[0].name.as_deref(), Some("백엔드"));
    // Must be StoppedAfterReboot (never auto-reexecuted!).
    assert_eq!(
        sub3.catalog.rooms[0].tabs[0].runtime,
        TabRuntimeState::StoppedAfterReboot
    );
    assert_eq!(sub3.catalog.rooms[0].tabs[0].pty_id, None);

    // Explicit restart brings a fresh PTY back on the same tab!
    let restarted: TabRestartResult = c3
        .call(
            "host.tab_restart",
            json!(TabRestartParams {
                tab_id: tab_id.clone(),
                cols: 80,
                rows: 24,
            }),
        )
        .unwrap();
    assert_eq!(restarted.tab.run_generation, 2);
    assert!(matches!(
        restarted.tab.runtime,
        TabRuntimeState::Running { .. }
    ));

    // Explicit close reaps PTY and prunes the empty room.
    let closed: TabCloseResult = c3
        .call(
            "host.tab_close",
            json!(TabCloseParams {
                tab_id,
                request_id: Some("close-1".into()),
            }),
        )
        .unwrap();
    assert!(closed.removed_room);
    let final_sub: CatalogSubscribeResult = c3.call("host.catalog_subscribe", json!({})).unwrap();
    assert!(final_sub.catalog.rooms.is_empty());
}
