use super::*;

fn args(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).into()).collect()
}

#[test]
fn refuses_tokens_and_host_all_on_every_write() {
    for flag in ["--token", "--token=secret"] {
        let err = parse(&args(&["host", "list", flag])).unwrap_err();
        assert!(!err.message.contains("secret"));
        assert!(err.message.contains("DDG_SESSION_TOKEN"));
    }
    for (group, command) in [
        ("session", "create"),
        ("session", "move"),
        ("session", "close"),
        ("session", "restart"),
        ("session", "write"),
        ("session", "screen"),
        ("msg", "send"),
    ] {
        let err = parse(&args(&[group, command, "--host", " ALL "])).unwrap_err();
        assert!(err.message.contains("only valid for session list"));
    }
}

#[test]
fn write_parses_enter_stdin_and_expected_generation() {
    let command = parse(&args(&[
        "session",
        "write",
        "ses_test",
        "--enter",
        "--stdin",
        "--expect-gen",
        "12",
    ]))
    .unwrap();
    assert!(command.stdin && command.enter);
    assert_eq!(
        command.params,
        json!({"session_id":"ses_test","expect_run_generation":12})
    );
    assert_eq!(
        parse(&args(&["msg", "send", "--to", "ses_test", "a b"]))
            .unwrap()
            .params,
        parse(&args(&["msg", "send", "ses_test", "a b"]))
            .unwrap()
            .params,
    );
}

#[test]
fn print_recv_array_and_preserve_list_partial_success() {
    let command = parse(&args(&["session", "list", "--host", " ALL "])).unwrap();
    assert_eq!(command.params["host_id"], "all");
    let partial = json!({"sessions": [], "errors": [{"host_id":"offline","error": RpcError::host_offline("offline")}]});
    let mut out = Vec::new();
    print_result(&mut out, command.method, partial.clone()).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&out).unwrap(), partial);
    out.clear();
    print_result(&mut out, "host.msg_recv", json!({"messages": []})).unwrap();
    assert_eq!(out, b"[]\n");
}
