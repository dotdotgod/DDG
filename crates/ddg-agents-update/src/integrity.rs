use crate::{UpdateError, UpdateErrorCode as Code};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::Path,
};

pub const RELEASE_PUBKEYS: &[(&str, [u8; 32])] = &[];

pub(crate) fn decode_sha256(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

pub fn verify_sha256(mut input: impl Read, expected_hex: &str) -> Result<(), UpdateError> {
    let expected = decode_sha256(expected_hex)
        .ok_or_else(|| UpdateError::new(Code::ManifestInvalid, "Invalid SHA-256 digest"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(UpdateError::new(Code::ChecksumMismatch, e.to_string())),
        }
    }
    verify_digest(hasher.finalize().as_ref(), expected)
}

pub(crate) fn verify_digest(actual: &[u8; 32], expected: [u8; 32]) -> Result<(), UpdateError> {
    // Compare every byte; the barrier prevents folding this into early-exit equality.
    let difference = actual
        .iter()
        .zip(expected)
        .fold(0, |acc, (a, b)| acc | std::hint::black_box(a ^ b));
    if difference != 0 {
        return Err(UpdateError::new(
            Code::ChecksumMismatch,
            "SHA-256 does not match the manifest",
        ));
    }
    Ok(())
}

/// Verify a caller-owned staging file; delete it immediately on any failure.
/// Use verify_sha256 for a live or rollback artifact that must not be deleted.
pub fn verify_temp_sha256(path: &Path, expected_hex: &str) -> Result<(), UpdateError> {
    let result = fs::File::open(path)
        .map_err(|e| UpdateError::new(Code::ChecksumMismatch, e.to_string()))
        .and_then(|file| verify_sha256(file, expected_hex));
    if let Err(error) = result {
        if let Err(cleanup) = fs::remove_file(path)
            && cleanup.kind() != io::ErrorKind::NotFound
        {
            return Err(UpdateError::new(
                error.code,
                format!("{error}; could not remove staging file: {cleanup}"),
            ));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(feature = "network")]
pub fn verify_manifest(
    raw_manifest: &[u8],
    detached_signature: Option<&[u8]>,
    expected_tag: &crate::version::ReleaseTag,
) -> Result<crate::manifest::ReleaseManifest, UpdateError> {
    verify_manifest_signature_with_keys(raw_manifest, detached_signature, RELEASE_PUBKEYS)?;
    crate::manifest::ReleaseManifest::parse(raw_manifest, expected_tag)
}

#[cfg(feature = "network")]
pub fn verify_manifest_signature_with_keys(
    raw_manifest: &[u8],
    detached_signature: Option<&[u8]>,
    keys: &[(&str, [u8; 32])],
) -> Result<(), UpdateError> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ed25519_dalek::{Signature, VerifyingKey};

    #[derive(serde::Deserialize)]
    struct Detached<'a> {
        algorithm: &'a str,
        key_id: &'a str,
        value: &'a str,
    }

    if keys.is_empty() {
        return Ok(());
    }
    let detached = detached_signature
        .ok_or_else(|| UpdateError::new(Code::SignatureMissing, "Missing manifest.json.sig"))?;
    let invalid = || {
        UpdateError::new(
            Code::SignatureInvalid,
            "Invalid Ed25519 release manifest signature",
        )
    };
    let detached: Detached<'_> = serde_json::from_slice(detached).map_err(|_| invalid())?;
    if detached.algorithm != "ed25519" {
        return Err(invalid());
    }
    let (_, key) = keys
        .iter()
        .find(|(id, _)| *id == detached.key_id)
        .ok_or_else(invalid)?;
    let key = VerifyingKey::from_bytes(key).map_err(|_| invalid())?;
    let mut signature = [0; 64];
    let len = STANDARD
        .decode_slice(detached.value, &mut signature)
        .map_err(|_| invalid())?;
    if len != signature.len() {
        return Err(invalid());
    }
    key.verify_strict(raw_manifest, &Signature::from_bytes(&signature))
        .map_err(|_| invalid())
}
