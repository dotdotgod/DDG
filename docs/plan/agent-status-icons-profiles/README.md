# 작업 계획: 탭 아이콘 · OSC 7501 에이전트 상태 · 에이전트 리스트 · `ddg` 기본 에이전트 설정

[UX·UI 설계](DESIGN_UI.md) · [데이터·네트워크·보안 설계](DESIGN_DATA.md) · [TDD 유닛 테스트](TESTS.md) · [사용자 시나리오](SCENARIOS.md) · [태스크 T0–T3](TASKS_CORE.md) · [태스크 T4–T6](TASKS_HOST_UI.md) · [플래닝 완료 보고](../../archive/report/2026-11-26/agent-status-icons-profiles/PLANNING.md)

## 상태와 확정 기록
- **상태**: **`T0`–`T6-A` 구현·자동/실환경 검증 완료 · `T6-B` 사람 GUI 실기 대기 (`Unverified`)** (`2026-11-26`, [배치 보고서](../../archive/report/2026-11-26/agent-status-icons-profiles/BATCH_T0_T6A.md))
- **확정 조합**:
  - **Phase B (요구사항)**: `A1`(전용 `ddg run`/`ddg agent`/`--agent` + `DefaultShell` 100% 호환) · `A2`(`ddg run` 기본 가시 룸+즉시 attach, `--bg`/`--background` 백그라운드) · `A3`(`~/.config/ddg/agents.json`+`host_overrides` CLI·GUI 공유, 일반 `+` 탭=셸 유지+별도 런처) · `A4`(OSC 7501 보고 + `ddg` 에이전트 실행 세션 리스트 포함, `rm_bg_*` 실행 중만 표시·종료 즉시 제거) · `A5`(내장 SVG + OSC 7501 `app` + OS `foreground` 최소 식별자 + 로컬 `~/.config/ddg/icons/`·`programs.json` 1차 포함) · `A6`(P0+P1 단일 마일스톤 일괄 포함)
  - **Phase C (UX)**: `UX-1`(Peek 기본 `want_control=false` + `[제어 요청]` 임시 키 입력 + 닫기 시 자동 반납) · `UX-2`(전체 리스트 수동 순서 + `manual` 자동 전환) · `UX-3`(사이드바 접이식 2단 섹션 상단 `에이전트` 최대 45% + 하단 `룸`, 외부 셸 `ddg run` 현재 `cwd` 새 가시 룸)
  - **Phase D (세부 정책)**: `D-1`(Peek `Control` 모드 `Esc`=터미널 전달, `Cmd/Ctrl+W`·`[닫기]`=Peek 종료) · `D-2`(런처 4지선다 `[현재 룸 새 탭]|[분할]|[새 룸]|[백그라운드]` + 핀/비핀 섹션 내부 수동 이동 서브트리 단위) · `D-3`(`ddg agent show` 기본 `env` 마스킹/`--reveal-env` 평문 + 동적 로더 4종 차단/`LD_LIBRARY_PATH` 허용) · `D-4`(`host_overrides.env: Map<String, Option<String>>` 상속 키 제거 전용 + 구버전 원격 호스트 `ps` 정확 매칭 1회 안전 재시작 후 `features` 검증)
- **계획 루트 근거**: `dotdotgod config /Users/dotdot/Workspace/dotdotgod-agents --json`의 `active-plan` (`role: active-task-intent`, `paths: ["docs/plan/**"]`).
- **역할·모델 분리**: 계획·설계·리뷰·관리·최종 검수는 **Antigravity (Gemini 4 Neon High)**, 실제 코드·테스트 구현은 실행 세션에서 로컬 **Pi (`pi --provider openai --model gpt-6.2-sol --thinking high`)** 단독 담당 ([글로벌 정책](file:///Users/dotdot/.gemini/config/rules/agent-orchestration-policy.md)).

## 배경과 목적
Herdr(`0.1.18` / `herdr-core`)의 3대 표준(내장 프로그램 테이블+SVG+포그라운드 프로세스 기반 탭 아이콘, OSC 7501 Program Status Protocol 기반 에이전트 상태·읽음·집계, 라이브 에이전트 리스트·서브에이전트 트리·정렬·핀·런처)을 DDG Agents의 3-Tier 멀티호스트 스택(`ddg-terminal-server` + `ddg-agents-host` + `ddg-agents-transport` + `ddg` CLI + `native/` GPUI)에 정합하게 설계 비교·도입하고, **`ddg`로 모든 에이전트를 실행할 때 사용자 정의 기본 에이전트 및 실행기 프로파일을 적용**할 수 있도록 확장한다.

### Herdr vs DDG Agents 설계 비교 요약
| 축 | Herdr (`0.1.18` / `herdr-core`) | DDG Agents 확정 설계 | DDG 고유 확장·보강 포인트 |
|---|---|---|---|
| **탭 아이콘** | 내장 `programs.json`(124개) + Lucide/LobeHub/Simple Icons SVG + 7501 `app` / OS `foreground` 추론 | 내장 SVG(`AssetSource`) + 로컬 `~/.config/ddg/programs.json`·`icons/` + 7501 `app` / OS `foreground` 최소 식별자 | 전체 `argv` 비노출(시크릿 차단), 로컬 SVG `roxmltree` XML AST 화이트리스트+단일 fd `O_NOFOLLOW`+512px OOM 가드, 원격 호스트는 내장 키 전용 |
| **에이전트 상태** | OSC 7501 (`idle`/`working`/`blocked`/`done`/`error`) + `herdr.seen` + 집계 + GPU 스피너 | 공식 OSC 7501 (`ESC]7501;key=value:...` + `7501;?` FIFO 응답) + 호스트 RAM `TabActivity`/`seen_seq` + 4단계 집계(`Tab→Split→Room→Machine`) | `pi` DA1 센티넬 FIFO 순서 보장, 4096B drain-and-discard, `catalog.json` `fsync` 비트리거(RAM 전용), 125ms(8Hz)/0Hz 배터리 절약 스피너 |
| **에이전트 리스트** | 7501 보고 세션(`agents()`) + `parent` 트리 + `priority`/`grouped` + 핀 + `setAgentOrder` | 7501 보고 + `ddg` 에이전트 실행 세션 + `parent_session_id`/`parent_host_id` 트리 + `priority`/`grouped`/`manual` + 핀 + `rm_bg_*` Peek/승격 | 실행 중 `rm_bg_*`만 노출·종료 즉시 제거, Peek `want_control=false` + 임시 `[제어 요청]` + 닫기 시 자동 반납, `plugin_host`/`hub` 숨김 격리 유지 |
| **`ddg` 기본 에이전트 실행** | `hc templates` / `knownAgents()` (기본 에이전트 없음) | `ddg run [agent]` + `ddg agent ...` + 기존 명령 `--agent` + `~/.config/ddg/agents.json`(+`host_overrides`) CLI·GUI 공유 + GUI 설정 모달 | 제품 하드코딩 모델 없음(`requirements.md` §1 준수), 사용자 프로파일만 적용, 기존 `DefaultShell` 100% 호환, `flock`+CAS+`0o600`+시크릿/동적로더 차단 |

## 6하원칙으로 정리한 요구사항
- **왜(Why)**: 멀티호스트·멀티룸·분할 패인에서 여러 AI 에이전트를 병렬 운영할 때 각 탭의 도구·실행 상태(`working`/`blocked`/`done`/`error`)를 즉시 파악하고, `ddg`로 에이전트를 실행할 때 기본 에이전트·인자·환경 설정을 반복 입력 없이 일관 적용하기 위함.
- **누가(Who)**: DDG Agents GPUI 데스크톱 앱과 `ddg` CLI로 여러 에이전트를 운영하는 개발자(오퍼레이터), 및 세션 내부에서 서브에이전트를 실행하는 오케스트레이터 에이전트.
- **무엇을(What)**: (1) 탭·패인·리스트 프로그램 아이콘(내장+로컬 오버라이드+OSC 7501 `app`+OS `foreground`), (2) OSC 7501 상태 수집·집계·호스트 RAM `seen`·스피너/배지, (3) 에이전트 리스트(서브에이전트 트리·`priority`/`grouped`/`manual` 정렬·핀·수동순서·`rm_bg_*` Peek/승격·런처 4지선다), (4) `ddg run`/`ddg agent`/`--agent` 및 `agents.json` CLI·GUI 공유 + GUI 설정 모달.
- **언제(When)**: `host-sessions`(D1–D7) 및 `session-id-messaging`(S0–S5-A) 완료 직후 단일 마일스톤(P0+P1)으로 순차·병렬 구현.
- **어디서(Where)**: 7개 워크스페이스 크레이트(`proto`, `vt`, `terminal-server`, `host`, `transport`, `cli`, `native`) + `schema/*.openrpc.json` + `docs/spec/requirements.md`.
- **어떻게(How)**: `catalog.json` v2 하위 호환 유지, `HostHelloResult.features = ["agent_activity_v1"]`·`HolderWelcome.features = ["fg_pgrp_v1"]` 기능 협상, RAM 전용 활동 모델, `ddg-agents-transport` 공용 설정/검증 모듈로 CLI·GUI 단일 진실 원천 유지.

## 우선순위와 포함·제외 범위
- **포함 (단일 마일스톤 — P0 + P1 일괄, A6 확정)**:
  1. **(P0-1)** `ddg` 에이전트 실행(`ddg run`, `ddg agent`, `--agent`) + `~/.config/ddg/agents.json`(+`host_overrides`) 공유 설정
  2. **(P0-2)** OSC 7501 파싱·FIFO `?` 응답·호스트 RAM `TabActivity`/`seen_seq`·계층 집계·스피너/배지 + OS `foreground` 최소 식별자
  3. **(P0-3)** 내장 SVG 카탈로그(`AssetSource`) + 로컬 `~/.config/ddg/programs.json`·`icons/`(`roxmltree` AST 화이트리스트)
  4. **(P0-4)** 에이전트 리스트(서브에이전트 트리·`rm_bg_*` Peek/승격·런처 4지선다)
  5. **(P1-1)** 정렬 3종(`priority`/`grouped`/`manual`)·핀·수동 순서(서브트리 단위)·키보드 포커스 존(`Cmd/Ctrl+Shift+L/A/P/T/N`)
  6. **(P1-2)** GUI `AgentSettingsModal`(`Cmd+,` / `Ctrl+Shift+,`, 충돌 배너, `.corrupt.<ts>` 복구 카드, `env`/`host_overrides` 3상태) + 구버전 원격 호스트 안전 업그레이드
- **제외 / 보류 (P2)**:
  - OSC 7501 미지원 구형 에이전트 CLI에 대한 래퍼/훅 자동 주입(1차는 OSC 7501 표준 + OS `foreground` + 실행기 프로필만 제공).
  - 프로젝트 로컬(`.ddg/config.json`) 자동 프로필 전환(임의 `argv` RCE 경계 검토 후 별도 마일스톤).
  - 봇 마크다운·스킬·팀 워크플로우(`docs/spec/requirements.md` §3–5)와의 결합.
  - 기존 `modal_text_input`의 한글 IME 조합·중간 커서 편집 개선(기존 `MachineModal` 패턴 유지; 알려진 한계로 기록).
  - 신규 유지용 자동화 GUI/크로스호스트 E2E 스위트 작성(기존 통합 테스트 회귀 + 사용자 시나리오 검증으로 대체).

## 요구사항 충돌과 결정 기록
| 충돌/결정 ID | 내용 | 확정 결정 및 근거 |
|---|---|---|
| **C1 (`requirements.md:7`)** | "별도 사용자용 CLI 미제공" vs `ddg` CLI 확장 | `T0`에서 `docs/spec/requirements.md` §1을 "GUI=주 관리·검토 환경, `ddg` CLI=운영자·에이전트 실행 및 자동화 인터페이스"로 공식 갱신 |
| **C2 (`requirements.md:10`)** | "특정 모델/`thinking: low` 기본값 금지" vs 기본 에이전트 설정 | 제품 하드코딩 기본값은 두지 않고 **사용자 설정(`agents.json`) 프리셋**으로만 허용(공존) |
| **C3 (`requirements.md:34-35`)** | "봇과 실행기 분리" vs 에이전트 카탈로그 | 본 계획의 에이전트는 **실행기(Executor) 프로파일**로 정의해 추후 봇 정의가 참조하도록 계층 분리 |
| **RC-01 (`rm_bg_*` 활동 가시성)** | `validation-requirements` 2차 표("Standard만") vs A4·UX-1("백그라운드 에이전트 상태/Peek 표시") | **오퍼레이터 채널(`HostState.activities`, `catalog_subscribe`)**에는 `Standard` + 실행 중 `rm_bg_*` 모두 포함(종료 시 즉시 제거); **`plugin_host.rs`·`hub.rs` 경계에서만 `rm_bg_*` 격리** |
| **RC-02 (Enum 명칭)** | 초안 혼용(`Running`/`Waiting`/`Approval`/`Input`) vs OSC 7501 공식 스펙 | 전 크레이트·문서·테스트를 **`AgentState::{ Idle, Working, Blocked, Done, Error }`** + **`BlockedKind::{ Permission, Question, Auth }`**로 통일 |
| **RC-03 (`agents.json` 스키마)** | `command`+`args` vs `argv`, `color`/`program_id` 중복 | **`argv: Vec<String>`** 통일(`ExecutorSpec::Command` 일관), `color`/`program_id` 삭제, `host_overrides.env: BTreeMap<String, Option<String>>` |
| **RC-04 (`ui.json` 접힘)** | `agents_section_collapsed`/`rooms_section_collapsed` vs `collapsed_sections` | **`collapsed_sections: BTreeSet<String>`** 단일 필드(`"section:agents"`, `"section:rooms"`, `"group:<key>"`), `ui_scale.rs:108` + `config_watch.rs:47,86` 필드 보존 수정 |
| **RC-05 (아이콘 상한·백업명)** | `256 KiB` vs `128 KiB`, `.bak.<ts>` vs `.corrupt.<ts>` | **`MAX_ICON_BYTES = 128 KiB`**, 손상 백업 **`agents.json.corrupt.<ts>[.<n>]`** (`create_new`, `0o600`)로 통일 |

## 역할·책임·작업 영역
- **플래닝·설계·리뷰·최종 검수 (Antigravity / Gemini 4 Neon High)**: Phase A~E 전 14개 독립 역할 서브에이전트(`user-requirements`, `data-requirements`, `validation-requirements`×2, `ux-designer`, `ux-reviewer`, `security-reviewer`×3, `presentation-layer-designer`, `presentation-layer-reviewer`, `data-network-layer-designer`, `data-network-layer-reviewer`, `test-designer`, `test-reviewer`, `review-conductor`) 교차 검증 완료.
- **실제 구현 (로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`)**: 새 실행 세션에서 `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`를 준수해 태스크별 기능 + TDD 유닛 테스트 구현. 완료마다 Antigravity가 `git diff`·테스트·보안 경계 독립 검수.

## 필수 Validation과 저장·조회·상태 정책
상세 13개 신뢰 경계 표, 저장·동시성·복구 표, 제외 근거 9건은 [DESIGN_DATA.md](DESIGN_DATA.md#1-필수-validation-및-신뢰-경계-확정-표-13개-경계)에 확정했다. 핵심 불변식 요약:
- **외부/비신뢰 경계 검증 후 내부 무조건 신뢰**: PTY 바이트 스트림(`ddg-agents-vt::osc7501`·`relay`), OS 프로세스(`proc_info.rs`), RPC 입력(`host_service.rs`), `agents.json`(`ddg-agents-transport::agent`), 로컬 아이콘(`native::icons` `roxmltree` AST) 1차 경계에서만 검증·살균하고 내부 전달 경로 재검증 금지.
- **RAM 전용 활동 상태**: OSC 7501·`foreground`·`seen_seq`는 `HostState.activities`(RAM)에만 보관하며 `catalog.json` `save_catalog()`를 절대 트리거하지 않는다.
- **하위 호환 게이팅**: `CATALOG_SCHEMA_VERSION = 2` 유지, `TabCreateParams`/`TabSplitParams`의 `#[serde(deny_unknown_fields)]` 대응을 위해 `HostHelloResult.features` `"agent_activity_v1"` 협상 후에만 선택 필드·`tab_mark_seen` 전송.

## 설계 · 테스트 · 시나리오 상세 링크
- **UX 설계 & Presentation Layer 설계 (ASCII 아트 5종 포함)**: [DESIGN_UI.md](DESIGN_UI.md)
- **Data Network Layer 설계 · 13개 신뢰 경계 · 알려진 한계 및 재검토 조건**: [DESIGN_DATA.md](DESIGN_DATA.md)
- **7개 크레이트 22개 TDD 유닛 테스트 및 제외 근거**: [TESTS.md](TESTS.md)
- **실환경 사용자 시나리오 (`A1-core`, `A1-live-agents`, `A2`, `A3`, `H1`, `H2`)**: [SCENARIOS.md](SCENARIOS.md)

## 영역별·보안 리뷰와 전체 리뷰 결과
- **Phase C (UX·보안)**: `ux-reviewer` F-01..F-08(단축키 `Cmd/Ctrl+K` 삭제·Linux `Ctrl+Shift+,`·Peek 전용 헤더·`ddg run --new-room` 불리언·좁은 사이드바), `security-reviewer` F-01..F-08(`want_control=false`·비대화형 `세션 보고:`·`split_safe_tokens` Fail-closed·값 비에코·로컬 아이콘 격리·원격 업데이트 핀 보존·OS `foreground` 인라인 플래그 중단) 전건 반영.
- **Phase D (PL·DNL·보안·검증 2차)**: `presentation-layer-reviewer` F-01..F-11(`UiPrefs::load_or_default` 버그·`observe_window_activation`·`ActiveOverlay`·`custom/<name>#<fnv64>` 해시 키·SVG 512px OOM 가드·`Ctrl+Shift+,` `<` 폴딩), `data-network-layer-reviewer` DNL-01..DNL-15(`pi` DA1 FIFO 응답 버퍼·무패딩 base64·`turn` 및 `tab_activity_snapshot` 삭제·`catalog_subscribe` `after_reply` 채널·`HolderWelcome.features`·`core` `GOODBYE::Detach`→`BYE` 대기·`TERM_PROGRAM=ddg` 및 외부 터미널 변수 scrub), `security-reviewer` D-SEC-01..D-SEC-09(`split_safe_tokens` 제어문자/말미 `\`·`roxmltree` AST·`plugin_host` 5경로 `redact_for_plugin`+`tab_create` idempotency 분리·4096B drain-and-discard·`KERN_PROCARGS2` `argc` 경계) 전건 반영.
- **Phase E (테스트·보안·전체 리뷰)**: `review-conductor` 판정 **`진행 가능`**(`RC-01`..`RC-06` 동기화), `test-reviewer` F-01..F-13, `security-reviewer` E-SEC-01..E-SEC-09(시나리오 `HOME`/`DDG_DIST_DIR`/`SSH_AUTH_SOCK`/루프백 Docker 격리 포함) 전건 반영 완료.

## 페이즈별 실행 계획
| 페이즈 | 태스크 | 유형 | 담당 역할 | 병렬 그룹 | 선행 조건 | 독립·통합 완료 기준 |
|---|---|---|---|---|---|---|
| **P0** | `T0` 사전 환경 확인 · `requirements.md` §1 갱신 | 조사/문서 | Antigravity Lead | 단독 | 없음 | `docs/spec/requirements.md` §1 갱신, 로컬 `pi` ready·`GHOSTTY_VT_LIB_DIR`·`docker` 확인 |
| **P1** | `T1` `ddg-agents-proto` + OpenRPC 스냅샷 | 구현 | `implementer` (Pi) | 단독 | `T0` | `UT-PROTO-01..04` + `DDG_UPDATE_SCHEMA=1` 스냅샷 통과 |
| **P2** | `T2-A` `ddg-agents-vt` ∥ `T2-B` `ddg-agents-transport` | 구현 | `implementer` (Pi) | **병렬 그룹 A** (크레이트·파일 완전 분리) | `T1` | `UT-VT-01..02` & `UT-TRANS-01..03` 통과 |
| **P3** | `T3` `ddg-terminal-server` | 구현 | `implementer` (Pi) | 단독 | `T1`, `T2-A` | `UT-TERM-01..04` + 기존 통합 테스트 통과 |
| **P4** | `T4-A` `ddg-agents-host` ∥ `T4-B` `ddg-agents-cli` | 구현 | `implementer` (Pi) | **병렬 그룹 B** (크레이트·파일 분리; `T4-B`는 `T1`+`T2-B` 계약 기준 구현, `T4-A` 완료 후 통합 `cli_e2e` 실행) | `T2-B`, `T3` | `UT-HOST-01..03`, `UT-CLI-01..02`, `cli_e2e` 통과 |
| **P5** | `T5` `native/` GPUI 클라이언트 | 구현 | `implementer` (Pi) | 단독 | `T2-B`, `T4-A` | `UT-NATIVE-01..04`, `cargo test --workspace`, `--check-vt` 통과 |
| **P6** | `T6-A` 에이전트 실환경 시나리오(`A1`–`A3`) · `T6-B` 사람 GUI 실기(`H1`–`H2`) · `T6-C` 아카이브/최종 보고/main 머지 인계 | 검증/종료 | Antigravity + 사람 | `T6-A` ∥ `T6-B` 가능 → `T6-C` | `T4-B`, `T5` | `A1-core`·`A2`·`A3` Pass (`A1-live-agents`/`H1`–`H2` 미수행 시 `Unverified` 명시), `FINAL.md` 작성 |

## 일정과 수행자
- **일정**: 미정 (사용자 착수 지시 후 새 실행 세션 진행)
- **수행자**: 구현 = 로컬 `pi --provider openai --model gpt-6.2-sol --thinking high` (`roles/implementer.md`), 검수·관리·최종 보고 = Antigravity (Gemini 4 Neon High), GUI 실기 검증(`T6-B`) = 사용자(사람).

## 위험·가정·미결정 사항
- **미결정 질문**: 없음(전 Phase B~D 사용자 결정 A1–A6, UX-1~3, D-1~4 확정).
- **잔여 미확인 위험**: `R-01`(macOS vs Linux OS `foreground` 권한/래퍼 차이), `R-02`(`codex` v0.166.0의 실제 OSC 7501 방출 조건 및 `claude` 로컬 미설치 `Unverified`), `R-03`(GPUI 0.2.2 2단 사이드바 45% 스크롤·Peek `Control` `Esc`/`Cmd+W` 실기 검증), `R-04`(원격 비표준 로그인 셸에서의 `ps` 정확 매칭).
- **비가역 실행**: 본 계획의 구현·테스트 범위에는 비가역 인프라/배포 작업 없음. 단, `T6-C`의 `git push` 또는 `main` 머지는 사용자 명시 승인 전까지 대기한다.

## 태스크 명세 목록
- [TASKS_CORE.md](TASKS_CORE.md): `T0` (사전 조사·문서 동기화), `T1` (`ddg-agents-proto`), `T2-A` (`ddg-agents-vt`), `T2-B` (`ddg-agents-transport`), `T3` (`ddg-terminal-server`)
- [TASKS_HOST_UI.md](TASKS_HOST_UI.md): `T4-A` (`ddg-agents-host`), `T4-B` (`ddg-agents-cli`), `T5` (`native/` GPUI), `T6-A` (에이전트 시나리오), `T6-B` (사람 GUI 실기), `T6-C` (종료·아카이브·머지 인계)

## 최종 완료 보고서 경로
- 플래닝 완료 보고서: [docs/archive/report/2026-11-26/agent-status-icons-profiles/PLANNING.md](../../archive/report/2026-11-26/agent-status-icons-profiles/PLANNING.md)
- 보고서 목차: [docs/archive/report/2026-11-26/agent-status-icons-profiles/README.md](../../archive/report/2026-11-26/agent-status-icons-profiles/README.md)
