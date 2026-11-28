use crate::{
    args::{Action, Args},
    config_path, connect, load_config,
    output::{Error, Output, Result, cell, table},
    selected,
};
use ddg_agents_proto::host::{ExecutorSpec, PresetDetectStatus};
use ddg_agents_transport::{
    HostProfile, HostTransport, PresetDetectError, PresetDetectOutcome,
    agent::{AgentConfigError, AgentHostOverride, AgentProfile, AgentsConfig, ConfigStamp},
    presets::{AgentPreset, BUILTIN_AGENT_PRESETS, find_agent_preset},
};
use serde::Serialize;
use serde_json::json;
use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

impl From<AgentConfigError> for Error {
    fn from(error: AgentConfigError) -> Self {
        match error {
            AgentConfigError::Conflict => {
                Self::new("config_conflict", "agents.json changed since it was read")
            }
            AgentConfigError::LockTimeout => {
                Self::new("config_locked", "timed out waiting for agents.lock")
            }
            AgentConfigError::Invalid(rule) => {
                match rule.split_once(':').map_or(rule.as_str(), |(code, _)| code) {
                    "reserved_env_key" => Self::new("reserved_env_key", "reserved environment key"),
                    "secret_env_rejected" => Self::new(
                        "secret_env_rejected",
                        "secret environment keys are not allowed",
                    ),
                    code => Self::new(
                        "config_invalid",
                        format!("agents.json failed validation ({code})"),
                    ),
                }
            }
            AgentConfigError::Corrupt { .. } => Self::new("config_invalid", error.to_string()),
            AgentConfigError::Io(_) => {
                Self::new("config_invalid", "could not read or write agents.json")
            }
        }
    }
}

fn path(args: &Args<'_>) -> Result<PathBuf> {
    Ok(if args.config.is_some() {
        config_path(args)?.with_file_name("agents.json")
    } else {
        AgentsConfig::default_path()?
    })
}

pub fn load(args: &Args<'_>) -> Result<AgentsConfig> {
    Ok(AgentsConfig::load(&path(args)?)?.0)
}

fn not_found() -> Error {
    Error::new("agent_not_found", "agent profile not found")
}

fn find<'a>(config: &'a AgentsConfig, id: &str) -> Result<&'a AgentProfile> {
    config.find(id).ok_or_else(not_found)
}

pub struct Launch {
    pub agent_profile: Option<String>,
    pub executor: ExecutorSpec,
    pub cwd: Option<String>,
}

pub fn resolve(
    config: &AgentsConfig,
    id: Option<&str>,
    args: &Args<'_>,
    host: &HostProfile,
) -> Result<Launch> {
    let id = id.or(config.default_agent_id.as_deref()).ok_or_else(|| {
        Error::new(
            "no_default_agent",
            "choose an agent or set one with ddg agent default",
        )
    })?;
    let profile = find(config, id)?;
    let resolved = profile.resolve_executor(
        Some(&host.id),
        &args.argv,
        args.value("cwd"),
        !matches!(host.transport, HostTransport::Local { .. }),
    );
    Ok(Launch {
        agent_profile: Some(profile.id.clone()),
        executor: resolved.executor,
        cwd: resolved.cwd,
    })
}

pub fn for_create(args: &mut Args<'_>, host: &HostProfile) -> Result<Launch> {
    if let Some(id) = args.value("agent") {
        return resolve(&load(args)?, Some(id), args, host);
    }
    let argv = std::mem::take(&mut args.argv);
    let executor = if argv.is_empty() {
        ExecutorSpec::DefaultShell
    } else {
        ExecutorSpec::Command {
            argv,
            env: Default::default(),
        }
    };
    Ok(Launch {
        agent_profile: None,
        executor,
        cwd: args.string("cwd"),
    })
}

fn show_profile(profile: &AgentProfile, reveal: bool) -> AgentProfile {
    let mut view = profile.clone();
    if !reveal {
        for value in view.env.values_mut().chain(
            view.host_overrides
                .values_mut()
                .flat_map(|host| host.env.values_mut().flatten()),
        ) {
            *value = "***".into();
        }
    }
    view
}

fn render_profile(profile: &AgentProfile) -> String {
    let mut out = format!(
        "Agent: {}\nLabel: {}\nArgv: {:?}\nCWD: {}\nIcon: {}\n",
        cell(&profile.id),
        cell(&profile.label),
        profile.argv,
        cell(profile.default_cwd.as_deref().unwrap_or("-")),
        cell(profile.icon.as_deref().unwrap_or("-"))
    );
    for (key, value) in &profile.env {
        let _ = writeln!(out, "  {key}={value:?}");
    }
    for (id, host) in &profile.host_overrides {
        let _ = writeln!(out, "Host override: {}", cell(id));
        if let Some(argv) = &host.argv {
            let _ = writeln!(out, "  Argv: {argv:?}");
        }
        if let Some(cwd) = &host.cwd {
            let _ = writeln!(out, "  CWD: {}", cell(cwd));
        }
        for (key, value) in &host.env {
            match value {
                Some(value) => {
                    let _ = writeln!(out, "  {key}={value:?}");
                }
                None => {
                    let _ = writeln!(out, "  {key} (unset)");
                }
            }
        }
    }
    out
}

fn change(mut args: Args<'_>, config: &mut AgentsConfig) -> Result<Option<String>> {
    use Action::*;
    let id = args.pos.first().copied();
    if args.action == AgentDefault {
        config.default_agent_id = id
            .map(|id| find(config, id).map(|p| p.id.clone()))
            .transpose()?;
        return Ok(config.default_agent_id.clone());
    }
    let preset = args
        .value("preset")
        .map(|id| {
            find_agent_preset(id)
                .ok_or_else(|| Error::new("preset_not_found", "built-in agent preset not found"))
        })
        .transpose()?;
    let id = preset
        .map(|preset| preset.id)
        .or(id)
        .expect("profile ID required by parser");
    let existing = config
        .agents
        .iter()
        .position(|p| p.id.eq_ignore_ascii_case(id));
    if args.action == AgentRm {
        let idx = existing.ok_or_else(not_found)?;
        let id = config.agents[idx].id.clone();
        if let Some(host) = args.value("host-override") {
            config.agents[idx].host_overrides.remove(host);
        } else {
            config.agents.remove(idx);
            if config.default_agent_id.as_deref() == Some(&id) {
                config.default_agent_id = None;
            }
        }
        return Ok(Some(id));
    }
    let profile = if args.action == AgentAdd {
        if existing.is_some() {
            return Err(Error::new("agent_exists", "agent profile already exists"));
        }
        config.agents.push(match preset {
            Some(preset) => preset.to_profile(),
            None => AgentProfile {
                id: id.into(),
                label: id.into(),
                argv: std::mem::take(&mut args.argv),
                default_cwd: None,
                env: Default::default(),
                icon: None,
                host_overrides: Default::default(),
            },
        });
        config.agents.last_mut().unwrap()
    } else {
        &mut config.agents[existing.ok_or_else(not_found)?]
    };
    if let Some(label) = args.string("label") {
        profile.label = label;
    }
    if let Some(icon) = args.string("icon") {
        profile.icon = Some(icon);
    }
    if let Some(host) = args.value("clear-host-override") {
        profile.host_overrides.remove(host);
    }
    let argv = (!args.argv.is_empty()).then(|| std::mem::take(&mut args.argv));
    let cwd = args.string("cwd");
    let env = args
        .values("env")
        .iter()
        .map(|pair| pair.split_once('=').expect("parsed KEY=VALUE"));
    if let Some(host) = args.string("host-override") {
        let over = profile
            .host_overrides
            .entry(host)
            .or_insert_with(|| AgentHostOverride {
                argv: None,
                cwd: None,
                env: Default::default(),
            });
        if argv.is_some() {
            over.argv = argv;
        }
        if cwd.is_some() {
            over.cwd = cwd;
        }
        over.env.extend(
            args.values("unset-env")
                .iter()
                .map(|key| ((*key).into(), None)),
        );
        over.env
            .extend(env.map(|(key, value)| (key.into(), Some(value.into()))));
    } else {
        if let Some(argv) = argv {
            profile.argv = argv;
        }
        if cwd.is_some() {
            profile.default_cwd = cwd;
        }
        for key in args.values("unset-env") {
            profile.env.remove(*key);
        }
        profile
            .env
            .extend(env.map(|(key, value)| (key.into(), value.into())));
    }
    if args.has("default") {
        config.default_agent_id = Some(profile.id.clone());
    }
    Ok(Some(profile.id.clone()))
}

pub fn run(args: Args<'_>, out: Output) -> Result {
    let path = path(&args)?;
    let (mut config, stamp): (AgentsConfig, ConfigStamp) = AgentsConfig::load(&path)?;
    match args.action {
        Action::AgentLs => {
            let rows = config
                .agents
                .iter()
                .map(|p| {
                    let default = config.default_agent_id.as_deref() == Some(&p.id);
                    (p, default)
                })
                .collect::<Vec<_>>();
            let value = json!({"default_agent_id": config.default_agent_id, "agents": rows.iter().map(|(p, default)| json!({"id": p.id, "label": p.label, "default": default})).collect::<Vec<_>>()});
            out.show(&value, || {
                table(
                    ["ID", "LABEL", "DEFAULT"],
                    rows.iter().map(|(p, default)| {
                        [
                            p.id.clone(),
                            p.label.clone(),
                            if *default { "yes" } else { "-" }.into(),
                        ]
                    }),
                )
            })
        }
        Action::AgentShow => {
            let view = show_profile(find(&config, args.pos[0])?, args.has("reveal-env"));
            out.show(&view, || render_profile(&view))
        }
        action => {
            let host_override = args.value("host-override");
            let id = change(args, &mut config)?;
            config.save(&path, &stamp)?;
            out.show(
                &json!({"id": id, "default_agent_id": config.default_agent_id}),
                || {
                    let verb = match action {
                        Action::AgentAdd => "added",
                        Action::AgentSet => "updated",
                        Action::AgentRm => "removed",
                        Action::AgentDefault => "default agent:",
                        _ => unreachable!(),
                    };
                    let id = cell(id.as_deref().unwrap_or("(none)"));
                    if let (Action::AgentRm, Some(host)) = (action, host_override) {
                        return format!("removed {id} override for {}\n", cell(host));
                    }
                    format!("{verb} {id}\n")
                },
            )
        }
    }
}

#[derive(Serialize)]
struct PresetRow {
    id: &'static str,
    label: &'static str,
    argv: &'static [&'static str],
    icon: &'static str,
    status: PresetDetectStatus,
    registered: bool,
}

fn preset_rows(config: &AgentsConfig, outcome: &PresetDetectOutcome) -> Vec<PresetRow> {
    BUILTIN_AGENT_PRESETS
        .iter()
        .map(|preset| {
            let status = match outcome {
                PresetDetectOutcome::Unsupported => PresetDetectStatus::Unknown,
                PresetDetectOutcome::Detected(items) => items
                    .iter()
                    .find(|item| item.id == preset.id)
                    .map_or(PresetDetectStatus::Unknown, |item| item.status),
            };
            PresetRow {
                id: preset.id,
                label: preset.label,
                argv: preset.argv,
                icon: preset.icon,
                status,
                registered: config.find(preset.id).is_some(),
            }
        })
        .collect()
}

#[derive(Debug, Default, Serialize)]
struct DetectedAdd<'a> {
    host_id: &'a str,
    added: Vec<&'static str>,
    skipped: Vec<&'static str>,
    unknown: Vec<String>,
}

fn save_detected(
    path: &Path,
    config: &mut AgentsConfig,
    stamp: &ConfigStamp,
    found: impl IntoIterator<Item = &'static AgentPreset>,
) -> Result<(Vec<&'static str>, Vec<&'static str>)> {
    let mut added = Vec::new();
    let mut skipped = Vec::new();
    for preset in found {
        if config.find(preset.id).is_some() {
            skipped.push(preset.id);
        } else {
            added.push(preset.id);
            config.agents.push(preset.to_profile());
        }
    }
    if !added.is_empty() {
        config.save(path, stamp)?;
    }
    Ok((added, skipped))
}

fn unavailable(message: impl AsRef<str>) -> Error {
    Error::new("presets_unavailable", message)
}

fn add_detected<'a>(
    path: &Path,
    host_id: &'a str,
    outcome: PresetDetectOutcome,
) -> Result<DetectedAdd<'a>> {
    let PresetDetectOutcome::Detected(items) = outcome else {
        return Err(unavailable("host does not support preset detection"));
    };
    let mut result = DetectedAdd {
        host_id,
        ..Default::default()
    };
    let mut found = Vec::new();
    for item in items {
        match item.status {
            PresetDetectStatus::Found => found.extend(find_agent_preset(&item.id)),
            PresetDetectStatus::Unknown => result.unknown.push(item.id),
            PresetDetectStatus::NotFound => {}
        }
    }
    if found.is_empty() {
        if !result.unknown.is_empty() {
            return Err(unavailable(
                "could not determine which presets are installed",
            ));
        }
    } else {
        let (mut config, stamp) = AgentsConfig::load(path)?;
        (result.added, result.skipped) = save_detected(path, &mut config, &stamp, found)?;
    }
    Ok(result)
}

async fn discover_presets(host: &HostProfile, no_detect: bool) -> Result<PresetDetectOutcome> {
    if no_detect {
        return Ok(PresetDetectOutcome::Unsupported);
    }
    let (client, _events) = connect(host).await?;
    let result = client.detect_agent_presets().await;
    client.close();
    result.map_err(|error| match error {
        PresetDetectError::Rpc(error) => error.into(),
        PresetDetectError::Protocol(message) => Error::new("protocol_error", message),
    })
}

pub async fn run_presets(args: &Args<'_>, out: Output) -> Result {
    let host_config = load_config(&config_path(args)?)?;
    let host = selected(args, &host_config)?;
    if args.action == Action::AgentAdd {
        let outcome = discover_presets(host, false)
            .await
            .map_err(|err| unavailable(err.message))?;
        let result = add_detected(&path(args)?, &host.id, outcome)?;
        return out.show(&result, || {
            format!(
                "Added: {}\nAlready registered: {}\nUnknown: {}\n",
                result.added.join(", "),
                result.skipped.join(", "),
                result.unknown.join(", "),
            )
        });
    }
    let outcome = discover_presets(host, args.has("no-detect")).await?;
    let rows = preset_rows(&load(args)?, &outcome);
    let supported = matches!(outcome, PresetDetectOutcome::Detected(_));
    out.show(
        &json!({"host_id": host.id, "detection_supported": supported, "presets": rows}),
        || {
            table(
                ["ID", "LABEL", "COMMAND", "STATUS", "REGISTERED"],
                rows.iter().map(|row| {
                    [
                        row.id.into(),
                        row.label.into(),
                        row.argv.join(" "),
                        match row.status {
                            PresetDetectStatus::Found => "found",
                            PresetDetectStatus::NotFound => "not_found",
                            PresetDetectStatus::Unknown => "unknown",
                        }
                        .into(),
                        if row.registered { "yes" } else { "no" }.into(),
                    ]
                }),
            )
        },
    )
}

#[cfg(test)]
mod agent_and_run_logic {
    use super::*;
    use crate::args::parse;
    use ddg_agents_proto::host::AgentPresetDetection;

    // UT-CLI-02
    #[test]
    fn preset_add_saves_v1_defaults_and_explicit_overrides_offline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let run = |command: &[&str]| {
            let raw = [
                "agent",
                "add",
                "--config",
                dir.path().join("hosts.json").to_str().unwrap(),
            ]
            .into_iter()
            .chain(command.iter().copied())
            .map(str::to_owned)
            .collect::<Vec<_>>();
            super::run(parse(&raw)?, Output(false))
        };
        run(&["--preset", "PI"]).unwrap();
        let (config, _) = AgentsConfig::load(&path).unwrap();
        assert_eq!(config.schema_version, 1);
        assert!(config.default_agent_id.is_none());
        assert_eq!(
            config.agents[0],
            find_agent_preset("pi").unwrap().to_profile()
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            run(&["--preset", "missing"]).unwrap_err().code,
            "preset_not_found"
        );
        assert_eq!(
            run(&["--preset", "pi", "--default"]).unwrap_err().code,
            "agent_exists"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            run(&["--preset", "agy"]).unwrap_err().code,
            "preset_not_found"
        );
        run(&[
            "--preset",
            "antigravity",
            "--default",
            "--label",
            "My Antigravity",
            "--icon",
            "pi",
            "--cwd",
            "/base",
            "--env",
            "MODE=custom",
        ])
        .unwrap();
        let (config, _) = AgentsConfig::load(&path).unwrap();
        assert_eq!(config.default_agent_id.as_deref(), Some("antigravity"));
        let profile = config.find("antigravity").unwrap();
        assert_eq!(profile.argv, ["agy"]);
        assert_eq!(profile.label, "My Antigravity");
        assert_eq!(profile.icon.as_deref(), Some("pi"));
        assert_eq!(profile.default_cwd.as_deref(), Some("/base"));
        assert_eq!(profile.env["MODE"], "custom");
    }

    fn detections(statuses: &[(&str, PresetDetectStatus)]) -> PresetDetectOutcome {
        PresetDetectOutcome::Detected(
            statuses
                .iter()
                .map(|&(id, status)| AgentPresetDetection {
                    id: id.into(),
                    status,
                })
                .collect(),
        )
    }

    // UT-CLI-01
    #[test]
    fn preset_listing_keeps_static_rows_and_registration_independent_of_detection() {
        use PresetDetectStatus::*;
        let mut config = AgentsConfig::default();
        config
            .agents
            .push(find_agent_preset("pi").unwrap().to_profile());
        for outcome in [
            PresetDetectOutcome::Unsupported,
            detections(&[("pi", Found), ("codex", NotFound), ("opencode", Unknown)]),
        ] {
            let supported = matches!(outcome, PresetDetectOutcome::Detected(_));
            let rows = serde_json::to_value(preset_rows(&config, &outcome)).unwrap();
            assert_eq!(rows.as_array().unwrap().len(), 5);
            assert_eq!(
                rows[0],
                json!({"id":"pi", "label":"Pi", "argv":["pi"], "icon":"pi",
                "status": if supported { "found" } else { "unknown" }, "registered":true})
            );
            assert_eq!(
                rows[2]["status"],
                if supported { "not_found" } else { "unknown" }
            );
            assert_eq!(rows[2]["registered"], false);
            assert_eq!(rows[3]["status"], "unknown");
        }
    }

    // UT-CLI-01: exercise the real dial and wire negotiation on an isolated socket.
    #[cfg(unix)]
    #[tokio::test]
    async fn preset_detection_distinguishes_legacy_offline_and_live_hosts() {
        use ddg_agents_os::ipc::LocalListener;
        use ddg_agents_proto::{
            frame::{CONTROL_CHANNEL, Frame, FrameKind, HEADER_LEN, decode_header},
            host::FEATURE_AGENT_PRESETS_V1,
            rpc::{Request, Response},
        };
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("h.sock");
        let host = HostProfile {
            transport: HostTransport::Local {
                sock_path: Some(socket.to_str().unwrap().into()),
            },
            ..HostProfile::local_default()
        };
        assert_eq!(
            discover_presets(&host, true).await.unwrap(),
            PresetDetectOutcome::Unsupported
        );
        assert_eq!(
            discover_presets(&host, false).await.unwrap_err().code,
            "unavailable"
        );
        let listener = LocalListener::bind(&socket).unwrap().into_tokio().unwrap();
        for supported in [false, true] {
            let serve = async {
                let mut peer = BufReader::new(listener.accept().await.unwrap().0);
                let mut hello = String::new();
                peer.read_line(&mut hello).await.unwrap();
                let request: Request = serde_json::from_str(&hello).unwrap();
                assert_eq!(request.method, "hello");
                let features = if supported {
                    vec![FEATURE_AGENT_PRESETS_V1]
                } else {
                    vec![]
                };
                let hello = Response::ok(
                    request.id,
                    json!({
                        "protocol_version":1, "host_id":"isolated", "conn_id":1,
                        "boot_generation":1, "features": features
                    }),
                );
                peer.write_all(format!("{}\n", json!(hello)).as_bytes())
                    .await
                    .unwrap();
                if supported {
                    let mut header = [0; HEADER_LEN];
                    peer.read_exact(&mut header).await.unwrap();
                    let (channel, kind, len) = decode_header(&header).unwrap();
                    assert_eq!((channel, kind), (CONTROL_CHANNEL, FrameKind::Data));
                    let mut payload = vec![0; len];
                    peer.read_exact(&mut payload).await.unwrap();
                    let request: Request = serde_json::from_slice(&payload).unwrap();
                    assert_eq!(request.method, "host.agent_presets_detect");
                    assert_eq!(request.params, json!({}));
                    let result = Response::ok(
                        request.id,
                        json!({"items":[
                            {"id":"pi", "status":"found"}, {"id":"codex", "status":"not_found"}
                        ]}),
                    );
                    peer.write_all(
                        &Frame::data(CONTROL_CHANNEL, serde_json::to_vec(&result).unwrap())
                            .encode()
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                }
                let mut unused = Vec::new();
                peer.read_to_end(&mut unused).await.unwrap();
                assert!(unused.is_empty());
            };
            let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(discover_presets(&host, false), serve)
            })
            .await
            .unwrap();
            let outcome = outcome.unwrap();
            assert_eq!(
                matches!(outcome, PresetDetectOutcome::Detected(_)),
                supported
            );
            let rows = preset_rows(&AgentsConfig::default(), &outcome);
            assert_eq!(
                rows[0].status,
                if supported {
                    PresetDetectStatus::Found
                } else {
                    PresetDetectStatus::Unknown
                }
            );
            assert_eq!(rows[3].status, PresetDetectStatus::Unknown);
        }
    }

    // UT-CLI-03
    #[test]
    fn detected_add_is_atomic_and_noop_preserves_file_state() {
        use PresetDetectStatus::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        for outcome in [
            PresetDetectOutcome::Unsupported,
            detections(&[("pi", Unknown)]),
        ] {
            assert_eq!(
                add_detected(&path, "local", outcome).unwrap_err().code,
                "presets_unavailable"
            );
            assert!(!path.exists());
        }
        let no_hits = add_detected(&path, "local", detections(&[("pi", NotFound)])).unwrap();
        assert_eq!(
            json!(no_hits),
            json!({"host_id":"local", "added":[], "skipped":[], "unknown":[]})
        );
        assert!(!path.exists());
        let first = add_detected(&path, "local", detections(&[("pi", Found)])).unwrap();
        assert_eq!(first.added, ["pi"]);
        let (mut config, stamp) = AgentsConfig::load(&path).unwrap();
        assert!(config.default_agent_id.is_none());
        config.default_agent_id = Some("pi".into());
        config.agents[0].argv = vec!["custom-pi".into()];
        config.save(&path, &stamp).unwrap();

        let batch = detections(&[("pi", Found), ("codex", Found), ("opencode", Unknown)]);
        let result = add_detected(&path, "remote", batch.clone()).unwrap();
        assert_eq!(
            json!(result),
            json!({"host_id":"remote", "added":["codex"], "skipped":["pi"], "unknown":["opencode"]})
        );
        let bytes = std::fs::read(&path).unwrap();
        let result = add_detected(&path, "remote", batch).unwrap();
        assert!(result.added.is_empty());
        assert_eq!(result.skipped, ["pi", "codex"]);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let (mut config, stamp) = AgentsConfig::load(&path).unwrap();
        assert_eq!(config.default_agent_id.as_deref(), Some("pi"));
        assert_eq!(config.find("pi").unwrap().argv, ["custom-pi"]);

        let mut snapshot = config.clone();
        config.agents[0].label = "Concurrent update".into();
        let latest = config.save(&path, &stamp).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let agy = find_agent_preset("antigravity").unwrap();
        assert_eq!(
            save_detected(&path, &mut snapshot, &stamp, [agy])
                .unwrap_err()
                .code,
            "config_conflict"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        for i in config.agents.len()..ddg_agents_transport::agent::MAX_AGENT_PROFILES - 1 {
            let mut custom = config.agents[0].clone();
            custom.id = format!("custom-{i}");
            config.agents.push(custom);
        }
        config.save(&path, &latest).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let hits = detections(&[("antigravity", Found), ("opencode", Found)]);
        let error = add_detected(&path, "local", hits).unwrap_err();
        assert_eq!(error.code, "config_invalid");
        assert!(error.message.contains("too_many_agent_profiles"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn show_masks_base_and_override_values_and_preserves_unset() {
        let raw = ["agent", "add", "pi", "--env=KEEP=base-value", "--", "pi"].map(str::to_owned);
        let mut config = AgentsConfig::default();
        change(parse(&raw).unwrap(), &mut config).unwrap();
        let raw = [
            "agent",
            "set",
            "pi",
            "--host-override=worker",
            "--env=REMOTE=override-value",
            "--unset-env=KEEP",
        ]
        .map(str::to_owned);
        change(parse(&raw).unwrap(), &mut config).unwrap();
        let profile = config.find("pi").unwrap();
        for reveal in [false, true] {
            let view = show_profile(profile, reveal);
            let json = serde_json::to_value(&view).unwrap();
            assert!(json["host_overrides"]["worker"]["env"]["KEEP"].is_null());
            let plain = render_profile(&view);
            assert!(plain.contains("KEEP (unset)"));
            for rendered in [plain, json.to_string()] {
                for value in ["base-value", "override-value"] {
                    assert_eq!(rendered.matches(value).count(), usize::from(reveal));
                }
                assert_eq!(rendered.matches("***").count(), if reveal { 0 } else { 2 });
            }
        }
    }

    #[test]
    fn forbidden_env_values_never_reach_either_stderr_format() {
        for (key, code) in [
            ("OPENAI_API_KEY", "secret_env_rejected"),
            ("dDg_sEsSiOn_ToKeN", "reserved_env_key"),
            ("LD_PRELOAD", "reserved_env_key"),
        ] {
            let raw = [
                "agent",
                "add",
                "pi",
                "--env",
                &format!("{key}=SECRET_VAL"),
                "--",
                "pi",
            ]
            .map(str::to_owned);
            let mut config = AgentsConfig::default();
            let error = change(parse(&raw).unwrap(), &mut config)
                .and_then(|_| config.validate().map_err(Error::from))
                .unwrap_err();
            assert_eq!(error.code, code);
            for json in [false, true] {
                let line = Output(json).error_line(&error);
                assert!(line.contains(code));
                assert!(!line.contains("SECRET_VAL"));
            }
        }
    }

    #[test]
    fn default_shell_and_literal_command_do_not_load_agent_config() {
        let host = HostProfile::local_default();
        let raw = ["room", "create", "--cwd=/explicit"].map(str::to_owned);
        let launch = for_create(&mut parse(&raw).unwrap(), &host).unwrap();
        assert_eq!(launch.executor, ExecutorSpec::DefaultShell);
        assert_eq!(launch.agent_profile, None);
        assert_eq!(launch.cwd.as_deref(), Some("/explicit"));
        let raw = ["session", "split", "anchor", "--", "shell", "--arg"].map(str::to_owned);
        let launch = for_create(&mut parse(&raw).unwrap(), &host).unwrap();
        assert_eq!(
            launch.executor,
            ExecutorSpec::Command {
                argv: vec!["shell".into(), "--arg".into()],
                env: Default::default()
            }
        );
        assert_eq!(launch.agent_profile, None);
        assert_eq!(launch.cwd, None);
    }

    #[test]
    fn mutations_update_defaults_and_allow_clearing_overrides() {
        let mut config = AgentsConfig::default();
        for raw in [
            "agent add pi --default -- pi",
            "agent set pi --label Pi --icon pi --cwd /base --env A=1 -- pi base",
            "agent set pi --host-override remote --cwd /remote --unset-env A -- pi remote",
        ] {
            let raw = raw
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            change(parse(&raw).unwrap(), &mut config).unwrap();
        }
        let profile = config.find("PI").unwrap();
        assert_eq!(config.default_agent_id.as_deref(), Some("pi"));
        assert_eq!(profile.label, "Pi");
        assert_eq!(profile.icon.as_deref(), Some("pi"));
        assert_eq!(profile.default_cwd.as_deref(), Some("/base"));
        assert_eq!(profile.argv, ["pi", "base"]);
        let remote = &profile.host_overrides["remote"];
        assert_eq!(remote.cwd.as_deref(), Some("/remote"));
        assert_eq!(remote.argv.as_deref().unwrap(), ["pi", "remote"]);
        assert_eq!(remote.env["A"], None);
        let raw = [
            "agent",
            "set",
            "pi",
            "--clear-host-override=remote",
            "--host-override=worker",
            "--env=B=2",
        ]
        .map(str::to_owned);
        change(parse(&raw).unwrap(), &mut config).unwrap();
        let overrides = &config.find("pi").unwrap().host_overrides;
        assert!(!overrides.contains_key("remote"));
        assert_eq!(overrides["worker"].env["B"].as_deref(), Some("2"));
        let raw = ["agent", "add", "PI", "--", "pi"].map(str::to_owned);
        assert_eq!(
            change(parse(&raw).unwrap(), &mut config).unwrap_err().code,
            "agent_exists"
        );
        for (raw, has_default, exists) in [
            ("agent rm pi --host-override worker", true, true),
            ("agent default --clear", false, true),
            ("agent default PI", true, true),
            ("agent rm pi", false, false),
        ] {
            let raw = raw
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            change(parse(&raw).unwrap(), &mut config).unwrap();
            assert_eq!(config.default_agent_id.is_some(), has_default);
            assert_eq!(config.find("pi").is_some(), exists);
            assert!(
                config
                    .find("pi")
                    .is_none_or(|p| p.host_overrides.is_empty())
            );
        }
    }
}
