# Data Network Layer 및 보안·검증 설계 (`DESIGN_DATA.md`)

관련 문서: [계획 개요](README.md) · [UX·UI 설계](DESIGN_UI.md) · [TDD 유닛 테스트](TESTS.md) · [사용자 시나리오](SCENARIOS.md)

---

## 1. 필수 Validation 및 신뢰 경계 확정 표 (13개 경계)

> **내부 코드 신뢰 원칙**: 아래 1차 신뢰 경계를 통과한 데이터는 내부 계약으로 신뢰하며, 후속 내부 홉에서 동일 조건을 재검증·재살균하지 않는다.

| # | 신뢰 경계 (출처) | 책임 크레이트 · 모듈 | 허용 / 거부 조건 (필수 Validation) | 통과 후 내부 보장 (재검증 금지 영역) | 실패 처리 |
|---|---|---|---|---|---|
| 1 | 표시 문자열 살균 | `ddg-agents-proto::id::sanitize_display_text` | `char::is_control()`, Bidi(`U+061C`, `U+200E`, `U+200F`, `U+202A..=U+202E`, `U+2066..=U+2069`), 줄/문단 분리자(**`U+2028`, `U+2029`**) 제거 후 `max_chars` 절단 | 단일 줄 안전 텍스트 보장. **`native` 렌더 루프 재살균 금지** | 정규화 |
| 2 | PTY 출력 (`OSC 7501` · `OSC 133;A`) | `ddg-agents-vt::{relay, osc7501}` | `MAX_OSC_7501_BYTES=4096`(버퍼 `4105B`), 초과 시 종결자(`BEL`/`ST`/`CAN`/`SUB`/비-`\` `ESC`)까지 **drain-and-discard**; `viewer_bytes`·`Vt` 완전 스트립; `Relay::flush()` 부분 7501 비유출; 무패딩 허용 표준 Base64 전용(평문 폴백 금지) → 엄격 UTF-8 → `sanitize_display_text`; `id` 화이트리스트 `^[A-Za-z0-9._-]{1,64}$`(`..` 금지); `MAX_STATUS_RECORDS=16`; `AgentState`/`BlockedKind` 닫힌 enum; `OSC 133;A` 관찰 클리어; `replay=true` 시 `7501;?` 응답 0B | 유효 enum·≤16 레코드·살균 UTF-8 보장. **`core`/`host`/`native` 재파싱·재살균 금지** | 레코드 Fail-closed + PTY 스트림 Fail-open |
| 3 | OS 포그라운드 조회 (`/proc`, `sysctl`, `tcgetpgrp`) | `ddg-terminal-server::{pty, holder, proc_info}` | `tcgetpgrp(master_fd) > 0`; Linux `/proc/<pid>/stat` `sid == pty.pid && pgrp == pgid` + UID 일치 / macOS `PROC_PIDTBSDINFO` `pbi_pgid == pgid && pbi_uid == geteuid()`; `KERN_PROCARGS2` `min(argc, MAX_ARGC)` 엄격 중단(`envp` 절대 미접근); Linux `cmdline` ≤ 4 KiB; 허용 인터프리터만 `script_hint` 스캔(`env`→`None`); `-c`/`-e`/`--eval`/`-p`/`-m`/`-`/미지 `-*` 즉시 중단; `=`/`://`/`@`/공백/제어 포함 토큰 폐기; basename만 64자 살균 | 시크릿·타 UID·타 세션 배제된 `ForegroundHint` 보장. **상위 계층 재필터링 금지** | Fail-open (`None`) |
| 4 | Core ↔ Holder IPC (`QUERY_FG_PGRP=8`, `FG_PGRP=108`) | `ddg-agents-proto::holder`, `ddg-terminal-server::{holder, core}` | `HOLDER_PROTO_VERSION=1` 유지, `HolderWelcome.features = ["fg_pgrp_v1"]`, `MAX_PAYLOAD` 유지, live `pty_id` 한정 조회 | 동일 UID `0o600` 소켓 내부 계약 | 미존재 PTY `None`, 구 Holder(`!fg_pgrp_v1`) 폴링 비활성 |
| 5 | Host RPC 입력 경계 | `ddg-agents-proto::host`, `ddg-agents-host::{server, host_service}` | `ExecutorSpec::Command.env`: 대소문자 무관 **`DDG_*` 전면 거부**, 키 `=`/`\0`/빈키·값 `\0` 거부; `agent_profile`: `^[a-z0-9._-]{1,64}$`; `parent_host_id`: non-empty ≤128자 제어금지(오직 인증된 `host.session_create` `caller` 유도, `tab_create` 지정 금지); `TabMarkSeenParams`: `deny_unknown_fields`, `run_generation` 일치, `seen_seq.max(req.min(activity_seq))`; `tab_create` idempotency 스코프 `"plugin"`/`"operator"` 격리 | `0 <= seen_seq <= activity_seq`, `DDG_*` 오염 없음 보장 | Fail-closed (`InvalidParams`/`GenerationMismatch`/`SessionNotFound`) |
| 6 | Plugin & Hub 격리 경계 | `ddg-agents-host::{plugin_host, host_service, hub}` | `plugin_host.rs` 5개 반환 경로(`catalog_get`, `room_create`, `room_rename`, `tab_create`, `tab_restart`) + idempotency 히트 전부 **`redact_for_plugin`**(`executor.env.clear()`, `RoomVisibility::Visible` 전용); `host.tab_mark_seen` 플러그인·`hub::allowed()` 제외; `rm_bg_*` 활동은 **오퍼레이터 채널 전용**(플러그인·Hub 완전 격리) | 플러그인/Hub로의 시크릿 env 및 숨김 세션 누출 차단 | Fail-closed |
| 7 | `catalog.json` v2 로드 경계 | `ddg-agents-proto::host`, `ddg-agents-host::catalog_store` | `schema_version == 2`, `≤ 2 MiB`, 선택 필드(`agent_profile`, `parent_host_id`) 검증(`parent_host_id.is_some() => parent_session_id.is_some()`); 레거시 로드 `strip_session_env` 대소문자 무관 `DDG_*` 제거 | 유효 `Catalog` v2 보장 | `.corrupt.<ts>` (`0o600`, `create_new`) 보존 후 초기화 |
| 8 | `agents.json` v1 경계 (단일 진실 원천) | `ddg-agents-transport::agent` | 크기 `≤ 512 KiB`, `schema_version == 1`, 최대 128 프로필; `id` `^[a-z0-9][a-z0-9._-]{0,63}$`(예약어 `all`/`default`/`none` 금지, 대소문자 무시 중복 금지); `argv` 비어있지 않음·NUL 금지(`split_safe_tokens` 공유: 따옴표 안팎 `\0`/`\r`/`\n`/`U+2028`/`U+2029`/제어/Bidi 및 말미 `\` 거부); `env`: 대소문자 `DDG_*` + 시크릿 키 패턴 + **동적 로더 4종(`LD_PRELOAD`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, `DYLD_FORCE_FLAT_NAMESPACE`) 차단**(`LD_LIBRARY_PATH` 허용); 부모 `std::env` 미복사; **오류 시 값·본문 비에코**; `O_NOFOLLOW` + `0o700` 디렉터리 + `0o600` `create_new` + `flock(agents.lock)` 내부 바이트 CAS | 안전한 `AgentProfile` 보장. **CLI·GUI 중복 검증 금지** | Fail-closed (비에코 오류), 손상 시 `.corrupt.<ts>[.<n>]` (`0o600`, `create_new`) |
| 9 | `programs.json` v1 경계 | `ddg-agents-transport::programs` | 크기 `≤ 256 KiB`, `schema_version == 1`, 매처 ≤128자 제어금지, `icon` 키 `^[a-z0-9_-]{1,64}$` | 유효 프로그램 매핑 보장 | Fail-open (경고 + 내장 기본값, 파일 미덮어쓰기) |
| 10 | 원격 호스트 협상 & 업그레이드 | `ddg-agents-transport::{client, install, dial}` | `HelloResult`/`HostHelloResult.features`: `"agent_activity_v1"` 협상; 미지원 호스트에는 `TabCreateParams`/`TabSplitParams` 선택 필드 `None` 생략(`deny_unknown_fields` 호환) 및 `tab_mark_seen` 스킵; 업그레이드 시 `expected_host_id` 선검증(`RequirePinnedHostId`)·보존(불일치 Fail-closed, 미덮어쓰기), `host`→`core` 종료 순서(`holder` 절대 보존), PID 파일 `[ ! -L ] && [ -f ]` + 양의 정수(`!= 1`) + `ps` comm/argv 검증, PID 부재 구버전은 `ps -u "$(id -u)" -o pid=,args=` 정확 매칭(`ddg-agents-host serve`/`ddg-terminal-server core`만, `$$`·`holder` 제외), 사후 `hello.features` `"agent_activity_v1"` 검증 | 업그레이드 신원 무결성 + 구버전 와이어 호환 보장 | 신원/사후 feature 실패: Fail-closed; 미지원 feature: Fail-open |
| 11 | `ddg` CLI 인자·출력 경계 | `ddg-agents-cli::{args, agent, run}` | `--token` 차단 유지, `multi_flags` 반복 옵션, `--new-room` 불리언 + `--name`, `--bg`/`--background` 별칭, `--host all` 조회 전용, `agent show` 기본 `env` 마스킹(`--reveal-env` 평문), 비-TTY/`--json` 즉시 attach 거부, 원격 CWD 로컬 경로 미전송 | 검증된 CLI 명령 보장 | Fail-closed (`Error::usage`, 값 비에코) |
| 12 | 로컬 아이콘 자산 경계 | `native::icons` | **원격 호스트**: 내장 키 전용(로컬 `icons/` 미조회). **로컬**: 단일 평면 파일명 `^[A-Za-z0-9._-]+\.(svg\|png)$`, 단일 fd `open(O_NOFOLLOW)` + `fstat` 일반파일 + **`≤ 128 KiB`**; SVG는 **`roxmltree` XML AST 화이트리스트**(정적 벡터 요소만 허용, `<script>`/`<foreignObject>`/`<image>`/`<style>`/`<a>`/`<animate*>`/`<!DOCTYPE`/`<!ENTITY`/`<?xml-stylesheet` 차단, `on*` 금지, `href`/`xlink:href` `#local-id`만 허용, 노드≤1024, 깊이≤16, `width`/`height`/`viewBox`≤**512px**); PNG는 매직 헤더+치수≤512×512 | 안전 렌더링 바이트 보장 | Fail-open (내장/기본 터미널 SVG 폴백 + 키별 1회 경고) |
| 13 | `ui.json` v2 경계 | `native::{ui_scale, config_watch}` | `schema_version == 2`, `≤ 64 KiB`, `collapsed_sections` 단일 필드(`"section:agents"`, `"section:rooms"`, `"group:<key>"`), **`load_or_default` 및 `config_watch.rs` in-place 정규화로 기존/신규 필드 무손실 보존** | 유효 `UiPrefs` v2 보장 | Fail-open (기본값, 손상 시 미덮어쓰기) |

---

## 2. 데이터 스키마 및 저장·동시성·복구 계약

### 2.1 `~/.config/ddg/agents.json` (Schema v1 — `ddg-agents-transport::agent`)
```rust
pub const AGENTS_SCHEMA_VERSION: u32 = 1;
pub const MAX_AGENTS_BYTES: usize = 512 * 1024;
pub const MAX_AGENT_PROFILES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentHostOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Some(val) = 추가/덮어쓰기, None(JSON null) = 상위 프로필 env 키 상속 제거 (와이어 변경 없음, D-4)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub id: String,
    pub label: String,
    pub argv: Vec<String>, // 1개 이상 필수, 셸 문자열 평가 금지
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host_overrides: BTreeMap<String, AgentHostOverride>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent_id: Option<String>,
    #[serde(default)]
    pub agents: Vec<AgentProfile>,
}
```
- **병합 규칙 (`resolve_executor(host_id, extra_args, cli_cwd)`)**:
  - `argv = (override.argv || profile.argv) + extra_args`.
  - `env`: 빈 `BTreeMap`에서 시작(부모 `std::env` 미복사) → `profile.env` → `override.env` (`Some(v)` 덮어쓰기, `None` 제거).
  - `cwd`: 로컬은 `cli_cwd.or(override.cwd).or(profile.default_cwd).or(local_current_dir)`; 원격은 `cli_cwd.or(override.cwd).or(profile.default_cwd)`(로컬 `current_dir` 절대 자동 전송 금지).
- **동시성·저장**: `<config_dir>/agents.lock` `flock(LOCK_EX | LOCK_NB)` 10ms 재시도(최대 2s, 테스트용 타임아웃 주입 가능) → 락 내부에서 원본 바이트 직접 비교 CAS(`sha2` 의존성 불필요) + `(mtime_nanos, len, ino)` 빠른 감지 → `0o700` 디렉터리 보정 + `create_new(true)` `0o600` 임시파일 + `sync_all()` + `rename()`.

### 2.2 `~/.config/ddg/programs.json` (v1) & `~/.config/ddg/ui.json` (v2)
- `programs.json`: `ProgramsConfig { schema_version: 1, programs: Vec<ProgramDefinition { id, label, icon, matchers: { osc_apps, comms, script_hints, agent_profiles } }> }` (`category` 삭제).
- `ui.json`: `UiPrefs { schema_version: 2, ui_scale_pct, agent_sort: Priority|Grouped|Manual, pinned_agents: Vec<String> (max 256), manual_agent_order: Vec<String> (max 512), collapsed_sections: BTreeSet<String> }` (`agents_section_collapsed`/`rooms_section_collapsed` 삭제).

### 2.3 `catalog.json` (v2 유지) & RAM 전용 `TabActivity`
- `TabRecord` 추가 선택 필드(`#[serde(default, skip_serializing_if = "Option::is_none")]`): `agent_profile: Option<String>`, `parent_host_id: Option<String>`.
- 인메모리 활동 타입 (`ddg-agents-proto::{terminal, host}`, `turn` 삭제):
  - `AgentState::{ Idle, Working, Blocked, Done, Error }`, `BlockedKind::{ Permission, Question, Auth }`
  - `AgentStatusRecord { id: Option<String>, state, kind: Option<BlockedKind>, progress: Option<u8>, app, title, msg, updated_ms: u64 }`
  - `AgentStatus { state, kind, progress, app, title, msg, records: Vec<AgentStatusRecord> (max 16), status_ms: u64 }`
  - `ForegroundHint { pid: u32, comm: String, script_hint: Option<String> }`
  - `TabActivity { tab_id, run_generation, agent_status: Option<AgentStatus>, foreground: Option<ForegroundHint>, activity_seq: u64, seen_seq: u64 }`
- **생명주기 전이**:
  - `activity_seq` 증가: `Blocked`/`Done`/`Error` 진입, 상태 전이, `ExternalInput`, 가시 탭 `Exited`에서만 증가 (`Working` progress 틱 및 `foreground` 1s 폴링 변화에서는 미증가).
  - `tab_restart`: 새 `run_generation`, `TabActivity` 초기화(`0/0`, `None`).
  - `tab_close` / `rm_bg_*` `Exited`: `HostState.activities.remove(&tab_id)` 즉시 삭제.
  - 가시 탭 `Exited`: `Working`/`Blocked` 및 `foreground` 제거, `exit_code == 0`→`Done` / `!= 0`→`Error`, `activity_seq += 1`.
  - `tab_move` 승격: `TabActivity` 및 `agent_profile` 보존 후 이벤트 발행.
  - `Core` `SIGKILL` 복구: Holder 1 MiB `tail`을 `Relay::feed_with_events(..., replay = true)`로 재생(7501;? 응답 0B) → `PtyInfo`에 복원 상태 포함 → `HostService` 재연결 시 `pty.subscribe`로 즉시 RAM 재구축.

---

## 3. OSC 7501 FIFO `Relay` · OS `foreground` · 프로토콜/RPC 계약

### 3.1 OSC 7501 & 단일 FIFO 응답 버퍼 (`ddg-agents-vt`)
- 공식 Program Status Protocol: `ESC ] 7501 ; key=value[:key=value...] (BEL | ESC \)` 및 기능 질의 `ESC ] 7501 ; ? (BEL | ESC \)`.
- **단일 FIFO 응답 순서 보장 (`pi` DA1 센티넬 대응)**: `Relay::feed_with_events`가 단일 응답 버퍼를 유지한다. `ESC ] 7501 ; ?`를 만나는 즉시(`replay == false`) 선행 `vt.replies()`를 먼저 drain한 뒤 `\x1b]7501;?\x1b\\`를 추가하고, 그 뒤에 오는 `DA1` 등 후속 바이트를 `vt`에 공급한다.
- **`pty.rs` 환경변수 정리**: 기본 `TERM_PROGRAM=ddg`, `TERM_PROGRAM_VERSION=<pkg_ver>` 설정(스푸핑 `ghostty` 금지). `Holder` 상속 외부 변수(`TMUX`, `TMUX_PANE`, `ZELLIJ`, `ZELLIJ_SESSION_NAME`, `KITTY_WINDOW_ID`, `GHOSTTY_RESOURCES_DIR`, `WEZTERM_PANE`, `ITERM_SESSION_ID`, `TERM_PROGRAM_VERSION`)를 `env_remove`로 제거(사용자 프로파일 명시값 우선).
- **스로틀/코얼레싱**: `state`/`kind` 전환·`clear` 0ms 즉시 방출; 동일 상태 내 `progress`/`msg`/`title` 200ms trailing-edge 코얼레싱; `foreground` 1,000ms 배치 폴링(락 밖에서 조회, 변경 시에만 방출).

### 3.2 `ddg-terminal-server` & `ddg-agents-host` RPC / OpenRPC 1.3
- `Holder ↔ Core` (`HOLDER_PROTO_VERSION = 1`): `HolderWelcome.features = ["fg_pgrp_v1"]`, `QUERY_FG_PGRP = 8` (`pty_id = 0` 배치), `FG_PGRP = 108` (`HolderFgPgrpBatch`). 구 `Holder`(`!fg_pgrp_v1`) 접속 시 `Core`가 폴링 비활성화.
- `core` `SIGTERM` 핸들러: `GOODBYE::Detach` 전송 후 `BYE` 수신(또는 200ms 타임아웃)까지 대기해 `Holder`가 삭제된 구 바이너리로 재기동하지 않도록 보장.
- `ddg-terminal-server`: `HelloResult.features` `"agent_activity_v1"`, `PtyInfo`에 `agent_status`/`foreground` 추가, `PtyEvent::AgentStatusChanged`/`ForegroundChanged` 추가.
- `ddg-agents-host`:
  - `HostHelloResult.features` `"agent_activity_v1"`.
  - `CatalogSubscribeResult.activities: Vec<TabActivity>` (초기 원자 스냅샷; 중복 `host.tab_activity_snapshot` RPC 삭제).
  - 신규 RPC: **`host.tab_mark_seen`** (`TabMarkSeenParams { tab_id, run_generation, seen_seq }`, `deny_unknown_fields`, 오퍼레이터 전용).
  - 신규 알림: `host.tab_activity_changed`, `host.tab_seen_changed` — 반드시 **`catalog_subscribe` 전용 채널(`st.subs`, `after_reply` 게이트 적용)**로 전송해 스냅샷-델타 역전 차단.
  - `RoomCreateParams`/`TabCreateParams`/`TabSplitParams`/`SessionCreateParams`: `agent_profile: Option<String>` (`skip_serializing_if = "Option::is_none"`).
  - `host.session_create` 원격 호출 시 인증된 `caller`로부터 `parent_session_id` + `parent_host_id` 기록, `finish_exit()`는 `parent_host_id`가 로컬 일치/`None`일 때만 로컬 부모 조회.
  - `DDG_UPDATE_SCHEMA=1`로 `schema/ddg-terminal-server.openrpc.json` 및 `schema/ddg-agents-host.openrpc.json` 동기화.

### 3.3 불필요한 검증·락·폴백 제외 근거 (9건)
1. `TabActivity`/`seen_seq` 디스크 영속화 및 별도 락 제외(기존 `HostState` `Mutex` + 단조 클램프).
2. 내부 홉(`core`→`host`→`native`) 간 OSC 7501/`foreground` 재살균·재파싱 제외.
3. 호스트 측 `TabRecord.agent_profile` ↔ 로컬 `agents.json` FK 검증 제외(슬러그 구문 검증만).
4. CLI/`native`에서의 `agents.json` 검증 중복 구현 제외(`ddg-agents-transport::agent` 단일 호출).
5. `env` 값 엔트로피 스캔 제외(키 패턴+동적 로더 4종+값 비에코로 충분).
6. `programs.json`·`ui.json`의 `flock`/CAS 제외(읽기 전용 / 단일 GUI LWW).
7. 구버전 호스트 추측성 RPC 프로브 제외(`hello.features` 명시 협상).
8. SVG DOM 재작성 새니타이저 제외(`roxmltree` AST 화이트리스트 + 내장 폴백).
9. `sha2` 크레이트 추가 및 `catalog.json` `ConfigStamp` 제외(512 KiB 직접 바이트 비교 / `host.lock` 단독 소유).

---

## 4. 선택한 해법의 적용 범위 · 알려진 한계 · 재검토 조건
1. **Core `SIGKILL` 1 MiB `tail` 한계**: 마지막 OSC 7501 이후 1 MiB 초과 출력 뒤 강제 종료 시 상태가 `None`/OS `foreground`로 복구됨. *재검토 조건*: 실제 운용에서 Core 크래시/업그레이드 후 상태 유실이 빈발할 때.
2. **구버전 생존 `Holder` 위의 신 `Core`**: 무중단 업그레이드 직후 기존 `Holder`의 PTY는 `fg_pgrp_v1` 미지원으로 `foreground`가 `None` 유지(OSC 7501은 즉시 동작). *재검토 조건*: 무중단 업그레이드 직후 기존 셸 탭의 `foreground` 아이콘 유지가 필수 요구로 대두될 때.
3. **`DockerExec` 수동 업그레이드**: `install_remote_host`는 `Ssh|ContainerSsh`만 자동 업그레이드 지원. *재검토 조건*: `DockerExec` 자동 배포 요구 발생 시.
4. **단명 프로세스(1초 미만) 및 SIP/타 UID**: 1s 배치 폴링보다 짧은 명령은 미포착, 타 UID는 `None`. *재검토 조건*: 주요 래퍼 에이전트 식별 누락이 반복 관측될 때.
5. **오프라인 호스트 고아 키**: `agents.json` `host_overrides`와 `ui.json` `pinned_agents`/`manual_agent_order`는 오프라인 호스트 보호를 위해 미연결 시 자동 삭제하지 않고 상한(256/512)만 적용. *재검토 조건*: 머신 ID 변경 기능 도입 시 키 마이그레이션 추가.
