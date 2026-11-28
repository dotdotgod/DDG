# 설계: 내장 외부 CLI 에이전트 프리셋 (`default-agent-presets`)

- **작성자**: Antigravity (Gemini 4 Neon — Design & Technical Review Owner)
- **기반 요구사항**: 사용자 확정 결정 `A1`–`A4` + Astra 요구사항 브리프 (`requirements-brief.md`)
- **상태**: 구현·독립 검수 완료 (2026-11-28, T1–T5 및 macOS 실환경 S1–S5 전체 통과 · Windows/Linux 데스크톱 실기 미검증 항목 기록)

---

## 1. 확정 범위 요약 (`A1`–`A4`)

| 결정 | 확정 내용 | 설계 반영 |
|---|---|---|
| **A1 (프리셋 목록)** | `pi`, `antigravity`, `codex`, `opencode`, `claude-code` (5종) | `crates/ddg-agents-transport/src/presets.rs`에 고정 읽기 전용 카탈로그로 정의. 기존 `programs.rs` 및 `native/assets/icons/*.svg`와 1:1 정합 |
| **A2 (자동 추가)** | 앱 실행 시 탐색해 설치된 항목 자동 추가 + 사용자 커스텀 추가 유지 | 로컬 호스트 연결 완료 시 1회 탐지 → 발견 항목을 `agents.json` v1에 CAS로 자동 추가 (`default_agent_id` 미변경, 기존 프로필 미덮어쓰기). `ui.json`의 `seeded_preset_ids`로 삭제된 프리셋의 재실행 부활(zombie) 방지 |
| **A3 (GUI 노출)** | **설정 모달(`AgentSettingsModal`)에만 표시**, 런처는 저장된 프로필 전용 유지 | `AgentLauncher` 변경 없음. 설정 모달 좌측 하단에 대상 머신 기준 `Detected Presets` 패널 추가(재추가/원격 머신 확인/새로고침 지원) |
| **A4 (CLI P0)** | P0에 CLI 전용 프리셋 조회·추가 명령 포함 | `ddg agent presets [--host <H>] [--no-detect] [--json]`, `ddg agent add --preset <id> ...`, `ddg agent add --detected [--host <H>] [--json]` 추가. 기존 `ddg agent ls`/`ddg run` 계약 100% 유지 |

---

## 2. 단일 진실 원천: 내장 프리셋 카탈로그 (`ddg-agents-proto` · `ddg-agents-transport::presets`)

고정 탐지 허용 목록 상수(`BUILTIN_AGENT_PRESET_BINS = ["pi", "agy", "codex", "opencode", "claude"]`)는 `crates/ddg-agents-proto`에 두고, 실행기 메타데이터 표(`BUILTIN_AGENT_PRESETS`)는 `crates/ddg-agents-transport/src/presets.rs`에 둔다(`ddg-terminal-server`와 `ddg-agents-host` 모두가 `ddg-agents-proto`를 공유하므로 전 계층이 동일한 5종 허용 목록을 강제한다). 표 순서는 UI/CLI 표시 순서일 뿐 자동 기본값 우선순위가 아니다.

| `id` | `label` | `argv` | `detect_bin` | `icon` | `programs.rs`·SVG 정합 |
|---|---|---|---|---|---|
| `pi` | `Pi` | `["pi"]` | `"pi"` | `"pi"` | `pi` / `pi.svg` |
| `antigravity` | `Antigravity` | `["agy"]` | `"agy"` | `"antigravity"` | `antigravity` / `antigravity.svg` |
| `codex` | `Codex` | `["codex"]` | `"codex"` | `"codex"` | `codex` / `codex.svg` |
| `opencode` | `OpenCode` | `["opencode"]` | `"opencode"` | `"opencode"` | `opencode` / `opencode.svg` |
| `claude-code` | `Claude Code` | `["claude"]` | `"claude"` | `"claude-code"` | `claude-code` / `claude-code.svg` |

### 불변식 (단위 테스트로 강제)
1. 모든 프리셋은 `id`가 유효한 slug(`validate_agent_profile_slug`)이며 대소문자 무시 유일성을 만족한다.
2. 모든 프리셋의 `(id, label, icon)`은 `ProgramsConfig::default()`의 내장 항목과 정확히 일치한다.
3. 모든 프리셋의 `detect_bin == argv[0]`이며, `detect_bin`은 순수 basename(`[A-Za-z0-9._-]+`, 경로 구분자·`..` 금지)이다.
4. `argv`에는 모델 고정, 추론 수준 고정, 승인/샌드박스 생략(`--dangerously-skip-permissions`, `--yolo`, `--full-auto` 등), workspace trust 강제 플래그를 포함하지 않는다.
5. `AgentPreset::to_profile(&self) -> AgentProfile`은 `default_cwd: None`, `env: BTreeMap::new()`, `host_overrides: BTreeMap::new()`인 순수 v1 `AgentProfile`을 생성하며 `AgentsConfig::validate()`를 통과한다.
6. `agents.json` 스키마(`schema_version: 1`, `#[serde(deny_unknown_fields)]`)는 일절 변경하지 않는다.

### 순수 병합 함수 (`merge_auto_detected_presets`)
```rust
pub struct AutoSeedOutcome {
    pub added_ids: Vec<String>,
    pub seeded_ids_changed: bool,
}

pub fn merge_auto_detected_presets(
    config: &mut AgentsConfig,
    seeded_ids: &mut BTreeSet<String>,
    found_preset_ids: &[String],
) -> AutoSeedOutcome
```
동작 규칙:
- `found_preset_ids`의 각 ID를 `BUILTIN_AGENT_PRESETS`에서 조회한다(알 수 없는 ID는 무시).
- 이미 `config.find(id)`가 존재하면(대소문자 무시): 기존 프로필을 절대 수정하지 않고 `seeded_ids.insert(id)`만 기록한다(사용자가 나중에 수동 삭제해도 다음 부팅에 부활하지 않도록 보장; 상한 도달 이후에도 기존 프로필 이력 기록은 계속 수행).
- 이미 `seeded_ids.contains(id)`이면 건너뛴다(사용자가 삭제한 자동 추가 프리셋 재부활 방지).
- `config.agents.len() >= MAX_AGENT_PROFILES`이면 신규 추가만 건너뛴다(`continue`; 미추가 항목은 `seeded_ids`에 기록하지 않음).
- 그렇지 않으면 `preset.to_profile()`을 `config.agents`에 추가하고 `seeded_ids.insert(id)`와 `added_ids.push(id)`를 수행한다.
- **`config.default_agent_id`는 어떤 경우에도 변경하지 않는다.**

---

## 3. Data & Network Layer 설계 (3-Tier 탐지 프로토콜)

DDG에서 명시적 `argv`를 실제로 실행하는 주체는 `ddg-terminal-server`의 **Holder 프로세스**(`crates/ddg-terminal-server/src/pty.rs:105` `Command::new(program)`)다. 장수 Holder와 `ddg-agents-host` 데몬의 `PATH`가 다를 수 있으므로, 탐지는 반드시 **Holder의 실제 실행 환경**에서 수행한다.

### 3.1 Holder ↔ Terminal Server Core (`ddg-agents-proto::holder`, `ddg-terminal-server`)
- **Feature 협상**: `HolderWelcome.features`에 `"probe_bins_v1"` 추가.
- **프레임 타입** (`crates/ddg-agents-proto/src/holder.rs`):
  - `QUERY_BINS: u8 = 9` (Core → Holder, `pty_id` 헤더 필드에 `request_id: u32` 저장)
  - `BINS_RESULT: u8 = 109` (Holder → Core, `pty_id` 헤더 필드에 `request_id: u32` 반사)
- **페이로드 (고정 허용 목록 전용)**:
  - `HolderProbeBins {}` (임의 파일명 수신 금지; Holder가 `ddg_agents_proto::BUILTIN_AGENT_PRESET_BINS` 5종만 고정 조회)
  - `HolderProbeBinsResult { items: Vec<HolderBinStatus> }`
  - `HolderBinStatus { bin: String, status: BinProbeStatus }` (`Found | NotFound | Unknown`)
- **Holder 비차단 실행 및 OS 탐지 알고리즘 (`holder.rs` — 비실행·메타데이터 전용)**:
  1. **동기 루프 비차단**: `QUERY_BINS` 수신 시 Holder의 동기 명령/PTY 입력 루프에서 파일시스템 I/O를 직접 수행하지 않고, 전용 단발/단일 비행 워커 스레드(`in_flight_probe` 상한 1건; 동시 요청 시 진행 중 대기자에 합류하거나 즉시 응답)에서 PATH 검사를 실행한 뒤 `BINS_RESULT`를 회신한다.
  2. `std::env::var_os("PATH")` 읽기. 없거나 비어 있으면 모든 항목 `Unknown`.
  3. `std::env::split_paths(&path_os)` 순회 시 비어 있거나 상대 경로(`!dir.is_absolute()`)인 요소가 있으면 `has_relative_path = true`로 기록하고 해당 요소는 건너뛴다(cwd 섀도잉 방지).
  4. **Unix**: 각 절대 디렉터리 `dir`에서 `candidate = dir.join(bin)`에 대해 `fs::metadata(&candidate)`가 `is_file()`이고, 동시에 유효 권한 검사 `libc::access(c_path, libc::X_OK) == 0`을 통과해야만 `Found`(소유자만 실행 가능한 `0o700` 타인 파일·`noexec` 마운트에 대한 `mode & 0o111` 거짓 양성 제거; 깨진 symlink·디렉터리는 미발견). 어느 절대 경로에서도 발견되지 않았으나 `has_relative_path == true`이면 단정하지 않고 `Unknown`, 아니면 `NotFound`.
  5. **Windows**: `CreateProcessW`가 셸 래퍼 없이 직접 실행할 수 있는 `.exe` 대상(`dir.join(bin)`이 `.exe`이거나 `dir.join(format!("{bin}.exe"))`가 일반 파일)만 `Found`. `.exe`는 없으나 동명 `.cmd`/`.bat`/`.ps1` 심(shim)이 존재하거나 `has_relative_path == true`이면 오탐/미탐을 감추지 않고 `Unknown`, 둘 다 없으면 `NotFound`.
  6. **금지 사항**: 바이너리 실행, `--version`/`--help` 호출, `which`/셸 문자열 조립, rc 파일 source, 응답에 실제 `PATH`·파일 경로·환경 변수 포함 일체 금지.

### 3.2 Terminal Server Core ↔ Host (`ddg-agents-proto::terminal`, `ddg-terminal-server::core`)
- **RPC 메서드**: `pty.probe_bins`
  - Params: `PtyProbeBinsParams {}` (`#[serde(deny_unknown_fields)]` — 내부 소켓 직접 접근자도 임의 바이너리명 조회 불가, `BUILTIN_AGENT_PRESET_BINS` 5종 고정)
  - Result: `PtyProbeBinsResult { items: Vec<BinProbeItem> }` (`BinProbeItem { bin: String, status: BinProbeStatus }`)
- **동작 및 대기자 정리**:
  - Core는 연결된 Holder가 `"probe_bins_v1"`을 광고하는지 확인한다.
  - Holder가 미연결이거나 `"probe_bins_v1"`이 없으면 5종 모두 `BinProbeStatus::Unknown` 반환(호스트 데몬 자체 PATH로 추측 금지).
  - Holder에 `QUERY_BINS`를 보내고 **3초 타임아웃**으로 `BINS_RESULT`를 대기한다. `Drop` 가드로 타임아웃·호출자 취소·Holder 단절 시 `pending_probes` 엔트리를 즉시 제거해 누수를 방지하고, 실패 시 5종 모두 `Unknown` 반환.
- **OpenRPC**: `schema/ddg-terminal-server.openrpc.json`에 `pty.probe_bins` 스냅샷 반영.

### 3.3 Host ↔ Client (`ddg-agents-proto::host`, `ddg-agents-host`, `ddg-agents-transport`)
- **Feature 협상**: `HostHelloResult.features`에 `"agent_presets_v1"` 추가 (`crates/ddg-agents-host/src/server.rs:427-432`).
- **RPC 메서드**: `host.agent_presets_detect`
  - Params: `HostAgentPresetsDetectParams {}` (`#[serde(deny_unknown_fields)]` — **클라이언트로부터 바이너리명·경로·명령을 일절 받지 않음**)
  - Result: `HostAgentPresetsDetectResult { items: Vec<AgentPresetDetection> }`
  - `AgentPresetDetection { id: String, status: PresetDetectStatus }` (`found | not_found | unknown`)
- **접근 경계 구분**:
  - 프로토콜/채널 경계: 운영자 연결(`server.rs`)에서만 디스패치하며 `plugin_host.rs` / `hub.rs` 세션 토큰 릴레이에는 비노출.
  - OS 사용자 경계: `host.sock`/`terminal.sock`은 동일 OS 사용자 소켓(`0o600`)이므로 디스패치 분리만으로 동일 사용자 PTY 프로세스의 소켓 접근 자체를 OS 격리할 수 없다. 대신 `host.agent_presets_detect`와 `pty.probe_bins` 양쪽 모두 빈 파라미터 + 5종 고정 허용 목록 + 비실행 메타데이터 조회로 제한해 소켓 직접 호출 시에도 공격 면을 확대하지 않는다.
- **호스트 처리 흐름**:
  1. `host.term()`으로 `pty.probe_bins {}` 호출 (3초 타임아웃; `TermConn` pending map 누수가 없도록 호출 취소/타임아웃 시 Drop 가드 정리).
  2. `term()` 미가용·RPC 미지원·타임아웃 시 5개 모두 `PresetDetectStatus::Unknown` 반환(`NotFound`로 오인 금지).
  3. 성공 시 `BUILTIN_AGENT_PRESETS`의 `detect_bin`과 `BinProbeStatus`를 1:1 매핑해 반환.
- **클라이언트 (`HostClient::detect_agent_presets`)**:
  - `supports_agent_presets()`가 `false`이면 RPC를 호출하지 않고 `PresetDetectOutcome::Unsupported` 반환(구버전 호스트 100% 하위 호환).
  - RPC 호출 타임아웃: **4초**(`HostClient` 내부 pending entry를 Drop/timeout 경로에서 반드시 정리해 누수 차단).
  - 응답 검증 경계: 최대 32건 초과 또는 중복 ID 포함 응답은 오류(`Protocol`)로 거부. 알 수 없는 프리셋 ID는 무시(실행·저장 금지). 클라이언트가 아는 5종 중 응답에서 누락된 ID는 `Unknown`으로 채운다.
- **OpenRPC**: `schema/ddg-agents-host.openrpc.json`에 `host.agent_presets_detect` 스냅샷 반영 (`DDG_UPDATE_SCHEMA=1`).

---

## 4. UX & Presentation Layer 설계 (`native/`)

### 4.1 앱 실행 시 자동 탐색 및 자동 추가 (`A2`)
1. **상태 저장 (`UiPrefs`, `native/src/ui_scale.rs`)**:
   - 기존 `ui.json` (`UI_PREFS_SCHEMA_VERSION = 2`)에 하위 호환 필드 추가:
     ```rust
     #[serde(default)]
     pub seeded_preset_ids: BTreeSet<String>,
     ```
   - `config_watch.rs`의 `UiPrefs` 리로드 및 저장 경로에서 `seeded_preset_ids`가 손실되지 않도록 보존한다.
2. **트리거 조건**:
   - `HostTransport::Local` 머신이 핸드셰이크를 완료(`HostClient::hello()`에서 `(conn_id, boot_generation, features)`를 직접 캡처; `StateChanged(Connected)`가 `boot_generation`을 갖지 않으므로 반드시 브리지 연결 스냅샷 기준 사용)하고 `supports_agent_presets()`가 참일 때, 해당 `(conn_id, boot_generation)`에 대해 아직 시작 자동 시드를 수행하지 않았으면 1회 비동기 `bridge.detect_agent_presets(host_id, DetectPurpose::StartupAutoSeed)`를 발송한다.
   - 원격 머신 연결은 공유 `agents.json`을 자동으로 변경하지 않는다(로컬 자동 시드 전용; 원격 머신에서 발견된 프리셋은 설정 모달이나 CLI로 명시 추가).
3. **응답 처리 (Fail-closed Write-Ahead 이력 예약 · 비파괴 · CAS 보장)**:
   - `(host_id, conn_id, boot_generation)`이 현재 로컬 연결 스냅샷과 불일치하면 폐기한다.
   - `Found` 상태인 프리셋 ID가 1건 이상일 때만 진행한다.
   - **사전 `ui.json` Fail-closed 확인**: `UiPrefs::load(&ui_path)`가 `ErrorKind::NotFound`가 아닌 오류(손상·권한 오류)를 반환하면 **자동 시드를 중단**한다(손상된 `ui.json`을 빈 집합으로 폴백해 삭제된 프리셋을 부활시키거나 기존 UI 설정을 덮어쓰는 사고 차단).
   - **사전 `agents.json` Fail-closed 확인**: `AgentsConfig::load(&agents_path)`가 `Err(AgentConfigError::Corrupt { .. })` 또는 기타 오류를 반환하면 **즉시 중단**한다(손상된 `agents.json` 자동 덮어쓰기/백업 초기화 금지). 파일 부재는 정상 빈 v1으로 간주한다.
   - `merge_auto_detected_presets(&mut config, &mut seeded_ids, &found_ids)` 실행:
     - **크래시 세이프 저장 순서 (Write-Ahead Reservation + `ui.json` 직렬화 병합)**:
       1. `UiPrefs` 모든 저장 경로(배율·정렬·접힘·시드 포함)에 파일 락 기반 read-modify-write 직렬화를 적용하고 `seeded_preset_ids`를 비파괴 합집합으로 병합해, 다중 창/동시 저장 시 이력이 유실되지 않게 보장한다.
       2. `seeded_ids_changed == true`이면 **`agents.json` 저장 전에 먼저** `ui.json`에 `seeded_preset_ids`를 지속(예약)하고, **이번 락 트랜잭션에서 실제로 새로 추가된 예약 집합 `newly_reserved_ids`**만 소유한다. `ui.json` 저장이 실패하면 자동 시드를 즉시 중단한다(`agents.json` 미수정).
       3. `ui.json` 예약 성공 후 `newly_reserved_ids` 중 미등록 항목만 `config.save(&agents_path, &stamp)`로 저장한다(`Conflict` 시 1회 재로드하되, 재병합 시에도 반드시 **이번 호출이 소유한 `newly_reserved_ids` 중 여전히 미등록이고 이번 호출에서 관측한 동시 삭제 대상이 아닌 항목만** 적용; 재충돌 시 중단하며, 드문 과잉 억제 시에도 사용자는 설정 모달 `Detected` 또는 CLI `ddg agent add --preset`으로 언제든 명시 추가 가능).
       4. 이 순서 덕분에 `agents.json` 저장 직후 앱이 비정상 종료되고 CLI가 해당 프로필을 삭제한 뒤 GUI를 재시작해도, `seeded_preset_ids`가 이미 지속되어 있으므로 삭제된 프로필이 부활하지 않는다.
     - 또한 GUI의 `AgentSettingsModal`에서 사용자가 내장 프리셋 ID를 삭제 후 `Save`할 때도 `ui.json`의 `seeded_preset_ids`에 해당 ID를 직렬화 합집합 기록한다.
   - 모달 편집 중 충돌 방지: `AgentSettingsModal`이 이미 열려 있더라도 모달의 미저장 초안은 건드리지 않는다(모달 저장 시 기존 CAS 충돌 배너가 안전하게 안내).
   - **보장 범위 명시**: GUI 시작 자동 시드 비부활 보장은 GUI가 `ui.json`에 기록한 시드 이력 기준이다. GUI를 한 번도 실행하지 않은 상태에서 CLI로 프리셋을 추가→삭제한 뒤 GUI를 최초 실행하면(`agents.json` v1에 삭제 tombstone 없음), GUI는 최초 시드로 간주해 1회 자동 추가할 수 있다(알려진 경계로 기록).

### 4.2 Agent Launcher (`A3`)
- `native/src/agent_launcher.rs` 및 `native/src/main.rs`의 런처 팝오버는 **변경하지 않는다**(저장된 프로필 전용 유지).
- 앱 시작 시 자동 추가가 완료되면 `self.agents`가 갱신되어 사용자가 별도 설정 없이도 런처에서 설치된 프리셋을 바로 실행할 수 있다.

### 4.3 Agent Settings Modal (`A3` — `native/src/agent_modal.rs`, `native/src/main.rs`)
- **위치**: 설정 모달 좌측 선택 열(`agent-settings-selector`) 하단에 구분선과 함께 `Detected Presets` 섹션을 추가한다.
- **대상 머신 독립성**:
  - `modal.detect_host: String` (모달을 열 때 현재 활성 머신 ID로 초기화)을 별도 필드로 둔다.
  - 기존 `modal.host: Option<String>`(**Per-Machine Overrides** 편집용)과 절대 혼용하지 않는다.
  - 상단 헤더: `"Detected"` 라벨 + 머신 선택 칩(로컬/연결된 원격 머신 전환 시 해당 머신 재탐지) + `[↻]` 새로고침 버튼.
- **상태별 표시 (5가지 상태 명확 구분 — All-`Unknown`을 0건으로 오보 금지)**:
  - `Loading`: `"Checking…"` (입력·편집·저장·취소 차단 금지)
  - `Unsupported`: `"Detection unsupported"`
  - `Unavailable(reason)`: `"Unavailable"` (오프라인/타임아웃/오류, 또는 **`Found`가 0건이면서 1건 이상 `Unknown`인 경우** — 구형 Holder·타임아웃을 `"No presets found"`로 오보 금지)
  - `Loaded (All NotFound)`: 모든 5종이 확정 `NotFound`(즉 `Found == 0 && Unknown == 0`)일 때만 `"No presets found"` 표시
  - `Loaded (Found >= 1)`: `Found` 항목에 아이콘 + 라벨 표시(일부 `Unknown`이 섞여 있으면 하단에 `"Some presets could not be checked"` 보조 문구 병기):
      - `modal.config.find(preset.id).is_some()`이면 비활성 `"Added"` 배지 표시(중복 추가 방지)
      - 미등록이면 클릭 가능한 `[+ <Label>]` 버튼 → `modal.add_preset(preset)` 호출:
        - 현재 편집 중인 필드 `commit_fields()` 검증 → 실패 시 오류 표시하고 중단
        - `preset.to_profile()`을 `modal.config.agents`에 추가, 해당 행 선택, `Field::Command` 포커스
        - 사용자가 `Save`를 눌러야 실제 `agents.json`에 커밋됨(초안 검토·커스터마이즈·취소 가능, `default_agent_id` 미변경)
- **단일 비행(Single-Flight) 및 Stale 폐기**:
  - `host_bridge.rs`는 호스트당 진행 중인 탐지 요청을 최대 1건으로 제한(중복 요청 합류 또는 이전 요청 취소).
  - 모달은 단조 증가 `detect_request_id: u64`를 보유하고, `AgentPresetsDetected` 이벤트의 `request_id` 또는 `(host_id, conn_id, boot_generation)`이 불일치하면 폐기한다.
  - 캐시: 모달 수명 동안만 메모리 보관(영구 캐시 금지, 새로고침/호스트 재연결 시 무효화).

```text
+----------------------------------------------------------------------+
| Agent Settings                                                   [X] |
+----------------------+-----------------------------------------------+
| [+ Add]              | ID [ pi          ]  Name [ Pi              ]  |
| > Pi                 | Launch command [ 'pi'                      ]  |
|   Codex              | [pi]                                          |
|                      | Icon [ pi        ] (icon) [ Set as Default ]  |
| -------------------- |                                               |
| Detected  [local][↻] | Environment Variables          [Show Values]  |
|  Pi         Added    | ...                                           |
|  Codex      Added    | Per-Machine Overrides  [vps]                  |
|  [+ Antigravity]     | ...                                           |
+----------------------+-----------------------------------------------+
| [Delete]                                                      [Save] |
+----------------------------------------------------------------------+
```

---

## 5. CLI P0 인터페이스 설계 (`A4` — `crates/ddg-agents-cli/`)

기존 `ddg agent ls` / `show` / `add` / `set` / `default` / `rm` 및 `ddg run`의 JSON 출력·인자 해석은 100% 하위 호환을 유지한다.

### 5.1 `ddg agent presets [--host <H>] [--no-detect] [--json]`
- **동작**:
  - `--no-detect`가 없으면 글로벌 `-H/--host`(기본 로컬/기본 호스트)에 연결해 `client.detect_agent_presets()` 호출.
    - 호스트가 `"agent_presets_v1"` 미지원이면 오류 대신 `detection_supported: false`, 모든 항목 `status: "unknown"`으로 정상 출력.
    - 호스트 연결 실패/오프라인이면 명시적 연결 오류 반환(exit non-zero, 0건으로 오인 금지).
  - `--no-detect`이면 호스트 연결 없이 정적 5종 카탈로그를 `detection_supported: false`, `status: "unknown"`으로 출력.
  - `AgentsConfig::load(&agents_path)`로 `registered: bool` 계산(파일 부재는 빈 목록, 손상 시 기존 `AgentConfigError` 반환).
- **JSON 출력**:
  ```json
  {
    "host_id": "local",
    "detection_supported": true,
    "presets": [
      {
        "id": "pi",
        "label": "Pi",
        "argv": ["pi"],
        "icon": "pi",
        "status": "found",
        "registered": true
      }
    ]
  }
  ```
- **텍스트 출력**: `ID`, `LABEL`, `COMMAND`, `STATUS`, `REGISTERED` 정렬 테이블.

### 5.2 `ddg agent add --preset <id>` & `ddg agent add --detected`
기존 `ddg agent add <id> -- <argv...>`와 충돌하지 않도록 상호 배타 규칙을 명확히 한다.

1. **`ddg agent add --preset <id> [--label <L>] [--icon <I>] [--cwd <D>] [--env K=V]... [--default] [--json]`**
   - 위치 인자 `<id>`와 `-- <argv>`는 `--preset`과 동시 사용 불가(명확한 usage 오류).
   - `<id>`를 `BUILTIN_AGENT_PRESETS`에서 대소문자 무시 검색 → 없으면 `preset_not_found`.
   - 프리셋의 `id`, `label`, `argv`, `icon`을 기본값으로 채우고, 사용자가 명시한 `--label`/`--icon`/`--cwd`/`--env`/`--default` 옵션을 적용.
   - 이미 등록된 ID면 기존과 동일하게 `agent_exists` 오류(덮어쓰기 금지).
   - `--default`를 명시한 경우에만 `default_agent_id` 설정.
2. **`ddg agent add --detected [--host <H>] [--json]`**
   - `--preset`, 위치 인자 `<id>`, `-- <argv>`, `--label`/`--icon`/`--cwd`/`--env`/`--default`와 동시 사용 불가(usage 오류).
   - 대상 호스트에 연결해 `client.detect_agent_presets()` 호출. 미지원·연결 실패·또는 **`Found == 0 && Unknown > 0`(판정 불가)**이면 `presets_unavailable` 오류 반환(파일 미생성/미수정; 확정 0건과 구분).
   - `Found` 상태인 프리셋 중 `agents.json`에 아직 없는 항목만 단일 CAS 트랜잭션으로 추가. 이미 등록된 항목은 `skipped`로 보고하며 수정하지 않음. `default_agent_id`는 절대 변경하지 않음.
   - 일괄 추가 시 `MAX_AGENT_PROFILES`(128)를 초과하면 부분 저장 없이 기존 용량 초과 오류로 전체 롤백(파일 무변경).
   - 새로 추가된 항목이 0건이면(확정 `NotFound` 또는 모두 이미 등록됨) `agents.json`에 쓰지 않음(원본 바이트/미존재 상태 보존).
   - 일부 `Unknown`이 함께 존재하면 `unknown` 배열에 별도 보고한다.
   - JSON 출력: `{ "host_id": "...", "added": ["pi", "codex"], "skipped": ["antigravity"], "unknown": [] }`.

---

## 6. 보안·신뢰 경계 요약

| 경계 | 통제 규칙 |
|---|---|
| **RPC 입력 (`host.agent_presets_detect`)** | 빈 파라미터 `#[serde(deny_unknown_fields)]`만 허용. 클라이언트가 임의 바이너리명·경로·셸 문자열 주입 불가. 운영자 연결 전용(`plugin_host` 비노출). |
| **Holder 탐지 (`holder.rs`)** | `std::env::var_os("PATH")`의 절대 경로 요소만 `fs::metadata` 조회. 바이너리 실행·셸 호출·rc 파일 읽기·상대 PATH 신뢰 금지. 실제 경로/PATH 문자열 응답 미포함. |
| **클라이언트 응답 소비** | 항목 수 상한(32), 중복 ID 거부, 모르는 ID 무시, 누락 ID는 `unknown`. 클라이언트는 자신의 정적 카탈로그에서만 `label`/`argv`/`icon` 결정. |
| **설정 파일 저장** | `agents.json` v1 `flock` + `ConfigStamp` CAS + `0o600` 임시파일 rename 유지. 손상 파일 자동 복구/덮어쓰기 금지. `default_agent_id` 자동 설정 금지. |
| **실행 플래그 안전성** | 승인 생략/샌드박스 해제/모델 고정 플래그 내장 금지. 발견 후 실제 실행 실패 시 자동 대체 실행(다른 바이너리/셸 폴백) 금지. |

---

## 7. 알려진 한계 및 후속 검토 항목
1. **macOS Finder 실행 PATH**: `DDG.app`을 터미널이 아닌 Finder/Spotlight에서 실행해 Holder가 최소 `PATH`(`/usr/bin:/bin:...`)를 상속한 경우, Holder 탐지와 실제 `Command::new(program)` 모두 동일한 최소 `PATH`를 보므로 거짓 양성은 없으나 `~/.local/bin`·`~/.pi/agent/bin`의 CLI가 미발견될 수 있다. 데몬 로그인 셸 PATH 로딩은 별도 마일스톤으로 분리한다.
2. **Codex 실행 수명 확인**: 내장 `argv`는 `["codex"]`로 시작하되, 실기 검증 시 DDG 관리 PTY에서 시작·detach/reattach·정상 종료에 문제가 없는지 확인한다(문제 발견 시 별도 이슈로 플래그 재검토).
3. **CLI 자동 시드 미수행**: CLI(`ddg`)는 명시 명령(`ddg agent add --detected` / `--preset`)으로만 수정하며, `ddg agent ls`나 `ddg run`이 백그라운드로 `agents.json`을 생성·수정하지 않는다.
