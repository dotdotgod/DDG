---
name: ddg-agents-host
description: Use the authenticated in-session bridge to manage DDG terminal sessions, write PTY input, and exchange inbox messages.
---

# DDG in-session bridge

Run `ddg-agents-host` inside a managed PTY. For external operator workflows and machine setup, run `ddg --skill` instead. `ddg-agents-host --skill` (alias `--skills`) prints this manual without a running host or session credentials.

The PTY gets `DDG_SESSION_ID`, `DDG_ROOM_ID`, `DDG_HOST_ID`, `DDG_HOST_SOCK`, `DDG_TERMINAL_SOCK`, and `DDG_SESSION_TOKEN`. The bridge reads its socket and token from the environment. Never pass `--token`, log the token, or forward it to another agent. Use the current `run_generation` from session results, not an assumed `DDG_RUN_GENERATION` variable (not injected). Session IDs (`ses_*`) survive restarts; the generation changes and the token rotates.

## Host and session commands

The following are arguments to `ddg-agents-host`. Responses are JSON by default; do not pass `--json`.

- `host list`: list this host and known peers.
- `session create [--host <id>] [--room <room_id>] [--cwd <dir>] [--title <title>] [-- <cmd...>]`: start a session (default shell if no command). Without `--room`, it starts hidden in the host's `rm_bg_*` room. Regular rooms use `rm_*` IDs.
- `session list [--host <id|all>] [--room <room_id>] [--include-hidden]`: discover session IDs and generations. Check the `errors` array for partial failures when querying all hosts; `all` is only valid here.
- `session move <session_id> [--host <id>] (--room <room_id> | --new-room [name])`: promote a hidden session without restarting it.
- `session restart <session_id> [--host <id>]`: start a new run, terminating the current run if it is active. `session close <session_id> [--host <id>]`: terminate the session, not just detach. Never close or restart another user's sessions without authorization.
- `session screen <session_id> [--host <id>] [--lines <n>] [--max-bytes <n>]`: read terminal text. `session write <session_id> [--host <id>] [text | --stdin] [--enter] [--expect-gen <n>]`: send PTY input without taking the interactive control lease. Read the generation and pass `--expect-gen` when a restart could race the write. `--enter` appends a carriage return.

## Inbox messages (not terminal input)

When `ddg` is available, `ddg msg send`, `ddg msg recv`, and `ddg msg subscribe` also use this bridge inside a session. Use `ddg --json msg ...` for JSON. An explicit `-H` or `--config` on `ddg msg send` selects the operator route instead; keep using `ddg-agents-host msg send --host <id>` for authenticated session-to-session relay.

- `msg send <session_id> <text>` or `msg send --to <session_id> --stdin`: send bytes to a session's inbox. Either form accepts `--host <id>`. The host determines the sender identity from the token.
- `msg recv [--max <n>] [--wait-ms <n>]`: drain messages for the current session as a JSON array, returning `[]` if none arrives before the timeout. Use a blocking wait, not sleeps and repeated zero-wait polls.
- `msg subscribe`: stream one message per line from the current inbox. Message IDs use `msg_*`; decode message bytes from `data_b64`.

Example:

```sh
ddg-agents-host session list --include-hidden
ddg-agents-host msg send --to "$PEER_SESSION_ID" "ready"
ddg-agents-host msg recv --wait-ms 5000
```

Omitting `--host` targets the current host. Cross-host commands require a serving host configured with `DDG_HUB_ENABLED=1` and a pinned peer profile; check `host list` first. Receive and subscribe always target the caller's own inbox.

Check exit status and stderr. Bridge errors are prefixed with `ddg-agents-host:`, followed by details (JSON for RPC errors). If credentials are missing or stale, run from the live session; do not supply a token on the command line. On a generation mismatch or a stopped target, read the session list again before attempting another write. Do not bypass permission or control errors.
