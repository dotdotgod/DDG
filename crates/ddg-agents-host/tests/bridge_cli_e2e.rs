#![cfg(unix)]

mod common;

use common::{HostSandbox, eventually};
use ddg_agents_proto::host::RoomCreateResult;
use serde_json::{Value, json};
use std::{fs, process::Command};

#[test]
fn skill_flags_work_without_session_environment_and_reject_tokens() {
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ddg-agents-host"))
            .args(args)
            .env_remove("DDG_SESSION_TOKEN")
            .env_remove("DDG_HOST_SOCK")
            .output()
            .unwrap()
    };
    for flag in ["--skill", "--skills"] {
        let output = run(&[flag]);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, include_bytes!("../src/SKILL.md"));
        for args in [&[flag, "--token", "secret"][..], &["--token=secret", flag]] {
            let output = run(args);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            let err = String::from_utf8(output.stderr).unwrap();
            assert!(err.contains("--token is not allowed") && !err.contains("secret"));
        }
    }
}

#[test]
fn shell_uses_injected_environment_to_run_bridge_commands() {
    let mut sb = HostSandbox::new("cli-e2e");
    sb.start_host();
    let mut host = sb.connect();
    let script = r#"
exec 2> "$OUT/stderr"
trap 'echo failed > "$OUT/done"' EXIT
set -eu
: "${DDG_SESSION_ID:?}" "${DDG_ROOM_ID:?}" "${DDG_HOST_ID:?}" "${DDG_SESSION_TOKEN:?}"
test -S "$DDG_HOST_SOCK" && test -S "$DDG_TERMINAL_SOCK"
"$BRIDGE" host list > "$OUT/hosts"
"$BRIDGE" msg recv --max 1 --wait-ms 10 > "$OUT/empty"
printf '%s' 'hello from stdin' | "$BRIDGE" msg send --to "$DDG_SESSION_ID" --stdin > "$OUT/send"
"$BRIDGE" msg recv > "$OUT/inbox"
"$BRIDGE" msg send "$DDG_SESSION_ID" 'streamed' > /dev/null
"$BRIDGE" msg subscribe > "$OUT/subscription" &
sub=$!
for n in 1 2 3 4 5 6 7 8 9 10; do
    test ! -s "$OUT/subscription" || break
    sleep 0.1
done
kill "$sub"
wait "$sub" || true
test -s "$OUT/subscription"
"$BRIDGE" session create --host local --cwd "$OUT" --title 'worker' -- /bin/sh -c 'read input; printf "REPLIED:%s\n" "$input"; exec cat' > "$OUT/create"
id=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["session"]["session_id"])' < "$OUT/create")
"$BRIDGE" session list --include-hidden > "$OUT/list"
printf 'cli write' | "$BRIDGE" session write "$id" --stdin --enter --expect-gen 1 > "$OUT/write"
for n in 1 2 3 4 5 6 7 8 9 10; do
    "$BRIDGE" session screen "$id" --lines 10 --max-bytes 4096 > "$OUT/screen"
    grep -q 'REPLIED:cli write' "$OUT/screen" && break
    sleep 0.1
done
"$BRIDGE" session move "$id" --new-room 'promoted' > "$OUT/move"
"$BRIDGE" session restart "$id" > "$OUT/restart"
"$BRIDGE" session close "$id" > "$OUT/close"
trap - EXIT
echo ok > "$OUT/done"
"#;
    let room: RoomCreateResult = host
        .call(
            "host.room_create",
            json!({
                "cols": 80, "rows": 24,
                "executor": {"kind":"command", "argv":["/bin/sh", "-c", script],
                    "env": {"BRIDGE":env!("CARGO_BIN_EXE_ddg-agents-host"), "OUT":sb.space}},
            }),
        )
        .unwrap();
    let status = eventually(|| {
        fs::read_to_string(sb.space.join("done"))
            .ok()
            .filter(|s| !s.is_empty())
    });
    assert_eq!(
        status.trim(),
        "ok",
        "{}",
        fs::read_to_string(sb.space.join("stderr")).unwrap()
    );
    let read = |name| -> Value {
        serde_json::from_slice(&fs::read(sb.space.join(name)).unwrap()).unwrap()
    };
    let id = &room.room.tabs[0].id;
    assert_eq!(fs::read_to_string(sb.space.join("empty")).unwrap(), "[]\n");
    assert_eq!(read("hosts")["hosts"][0]["host_id"], host.hello["host_id"]);
    assert_eq!(read("inbox")[0]["from_session_id"], *id);
    assert_eq!(read("inbox")[0]["data_b64"], "aGVsbG8gZnJvbSBzdGRpbg==");
    assert_eq!(read("subscription")["data_b64"], "c3RyZWFtZWQ=");
    let created = read("create");
    assert_eq!(created["session"]["title"], "worker");
    assert_eq!(created["session"]["visibility"], "hidden");
    assert_eq!(read("list")["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(read("write")["byte_len"], 10);
    assert!(
        read("screen")["text"]
            .as_str()
            .unwrap()
            .contains("REPLIED:cli write")
    );
    assert_eq!(read("move")["session"]["visibility"], "visible");
    assert_eq!(read("restart")["session"]["run_generation"], 2);
    assert_eq!(read("close")["removed_room"], true);
}
