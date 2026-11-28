# 최종 완료 보고서 (`FINAL.md`): 탭 아이콘 · OSC 7501 에이전트 상태 · 에이전트 리스트 · `ddg` 기본 에이전트 설정

- **보고 대상**: 전체 작업 종료 보고 (`T0`–`T6-A`, `T6-C` 완료 · `T6-B` 사람 GUI 실기 `Unverified` 별도 추적, 사용자 명시 승인에 따른 종료·아카이브 이관)
- **보고 일자**: `2026-11-26`
- **완료 상태**: **구현·자동/실환경 검증 완료(`T0`–`T6-A`) · 아카이브 이관 완료(`T6-C`) · 사람 GUI 실기(`T6-B`) `Unverified`**
- **보관된 계획**: [docs/archive/plan/agent-status-icons-profiles/README.md](../../../plan/agent-status-icons-profiles/README.md)
  - 상세 설계·테스트·시나리오·태스크: [DESIGN_UI.md](../../../plan/agent-status-icons-profiles/DESIGN_UI.md) · [DESIGN_DATA.md](../../../plan/agent-status-icons-profiles/DESIGN_DATA.md) · [TESTS.md](../../../plan/agent-status-icons-profiles/TESTS.md) · [SCENARIOS.md](../../../plan/agent-status-icons-profiles/SCENARIOS.md) · [TASKS_CORE.md](../../../plan/agent-status-icons-profiles/TASKS_CORE.md) · [TASKS_HOST_UI.md](../../../plan/agent-status-icons-profiles/TASKS_HOST_UI.md)
- **선행 보고서(보존)**: [PLANNING.md](PLANNING.md) · [BATCH_T0_T6A.md](BATCH_T0_T6A.md) · [보고서 목차](README.md)
- **브랜치 / `main` 머지 상태**: 미수행 (워크스페이스에 `.git` 메타데이터 부재 · `git push`/머지 미실행)

---

## 1. 요구사항 · 설계 결정 기록 및 제외된 대안

### 확정 결정 출처 (`Phase B`–`Phase E`)
- **요구사항 (`A1`–`A6`)**:
  - `A1`: 전용 `ddg run [agent]`, `ddg agent ...`, 기존 `room create`/`session create`/`session split`의 `--agent <id>` 추가; 인자 생략 시 `DefaultShell` 100% 호환 유지; `docs/spec/requirements.md` §1 공식 갱신.
  - `A2`: `ddg run` 기본 가시 룸 + 즉시 대화형 attach, `--bg`/`--background` 시 숨김 백그라운드 생성.
  - `A3`: `~/.config/ddg/agents.json`(+ `host_overrides`)을 CLI·GPUI GUI 공유 저장소로 채택; 일반 `+` 탭은 `DefaultShell` 유지 + 별도 `AgentLauncher` 제공.
  - `A4`: OSC 7501 보고 세션 + `ddg` 에이전트 실행 세션을 리스트에 포함; 숨김 `rm_bg_*`는 실행 중만 노출·종료 즉시 제거.
  - `A5`: 내장 SVG 카탈로그 + OSC 7501 `app` + OS `foreground` 최소 식별자(`comm` + 인터프리터 스크립트 basename 1개, 전체 `argv` 비노출) + 로컬 `~/.config/ddg/icons/`·`programs.json` 오버라이드 1차 포함(원격 호스트는 내장 키 전용).
  - `A6`: P0 + P1 단일 마일스톤 일괄 구현(핀, `priority`/`grouped`/`manual` 정렬, 수동 순서, GUI `AgentSettingsModal` 포함).
- **UX (`UX-1`–`UX-3`) & 세부 정책 (`D-1`–`D-4`)**:
  - `UX-1` / `D-1`: 백그라운드 Peek는 `want_control=false`(`Readonly`)로 연결하며 배너 `[제어 요청]`으로 임시 제어 허용; `Readonly` `Esc`=Peek 닫기, `Control` `Esc`=터미널 전달·`Cmd/Ctrl+W`/버튼=Peek 닫기(절대 `close_tab` 호출 금지) + 획득 제어권 자동 반납(`D-SEC-09` 3분기).
  - `UX-2` / `D-2`: 핀/비핀 섹션 내부에서 부모+자식 서브트리 단위 수동 이동 시 현재 표시 순서를 스냅샷하며 `manual` 모드 자동 전환; 런처 4지선다(`[현재 룸 새 탭]|[분할]|[새 룸]|[백그라운드]`).
  - `UX-3`: 접이식 2단 사이드바(상단 `에이전트` max 45% height, `룸` 접힘 시 해제 + 하단 `룸`); 외부 셸 `ddg run`은 현재 `cwd` 새 가시 룸 생성.
  - `D-3`: `ddg agent show` 기본 `env` 값 마스킹(`--reveal-env` 평문), 동적 로더 4종(`LD_PRELOAD`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, `DYLD_FORCE_FLAT_NAMESPACE`) 차단·`LD_LIBRARY_PATH` 허용.
  - `D-4`: `host_overrides.env: BTreeMap<String, Option<String>>` (`null`로 상속 키 제거); 구버전 원격 호스트 업그레이드 시 PID 파일 부재 환경에서 `ps -u "$(id -u)" -o pid=,args=` 정확 매칭으로 `host`→`core`만 안전 종료(`holder`·PTY 보존) 후 `hello.features` `"agent_activity_v1"` 검증.
- **통합 계약 (`RC-01`–`RC-06`)**:
  - `AgentState::{ Idle, Working, Blocked, Done, Error }` + `BlockedKind::{ Permission, Question, Auth }` 통일.
  - `HostState.activities` RAM 전용(`catalog.json` `save_catalog()` 비트리거), 오퍼레이터 채널에는 `Standard` + 실행 중 `rm_bg_*` 포함, `plugin_host.rs`·`hub.rs` 경계에서만 `rm_bg_*`·`tab_mark_seen` 격리.
  - `agents.json` v1 `argv: Vec<String>`, 손상 백업 `agents.json.corrupt.<ts>[.<n>]` (`0o600`, `create_new`, `O_NOFOLLOW`).
  - `ui.json` v2 `collapsed_sections: BTreeSet<String>` 단일화 + `ui_scale.rs`/`config_watch.rs` 필드 유실 버그 수정.
  - `MAX_ICON_BYTES = 128 KiB`, `roxmltree` XML AST 화이트리스트 + `512px` OOM 가드 + `custom/<name>#<fnv64>`.

### 주요 제외 대안 및 근거
- `catalog.json` v3 마이그레이션 → 기존 v2 무중단 하위 호환 및 `HostHelloResult.features = ["agent_activity_v1"]` 협상으로 대체.
- 문자열 기반 SVG 블랙리스트 및 외부 `sha2` 크레이트 추가 → `Cargo.lock`에 이미 존재하는 `roxmltree` XML AST 화이트리스트와 표준 FNV-1a 64-bit 해시로 대체(신규 외부 크레이트 0건).
- `turn` 카운터 및 별도 `tab_activity_snapshot` RPC → `catalog_subscribe`의 원자적 `activities` 스냅샷 + `after_reply` 게이트 델타 채널로 레이스 제거.
- GPUI `with_animation().repeat()` 60/120Hz 상시 스피너 → `Desktop::drain()`에서 `has_visible_working_spinner()` 참일 때만 125ms(8Hz) 틱, 유휴 시 0Hz.
- 구형 에이전트 CLI 래퍼 자동 주입 및 프로젝트 로컬 `.ddg/config.json` 자동 전환 → 보안 경계(RCE) 분리를 위해 P2 보류.

---

## 2. 역할별 산출물 · 실행 결과 및 사후 수정 이력

- **역할 분리**: 계획·설계·보안/기술 리뷰·최종 검수·문서화 = Antigravity (Gemini 4 Neon High), 모든 Rust/Cargo/스키마 구현 및 테스트 = 로컬 `pi` (`openai/gpt-6.2-sol`, `roles/implementer.md`).
- **태스크 완료 매트릭스**:
  - `T0` (환경 점검 · `requirements.md` §1): **완료**
  - `T1` (`ddg-agents-proto` + OpenRPC 1.3 스냅샷): **완료** — 35 unit + 2 compat = **37 PASS**, `rustfmt` PASS
  - `T2-A` (`ddg-agents-vt` OSC 7501 · FIFO `Relay`) ∥ `T2-B` (`ddg-agents-transport` 설정 · 업그레이드): **완료** — `vt` **17 PASS**, `transport` **33 unit + 1 local integration PASS** (`docker_live` 1 ignored), `rustfmt` PASS
  - `T3` (`ddg-terminal-server` 상태 · OS `foreground` · `core.pid`): **완료** — 19 unit + 4 integration = **23 PASS**
  - `T4-A` (`ddg-agents-host` RAM `activities` · 보안 격리 · `host.pid`) ∥ `T4-B` (`ddg-agents-cli` `ddg run`/`agent`/`--agent`): **완료** — `host` 30 unit + 16 integration = **46 PASS**, `cli` 25 unit + 5 `cli_e2e` = **30 PASS**, `rustfmt` PASS
  - `T5` (`native/` GPUI 클라이언트): **완료** — **53 PASS** + `--check-vt` (`libghostty-vt initialized`) PASS + `rustfmt` PASS
  - `T6-A` (워크스페이스 회귀 + 실환경 시나리오 `A1`–`A3`): **완료** — `A1-core` PASS, `A2` PASS, `A1-live-agents` 실측 완료(`pi 1.6.1`·`codex 0.166.0`), `A3` Docker 멀티호스트·구버전 무중단 업그레이드 PASS (잔존 컨테이너/프로세스 0개)
  - `T6-B` (사람 GUI 실기 `H1`–`H2`): **`Unverified`** (사용자 선택에 따라 미실행 상태로 기록하고 별도 추적)
  - `T6-C` (최종 보고서 · 아카이브 이관): **완료** (`docs/archive/plan/agent-status-icons-profiles/` 이관 및 인덱스·상대 링크 갱신 완료)
- **독립 검수 중 발견·조치된 3건 (전건 수정 및 재검증 PASS)**:
  1. `T2-B` `agent.rs`가 `a..b`를 허용하던 슬러그 검증 불일치 → `proto::host::validate_agent_profile_slug` 직접 호출로 통일.
  2. `T1`의 `sanitize_display_text` `U+2028`/`U+2029` 제거 강화로 인한 `cli/src/msg.rs:196` 실패 → `plain()`에서 사전 공백 치환으로 복구.
  3. `T5`의 GPUI 0.2.2 Taffy 정수 픽셀 스냅에 따른 `ui_zoom_resizes_the_tab_bar_and_sidebar` 반올림 오차(43px vs 43.5px) → `tab_bar` 높이·`canvas.y` 반올림 정합화.
- **필수 범위 밖 작업 검토**:
  - `T5`에서 `native/Cargo.toml`에 `roxmltree`·`libc`를 직접 의존성으로 추가함에 따라 루트 `Cargo.lock`이 자동 동기화됨(`Cargo.lock`에 이미 존재하던 크레이트 재사용, 신규 외부 다운로드 없음).
  - 병렬로 진행 중이던 타 세션(`ddg-upd-t2`)이 본 계획 범위 밖 크레이트 `crates/ddg-agents-update`를 편집 중이었으므로 최종 통합 검증은 `--workspace --exclude ddg-agents-update`로 수행함(본 계획은 `ddg-agents-update`를 전혀 수정하지 않았음).

---

## 3. 비가역 실행 결정 기록

| 대상 | 영향 및 비가역 이유 | 실행 여부 | 승인·권한 조건 |
|---|---|---|---|
| `main` 브랜치 머지 및 원격 `git push` | 브랜치 히스토리 변경 및 원격 공개 | **미수행** | 워크스페이스 `.git` 부재 및 사용자 명시 승인 필요(보고서 작성·아카이브 이관은 머지 승인이 아님) |
| 원격 운영 호스트 / 실 `~/.config/ddg` 변경 | 사용자 설정·세션 영향 | **미수행 (범위 외)** | 모든 실환경 시나리오는 `mktemp -d` + 루프백 Docker 격리 환경에서만 수행 후 완전 삭제 |

---

## 4. 미검증 항목 (`Unverified`) 및 남은 위험

1. **`T6-B` 사람 GUI 실기 (`H1`, `H2`) — `Unverified` (`R-03`)**: GPUI 0.2.2 실창의 2단 사이드바(45% cap·`룸` 접힘 시 해제·160px 축약), 22px 분할 헤더(50%/100%/150% 줌), `observe_window_activation` 창 복귀 `seen` 해제, 유휴 0Hz CPU, `AgentLauncher`/`AgentSettingsModal` 실창 상호작용, 로컬 SVG 핫스왑 래스터화는 아직 사람이 직접 확인하지 않음.
2. **외부 에이전트 실측 범위 — `Unverified` (`R-02`)**: `claude` CLI 미설치, `pi`/`codex` 유료 LLM 라이브 턴.
3. **비-macOS 데스크톱 및 WAN SSH — `Unverified` (`R-01`, `R-04`)**: Linux X11/Wayland GPUI 실창, 비표준 원격 로그인 셸의 `ps` 동작, WAN 패킷 손실 환경.
4. **알려진 한계**: `modal_text_input`의 한글 IME 조합/중간 커서 미지원; `core` `SIGKILL` 시 1 MiB `tail` 초과 출력 직전의 OSC 7501 상태 유실 가능.

---

## 5. 향후 `main` 머지 / Squash 커밋 본문 인계 초안

Git 저장소에서 본 변경분을 머지하거나 squash 커밋할 때 아래 내용을 본문에 포함하십시오:

```text
Add tab icons, OSC 7501 agent status, agent list, and ddg default agent profiles

Plan: docs/archive/plan/agent-status-icons-profiles/README.md
Report: docs/archive/report/2026-11-26/agent-status-icons-profiles/FINAL.md

Decisions:
- A1-A6: Dedicated `ddg run`/`ddg agent`/`--agent` with 100% `DefaultShell` compatibility,
  shared `~/.config/ddg/agents.json` (+ `host_overrides`), OSC 7501 + OS foreground hint +
  local SVG/programs overrides (remote built-in keys only), single P0+P1 milestone.
- UX-1..3 / D-1..4 / RC-01..06: Collapsible 2-section sidebar, 22px split headers, Peek
  Readonly-by-default (`want_control=false`) with temporary claim/release, 4-target launcher,
  `AgentState::{Idle,Working,Blocked,Done,Error}` in RAM-only `HostState.activities`,
  `roxmltree` XML AST SVG whitelist (128 KiB / 512px guard), 8Hz/0Hz spinner gate,
  and pinned legacy remote host safe upgrade preserving `holder`/PTY PIDs.

Excluded alternatives:
- catalog.json v3 migration (kept v2 + `agent_activity_v1` negotiation), string-based SVG
  blacklist, `sha2` crate addition, `turn`/`tab_activity_snapshot`, 60Hz `with_animation()`.

Verification:
- Unit & integration PASS across all 7 affected crates (proto 37, vt 17, transport 34,
  terminal-server 23, host 46, cli 30, native 53 + `--check-vt`) and `ddg-agents-os` (5).
- Real-env scenarios PASS: A1-core, A2 (SIGKILL 1 MiB tail + catalog invariance),
  A1-live-agents (`pi 1.6.1` OSC 7501 live / `codex 0.166.0` foreground fallback),
  A3 (Docker multi-host + zero-downtime legacy upgrade preserving holder/PTY PIDs).
- Unverified / remaining risks: T6-B human GUI verification (H1-H2), `claude` CLI (not
  installed), live paid LLM turns, Linux desktop GUI & WAN SSH (R-01..R-04).
```
