//! Exercise the SSH process boundary without a network, SSHD or Docker daemon.

#![cfg(unix)]

use ddg_agents_proto::terminal::PROTOCOL_VERSION;
use ddg_agents_transport::remote_update::{
    RemoteUpdateOptions, apply_remote_update, rollback_remote_update,
};
use ddg_agents_transport::{HostProfile, HostTransport, install_remote_host};
use ddg_agents_update::config::{AutoCheckPolicy, UpdateConfig};
use ddg_agents_update::{ApplyOutcome, UpdateErrorCode};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

// The SSH stub rejects secrets before running any remote command. It checks every
// step, including both independently authenticated update hello connections.
const SSH: &str = r#"#!/bin/sh
set -eu
[ "${GITHUB_TOKEN+x}${GH_TOKEN+x}" = '' ] || exit 31
strict=no
fresh=no
while [ "$1" != -- ]; do
    case "$1" in
        StrictHostKeyChecking=yes) strict=yes ;;
        StrictHostKeyChecking=*|SendEnv*|SetEnv*) exit 32 ;;
        ControlPath=none) fresh=yes ;;
    esac
    shift
done
[ "$strict" = yes ] || exit 33
case "$2" in
    ddg@fixture) [ "$fresh" = no ] ;;
    fixture) [ "$fresh" = yes ] ;;
    *) exit 34 ;;
esac
export HOME="$DDG_TEST_REMOTE"
printf '%s\n' "$2" >> "$HOME/ssh.log"
exec /bin/sh -c "$3"
"#;

const HOST: &str = r#"#!/bin/sh
set -eu
case "$1" in
connect)
    read -r hello
    version=$(cat "$HOME/version")
    id=$(cat "$HOME/host-id")
    printf '{"jsonrpc":"2.0","id":1,"result":{"protocol_version":%s,"host_id":"%s","boot_generation":1,"conn_id":1,"features":["agent_activity_v1"],"app_version":"%s","rollback_version":"1.0.0"}}\n' "$DDG_TEST_PROTOCOL" "$id" "$version"
    ;;
--version)
    if [ "$(cat "$HOME/post")" = bad-smoke ]; then
        echo 'ddg-agents-host 8.0.0'
    else
        echo 'ddg-agents-host 2.0.0'
    fi
    ;;
__apply-daemon-swap)
    [ "$#" = 9 ] && [ "$2" = --install-dir ] && [ "$3" = "$HOME/.local/bin" ] && [ "$4" = --staging-dir ]
    case "$5" in "$HOME"/.local/state/ddg/update/staging/*) ;; *) exit 1 ;; esac
    [ "$0" = "$5/bin/ddg-agents-host" ] && [ -x "$5/bin/ddg-terminal-server" ]
    [ "$6" = --version ] && [ "$7" = v2.0.0 ] && [ "$8" = --manifest-sha256 ]
    [ "$9" = "$(cat "$HOME/manifest-sha256")" ] && [ -f "$5/manifest.json" ]
    echo apply >> "$HOME/swaps"
    case $(cat "$HOME/post") in
        wrong-id) echo different > "$HOME/host-id"; echo 2.0.0 > "$HOME/version" ;;
        wrong-version) echo 8.0.0 > "$HOME/version" ;;
        ok) echo 2.0.0 > "$HOME/version" ;;
    esac
    echo '{"outcome":"completed","from_version":"1.0.0","to_version":"2.0.0","tokens_preserved":true}'
    ;;
__rollback-daemon-swap)
    [ "$#" = 4 ] && [ "$2" = --install-dir ] && [ "$3" = "$HOME/.local/bin" ] && [ "$4" = --restart-sessions ]
    [ "$0" = "$HOME/.local/bin/ddg-agents-host" ]
    if [ -f "$HOME/reject-rollback" ]; then
        printf 'helper diagnostic\n{"code":"rollback_verification_failed","message":"fixture rollback refused"}\n' >&2
        exit 1
    fi
    echo rollback >> "$HOME/swaps"
    echo 1.0.0 > "$HOME/version"
    echo '{"outcome":"completed","from_version":"2.0.0","to_version":"1.0.0","tokens_preserved":true}'
    ;;
*) exit 1 ;;
esac
"#;

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn executable(path: &Path, bytes: &str) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn ssh_update_and_container_bootstrap() {
    // Re-exec to seed process-local credentials without mutating the shared test environment.
    if let Some(root) = std::env::var_os("DDG_TEST_ROOT") {
        exercise(std::path::Path::new(&root)).await;
        return;
    }
    let mut nonce = [0; 8];
    ddg_agents_proto::id::fill_csprng(&mut nonce).unwrap();
    let root = std::env::temp_dir().join(format!("ddg-update-{:016x}", u64::from_be_bytes(nonce)));
    let remote = root.join("remote home");
    fs::create_dir_all(&remote).unwrap();
    executable(&root.join("ssh"), SSH);
    let arch = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "aarch64"
    } else {
        "x86_64"
    };
    executable(
        &root.join("uname"),
        &format!("#!/bin/sh\ncase $1 in -s) echo Linux;; -m) echo {arch};; *) exit 1;; esac\n"),
    );
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ssh_update_and_container_bootstrap",
            "--nocapture",
        ])
        .env(
            "PATH",
            std::env::join_paths([root.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap(),
        )
        .env("DDG_TEST_ROOT", &root)
        .env("DDG_TEST_REMOTE", &remote)
        .env("DDG_TEST_PROTOCOL", PROTOCOL_VERSION.to_string())
        .env("DDG_TEST_PLATFORM", format!("linux-{arch}"))
        .env("DDG_DIST_DIR", root.join("dist"))
        .env("DDG_CONFIG_DIR", &root)
        .env("GITHUB_TOKEN", "fixture-token-one")
        .env("GH_TOKEN", "fixture-token-two")
        .output()
        .unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(
        child.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

async fn exercise(root: &Path) {
    let platform = std::env::var("DDG_TEST_PLATFORM").unwrap();
    let remote = root.join("remote home");
    let cache = root.join("dist").join(&platform);
    let log = remote.join("ssh.log");
    let stage = root.join("verified release");
    let remote_stages = remote.join(".local/state/ddg/update/staging");
    let terminal = "#!/bin/sh\nexit 0\n";
    let host = "ddg-agents-host";
    let term = "ddg-terminal-server";
    let container = HostProfile {
        transport: HostTransport::ContainerSsh {
            host: "fixture".into(),
            port: 2222,
            user: "ddg".into(),
            known_hosts_file: None,
            identity_file: None,
        },
        ..HostProfile::local_default()
    };
    let docker = HostProfile {
        transport: HostTransport::DockerExec {
            container: "fixture".into(),
            ssh_host: None,
            known_hosts_file: None,
            identity_file: None,
        },
        ..HostProfile::local_default()
    };
    let apply = async |profile: &HostProfile| {
        apply_remote_update(
            profile,
            "v2.0.0",
            &stage,
            "invalid",
            RemoteUpdateOptions::default(),
        )
        .await
    };
    for profile in [&container, &docker] {
        for error in [
            apply(profile).await.unwrap_err(),
            rollback_remote_update(profile, false).await.unwrap_err(),
        ] {
            assert_eq!(
                serde_json::to_value(error.code).unwrap(),
                "container_update_unsupported"
            );
        }
    }
    assert_eq!(
        apply(&HostProfile::local_default()).await.unwrap_err().code,
        UpdateErrorCode::HostSecurityBlocked
    );
    let mut ssh = HostProfile {
        transport: HostTransport::Ssh {
            target: "fixture".into(),
            port: None,
            known_hosts_file: None,
            identity_file: None,
        },
        ..HostProfile::local_default()
    };
    assert_eq!(
        apply(&ssh).await.unwrap_err().code,
        UpdateErrorCode::HostSecurityBlocked
    );
    assert_eq!(
        rollback_remote_update(&ssh, false).await.unwrap_err().code,
        UpdateErrorCode::HostSecurityBlocked
    );
    assert!(!log.exists());
    let invalid_tag = apply_remote_update(
        &ssh,
        "2.0.0; touch marker",
        &stage,
        "invalid",
        RemoteUpdateOptions::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(invalid_tag.code, UpdateErrorCode::ManifestInvalid);
    assert!(!log.exists());

    fs::write(
        root.join("update.json"),
        serde_json::to_vec(&UpdateConfig {
            auto_check: AutoCheckPolicy::Off,
            ..UpdateConfig::default()
        })
        .unwrap(),
    )
    .unwrap();
    fs::create_dir_all(&cache).unwrap();
    fs::set_permissions(root.join("dist"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(cache.join(host), HOST).unwrap();
    fs::write(cache.join(term), terminal).unwrap();
    fs::write(cache.join(format!("{host}.sha256")), "0".repeat(64)).unwrap();
    fs::write(
        cache.join(format!("{term}.sha256")),
        sha(terminal.as_bytes()),
    )
    .unwrap();
    fs::write(remote.join("host-id"), "pinned\n").unwrap();
    fs::write(remote.join("version"), "1.0.0\n").unwrap();
    assert!(
        install_remote_host(&container, None)
            .await
            .unwrap_err()
            .message
            .contains("SHA-256 does not match")
    );
    assert!(!remote.join(".local/bin").exists());
    fs::write(cache.join(format!("{host}.sha256")), sha(HOST.as_bytes())).unwrap();
    let installed = install_remote_host(&container, None).await.unwrap();
    assert_eq!(installed.host_id, "pinned");
    for dir in [root.join("dist"), cache] {
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        fs::read_to_string(remote.join(".local/bin").join(host)).unwrap(),
        HOST
    );
    assert_eq!(
        fs::read_to_string(remote.join(".local/bin").join(term)).unwrap(),
        terminal
    );

    fs::create_dir_all(stage.join("bin")).unwrap();
    fs::write(stage.join("bin").join(host), HOST).unwrap();
    fs::write(stage.join("bin").join(term), terminal).unwrap();
    let manifest = serde_json::to_vec(&json!({
        "schema_version":1, "version":"2.0.0", "tag":"v2.0.0", "channel":"stable", "published_at":"2026-01-01T00:00:00Z",
        "protocol_version":PROTOCOL_VERSION,"holder_protocol_version":1,"holder_restart_required":false,"min_compatible_schema":1,"release_notes_summary":"",
        "artifacts":[{"platform":platform,"name":"fixture.tar.gz","sha256":"0".repeat(64),"binaries":{
            (host):{"path":format!("bin/{host}"), "sha256":sha(HOST.as_bytes())},
            (term):{"path":format!("bin/{term}"), "sha256":sha(terminal.as_bytes())}
        }}]
    })).unwrap();
    let digest = sha(&manifest);
    fs::write(stage.join("manifest.json"), &manifest).unwrap();
    fs::write(remote.join("manifest-sha256"), &digest).unwrap();
    let apply = async |profile: &HostProfile| {
        apply_remote_update(
            profile,
            "v2.0.0",
            &stage,
            &digest,
            RemoteUpdateOptions::default(),
        )
        .await
    };
    ssh.expected_host_id = Some("wrong".into());
    assert_eq!(
        apply(&ssh).await.unwrap_err().code,
        UpdateErrorCode::HostSecurityBlocked
    );
    assert!(!remote_stages.exists());
    assert!(!remote.join("swaps").exists());
    ssh.expected_host_id = Some("pinned".into());
    for post in ["bad-smoke", "wrong-id", "wrong-version", "ok"] {
        fs::write(remote.join("host-id"), "pinned\n").unwrap();
        fs::write(remote.join("version"), "1.0.0\n").unwrap();
        fs::write(remote.join("post"), post).unwrap();
        let result = apply(&ssh).await;
        if post == "ok" {
            assert!(
                matches!(result.unwrap(), ApplyOutcome::Completed { to_version, .. } if to_version == "2.0.0")
            );
        } else {
            assert_eq!(
                result.unwrap_err().code,
                UpdateErrorCode::HostSecurityBlocked
            );
        }
        assert_eq!(fs::read_dir(&remote_stages).unwrap().count(), 0);
    }
    fs::write(remote.join("reject-rollback"), "").unwrap();
    let error = rollback_remote_update(&ssh, true).await.unwrap_err();
    assert_eq!(error.code, UpdateErrorCode::RollbackVerificationFailed);
    assert_eq!(error.message(), "fixture rollback refused");
    fs::remove_file(remote.join("reject-rollback")).unwrap();
    assert!(
        matches!(rollback_remote_update(&ssh, true).await.unwrap(), ApplyOutcome::Completed { to_version, .. } if to_version == "1.0.0")
    );
    assert_eq!(
        fs::read_to_string(remote.join("swaps")).unwrap(),
        "apply\napply\napply\nrollback\n"
    );
}
