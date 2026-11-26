use crate::{
    args::{Action, Args},
    config_path,
    output::{Error, Output, Result, cell, table},
};
use ddg_agents_proto::host::ExecutorSpec;
use ddg_agents_transport::{
    HostProfile, HostTransport,
    agent::{AgentConfigError, AgentHostOverride, AgentProfile, AgentsConfig, ConfigStamp},
};
use serde_json::json;
use std::{fmt::Write, path::PathBuf};

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
    let id = id.expect("profile ID required by parser");
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
        config.agents.push(AgentProfile {
            id: id.into(),
            label: id.into(),
            argv: std::mem::take(&mut args.argv),
            default_cwd: None,
            env: Default::default(),
            icon: None,
            host_overrides: Default::default(),
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

#[cfg(test)]
mod agent_and_run_logic {
    use super::*;
    use crate::args::parse;

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
