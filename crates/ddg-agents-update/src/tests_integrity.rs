use crate::{
    UpdateErrorCode as Code,
    integrity::{
        RELEASE_PUBKEYS, verify_manifest, verify_manifest_signature_with_keys, verify_sha256,
        verify_temp_sha256,
    },
    version::ReleaseTag,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use std::{
    fs,
    io::{self, Read},
};

const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const V1: &[u8] = br#"{
    "schema_version": 1,
    "version": "1.0.0",
    "tag": "v1.0.0",
    "channel": "stable",
    "published_at": "2026-01-01T12:00:00Z",
    "protocol_version": 2,
    "holder_protocol_version": 2,
    "holder_restart_required": false,
    "min_compatible_schema": 1,
    "release_notes_summary": "Release",
    "artifacts": [{
        "platform": "macos-arm64", "name": "DDG-Agents.app.zip",
        "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    }]
}"#;

#[test]
fn integrity_streaming_hash_and_staging_cleanup() {
    struct ShortReads<'a>(&'a [u8]);
    impl Read for ShortReads<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let len = out.len().min(1);
            self.0.read(&mut out[..len])
        }
    }
    assert!(verify_sha256(ShortReads(b"abc"), ABC).is_ok());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("artifact.part");
    fs::write(&path, b"abc").unwrap();
    verify_temp_sha256(&path, ABC).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"abc");
    fs::write(&path, b"changed").unwrap();
    assert_eq!(
        verify_temp_sha256(&path, ABC).unwrap_err().code,
        Code::ChecksumMismatch
    );
    assert_eq!(dir.path().read_dir().unwrap().count(), 0);
    fs::write(&path, b"abc").unwrap();
    assert_eq!(
        verify_temp_sha256(&path, "garbage").unwrap_err().code,
        Code::ManifestInvalid
    );
    assert_eq!(dir.path().read_dir().unwrap().count(), 0);
}

#[test]
fn integrity_detached_signature_is_over_exact_manifest_bytes() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let keys = [("test", key.verifying_key().to_bytes())];
    let signature = |bytes: &[u8], algorithm: &str, key_id: &str| {
        serde_json::to_vec(&serde_json::json!({
            "algorithm": algorithm,
            "key_id": key_id,
            "value": STANDARD.encode(key.sign(bytes).to_bytes())
        }))
        .unwrap()
    };
    let valid = signature(V1, "ed25519", "test");
    let v2 = String::from_utf8_lossy(V1).replace("1.0.0", "2.0.0");
    let changed = [b" " as &[u8], V1].concat();

    assert!(verify_manifest_signature_with_keys(V1, Some(&valid), &keys).is_ok());
    assert_eq!(
        verify_manifest_signature_with_keys(V1, None, &keys)
            .unwrap_err()
            .code,
        Code::SignatureMissing
    );
    for bad in [
        b"not JSON".to_vec(),
        signature(V1, "unknown", "test"),
        signature(V1, "ed25519", "wrong-key"),
        signature(v2.as_bytes(), "ed25519", "test"),
        signature(&changed, "ed25519", "test"),
    ] {
        assert_eq!(
            verify_manifest_signature_with_keys(V1, Some(&bad), &keys)
                .unwrap_err()
                .code,
            Code::SignatureInvalid
        );
    }
    assert!(verify_manifest_signature_with_keys(V1, None, &[]).is_ok());
    let tag = ReleaseTag::parse("v1.0.0").unwrap();
    let manifest = verify_manifest(V1, None, &tag).unwrap();
    assert_eq!(manifest.artifacts[0].sha256, ABC);
    assert_eq!(
        verify_manifest(v2.as_bytes(), None, &tag).unwrap_err().code,
        Code::ManifestInvalid
    );
}

#[test]
fn integrity_release_keys_ignore_runtime_environment() {
    let child = "DDG_UPDATE_TEST_KEY_ENV";
    if std::env::var_os(child).is_some() {
        assert!(RELEASE_PUBKEYS.is_empty());
        assert!(verify_manifest(V1, None, &ReleaseTag::parse("v1.0.0").unwrap()).is_ok());
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests_integrity::integrity_release_keys_ignore_runtime_environment",
        ])
        .env(child, "1")
        .env("RELEASE_PUBKEYS", r#"{"test":[7]}"#)
        .env("DDG_RELEASE_PUBKEYS", r#"{"test":[7]}"#)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}
