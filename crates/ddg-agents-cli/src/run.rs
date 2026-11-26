use crate::{
    agent,
    args::Args,
    attach, config_path, connect, dimensions, load_config,
    output::{Error, Output, Result, cell, tab_error},
    selected,
};
use ddg_agents_proto::host::{
    Catalog, RoomCreateParams, RoomVisibility, SessionMoveTarget, TabCreateParams,
};
use ddg_agents_transport::HostProfile;
use serde_json::json;
use std::io::{self, IsTerminal};

fn check_attach(background: bool, tty: bool, json: bool) -> Result {
    if !background && (!tty || json) {
        return Err(Error::usage("interactive attach requires a TTY; use --bg"));
    }
    Ok(())
}

fn caller_session(hello_host_id: &str, env: impl Fn(&str) -> Option<String>) -> Option<String> {
    if env("DDG_HOST_ID").as_deref() == Some(hello_host_id) {
        // DDG_ROOM_ID is immutable even when a background session is promoted.
        env("DDG_SESSION_ID").filter(|id| !id.is_empty())
    } else {
        None
    }
}

fn current_visible_room<'a>(catalog: &'a Catalog, session_id: &str) -> Option<&'a str> {
    catalog
        .rooms
        .iter()
        .find(|room| {
            room.visibility == RoomVisibility::Visible
                && room.tabs.iter().any(|tab| tab.id == session_id)
        })
        .map(|room| room.id.as_str())
}

pub async fn run(args: &Args<'_>, out: Output) -> Result {
    let background = args.has("bg");
    check_attach(
        background,
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        args.json,
    )?;
    let (cols, rows) = dimensions(args)?;
    let config = load_config(&config_path(args)?)?;
    let host: &HostProfile = selected(args, &config)?;
    let launch = agent::resolve(&agent::load(args)?, args.pos.first().copied(), args, host)?;
    let (client, events) = connect(host).await?;
    let mut room_id = args.string("room");
    if !background
        && !args.has("new-room")
        && room_id.is_none()
        && let Some(hello) = client.hello()
        && let Some(session_id) = caller_session(&hello.host_id, |key| std::env::var(key).ok())
    {
        let subscription = client.catalog_subscribe().await?;
        room_id = current_visible_room(&subscription.catalog, &session_id).map(str::to_owned);
        if let Some(channel) = subscription.channel {
            client.detach_channel(channel);
        }
    }
    let name = args.string("name");
    let title = args.string("title");
    let in_new_room = !background && room_id.is_none();
    let id = if in_new_room && title.is_none() {
        let room = client
            .room_create_with_agent(RoomCreateParams {
                agent_profile: launch.agent_profile,
                request_id: None,
                name,
                space_cwd: launch.cwd,
                executor: launch.executor,
                cols,
                rows,
            })
            .await?
            .room;
        let tab = room
            .tabs
            .into_iter()
            .next()
            .ok_or_else(|| Error::new("internal", "created room has no session"))?;
        tab.id
    } else {
        // shortcut: room_create cannot set the first tab's title; use atomic creation once the RPC can carry a title.
        let result = client
            .tab_create_with_agent(TabCreateParams {
                agent_profile: launch.agent_profile,
                request_id: None,
                room_id,
                background: false,
                space_cwd: launch.cwd,
                title,
                executor: launch.executor,
                cols,
                rows,
            })
            .await?;
        let id = result.tab.id;
        if in_new_room {
            client.tab_move(id.clone(), SessionMoveTarget::NewRoom { name }).await.map_err(|error| {
                let mut error = tab_error(error, &host.id);
                error.message.push_str(&format!("; could not confirm promotion of {} (check ddg session ls --include-hidden)", cell(&id)));
                error
            })?;
        }
        id
    };
    if background {
        return out.show(&json!({"session_id": id, "background": true}), || {
            format!("{}\tbackground\n", cell(&id))
        });
    }
    attach::run(&client, events, &id, false, false, 2000, out).await?;
    let config_hint = if args.config.is_some() {
        " (with the same --config)"
    } else {
        ""
    };
    eprintln!(
        "[ddg] detaching does not stop the session. If still running, reconnect: ddg -H {} session attach {}{config_hint}",
        cell(&host.id),
        cell(&id)
    );
    Ok(())
}

#[cfg(test)]
mod agent_and_run_logic {
    use super::*;
    use crate::args::parse;
    use ddg_agents_proto::host::ExecutorSpec;
    use ddg_agents_transport::agent::{AgentHostOverride, AgentProfile, AgentsConfig};

    #[test]
    fn launch_uses_default_and_only_profile_env_with_remote_cwd_precedence() {
        let mut config = AgentsConfig::default();
        let raw = ["run", "--bg", "--", "--fast"].map(str::to_owned);
        let args = parse(&raw).unwrap();
        let mut host = HostProfile::local_default();
        host.id = "remote".into();
        host.transport = ddg_agents_transport::HostTransport::DockerExec {
            container: "demo".into(),
            ssh_host: None,
            known_hosts_file: None,
            identity_file: None,
        };
        assert_eq!(
            agent::resolve(&config, None, &args, &host)
                .err()
                .unwrap()
                .code,
            "no_default_agent"
        );
        assert_eq!(
            agent::resolve(&config, Some("missing"), &args, &host)
                .err()
                .unwrap()
                .code,
            "agent_not_found"
        );
        config.default_agent_id = Some("pi".into());
        config.agents.push(AgentProfile {
            id: "pi".into(),
            label: "Pi".into(),
            argv: vec!["pi".into()],
            default_cwd: None,
            env: [("KEEP".into(), "profile".into())].into(),
            icon: None,
            host_overrides: Default::default(),
        });
        let launch = agent::resolve(&config, None, &args, &host).unwrap();
        assert_eq!(launch.agent_profile.as_deref(), Some("pi"));
        assert_eq!(launch.cwd, None);
        // An exact match rejects any inherited env, including OPENAI_API_KEY and DDG_SESSION_TOKEN.
        assert_eq!(
            launch.executor,
            ExecutorSpec::Command {
                argv: vec!["pi".into(), "--fast".into()],
                env: [("KEEP".into(), "profile".into())].into(),
            }
        );
        let local = HostProfile::local_default();
        assert_eq!(
            agent::resolve(&config, None, &args, &local).unwrap().cwd,
            Some(
                std::env::current_dir()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
        config.agents[0].default_cwd = Some("/base".into());
        assert_eq!(
            agent::resolve(&config, None, &args, &host)
                .unwrap()
                .cwd
                .as_deref(),
            Some("/base")
        );
        config.agents[0].host_overrides.insert(
            "remote".into(),
            AgentHostOverride {
                argv: Some(vec!["remote-pi".into()]),
                cwd: Some("/override".into()),
                env: [("KEEP".into(), None)].into(),
            },
        );
        assert_eq!(
            agent::resolve(&config, None, &args, &host)
                .unwrap()
                .cwd
                .as_deref(),
            Some("/override")
        );
        let raw = ["run", "PI", "--bg", "--cwd=/explicit", "--", "--fast"].map(str::to_owned);
        let launch = agent::resolve(&config, Some("PI"), &parse(&raw).unwrap(), &host).unwrap();
        assert_eq!(launch.cwd.as_deref(), Some("/explicit"));
        assert_eq!(
            launch.executor,
            ExecutorSpec::Command {
                argv: vec!["remote-pi".into(), "--fast".into()],
                env: Default::default(),
            }
        );
    }

    #[test]
    fn uses_live_visible_room_for_promoted_session_and_not_a_foreign_host() {
        use serde_json::json;
        let session = "ses_aaaaaaaaaaaaaaaaaaaaaaaaaa";
        let env = |key: &str| match key {
            "DDG_HOST_ID" => Some("live".into()),
            "DDG_SESSION_ID" => Some(session.into()),
            "DDG_ROOM_ID" => Some("rm_bg_original".into()),
            _ => None,
        };
        let mut catalog = Catalog::empty("live");
        catalog.rooms.push(serde_json::from_value(json!({
            "id": "rm_promoted", "kind": "standard", "visibility": "visible", "auto_name": "live",
            "tabs": [{ "id": session, "run_generation": 1, "runtime": {"state": "running", "pty_id": 1, "pid": 2} }],
        })).unwrap());
        let caller = caller_session("live", env).unwrap();
        assert_eq!(current_visible_room(&catalog, &caller), Some("rm_promoted"));
        assert_eq!(caller_session("other", env), None);
        assert_eq!(current_visible_room(&catalog, "ses_missing"), None);
        catalog.rooms[0].visibility = RoomVisibility::Hidden;
        assert_eq!(current_visible_room(&catalog, &caller), None);
    }

    #[test]
    fn interactive_run_requires_tty_non_json_unless_background() {
        for (bg, tty, json) in [
            (false, false, false),
            (false, true, true),
            (false, false, true),
        ] {
            let error = check_attach(bg, tty, json).unwrap_err();
            assert_eq!(error.code, "invalid_params");
            assert_eq!(error.message, "interactive attach requires a TTY; use --bg");
        }
        assert!(check_attach(false, true, false).is_ok());
        assert!(check_attach(true, false, true).is_ok());
    }
}
