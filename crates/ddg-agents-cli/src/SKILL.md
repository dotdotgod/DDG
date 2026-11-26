---
name: ddg
description: Manage local and remote DDG machines, rooms, persistent terminal sessions, and inter-session messaging via the ddg operator CLI and in-session ddg-agents-host bridge CLI.
---

# DDG operator manual

Use `ddg` from any shell to manage machine profiles, rooms, and persistent terminal sessions. Inside a managed PTY, `ddg msg` can use the authenticated bridge through the injected `$DDG_SESSION_TOKEN` and `$DDG_HOST_SOCK`. Use `ddg-agents-host` for the full bridge API.

Use IDs from command results, not display names: `rm_*` is a room, `rm_bg_*` is a host's hidden background room, `ses_*` is a session (tab), and `msg_*` is an inbox message. Restart preserves the session ID but increments `run_generation`. Interactive attach has a single-controller lease; observer attaches can read without controlling. `ddg session send` is a one-shot PTY write that does not take the lease.

## Machines and remote setup

- `ddg machine ls [--probe]` lists local profiles; `--probe` adds live health and identity results. `ddg machine discover` lists SSH config and Docker candidates without adding them.
- `ddg machine add <target-or-quoted-ssh-cmd> [--install | --no-test]` normally tests the connection and pins the returned `expected_host_id` before saving. Example: `ddg machine add 'ssh -p 2222 user@host' --id worker --install`. For Docker use `ddg machine add --docker <container> [--ssh-host <host>]`; for a container with SSH use `ddg machine add --container-ssh <user@host:port>`.
- `--install` uploads platform-matched binaries over SSH (including container SSH), verifies the handshake, and pins identity. It is not supported over Docker exec or local transport. `--no-test` saves without probing or pinning and skips installation even if both flags are given. For an existing SSH profile, use `ddg machine install <id>`.
- `ddg machine test <id>` probes without changing the profile. `ddg machine default <id>` changes the default selection. `ddg machine rm <id>` removes the profile, not the remote software or sessions; `local` and the last machine cannot be removed.
- SSH uses `StrictHostKeyChecking=yes` in batch mode. Set up trusted known-host entries and keys first (`--known-hosts` and `--identity` can select files). The SSH host key and DDG's pinned `expected_host_id` are separate checks. Investigate mismatches; never bypass them.

## Updates

- `ddg --version` prints the installed CLI version without contacting a host. `ddg update status` reads installed and cached state; `ddg update check` explicitly checks GitHub. Both default to `local`, not the configured default machine, and accept `-H <id>` or `-H all`.
- `ddg update apply [--channel <stable|prerelease>] [--version <tag>] [--allow-downgrade] [--restart-sessions] [--force] [--yes]` and `ddg update rollback [--restart-sessions] [--yes]` target one machine (local or pinned SSH). For Docker or container SSH, update the image and recreate the container instead. Do not pass `--restart-sessions` without authorization to interrupt active sessions; `--yes` never implies it or `--allow-downgrade`. `--force` only retries a recorded failed version.
- `ddg update config [--auto-check <on|off>] [--channel <stable|prerelease>] [--repo <owner/repo> | --clear-repo]` reads or changes local update settings and does not accept `-H`. A custom source is flagged but still subject to release integrity and signing checks. Set GitHub credentials through the environment, never `--token`.
- Update commands with `--json` write one JSON object to stdout even on failure; check the exit status and `error.code`. Other commands never check for updates or print update notices.

## Agent profiles and runs

- `ddg agent ls` lists profiles and the default. `ddg agent add <id> [--label <label>] [--icon <icon>] [--cwd <dir>] [--default] -- <argv...>` creates a profile from literal argv, without shell evaluation. `ddg agent set <id>` accepts the same options; omit `-- <argv...>` to keep the existing command.
- `ddg agent show <id>` masks environment values in text and JSON. Use `--reveal-env` only when you intend to disclose them. Use repeatable `--env KEY=VALUE` and `--unset-env KEY` to edit environment entries. Reserved and secret environment keys cannot be stored.
- `ddg agent set <id> --host-override <machine>` applies `--cwd`, `--env`, `--unset-env`, and `-- <argv...>` to that machine's override. An override `--unset-env` suppresses inherited keys (`null` in JSON). Use `ddg agent rm <id> --host-override <machine>` or `ddg agent set <id> --clear-host-override <machine>` to remove an override. `ddg agent rm <id>` deletes the profile. `ddg agent default <id>` selects the default; `ddg agent default --clear` clears it.
- `ddg run [agent] [--room <room_id> | --new-room [--name <name>] | --bg | --background] [--cwd <dir>] [--title <title>] [--cols <n>] [--rows <n>] [-- <extra-args...>]` launches a profile. With no agent ID, it uses the configured default. It attaches in a TTY unless you use `--bg`, which returns a hidden session ID without attaching and is required in scripts or with `--json`. Without a placement option, a visible session on the selected host gets a sibling tab; otherwise, a new visible room is created. Press `Ctrl-]` to detach without stopping the session.
- `ddg room create`, `ddg session create`, and `ddg session split` accept `--agent <id>`. With `--agent`, arguments after `--` append to the profile's command. Without it, those commands keep their existing default-shell and literal-command behavior. Profile runs use the explicit CWD first, then the machine override, then the profile CWD. Only local launches can fall back to your current directory.
- Agent profiles live in `agents.json` next to the selected `hosts.json` (pass `--config <hosts_path>` to change the location). They do not inherit the caller's environment.

## Rooms and sessions

- `ddg ls` or `ddg status` shows all machines by default. Select a machine profile with `-H <id>`. `ddg room ls` and `ddg session ls` default to the configured machine; both take `--include-hidden` and `-H all`. On all-host queries, a successful exit can still contain failures in the JSON `errors` array alongside reachable hosts' data. With one selected host, a connection failure is a command error. Do not use `-H all` for writes.
- `ddg room create [--name <name>] [--cwd <dir>] [-- <cmd...>]` starts a room with its first session. Without a command, creation starts a default shell. `ddg room rename <room_id> (<name> | --clear)` sets or clears the custom name. `ddg room close <room_id> [--yes]` closes the room's sessions; `--yes` is required for multiple tabs. Background rooms cannot be closed this way.
- `ddg session ls [--room <room_id>] [--include-hidden]` returns session IDs, runtime state, generation, and negotiated agent activity, including foreground hints when available. `ddg session create [--room <room_id> | --background] [--cwd <dir>] [--title <title>] [-- <cmd...>]` creates a session; omitting `--room` also selects the hidden background room. Creation supports `--cols` and `--rows`.
- `ddg session read <session_id> [--lines <n>] [--max-bytes <n>]` reads the terminal screen. `ddg session send <session_id> [text | --stdin] [--enter] [--expect-gen <n>]` writes bytes, not an inbox message. Quote text as one argument or pipe bytes with `--stdin`. `--enter` appends a carriage return.
- `ddg session split <session_id> [--horizontal | --vertical] [--ratio <15..85>] [--cwd <dir>] [--title <title>] [--cols <n>] [--rows <n>] [-- <cmd...>]` starts a new pane beside a visible session (horizontal, 50% by default), inheriting the anchor's working directory unless overridden. Plain output is the new session ID and room ID, separated by a tab.
- `ddg session move <session_id> (--room <room_id> | --new-room [name])` promotes a hidden session into a visible room without restarting it. `ddg session restart <session_id> [--cols <n>] [--rows <n>]` starts a new run, terminating the current run if it is active. `ddg session close <session_id>` terminates the session.
- `ddg session wait <session_id> [--output <pattern>] [--timeout <secs>] [--or-message]` waits for exit/close, or for literal screen or terminal output (ANSI escapes are ignored). The timeout defaults to 180 seconds. JSON reports `reason`: `exited`, `closed`, `output`, `message`, or `timeout`. A timeout is a successful check-in, not a match; check `reason` or `matched`. With `--output`, an exit before a match fails with `session_exited`. Inside a DDG session, `--or-message` can return one message from your own inbox instead. A message racing another wait result may already have been delivered to the waiter.
- `ddg session attach <session_id> [--readonly] [--force] [--history <n>]` streams a session. It accepts input from a TTY when you have control; `--readonly` observes, and `--force` may take control from another client. Press `Ctrl-]` to detach without terminating. With `--json`, output is newline-delimited JSON with `session_id` and base64 `data_b64`.

Operator attach, wait, restart, and close work on hidden background sessions directly; no promotion is needed. Background sessions are removed on exit, so start waiting before they finish.

Example: after creating a session and reading its current generation, substitute its ID and generation below.

```sh
ddg --json session ls -H local --include-hidden
ddg --json session send ses_example hello --enter --expect-gen 1 -H local
ddg --json session read ses_example -H local
```

## Inbox messages (not terminal input)

- `ddg msg send <session_id> <text>` or `ddg msg send --to <session_id> --stdin` sends up to 65,536 bytes. Inside a session, with no `-H` or `--config`, the message uses the bridge and your `ses_*` sender identity. Outside a session, or with an explicit `-H <profile>` or `--config`, it uses the selected machine profile and `host.tab_msg_send` with sender `operator`. The operator route does not need hub relay. For authenticated cross-host sending as a session, use `ddg-agents-host msg send --host <id>` with a configured hub.
- `ddg msg recv [--max <n>] [--wait-ms <ms>]` drains your own inbox (default: 256 messages, zero wait). Use `--wait-ms` instead of polling. With `--json`, it prints an array of messages.
- `ddg msg subscribe` streams one message per line (JSON records with `--json`); stop with Ctrl-C. Recv and subscribe require a live in-session environment and do not accept `-H` or `--config`. JSON message payloads use base64 `data_b64`. `inbox` is an alias for `msg`.

## Bridge inside a session

Run `ddg-agents-host --skill` for the bridge manual. The PTY gets `DDG_SESSION_ID`, `DDG_ROOM_ID`, `DDG_HOST_ID`, `DDG_HOST_SOCK`, `DDG_TERMINAL_SOCK`, and `DDG_SESSION_TOKEN`. Do not rely on `DDG_RUN_GENERATION`; this version does not inject it. Read `run_generation` from session results instead.

- `ddg-agents-host host list` shows the local host and known peers.
- `ddg-agents-host session create` defaults to hidden; add `--room <room_id>` to target a room. `ddg-agents-host session list` supports `--room`, `--include-hidden`, and `--host all`. Use `ddg-agents-host session move` with `--room` or `--new-room [name]` to promote; use `ddg-agents-host session close` or `ddg-agents-host session restart` when authorized. Each takes a session ID.
- `ddg-agents-host session screen <session_id> [--lines <n>] [--max-bytes <n>]` reads the screen. `ddg-agents-host session write <session_id> [text | --stdin] [--enter] [--expect-gen <n>]` writes PTY input without taking control.
- `ddg-agents-host msg send <session_id> <text>` sends an inbox message (also accepts `--to <session_id>` and `--stdin` instead of positional ID and text). `ddg-agents-host msg recv [--max <n>] [--wait-ms <n>]` returns a JSON array, empty on timeout. Use `--wait-ms` instead of sleeping or polling. `ddg-agents-host msg subscribe` streams one JSON message per line. Payloads use base64 `data_b64`.
- Without a target the bridge uses its own host. With `DDG_HUB_ENABLED=1` on a serving host and pinned peer profiles, session commands and `msg send` accept `--host <id>` for cross-host relay; recv and subscribe are for the caller's inbox. The bridge already outputs JSON; do not add `--json` to bridge calls.

## Guardrails and errors

- Never pass `--token` or print/copy the injected token. Always use `--json` when scripting `ddg`. Check exit status and any partial results. For race-sensitive terminal input, get the current generation first and pass `--expect-gen`; after a mismatch, recheck the target before retrying.
- Don't close or restart user sessions without authorization. Don't remove `local` or `rm_bg_*`; clean up only scratch sessions you own unless asked to affect other sessions. For scratch testing, set `DDG_STATE_DIR`, `DDG_RUN_DIR`, and `DDG_CONFIG_DIR` to fresh private directories for the test host and CLI.
- Outside `ddg update`, a failed `ddg --json` command reports `{"error":{"code":"...","message":"..."}}` on stderr. For `not_installed`, use the SSH install workflow. For `host_key_mismatch` or `host_id_mismatch`, stop and verify identity. For `auth_required`, fix SSH credentials. For `generation_mismatch` or `session_not_running`, refresh session state. For `control_denied` or `permission_denied`, do not force or bypass control without authorization. Bridge errors appear on stderr with the `ddg-agents-host:` prefix; RPC errors include JSON details after that prefix.

Use `ddg --help` for the full flag list. `ddg --skill` (alias `ddg --skills` or `ddg skill`) prints this file. `ddg skill install [--dir <path>]` replaces `SKILL.md` in the specified directory or in `$HOME/.agents/skills/ddg` by default.
