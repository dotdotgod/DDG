use crate::{
    UpdateErrorCode as Code,
    macos::{
        EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID, codesign_requirement, macos_preflight_check,
        verify_macho_arch, verify_macos_app_bundle,
    },
    rollback::{RollbackRecord, restore_rollback_binary, verify_rollback_set},
};
#[cfg(unix)]
use crate::{
    macos::{SwapJob, validate_helper_swap_job},
    swap::{copy_or_move_preserving_backup, gc_stale_tombstones, make_tombstone_name},
};
use std::{collections::BTreeMap, fs};

const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[test]
fn macho_thin_and_universal_architectures() {
    const INTEL: (&str, u32) = ("darwin-x86_64", 0x0100_0007);
    const ARM: (&str, u32) = ("darwin-arm64", 0x0100_000c);
    fn thin(cpu: u32, big_endian: bool) -> [u8; 32] {
        let bytes = |value: u32| {
            if big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        };
        let mut header = [0; 32];
        header[..4].copy_from_slice(&bytes(0xfeed_facf));
        header[4..8].copy_from_slice(&bytes(cpu));
        header
    }
    fn fat(cpus: &[u32], fat64: bool) -> Vec<u8> {
        let magic = if fat64 {
            0xcafe_babfu32
        } else {
            0xcafe_babeu32
        };
        let entry_len = if fat64 { 32 } else { 20 };
        let mut bytes = Vec::from(magic.to_be_bytes());
        bytes.extend((cpus.len() as u32).to_be_bytes());
        for (index, &cpu) in cpus.iter().enumerate() {
            let offset = bytes.len() + entry_len * (cpus.len() - index) + 32 * index;
            bytes.extend(cpu.to_be_bytes());
            bytes.extend([0; 4]); // CPU subtype
            if fat64 {
                bytes.extend((offset as u64).to_be_bytes());
                bytes.extend(32u64.to_be_bytes());
                bytes.extend([0; 8]);
            } else {
                bytes.extend((offset as u32).to_be_bytes());
                bytes.extend(32u32.to_be_bytes());
                bytes.extend([0; 4]);
            }
        }
        for &cpu in cpus {
            bytes.extend(thin(cpu, false));
        }
        bytes
    }

    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("daemon");
    let check = |bytes: &[u8], intel: bool, arm: bool| {
        fs::write(&bin, bytes).unwrap();
        for (platform, passes) in [(INTEL.0, intel), (ARM.0, arm)] {
            let result = verify_macho_arch(&bin, platform);
            if passes {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().code, Code::ArchiveUnsafe);
            }
        }
    };
    for big_endian in [false, true] {
        check(&thin(INTEL.1, big_endian), true, false);
        check(&thin(ARM.1, big_endian), false, true);
    }
    for fat64 in [false, true] {
        let both = fat(&[INTEL.1, ARM.1], fat64);
        check(&both, true, true);
        check(&fat(&[ARM.1], fat64), false, true);
        check(&both[..both.len() - 1], false, false);
        let mut mismatched = fat(&[INTEL.1], fat64);
        let slice = mismatched.len() - 32;
        mismatched[slice..].copy_from_slice(&thin(ARM.1, false));
        check(&mismatched, false, false);
    }
    let mut elf = [0; 32];
    elf[..4].copy_from_slice(b"\x7fELF");
    for bad in [
        elf.as_slice(),
        b"#! /bin/sh\necho not an executable\n",
        &thin(INTEL.1, false)[..8],
        b"\xca\xfe\xba\xbe\x00\x00\x00\x11",
    ] {
        check(bad, false, false);
    }
    assert_eq!(
        verify_macho_arch(&bin, "linux-x86_64").unwrap_err().code,
        Code::AssetNotFound
    );
}

#[test]
fn macos_and_rollback_preflight_and_pinned_requirement() {
    assert_eq!(
        macos_preflight_check("/private/AppTranslocation/random/DDG.app".as_ref())
            .unwrap_err()
            .code,
        Code::AppTranslocated
    );
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("DDG.app");
    assert!(macos_preflight_check(&app).is_ok());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let error = macos_preflight_check(&app).unwrap_err();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(error.code, Code::InstallDirNotWritable);
    }
    assert_eq!(
        codesign_requirement("ABCDE12345", "com.acme.ddg"),
        r#"anchor apple generic and identifier "com.acme.ddg" and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = "ABCDE12345""#
    );
    assert_eq!(EXPECTED_APPLE_TEAM_ID, "PLACEHOLDER_TEAM_ID");
    assert_eq!(EXPECTED_BUNDLE_ID, "com.dotdotgod.agents");
    assert_eq!(
        verify_macos_app_bundle(&app, "ATTACKTEAM", EXPECTED_BUNDLE_ID)
            .unwrap_err()
            .code,
        Code::CodesignInvalid
    );
}

#[test]
fn macos_and_rollback_custom_source_cannot_override_compiled_identity() {
    let child = "DDG_TEST_APP_ID_OVERRIDE";
    if std::env::var_os(child).is_some() {
        let (repo, _) = crate::config::UpdateConfig::default()
            .effective_repo()
            .unwrap();
        assert_eq!(repo.as_str(), "custom/repo");
        assert_eq!(EXPECTED_APPLE_TEAM_ID, "PLACEHOLDER_TEAM_ID");
        let team = std::env::var("DDG_EXPECTED_APPLE_TEAM_ID").unwrap();
        let err = verify_macos_app_bundle("Unsigned.app".as_ref(), &team, EXPECTED_BUNDLE_ID)
            .unwrap_err();
        assert_eq!(err.code, Code::CodesignInvalid);
        assert!(err.message().contains("compiled release identity"));
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests_macos::macos_and_rollback_custom_source_cannot_override_compiled_identity",
        ])
        .env(child, "1")
        .env("DDG_UPDATE_REPO", "custom/repo")
        .env("EXPECTED_APPLE_TEAM_ID", "ATTACKTEAM")
        .env("DDG_EXPECTED_APPLE_TEAM_ID", "ATTACKTEAM")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[cfg(unix)]
#[test]
fn macos_and_rollback_helper_consumes_job_and_rejects_paths() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let staging = state.join("staging");
    fs::create_dir(&state).unwrap();
    fs::create_dir(&staging).unwrap();
    for path in [&state, &staging] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let target = dir.path().join("DDG.app");
    let staged = staging.join("DDG.app");
    fs::create_dir(&target).unwrap();
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("ddg"), b"abc").unwrap();
    let archive = staging.join("release.zip");
    fs::write(&archive, b"abc").unwrap();
    let job_path = state.join("job.json");
    let job = SwapJob {
        staging_app_path: staged.clone(),
        target_app_path: target.clone(),
        archive_path: archive,
        archive_sha256: ABC.into(),
        binary_sha256s: BTreeMap::from([("ddg".into(), ABC.into())]),
    };
    let submit = |path: &std::path::Path, value: &SwapJob| {
        ddg_agents_os::fs::write_atomic(path, &serde_json::to_vec(value).unwrap()).unwrap();
    };
    let validate = |path: &std::path::Path| {
        validate_helper_swap_job(path, &state, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)
    };

    let outside = dir.path().join("outside.json");
    submit(&outside, &job);
    assert!(validate(&outside).is_err());
    assert!(outside.exists());
    symlink(&outside, &job_path).unwrap();
    assert!(validate(&job_path).is_err());
    assert!(outside.exists());
    fs::remove_file(&job_path).unwrap();

    ddg_agents_os::fs::write_atomic(&job_path, b"invalid JSON").unwrap();
    assert!(validate(&job_path).is_err());
    assert!(!job_path.exists());

    let mut invalid = job.clone();
    invalid.staging_app_path = target.clone();
    submit(&job_path, &invalid);
    assert!(validate(&job_path).is_err());
    assert!(!job_path.exists());

    fs::rename(&staged, staging.join("saved.app")).unwrap();
    symlink(&target, &staged).unwrap();
    submit(&job_path, &job);
    assert!(validate(&job_path).is_err());
    assert!(!job_path.exists());
    fs::remove_file(&staged).unwrap();
    fs::rename(staging.join("saved.app"), &staged).unwrap();

    fs::rename(&target, dir.path().join("old.app")).unwrap();
    symlink(&staged, &target).unwrap();
    submit(&job_path, &job);
    assert!(validate(&job_path).is_err());
    assert!(!job_path.exists());
    fs::remove_file(&target).unwrap();
    fs::rename(dir.path().join("old.app"), &target).unwrap();

    fs::write(staged.join("ddg"), b"tampered").unwrap();
    submit(&job_path, &job);
    assert_eq!(
        validate(&job_path).unwrap_err().code,
        Code::ChecksumMismatch
    );
    assert!(!job_path.exists());
}

#[test]
fn macos_and_rollback_verifies_all_files_before_restore() {
    let dir = tempfile::tempdir().unwrap();
    let prev = dir.path().join("prev");
    fs::create_dir_all(prev.join("bin")).unwrap();
    fs::write(prev.join("archive.zip"), b"abc").unwrap();
    for bin in ["ddg", "host"] {
        fs::write(prev.join("bin").join(bin), b"abc").unwrap();
    }
    let record = RollbackRecord {
        version: "1.0.0".into(),
        platform: "macos-arm64".into(),
        recorded_at: 1,
        archive_rel_path: "archive.zip".into(),
        archive_sha256: ABC.into(),
        binary_sha256s: BTreeMap::from([
            ("bin/ddg".into(), ABC.into()),
            ("bin/host".into(), ABC.into()),
        ]),
        catalog_schema_version: 1,
        hosts_schema_version: 1,
        holder_protocol_version: 1,
    };
    verify_rollback_set(&prev, &record).unwrap();
    let destination = dir.path().join("ddg");
    fs::write(&destination, b"new").unwrap();
    fs::write(prev.join("bin/host"), b"tampered").unwrap();
    assert_eq!(
        restore_rollback_binary(&prev, &record, "bin/ddg", &destination)
            .unwrap_err()
            .code,
        Code::RollbackVerificationFailed
    );
    assert_eq!(fs::read(&destination).unwrap(), b"new");
    fs::write(prev.join("bin/host"), b"abc").unwrap();
    restore_rollback_binary(&prev, &record, "bin/ddg", &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"abc");
    assert!(prev.join("bin/ddg").exists());
    fs::write(prev.join("archive.zip"), b"tampered").unwrap();
    assert_eq!(
        verify_rollback_set(&prev, &record).unwrap_err().code,
        Code::RollbackVerificationFailed
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_and_rollback_rejects_unsigned_bundle_even_with_matching_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let prev = dir.path().join("prev");
    fs::create_dir_all(prev.join("Unsigned.app/Contents/MacOS")).unwrap();
    fs::write(prev.join("archive.zip"), b"abc").unwrap();
    fs::write(prev.join("Unsigned.app/Contents/MacOS/ddg"), b"abc").unwrap();
    let record = RollbackRecord {
        version: "1.0.0".into(),
        platform: "macos-arm64".into(),
        archive_rel_path: "archive.zip".into(),
        archive_sha256: ABC.into(),
        binary_sha256s: BTreeMap::from([("Unsigned.app/Contents/MacOS/ddg".into(), ABC.into())]),
        ..RollbackRecord::default()
    };
    let error = verify_rollback_set(&prev, &record).unwrap_err();
    assert_eq!(error.code, Code::RollbackVerificationFailed);
    assert!(error.message().contains("Developer ID"));
}

#[cfg(unix)]
#[test]
fn macos_and_rollback_preserves_backup_and_skips_locked_tombstones() {
    use ddg_agents_os::lock::FileLock;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("bundle.app");
    let backup = dir.path().join("backup.app");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("bin"), b"abc").unwrap();
    copy_or_move_preserving_backup(&source, &backup).unwrap();
    assert_eq!(fs::read(backup.join("bin")).unwrap(), b"abc");
    assert!(!source.exists());
    let bin = dir.path().join("ddg.exe");
    let first = make_tombstone_name(&bin, "nonce1").unwrap();
    let second = make_tombstone_name(&bin, "nonce2").unwrap();
    let unrelated = dir.path().join("keep.old.");
    for file in [&first, &second, &unrelated] {
        fs::write(file, b"old").unwrap();
    }
    let lock = FileLock::try_lock_exclusive(&first).unwrap();
    assert_eq!(gc_stale_tombstones(dir.path()).unwrap(), 1);
    assert!(first.exists() && unrelated.exists() && !second.exists());
    drop(lock);
    // A parallel fork may briefly inherit the open flock description until exec.
    for _ in 0..20 {
        if gc_stale_tombstones(dir.path()).unwrap() == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!first.exists());
    assert!(unrelated.exists());
}
