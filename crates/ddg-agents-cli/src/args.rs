use crate::output::{Error, Result};
use ddg_agents_proto::host::{MAX_SPLIT_RATIO_BP, MIN_SPLIT_RATIO_BP};
use std::{collections::HashMap, path::PathBuf, str::FromStr};

pub const HELP: &str = "Usage: ddg [--json] [-H|--host <id>] [--config <path>] <command>

Commands:
  ls | status                                      Overview (defaults to all hosts)
  skill [install [--dir <path>]]                    Print or install the agent manual
  run [agent] [--room <room_id> | --new-room [--name <name>] | --bg | --background]
              [--cwd <dir>] [--title <title>] [--cols <n>] [--rows <n>] [-- <args...>]
  agent (a) ls | show <id> [--reveal-env]
  agent add | set <id> [--label <label>] [--icon <icon>] [--cwd <dir>] [--default]
                     [--env KEY=VALUE] [--unset-env KEY] [--host-override <host>]
                     [--clear-host-override <host>] [-- <argv...>]
  agent default (<id> | --clear)
  agent rm <id> [--host-override <host>]
  machine (m) ls [--probe]
  machine add <target-or-quoted-ssh-command> [options]
  machine add --docker <container> [--ssh-host <host>] [options]
  machine add --container-ssh <user@host:port> [options]
    Add options: --id <id> --label <name> --port <n> --identity <path>
                 --known-hosts <path> --install --no-test
  machine rm | default | test | install <id>
  machine discover
  room (r) ls [--include-hidden]
  room create [--name <name>] [--agent <id>] [--cwd <dir>] [--cols <n>] [--rows <n>] [-- <cmd-or-args...>]
  room rename <room_id> (<name> | --clear)
  room close <room_id> [--yes]                       --yes required for multiple tabs
  session (s) ls [--room <room_id>] [--include-hidden]
  session create [--room <room_id> | --background] [--title <title>]
                 [--agent <id>] [--cwd <dir>] [--cols <n>] [--rows <n>] [-- <cmd-or-args...>]
  session split <session_id> [--horizontal | --vertical] [--ratio <15..85>]
                 [--agent <id>] [--cwd <dir>] [--title <title>] [--cols <n>] [--rows <n>] [-- <cmd-or-args...>]
  session read <session_id> [--lines <n>] [--max-bytes <n>]
  session send <session_id> [text | --stdin] [--enter] [--expect-gen <n>]
  session move <session_id> (--room <room_id> | --new-room [name])
  session restart <session_id> [--cols <n>] [--rows <n>]
  session close <session_id>
  session attach <session_id> [--readonly] [--force] [--history <n>]
  session wait <session_id> [--output <pattern>] [--timeout <seconds>] [--or-message]
  msg (inbox) send (<session_id> | --to <session_id>) (text | --stdin)
  msg recv [--max <n>] [--wait-ms <ms>]              Current session inbox only
  msg subscribe                                    Stream the current session inbox
  update status | check                           Defaults to local; accepts -H <id> or -H all
  update apply [--channel <stable|prerelease>] [--version <tag>]
               [--allow-downgrade] [--restart-sessions] [--force] [--yes]
  update rollback [--restart-sessions] [--yes]
  update config [--auto-check <on|off>] [--channel <stable|prerelease>]
                [--repo <owner/repo> | --clear-repo]
    Apply/rollback default to local and require one target; config does not accept -H.

Global options may appear before or after subcommands. --host all works with
ls, status, room ls, session ls, update status and update check. Non-update host
queries otherwise use the configured default.
Attach sends TTY input; Ctrl-] detaches. With --json, attach emits newline-delimited
JSON records with base64 terminal data. Use -h/--help or -V/--version.

Agent? Run: ddg --skill
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Help,
    Version,
    Skill,
    SkillInstall,
    Overview,
    Run,
    AgentLs,
    AgentShow,
    AgentAdd,
    AgentSet,
    AgentDefault,
    AgentRm,
    MachineLs,
    MachineAdd,
    MachineRm,
    MachineDefault,
    MachineTest,
    MachineInstall,
    MachineDiscover,
    RoomLs,
    RoomCreate,
    RoomRename,
    RoomClose,
    SessionLs,
    SessionCreate,
    SessionSplit,
    SessionRead,
    SessionSend,
    SessionMove,
    SessionRestart,
    SessionClose,
    SessionAttach,
    SessionWait,
    MsgSend,
    MsgRecv,
    MsgSubscribe,
    UpdateStatus,
    UpdateCheck,
    UpdateApply,
    UpdateRollback,
    UpdateConfig,
    UpdateApplyHelper,
}

#[derive(Debug)]
pub struct Args<'a> {
    pub action: Action,
    pub json: bool,
    pub host: Option<&'a str>,
    pub config: Option<PathBuf>,
    pub pos: Vec<&'a str>,
    pub argv: Vec<String>,
    flags: HashMap<&'a str, Option<&'a str>>,
    multi_flags: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> Args<'a> {
    pub fn has(&self, flag: &str) -> bool {
        self.flags.contains_key(flag)
    }

    pub fn value(&self, flag: &str) -> Option<&'a str> {
        self.flags.get(flag).copied().flatten()
    }

    pub fn string(&self, flag: &str) -> Option<String> {
        self.value(flag).map(str::to_owned)
    }

    pub fn values(&self, flag: &str) -> &[&'a str] {
        self.multi_flags.get(flag).map_or(&[], Vec::as_slice)
    }

    pub fn number<T: FromStr>(&self, flag: &str) -> Result<Option<T>> {
        self.value(flag)
            .map(|v| {
                v.parse()
                    .map_err(|_| Error::usage(format!("invalid number for --{flag}")))
            })
            .transpose()
    }
}

pub fn parse(raw: &[String]) -> Result<Args<'_>> {
    let mut words = Vec::new();
    let mut pass = raw.iter().map(String::as_str).peekable();
    let (mut json, mut host, mut config, mut help) = (false, None, None, false);
    let mut skill = false;
    let mut tail = None;
    // Check independently of option ordering; never echo a supplied token.
    if pass
        .clone()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--token" || arg.starts_with("--token="))
    {
        return Err(Error::usage("--token is not allowed for operator commands"));
    }
    while let Some(arg) = pass.next() {
        let (key, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(k, v)| (k, Some(v)));
        match (key, inline) {
            ("--", None) => {
                tail = Some(pass.collect::<Vec<_>>());
                break;
            }
            ("--json", None) => json = true,
            ("-h" | "--help", None) => help = true,
            ("--skill" | "--skills", None) => skill = true,
            ("-H" | "--host" | "--config", _) => {
                let value = inline
                    .or_else(|| pass.next_if(|arg| !arg.starts_with('-')))
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::usage(format!("{key} requires a value")))?;
                let slot = if key == "--config" {
                    &mut config
                } else {
                    &mut host
                };
                if slot.replace(value).is_some() {
                    return Err(Error::usage(format!("duplicate {key}")));
                }
            }
            _ => words.push(arg),
        }
    }

    use Action::*;
    let version = words.contains(&"-V")
        || (!words.starts_with(&["update", "apply"]) && words.contains(&"--version"));
    let (action, rest) = if help {
        (Help, &[][..])
    } else if version {
        (Version, &[][..])
    } else if skill {
        (Skill, &[][..])
    } else {
        match words.as_slice() {
            [] if tail.is_none() => (Help, &[][..]),
            ["skill"] => (Skill, &[][..]),
            ["skill", "install", rest @ ..] => (SkillInstall, rest),
            ["ls" | "status", rest @ ..] => (Overview, rest),
            ["run", rest @ ..] => (Run, rest),
            ["update", sub, rest @ ..] => (
                match *sub {
                    "status" => UpdateStatus,
                    "check" => UpdateCheck,
                    "apply" => UpdateApply,
                    "rollback" => UpdateRollback,
                    "config" => UpdateConfig,
                    _ => return Err(Error::usage("unknown update command; see ddg --help")),
                },
                rest,
            ),
            ["__update-apply-helper", rest @ ..] => (UpdateApplyHelper, rest),
            ["agent" | "a", sub, rest @ ..] => (
                match *sub {
                    "ls" => AgentLs,
                    "show" => AgentShow,
                    "add" => AgentAdd,
                    "set" => AgentSet,
                    "default" => AgentDefault,
                    "rm" => AgentRm,
                    _ => return Err(Error::usage("unknown agent command; see ddg --help")),
                },
                rest,
            ),
            ["machine" | "m", sub, rest @ ..] => (
                match *sub {
                    "ls" => MachineLs,
                    "add" => MachineAdd,
                    "rm" => MachineRm,
                    "default" => MachineDefault,
                    "test" => MachineTest,
                    "install" => MachineInstall,
                    "discover" => MachineDiscover,
                    _ => return Err(Error::usage("unknown machine command; see ddg --help")),
                },
                rest,
            ),
            ["room" | "r", sub, rest @ ..] => (
                match *sub {
                    "ls" => RoomLs,
                    "create" => RoomCreate,
                    "rename" => RoomRename,
                    "close" => RoomClose,
                    _ => return Err(Error::usage("unknown room command; see ddg --help")),
                },
                rest,
            ),
            ["session" | "s", sub, rest @ ..] => (
                match *sub {
                    "ls" => SessionLs,
                    "create" => SessionCreate,
                    "split" => SessionSplit,
                    "read" => SessionRead,
                    "send" => SessionSend,
                    "move" => SessionMove,
                    "restart" => SessionRestart,
                    "close" => SessionClose,
                    "attach" => SessionAttach,
                    "wait" => SessionWait,
                    _ => return Err(Error::usage("unknown session command; see ddg --help")),
                },
                rest,
            ),
            ["msg" | "inbox", sub, rest @ ..] => (
                match *sub {
                    "send" => MsgSend,
                    "recv" => MsgRecv,
                    "subscribe" => MsgSubscribe,
                    _ => return Err(Error::usage("unknown msg command; see ddg --help")),
                },
                rest,
            ),
            _ => return Err(Error::usage("expected a command; see ddg --help")),
        }
    };

    if host.is_some_and(|id| id.trim().eq_ignore_ascii_case("all"))
        && !matches!(
            action,
            Overview | RoomLs | SessionLs | UpdateStatus | UpdateCheck | Help | Version | Skill
        )
    {
        return Err(Error::usage(
            "--host all is only supported on ls, status, room ls, session ls, update status and update check",
        ));
    }

    let (values, switches, min, max): (&[&str], &[&str], usize, usize) = match action {
        SkillInstall => (&["dir"], &[], 0, 0),
        UpdateApply => (
            &["channel", "version"],
            &["allow-downgrade", "restart-sessions", "force", "yes"],
            0,
            0,
        ),
        UpdateRollback => (&[], &["restart-sessions", "yes"], 0, 0),
        UpdateConfig => (&["auto-check", "channel", "repo"], &["clear-repo"], 0, 0),
        UpdateApplyHelper => (
            &["job", "state-dir", "parent-pid"],
            &["restart-sessions", "relaunch"],
            0,
            0,
        ),
        Run => (
            &["room", "name", "cwd", "title", "cols", "rows"],
            &["new-room", "bg"],
            0,
            1,
        ),
        AgentShow => (&[], &["reveal-env"], 1, 1),
        AgentAdd | AgentSet => (
            &[
                "label",
                "icon",
                "cwd",
                "env",
                "unset-env",
                "host-override",
                "clear-host-override",
            ],
            &["default"],
            1,
            1,
        ),
        AgentDefault => (&[], &["clear"], 0, 1),
        AgentRm => (&["host-override"], &[], 1, 1),
        MachineLs => (&[], &["probe"], 0, 0),
        MachineAdd => (
            &[
                "docker",
                "ssh-host",
                "container-ssh",
                "id",
                "label",
                "port",
                "identity",
                "known-hosts",
            ],
            &["install", "no-test"],
            0,
            1,
        ),
        MachineRm | MachineDefault | MachineTest | MachineInstall | SessionClose => {
            (&[], &[], 1, 1)
        }
        RoomLs => (&[], &["include-hidden"], 0, 0),
        RoomCreate => (&["name", "cwd", "cols", "rows", "agent"], &[], 0, 0),
        RoomRename => (&[], &["clear"], 1, 2),
        RoomClose => (&[], &["yes"], 1, 1),
        SessionLs => (&["room"], &["include-hidden"], 0, 0),
        SessionCreate => (
            &["room", "cwd", "title", "cols", "rows", "agent"],
            &["background"],
            0,
            0,
        ),
        SessionSplit => (
            &["ratio", "cwd", "title", "cols", "rows", "agent"],
            &["horizontal", "vertical"],
            1,
            1,
        ),
        SessionRead => (&["lines", "max-bytes"], &[], 1, 1),
        SessionSend => (&["expect-gen"], &["stdin", "enter"], 1, 2),
        SessionMove => (&["room", "new-room"], &[], 1, 1),
        SessionRestart => (&["cols", "rows"], &[], 1, 1),
        SessionAttach => (&["history"], &["readonly", "force"], 1, 1),
        SessionWait => (&["output", "timeout"], &["or-message"], 1, 1),
        MsgSend => (&["to"], &["stdin"], 0, 2),
        MsgRecv => (&["max", "wait-ms"], &[], 0, 0),
        _ => (&[], &[], 0, 0),
    };

    let mut args = Args {
        action,
        json,
        host,
        config: config.map(PathBuf::from),
        pos: Vec::new(),
        argv: Vec::new(),
        flags: HashMap::new(),
        multi_flags: HashMap::new(),
    };
    if matches!(action, Help | Version) || skill {
        return Ok(args);
    }
    let mut rest = rest.iter().copied().peekable();
    while let Some(arg) = rest.next() {
        if let Some(option) = arg.strip_prefix("--") {
            let (key, inline) = option
                .split_once('=')
                .map_or((option, None), |(k, v)| (k, Some(v)));
            let key = if action == Run && key == "background" {
                "bg"
            } else {
                key
            };
            if action == MsgSend && key == "to" && !args.pos.is_empty() {
                return Err(Error::usage(
                    "put --to before the message text; omit a positional destination",
                ));
            }
            let value = if values.contains(&key) {
                let v = inline.or_else(|| rest.next_if(|arg| !arg.starts_with('-')));
                if v.is_none() && key != "new-room" {
                    return Err(Error::usage(format!("--{key} requires a value")));
                }
                v
            } else if switches.contains(&key) && inline.is_none() {
                None
            } else {
                return Err(Error::usage(
                    "unknown option or unexpected option value; see ddg --help",
                ));
            };
            if let Some(value) = value.filter(|_| matches!(key, "env" | "unset-env")) {
                if key == "env"
                    && !value
                        .split_once('=')
                        .is_some_and(|(name, _)| !name.is_empty())
                {
                    return Err(Error::usage("--env requires KEY=VALUE"));
                }
                args.multi_flags.entry(key).or_default().push(value);
                continue;
            }
            if args.flags.insert(key, value).is_some() {
                return Err(Error::usage(format!("duplicate --{key}")));
            }
        } else if arg.starts_with('-') {
            return Err(Error::usage("unknown option; see ddg --help"));
        } else if action == Run && args.has("new-room") {
            return Err(Error::usage(
                "put the agent ID before --new-room; use --name to name the room",
            ));
        } else {
            args.pos.push(arg);
        }
    }
    if let Some(tail) = tail {
        if matches!(
            action,
            Run | AgentAdd | AgentSet | RoomCreate | SessionCreate | SessionSplit
        ) {
            if tail.is_empty() {
                return Err(Error::usage("expected a command after --"));
            }
            args.argv = tail.into_iter().map(str::to_owned).collect();
        } else if matches!(action, AgentLs | AgentShow | AgentDefault | AgentRm) {
            return Err(Error::usage("unexpected --; see ddg --help"));
        } else {
            args.pos.extend(tail);
        }
    }
    if !(min..=max).contains(&args.pos.len()) {
        return Err(Error::usage("wrong number of arguments; see ddg --help"));
    }

    let has = |flag| args.has(flag);
    if matches!(action, UpdateApply | UpdateConfig)
        && args
            .value("channel")
            .is_some_and(|value| !matches!(value, "stable" | "prerelease"))
    {
        return Err(Error::usage("--channel must be stable or prerelease"));
    }
    let error = match action {
        UpdateConfig | UpdateApplyHelper if args.host.is_some() => {
            Some("--host is not supported here")
        }
        UpdateConfig if has("repo") && has("clear-repo") => {
            Some("use --repo or --clear-repo, not both")
        }
        UpdateConfig
            if args
                .value("auto-check")
                .is_some_and(|value| !matches!(value, "on" | "off")) =>
        {
            Some("--auto-check must be on or off")
        }
        UpdateApplyHelper
            if args.config.is_some()
                || ["job", "state-dir"]
                    .into_iter()
                    .any(|key| args.value(key).is_none_or(str::is_empty)) =>
        {
            Some("the helper requires --job and --state-dir")
        }
        UpdateApplyHelper
            if args
                .number::<i32>("parent-pid")?
                .is_some_and(|pid| pid <= 1) =>
        {
            Some("invalid --parent-pid")
        }
        SkillInstall if args.value("dir") == Some("") => Some("--dir requires a nonempty path"),
        Run if ["room", "new-room", "bg"]
            .into_iter()
            .filter(|flag| has(flag))
            .count()
            > 1 =>
        {
            Some("specify at most one of --room, --new-room, or --bg")
        }
        Run if has("name") && !has("new-room") => Some("--name requires --new-room"),
        AgentAdd if args.argv.is_empty() => Some("expected a command after --"),
        AgentDefault if has("clear") != args.pos.is_empty() => {
            Some("specify an agent ID or --clear, not both")
        }
        MachineAdd
            if usize::from(!args.pos.is_empty())
                + usize::from(has("docker"))
                + usize::from(has("container-ssh"))
                != 1 =>
        {
            Some("specify one SSH target, --docker, or --container-ssh")
        }
        MachineAdd if has("ssh-host") && !has("docker") => Some("--ssh-host requires --docker"),
        RoomRename if has("clear") == (args.pos.len() == 2) => {
            Some("specify a room name or --clear, not both")
        }
        SessionCreate if has("room") && has("background") => {
            Some("--room and --background cannot be combined")
        }
        SessionSplit if has("horizontal") && has("vertical") => {
            Some("--horizontal and --vertical cannot be combined")
        }
        SessionSplit
            if args.number::<u16>("ratio")?.is_some_and(|ratio| {
                !(MIN_SPLIT_RATIO_BP / 100..=MAX_SPLIT_RATIO_BP / 100).contains(&ratio)
            }) =>
        {
            Some("--ratio must be in 15..=85")
        }
        SessionSend if has("stdin") && args.pos.len() == 2 => Some("use text or --stdin, not both"),
        SessionWait if args.value("output") == Some("") => Some("--output must not be empty"),
        SessionWait if args.number::<u64>("timeout")? == Some(0) => {
            Some("--timeout must be positive")
        }
        MsgSend if args.value("to") == Some("") || (!has("to") && args.pos.is_empty()) => {
            Some("expected a destination session ID")
        }
        MsgSend if args.pos.len() != usize::from(!has("to")) + usize::from(!has("stdin")) => {
            Some("specify one destination, and either text or --stdin")
        }
        MsgRecv | MsgSubscribe if args.host.is_some() || args.config.is_some() => {
            Some("inbox reads use the current session; omit --host and --config")
        }
        SessionMove if has("room") == has("new-room") => {
            Some("specify exactly one of --room or --new-room")
        }
        _ => None,
    };
    if let Some(message) = error {
        return Err(Error::usage(message));
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_subcommands_and_flags() {
        for command in [
            "update status -H all",
            "update check -H prod",
            "update apply --channel prerelease --version v1.2.3-rc.1 --allow-downgrade --restart-sessions --force --yes",
            "update rollback --restart-sessions --yes",
            "update config --auto-check off --channel stable --repo owner/repo",
            "update config --clear-repo",
            "__update-apply-helper --job job --state-dir dir --parent-pid 42 --restart-sessions --relaunch",
        ] {
            let raw = command
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert!(parse(&raw).is_ok(), "{command}");
        }
        for command in [
            "update apply -H all",
            "update rollback -H ALL",
            "update config -H local",
            "update config --repo owner/repo --clear-repo",
            "update config --auto-check yes",
            "update apply --channel edge",
            "update apply --allow-custom-source",
            "__update-apply-helper --job job --state-dir dir --parent-pid -1",
        ] {
            let raw = command
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert!(parse(&raw).is_err(), "{command}");
        }
    }

    #[test]
    fn agent_commands_and_run_flags() {
        for flag in ["--bg", "--background"] {
            let raw = ["run", "pi", flag, "--", "-m", "foo"].map(str::to_owned);
            let args = parse(&raw).unwrap();
            assert_eq!(args.pos, ["pi"]);
            assert_eq!(args.argv, ["-m", "foo"]);
            assert!(args.has("bg"));
        }
        let raw = ["run", "pi", "--new-room", "--name", "dev"].map(str::to_owned);
        let args = parse(&raw).unwrap();
        assert!(args.has("new-room"));
        assert_eq!(args.value("new-room"), None);
        assert_eq!(args.value("name"), Some("dev"));
        let raw = [
            "agent", "add", "pi", "--env", "A=1", "--env", "B=2", "--", "pi",
        ]
        .map(str::to_owned);
        let args = parse(&raw).unwrap();
        assert_eq!(args.argv, ["pi"]);
        assert_eq!(args.values("env"), ["A=1", "B=2"]);
        let raw = ["agent", "set", "pi", "--unset-env=A", "--unset-env=B"].map(str::to_owned);
        assert_eq!(parse(&raw).unwrap().values("unset-env"), ["A", "B"]);
        let raw = ["a", "show", "pi", "--reveal-env"].map(str::to_owned);
        assert!(parse(&raw).unwrap().has("reveal-env"));
        for raw in ["session create", "room create", "session split anchor"] {
            let raw = raw
                .split_whitespace()
                .chain(["--agent", "pi", "--", "--fast"])
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let args = parse(&raw).unwrap();
            assert_eq!(args.value("agent"), Some("pi"));
            assert_eq!(args.argv, ["--fast"]);
        }
        let raw = ["run", "--new-room", "--name=dev"].map(str::to_owned);
        assert!(parse(&raw).unwrap().pos.is_empty());

        for raw in [
            "run pi --new-room dev",
            "run --new-room dev",
            "run pi --new-room=dev",
            "run pi --name dev",
            "run pi --room room --new-room",
            "run pi --room room --background",
            "run pi --new-room --bg",
            "run pi --bg --host all",
            "agent add pi --env SECRET_VAL -- pi",
            "agent set pi --env =SECRET_VAL",
            "agent set pi --token=SECRET_VAL",
        ] {
            let argv = raw
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let err = parse(&argv).expect_err(raw);
            assert!(!err.message.contains("SECRET_VAL"));
        }
    }

    #[test]
    fn parser_matrix() {
        for (argv, action, count) in [
            ("ls", Action::Overview, 0),
            ("status --host=all", Action::Overview, 0),
            ("m ls --probe", Action::MachineLs, 0),
            ("machine add --container-ssh u@h:22", Action::MachineAdd, 0),
            ("m add --docker ct --ssh-host host", Action::MachineAdd, 0),
            ("m rm host", Action::MachineRm, 1),
            ("m default host", Action::MachineDefault, 1),
            ("m test host", Action::MachineTest, 1),
            ("m install host", Action::MachineInstall, 1),
            ("m discover", Action::MachineDiscover, 0),
            ("r ls -H all --include-hidden", Action::RoomLs, 0),
            (
                "room create --cols 80 -- /bin/sh -c hello",
                Action::RoomCreate,
                0,
            ),
            ("r rename abc --clear", Action::RoomRename, 1),
            ("r close abc --yes", Action::RoomClose, 1),
            ("s ls --room abc", Action::SessionLs, 0),
            (
                "s create --background --title worker",
                Action::SessionCreate,
                0,
            ),
            ("s read xyz --lines 120", Action::SessionRead, 1),
            (
                "s send xyz --stdin --enter --expect-gen 2",
                Action::SessionSend,
                1,
            ),
            ("s send xyz -- --flaglike-text", Action::SessionSend, 2),
            ("s move xyz --new-room", Action::SessionMove, 1),
            ("s move xyz --new-room named", Action::SessionMove, 1),
            ("s restart xyz --rows 40", Action::SessionRestart, 1),
            ("s close xyz", Action::SessionClose, 1),
            (
                "s wait xyz -H work --output READY --timeout 1 --or-message",
                Action::SessionWait,
                1,
            ),
            ("msg send xyz hello", Action::MsgSend, 2),
            ("inbox send --to xyz hello", Action::MsgSend, 1),
            ("msg send xyz --stdin -H work", Action::MsgSend, 1),
            ("msg send --to=xyz --stdin", Action::MsgSend, 0),
            ("msg recv --max 1 --wait-ms 100", Action::MsgRecv, 0),
            ("inbox subscribe", Action::MsgSubscribe, 0),
            (
                "s attach xyz --force --history 20",
                Action::SessionAttach,
                1,
            ),
        ] {
            let raw: Vec<_> = argv.split_whitespace().map(str::to_owned).collect();
            let parsed = parse(&raw).unwrap_or_else(|e| panic!("{argv}: {}", e.message));
            assert_eq!((parsed.action, parsed.pos.len()), (action, count), "{argv}");
        }
    }

    #[test]
    fn global_flags_ignore_executor_argv_and_work_on_either_side() {
        for argv in [
            "-H local --config hosts.json --json session create -- cmd --json -V",
            "session --json create --config=hosts.json --host=local -- cmd --json -V",
        ] {
            let raw: Vec<_> = argv.split_whitespace().map(str::to_owned).collect();
            let parsed = parse(&raw).unwrap();
            assert_eq!(parsed.action, Action::SessionCreate);
            assert_eq!(parsed.host, Some("local"));
            assert_eq!(parsed.config, Some("hosts.json".into()));
            assert!(parsed.json);
            assert_eq!(parsed.argv, ["cmd", "--json", "-V"]);
        }
        for (argv, action) in [(["-h", "s"], Action::Help), (["s", "-V"], Action::Version)] {
            let raw = argv.map(str::to_owned);
            assert_eq!(parse(&raw).unwrap().action, action);
        }
    }

    #[test]
    fn session_split_parses_options_and_executor() {
        let raw = "session split anchor --vertical --ratio=85 --cwd /tmp --title pane --cols 80 --rows 24 -H work -- cmd --horizontal"
            .split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        let args = parse(&raw).unwrap();
        assert_eq!(args.action, Action::SessionSplit);
        assert_eq!(args.pos, ["anchor"]);
        assert_eq!(args.host, Some("work"));
        assert!(args.has("vertical") && !args.has("horizontal"));
        assert_eq!(args.number::<u16>("ratio").unwrap(), Some(85));
        assert_eq!(args.value("cwd"), Some("/tmp"));
        assert_eq!(args.value("title"), Some("pane"));
        assert_eq!(args.argv, ["cmd", "--horizontal"]);
        assert!(parse(&["s", "split", "anchor"].map(str::to_owned)).is_ok());
        assert!(
            parse(&["s", "split", "anchor", "--horizontal", "--ratio=15"].map(str::to_owned))
                .is_ok()
        );
    }

    #[test]
    fn skill_flags_and_subcommands() {
        for (argv, json) in [
            ("--skill", false),
            ("-H all room --skills bad-option", false),
            ("skill", false),
            ("--json --skill", true),
        ] {
            let raw: Vec<_> = argv.split_whitespace().map(str::to_owned).collect();
            let parsed = parse(&raw).unwrap();
            assert_eq!((parsed.action, parsed.json), (Action::Skill, json));
        }
        let raw = ["skill", "install", "--dir", "/tmp/agent skills"].map(str::to_owned);
        let parsed = parse(&raw).unwrap();
        assert_eq!(parsed.action, Action::SkillInstall);
        assert_eq!(parsed.value("dir"), Some("/tmp/agent skills"));
        let raw = ["session", "create", "--", "cmd", "--skill"].map(str::to_owned);
        assert_eq!(parse(&raw).unwrap().argv, ["cmd", "--skill"]);
        assert!(HELP.ends_with("Agent? Run: ddg --skill\n"));
    }

    #[test]
    fn invalid_combinations_and_tokens_are_rejected() {
        for argv in [
            "--token secret ls",
            "--token --skill",
            "--skills --token=secret",
            "skill install --dir=",
            "skill install --dir",
            "status --token=secret --json",
            "r close id -H all",
            "m add",
            "m add host --docker other",
            "m add host --ssh-host foo",
            "r rename id foo --clear",
            "r rename id",
            "s create --room id --background",
            "s split",
            "s split id --horizontal --vertical",
            "s split id --ratio 14",
            "s split id --ratio 86",
            "s split id --ratio 33.5",
            "s split id --",
            "s move id",
            "s move id --room room --new-room",
            "s send id text --stdin",
            "s send id --expect-gen",
            "s create --",
            "s attach id --unknown",
            "s wait",
            "s wait id --timeout 0",
            "s wait id --timeout nope",
            "s wait id --timeout -1",
            "s wait id --output=",
            "s wait id -H all",
            "msg send",
            "msg send id",
            "msg send id hello --stdin",
            "msg send id --to other",
            "msg send id --to other --stdin",
            "msg send id --to other hello",
            "msg send --to= --stdin",
            "msg send id hello -H all",
            "msg recv -H local",
            "msg subscribe --config=hosts.json",
        ] {
            let raw: Vec<_> = argv.split_whitespace().map(str::to_owned).collect();
            let error = parse(&raw).expect_err(argv);
            assert!(!error.message.contains("secret"));
        }
    }
}
