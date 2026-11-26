use crate::{
    CatalogSnapshot, agent,
    args::{Action, Args},
    connect, dimensions,
    output::{Error, Output, Result, cell, table},
    partial_errors, room_name, running, selected, snapshots, visible,
};
use ddg_agents_proto::host::{RoomCreateParams, RoomKind, RoomVisibility};
use ddg_agents_transport::HostsConfig;
use serde::Serialize;
use serde_json::json;

#[derive(Serialize)]
pub struct RoomRow<'a> {
    host: &'a str,
    room_id: &'a str,
    name: &'a str,
    auto_name: &'a str,
    visibility: RoomVisibility,
    tabs: usize,
    pub running: usize,
}

pub fn rows(snapshot: &CatalogSnapshot, include_hidden: bool) -> Vec<RoomRow<'_>> {
    snapshot
        .catalog
        .rooms
        .iter()
        .filter(|room| visible(room, include_hidden))
        .map(|room| RoomRow {
            host: &snapshot.profile.id,
            room_id: &room.id,
            name: room_name(room),
            auto_name: &room.auto_name,
            visibility: room.visibility,
            tabs: room.tabs.len(),
            running: running(&room.tabs),
        })
        .collect()
}

pub fn render(rows: &[RoomRow<'_>]) -> String {
    table(
        ["HOST", "ROOM_ID", "NAME", "VISIBILITY", "TABS", "RUNNING"],
        rows.iter().map(|r| {
            [
                r.host.into(),
                r.room_id.into(),
                r.name.into(),
                match r.visibility {
                    RoomVisibility::Visible => "visible",
                    RoomVisibility::Hidden => "hidden",
                }
                .into(),
                r.tabs.to_string(),
                r.running.to_string(),
            ]
        }),
    )
}

pub async fn run(mut args: Args<'_>, config: &HostsConfig, out: Output) -> Result {
    if args.action == Action::RoomLs {
        let (snapshots, errors) = snapshots(&args, config).await?;
        let rooms = snapshots
            .iter()
            .flat_map(|s| rows(s, args.has("include-hidden")))
            .collect::<Vec<_>>();
        partial_errors(out, &errors);
        return out.show(&json!({"rooms": rooms, "errors": errors}), || {
            render(&rooms)
        });
    }
    let profile = selected(&args, config)?;
    let create = if args.action == Action::RoomCreate {
        Some((dimensions(&args)?, agent::for_create(&mut args, profile)?))
    } else {
        None
    };
    let (client, _events) = connect(profile).await?;
    match args.action {
        Action::RoomCreate => {
            let ((cols, rows), launch) = create.unwrap();
            let result = client
                .room_create_with_agent(RoomCreateParams {
                    agent_profile: launch.agent_profile,
                    request_id: None,
                    name: args.string("name"),
                    space_cwd: launch.cwd,
                    executor: launch.executor,
                    cols,
                    rows,
                })
                .await?;
            out.show(&result, || {
                format!(
                    "{}\t{}\n",
                    cell(&result.room.id),
                    result
                        .room
                        .tabs
                        .first()
                        .map_or_else(|| "-".into(), |tab| cell(&tab.id))
                )
            })
        }
        Action::RoomRename => {
            let result = client
                .room_rename(args.pos[0].into(), args.pos.get(1).map(|s| (*s).into()))
                .await?;
            out.show(&result, || {
                format!(
                    "{}\t{}\n",
                    cell(&result.room.id),
                    cell(room_name(&result.room))
                )
            })
        }
        Action::RoomClose => {
            let id = args.pos[0];
            let catalog = client.catalog_subscribe().await?.catalog;
            let room = catalog
                .rooms
                .into_iter()
                .find(|r| r.id == id)
                .ok_or_else(|| Error::new("room_not_found", "room not found"))?;
            if room.kind == RoomKind::Background || id.starts_with("rm_bg_") {
                return Err(Error::new(
                    "permission_denied",
                    "cannot close the background room",
                ));
            }
            if room.tabs.len() > 1 && !args.has("yes") {
                return Err(Error::usage(
                    "room has multiple tabs; pass --yes to close all of them",
                ));
            }
            let mut closed = Vec::new();
            for tab in room.tabs {
                match client.tab_close(tab.id.clone(), None).await {
                    Ok(_) => closed.push(tab.id),
                    Err(error) => {
                        let mut error = Error::from(error);
                        error.message = format!(
                            "closed {} tab(s) before {} failed: {}",
                            closed.len(),
                            cell(&tab.id),
                            error.message
                        );
                        return Err(error);
                    }
                }
            }
            out.show(&json!({"room_id": id, "closed": closed}), || {
                format!("closed {} ({} tabs)\n", cell(id), closed.len())
            })
        }
        _ => unreachable!(),
    }
}
