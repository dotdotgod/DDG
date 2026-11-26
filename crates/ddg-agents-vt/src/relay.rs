//! Server-side VT relay: consumes OSC 7501 and strips queries answered by the
//! server `Vt` so attached viewers never answer them a second time.

use crate::{
    Vt,
    osc7501::{self, MAX_OSC_7501_BYTES, PREFIX},
};
use ddg_agents_proto::terminal::AgentStatus;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const MAX_CSI: usize = 64;
const MAX_STRING: usize = 512;

#[derive(Default)]
pub struct Relay {
    held: Vec<u8>,
    discarding_7501: bool,
    agent_status: Option<AgentStatus>,
}

#[derive(Debug, Default)]
pub struct RelayOutput {
    pub replies: Vec<u8>,
    pub viewer_bytes: Vec<u8>,
    pub status_changed: bool,
}

#[derive(PartialEq, Eq)]
enum ReplyMode {
    Legacy,
    Live,
    Replay,
}

enum Seq {
    Pending,
    Prompt,
    Done { maybe_query: bool },
}

impl Relay {
    /// Returns viewer bytes; VT replies stay pending for `take_replies`.
    /// Use `feed_with_events` for OSC 7501 feature-query replies and timestamps.
    pub fn feed(&mut self, vt: &mut Vt, input: &[u8]) -> Vec<u8> {
        self.feed_inner(vt, input, 0, ReplyMode::Legacy)
            .viewer_bytes
    }

    /// Returns filtered viewer bytes, FIFO replies, and whether a status changed.
    /// Replay restores status and VT state but never sends replies back to the PTY.
    pub fn feed_with_events(
        &mut self,
        vt: &mut Vt,
        input: &[u8],
        now_ms: u64,
        replay: bool,
    ) -> RelayOutput {
        self.feed_inner(
            vt,
            input,
            now_ms,
            if replay {
                ReplyMode::Replay
            } else {
                ReplyMode::Live
            },
        )
    }

    pub fn agent_status(&self) -> Option<&AgentStatus> {
        self.agent_status.as_ref()
    }

    fn feed_inner(
        &mut self,
        vt: &mut Vt,
        input: &[u8],
        now_ms: u64,
        mode: ReplyMode,
    ) -> RelayOutput {
        let mut out = RelayOutput {
            viewer_bytes: Vec::with_capacity(input.len()),
            ..RelayOutput::default()
        };
        let mut run = 0;
        for (i, &b) in input.iter().enumerate() {
            if self.held.is_empty() {
                if b == ESC {
                    pass(vt, &mut out.viewer_bytes, &input[run..i]);
                    self.held.push(b);
                }
                continue;
            }
            if self.held.starts_with(PREFIX) {
                let escaped = self.held.last() == Some(&ESC);
                let st = escaped && b == b'\\';
                let cancelled = matches!(b, 0x18 | 0x1a);
                if st || b == BEL || cancelled {
                    if !self.discarding_7501 && (st || (b == BEL && !escaped)) {
                        let end = self.held.len() - usize::from(st);
                        let payload = &self.held[PREFIX.len()..end];
                        if payload == b"?" {
                            if mode == ReplyMode::Live {
                                out.replies.extend(vt.replies());
                                out.replies.extend_from_slice(b"\x1b]7501;?\x1b\\");
                            }
                        } else {
                            out.status_changed |=
                                osc7501::apply(&mut self.agent_status, payload, now_ms);
                        }
                    }
                    self.held.clear();
                    self.discarding_7501 = false;
                    run = i + 1;
                    continue;
                }
                if self.discarding_7501 {
                    // Resync only at an explicit end; an embedded ESC may be part
                    // of the rejected payload and must never reach either consumer.
                    self.held.truncate(PREFIX.len());
                    if b == ESC {
                        self.held.push(b);
                    }
                    continue;
                }
                if escaped {
                    self.held.clear();
                    self.held.push(ESC);
                    if b != ESC {
                        self.held.push(b);
                    }
                } else {
                    if b != ESC && self.held.len() == PREFIX.len() + MAX_OSC_7501_BYTES {
                        self.discarding_7501 = true;
                        self.held.truncate(PREFIX.len());
                    } else {
                        self.held.push(b);
                    }
                    continue;
                }
            } else {
                self.held.push(b);
            }
            match self.judge() {
                Seq::Pending => continue,
                Seq::Prompt => {
                    out.status_changed |= osc7501::shell_prompt(&mut self.agent_status, now_ms);
                    pass(vt, &mut out.viewer_bytes, &self.held);
                }
                Seq::Done { maybe_query: true } => {
                    let before = vt.pending_replies_len();
                    vt.write(&self.held);
                    if vt.pending_replies_len() == before {
                        // Server VT did not answer this sequence; pass through.
                        out.viewer_bytes.extend_from_slice(&self.held);
                    }
                }
                Seq::Done { maybe_query: false } => pass(vt, &mut out.viewer_bytes, &self.held),
            }
            self.held.clear();
            run = i + 1;
        }
        if self.held.is_empty() {
            pass(vt, &mut out.viewer_bytes, &input[run..]);
        }
        match mode {
            ReplyMode::Legacy => {}
            ReplyMode::Live => {
                out.replies.extend(vt.replies());
                out.replies = without_resize_reports(&out.replies);
            }
            ReplyMode::Replay => vt.drop_replies_after(0),
        }
        out
    }

    /// Flushes half-received VT sequences into `vt`, except possible OSC 7501 frames.
    pub fn flush(&mut self, vt: &mut Vt) {
        if !PREFIX.starts_with(&self.held) && !self.held.starts_with(PREFIX) {
            vt.write(&std::mem::take(&mut self.held));
        }
    }

    /// Returns server-generated PTY replies, stripping automatic mode-2048
    /// in-band resize reports (`CSI 48;...t`) which lack client pixel geometry.
    pub fn take_replies(vt: &mut Vt) -> Vec<u8> {
        without_resize_reports(&vt.replies())
    }

    fn judge(&self) -> Seq {
        let h = &self.held;
        let last = *h.last().expect("held starts with ESC");
        match h.get(1) {
            None => Seq::Pending,
            Some(b'[') if h.len() > 2 && (0x40..=0x7e).contains(&last) => Seq::Done {
                maybe_query: b"cnpqtu".contains(&last),
            },
            Some(b'[') if h.len() > MAX_CSI => Seq::Done { maybe_query: false },
            Some(b'[') => Seq::Pending,
            Some(b']') | Some(b'P') => {
                let ended =
                    (last == BEL && h[1] == b']') || (h.ends_with(b"\x1b\\") && h.len() > 3);
                if ended {
                    let body = &h[2..h.len() - if last == BEL { 1 } else { 2 }];
                    if h[1] == b']' && (body == b"133;A" || body.starts_with(b"133;A;")) {
                        return Seq::Prompt;
                    }
                    let query = match h[1] {
                        b']' => body.contains(&b'?'),
                        _ => body.starts_with(b"$q") || body.starts_with(b"+q"),
                    };
                    Seq::Done { maybe_query: query }
                } else if h.len() > MAX_STRING {
                    Seq::Done { maybe_query: false }
                } else {
                    Seq::Pending
                }
            }
            Some(_) => Seq::Done { maybe_query: false },
        }
    }
}

fn pass(vt: &mut Vt, out: &mut Vec<u8>, bytes: &[u8]) {
    if !bytes.is_empty() {
        vt.write(bytes);
        out.extend_from_slice(bytes);
    }
}

fn without_resize_reports(replies: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(replies.len());
    let mut i = 0;
    while i < replies.len() {
        if let Some(rest) = replies[i..].strip_prefix(b"\x1b[48;") {
            let n = rest
                .iter()
                .take_while(|b| b.is_ascii_digit() || **b == b';')
                .count();
            if rest.get(n) == Some(&b't') {
                i += 5 + n + 1;
                continue;
            }
        }
        out.push(replies[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddg_agents_proto::terminal::AgentState;

    #[test]
    fn strips_answered_queries_even_when_byte_split() {
        for split in [false, true] {
            let mut vt = Vt::new(80, 24).unwrap();
            let mut relay = Relay::default();
            // DSR (CSI 6n) is answered by Ghostty VT; OSC 52 clipboard query is blocked/unanswered;
            // plain SGR and title OSC 2 pass through untouched.
            let input = b"hi \x1b[31mred\x1b[0m \x1b[6n\x1b]2;title\x07 end";
            let out: Vec<u8> = if split {
                input
                    .iter()
                    .flat_map(|b| relay.feed(&mut vt, std::slice::from_ref(b)))
                    .collect()
            } else {
                relay.feed(&mut vt, input)
            };
            assert_eq!(
                String::from_utf8_lossy(&out),
                "hi \x1b[31mred\x1b[0m \x1b]2;title\x07 end",
                "split={split}"
            );
            assert_eq!(Relay::take_replies(&mut vt), b"\x1b[1;8R");
            assert_eq!(vt.title(), "title");
        }
    }

    fn stream(
        relay: &mut Relay,
        vt: &mut Vt,
        input: &[u8],
        split: bool,
        replay: bool,
    ) -> RelayOutput {
        let mut result = RelayOutput::default();
        let chunk_size = if split { 1 } else { input.len() };
        for chunk in input.chunks(chunk_size) {
            let out = relay.feed_with_events(vt, chunk, 7, replay);
            result.viewer_bytes.extend(out.viewer_bytes);
            result.replies.extend(out.replies);
            result.status_changed |= out.status_changed;
        }
        result
    }

    #[test]
    fn osc7501_fifo_and_replay() {
        let mut terminal = Vt::new(80, 24).unwrap();
        terminal.write(b"\x1b[c");
        let da1 = terminal.replies();
        assert!(!da1.is_empty());
        let query = b"\x1b]7501;?\x1b\\";
        for split in [false, true] {
            for replay in [false, true] {
                let mut vt = Vt::new(80, 24).unwrap();
                let mut relay = Relay::default();
                let input = b"hi\x1b[6n\x1b[c\x1b]7501;?\x07\x1b[c\x1b]7501;?\x1b\\\x1b[c\x1b]7501;state=working\x07!";
                let out = stream(&mut relay, &mut vt, input, split, replay);
                assert_eq!(out.viewer_bytes, b"hi!");
                assert_eq!(vt.plain_text().trim(), "hi!");
                assert_eq!(vt.continuation(), Some(Vec::new()));
                let fifo = [b"\x1b[1;3R".as_slice(), &da1, query, &da1, query, &da1].concat();
                assert_eq!(out.replies, if replay { Vec::new() } else { fifo });
                assert!(vt.replies().is_empty());
                assert!(out.status_changed);
                assert_eq!(relay.agent_status().unwrap().state, AgentState::Working);
            }
        }
    }

    #[test]
    fn osc7501_oversize_discards_embedded_controls_and_recovers() {
        for split in [false, true] {
            for end in [b"\x07".as_slice(), b"\x1b\\", b"\x18", b"\x1a"] {
                let mut vt = Vt::new(80, 24).unwrap();
                let mut relay = Relay::default();
                let mut input = b"\x1b]7501;".to_vec();
                input.extend_from_slice(&[b'x'; 4097]);
                input.extend_from_slice(b"\x1b[31mLEAK\x1b]52;c;YQ==");
                input.extend_from_slice(end);
                input.extend_from_slice(b"OK\x1b]7501;state=done\x1b\\");
                let out = stream(&mut relay, &mut vt, &input, split, false);
                assert_eq!(out.viewer_bytes, b"OK");
                assert!(out.replies.is_empty());
                assert_eq!(relay.agent_status().unwrap().state, AgentState::Done);
                let mut expected = Vt::new(80, 24).unwrap();
                expected.write(b"OK");
                assert_eq!(vt.paint(0), expected.paint(0));
            }
        }
    }

    #[test]
    fn osc7501_aborts_and_reparses_next_escape() {
        for split in [false, true] {
            for cancel in [b"\x18".as_slice(), b"\x1a", b""] {
                let mut vt = Vt::new(80, 24).unwrap();
                let mut relay = Relay::default();
                let text = if cancel.is_empty() {
                    b"".as_slice()
                } else {
                    b"plain "
                };
                let expected = [text, b"\x1b[32mOK"].concat();
                let input = [b"\x1b]7501;state=working", cancel, &expected].concat();
                let out = stream(&mut relay, &mut vt, &input, split, false);
                assert_eq!(out.viewer_bytes, expected);
                assert!(!out.status_changed);
                assert!(relay.agent_status().is_none());
                assert_eq!(
                    vt.plain_text().trim(),
                    if cancel.is_empty() { "OK" } else { "plain OK" }
                );
            }
        }
    }

    #[test]
    fn osc7501_flush_keeps_partial_prefix_payload_and_drain_out_of_vt() {
        for split_at in [1, 5, 7, 12] {
            let mut vt = Vt::new(80, 24).unwrap();
            let mut relay = Relay::default();
            let input = b"\x1b]7501;state=working\x1b\\OK";
            assert!(
                relay
                    .feed_with_events(&mut vt, &input[..split_at], 1, false)
                    .viewer_bytes
                    .is_empty()
            );
            relay.flush(&mut vt);
            assert_eq!(vt.continuation(), Some(Vec::new()));
            let out = relay.feed_with_events(&mut vt, &input[split_at..], 2, false);
            assert_eq!(out.viewer_bytes, b"OK");
            assert!(out.status_changed);
            assert_eq!(vt.plain_text().trim(), "OK");
        }
        let mut vt = Vt::new(80, 24).unwrap();
        let mut relay = Relay::default();
        let input = [b"\x1b]7501;".as_slice(), &[b'x'; 4097], b"\x1b"].concat();
        assert!(
            relay
                .feed_with_events(&mut vt, &input, 1, false)
                .viewer_bytes
                .is_empty()
        );
        relay.flush(&mut vt);
        assert_eq!(vt.continuation(), Some(Vec::new()));
        let out = relay.feed_with_events(&mut vt, b"\\OK", 2, false);
        assert_eq!(out.viewer_bytes, b"OK");
        assert!(!out.status_changed);
        assert_eq!(vt.plain_text().trim(), "OK");
    }

    #[test]
    fn strips_in_band_resize_reports_from_server_replies() {
        let mut vt = Vt::new(80, 24).unwrap();
        let mut relay = Relay::default();
        let out = relay.feed(&mut vt, b"\x1b[?2048h\x1b[6n");
        assert_eq!(out, b"\x1b[?2048h");
        vt.resize(100, 30);
        assert_eq!(Relay::take_replies(&mut vt), b"\x1b[1;1R");
    }
}
