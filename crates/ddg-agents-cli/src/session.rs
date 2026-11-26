use crate::{
    CatalogSnapshot, agent,
    args::{Action, Args},
    attach, connect, dimensions,
    output::{Error, Output, Result, cell, tab_error, table},
    partial_errors, room_name, selected, snapshots, visible,
};
use ddg_agents_proto::{
    host::{SessionMoveTarget, SplitAxis, TabCreateParams, TabRuntimeState, TabSplitParams},
    id::encode_base64,
    terminal::{AgentState, BlockedKind, ForegroundHint},
};
use ddg_agents_transport::HostsConfig;
use serde::Serialize;
use serde_json::json;
use std::{
    collections::HashMap,
    io::{self, Read},
};

#[derive(Serialize)]
pub struct SessionRow<'a> {
    host: &'a str,
    session_id: &'a str,
    room_id: &'a str,
    room_name: &'a str,
    title: &'a str,
    state: &'static str,
    pid: Option<u32>,
    run_generation: u64,
    cwd: Option<&'a str>,
    agent_profile: Option<&'a str>,
    agent_state: Option<AgentState>,
    blocked_kind: Option<BlockedKind>,
    unseen: bool,
    foreground: Option<&'a ForegroundHint>,
}

pub fn rows<'a>(
    snapshot: &'a CatalogSnapshot,
    include_hidden: bool,
    room_id: Option<&str>,
) -> Vec<SessionRow<'a>> {
    let activities: HashMap<_, _> = snapshot
        .activities
        .iter()
        .map(|activity| {
            (
                (activity.tab_id.as_str(), activity.run_generation),
                activity,
            )
        })
        .collect();
    snapshot
        .catalog
        .rooms
        .iter()
        .filter(|room| visible(room, include_hidden) && room_id.is_none_or(|id| room.id == id))
        .flat_map(|room| {
            let activities = &activities;
            room.tabs.iter().map(move |tab| {
                let activity = activities.get(&(tab.id.as_str(), tab.run_generation));
                let status = activity.and_then(|a| a.agent_status.as_ref());
                let (state, pid) = match tab.runtime {
                    TabRuntimeState::Running { pid, .. } => ("running", Some(pid)),
                    TabRuntimeState::Exited { .. } => ("exited", None),
                    TabRuntimeState::StoppedAfterReboot => ("stopped_after_reboot", None),
                };
                SessionRow {
                    host: &snapshot.profile.id,
                    session_id: &tab.id,
                    room_id: &room.id,
                    room_name: room_name(room),
                    title: &tab.title,
                    state,
                    pid,
                    run_generation: tab.run_generation,
                    cwd: tab.last_cwd.as_deref().or(tab.space_cwd.as_deref()),
                    agent_profile: tab.agent_profile.as_deref(),
                    agent_state: status.map(|s| s.state),
                    blocked_kind: status.and_then(|s| s.kind),
                    unseen: activity.is_some_and(|a| a.activity_seq > a.seen_seq),
                    foreground: activity.and_then(|a| a.foreground.as_ref()),
                }
            })
        })
        .collect()
}

pub fn render(rows: &[SessionRow<'_>]) -> String {
    table(
        [
            "HOST",
            "SESSION_ID",
            "ROOM_ID",
            "ROOM",
            "TITLE",
            "STATE",
            "PID",
            "GEN",
            "CWD",
            "AGENT_PROFILE",
            "AGENT_STATE",
            "BLOCKED_KIND",
            "UNSEEN",
            "FOREGROUND",
        ],
        rows.iter().map(|s| {
            [
                s.host.into(),
                s.session_id.into(),
                s.room_id.into(),
                s.room_name.into(),
                s.title.into(),
                s.state.into(),
                s.pid.map_or_else(|| "-".into(), |pid| pid.to_string()),
                s.run_generation.to_string(),
                s.cwd.unwrap_or("-").into(),
                s.agent_profile.unwrap_or("-").into(),
                json!(s.agent_state).as_str().unwrap_or("-").into(),
                json!(s.blocked_kind).as_str().unwrap_or("-").into(),
                s.unseen.to_string(),
                s.foreground
                    .map_or("-", |fg| fg.script_hint.as_deref().unwrap_or(&fg.comm))
                    .into(),
            ]
        }),
    )
}

const MAX_INPUT: usize = 65_536;

fn input_data(mut input: impl Read, enter: bool) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take(MAX_INPUT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if enter {
        bytes.push(b'\r');
    }
    if bytes.len() > MAX_INPUT {
        return Err(Error::usage(
            "input must not exceed 65536 bytes including --enter",
        ));
    }
    if bytes.is_empty() {
        return Err(Error::usage(
            "input is empty; pass text, --stdin or --enter",
        ));
    }
    Ok(encode_base64(&bytes))
}

pub async fn run(mut args: Args<'_>, config: &HostsConfig, out: Output) -> Result {
    use Action::*;
    if args.action == SessionLs {
        let (snapshots, errors) = snapshots(&args, config).await?;
        let sessions = snapshots
            .iter()
            .flat_map(|s| rows(s, args.has("include-hidden"), args.value("room")))
            .collect::<Vec<_>>();
        partial_errors(out, &errors);
        return out.show(&json!({"sessions": sessions, "errors": errors}), || {
            render(&sessions)
        });
    }
    let host = selected(&args, config)?;
    let id = args.pos.first().copied().unwrap_or_default();
    match args.action {
        SessionCreate => {
            let (cols, rows) = dimensions(&args)?;
            let launch = agent::for_create(&mut args, host)?;
            let (client, _events) = connect(host).await?;
            let room = args.string("room");
            let result = client
                .tab_create_with_agent(TabCreateParams {
                    agent_profile: launch.agent_profile,
                    request_id: None,
                    room_id: room.clone(),
                    background: false,
                    space_cwd: launch.cwd,
                    title: args.string("title"),
                    executor: launch.executor,
                    cols,
                    rows,
                })
                .await?;
            out.show(&result, || {
                format!(
                    "{}\t{}\n",
                    cell(&result.tab.id),
                    room.as_deref().map_or_else(|| "background".into(), cell)
                )
            })
        }
        SessionSplit => {
            let (cols, rows) = dimensions(&args)?;
            let axis = if args.has("vertical") {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            };
            let launch = agent::for_create(&mut args, host)?;
            let (client, _events) = connect(host).await?;
            let result = client
                .tab_split_with_agent(TabSplitParams {
                    agent_profile: launch.agent_profile,
                    request_id: None,
                    anchor_tab_id: id.into(),
                    axis,
                    ratio_bp: args.number::<u16>("ratio")?.map(|ratio| ratio * 100),
                    space_cwd: launch.cwd,
                    title: args.string("title"),
                    executor: launch.executor,
                    cols,
                    rows,
                })
                .await
                .map_err(|e| tab_error(e, &host.id))?;
            out.show(&result, || {
                format!("{}\t{}\n", cell(&result.tab.id), cell(&result.room.id))
            })
        }
        SessionRead => {
            let lines = args.number("lines")?.unwrap_or(24);
            let max_bytes = args.number("max-bytes")?.unwrap_or(262_144);
            let (client, _events) = connect(host).await?;
            let result = client
                .tab_screen(id.into(), lines, max_bytes)
                .await
                .map_err(|e| tab_error(e, &host.id))?;
            out.show(&result, || result.text.clone())
        }
        SessionSend => {
            let generation = args.number("expect-gen")?;
            let enter = args.has("enter");
            let data = if args.has("stdin") {
                input_data(io::stdin().lock(), enter)?
            } else {
                input_data(args.pos.get(1).unwrap_or(&"").as_bytes(), enter)?
            };
            let (client, _events) = connect(host).await?;
            let result = client
                .tab_write(id.into(), data, generation)
                .await
                .map_err(|e| tab_error(e, &host.id))?;
            out.show(&result, || {
                format!("sent {} bytes to {}\n", result.byte_len, cell(id))
            })
        }
        SessionMove => {
            let target = match args.string("room") {
                Some(room_id) => SessionMoveTarget::ExistingRoom { room_id },
                None => SessionMoveTarget::NewRoom {
                    name: args.string("new-room"),
                },
            };
            let (client, _events) = connect(host).await?;
            let result = client
                .tab_move(id.into(), target)
                .await
                .map_err(|e| tab_error(e, &host.id))?;
            out.show(&result, || {
                format!(
                    "{}\t{}\n",
                    cell(&result.session.session_id),
                    cell(&result.session.room_id)
                )
            })
        }
        SessionRestart => {
            let (cols, rows) = dimensions(&args)?;
            let (client, _events) = connect(host).await?;
            let result = client.tab_restart(id.into(), cols, rows).await?;
            out.show(&result, || {
                format!(
                    "restarted {} (generation {})\n",
                    cell(id),
                    result.tab.run_generation
                )
            })
        }
        SessionClose => {
            let (client, _events) = connect(host).await?;
            let result = client.tab_close(id.into(), None).await?;
            out.show(&result, || format!("closed {}\n", cell(id)))
        }
        SessionAttach => {
            let history = args.number("history")?.unwrap_or(2000);
            let (client, events) = connect(host).await?;
            attach::run(
                &client,
                events,
                id,
                args.has("readonly"),
                args.has("force"),
                history,
                out,
            )
            .await
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_activity_for_the_current_run_only() {
        let snapshot = CatalogSnapshot {
            profile: ddg_agents_transport::HostProfile::local_default(),
            catalog: serde_json::from_value(json!({
                "schema_version": 2, "host_id": "host", "rooms": [{
                    "id": "rm_one", "kind": "standard", "visibility": "visible", "auto_name": "One",
                    "tabs": [{"id": "ses_pi", "run_generation": 3, "agent_profile": "pi", "runtime": {"state": "running", "pty_id": 1, "pid": 5}}]
                }]
            })).unwrap(),
            activities: serde_json::from_value(json!([{
                "tab_id": "ses_pi", "run_generation": 3, "activity_seq": 2, "seen_seq": 1,
                "agent_status": {"state": "blocked", "kind": "permission", "records": [], "status_ms": 0},
                "foreground": {"pid": 5, "comm": "node", "script_hint": "pi"}
            }])).unwrap(),
        };
        let sessions = rows(&snapshot, false, None);
        let json = serde_json::to_value(&sessions[0]).unwrap();
        assert_eq!(json["agent_profile"], "pi");
        assert_eq!(json["agent_state"], "blocked");
        assert_eq!(json["blocked_kind"], "permission");
        assert_eq!(json["unseen"], true);
        assert_eq!(json["foreground"]["script_hint"], "pi");
        assert!(render(&sessions).contains("\tpi\tblocked\tpermission\ttrue\tpi\n"));

        let mut snapshot = snapshot;
        snapshot.activities[0].run_generation = 2;
        let stale = rows(&snapshot, false, None);
        assert_eq!(stale[0].agent_profile, Some("pi"));
        assert_eq!(stale[0].agent_state, None);
        assert!(!stale[0].unseen);
        assert!(stale[0].foreground.is_none());
    }

    #[test]
    fn input_bounds_include_enter_and_reject_empty() {
        assert_eq!(input_data(&b"abc"[..], true).unwrap(), "YWJjDQ==");
        assert!(input_data(&[][..], false).is_err());
        assert!(input_data(&vec![b'x'; MAX_INPUT][..], false).is_ok());
        assert!(input_data(&vec![b'x'; MAX_INPUT][..], true).is_err());
        assert!(input_data(&vec![b'x'; MAX_INPUT + 1][..], false).is_err());
    }
}
