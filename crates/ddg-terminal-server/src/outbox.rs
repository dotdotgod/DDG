//! Per-connection credit-based binary frame scheduler (`Outbox`).
//!
//! Control frames (channel 0 JSON-RPC + CLOSE) always go first. Stream channels
//! share remaining bandwidth round-robin in 16 KiB slices bounded by each
//! channel's receiver credit (`INITIAL_CREDIT = 256 KiB`). A slow viewer whose
//! pending queue exceeds `MAX_PENDING = 4 MiB` has its backlog dropped and
//! replaced with a fresh VT `paint()` resync.

use ddg_agents_proto::frame::{
    CONTROL_CHANNEL, DATA_CHUNK, FrameKind, INITIAL_CREDIT, MAX_CONTROL, MAX_PENDING, encode_frame,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
pub struct Conn {
    pub id: u64,
    outbox: Mutex<Outbox>,
    pub wake: Notify,
    pub overflowed: Notify,
    pub disconnected: Notify,
    next_channel: AtomicU32,
    pub gone: AtomicBool,
}

impl Conn {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }

    pub fn new_channel(&self) -> u32 {
        self.next_channel.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn with_outbox<R>(&self, f: impl FnOnce(&mut Outbox) -> R) -> R {
        let mut out = self.outbox.lock().unwrap_or_else(|p| p.into_inner());
        let res = f(&mut out);
        if out.overflowed {
            self.overflowed.notify_one();
        }
        drop(out);
        self.wake.notify_one();
        res
    }

    /// Stage a complete inbox record only when its credit is already available.
    pub fn try_inbox_data(&self, channel: u32, bytes: &[u8]) -> bool {
        let mut out = self.outbox.lock().unwrap_or_else(|p| p.into_inner());
        if self.gone.load(Ordering::SeqCst) {
            return false;
        }
        let Some(s) = out.streams.get_mut(&channel) else {
            return false;
        };
        let room = s.credit.min(MAX_PENDING).saturating_sub(s.pending.len());
        if bytes.len() > room {
            return false;
        }
        s.pending.extend(bytes);
        drop(out);
        self.wake.notify_one();
        true
    }

    pub fn next_frame(&self) -> Option<Vec<u8>> {
        self.outbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_frame()
    }
}

struct Stream {
    pending: VecDeque<u8>,
    resizes: VecDeque<(usize, [u8; 4])>,
    paused: bool,
    credit: usize,
    ending: Option<Vec<u8>>,
}

#[derive(Default)]
pub struct Outbox {
    control: VecDeque<Vec<u8>>,
    control_bytes: usize,
    pub overflowed: bool,
    streams: BTreeMap<u32, Stream>,
    turn: u32,
}

impl Outbox {
    pub fn control_json(&mut self, json_bytes: &[u8]) {
        if let Ok(frame) = encode_frame(CONTROL_CHANNEL, FrameKind::Data, json_bytes) {
            self.control_raw(frame);
        }
    }

    pub fn control_raw(&mut self, frame: Vec<u8>) {
        if self.overflowed {
            return;
        }
        self.control_bytes += frame.len();
        if self.control_bytes > MAX_CONTROL {
            *self = Self {
                overflowed: true,
                ..Self::default()
            };
            return;
        }
        self.control.push_back(frame);
    }

    pub fn open_paused(&mut self, channel: u32) {
        self.streams.insert(
            channel,
            Stream {
                pending: VecDeque::new(),
                resizes: VecDeque::new(),
                paused: true,
                credit: INITIAL_CREDIT,
                ending: None,
            },
        );
    }

    #[cfg(test)]
    pub fn open(&mut self, channel: u32) {
        self.open_paused(channel);
        self.resume(channel);
    }

    pub fn resume(&mut self, channel: u32) {
        if let Some(s) = self.streams.get_mut(&channel) {
            s.paused = false;
        }
    }

    pub fn credit(&mut self, channel: u32, bytes: usize) {
        if let Some(s) = self.streams.get_mut(&channel) {
            s.credit = s.credit.saturating_add(bytes);
        }
    }

    pub fn resize(&mut self, channel: u32, cols: u16, rows: u16) {
        if let Some(s) = self.streams.get_mut(&channel) {
            let mut payload = [0u8; 4];
            payload[0..2].copy_from_slice(&cols.to_le_bytes());
            payload[2..4].copy_from_slice(&rows.to_le_bytes());
            s.resizes.push_back((s.pending.len(), payload));
        }
    }

    /// Queues stream bytes. Returns `false` (after clearing pending backlog)
    /// when adding `bytes` would exceed `MAX_PENDING` so caller can enqueue a
    /// fresh repaint instead.
    pub fn data(&mut self, channel: u32, bytes: &[u8]) -> bool {
        let Some(s) = self.streams.get_mut(&channel) else {
            return true;
        };
        if s.pending.len() + bytes.len() > MAX_PENDING {
            s.pending.clear();
            let latest_resize = s.resizes.pop_back();
            s.resizes.clear();
            if let Some((_, size)) = latest_resize {
                s.resizes.push_back((0, size));
            }
            return false;
        }
        s.pending.extend(bytes);
        true
    }

    /// Queues NDJSON event lines; on overflow keeps only the remainder of any
    /// partially sent line so a `missed` snapshot starts cleanly on a new line.
    pub fn data_lines(&mut self, channel: u32, bytes: &[u8]) -> bool {
        let Some(s) = self.streams.get_mut(&channel) else {
            return true;
        };
        if s.pending.len() + bytes.len() > MAX_PENDING {
            let rest = s
                .pending
                .iter()
                .position(|b| *b == b'\n')
                .map_or(s.pending.len(), |at| at + 1);
            s.pending.truncate(rest);
            return false;
        }
        s.pending.extend(bytes);
        true
    }

    pub fn queued(&self, channel: u32) -> usize {
        self.streams.get(&channel).map_or(0, |s| s.pending.len())
    }

    /// Immediately drops pending output and sends CLOSE (client detached).
    pub fn close_now(&mut self, channel: u32, reason: &[u8]) {
        self.streams.remove(&channel);
        if let Ok(frame) = encode_frame(channel, FrameKind::Close, reason) {
            self.control_raw(frame);
        }
    }

    /// Delivers queued output first, then sends CLOSE (process exited).
    pub fn finish(&mut self, channel: u32, reason: &[u8]) {
        match self.streams.get_mut(&channel) {
            Some(s) => s.ending = Some(reason.to_vec()),
            None => {
                if let Ok(frame) = encode_frame(channel, FrameKind::Close, reason) {
                    self.control_raw(frame);
                }
            }
        }
    }

    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if let Some(frame) = self.control.pop_front() {
            self.control_bytes -= frame.len();
            return Some(frame);
        }
        if let Some((&channel, _)) = self
            .streams
            .iter()
            .find(|(_, s)| !s.paused && s.ending.is_some() && s.pending.is_empty())
        {
            let reason = self
                .streams
                .remove(&channel)
                .and_then(|s| s.ending)
                .unwrap_or_default();
            return encode_frame(channel, FrameKind::Close, &reason).ok();
        }
        let ready: Vec<u32> = self
            .streams
            .iter()
            .filter(|(_, s)| {
                !s.paused
                    && ((s.credit > 0 && !s.pending.is_empty())
                        || s.resizes.front().is_some_and(|(at, _)| *at == 0))
            })
            .map(|(ch, _)| *ch)
            .collect();
        let channel = *ready.iter().find(|ch| **ch > self.turn).or(ready.first())?;
        self.turn = channel;
        let s = self.streams.get_mut(&channel).expect("ready stream");
        if s.resizes.front().is_some_and(|(at, _)| *at == 0) {
            let (_, size) = s.resizes.pop_front().unwrap();
            return encode_frame(channel, FrameKind::Resize, &size).ok();
        }
        let boundary = s.resizes.front().map_or(usize::MAX, |(at, _)| *at);
        let n = s.pending.len().min(s.credit).min(DATA_CHUNK).min(boundary);
        for (at, _) in &mut s.resizes {
            *at -= n;
        }
        s.credit -= n;
        let chunk: Vec<u8> = s.pending.drain(..n).collect();
        encode_frame(channel, FrameKind::Data, &chunk).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddg_agents_proto::frame::HEADER_LEN;

    #[test]
    fn stalled_stream_never_blocks_control_or_other_streams_and_resyncs_past_4mib() {
        let mut out = Outbox::default();
        out.open(1);
        out.open(2);
        assert!(out.data(1, &vec![b'a'; INITIAL_CREDIT * 2]));
        let mut sent = 0;
        while let Some(f) = out.next_frame() {
            sent += f.len() - HEADER_LEN;
        }
        assert_eq!(sent, INITIAL_CREDIT);

        out.control_json(b"{\"ok\":true}");
        assert!(out.data(2, b"other"));
        assert_eq!(&out.next_frame().unwrap()[HEADER_LEN..], b"{\"ok\":true}");
        assert_eq!(&out.next_frame().unwrap()[HEADER_LEN..], b"other");
        assert!(out.next_frame().is_none());

        // Past MAX_PENDING: returns false and drops backlog for fresh repaint.
        assert!(!out.data(1, &vec![b'b'; MAX_PENDING]));
        assert!(out.data(1, b"\x18FRESH"));
        out.credit(1, 64);
        assert_eq!(&out.next_frame().unwrap()[HEADER_LEN..], b"\x18FRESH");
    }

    #[test]
    fn finish_delivers_final_bytes_before_close() {
        let mut out = Outbox::default();
        out.open(1);
        assert!(out.data(1, &vec![b'x'; INITIAL_CREDIT]));
        assert!(out.data(1, b"END"));
        out.finish(1, b"{}");
        while let Some(f) = out.next_frame() {
            assert_eq!(f[4], FrameKind::Data as u8);
        }
        out.credit(1, 3);
        assert_eq!(&out.next_frame().unwrap()[HEADER_LEN..], b"END");
        let close = out.next_frame().unwrap();
        assert_eq!(close[4], FrameKind::Close as u8);
    }
}
