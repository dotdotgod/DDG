use super::*;
use serde_json::json;

const SESSION: &str = "ses_00000000000000000000000001";

fn request(method: &str) -> RelayEnvelope {
    RelayEnvelope {
        hop: 1,
        request_id: "1".into(),
        method: method.into(),
        from_host_id: "forged".into(),
        params: json!({"from_session_id": SESSION, "session_token": "must not cross"}),
    }
}

#[test]
fn hub_requires_exact_opt_in() {
    for value in [None, Some(""), Some("0"), Some(" 1 "), Some("true")] {
        assert!(!enabled(value));
    }
    assert!(enabled(Some("1")));
}

#[test]
fn relay_checks_hop_allowlist_self_and_replaces_origin() {
    for method in [
        "host.msg_send",
        "host.session_create",
        "host.session_list",
        "host.session_move",
        "host.session_close",
        "host.session_restart",
        "host.session_screen",
        "host.session_write",
        "host.list_hosts",
    ] {
        let mut envelope = request(method);
        let caller = validate_relay(&mut envelope, "hub", Some("pinned-peer")).unwrap();
        assert_eq!(envelope.from_host_id, "pinned-peer");
        assert_eq!(caller.host_id, "pinned-peer");
        assert_eq!(caller.session_id, SESSION);
        assert!(caller.is_remote);
        assert!(envelope.params.get("session_token").is_none());
    }
    let mut envelope = request("host.msg_send");
    for hop in [0, 2, 255] {
        envelope.hop = hop;
        assert_eq!(
            validate_relay(&mut envelope, "hub", Some("peer"))
                .unwrap_err()
                .data
                .code
                .as_deref(),
            Some("invalid_params")
        );
    }
    assert_eq!(
        validate_relay(&mut request("host.msg_recv"), "hub", Some("peer"))
            .unwrap_err()
            .data
            .code
            .as_deref(),
        Some("permission_denied")
    );
    assert_eq!(
        validate_relay(&mut request("host.msg_send"), "hub", Some("hub"))
            .unwrap_err()
            .data
            .code
            .as_deref(),
        Some("host_security_blocked")
    );
}

#[test]
fn outbound_relay_strips_credentials_and_canonicalizes_destination() {
    let caller = Caller {
        session_id: SESSION.into(),
        host_id: "authenticated".into(),
        is_remote: false,
    };
    let params = json!({"host_id":"peer", "session_token":"secret", "from_host_id":"forged", "from_session_id":"forged", "is_remote":false});
    for upstream in [true, false] {
        let (link, _) = Link::new(upstream);
        let envelope = link.envelope("host.session_list", params.clone(), &caller);
        assert_eq!(envelope.hop, 1);
        assert_eq!(envelope.from_host_id, "authenticated");
        let expected = if upstream {
            json!({"host_id":"peer", "from_session_id":SESSION})
        } else {
            json!({"from_session_id":SESSION})
        };
        assert_eq!(envelope.params, expected);
    }
}

#[test]
fn relay_attach_requires_a_non_hub_token_free_framed_connection() {
    for (hub, token, framed, code) in [
        (true, false, true, "permission_denied"),
        (false, true, true, "permission_denied"),
        (false, false, false, "invalid_params"),
    ] {
        assert_eq!(
            attach_allowed(hub, token, framed)
                .unwrap_err()
                .data
                .code
                .as_deref(),
            Some(code)
        );
    }
    assert!(attach_allowed(false, false, true).is_ok());
}

#[test]
fn replacement_stops_reconnect_and_list_hosts_has_only_summary_fields() {
    assert!(peer_end(StreamError::Closed(CloseReason::RelayReplaced)).1);
    assert!(!peer_end(StreamError::Disconnected).1);
    let list = Relay::default().summaries("hub");
    assert_eq!(
        json!(list),
        json!({"hosts": [{"host_id": "hub", "label": "local", "status": "connected"}]})
    );
}
