# 배치 완료 보고서 (`T0`–`T6-A`): 탭 아이콘 · OSC 7501 에이전트 상태 · 에이전트 리스트 · `ddg` 기본 에이전트 설정

- **보고 일자**: `2026-11-26`
- **배치 상태**: **부분 완료 (`T0`–`T6-A` 구현·독립 검수·실환경 검증 PASS · `T6-B` 사람 GUI 실기 대기 `Unverified`)**
- **활성 계획**: [docs/plan/agent-status-icons-profiles/README.md](../../../../plan/agent-status-icons-profiles/README.md) (전체 완료 전까지 아카이브 이관 보류)
- **선행 보고서**: [PLANNING.md](PLANNING.md) (보존)
- **브랜치 / 머지 상태**: 워크스페이스에 `.git` 메타데이터 부재 · `main` 머지/푸시 미수행 (사용자 승인 대기)

---

## 1. 수행 범위 및 태스크 완료 현황

| 태스크 | 영역 | 상태 | 담당 및 검증 요약 |
|---|---|---|---|
| **T0** | 환경 점검 · `docs/spec/requirements.md` §1 갱신 | **DONE** | `pi auth check` (`openai/gpt-6.2-sol`) `ready`, `cargo 1.100.0`, `docker 29.9.3`, `GHOSTTY_VT_LIB_DIR` 확인 및 `requirements.md` §1 갱신 |
| **T1** | `ddg-agents-proto` + OpenRPC 1.3 스냅샷 | **DONE** | `AgentState`/`BlockedKind`/`AgentStatus`/`ForegroundHint`/`TabActivity`, `agent_activity_v1`·`fg_pgrp_v1` 협상, `sanitize_display_text` 강화, OpenRPC 스냅샷 일치 (37/37 PASS) |
| **T2-A** | `ddg-agents-vt` OSC 7501 파서 · FIFO `Relay` | **DONE** | `osc7501.rs` + `relay.rs` FIFO `7501;?` 응답(DA1 선행), 4096B drain-and-discard 0B 비유출, `replay=true` 0B 응답, `OSC 133;A` clear (17/17 PASS) |
| **T2-B** | `ddg-agents-transport` 공용 설정 · 원격 안전 업그레이드 | **DONE** | `agent.rs`/`programs.rs`/`profile.rs`/`client.rs`/`install.rs` — `agents.json` `flock`+CAS+`0o600`+`.corrupt.<ts>`, 동적 로더 4종/시크릿/예약어 차단, `validate_agent_profile_slug` 정합화, 구버전 `ps` 정확 매칭 안전 업그레이드 (33 unit + 1 local integration PASS) |
| **T3** | `ddg-terminal-server` 상태 · OS `foreground` · PID 수명주기 | **DONE** | `proc_info.rs` (Linux `openat` `/proc/{pgid}` + macOS `KERN_PROCARGS2` bounded `argc`, `envp` 미접근), `TERM_PROGRAM=ddg` 및 외부 터미널 변수 제거, `GOODBYE::Detach`→`BYE`, `core.pid` 0600·정상 종료 정리 (23/23 PASS) |
| **T4-A** | `ddg-agents-host` RAM `activities` · 보안 격리 · `host.pid` | **DONE** | `HostState.activities` RAM 전용(`catalog.json` 비트리거), `catalog_subscribe` `after_reply` 원자 스트림, `tab_mark_seen` 세대 검증/단조 클램프, `plugin_host` 5경로 `redact_for_plugin`+`rm_bg_*` 숨김, `hub.rs` 오퍼레이터 RPC 차단, `host.pid` (46/46 PASS) |
| **T4-B** | `ddg-agents-cli` `ddg run`/`ddg agent`/`--agent` | **DONE** | `args.rs`/`agent.rs`/`run.rs`/`room.rs`/`session.rs`/`machine.rs`/`output.rs`/`msg.rs`/`SKILL.md` — 기본 마스킹/`--reveal-env`, 비에코 오류, 비-TTY 거부, `msg.rs` `U+2028`/`U+2029` 사전 치환 보강 (유닛 25/25 + `cli_e2e` 5/5 PASS) |
| **T5** | `native/` GPUI 클라이언트 UI · 아이콘 · 모달 · Peek | **DONE** | `icons.rs` (`AppAssets` 확장 + `roxmltree` XML AST 화이트리스트 + 512px OOM 가드 + FNV-1a 해시 캐시 + 원격 내장 키 전용), `agent_list.rs`, `agent_launcher.rs`, `agent_modal.rs`, `ui_scale.rs` & `config_watch.rs` (`UiPrefs` v2 필드 보존), `host_bridge.rs`, `main.rs` (`ActiveOverlay`, 2단 사이드바, 22px 분할 헤더, Peek `want_control=false`+`release_control` 3분기, `observe_window_activation`, 8Hz/0Hz 스피너 게이트, `ui_zoom` 반올림 정합화) (53/53 PASS + `--check-vt` PASS) |
| **T6-A** | 워크스페이스 회귀 + 실환경 시나리오 (`A1`–`A3`) | **DONE** | 7개 대상 크레이트 전 테스트 PASS + `A1-core` PASS + `A2` PASS + `A1-live-agents` 실측 완료 + `A3` Docker 멀티호스트/구버전 무중단 업그레이드 PASS + 잔존 컨테이너/프로세스 0개 |
| **T6-B** | 사람 GUI 실기 검증 (`H1`–`H2`) | **Unverified** | macOS 데스크톱 GPUI 실창 시각·키보드·SVG 핫스왑·0Hz 유휴 CPU 확인 대기 (사람 수행 필요) |
| **T6-C** | 최종 보고서(`FINAL.md`) · 아카이브 이관 · 머지 인계 | **대기** | `T6-B` 완료(또는 사용자 확인) 후 `FINAL.md` 작성 및 계획 아카이브 이관 예정 |

---

## 2. 독립 검수 중 발견·수정된 결함

1. **슬러그 검증 불일치 (`T2-B` 후속 수정)**: `agents.json` 검증 정규식이 `a..b`를 허용하던 반면 `T1`의 `ddg_agents_proto::host::validate_agent_profile_slug`는 `..`를 거부 → `crates/ddg-agents-transport/src/agent.rs`가 `proto` 검증 함수를 직접 호출하도록 통일하고 회귀 테스트 추가.
2. **`T1` `sanitize_display_text` 강화에 따른 `msg.rs` 회귀 (`T4-B` 후속 수정)**: `U+2028`/`U+2029`가 `sanitize_display_text` 내부에서 제거되면서 `msg.rs:196`이 `"hithere"`를 반환 → `crates/ddg-agents-cli/src/msg.rs`의 `plain()`에서 `sanitize_display_text` 호출 전에 `U+2028`/`U+2029`를 공백으로 정규화하여 25/25 PASS 복구.
3. **`ui_zoom_resizes_the_tab_bar_and_sidebar` 픽셀 반올림 오차 (`T5` 후속 수정)**: GPUI 0.2.2 Taffy 레이아웃의 정수 픽셀 스냅과 `tab_bar` 높이·`canvas.y` 계산을 `TAB_BAR_HEIGHT_REM * rem_px` 반올림 기준으로 정합화하여 53/53 PASS 복구.

---

## 3. 자동 테스트 검증 결과 (7개 대상 크레이트)

> 참고: 검증 시점에 다른 병렬 작업(`ddg-upd-t2`)이 본 계획 범위 밖 크레이트 `crates/ddg-agents-update`를 편집 중이었으므로, 최종 통합 회귀 명령은 `--workspace --exclude ddg-agents-update`로 실행했다(`ddg-agents-update`는 본 계획에서 전혀 수정하지 않았으며, 병렬 편집 전 시점의 전체 `cargo test --workspace`는 257 passed / 0 failed / 1 ignored 통과 확인).

| 크레이트 | 실행 결과 | 비고 |
|---|---|---|
| `ddg-agents-proto` | **35 unit + 2 compat = 37 PASS** | OpenRPC 1.3 스냅샷 일치, `rustfmt --check` PASS |
| `ddg-agents-vt` | **17 PASS** | `UT-VT-01..02` 포함 |
| `ddg-agents-transport` | **33 unit + 1 local integration PASS** | `docker_live` 1 ignored, `rustfmt --check` PASS |
| `ddg-terminal-server` | **19 unit + 4 integration = 23 PASS** | `UT-TERM-01..04` 포함 |
| `ddg-agents-host` | **30 unit + 16 integration = 46 PASS** | `UT-HOST-01..03`, `rustfmt --check` PASS |
| `ddg-agents-cli` | **25 unit + 5 `cli_e2e` = 30 PASS** | `UT-CLI-01..02`, `rustfmt --check` PASS |
| `ddg-agents-native` | **53 PASS** + `--check-vt` PASS | `UT-NATIVE-01..04`, `libghostty-vt initialized`, `rustfmt --check` PASS |
| `ddg-agents-os` | **5 PASS** | 기존 회귀 확인 |

---

## 4. 실환경 사용자 시나리오 검증 결과 (`T6-A`)

모든 시나리오는 `SCENARIOS.md` 공통 격리 프리앰블(`HOME`, `DDG_CONFIG_DIR`, `DDG_STATE_DIR`, `DDG_RUN_DIR`, `DDG_PLUGINS_DIR`, `DDG_DIST_DIR`, `XDG_*`, `SSH_AUTH_SOCK`/`DDG_*` 제거, `DDG_HUB_ENABLED=0`) 하에서 실행되었으며, 종료 후 잔존 컨테이너 `0`개·잔존 스크래치 프로세스 `0`개를 확인했다.

### `A1-core` — **PASS**
- `agents.json` 권한 `0600`, 설정 디렉터리 `0700` 확인.
- `LD_PRELOAD`, `DDG_HOST_SOCK`, `OPENAI_API_KEY=sk-secret-999` 3종 차단 + stderr/stdout 시크릿 값 에코 `0`건 확인.
- `ddg agent show mock` 기본 `***` 마스킹 vs `--reveal-env` 평문 출력 확인.
- `ddg run --bg`로 실행한 세션에서 `TERM_PROGRAM=ddg` 설정, `TMUX`/`KITTY_WINDOW_ID`/`GHOSTTY_RESOURCES_DIR`/`OPENAI_API_KEY` 부모 변수 `0`건 유출, `CUSTOM`·`LD_LIBRARY_PATH` 프로파일 변수 주입, 실행 중 `agent_state=working` 관측 후 프로세스 종료 즉시 목록에서 제거 확인.
- 비-TTY 파이프(`ddg run --json < /dev/null`) 즉시 거부(`invalid_params`) 확인.
- 2-프로세스 동시 `ddg agent set` 경합 후 `agents.json` 유효 JSON 보존 확인.

### `A2` — **PASS**
- OSC 7501 `?` 응답 1회 수신 + 10회 상태 전이(`Blocked(permission)` 최종) 동안 `catalog.json` `(mtime, sha256)` 100% 불변 확인.
- `ddg-terminal-server core` `SIGKILL` 후 자식 PTY 생존(`kill -0`), `core` 재기동 시 Holder `tail` 재생으로 `Blocked(permission)` 상태 복구 + PTY 중복 `7501;?` 응답 `0`바이트(총 카운트 1 유지) 확인.
- `core` `SIGTERM` 종료 후 `core.pid` 삭제 + PTY 생존 확인.
- 손상 `agents.json` 입력 시 `ddg agent ls` 실패 + `agents.json.corrupt.<ts>` (`0600`) 보존 확인.

### `A1-live-agents` — **실측 완료 (부분 `Unverified`)**
| 대상 에이전트 | 버전 | 실측 결과 | 비고 |
|---|---|---|---|
| `pi` | `1.6.1` | **PASS** — FIFO `7501;?` 응답 수신 후 실제 OSC 7501 `agent_state="blocked"`, `blocked_kind="permission"`, `unseen=true`, `foreground={comm:"node"}` 방출 확인 | 격리 `HOME` 초기 프로브 실측; 유료 LLM 라이브 턴은 `Unverified` |
| `codex` | `0.166.0` | **PASS** — 초기 대기 시 OSC 7501 미방출(`agent_state=null`) 및 OS `foreground={comm:"codex"}`·`agent_profile="codex"` 폴백 실측 | 유료 LLM 라이브 턴 및 `features.psp` 활성 턴은 `Unverified` |
| `claude` | 미설치 | **`Unverified`** | 로컬 머신에 `claude` 바이너리 미설치 |

### `A3` (Docker 멀티호스트 & 구버전 원격 호스트 안전 업그레이드) — **PASS**
- 루프백 바인딩(`127.0.0.1:<ephemeral>:2222`) 구버전 컨테이너(`ddg-agents-host:test`, `features: ["credit_v1","single_control_v1","passthrough_v1"]`, PID 파일 부재) 대상 검증.
- 잘못된 `expected_host_id: "wrong-host-id"` 업그레이드 시도 → `host_id_mismatch` Fail-closed 거부, `hosts.json` sha256 불변, 원격 `host` 데몬 PID 미변경 확인.
- 실제 `host_id` 핀 설정 후 신규 빌드 Linux `arm64` 바이너리(`DDG_DIST_DIR/linux-aarch64/`)로 `ddg machine install` 수행:
  - 업그레이드 전 `pty=61, holder=16, host=29, core=14` → 업그레이드 후 **`pty=61(alive), holder=16(동일), host=219(교체), core=220(교체)`** (PTY 무중단 보존!).
  - `expected_host_id` 핀 보존 및 `hello.features`에 `"agent_activity_v1"` 포함 검증.
- `host_overrides.env: {"DROP_ME": null, "REMOTE": "y"}`로 원격 `ddg -H remote-a3 run rmock --bg` 실행 → 원격 `env`에 `KEEP=1`, `REMOTE=y` 존재, `DROP_ME` `0`건, `pwd=/workspace`(로컬 `/Users/...` 미유출), 종료 후 숨김 세션 정상 정리 확인.

---

## 5. 미검증 항목 (`Unverified`) 및 남은 위험

1. **`T6-B` 사람 GUI 실기 검증 (`H1`, `H2`) — `Unverified`**:
   - GPUI 실창의 2단 사이드바(45% cap·`룸` 접힘 시 해제·`160px` 축약·호버 툴팁), 22px 분할 패인 헤더(50%/100%/150% 줌), `observe_window_activation` 창 복귀 `seen` 해제, 유휴 0Hz CPU, `AgentLauncher` 4지선다, `AgentSettingsModal` 키보드/충돌/복구 UI, 로컬 SVG 핫스왑 래스터화는 사람이 직접 GUI를 띄워 확인해야 한다.
2. **외부 CLI 실측 범위 — `Unverified`**: `claude` CLI 미설치, `pi`/`codex` 유료 LLM 라이브 턴.
3. **비-macOS 데스크톱 및 WAN SSH — `Unverified`**: Linux X11/Wayland GPUI 실창 및 실제 WAN 패킷 손실 환경.
4. **알려진 한계**: `modal_text_input`의 한글 IME 조합/중간 커서 미지원, 1 MiB `tail` 초과 출력 후 `core` `SIGKILL` 시 직전 OSC 7501 유실, 워크스페이스 루트 `Cargo.lock`은 `native/Cargo.toml`(`roxmltree`, `libc`)과 동기화 필요(`--locked` 빌드 전 갱신).

---

## 6. 다음 단계 (`T6-B` 사람 GUI 실기 실행 가이드)

격리 환경에서 실제 운영 설정(`~/.config/ddg`)을 건드리지 않고 `H1`·`H2`를 확인하려면 아래 명령으로 앱을 실행한 뒤 [SCENARIOS.md](../../../../plan/agent-status-icons-profiles/SCENARIOS.md#사용자-시나리오-h1--사람-gui-실기-2단-사이드바--22px-분할-헤더--0hz-유휴--런처-4지선다--정렬핀peek)의 `H1`·`H2` 체크리스트를 수행하십시오.

```sh
WORK="$(mktemp -d /tmp/ddg-gui-verify-XXXXXX)"
mkdir -p -m 0700 "$WORK/cfg" "$WORK/state" "$WORK/run" "$WORK/plugins" "$WORK/cfg/icons"
env DDG_CONFIG_DIR="$WORK/cfg" DDG_STATE_DIR="$WORK/state" DDG_RUN_DIR="$WORK/run" \
    DDG_PLUGINS_DIR="$WORK/plugins" DDG_HUB_ENABLED=0 \
    GHOSTTY_VT_LIB_DIR=/tmp/ddg-vt.t10r26/ghostty-vt.xcframework/macos-arm64_x86_64 \
    cargo run -p ddg-agents-native
```
