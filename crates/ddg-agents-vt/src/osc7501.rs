//! OSC 7501 status validation and bounded per-PTY aggregation.

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use ddg_agents_proto::{
    id::sanitize_display_text,
    terminal::{AgentState, AgentStatus, AgentStatusRecord, BlockedKind},
};

pub(crate) const PREFIX: &[u8] = b"\x1b]7501;";
pub(crate) const MAX_OSC_7501_BYTES: usize = 4096;
const MAX_STATUS_RECORDS: usize = 16;

enum Command {
    Clear(Option<String>),
    Set(AgentStatusRecord),
}

fn parse(payload: &[u8], now_ms: u64) -> Option<Command> {
    use AgentState::*;

    let mut state = None;
    let mut record = AgentStatusRecord {
        id: None,
        state: Idle,
        kind: None,
        progress: None,
        app: None,
        title: None,
        msg: None,
        updated_ms: now_ms,
    };
    for pair in std::str::from_utf8(payload).ok()?.split(':') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "state" => {
                state = Some(match value {
                    "clear" => None,
                    "idle" => Some(Idle),
                    "working" => Some(Working),
                    "blocked" => Some(Blocked),
                    "done" => Some(Done),
                    "error" => Some(Error),
                    _ => return None,
                });
            }
            "kind" => {
                record.kind = Some(match value {
                    "permission" => BlockedKind::Permission,
                    "question" => BlockedKind::Question,
                    "auth" => BlockedKind::Auth,
                    _ => return None,
                })
            }
            "progress" => {
                if !value.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                record.progress = Some(value.parse::<u8>().ok().filter(|&n| n <= 100)?);
            }
            "id" => {
                if !(1..=64).contains(&value.len())
                    || value.contains("..")
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                {
                    return None;
                }
                record.id = Some(value.to_owned());
            }
            "app" => record.app = Some(sanitize_display_text(value, 64)),
            "title" => record.title = Some(display_base64(value, 256, 120)?),
            "msg" => record.msg = Some(display_base64(value, 2048, 512)?),
            _ => {}
        }
    }
    let state = state?;
    if (record.kind.is_some() && state != Some(Blocked))
        || (record.progress.is_some() && !matches!(state, Some(Working | Blocked)))
    {
        return None;
    }
    let Some(state) = state else {
        return Some(Command::Clear(record.id));
    };
    record.state = state;
    Some(Command::Set(record))
}

fn display_base64(value: &str, max_bytes: usize, max_chars: usize) -> Option<String> {
    let engine = if value.ends_with('=') {
        STANDARD
    } else {
        STANDARD_NO_PAD
    };
    let bytes = engine.decode(value).ok()?;
    if bytes.len() > max_bytes {
        return None;
    }
    Some(sanitize_display_text(
        std::str::from_utf8(&bytes).ok()?,
        max_chars,
    ))
}

pub(crate) fn apply(status: &mut Option<AgentStatus>, payload: &[u8], now_ms: u64) -> bool {
    match parse(payload, now_ms) {
        None => false,
        Some(Command::Clear(None)) => status.take().is_some(),
        Some(Command::Clear(Some(id))) => retain(status, now_ms, |r| r.id.as_ref() != Some(&id)),
        Some(Command::Set(record)) => {
            let records = if let Some(current) = status {
                if let Some(i) = current.records.iter().position(|r| r.id == record.id) {
                    if current.records[i] == record {
                        return false;
                    }
                    current.records.remove(i);
                } else if current.records.len() == MAX_STATUS_RECORDS {
                    return false;
                }
                current.records.push(record);
                std::mem::take(&mut current.records)
            } else {
                vec![record]
            };
            *status = aggregate(records, now_ms);
            true
        }
    }
}

pub(crate) fn shell_prompt(status: &mut Option<AgentStatus>, now_ms: u64) -> bool {
    retain(status, now_ms, |r| {
        !matches!(r.state, AgentState::Working | AgentState::Blocked)
    })
}

fn retain(
    status: &mut Option<AgentStatus>,
    now_ms: u64,
    keep: impl FnMut(&AgentStatusRecord) -> bool,
) -> bool {
    let Some(current) = status else { return false };
    let before = current.records.len();
    current.records.retain(keep);
    if current.records.len() == before {
        return false;
    }
    *status = aggregate(std::mem::take(&mut current.records), now_ms);
    true
}

fn aggregate(records: Vec<AgentStatusRecord>, now_ms: u64) -> Option<AgentStatus> {
    use AgentState::*;

    let top = records.iter().max_by_key(|r| {
        (
            match r.state {
                Idle => 0,
                Done => 1,
                Working => 2,
                Error => 3,
                Blocked => 4,
            },
            r.updated_ms,
        )
    })?;
    Some(AgentStatus {
        state: top.state,
        kind: top.kind,
        progress: top.progress,
        app: top.app.clone(),
        title: top.title.clone(),
        msg: top.msg.clone(),
        records,
        status_ms: now_ms,
    })
}

#[cfg(test)]
mod tests {
    use crate::{Relay, Vt};
    use base64::{
        Engine,
        engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    };
    use ddg_agents_proto::terminal::{AgentState::*, BlockedKind};

    fn command(relay: &mut Relay, vt: &mut Vt, payload: &str, now: u64) -> bool {
        let seq = format!("\x1b]7501;{payload}\x07");
        let out = relay.feed_with_events(vt, seq.as_bytes(), now, false);
        assert!(out.viewer_bytes.is_empty());
        assert!(out.replies.is_empty());
        out.status_changed
    }

    #[test]
    fn osc7501_strict_fields_and_display_limits() {
        let mut relay = Relay::default();
        let mut vt = Vt::new(80, 24).unwrap();
        let title = STANDARD_NO_PAD.encode(format!("\x1b\u{202e}\u{2028}{}", "é".repeat(122)));
        let msg = STANDARD.encode(format!("\n\u{2029}{}", "한".repeat(513)));
        let app = format!("\u{2067}{}", "a".repeat(65));
        let input = format!(
            "state=blocked:kind=permission:progress=100:id=A:app={app}:title={title}:msg={msg}"
        );
        assert!(command(&mut relay, &mut vt, &input, 10));
        let status = relay.agent_status().unwrap();
        assert_eq!(status.state, Blocked);
        assert_eq!(status.kind, Some(BlockedKind::Permission));
        assert_eq!(status.progress, Some(100));
        assert_eq!(status.app, Some("a".repeat(64)));
        assert_eq!(status.title, Some("é".repeat(120)));
        assert_eq!(status.msg, Some("한".repeat(512)));
        assert_eq!(status.status_ms, 10);
        assert_eq!(status.records[0].updated_ms, 10);
        let old = status.clone();
        for bad in [
            "state=working:msg=hello",
            "state=working:msg=8J-YgA",
            "state=working:msg=/w==",
            "state=working:msg=Zg=",
            "state=working:msg=Zh",
            "state=working:id=",
            "state=working:id=a..b",
            "state=working:id=a/b",
            "state=working:id=a b",
            "state=working:id=a\0b",
            "state=working:id=한",
            "state=running",
            "state=running:state=working",
            "state=blocked:kind=approval",
            "state=working:kind=auth",
            "state=done:progress=1",
            "state=working:progress=101",
            "state=working:progress=-1",
            "id=A",
            "state=clear:id=..",
        ] {
            assert!(!command(&mut relay, &mut vt, bad, 11), "{bad:?}");
            assert_eq!(relay.agent_status(), Some(&old));
        }
        for (field, max) in [("title", 256), ("msg", 2048)] {
            for len in [max, max + 1] {
                let encoded = STANDARD.encode(vec![b'q'; len]);
                assert_eq!(
                    command(
                        &mut relay,
                        &mut vt,
                        &format!("state=working:{field}={encoded}"),
                        12
                    ),
                    len == max,
                );
            }
        }
        for (name, kind) in [
            ("question", BlockedKind::Question),
            ("auth", BlockedKind::Auth),
        ] {
            assert!(command(
                &mut relay,
                &mut vt,
                &format!("state=blocked:id=A:kind={name}:progress=0"),
                13
            ));
            assert_eq!(relay.agent_status().unwrap().kind, Some(kind));
            assert_eq!(relay.agent_status().unwrap().progress, Some(0));
        }
    }

    #[test]
    fn osc7501_payload_limits_with_byte_splits() {
        for len in [513, 4096, 4097] {
            for end in ["\x07", "\x1b\\"] {
                let mut relay = Relay::default();
                let mut vt = Vt::new(80, 24).unwrap();
                let fields = "state=working:app=";
                let seq = format!("\x1b]7501;{fields}{}{end}", "a".repeat(len - fields.len()));
                let mut changed = false;
                for byte in seq.bytes() {
                    let out = relay.feed_with_events(&mut vt, &[byte], 1, false);
                    assert!(out.viewer_bytes.is_empty());
                    changed |= out.status_changed;
                }
                assert_eq!(changed, len <= 4096);
                assert_eq!(
                    relay.agent_status().and_then(|s| s.app.clone()),
                    (len <= 4096).then(|| "a".repeat(64))
                );
                assert_eq!(vt.continuation(), Some(Vec::new()));
            }
        }
    }

    #[test]
    fn osc7501_record_cap_updates_and_clears() {
        let mut relay = Relay::default();
        let mut vt = Vt::new(80, 24).unwrap();
        let max_id = format!("A_-.{}", "b".repeat(60));
        assert!(command(
            &mut relay,
            &mut vt,
            &format!("state=idle:id={max_id}"),
            1
        ));
        assert!(!command(
            &mut relay,
            &mut vt,
            &format!("state=idle:id={max_id}B"),
            1
        ));
        for n in 0..15 {
            assert!(command(
                &mut relay,
                &mut vt,
                &format!("state=working:id={n}"),
                2
            ));
        }
        assert!(!command(&mut relay, &mut vt, "state=done:id=extra", 3));
        assert_eq!(relay.agent_status().unwrap().records.len(), 16);
        assert!(command(&mut relay, &mut vt, "state=blocked:id=0", 4));
        assert_eq!(relay.agent_status().unwrap().state, Blocked);
        assert_eq!(relay.agent_status().unwrap().records.len(), 16);
        assert!(command(&mut relay, &mut vt, "state=clear:id=0", 5));
        assert!(!command(&mut relay, &mut vt, "state=clear:id=0", 6));
        assert!(command(&mut relay, &mut vt, "state=done:id=extra", 7));
        assert_eq!(relay.agent_status().unwrap().records.len(), 16);
        assert!(command(&mut relay, &mut vt, "state=clear", 8));
        assert!(relay.agent_status().is_none());
    }

    #[test]
    fn osc7501_aggregation_and_shell_prompt() {
        for prompt in [b"\x1b]133;A\x07".as_slice(), b"\x1b]133;A;aid=1\x1b\\"] {
            let mut relay = Relay::default();
            let mut vt = Vt::new(80, 24).unwrap();
            for (i, (name, state)) in [
                ("idle", Idle),
                ("done", Done),
                ("working", Working),
                ("error", Error),
                ("blocked", Blocked),
            ]
            .into_iter()
            .enumerate()
            {
                let now = 10 - i as u64;
                assert!(command(
                    &mut relay,
                    &mut vt,
                    &format!("state={name}:id={name}"),
                    now
                ));
                assert_eq!(relay.agent_status().unwrap().state, state);
            }
            assert!(command(
                &mut relay,
                &mut vt,
                "state=blocked:id=newer:app=latest",
                11
            ));
            assert_eq!(relay.agent_status().unwrap().app.as_deref(), Some("latest"));
            assert!(!command(
                &mut relay,
                &mut vt,
                "state=blocked:id=newer:app=latest",
                11
            ));
            assert!(command(
                &mut relay,
                &mut vt,
                "state=working:app=default",
                12
            ));
            let mut viewer = Vec::new();
            let mut changed = false;
            for b in prompt {
                let out = relay.feed_with_events(&mut vt, &[*b], 13, false);
                viewer.extend(out.viewer_bytes);
                changed |= out.status_changed;
            }
            assert_eq!(viewer, prompt);
            assert!(changed);
            let status = relay.agent_status().unwrap();
            assert_eq!(status.state, Error);
            assert_eq!(status.status_ms, 13);
            assert_eq!(
                status.records.iter().map(|r| r.state).collect::<Vec<_>>(),
                [Idle, Done, Error]
            );
            assert!(
                !relay
                    .feed_with_events(&mut vt, b"\x1b]133;AB\x07", 14, false)
                    .status_changed
            );
        }
    }
}
