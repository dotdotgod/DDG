# 작업 계획: 전역 고유 Room/Session ID · 백그라운드 룸 · 양방향 세션 통신

[UI·UX 설계](DESIGN_UI.md) · [데이터·네트워크 설계](DESIGN_DATA.md) · [테스트 설계](TESTS.md) · [사용자 시나리오](SCENARIOS.md) · [태스크 S0–S2](TASKS_CORE.md) · [태스크 S3–S5](TASKS_HOST_UI.md) · [플래닝 완료 보고](../../archive/report/2026-11-25/session-id-messaging/PLANNING.md)

## 상태와 확정 기록
- **상태**: 배치 `S0–S5-A` 구현·테스트·Docker E2E 완료 (`2026-11-25`) · `S5-B` 사람 GUI 실기 검증 대기 (`Unverified`) — [배치 보고](../../archive/report/2026-11-25/session-id-messaging/BATCH_S0_S5A.md)
- **확정 조합**: `1B` · `2A` · `3A` · `Q1=A1(허용)` · `Q2=A2(즉시 삭제)` · `Q3=A3(백업 후 v2 초기화)` · `Q4+Q5=A4+Option 1(Hub 단독 중계)` · `Q6=허용(제어권 유지)`
- **역할·모델 분리**: 계획·설계·리뷰·관리·최종 검수는 **Antigravity (Gemini 4 Neon)**, 실제 코드/테스트 구현은 실행 세션에서 **Pi (`pi --provider openai --model gpt-6.2-sol --thinking high`)** 단독 담당 ([글로벌 정책](file:///Users/dotdot/.gemini/config/rules/agent-orchestration-policy.md)).

## 배경과 목적
기존 `u64` 호스트 로컬 `RoomId`/`TabId`는 멀티 호스트 환경에서 충돌하며, 룸 없는 임시/에이전트 세션이나 에이전트↔에이전트·에이전트↔터미널 간 양방향 통신 수단이 없다. 서버(`ddg-terminal-server` + `ddg-agents-host`)가 전역 고유 ID로 룸·세션을 소유하고, 숨김 백그라운드 룸과 단일/멀티 호스트 양방향 메시징·화면 조회·입력 주입을 제공하도록 확장한다.

## 6하원칙으로 정리한 요구사항
- **Who**: GUI 사용자, 호스트 내/간 실행되는 AI 에이전트, 터미널 세션, 플러그인 클라이언트.
- **What**: 모든 룸(`rm_<26>`, 숨김 배경 `rm_bg_<host_prefix>`)과 세션(`ses_<26>`)에 필수 전역 고유 ID 부여; 룸 없는 임시 세션을 숨김 백그라운드 룸에 자동 할당; 세션 간 구조화 메시징(`msg send/recv/subscribe`), 화면 읽기(`read_screen`), 원시 입력 주입(`write_input`) 및 Hub 기반 원격 호스트 중계.
- **When**: 세션 생성·재시작(`1B`: ID 유지, `run_generation += 1`)·승격(`3A`: 숨김→가시 1회)·종료(`Q2`: 숨김 즉시 삭제)·업그레이드(`Q3`: v1 백업 후 v2 초기화) 전 수명주기.
- **Where**: `ddg-agents-proto`, `ddg-terminal-server`, `ddg-agents-transport`, `ddg-agents-host`, `native/` GPUI.
- **Why**: 멀티 호스트·멀티 에이전트 오케스트레이션의 고유 식별, 제어권 침해 없는 터미널 협업, 원격 호스트의 무자격증명(Zero Foreign Credentials) 보안 모델 확립.
- **How**: 2계층 소유(`ddg-terminal-server`=저수준 `session_id`+`mailbox`, `ddg-agents-host`=`room_id`+Catalog v2+`token_store`+`hub`+`cli`), 스폰 시 6개 `DDG_*` 환경변수 자동 주입(`2A`).

## 우선순위와 포함·제외 범위
- **P0 (필수)**: 전역 ID 코덱/CSPRNG, Catalog v2 및 v1 `.v1.bak` 리셋, `1B` 재시작 세대 관리, 숨김 백그라운드 룸(`3A`·`Q2`), RAM 토큰 인증, 단일 호스트 메시징/화면/입력, 브리지 CLI(`2A`), `plugin_host` 격리, `native/` v2 호환 및 최소 표시 UI.
- **P1 (포함)**: Opt-in Hub(`DDG_HUB_ENABLED=1`) + `host.relay_attach` 단일 양방향 채널 기반 크로스 호스트 중계(`Q4+Q5`), `--host all` 읽기 전용 조회, Docker 크로스 호스트 E2E.
- **제외 범위**: 숨김 룸 GUI 사이드바 노출/관리 UI, 메시지 디스크 영속화/재전송 큐, 가시→숨김 역강등, `u64`↔`String` 혼합 호환 API, 원격 호스트 간 직접 P2P 메시징, 기존 `~/.local/state/ddg` 자동 실운영 마이그레이션(테스트는 스크래치 디렉터리 격리).

## 요구사항 충돌과 결정 기록
| 항목 | 확정 결정 | 대안 및 제외 이유 |
|---|---|---|
| 재시작 ID (`1B`) | `session_id` 유지 + `run_generation += 1` | 새 ID 발급(`1A`)은 진행 중인 에이전트 통신 대상을 끊음 |
| 에이전트 접근 (`2A`) | 6개 `DDG_*` env + 소켓 RPC + 브리지 CLI | 전용 SDK만 제공 시 셸/외부 에이전트 호환성 저하 |
| 임시 세션 (`3A`/`Q2`) | 숨김 룸 할당, `session_move` 승격, 종료 시 즉시 삭제 | 종료 후 보존 시 백그라운드 64개 한도 고갈 및 유령 세션 누적 |
| 원격 입력 (`Q1`/`Q6`) | 호스트 인증+토큰 검증 후 허용, GUI 제어권(`Control`) 유지 | 읽기 전용 제한 시 에이전트↔터미널 협업 요구 미충족 |
| 크로스 호스트 (`Q4`/`Q5`) | Hub 단독 중계(Option 1), 원격 호스트에 타 호스트 설정 0개 | 풀 메시/원격 키 배포는 자격증명 확산 위험 |
| v1 호환 (`Q3`) | `catalog.json.v1.bak.<ts>`(`0o600`) 백업 후 v2 초기화 | 듀얼 스키마 유지는 RPC·클라이언트 복잡도 급증 |

## 역할·책임·작업 영역
- **설계·리뷰·관리 (Antigravity / Gemini 4 Neon)**: 14개 독립 서브에이전트(요구·데이터·검증·UX·UX리뷰·보안리뷰×2·UI설계/리뷰·데이터네트워크설계/리뷰·테스트설계/리뷰·전체리뷰)로 전 페이즈 교차 검증 완료.
- **구현 (Pi / `openai/gpt-6.2-sol`, `--thinking high`)**: 실행 세션에서 `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` 준수하여 각 태스크의 코드+유닛 테스트 구현; 완료마다 Antigravity가 `diff`/테스트 독립 검수.

## 필수 Validation과 저장·조회·상태 정책
- **경계별 검증·신뢰 정책**: [DESIGN_DATA.md](DESIGN_DATA.md)의 신뢰 경계표 기준 — (1) 외부 RPC/CLI/파일 입력 경계에서 형식·길이·세대·토큰·권한 검증, (2) `catalog.json` v1→백업·리셋 및 스키마 검증, (3) `HostService` 내부는 검증 완료된 `SessionId`/`RoomId`/`AuthenticatedCaller`를 신뢰하며 중복 검증 생략, (4) `DDG_TERMINAL_SOCK`은 동일 OS UID 신뢰 경계로 문서화하고 `host`는 모든 세션 RPC에 `session_token` 필수 검증.

## 설계 · 테스트 · 리뷰 상세 링크
- **UX & Presentation Layer (ASCII 아트 3종 포함)**: [DESIGN_UI.md](DESIGN_UI.md)
- **Data/Network Layer · 보안 · 한계 및 재검토 조건**: [DESIGN_DATA.md](DESIGN_DATA.md)
- **TDD 유닛 테스트(5개 크레이트) & 격리 통합 테스트**: [TESTS.md](TESTS.md)
- **실환경 사용자 시나리오 (에이전트 + 사람 GUI)**: [SCENARIOS.md](SCENARIOS.md)

## 영역별·보안 리뷰와 전체 리뷰 결과
- **Phase C (UX/보안)**: F-1..F-7(승격 UX, 외부 입력 배지 범위, 리셋 배너 단일화), S-1..S-8(토큰 CLI 금지, Bidi/제어문자 정제, `0o600` 감사 로그, `DDG_HUB_ENABLED` 기본 OFF) 전건 설계 반영.
- **Phase D (UI/데이터·네트워크/보안)**: P-1..P-7(GPUI `0.2.2` `ml_auto().flex_shrink_0()`, `TabAttachFailed`, `run_generation` VT 리셋), DN-1..DN-12(`pending_recovery_exits`·`purge_mailbox` 제거, `InFlight` 즉시 `Conflict`, `boot_generation`), D-1..D-8(`RequirePinnedHostId`, `relay_exec` 제거, `ct_eq`+zeroize, `LC_ALL=C`) 전건 반영.
- **Phase E (전체 리뷰·테스트 리뷰)**: 전체 리뷰 지휘자 판정 **`진행 가능`**; `plugin_host.rs` `pty.list`/`pty.kill` 숨김 세션 격리 및 `test-reviewer` F-01..F-11(음성 계약 테스트, `all` 호스트 ID 차단, 주입 시계, 순수 함수 `native` TDD, 스크래치 격리/`Unverified` 규칙) 전건 반영 완료.

## 페이즈별 실행 계획
1. **S0 (조사/환경 준비 — 완료)**: `smaxh-mac` 흔적 정리 완료 + 사용자 승인으로 **로컬 `pi` (`openai/gpt-6.2-sol`, `--thinking high`)** 실행 확정 (`GHOSTTY_VT_LIB_DIR`·`cargo 1.100.0`·`docker 29.9.3` 확인) → [TASKS_CORE.md](TASKS_CORE.md#태스크-s0-실행-환경-확정-및-smaxh-mac-정리)
2. **S1 (구현)**: `crates/ddg-agents-proto` + OpenRPC 스키마 재생성 → [TASKS_CORE.md](TASKS_CORE.md#태스크-s1-ddg-agents-proto-v2-계약-및-openrpc)
3. **S2 (병렬 구현 그룹)**: `S2-A`(`ddg-terminal-server`) ∥ `S2-B`(`ddg-agents-transport`) — 파일·크레이트 완전 분리 → [TASKS_CORE.md](TASKS_CORE.md#태스크-s2-a-ddg-terminal-server-세션-id-및-mailbox)
4. **S3 (순차 구현)**: `S3-A`(`ddg-agents-host` 핵심/토큰/카탈로그/플러그인) → `S3-B`(`cli.rs` + `hub.rs`) → [TASKS_HOST_UI.md](TASKS_HOST_UI.md#태스크-s3-a-ddg-agents-host-카탈로그-v2토큰세션플러그인)
5. **S4 (구현)**: `native/` GPUI v2 마이그레이션 및 최소 UX → [TASKS_HOST_UI.md](TASKS_HOST_UI.md#태스크-s4-native-gpui-v2-전환-및-최소-ux)
6. **S5 (검증·종료)**: 전체 워크스페이스·Docker 에이전트 시나리오(S5-A), 사람 GUI 시나리오(S5-B), 완료 보고 및 `docs/archive/plan/` 이관(S5-C) → [TASKS_HOST_UI.md](TASKS_HOST_UI.md#태스크-s5-a-워크스페이스-통합-및-에이전트-시나리오-1-검증)

## 일정과 수행자 · 위험 및 미결정 사항
- **일정/수행자**: 착수 시점 미정(사용자 트리거 후 실행 세션 진행). 구현=`Pi (openai/gpt-6.2-sol)`, 리뷰·검수=`Antigravity (Gemini 4 Neon)`.
- **남은 미결정 사항**: 없음(전 요구·설계·S0 실행 머신 확정). 사용자 명시 승인으로 본 프로젝트 구현은 **로컬 `pi` (`openai/gpt-6.2-sol`, `--thinking high`)** + **Antigravity (Gemini 4 Neon)** 독립 검수 체제로 진행.
- **최종 완료 보고서 경로**: [PLANNING.md](../../archive/report/2026-11-25/session-id-messaging/PLANNING.md)
