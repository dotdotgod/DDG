use crate::{
    UpdateError, UpdateErrorCode as Code,
    cache::{gc_dist_cache, verify_cached_artifact},
    manifest::ReleaseManifest,
    rollback::RollbackRecord,
    state::{FailedVersionRecord, InProgressSwap, PendingHolderRestart, UpdateStateStore},
    version::ReleaseTag,
};
use ddg_agents_os::lock::FileLock;
use std::fs;

const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[cfg(feature = "network")]
#[tokio::test]
async fn state_and_cache_manager_off_busy_and_force_policy() {
    use crate::{
        config::{AutoCheckPolicy, UpdateConfig},
        manager::{CheckOutcome, LocalUpdateOptions, UpdateManager, UpdatePaths},
    };
    let dir = tempfile::tempdir().unwrap();
    let paths = UpdatePaths {
        config_dir: dir.path().join("config"),
        state_dir: dir.path().join("state"),
        dist_dir: dir.path().join("dist"),
    };
    let manager = UpdateManager::new(paths.clone());
    let mut config = UpdateConfig {
        auto_check: AutoCheckPolicy::Off,
        ..UpdateConfig::default()
    };
    manager.save_config(&config).unwrap();
    assert_eq!(
        manager.check_for_updates(true).await.unwrap(),
        Some(CheckOutcome::Disabled)
    );
    assert!(!paths.state_dir.exists());

    config.auto_check = AutoCheckPolicy::On;
    manager.save_config(&config).unwrap();
    fs::create_dir(&paths.state_dir).unwrap();
    let lock = FileLock::try_lock_exclusive(&paths.state_dir.join("update.lock")).unwrap();
    assert_eq!(manager.check_for_updates(true).await.unwrap(), None);
    assert_eq!(
        manager.check_for_updates(false).await.unwrap_err().code,
        Code::LockBusy
    );
    drop(lock);

    let target = ReleaseTag::parse("v0.0.1").unwrap();
    let mut options = LocalUpdateOptions {
        platform: "macos-arm64".into(),
        force: true,
        allow_downgrade: true,
        restart_sessions: true,
        active_pty_count: 2,
        holder_protocol_version: 1,
        data_schema_version: 1,
        schema_compatible: false,
        container: true,
    };
    assert_eq!(
        manager
            .plan_local_update(&target, &options)
            .await
            .unwrap_err()
            .code,
        Code::ContainerUpdateUnsupported
    );
    options.container = false;
    let manager = UpdateManager::new(UpdatePaths {
        state_dir: dir.path().join("unlocked-state"),
        ..paths
    });
    assert_eq!(
        manager
            .plan_local_update(&target, &options)
            .await
            .unwrap_err()
            .code,
        Code::SchemaIncompatibleDowngrade
    );
}

// Forking test children can retain an open flock until exec.
fn retry_busy<T>(mut op: impl FnMut() -> Result<T, UpdateError>) -> Result<T, UpdateError> {
    for _ in 0..20 {
        match op() {
            Err(err) if err.code == Code::LockBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            result => return result,
        }
    }
    op()
}

#[test]
fn state_and_cache_locked_atomic_mutation_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let store = UpdateStateStore::new(dir.path());
    let state_path = dir.path().join("update-state.json");
    let lock = FileLock::try_lock_exclusive(&dir.path().join("update.lock")).unwrap();
    assert_eq!(store.update(|_| Ok(())).unwrap_err().code, Code::LockBusy);
    assert_eq!(
        store
            .try_update_background(|_| -> Result<(), UpdateError> {
                panic!("lock contention must skip the callback")
            })
            .unwrap(),
        None
    );
    assert!(!state_path.exists());
    drop(lock);

    retry_busy(|| {
        store.update(|state| {
            state.etag = Some("existing".into());
            Ok(())
        })
    })
    .unwrap();
    let before = fs::read(&state_path).unwrap();
    assert_eq!(
        retry_busy(|| store.update(|state| {
            state.etag = None;
            Err::<(), _>(UpdateError::new(Code::NetworkError, "failed"))
        }))
        .unwrap_err()
        .code,
        Code::NetworkError
    );
    assert_eq!(fs::read(&state_path).unwrap(), before);
    assert_eq!(
        retry_busy(|| store.load()).unwrap().etag.as_deref(),
        Some("existing")
    );

    fs::write(&state_path, b"broken state").unwrap();
    let recovered = retry_busy(|| store.load()).unwrap();
    assert_eq!(recovered.schema_version, 1);
    assert_eq!(recovered.etag, None);
    assert_eq!(retry_busy(|| store.load()).unwrap(), recovered);
    let backups: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(Result::unwrap)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("update-state.json.corrupt.")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = &backups[0];
    assert!(backup.file_name().to_string_lossy().ends_with(".bak"));
    assert_eq!(fs::read(backup.path()).unwrap(), b"broken state");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [state_path, backup.path()] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn state_and_cache_retries_clear_only_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let store = UpdateStateStore::new(dir.path());
    store
        .update(|state| {
            state.failed_versions.insert(
                "2.0.0".into(),
                FailedVersionRecord {
                    failed_at: 1,
                    reason: "smoke test".into(),
                },
            );
            Ok(())
        })
        .unwrap();
    let state = retry_busy(|| store.load()).unwrap();
    assert_eq!(
        state.should_auto_attempt("2.0.0").unwrap_err().code,
        Code::FailedVersionBlocked
    );
    assert!(state.should_attempt("v2.0.0", false).is_err());
    assert!(state.should_attempt("2.0.0", true).is_ok());
    assert!(state.failed_versions.contains_key("2.0.0"));
    retry_busy(|| {
        store.update(|state| {
            state.record_success("v2.0.0");
            Ok(())
        })
    })
    .unwrap();
    let state = retry_busy(|| store.load()).unwrap();
    assert!(state.should_auto_attempt("2.0.0").is_ok());
    assert_eq!(state.current_version, "2.0.0");
    assert_eq!(
        state.previous_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn state_and_cache_gc_keeps_top_two_per_platform_and_pins() {
    let dir = tempfile::tempdir().unwrap();
    let make = |version: &str, platform: &str| {
        fs::create_dir_all(dir.path().join(version).join(platform)).unwrap()
    };
    let mut state = crate::state::UpdateState {
        current_version: "1.0.0".into(),
        rollback: Some(RollbackRecord {
            version: "1.1.0".into(),
            ..RollbackRecord::default()
        }),
        in_progress: Some(InProgressSwap {
            from_version: "1.0.0".into(),
            to_version: "1.2.0".into(),
        }),
        pending_holder_restart: Some(PendingHolderRestart {
            to_version: "1.3.0".into(),
            active_pty_count: 2,
        }),
        ..Default::default()
    };
    for version in [
        "1.0.0",
        "1.1.0",
        "1.2.0",
        "1.3.0",
        "1.4.0",
        "1.9.0",
        "1.10.0-rc.1",
        "1.10.0",
    ] {
        make(version, "linux-x86_64");
    }
    for version in ["1.4.0", "1.9.0", "1.10.0"] {
        make(version, "macos-arm64");
    }
    fs::create_dir(dir.path().join("linux-x86_64")).unwrap();
    gc_dist_cache(dir.path(), &state.cache_pins()).unwrap();
    for version in ["1.0.0", "1.1.0", "1.2.0", "1.3.0", "1.10.0-rc.1", "1.10.0"] {
        assert!(dir.path().join(version).join("linux-x86_64").exists());
    }
    assert!(!dir.path().join("1.9.0/linux-x86_64").exists());
    assert!(dir.path().join("1.9.0/macos-arm64").exists());
    assert!(!dir.path().join("1.4.0").exists());
    assert!(dir.path().join("linux-x86_64").exists());
    state.pending_holder_restart = None;
    gc_dist_cache(dir.path(), &state.cache_pins()).unwrap();
    assert!(!dir.path().join("1.3.0").exists());
}

#[test]
fn state_and_cache_manifest_binary_fields_and_rehash() {
    let dir = tempfile::tempdir().unwrap();
    let entry = dir.path().join("1.1.0/linux-x86_64");
    fs::create_dir_all(entry.join("bin")).unwrap();
    let raw = serde_json::json!({
        "schema_version": 1, "version": "1.1.0", "tag": "v1.1.0", "channel": "stable",
        "published_at": "2026-01-01T12:00:00Z", "protocol_version": 1,
        "holder_protocol_version": 1, "holder_restart_required": false, "min_compatible_schema": 1,
        "release_notes_summary": "",
        "artifacts": [{"platform": "linux-x86_64", "name": "linux.tar.gz", "sha256": ABC,
            "size": 3, "binaries": {"ddg": {"path": "bin/ddg", "sha256": ABC}}}]
    });
    let tag = ReleaseTag::parse("v1.1.0").unwrap();
    let manifest = ReleaseManifest::parse(&serde_json::to_vec(&raw).unwrap(), &tag).unwrap();
    assert_eq!(manifest.artifacts[0].size, Some(3));
    assert_eq!(manifest.artifacts[0].binaries["ddg"].path, "bin/ddg");

    for path in ["../outside", "/outside", "bin/ddg:stream"] {
        let mut bad = raw.clone();
        bad["artifacts"][0]["binaries"]["ddg"]["path"] = path.into();
        assert_eq!(
            ReleaseManifest::parse(&serde_json::to_vec(&bad).unwrap(), &tag)
                .unwrap_err()
                .code,
            Code::ManifestInvalid
        );
    }
    let mut bad = raw.clone();
    bad["artifacts"][0]["binaries"]["ddg"]["sha256"] = "incorrect".into();
    assert!(ReleaseManifest::parse(&serde_json::to_vec(&bad).unwrap(), &tag).is_err());

    fs::write(
        entry.join("manifest.json"),
        serde_json::to_vec(&raw).unwrap(),
    )
    .unwrap();
    fs::write(entry.join("linux.tar.gz"), b"abc").unwrap();
    fs::write(entry.join("bin/ddg"), b"abc").unwrap();
    assert_eq!(
        verify_cached_artifact(dir.path(), &manifest, "linux-x86_64").unwrap(),
        entry.join("linux.tar.gz")
    );
    fs::write(entry.join("bin/ddg"), b"tampered").unwrap();
    assert_eq!(
        verify_cached_artifact(dir.path(), &manifest, "linux-x86_64")
            .unwrap_err()
            .code,
        Code::ChecksumMismatch
    );
    assert!(!entry.exists());
}
