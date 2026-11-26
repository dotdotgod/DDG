# Workspace debug tools

Run from the workspace root. Put `--json` before or after the subcommand to
keep machine-readable output on stdout and diagnostics on stderr. Exit codes: `0` success, `2` invalid arguments,
`3` build/target failure, `4` missing tool or deferred command, `124` timeout.
`inspect` always emits JSON.

```sh
scripts/ddg-debug doctor --json
scripts/ddg-debug asm -p ddg-agents-proto --list --json
scripts/ddg-debug asm -p ddg-agents-proto ddg_agents_proto::frame::decode_header --target win-x64 --json
scripts/ddg-debug layout -p ddg-agents-proto --filter frame::Frame --json
scripts/ddg-debug inspect --bin /path/to/test-binary --break main --mem '$sp:16' --step-inst 2
scripts/ddg-debug mem --os macos --pid $$ --tool summary --json
python3 scripts/debug/test_debug.py
```

- `asm` prefers the library when present, so pure Rust cross-target assembly needs no target linker. Use `--list` with the intended `--format`; MIR names differ from assembly names (e.g. `fn decode_header(`). `--rust` needs `rust-src` on the active toolchain.
- `layout` sends `-Z print-type-sizes` to the selected crate's nightly `rustc`, not to dependency builds. Field and padding offsets are bytes from the start of the type. `alignment: null` means rustc didn't report field alignment. Layout is toolchain- and target-specific, not an ABI promise.
- `inspect --break 0x…` resolves an executable's file virtual address (e.g. from `nm`) across relocation. `--watch` arms a write watchpoint at the initial breakpoint. If stepping, `stop_reason` is the initial breakpoint/watchpoint reason and the snapshot and `pc` are from the final step. Use `--args --` at the end to pass debuggee flags verbatim. Memory expressions and dumps can expose secrets; only inspect targets you intend to run and share.
- With `--remote HOST:PORT`, `inspect` uses a trusted external `lldb-server platform` endpoint and a local binary with matching symbols. Without `--remote`, the two Linux OS aliases on macOS run the same inspector inside their debug container.
- `mem --cmd /path/to/executable ARG` launches the target and samples it by PID; use `--cmd --` at the end for raw flags. The process must stay alive long enough to sample, except when using `--tool leaks`, which checks via `leaks --atExit`.

## Linux debug containers

```sh
scripts/ddg-debug linux-up --arch arm64 --build --json
scripts/ddg-debug inspect --os linux-arm64 --bin ddg --break main --mem '$sp:16' --step-inst 2
scripts/ddg-debug mem --os linux-arm64 --tool valgrind --json --cmd -- ddg --version
scripts/ddg-debug mem --os linux-arm64 --tool heaptrack --json --cmd -- ddg --version
scripts/ddg-debug mem --os linux-arm64 --tool heaptrack --pid 42 --json
scripts/ddg-debug linux-dap --arch arm64 --json
scripts/ddg-debug linux-down --arch all --json
```

Use `amd64` / `linux-amd64` for x86-64. The debug image leaves production Docker
files alone. It builds `ddg-terminal-server`, `ddg-agents-host`, and the
`ddg-agents-cli` executable `ddg` in `/opt/ddg-target/debug`; the host workspace
is read-only and Cargo download caches are named volumes. `linux-down` preserves
the caches. LLDB platform ports are published only on `127.0.0.1` (arm64 `1234`,
amd64 `1235`). Override with `DDG_DEBUG_PORT_ARM64` / `DDG_DEBUG_PORT_AMD64` or
`linux-dap --port N`; a port change recreates the selected container. The platform
listener is exposed, not LLDB's dynamic inferior ports; use Docker-exec `inspect`
for runtime snapshots. Only run this ptrace-enabled image for trusted local work.

Valgrind starts the program itself; use `--cmd`, not `--pid`. Findings are
reported in JSON even on exit `3`, and the raw XML is copied to a private temp
directory on the host. Heaptrack can inject into a process in the container
PID namespace; prefer `--cmd` when possible, since runtime injection can crash
the target. JSON reports and raw traces may contain paths, symbols, and secrets.

## Headless GPUI tests and profiling

```sh
scripts/ddg-debug gpui-test --json
scripts/ddg-debug gpui-test --filter ui_zoom --seed 42 --json
scripts/ddg-debug gpui-trace --check-vt --metal-validation --json
scripts/ddg-debug gpui-trace --template hitches --duration 5 --json
scripts/ddg-debug profile --kind cpu --duration 2 --bin /path/to/local-binary --json
scripts/ddg-debug profile --kind metal --duration 10 --bin /path/to/app --metal-validation --out /tmp/ddg-profiles --json
scripts/ddg-debug profile --kind alloc --duration 5 --bin /path/to/app --json
python3 scripts/debug/xctrace_summarize.py /path/to/export.xml
python3 scripts/debug/xctrace_summarize.py --kind valgrind /path/to/valgrind.xml
```

`gpui-test` runs only the headless visual tests, with no OS window. `gpui-trace`
uses a release binary if available, otherwise a debug binary (builds one if
needed); `--check-vt` initializes Ghostty without tracing. `--args --` passes
app flags through verbatim. CPU recording uses `samply` without opening a
browser; macOS system binaries are typically not profileable by `samply`.
For Xcode traces, the exported tables depend on the installed Xcode, template,
and target permissions. The summary identifies missing metrics;
Metal frame rows may represent displayed surfaces rather than app frame IDs.
Profile files, trace bundles, and XML exports are retained in a unique directory
under `--out` or the system temp directory.

## Persistent debugger setup

Load the opt-in stdio entries from [`config/debug-mcp.json`](../../config/debug-mcp.json)
with the workspace root as the servers' working directory. No user configuration
or root `.mcp.json` is needed.

- **Native MCP:** `scripts/debug/lldb_mcp.py` runs `xcrun lldb-mcp`. This Xcode build's stdio proxy requires exactly one discoverable LLDB backend, so the launcher starts it on a private Unix socket with a temporary `HOME` and cleans it up on exit.
- **DAP MCP:** `debugmcp serve --stdio` uses [`.debugmcp.json`](../../.debugmcp.json) to run `/usr/bin/xcrun lldb-dap`. Verify the adapter with `debugmcp adapter validate rust`. For `start_debugging`, pass the **built executable** as `fileFullPath`, the workspace as `workingDirectory`, and `rust` as `configurationName`.
- **DAP CLI:** the `debugger-cli` crate installs the `debugger` binary. To point it at Xcode's adapter, run `export PATH="$(dirname "$(xcrun --find lldb-dap)"):$HOME/.cargo/bin:$PATH"`, then `debugger start /path/to/executable --stop-on-entry` and `debugger stop` when finished.
