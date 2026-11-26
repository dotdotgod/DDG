use crate::{
    CONNECT_TIMEOUT,
    args::{Action, Args},
    ordered,
    output::{Error, Output, Result, cell, diagnostic, dial_status, table},
    profile,
};
use ddg_agents_transport::{
    DialError, DialErrorKind, HostProfile, HostTransport, HostsConfig, RemoteInstallInfo,
    dial_and_handshake, discover_docker_containers, discover_ssh_config_hosts, install_remote_host,
    parse_docker_input, parse_ssh_input,
};
use serde::Serialize;
use serde_json::json;
use std::{path::Path, time::Instant};
use tokio::{sync::mpsc, task::JoinSet};

async fn handshake(
    profile: &HostProfile,
    client_id: &str,
) -> std::result::Result<String, DialError> {
    let dial = tokio::time::timeout(CONNECT_TIMEOUT, dial_and_handshake(profile, client_id));
    Ok(dial
        .await
        .map_err(|_| DialError::new(DialErrorKind::Unavailable, "connection timed out"))??
        .hello
        .host_id)
}

async fn install(profile: &HostProfile, out: Output) -> Result<RemoteInstallInfo> {
    if out.0 {
        return Ok(install_remote_host(profile, None).await?);
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let progress = async {
        while let Some(stage) = rx.recv().await {
            eprintln!("[ddg] {}", diagnostic(&stage));
        }
    };
    let (result, ()) = tokio::join!(install_remote_host(profile, Some(tx)), progress);
    Ok(result?)
}

#[derive(Serialize)]
struct Probe {
    status: &'static str,
    host_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Error>,
}

#[derive(Serialize)]
struct MachineRow<'a> {
    id: &'a str,
    label: &'a str,
    default: bool,
    transport: String,
    expected_host_id: Option<&'a str>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    probe: Option<Probe>,
}

fn summary(transport: &HostTransport) -> String {
    match transport {
        HostTransport::Local { .. } => "local".into(),
        HostTransport::Ssh { target, port, .. } => match port {
            Some(port) => format!("ssh {target}:{port}"),
            None => format!("ssh {target}"),
        },
        HostTransport::DockerExec {
            container,
            ssh_host,
            ..
        } => match ssh_host {
            Some(host) => format!("docker {container} via {host}"),
            None => format!("docker {container}"),
        },
        HostTransport::ContainerSsh {
            host, port, user, ..
        } => format!("container-ssh {user}@{host}:{port}"),
    }
}

async fn list(config: &HostsConfig, probe: bool, out: Output) -> Result {
    let mut probes = Vec::new().into_iter();
    if probe {
        let mut jobs = JoinSet::new();
        for (idx, profile) in config.hosts.iter().cloned().enumerate() {
            jobs.spawn(async move {
                let probe = match handshake(&profile, "ddg-cli-probe").await {
                    Ok(host_id) => Probe {
                        status: "connected",
                        host_id: Some(host_id),
                        error: None,
                    },
                    Err(error) => Probe {
                        status: dial_status(error.kind),
                        host_id: None,
                        error: Some(error.into()),
                    },
                };
                (idx, probe)
            });
        }
        probes = ordered(jobs).await?.into_iter();
    }
    let rows = config
        .hosts
        .iter()
        .map(|profile| MachineRow {
            id: &profile.id,
            label: &profile.label,
            default: profile
                .id
                .trim()
                .eq_ignore_ascii_case(config.default_host_id.trim()),
            transport: summary(&profile.transport),
            expected_host_id: profile.expected_host_id.as_deref(),
            probe: probes.next(),
        })
        .collect::<Vec<_>>();
    out.show(&rows, || {
        if probe {
            table(
                [
                    "",
                    "ID",
                    "LABEL",
                    "TRANSPORT",
                    "PINNED_HOST_ID",
                    "STATUS",
                    "HOST_ID",
                ],
                rows.iter().map(|r| {
                    let p = r.probe.as_ref().expect("one probe per profile");
                    let [marker, id, label, transport, pinned] = machine_cells(r);
                    [
                        marker,
                        id,
                        label,
                        transport,
                        pinned,
                        p.status.into(),
                        p.host_id.as_deref().unwrap_or("-").into(),
                    ]
                }),
            )
        } else {
            table(
                ["", "ID", "LABEL", "TRANSPORT", "PINNED_HOST_ID"],
                rows.iter().map(machine_cells),
            )
        }
    })
}

fn machine_cells(row: &MachineRow<'_>) -> [String; 5] {
    [
        if row.default { "*" } else { " " }.into(),
        row.id.into(),
        row.label.into(),
        row.transport.clone(),
        row.expected_host_id.unwrap_or("-").into(),
    ]
}

fn new_profile(config: &HostsConfig, args: &Args<'_>) -> Result<HostProfile> {
    let port: Option<u16> = args.number("port")?;
    let identity = args.string("identity");
    let known_hosts = args.string("known-hosts");
    let (transport, label) = if let Some(docker) = args.value("docker") {
        if port.is_some()
            || (!args.has("ssh-host") && (identity.is_some() || known_hosts.is_some()))
        {
            return Err(Error::usage(
                "Docker does not support --port; SSH paths require --ssh-host",
            ));
        }
        let container = parse_docker_input(docker)
            .map_err(|_| Error::usage("invalid Docker container or docker exec command"))?;
        let label = container.clone();
        (
            HostTransport::DockerExec {
                container,
                ssh_host: args.string("ssh-host"),
                known_hosts_file: known_hosts,
                identity_file: identity,
            },
            label,
        )
    } else if let Some(address) = args.value("container-ssh") {
        let invalid = || Error::usage("--container-ssh requires user@host:port");
        let (user, server) = address.split_once('@').ok_or_else(invalid)?;
        let (host, address_port) = server.rsplit_once(':').ok_or_else(invalid)?;
        let address_port: u16 = address_port.parse().map_err(|_| invalid())?;
        (
            HostTransport::ContainerSsh {
                user: user.into(),
                host: host.into(),
                port: port.unwrap_or(address_port),
                known_hosts_file: known_hosts,
                identity_file: identity,
            },
            host.into(),
        )
    } else {
        let mut transport = parse_ssh_input(args.pos[0])
            .map_err(|_| Error::usage("invalid SSH target or unsupported SSH command options"))?;
        let HostTransport::Ssh {
            target,
            port: old_port,
            known_hosts_file,
            identity_file,
        } = &mut transport
        else {
            unreachable!("parse_ssh_input returns SSH transport");
        };
        *old_port = port.or(*old_port);
        *known_hosts_file = known_hosts.or_else(|| known_hosts_file.take());
        *identity_file = identity.or_else(|| identity_file.take());
        let label = target.rsplit('@').next().unwrap_or(target).to_owned();
        (transport, label)
    };
    let label = args.string("label").unwrap_or(label);
    let id = args.string("id").unwrap_or_else(|| slug(config, &label));
    let profile = HostProfile {
        id: id.trim().to_owned(),
        label,
        transport,
        expected_host_id: None,
    };
    profile
        .validate()
        .map_err(|_| Error::usage("invalid machine ID, label, SSH target, port or path"))?;
    if config.find(&profile.id).is_some() || profile.id.eq_ignore_ascii_case("local") {
        return Err(Error::usage("machine ID is already in use or reserved"));
    }
    Ok(profile)
}

fn slug(config: &HostsConfig, label: &str) -> String {
    let stem = label
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase();
    let stem = if stem.is_empty() || matches!(stem.as_str(), "all" | "local") {
        "machine"
    } else {
        &stem
    };
    if config.find(stem).is_none() {
        return stem.into();
    }
    (2..)
        .map(|n| format!("{stem}-{n}"))
        .find(|id| config.find(id).is_none())
        .unwrap()
}

fn remove(config: &mut HostsConfig, id: &str) -> Result<String> {
    let id = profile(config, id)?.id.clone();
    if id.trim().eq_ignore_ascii_case("local") || config.hosts.len() == 1 {
        return Err(Error::usage("cannot remove local or the last machine"));
    }
    config.hosts.retain(|profile| profile.id != id);
    if config.find(&config.default_host_id).is_none() {
        config.default_host_id = config.hosts[0].id.clone();
    }
    Ok(id)
}

fn set_default(config: &mut HostsConfig, id: &str) -> Result {
    config.default_host_id = profile(config, id)?.id.clone();
    Ok(())
}

fn pin_installed(profile: &mut HostProfile, host_id: &str) -> Result<bool> {
    if let Some(pin) = &profile.expected_host_id {
        if pin != host_id {
            return Err(Error::new(
                "host_id_mismatch",
                "installed host does not match the pinned identity",
            ));
        }
        return Ok(false);
    }
    profile.expected_host_id = Some(host_id.into());
    Ok(true)
}

pub async fn run(args: &Args<'_>, mut config: HostsConfig, path: &Path, out: Output) -> Result {
    use Action::*;
    match args.action {
        MachineLs => list(&config, args.has("probe"), out).await,
        MachineAdd => {
            let mut profile = new_profile(&config, args)?;
            if !args.has("no-test") {
                profile.expected_host_id = Some(if args.has("install") {
                    install(&profile, out).await?.host_id
                } else {
                    handshake(&profile, "ddg-cli-add").await.map_err(|e| {
                        if e.kind == DialErrorKind::HostNotInstalled {
                            Error::new("not_installed", format!(
                                "ddg-agents-host is not installed; re-run with --install or add with --no-test then run ddg machine install {}",
                                cell(&profile.id)
                            ))
                        } else { e.into() }
                    })?
                });
            }
            let response = json!({"id": profile.id, "label": profile.label, "transport": summary(&profile.transport), "expected_host_id": profile.expected_host_id});
            let confirmation = format!("added {} ({})\n", cell(&profile.id), cell(&profile.label));
            config.hosts.push(profile);
            config.save(path)?;
            out.show(&response, || confirmation)
        }
        MachineRm => {
            let id = remove(&mut config, args.pos[0])?;
            config.save(path)?;
            out.show(
                &json!({"id": id, "default_host_id": config.default_host_id}),
                || format!("removed {}\n", cell(&id)),
            )
        }
        MachineDefault => {
            set_default(&mut config, args.pos[0])?;
            config.save(path)?;
            out.show(&json!({"default_host_id": config.default_host_id}), || {
                format!("default machine: {}\n", cell(&config.default_host_id))
            })
        }
        MachineTest => {
            let profile = profile(&config, args.pos[0])?;
            let start = Instant::now();
            let host_id = handshake(profile, "ddg-cli-test").await.map_err(|e| {
                if e.kind == DialErrorKind::HostNotInstalled {
                    Error::new(
                        "not_installed",
                        format!("run ddg machine install {}", cell(&profile.id)),
                    )
                } else {
                    e.into()
                }
            })?;
            let latency = start.elapsed().as_millis();
            out.show(
                &json!({"id": profile.id, "host_id": host_id, "latency_ms": latency}),
                || format!("{}: {} ({latency} ms)\n", cell(&profile.id), cell(&host_id)),
            )
        }
        MachineInstall => {
            let profile = profile(&config, args.pos[0])?;
            let info = install(profile, out).await?;
            let id = profile.id.clone();
            let profile = config.hosts.iter_mut().find(|p| p.id == id).unwrap();
            if pin_installed(profile, &info.host_id)? {
                config.save(path)?;
            }
            out.show(
                &json!({"id": id, "host_id": info.host_id, "platform": info.platform.slug()}),
                || {
                    format!(
                        "installed {} ({}, {})\n",
                        cell(&id),
                        cell(&info.host_id),
                        info.platform.slug()
                    )
                },
            )
        }
        MachineDiscover => {
            let ssh = discover_ssh_config_hosts();
            let docker = discover_docker_containers();
            out.show(&json!({"ssh": ssh, "docker": docker}), || {
                table(
                    ["TRANSPORT", "TARGET"],
                    ssh.iter()
                        .map(|s| ["ssh".into(), s.clone()])
                        .chain(docker.iter().map(|s| ["docker".into(), s.clone()])),
                )
            })
        }
        _ => unreachable!(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn installation_cannot_replace_existing_pin() {
        let mut profile = HostProfile::local_default();
        assert!(pin_installed(&mut profile, "first").unwrap());
        assert!(!pin_installed(&mut profile, "first").unwrap());
        assert_eq!(
            pin_installed(&mut profile, "other").unwrap_err().code,
            "host_id_mismatch"
        );
        assert_eq!(profile.expected_host_id.as_deref(), Some("first"));
    }
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn add_default_and_remove_keep_private_config() {
        let dir = TempDir(std::env::temp_dir().join(format!(
                "ddg-cli-unit-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        let path = dir.0.join("hosts.json");
        for argv in [
            vec![
                "m",
                "add",
                "ssh -p 2222 user@example.test",
                "--label",
                "Dev Box",
                "--no-test",
            ],
            vec![
                "m",
                "add",
                "--docker",
                "docker exec -it container bash",
                "--label",
                "Dev Box",
                "--no-test",
            ],
            vec!["m", "default", "DEV-BOX"],
        ] {
            let raw: Vec<_> = argv.into_iter().map(str::to_owned).collect();
            run(
                &crate::args::parse(&raw).unwrap(),
                crate::load_config(&path).unwrap(),
                &path,
                Output(true),
            )
            .await
            .unwrap();
        }
        let cfg = crate::load_config(&path).unwrap();
        assert_eq!(cfg.default_host_id, "dev-box");
        assert!(matches!(
            cfg.find("dev-box").unwrap().transport,
            HostTransport::Ssh {
                port: Some(2222),
                ..
            }
        ));
        assert!(matches!(
            cfg.find("dev-box-2").unwrap().transport,
            HostTransport::DockerExec { .. }
        ));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let raw = ["m", "rm", "dev-box"].map(str::to_owned);
        run(&crate::args::parse(&raw).unwrap(), cfg, &path, Output(true))
            .await
            .unwrap();
        let mut cfg = crate::load_config(&path).unwrap();
        assert_eq!(cfg.default_host_id, "local");
        assert!(cfg.find("dev-box").is_none());
        assert!(remove(&mut cfg, "LOCAL").is_err());
        assert!(set_default(&mut cfg, "missing").is_err());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn container_ssh_and_credential_paths_are_not_echoed() {
        let raw = [
            "m",
            "add",
            "--container-ssh",
            "dev@example.test:2222",
            "--identity",
            "/tmp/key-secret",
            "--known-hosts",
            "/tmp/known-secret",
        ]
        .map(str::to_owned);
        let host =
            new_profile(&HostsConfig::default(), &crate::args::parse(&raw).unwrap()).unwrap();
        assert_eq!(host.id, "example-test");
        assert!(matches!(
            host.transport,
            HostTransport::ContainerSsh { port: 2222, .. }
        ));
        assert!(!summary(&host.transport).contains("secret"));
    }
}
