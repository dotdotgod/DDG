use super::{ActiveConn, ChannelKind};
use ddg_agents_proto::frame::{
    DATA_CHUNK, Frame, FrameKind, INITIAL_CREDIT, MAX_PENDING, decode_credit, encode_frame,
};
use ddg_agents_proto::host::RelayFrame;
use ddg_agents_proto::terminal::{CloseReason, SessionMessage};
use serde::de::DeserializeOwned;
use std::{collections::VecDeque, fmt, marker::PhantomData, sync::Arc};
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamError {
    Closed(CloseReason),
    Disconnected,
    InvalidFrame,
    Backpressure,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(reason) => write!(f, "stream closed: {reason:?}"),
            Self::Disconnected => f.write_str("stream disconnected"),
            Self::InvalidFrame => f.write_str("invalid stream frame or NDJSON record"),
            Self::Backpressure => f.write_str("stream output buffer is full"),
        }
    }
}

impl std::error::Error for StreamError {}

type Inbound = Result<Vec<u8>, StreamError>;
type Wire = mpsc::UnboundedSender<Vec<u8>>;

pub(super) struct StreamIngress {
    tx: mpsc::UnboundedSender<Inbound>,
    receive_credit: usize,
    send_credit: usize,
    pending: VecDeque<u8>,
}

impl StreamIngress {
    pub(super) fn new() -> (Self, mpsc::UnboundedReceiver<Inbound>) {
        // The peer's byte window bounds the inbound queue. Empty frames are not queued.
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                receive_credit: INITIAL_CREDIT,
                send_credit: INITIAL_CREDIT,
                pending: VecDeque::new(),
            },
            rx,
        )
    }

    /// Returns false when the channel should be removed from the connection.
    pub(super) fn on_frame(
        &mut self,
        wire: &Wire,
        channel: u32,
        kind: FrameKind,
        payload: Vec<u8>,
    ) -> bool {
        let result = match kind {
            FrameKind::Data if payload.is_empty() => return true,
            FrameKind::Data if payload.len() <= self.receive_credit => {
                self.receive_credit -= payload.len();
                return self.tx.send(Ok(payload)).is_ok();
            }
            FrameKind::Credit => match decode_credit(&payload) {
                Ok(bytes) => {
                    self.send_credit = self
                        .send_credit
                        .saturating_add(bytes as usize)
                        .min(INITIAL_CREDIT);
                    match self.flush(wire, channel) {
                        Ok(()) => return true,
                        Err(err) => err,
                    }
                }
                Err(_) => StreamError::InvalidFrame,
            },
            FrameKind::Close if payload.is_empty() => StreamError::Closed(CloseReason::Detached),
            FrameKind::Close => serde_json::from_slice(&payload)
                .map(StreamError::Closed)
                .unwrap_or(StreamError::InvalidFrame),
            _ => StreamError::InvalidFrame,
        };
        let _ = self.tx.send(Err(result));
        if kind != FrameKind::Close {
            send_close(wire, channel);
        }
        false
    }

    fn flush(&mut self, wire: &Wire, channel: u32) -> Result<(), StreamError> {
        while self.send_credit > 0 && !self.pending.is_empty() {
            let len = self.send_credit.min(DATA_CHUNK).min(self.pending.len());
            let payload: Vec<_> = self.pending.drain(..len).collect();
            send_frame(wire, Frame::data(channel, payload))?;
            self.send_credit -= len;
        }
        Ok(())
    }
}

fn send_frame(wire: &Wire, frame: Frame) -> Result<(), StreamError> {
    let bytes = frame.encode().map_err(|_| StreamError::InvalidFrame)?;
    wire.send(bytes).map_err(|_| StreamError::Disconnected)
}

pub(super) fn send_close(wire: &Wire, channel: u32) {
    if let Ok(bytes) = encode_frame(channel, FrameKind::Close, &[]) {
        let _ = wire.send(bytes);
    }
}

pub type RelayStreamHandle = NdjsonStream<RelayFrame>;
pub type MessageStreamHandle = NdjsonStream<SessionMessage>;

/// A credit-controlled stream tied to the connection that opened it. Drop it
/// to detach. It never silently attaches to a replacement connection.
pub struct NdjsonStream<T> {
    connection: Arc<ActiveConn>,
    channel: u32,
    rx: mpsc::UnboundedReceiver<Inbound>,
    chunk: Vec<u8>,
    offset: usize,
    line: Vec<u8>,
    record: PhantomData<T>,
}

impl<T> NdjsonStream<T> {
    pub(super) fn new(
        connection: Arc<ActiveConn>,
        channel: u32,
        rx: mpsc::UnboundedReceiver<Inbound>,
    ) -> Self {
        Self {
            connection,
            channel,
            rx,
            chunk: Vec::new(),
            offset: 0,
            line: Vec::new(),
            record: PhantomData,
        }
    }

    pub fn channel(&self) -> u32 {
        self.channel
    }

    fn with_channel<R>(
        &self,
        f: impl FnOnce(&mut StreamIngress) -> Result<R, StreamError>,
    ) -> Result<R, StreamError> {
        let mut channels = self
            .connection
            .channels
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !self.rx.is_closed()
            && let Some(ChannelKind::Ndjson(stream)) = channels.get_mut(&self.channel)
        {
            return f(stream);
        }
        Err(StreamError::Disconnected)
    }

    fn detach(&self) {
        let mut channels = self
            .connection
            .channels
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !self.rx.is_closed() && channels.remove(&self.channel).is_some() {
            send_close(&self.connection.tx, self.channel);
        }
    }
}

impl<T: DeserializeOwned> NdjsonStream<T> {
    /// Reads one NDJSON record; a peer's CLOSE is returned as `StreamError::Closed`.
    /// Partial records survive cancellation of the receive future.
    pub async fn recv(&mut self) -> Result<T, StreamError> {
        loop {
            if self.offset == self.chunk.len() {
                self.chunk = self.rx.recv().await.ok_or(StreamError::Disconnected)??;
                self.offset = 0;
            }
            let rest = &self.chunk[self.offset..];
            let newline = rest.iter().position(|&b| b == b'\n');
            let len = newline.map_or(rest.len(), |pos| pos + 1);
            if self.line.len() + len > MAX_PENDING {
                self.detach();
                return Err(StreamError::InvalidFrame);
            }
            self.line.extend_from_slice(&rest[..len]);
            self.offset += len;
            if self.offset == self.chunk.len() {
                // Credit is returned as the caller consumes bytes, not while the
                // shared connection reader is filling its application queue.
                let _ = self.with_channel(|stream| {
                    send_frame(
                        &self.connection.tx,
                        Frame::credit(self.channel, self.offset as u32),
                    )?;
                    stream.receive_credit += self.offset;
                    Ok(())
                });
            }
            if newline.is_some() {
                let result =
                    serde_json::from_slice(&self.line).map_err(|_| StreamError::InvalidFrame);
                self.line.clear();
                if result.is_err() {
                    self.detach();
                }
                return result;
            }
        }
    }
}

impl NdjsonStream<RelayFrame> {
    /// Queues a whole relay record. Wire writes stop when peer credit runs out.
    pub fn send(&self, record: &RelayFrame) -> Result<(), StreamError> {
        let mut line = serde_json::to_vec(record).map_err(|_| StreamError::InvalidFrame)?;
        line.push(b'\n');
        self.with_channel(|stream| {
            if line.len() > MAX_PENDING {
                return Err(StreamError::InvalidFrame);
            }
            if stream.pending.len() + line.len() > MAX_PENDING {
                return Err(StreamError::Backpressure);
            }
            stream.pending.extend(line);
            stream.flush(&self.connection.tx, self.channel)
        })
    }
}

impl<T> Drop for NdjsonStream<T> {
    fn drop(&mut self) {
        self.detach();
    }
}
