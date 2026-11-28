//! Internal `ddg-terminal-server` Core ↔ Holder binary wire protocol:
//! `[kind: u8][pty_id: u32 LE][len: u32 LE][payload: len bytes]`

use crate::frame::MAX_PAYLOAD;
pub use crate::terminal::{
    BinProbeItem as HolderBinStatus, BinProbeStatus, PtyProbeBinsParams as HolderProbeBins,
    PtyProbeBinsResult as HolderProbeBinsResult,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Read};

pub const HOLDER_PROTO_VERSION: u32 = 1;
pub const HOLDER_HEADER_LEN: usize = 9;
pub const FEATURE_PROBE_BINS_V1: &str = "probe_bins_v1";

// Core → Holder
pub const HELLO: u8 = 1;
pub const SPAWN: u8 = 2;
pub const INPUT: u8 = 3;
pub const RESIZE: u8 = 4;
pub const SIGNAL: u8 = 5;
pub const RELEASE: u8 = 6;
pub const GOODBYE: u8 = 7;
/// Query the live foreground process group, or all live PTYs when pty_id is 0.
pub const QUERY_FG_PGRP: u8 = 8;
/// Probe only the built-in basenames; the header's `pty_id` carries the request ID.
pub const QUERY_BINS: u8 = 9;

// Holder → Core
pub const WELCOME: u8 = 101;
pub const TAIL: u8 = 102;
pub const OUTPUT: u8 = 103;
pub const SPAWNED: u8 = 104;
pub const SPAWN_FAILED: u8 = 105;
pub const EXITED: u8 = 106;
pub const BYE: u8 = 107;
pub const FG_PGRP: u8 = 108;
/// Reply to QUERY_BINS; echoes its request ID in the header's `pty_id`.
pub const BINS_RESULT: u8 = 109;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderHello {
    pub version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderSpawn {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub run_generation: u64,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderSignal {
    pub signal: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HolderGoodbye {
    Detach,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldPty {
    pub pty_id: u32,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub run_generation: u64,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    pub exit: Option<HolderExit>,
    pub raw_offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderWelcome {
    pub version: u32,
    pub instance: String,
    pub pid: u32,
    pub ptys: Vec<HeldPty>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderFgPgrp {
    pub pty_id: u32,
    pub pgrp: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderFgPgrpBatch {
    pub entries: Vec<HolderFgPgrp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderSpawned {
    pub pid: u32,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub run_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderSpawnFailed {
    pub error: String,
}

pub fn encode_frame(kind: u8, pty_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HOLDER_HEADER_LEN + payload.len());
    out.push(kind);
    out.extend_from_slice(&pty_id.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn encode_json(kind: u8, pty_id: u32, msg: &impl Serialize) -> Vec<u8> {
    encode_frame(
        kind,
        pty_id,
        &serde_json::to_vec(msg).expect("serializable"),
    )
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, u32, Vec<u8>)>> {
    let mut header = [0u8; HOLDER_HEADER_LEN];
    let mut read = 0;
    while read < HOLDER_HEADER_LEN {
        match r.read(&mut header[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated holder frame header",
                ));
            }
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let pty_id = u32::from_le_bytes(header[1..5].try_into().unwrap());
    let len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::other(format!("holder frame of {len} bytes")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((header[0], pty_id, payload)))
}

pub fn parse_json<T: serde::de::DeserializeOwned>(payload: &[u8]) -> io::Result<T> {
    serde_json::from_slice(payload).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // UT-PROTO-01
    #[test]
    fn preset_probe_round_trips_with_request_id() {
        assert_eq!(
            crate::BUILTIN_AGENT_PRESET_BINS,
            ["pi", "agy", "codex", "opencode", "claude"]
        );
        let request_id = 0x0102_0304;
        let frame = encode_json(QUERY_BINS, request_id, &HolderProbeBins {});
        let (kind, id, payload) = read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!((kind, id), (9, request_id));
        assert_eq!(payload, b"{}");
        assert_eq!(
            parse_json::<HolderProbeBins>(&payload).unwrap(),
            HolderProbeBins {}
        );
        assert!(parse_json::<HolderProbeBins>(br#"{"bins":["sh"]}"#).is_err());
        assert!(parse_json::<HolderProbeBins>(b"[]").is_err());

        let reply = HolderProbeBinsResult {
            items: vec![HolderBinStatus {
                bin: "pi".into(),
                status: BinProbeStatus::Found,
            }],
        };
        let frame = encode_json(BINS_RESULT, id, &reply);
        let (kind, id, payload) = read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!((kind, id), (109, request_id));
        assert_eq!(
            parse_json::<HolderProbeBinsResult>(&payload).unwrap(),
            reply
        );
        assert_eq!(
            parse_json::<serde_json::Value>(&payload).unwrap(),
            json!({"items": [{"bin": "pi", "status": "found"}]})
        );
    }

    #[test]
    fn holder_v1_welcome_negotiates_optional_features() {
        let mut wire = json!({"version": 1, "instance": "holder", "pid": 2, "ptys": []});
        let welcome: HolderWelcome = serde_json::from_value(wire.clone()).unwrap();
        assert!(welcome.features.is_empty());
        assert_eq!(json!(welcome), wire);
        wire["features"] = json!(["fg_pgrp_v1", FEATURE_PROBE_BINS_V1]);
        assert_eq!(wire["features"][1], "probe_bins_v1");
        let welcome: HolderWelcome = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(json!(welcome), wire);
    }

    #[test]
    fn holder_foreground_frames_and_payload_boundaries() {
        let query = encode_frame(QUERY_FG_PGRP, 0, &[]);
        assert_eq!(query, [8, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            read_frame(&mut query.as_slice()).unwrap(),
            Some((8, 0, vec![]))
        );

        let reply = HolderFgPgrpBatch {
            entries: vec![
                HolderFgPgrp {
                    pty_id: 2,
                    pgrp: Some(42),
                },
                HolderFgPgrp {
                    pty_id: 3,
                    pgrp: None,
                },
            ],
        };
        let frame = encode_json(FG_PGRP, 0, &reply);
        let (kind, pty_id, payload) = read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!((kind, pty_id), (108, 0));
        assert_eq!(
            parse_json::<serde_json::Value>(&payload).unwrap(),
            json!({
                "entries": [{"pty_id": 2, "pgrp": 42}, {"pty_id": 3, "pgrp": null}],
            })
        );
        assert_eq!(parse_json::<HolderFgPgrpBatch>(&payload).unwrap(), reply);

        let mut frame = encode_frame(FG_PGRP, 0, &vec![1; MAX_PAYLOAD]);
        let (_, _, payload) = read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!(payload.len(), MAX_PAYLOAD);
        assert!(payload.iter().all(|&b| b == 1));
        frame[5..9].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert_eq!(
            read_frame(&mut &frame[..HOLDER_HEADER_LEN])
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other,
        );
        assert_eq!(
            read_frame(&mut &frame[..8]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof,
        );
    }

    #[test]
    fn holder_v1_defaults_and_round_trips_session_metadata() {
        assert_eq!(HOLDER_PROTO_VERSION, 1);
        let old_spawn = json!({"argv": [], "env": {}, "cwd": "/", "cols": 80, "rows": 24});
        let old_pty = json!({"pty_id": 1, "pid": 2, "cols": 80, "rows": 24, "raw_offset": 0});
        fn check<T: serde::de::DeserializeOwned + Serialize>(old: serde_json::Value) {
            let v = json!(serde_json::from_value::<T>(old.clone()).unwrap());
            assert_eq!(v["session_id"], "");
            assert_eq!(v["run_generation"], 0);

            let mut new = old;
            new["session_id"] = json!("ses_00000000000000000000000001");
            new["run_generation"] = json!(7);
            let v = json!(serde_json::from_value::<T>(new.clone()).unwrap());
            assert_eq!(v["session_id"], new["session_id"]);
            assert_eq!(v["run_generation"], new["run_generation"]);
        }
        check::<HolderSpawn>(old_spawn);
        check::<HeldPty>(old_pty);
        check::<HolderSpawned>(json!({"pid": 2}));
    }
}
