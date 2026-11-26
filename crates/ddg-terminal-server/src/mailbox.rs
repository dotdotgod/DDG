//! Generation-scoped, RAM-only session inboxes. Core serializes all mutations.

use crate::outbox::Conn;
use ddg_agents_proto::id::{decode_base64_bounded, generate_message_id};
use ddg_agents_proto::rpc::{ErrorKind, RpcError};
use ddg_agents_proto::terminal::{
    CloseReason, SessionMessage, SessionMessagePayload, SessionSendParams, SessionSendResult,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Notify;

pub const MAX_MESSAGE_BYTES: usize = 65_536;
pub const MAX_MESSAGES: usize = 256;
const MAX_SESSION_BYTES: usize = 1_048_576;
const MAX_GLOBAL_BYTES: usize = 33_554_432;

struct Entry {
    message: SessionMessage,
    bytes: usize,
    order: u64,
}

impl Entry {
    fn regular(&self) -> bool {
        matches!(&self.message.payload, SessionMessagePayload::Message { .. })
    }
}

#[derive(Default)]
struct Inbox {
    pty_id: u32,
    queue: VecDeque<Entry>,
    bytes: usize,
    changed: Arc<Notify>,
    subscribers: VecDeque<(Arc<Conn>, u32)>,
}

impl Inbox {
    fn protected_bytes(&self) -> impl Iterator<Item = usize> + '_ {
        self.queue
            .iter()
            .filter(|item| !item.regular())
            .map(|item| item.bytes)
    }

    fn remove(&mut self, at: usize) -> Option<Entry> {
        let entry = self.queue.remove(at)?;
        self.bytes -= entry.bytes;
        Some(entry)
    }

    fn flush(&mut self) -> usize {
        let mut consumed = 0;
        // Streams and recv consume the same FIFO; a stalled stream leaves records queued.
        while !self.subscribers.is_empty()
            && let Some(front) = self.queue.front()
        {
            let mut line = serde_json::to_vec(&front.message).expect("serializable");
            line.push(b'\n');
            let Some(at) = self
                .subscribers
                .iter()
                .position(|(conn, channel)| conn.try_inbox_data(*channel, &line))
            else {
                break;
            };
            let len = self.subscribers.len();
            self.subscribers.rotate_left((at + 1) % len);
            consumed += self.remove(0).expect("front exists").bytes;
        }
        consumed
    }
}

#[derive(Default)]
pub struct Mailboxes {
    active: HashMap<String, Inbox>,
    bytes: usize,
    order: u64,
}

impl Mailboxes {
    pub fn open(&mut self, session: &str, pty_id: u32) {
        if !session.is_empty() {
            self.active.entry(session.to_owned()).or_default().pty_id = pty_id;
        }
    }

    pub fn owner(&self, session: &str) -> Option<u32> {
        self.active.get(session).map(|inbox| inbox.pty_id)
    }

    pub fn signal(&self, session: &str) -> Result<Arc<Notify>, RpcError> {
        Ok(self.live(session)?.changed.clone())
    }

    fn live(&self, session: &str) -> Result<&Inbox, RpcError> {
        self.active.get(session).ok_or_else(|| missing(session))
    }

    pub fn send(&mut self, params: SessionSendParams) -> Result<SessionSendResult, RpcError> {
        self.live(&params.to_session_id)?;
        let (decoded, encoded) = match &params.payload {
            SessionMessagePayload::Message { data_b64 } => (
                decode_base64_bounded(data_b64, MAX_MESSAGE_BYTES)
                    .map_err(|e| RpcError::invalid_params(e.to_string()))?
                    .len(),
                data_b64.len(),
            ),
            SessionMessagePayload::SessionExited { .. } => (0, 0),
        };
        let message_id = generate_message_id()
            .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string(), None))?;
        let session = params.to_session_id;
        let message = SessionMessage {
            message_id: message_id.clone(),
            to_session_id: session.clone(),
            from_session_id: params.from_session_id,
            from_host_id: params.from_host_id,
            is_remote: params.is_remote,
            payload: params.payload,
        };
        // Charge decoded bodies, but never less than the size of the metadata (including system notifications).
        let overhead = serde_json::to_vec(&message).expect("serializable").len() - encoded;
        let bytes = decoded.max(overhead);
        if bytes > MAX_MESSAGE_BYTES {
            return Err(RpcError::invalid_params(
                "message metadata exceeds 65536 bytes",
            ));
        }

        let inbox = self.active.get_mut(&session).expect("live inbox");
        let full = |inbox: &Inbox| {
            inbox.queue.len() >= MAX_MESSAGES || inbox.bytes + bytes > MAX_SESSION_BYTES
        };
        let global_full = |used| used + bytes > MAX_GLOBAL_BYTES;
        // Check that protected notices leave enough room before evicting anything.
        if full(inbox)
            && (inbox.protected_bytes().count() >= MAX_MESSAGES
                || inbox.protected_bytes().sum::<usize>() + bytes > MAX_SESSION_BYTES)
            || global_full(self.bytes)
                && global_full(
                    self.active
                        .values()
                        .flat_map(Inbox::protected_bytes)
                        .sum::<usize>(),
                )
        {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                "mailbox full of exit notifications",
                None,
            ));
        }
        let inbox = self.active.get_mut(&session).expect("live inbox");
        while full(inbox) {
            let at = inbox
                .queue
                .iter()
                .position(Entry::regular)
                .expect("regular eviction candidate");
            self.bytes -= inbox.remove(at).expect("candidate exists").bytes;
        }
        while global_full(self.bytes) {
            // shortcut: scan live inboxes on global eviction; index regular messages if this becomes hot.
            let (inbox, at, _) = self
                .active
                .values_mut()
                .filter_map(|inbox| {
                    let at = inbox.queue.iter().position(Entry::regular)?;
                    let order = inbox.queue[at].order;
                    Some((inbox, at, order))
                })
                .min_by_key(|(_, _, order)| *order)
                .expect("global regular eviction candidate");
            self.bytes -= inbox.remove(at).expect("candidate exists").bytes;
            self.bytes -= inbox.flush();
        }
        let inbox = self.active.get_mut(&session).expect("live inbox");
        self.order += 1;
        inbox.queue.push_back(Entry {
            message,
            bytes,
            order: self.order,
        });
        inbox.bytes += bytes;
        self.bytes += bytes;
        self.bytes -= inbox.flush();
        inbox.changed.notify_waiters();
        Ok(SessionSendResult { message_id })
    }

    pub fn take(&mut self, session: &str, max: usize) -> Vec<SessionMessage> {
        let Some(inbox) = self.active.get_mut(session) else {
            return Vec::new();
        };
        let mut messages = Vec::new();
        while messages.len() < max
            && let Some(entry) = inbox.remove(0)
        {
            self.bytes -= entry.bytes;
            messages.push(entry.message);
        }
        self.bytes -= inbox.flush();
        messages
    }

    pub fn subscribe(&mut self, session: &str, conn: &Arc<Conn>) -> Result<u32, RpcError> {
        if conn.gone.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RpcError::new(
                ErrorKind::Unavailable,
                "connection closed",
                None,
            ));
        }
        let inbox = self
            .active
            .get_mut(session)
            .ok_or_else(|| missing(session))?;
        let channel = conn.new_channel();
        conn.with_outbox(|out| out.open_paused(channel));
        inbox.subscribers.push_back((conn.clone(), channel));
        self.bytes -= inbox.flush();
        Ok(channel)
    }

    pub fn refill(&mut self, conn: &Conn) {
        for inbox in self.active.values_mut() {
            if inbox.subscribers.iter().any(|(sub, _)| sub.id == conn.id) {
                self.bytes -= inbox.flush();
            }
        }
    }

    pub fn unsubscribe(&mut self, conn: &Conn, channel: Option<u32>) {
        for inbox in self.active.values_mut() {
            inbox
                .subscribers
                .retain(|(sub, ch)| sub.id != conn.id || channel.is_some_and(|c| c != *ch));
        }
    }

    pub fn purge_session(&mut self, session: &str, reason: &CloseReason) {
        if let Some(inbox) = self.active.remove(session) {
            self.bytes -= inbox.bytes;
            inbox.changed.notify_waiters();
            let reason = serde_json::to_vec(reason).expect("serializable");
            for (conn, channel) in inbox.subscribers {
                // Any staged messages already have credit, so CLOSE cannot wait on the client.
                conn.with_outbox(|out| out.finish(channel, &reason));
            }
        }
    }
}

pub fn missing(session: &str) -> RpcError {
    RpcError::session_not_found(format!("no live PTY for session {session}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddg_agents_proto::id::encode_base64;

    fn send(
        boxes: &mut Mailboxes,
        to: &str,
        payload: SessionMessagePayload,
    ) -> Result<String, RpcError> {
        boxes
            .send(SessionSendParams {
                to_session_id: to.into(),
                from_session_id: "sender".into(),
                from_host_id: "host".into(),
                is_remote: false,
                payload,
            })
            .map(|r| r.message_id)
    }

    fn normal(data_b64: &str) -> SessionMessagePayload {
        SessionMessagePayload::Message {
            data_b64: data_b64.into(),
        }
    }

    #[test]
    fn count_eviction_preserves_exit_notification_and_fifo() {
        let mut boxes = Mailboxes::default();
        boxes.open("a", 1);
        let first = send(&mut boxes, "a", normal("")).unwrap();
        let system = send(
            &mut boxes,
            "a",
            SessionMessagePayload::SessionExited {
                session_id: "child".into(),
                room_id: "room".into(),
                run_generation: 9,
                exit_code: None,
            },
        )
        .unwrap();
        let surviving = send(&mut boxes, "a", normal("Zg==")).unwrap();
        for _ in 0..254 {
            send(&mut boxes, "a", normal("")).unwrap();
        }
        let messages = boxes.take("a", 256);
        assert_eq!(messages.len(), 256);
        assert_eq!(messages[0].message_id, system);
        assert_eq!(messages[1].message_id, surviving);
        assert!(!messages.iter().any(|m| m.message_id == first));
        assert_eq!(boxes.bytes, 0);
    }

    #[test]
    fn credited_stream_delivery_releases_the_global_charge() {
        let mut boxes = Mailboxes::default();
        boxes.open("a", 1);
        let id = send(&mut boxes, "a", normal("Zg==")).unwrap();
        assert!(boxes.bytes > 0);
        let conn = Arc::new(Conn::new(1));
        let channel = boxes.subscribe("a", &conn).unwrap();
        assert_eq!(boxes.bytes, 0);
        assert!(
            conn.next_frame().is_none(),
            "stream starts paused for the RPC reply"
        );
        conn.with_outbox(|out| out.resume(channel));
        let frame = conn.next_frame().unwrap();
        let message: SessionMessage =
            serde_json::from_slice(&frame[ddg_agents_proto::frame::HEADER_LEN..]).unwrap();
        assert_eq!(message.message_id, id);
    }

    #[test]
    fn message_session_and_global_byte_caps_have_exact_accounting() {
        let mut boxes = Mailboxes::default();
        boxes.open("0", 1);
        let oversize = encode_base64(&vec![0; 65_537]);
        assert_eq!(
            send(&mut boxes, "0", normal(&oversize))
                .unwrap_err()
                .data
                .code
                .as_deref(),
            Some("invalid_params")
        );
        let data = encode_base64(&vec![0; 65_536]);
        let oldest = send(&mut boxes, "0", normal(&data)).unwrap();
        let next = send(&mut boxes, "0", normal(&data)).unwrap();
        for _ in 0..15 {
            send(&mut boxes, "0", normal(&data)).unwrap();
        }
        assert_eq!(boxes.bytes, 1_048_576);
        let messages = boxes.take("0", 1);
        assert_ne!(messages[0].message_id, oldest);
        assert_eq!(messages[0].message_id, next);
        assert_eq!(boxes.bytes, 1_048_576 - 65_536);

        let global_oldest = boxes.active["0"]
            .queue
            .front()
            .unwrap()
            .message
            .message_id
            .clone();
        for n in 1..=31 {
            let id = n.to_string();
            boxes.open(&id, n + 1);
            for _ in 0..16 {
                send(&mut boxes, &id, normal(&data)).unwrap();
            }
        }
        boxes.open("overflow", 33);
        for _ in 0..2 {
            send(&mut boxes, "overflow", normal(&data)).unwrap();
        }
        assert_eq!(boxes.bytes, 33_554_432);
        let messages = boxes.take("0", 256);
        assert_eq!(messages.len(), 14);
        assert!(!messages.iter().any(|m| m.message_id == global_oldest));
        for n in 0..=31 {
            boxes.purge_session(&n.to_string(), &CloseReason::Detached);
        }
        boxes.purge_session("overflow", &CloseReason::Detached);
        assert_eq!(boxes.bytes, 0);
    }
}
