//! Time-sortable session, room, and message IDs and wire-input helpers.

use base64::{Engine, engine::general_purpose::STANDARD};
use std::fmt;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const ROOM_PREFIX: &str = "rm_";
const BACKGROUND_ROOM_PREFIX: &str = "rm_bg_";
const SESSION_PREFIX: &str = "ses_";
const MESSAGE_PREFIX: &str = "msg_";

#[derive(Debug)]
pub enum IdError {
    InvalidId,
    InvalidHostPrefix,
    InvalidTimestamp,
    Random(io::Error),
    InvalidBase64,
    Base64TooLarge,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId => f.write_str("invalid Crockford ID"),
            Self::InvalidHostPrefix => f.write_str("invalid background room host prefix"),
            Self::InvalidTimestamp => f.write_str("timestamp out of 48-bit range"),
            Self::Random(err) => write!(f, "OS randomness unavailable: {err}"),
            Self::InvalidBase64 => f.write_str("invalid RFC 4648 base64"),
            Self::Base64TooLarge => f.write_str("base64 payload exceeds decoded byte limit"),
        }
    }
}

impl std::error::Error for IdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Random(err) => Some(err),
            _ => None,
        }
    }
}

/// Fills the entire buffer with OS randomness or returns an error. Do not use
/// any bytes in the buffer after an error.
pub fn fill_csprng(mut bytes: &mut [u8]) -> Result<(), IdError> {
    while !bytes.is_empty() {
        // getentropy accepts up to 256 bytes per call. Small getrandom reads
        // also avoid signal-induced short reads once the Linux RNG is ready.
        let chunk_len = bytes.len().min(256);
        match os_random(&mut bytes[..chunk_len]) {
            Ok(0) => {
                return Err(IdError::Random(io::Error::from(
                    io::ErrorKind::UnexpectedEof,
                )));
            }
            Ok(n) => bytes = &mut bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(IdError::Random(e)),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn os_random(bytes: &mut [u8]) -> io::Result<usize> {
    // SAFETY: bytes is writable for its full length; getrandom keeps no pointer.
    let n = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn os_random(bytes: &mut [u8]) -> io::Result<usize> {
    // SAFETY: bytes is writable for its full length; getentropy keeps no pointer.
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes.len())
}

#[cfg(windows)]
fn os_random(bytes: &mut [u8]) -> io::Result<usize> {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    // fill_csprng limits each call to 256 bytes.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom NTSTATUS {status:#x}"
        )));
    }
    Ok(bytes.len())
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn os_random(_: &mut [u8]) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}

fn encode_crockford(value: u128) -> String {
    (0..26)
        .rev()
        .map(|n| char::from(CROCKFORD[((value >> (n * 5)) & 31) as usize]))
        .collect()
}

fn parse_id(id: &str, prefix: &str) -> Result<u128, IdError> {
    let suffix = id
        .strip_prefix(prefix)
        .ok_or(IdError::InvalidId)?
        .as_bytes();
    if suffix.len() != 26 || !(b'0'..=b'7').contains(&suffix[0]) {
        return Err(IdError::InvalidId);
    }
    suffix.iter().try_fold(0, |value, &b| {
        let digit = CROCKFORD
            .iter()
            .position(|&c| c == b)
            .ok_or(IdError::InvalidId)?;
        Ok((value << 5) | digit as u128)
    })
}

fn generate_id_at(
    prefix: &str,
    millis: u128,
    random: impl FnOnce(&mut [u8]) -> Result<(), IdError>,
) -> Result<String, IdError> {
    if millis >= 1 << 48 {
        return Err(IdError::InvalidTimestamp);
    }
    let mut bytes = (millis << 80).to_be_bytes();
    random(&mut bytes[6..])?;
    Ok(format!(
        "{prefix}{}",
        encode_crockford(u128::from_be_bytes(bytes))
    ))
}

fn generate_id(prefix: &str) -> Result<String, IdError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| IdError::InvalidTimestamp)?
        .as_millis();
    generate_id_at(prefix, millis, fill_csprng)
}

/// IDs of the same kind sort by creation millisecond, not within a millisecond.
pub fn generate_room_id() -> Result<String, IdError> {
    generate_id(ROOM_PREFIX)
}

pub fn generate_session_id() -> Result<String, IdError> {
    generate_id(SESSION_PREFIX)
}

pub fn generate_message_id() -> Result<String, IdError> {
    generate_id(MESSAGE_PREFIX)
}

pub fn parse_standard_room_id(id: &str) -> Result<u128, IdError> {
    parse_id(id, ROOM_PREFIX)
}

pub fn parse_session_id(id: &str) -> Result<u128, IdError> {
    parse_id(id, SESSION_PREFIX)
}

pub fn parse_message_id(id: &str) -> Result<u128, IdError> {
    parse_id(id, MESSAGE_PREFIX)
}

fn validate_host_prefix(prefix: &str) -> Result<(), IdError> {
    if prefix.is_empty()
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    {
        return Err(IdError::InvalidHostPrefix);
    }
    Ok(())
}

/// Builds a background room ID from a nonempty lowercase ASCII alphanumeric host prefix.
pub fn background_room_id(host_prefix: &str) -> Result<String, IdError> {
    validate_host_prefix(host_prefix)?;
    Ok(format!("{BACKGROUND_ROOM_PREFIX}{host_prefix}"))
}

pub fn validate_background_room_id(id: &str) -> Result<(), IdError> {
    validate_host_prefix(
        id.strip_prefix(BACKGROUND_ROOM_PREFIX)
            .ok_or(IdError::InvalidId)?,
    )
}

pub fn validate_room_id(id: &str) -> Result<(), IdError> {
    if id.starts_with(BACKGROUND_ROOM_PREFIX) {
        validate_background_room_id(id)
    } else {
        parse_standard_room_id(id).map(|_| ())
    }
}

pub fn encode_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decodes the standard, canonically padded RFC 4648 alphabet without
/// allocating a decoded buffer for oversized input.
pub fn decode_base64_bounded(s: &str, max_bytes: usize) -> Result<Vec<u8>, IdError> {
    if s.len() > max_bytes.div_ceil(3).saturating_mul(4) {
        return Err(IdError::Base64TooLarge);
    }
    if !s.len().is_multiple_of(4) {
        return Err(IdError::InvalidBase64);
    }
    let padding = if s.ends_with("==") {
        2
    } else {
        usize::from(s.ends_with('='))
    };
    if s.len() / 4 * 3 - padding > max_bytes {
        return Err(IdError::Base64TooLarge);
    }
    STANDARD.decode(s).map_err(|_| IdError::InvalidBase64)
}

pub fn sanitize_display_text(s: &str, max_chars: usize) -> String {
    s.chars()
        .filter(|&c| !c.is_control()
            && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn crockford_ids_are_canonical_and_time_sorted() {
        assert_eq!(encode_crockford(u128::MAX), "7zzzzzzzzzzzzzzzzzzzzzzzzz");
        for (prefix, parse) in [
            (
                "rm_",
                parse_standard_room_id as fn(&str) -> Result<u128, IdError>,
            ),
            ("ses_", parse_session_id),
            ("msg_", parse_message_id),
        ] {
            let early = generate_id_at(prefix, 1, |b| {
                b.fill(255);
                Ok(())
            })
            .unwrap();
            let late = generate_id_at(prefix, 2, |_| Ok(())).unwrap();
            assert!(early < late);
            assert_eq!(parse(&early).unwrap() >> 80, 1);
            assert_eq!(parse(&late).unwrap(), 2 << 80);
            assert!(parse(&early.to_ascii_uppercase()).is_err());
            assert!(parse(&format!("{prefix}0000000000000000000000000Z")).is_err());
            for invalid in [
                "80000000000000000000000000",
                "0000000000000000000000000o",
                "short",
            ] {
                assert!(parse(&format!("{prefix}{invalid}")).is_err());
            }
        }

        assert_eq!(background_room_id("host123").unwrap(), "rm_bg_host123");
        assert!(validate_room_id("rm_bg_host123").is_ok());
        assert!(validate_room_id("rm_00000000000000000000000000").is_ok());
        for invalid in ["", "Host", "../host", " some", "host_"] {
            assert!(background_room_id(invalid).is_err());
            assert!(validate_background_room_id(&format!("rm_bg_{invalid}")).is_err());
        }
        assert!(validate_background_room_id("RM_BG_host1").is_err());
    }

    #[test]
    fn os_ids_are_unique() {
        let mut all = HashSet::new();
        for _ in 0..256 {
            let room = generate_room_id().unwrap();
            let session = generate_session_id().unwrap();
            let message = generate_message_id().unwrap();
            assert!(parse_standard_room_id(&room).is_ok());
            assert!(parse_session_id(&session).is_ok());
            assert!(parse_message_id(&message).is_ok());
            assert!(all.insert(room) && all.insert(session) && all.insert(message));
        }
    }

    #[test]
    fn os_csprng_fills_beyond_one_chunk_with_distinct_nonzero_bytes() {
        let mut samples = HashSet::new();
        for _ in 0..16 {
            let mut bytes = [0; 512];
            fill_csprng(&mut bytes).unwrap();
            assert!(
                bytes
                    .as_chunks::<256>()
                    .0
                    .iter()
                    .all(|chunk| chunk.iter().any(|&b| b != 0))
            );
            assert!(samples.insert(bytes));
        }
    }

    #[test]
    fn entropy_failure_cannot_issue_an_id() {
        let err = generate_id_at("ses_", 42, |b| {
            assert_eq!(b.len(), 10);
            b[0] = 1; // Even a partially filled buffer is unusable.
            Err(IdError::Random(std::io::Error::other(
                "injected RNG failure",
            )))
        })
        .unwrap_err();
        assert!(matches!(err, IdError::Random(ref e) if e.to_string() == "injected RNG failure"));
    }

    #[test]
    fn base64_is_bounded_and_canonically_padded() {
        let bytes = vec![0x5a; 65_536];
        let encoded = encode_base64(&bytes);
        assert_eq!(encoded.len(), 87_384);
        assert_eq!(decode_base64_bounded(&encoded, 65_536).unwrap(), bytes);
        let one_over = encode_base64(&vec![0; 65_537]);
        assert_eq!(one_over.len(), 87_384);
        assert!(matches!(
            decode_base64_bounded(&one_over, 65_536),
            Err(IdError::Base64TooLarge)
        ));
        assert!(matches!(
            decode_base64_bounded(&"!".repeat(87_385), 65_536),
            Err(IdError::Base64TooLarge)
        ));

        for (plain, encoded) in [
            (b"".as_slice(), ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
        ] {
            assert_eq!(encode_base64(plain), encoded);
            assert_eq!(decode_base64_bounded(encoded, plain.len()).unwrap(), plain);
        }
        for bad in [
            "Zg", "Zg=", "Zg===", "Z===", "Zg=A", "Zh==", "Zm9=", "Zg==\n", "-w==",
        ] {
            assert!(
                matches!(decode_base64_bounded(bad, 16), Err(IdError::InvalidBase64)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn sanitize_display_text_removes_controls_before_counting_chars() {
        let controls: String = ('\u{0}'..='\u{1f}')
            .chain('\u{7f}'..='\u{9f}')
            .chain(['\r', '\n', '\u{061c}', '\u{200e}', '\u{200f}'])
            .chain('\u{2028}'..='\u{202e}')
            .chain('\u{2066}'..='\u{2069}')
            .collect();
        assert_eq!(
            sanitize_display_text(&format!("{controls}A{controls}한🙂{controls}"), 4),
            "A한🙂"
        );
        assert_eq!(
            sanitize_display_text(&format!("é{controls}한🙂x"), 3),
            "é한🙂"
        );
        assert_eq!(sanitize_display_text(&controls, usize::MAX), "");
        assert_eq!(sanitize_display_text("abc", 0), "");
    }
}
