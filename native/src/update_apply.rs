use crate::update_controller::{Change, VersionInfo, storage};
use ddg_agents_transport::{HostProfile, HostTransport, HostsConfig};
use ddg_agents_update::{ApplyOutcome, UpdateError, UpdateErrorCode as Code, manager::UpdatePaths};
use std::{path::PathBuf, process::Stdio};

fn sibling_cli() -> Result<PathBuf, UpdateError> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(storage)?;
    let cli = exe.with_file_name(if cfg!(windows) { "ddg.exe" } else { "ddg" });
    if !cli.is_file() {
        return Err(UpdateError::new(
            Code::AssetNotFound,
            "Could not find the ddg executable installed with the app.",
        ));
    }
    Ok(cli)
}

pub async fn apply(
    paths: UpdatePaths,
    remote: Option<HostProfile>,
    version: VersionInfo,
    change: Change,
    restart: bool,
    downgrade: bool,
) -> Result<ApplyOutcome, UpdateError> {
    match change {
        Change::LocalApply { tag, repo } => {
            #[cfg(target_os = "macos")]
            return macos::handoff(paths, version, tag, repo, restart, downgrade).await;
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (paths, version, tag, repo, downgrade);
                Err(UpdateError::new(
                    Code::InstallDirNotWritable,
                    "Use the installer to update the desktop app on this platform.",
                ))
            }
        }
        Change::ApplyRemote | Change::Rollback => {
            if let Some(profile) = &remote {
                if !matches!(profile.transport, HostTransport::Ssh { .. }) {
                    return Err(UpdateError::new(
                        Code::ContainerUpdateUnsupported,
                        "Update the container image, then reconnect.",
                    ));
                }
                if profile.expected_host_id.is_none() {
                    return Err(UpdateError::new(
                        Code::HostSecurityBlocked,
                        "Pin the remote Host ID first.",
                    ));
                }
            }
            let mut command = tokio::process::Command::new(sibling_cli()?);
            command
                .arg("update")
                .arg(if matches!(change, Change::Rollback) {
                    "rollback"
                } else {
                    "apply"
                })
                .args(["--yes", "--json"])
                .env_remove("DDG_SESSION_TOKEN")
                .stdin(Stdio::null());
            // Give the CLI the exact profile the operator saw, not a potentially changed hosts.json.
            let tmp = tempfile::tempdir().map_err(storage)?;
            let config = tmp.path().join("hosts.json");
            let mut hosts = HostsConfig::default();
            if let Some(profile) = remote {
                command.arg("-H").arg(&profile.id);
                hosts.hosts.push(profile);
            }
            hosts.save(&config).map_err(storage)?;
            command.arg("--config").arg(&config);
            if restart {
                command.arg("--restart-sessions");
            }
            if downgrade {
                command.arg("--allow-downgrade");
            }
            let output = command.output().await.map_err(storage)?;
            #[derive(serde::Deserialize)]
            struct DaemonError {
                code: Code,
                message: String,
            }
            #[derive(serde::Deserialize)]
            struct Reply {
                result: Option<ApplyOutcome>,
                error: Option<DaemonError>,
                status: Option<String>,
            }
            let reply = serde_json::from_slice::<Reply>(&output.stdout).map_err(|_| {
                UpdateError::new(
                    Code::DaemonSwapFailedRollbackFailed,
                    "ddg did not return an update result.",
                )
            })?;
            if let Some(error) = reply.error {
                return Err(UpdateError::new(error.code, error.message));
            }
            if output.status.success() {
                if let Some(outcome) = reply.result {
                    return Ok(outcome);
                }
                if reply.status.as_deref() == Some("up_to_date") {
                    let current = version.app_version.unwrap_or_default();
                    return Ok(ApplyOutcome::Completed {
                        from_version: current.clone(),
                        to_version: current,
                        tokens_preserved: true,
                    });
                }
            }
            Err(UpdateError::new(
                Code::DaemonSwapFailedRollbackFailed,
                "ddg could not confirm that the update completed.",
            ))
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use crate::update_controller::enclosing_app;
    use ddg_agents_os::{
        fs::{ensure_private_dir, write_atomic},
        process::spawn_detached,
    };
    use ddg_agents_proto::{holder::HOLDER_PROTO_VERSION, terminal::PROTOCOL_VERSION};
    use ddg_agents_update::{
        archive::{extract_tar_gz, extract_zip},
        github::{self, FetchResult},
        integrity::{RELEASE_PUBKEYS, verify_manifest, verify_sha256},
        macos::{
            EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID, SwapJob, macos_preflight_check,
            verify_macos_app_bundle,
        },
        manager::UpdateManager,
        version::{ReleaseTag, validate_apply_consent},
    };
    use std::{collections::BTreeMap, fs::File, path::Path, process::Command};

    async fn fetch(url: &str) -> Result<Vec<u8>, UpdateError> {
        match github::fetch_json(url, None).await? {
            FetchResult::Modified { body, .. } => Ok(body),
            _ => Err(UpdateError::new(
                Code::NetworkError,
                "Could not load release information.",
            )),
        }
    }

    async fn live_holder(cached: &VersionInfo) -> Result<(u32, u32), UpdateError> {
        use std::os::unix::fs::FileTypeExt;
        let run = ddg_agents_os::paths::run_dir().map_err(storage)?;
        let socket = run.join("host.sock");
        let host = HostProfile {
            transport: HostTransport::Local {
                sock_path: Some(socket.to_string_lossy().into_owned()),
            },
            ..HostProfile::local_default()
        };
        let is_socket = |path: &Path| {
            path.symlink_metadata()
                .is_ok_and(|info| info.file_type().is_socket())
        };
        if is_socket(&socket)
            && let Ok(Ok(connection)) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                ddg_agents_transport::dial_and_handshake(&host, "ddg-native-update"),
            )
            .await
            && let (Some(holder), Some(active)) = (
                connection.hello.holder_protocol_version,
                connection.hello.active_pty_count,
            )
        {
            return Ok((holder, active));
        }
        // A live runtime without a fresh count must not be treated as having no sessions.
        let live = is_socket(&socket)
            || is_socket(&run.join("terminal.sock"))
            || is_socket(&run.join("holder.sock"));
        Ok((
            cached
                .holder_protocol_version
                .unwrap_or(HOLDER_PROTO_VERSION),
            cached
                .active_pty_count
                .unwrap_or_default()
                .max(u32::from(live)),
        ))
    }

    pub async fn handoff(
        paths: UpdatePaths,
        info: VersionInfo,
        target: String,
        expected_repo: Option<String>,
        restart: bool,
        downgrade: bool,
    ) -> Result<ApplyOutcome, UpdateError> {
        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .map_err(storage)?;
        let app = enclosing_app(&exe).ok_or_else(|| {
            UpdateError::new(
                Code::InstallDirNotWritable,
                "Run the update from the installed DDG app bundle.",
            )
        })?;
        macos_preflight_check(app)?;
        verify_macos_app_bundle(app, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)?;
        let cli = sibling_cli()?.canonicalize().map_err(storage)?;
        if !cli.starts_with(app) {
            return Err(UpdateError::new(
                Code::CodesignInvalid,
                "ddg is not inside a signed app bundle.",
            ));
        }
        let tag = ReleaseTag::parse(&target)?;
        let current = ReleaseTag::parse(env!("CARGO_PKG_VERSION"))?;
        let manager = UpdateManager::new(paths.clone());
        let config = manager.load_config()?;
        if config.channel == ddg_agents_update::config::UpdateChannel::Stable
            && !tag.version().pre.is_empty()
        {
            return Err(UpdateError::new(
                Code::ManifestInvalid,
                "Select the prerelease channel first.",
            ));
        }
        let state = manager.load_state()?;
        state.should_attempt(tag.as_str(), false)?;
        let (repo, _) = config.effective_repo()?;
        if expected_repo
            .as_deref()
            .is_some_and(|shown| shown != repo.as_str())
        {
            return Err(UpdateError::new(
                Code::InvalidRepo,
                "The release source changed. Check for updates again.",
            ));
        }
        let base = format!(
            "https://github.com/{}/releases/download/{}",
            repo.as_str(),
            tag.as_str().replace('+', "%2B")
        );
        let raw = fetch(&format!("{base}/manifest.json")).await?;
        let signature = if RELEASE_PUBKEYS.is_empty() {
            None
        } else {
            Some(fetch(&format!("{base}/manifest.json.sig")).await?)
        };
        let manifest = verify_manifest(&raw, signature.as_deref(), &tag)?;
        if manifest.protocol_version != PROTOCOL_VERSION {
            return Err(UpdateError::new(
                Code::ProtocolIncompatible,
                "The release protocol is incompatible with this app.",
            ));
        }
        let (holder, active) = live_holder(&info).await?;
        validate_apply_consent(
            current.version(),
            tag.version(),
            manifest.min_compatible_schema <= 1,
            manifest.holder_restart_required || manifest.holder_protocol_version != holder,
            active,
            downgrade,
            restart,
        )?;
        if state.pending_holder_restart.is_some() && !restart {
            return Err(UpdateError::new(
                Code::HolderRestartRequired,
                "Confirmation is required to stop running sessions.",
            ));
        }
        let platform = match std::env::consts::ARCH {
            "aarch64" => "darwin-arm64",
            "x86_64" => "darwin-x86_64",
            _ => "",
        };
        let artifact = manifest
            .artifacts
            .iter()
            .find(|a| a.platform == platform)
            .filter(|a| {
                [
                    "ddg",
                    "ddg-agents-host",
                    "ddg-terminal-server",
                    "ddg-agents-native",
                ]
                .iter()
                .all(|name| a.binaries.contains_key(*name))
            })
            .ok_or_else(|| {
                UpdateError::new(
                    Code::AssetNotFound,
                    "No complete app bundle is available for this platform.",
                )
            })?;
        ensure_private_dir(&paths.state_dir).map_err(storage)?;
        let staging = paths.staging_dir();
        ensure_private_dir(&staging).map_err(storage)?;
        let staging = staging.canonicalize().map_err(storage)?;
        let stage = tempfile::Builder::new()
            .prefix(".ddg-cli-")
            .tempdir_in(staging)
            .map_err(storage)?;
        ensure_private_dir(stage.path()).map_err(storage)?;
        let archive = stage.path().join("release.archive");
        github::download_archive(
            &format!("{base}/{}", artifact.name),
            &archive,
            &artifact.sha256,
        )
        .await?;
        let file = File::open(&archive).map_err(storage)?;
        if artifact.name.ends_with(".zip") {
            extract_zip(file, stage.path())?;
        } else if artifact.name.ends_with(".tar.gz") {
            extract_tar_gz(file, stage.path())?;
        } else {
            return Err(UpdateError::new(
                Code::ArchiveUnsafe,
                "Unsupported archive format.",
            ));
        }
        let unsafe_path =
            || UpdateError::new(Code::ArchiveUnsafe, "Invalid app bundle path.");
        let bundle = stage.path().join(
            enclosing_app(Path::new(&artifact.binaries["ddg"].path)).ok_or_else(unsafe_path)?,
        );
        let binary_sha256s: BTreeMap<_, _> = artifact
            .binaries
            .values()
            .map(|binary| {
                let path = stage.path().join(&binary.path);
                let relative = path
                    .strip_prefix(&bundle)
                    .ok()
                    .and_then(Path::to_str)
                    .ok_or_else(unsafe_path)?;
                if path.canonicalize().map_err(storage)? != path
                    || !path.symlink_metadata().map_err(storage)?.is_file()
                {
                    return Err(unsafe_path());
                }
                verify_sha256(File::open(&path).map_err(storage)?, &binary.sha256)?;
                Ok((relative.to_owned(), binary.sha256.clone()))
            })
            .collect::<Result<_, UpdateError>>()?;
        verify_macos_app_bundle(&bundle, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)?;
        let write = |name: &str, bytes: &[u8]| {
            let dest = stage.path().join(name);
            if !matches!(dest.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            {
                return Err(unsafe_path());
            }
            write_atomic(&dest, bytes).map_err(storage)
        };
        // Retain the original verified bytes for the helper's live daemon replacement.
        write("manifest.json", &raw)?;
        if let Some(signature) = signature {
            write("manifest.json.sig", &signature)?;
        }
        let job = SwapJob {
            staging_app_path: bundle,
            target_app_path: app.into(),
            archive_path: archive,
            archive_sha256: artifact.sha256.clone(),
            binary_sha256s,
        };
        let name = ".ddg-cli-job.json";
        write(name, &serde_json::to_vec(&job).map_err(storage)?)?;
        write(
            "cli-consent.json",
            &serde_json::to_vec(&serde_json::json!({"allow_downgrade": downgrade, "force": false}))
                .map_err(storage)?,
        )?;
        let state = paths.state_dir.canonicalize().map_err(storage)?;
        let command = helper_command(
            cli,
            &stage.path().join(name),
            &state,
            std::process::id(),
            restart,
        );
        let helper_pid = spawn_detached(command).map_err(storage)?.id();
        let _ = stage.keep();
        Ok(ApplyOutcome::HandedOffToHelper {
            helper_pid,
            to_version: tag.version().to_string(),
        })
    }

    fn helper_command(
        cli: PathBuf,
        job: &Path,
        state: &Path,
        parent_pid: u32,
        restart: bool,
    ) -> Command {
        let mut command = Command::new(cli);
        command
            .arg("__update-apply-helper")
            .arg("--job")
            .arg(job)
            .arg("--state-dir")
            .arg(state)
            .arg("--parent-pid")
            .arg(parent_pid.to_string())
            .arg("--relaunch")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .env_remove("DDG_SESSION_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if restart {
            command.arg("--restart-sessions");
        }
        command
    }

    #[test]
    fn update_controller_helper_contract() {
        for restart in [false, true] {
            let command = helper_command(
                "/app/ddg".into(),
                Path::new("/state/job file"),
                Path::new("/state"),
                42,
                restart,
            );
            let mut args = vec![
                "__update-apply-helper",
                "--job",
                "/state/job file",
                "--state-dir",
                "/state",
                "--parent-pid",
                "42",
                "--relaunch",
            ];
            if restart {
                args.push("--restart-sessions");
            }
            assert!(command.get_args().eq(args));
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == "GITHUB_TOKEN" && value.is_none())
            );
        }
    }
}
