mod agent;
mod args;
mod attach;
mod machine;
mod msg;
mod output;
mod room;
mod run;
mod session;
mod update;
mod wait;

use args::{Action, Args};
use ddg_agents_proto::host::{
    Catalog, RoomRecord, RoomVisibility, TabActivity, TabRecord, TabRuntimeState,
};
use ddg_agents_transport::{
    DialError, DialErrorKind, HostClient, HostEvent, HostProfile, HostsConfig,
};
use output::{Error, Output, Result, cell, table};
use serde::Serialize;
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinSet};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const SKILL: &str = include_str!("SKILL.md");

fn main() -> ExitCode {
    let raw = std::env::args_os().skip(1).collect::<Vec<_>>();
    if raw.len() == 1 && (raw[0] == "--version" || raw[0] == "-V") {
        println!("ddg {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let json = raw
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");
    let update = {
        let mut words = raw.iter().map(|arg| arg.to_string_lossy());
        loop {
            let Some(word) = words.next() else {
                break false;
            };
            match word.as_ref() {
                "--" => break false,
                "-H" | "--host" | "--config" | "--token" => {
                    words.next();
                }
                flag if flag.starts_with('-') => {}
                command => break matches!(command, "update" | "__update-apply-helper"),
            }
        }
    };
    let raw = raw
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| Error::usage("arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>>>();
    match raw.and_then(|raw| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(run(&raw))
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            Output(json).error(&error, update);
            ExitCode::FAILURE
        }
    }
}

async fn run(raw: &[String]) -> Result {
    let args = args::parse(raw)?;
    let out = Output(args.json);
    match args.action {
        Action::Help => return out.show(&json!({"help": args::HELP}), || args::HELP.into()),
        Action::Version => {
            let version = env!("CARGO_PKG_VERSION");
            return out.show(&json!({"version": version}), || format!("ddg {version}\n"));
        }
        Action::Skill => {
            return out.show(
                &json!({"skill": SKILL, "version": env!("CARGO_PKG_VERSION")}),
                || SKILL.into(),
            );
        }
        Action::SkillInstall => {
            let dir = match args.value("dir") {
                Some(dir) => PathBuf::from(dir),
                None => PathBuf::from(
                    std::env::var_os("HOME")
                        .filter(|home| !home.is_empty())
                        .ok_or_else(|| Error::usage("set HOME or pass --dir"))?,
                )
                .join(".agents/skills/ddg"),
            };
            let path = install_skill(&dir)?;
            let path = path.display();
            return out.show(
                &json!({"path": path.to_string(), "bytes": SKILL.len(), "version": env!("CARGO_PKG_VERSION")}),
                || format!("installed {path}\n"),
            );
        }
        Action::MsgSend | Action::MsgRecv | Action::MsgSubscribe => {
            return msg::run(&args, out).await;
        }
        Action::AgentLs
        | Action::AgentShow
        | Action::AgentAdd
        | Action::AgentSet
        | Action::AgentDefault
        | Action::AgentRm => return agent::run(args, out),
        Action::Run => return run::run(&args, out).await,
        Action::UpdateStatus
        | Action::UpdateCheck
        | Action::UpdateApply
        | Action::UpdateRollback
        | Action::UpdateConfig
        | Action::UpdateApplyHelper => return update::run(&args, out).await,
        _ => {}
    }
    let path = config_path(&args)?;
    let config = load_config(&path)?;
    use Action::*;
    match args.action {
        Overview => overview(&args, &config, out).await,
        MachineLs | MachineAdd | MachineRm | MachineDefault | MachineTest | MachineInstall
        | MachineDiscover => machine::run(&args, config, &path, out).await,
        RoomLs | RoomCreate | RoomRename | RoomClose => room::run(args, &config, out).await,
        SessionLs | SessionCreate | SessionSplit | SessionRead | SessionSend | SessionMove
        | SessionRestart | SessionClose | SessionAttach => session::run(args, &config, out).await,
        SessionWait => wait::run(&args, &config, out).await,
        Help | Version | Skill | SkillInstall | MsgSend | MsgRecv | MsgSubscribe | Run
        | AgentLs | AgentShow | AgentAdd | AgentSet | AgentDefault | AgentRm | UpdateStatus
        | UpdateCheck | UpdateApply | UpdateRollback | UpdateConfig | UpdateApplyHelper => {
            unreachable!()
        }
    }
}

fn install_skill(dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("SKILL.md");
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    for n in 0.. {
        let tmp = dir.join(format!(".SKILL.md.tmp.{}.{n}", std::process::id()));
        let mut file = match options.open(&tmp) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let result = (|| {
            file.write_all(SKILL.as_bytes())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o644))?;
            }
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return result.map(|()| path);
    }
    unreachable!()
}

fn config_path(args: &Args<'_>) -> Result<PathBuf> {
    Ok(match &args.config {
        Some(path) if path.is_relative() => std::env::current_dir()?.join(path),
        Some(path) => path.clone(),
        None => HostsConfig::default_path()?,
    })
}

fn load_config(path: &Path) -> Result<HostsConfig> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HostsConfig::default()),
        Err(e) => return Err(e.into()),
    };
    let config: HostsConfig = serde_json::from_slice(&bytes)
        .map_err(|_| Error::new("invalid_config", "could not parse hosts.json"))?;
    config.validate().map_err(|_| {
        Error::new(
            "invalid_config",
            "hosts.json contains invalid or duplicate profiles",
        )
    })?;
    if config.find(&config.default_host_id).is_none() {
        return Err(Error::new(
            "invalid_config",
            "hosts.json default_host_id must name an existing profile",
        ));
    }
    Ok(config)
}

fn profile<'a>(config: &'a HostsConfig, id: &str) -> Result<&'a HostProfile> {
    config.find(id).ok_or_else(|| {
        Error::new(
            "host_not_found",
            format!("unknown machine profile: {}", cell(id)),
        )
    })
}

fn selected<'a>(args: &Args<'_>, config: &'a HostsConfig) -> Result<&'a HostProfile> {
    profile(config, args.host.unwrap_or(&config.default_host_id))
}

async fn connect(
    profile: &HostProfile,
) -> std::result::Result<(Arc<HostClient>, mpsc::UnboundedReceiver<HostEvent>), DialError> {
    tokio::time::timeout(
        CONNECT_TIMEOUT,
        HostClient::connect_once(profile.clone(), "ddg-cli"),
    )
    .await
    .map_err(|_| DialError::new(DialErrorKind::Unavailable, "connection timed out"))?
}

#[derive(Serialize)]
struct HostError {
    host: String,
    status: &'static str,
    error: Error,
}

struct CatalogSnapshot {
    profile: HostProfile,
    catalog: Catalog,
    activities: Vec<TabActivity>,
}

async fn snapshot(profile: HostProfile) -> std::result::Result<CatalogSnapshot, HostError> {
    let host = profile.id.clone();
    let (client, _events) = connect(&profile).await.map_err(|error| HostError {
        host: host.clone(),
        status: output::dial_status(error.kind),
        error: error.into(),
    })?;
    let result = client.catalog_subscribe().await;
    client.close();
    let result = result.map_err(|error| HostError {
        host,
        status: "unavailable",
        error: error.into(),
    })?;
    Ok(CatalogSnapshot {
        profile,
        catalog: result.catalog,
        activities: result.activities,
    })
}

async fn snapshots(
    args: &Args<'_>,
    config: &HostsConfig,
) -> Result<(Vec<CatalogSnapshot>, Vec<HostError>)> {
    let all = args.host.map_or(args.action == Action::Overview, |id| {
        id.trim().eq_ignore_ascii_case("all")
    });
    if !all {
        return snapshot(selected(args, config)?.clone())
            .await
            .map(|value| (vec![value], Vec::new()))
            .map_err(|error| error.error);
    }
    let mut jobs = JoinSet::new();
    for (idx, profile) in config.hosts.iter().cloned().enumerate() {
        jobs.spawn(async move { (idx, snapshot(profile).await) });
    }
    let results = ordered(jobs).await?;
    let (mut snapshots, mut errors) = (Vec::new(), Vec::new());
    for result in results {
        match result {
            Ok(value) => snapshots.push(value),
            Err(error) => errors.push(error),
        }
    }
    Ok((snapshots, errors))
}

async fn ordered<T: Send + 'static>(mut jobs: JoinSet<(usize, T)>) -> Result<Vec<T>> {
    let mut results = Vec::new();
    while let Some(job) = jobs.join_next().await {
        results.push(job.map_err(|_| Error::new("internal", "host task did not complete"))?);
    }
    results.sort_unstable_by_key(|(index, _)| *index);
    Ok(results.into_iter().map(|(_, value)| value).collect())
}

fn partial_errors(out: Output, errors: &[HostError]) {
    if !out.0 {
        for e in errors {
            eprintln!(
                "ddg: {}: {} ({})",
                output::diagnostic(&e.host),
                e.error.message,
                e.status
            );
        }
    }
}

fn running(tabs: &[TabRecord]) -> usize {
    tabs.iter()
        .filter(|tab| matches!(tab.runtime, TabRuntimeState::Running { .. }))
        .count()
}

fn room_name(room: &RoomRecord) -> &str {
    room.name.as_deref().unwrap_or(&room.auto_name)
}

async fn overview(args: &Args<'_>, config: &HostsConfig, out: Output) -> Result {
    let (snapshots, errors) = snapshots(args, config).await?;
    let mut hosts = Vec::new();
    let mut rooms = Vec::new();
    let mut sessions = Vec::new();
    let mut host_rows = Vec::new();
    for snapshot in &snapshots {
        let host_rooms = room::rows(snapshot, false);
        let host_sessions = session::rows(snapshot, false, None);
        let count = host_rooms.len();
        let running = host_rooms.iter().map(|r| r.running).sum::<usize>();
        host_rows.push([
            snapshot.profile.id.clone(),
            snapshot.profile.label.clone(),
            "connected".into(),
            snapshot.catalog.host_id.clone(),
            count.to_string(),
            running.to_string(),
        ]);
        hosts.push(json!({
            "host": snapshot.profile.id,
            "label": snapshot.profile.label,
            "status": "connected",
            "host_id": snapshot.catalog.host_id,
            "room_count": count,
            "running": running,
            "rooms": host_rooms,
            "sessions": host_sessions,
        }));
        rooms.extend(host_rooms);
        sessions.extend(host_sessions);
    }
    for e in &errors {
        host_rows.push([
            e.host.clone(),
            "-".into(),
            e.status.into(),
            "-".into(),
            "-".into(),
            "-".into(),
        ]);
    }
    partial_errors(out, &errors);
    out.show(&json!({"hosts": hosts, "errors": errors}), || {
        let mut text = table(
            ["HOST", "LABEL", "STATUS", "HOST_ID", "ROOMS", "RUNNING"],
            host_rows,
        );
        text.push_str(&room::render(&rooms));
        text.push_str(&session::render(&sessions));
        text
    })
}

fn dimensions(args: &Args<'_>) -> Result<(u16, u16)> {
    let (cols, rows) = attach::terminal_size();
    let cols = args.number::<u16>("cols")?.unwrap_or(cols);
    let rows = args.number::<u16>("rows")?.unwrap_or(rows);
    if cols == 0 || rows == 0 {
        return Err(Error::usage("cols and rows must be positive"));
    }
    Ok((cols, rows))
}

fn visible(room: &RoomRecord, include_hidden: bool) -> bool {
    include_hidden || room.visibility == RoomVisibility::Visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_frontmatter_and_operator_commands() {
        let skill = SKILL.replace("\r\n", "\n");
        let (meta, body) = skill
            .strip_prefix("---\n")
            .unwrap()
            .split_once("\n---\n")
            .unwrap();
        assert_eq!(
            meta,
            "name: ddg\ndescription: Manage local and remote DDG machines, rooms, persistent terminal sessions, and inter-session messaging via the ddg operator CLI and in-session ddg-agents-host bridge CLI."
        );
        for command in [
            "ls",
            "status",
            "run",
            "agent ls",
            "agent show",
            "agent add",
            "agent set",
            "agent default",
            "agent rm",
            "machine ls",
            "machine add",
            "machine install",
            "machine test",
            "machine default",
            "machine rm",
            "machine discover",
            "room ls",
            "room create",
            "room rename",
            "room close",
            "session ls",
            "session create",
            "session split",
            "session read",
            "session send",
            "session move",
            "session restart",
            "session close",
            "session attach",
            "session wait",
            "msg send",
            "msg recv",
            "msg subscribe",
            "update status",
            "update check",
            "update apply",
            "update rollback",
            "update config",
        ] {
            assert!(
                body.contains(&format!("`ddg {command}")),
                "missing {command}"
            );
        }
        let examples = body
            .split_once("```sh\n")
            .unwrap()
            .1
            .split_once("```")
            .unwrap()
            .0;
        for line in examples.lines() {
            let args = line
                .strip_prefix("ddg ")
                .unwrap()
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            args::parse(&args).unwrap_or_else(|err| panic!("{line}: {}", err.message));
        }
    }
}
