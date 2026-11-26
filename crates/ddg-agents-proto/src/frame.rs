//! Binary framed wire layout:
//! `[channel: u32 LE][kind: u8][len: u32 LE][payload: len bytes]`
//!
//! Channel 0 carries JSON-RPC 2.0 control messages; non-zero channels carry
//! multiplexed terminal byte streams or event streams.

use std::io::{self, Read, Write};

pub const HEADER_LEN: usize = 9;
pub const MAX_PAYLOAD: usize = 16 << 20;
pub const CONTROL_CHANNEL: u32 = 0;

/// Initial receiver credit per stream channel (256 KiB).
pub const INITIAL_CREDIT: usize = 256 * 1024;
/// Maximum queued stream bytes before dropping backlog and sending a repaint (4 MiB).
pub const MAX_PENDING: usize = 4 << 20;
/// Maximum queued control bytes before dropping a misbehaving client (64 MiB).
pub const MAX_CONTROL: usize = 64 << 20;
/// Maximum DATA payload size per round-robin slice (16 KiB).
pub const DATA_CHUNK: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Data = 0,
    Credit = 1,
    Close = 2,
    Resize = 3,
}

impl TryFrom<u8> for FrameKind {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Data),
            1 => Ok(Self::Credit),
            2 => Ok(Self::Close),
            3 => Ok(Self::Resize),
            other => Err(format!("unknown frame kind: {other}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub channel: u32,
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn data(channel: u32, payload: Vec<u8>) -> Self {
        Self {
            channel,
            kind: FrameKind::Data,
            payload,
        }
    }

    pub fn credit(channel: u32, bytes: u32) -> Self {
        Self {
            channel,
            kind: FrameKind::Credit,
            payload: bytes.to_le_bytes().to_vec(),
        }
    }

    pub fn close(channel: u32, reason: Vec<u8>) -> Self {
        Self {
            channel,
            kind: FrameKind::Close,
            payload: reason,
        }
    }

    pub fn resize(channel: u32, cols: u16, rows: u16) -> Self {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&cols.to_le_bytes());
        payload.extend_from_slice(&rows.to_le_bytes());
        Self {
            channel,
            kind: FrameKind::Resize,
            payload,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        encode_frame(self.channel, self.kind, &self.payload)
    }
}

pub fn encode_frame(channel: u32, kind: FrameKind, payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() > MAX_PAYLOAD {
        return Err(format!(
            "frame payload of {} bytes exceeds max {}",
            payload.len(),
            MAX_PAYLOAD
        ));
    }
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&channel.to_le_bytes());
    out.push(kind as u8);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn decode_header(header: &[u8; HEADER_LEN]) -> Result<(u32, FrameKind, usize), String> {
    let channel = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let kind = FrameKind::try_from(header[4])?;
    let len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(format!(
            "frame payload of {len} bytes exceeds max {MAX_PAYLOAD}"
        ));
    }
    Ok((channel, kind, len))
}

pub fn decode_credit(payload: &[u8]) -> Result<u32, String> {
    let bytes: [u8; 4] = payload
        .try_into()
        .map_err(|_| format!("invalid CREDIT payload length: {}", payload.len()))?;
    Ok(u32::from_le_bytes(bytes))
}

pub fn decode_resize(payload: &[u8]) -> Result<(u16, u16), String> {
    if payload.len() != 4 {
        return Err(format!("invalid RESIZE payload length: {}", payload.len()));
    }
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    if cols == 0 || rows == 0 {
        return Err(format!("invalid terminal resize dimensions: {cols}x{rows}"));
    }
    Ok((cols, rows))
}

/// Reads one frame from a synchronous reader. Returns `Ok(None)` on clean EOF
/// between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    let mut read = 0;
    while read < HEADER_LEN {
        match r.read(&mut header[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame header",
                ));
            }
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let (channel, kind, len) = decode_header(&header).map_err(io::Error::other)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame {
        channel,
        kind,
        payload,
    }))
}

pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let encoded = frame.encode().map_err(io::Error::other)?;
    w.write_all(&encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_all_four_frame_kinds() {
        let frames = vec![
            Frame::data(0, br#"{"jsonrpc":"2.0"}"#.to_vec()),
            Frame::credit(7, 65536),
            Frame::close(7, br#"{"reason":"detached"}"#.to_vec()),
            Frame::resize(7, 120, 40),
        ];
        let mut wire = Vec::new();
        for f in &frames {
            write_frame(&mut wire, f).unwrap();
        }
        let mut cursor = Cursor::new(wire);
        let mut decoded = Vec::new();
        while let Some(f) = read_frame(&mut cursor).unwrap() {
            decoded.push(f);
        }
        assert_eq!(decoded, frames);
        assert_eq!(decode_credit(&decoded[1].payload).unwrap(), 65536);
        assert_eq!(decode_resize(&decoded[3].payload).unwrap(), (120, 40));
    }

    #[test]
    fn rejects_invalid_headers_and_dimensions() {
        let mut bad_kind = [0u8; HEADER_LEN];
        bad_kind[4] = 99;
        assert!(decode_header(&bad_kind).is_err());

        let mut too_big = [0u8; HEADER_LEN];
        too_big[5..9].copy_from_slice(&((MAX_PAYLOAD as u32) + 1).to_le_bytes());
        assert!(decode_header(&too_big).is_err());

        assert!(decode_resize(&[0, 0, 24, 0]).is_err());
        assert!(decode_resize(&[80, 0]).is_err());
        assert!(decode_credit(&[1, 2]).is_err());

        let mut truncated = Cursor::new(vec![0u8; 4]);
        assert!(read_frame(&mut truncated).is_err());
    }
}
