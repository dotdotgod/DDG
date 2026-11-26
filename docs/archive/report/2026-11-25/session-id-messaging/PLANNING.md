# 작업 결과 보고서: 전역 고유 Room/Session ID · 백그라운드 룸 · 양방향 세션 통신

[보고 목차](README.md) · [활성 계획](../../../../plan/session-id-messaging/README.md)

## 보고 대상
**계획 작성** (`2026-11-25`) — 구현·검증·머지는 본 플래닝 세션에서 수행하지 않았으며 실행 세션으로 인계한다.

## 대상 태스크·배치 ID와 포함 범위
- 대상: `/ddg-planning` Phase A–G 전체(요구사항·UX·UI·데이터/네트워크·보안·테스트·시나리오·태스크 S0–S5 분해 및 보고).
- 모델 분리: 계획·설계·리뷰·관리 = **Antigravity (Gemini 4 Neon)**, 실행 세션 구현 = **Pi (`pi --provider openai --model gpt-6.2-sol --thinking high`)**.

## 해당 범위 상태
**완료** (계획 작성 범위 100% 완료; 미결정 질문 0건).

## 전체 계획 상태와 남은 범위
- **완료**: Phase A–G 플래닝, 14개 독립 Neon 서브에이전트 교차 설계·리뷰, 모듈형 계획·태스크·시나리오 확정.
- **남은 범위**: 실행 세션 착수(S0 환경/동기화 확정 → S1 → S2-A∥S2-B → S3-A → S3-B∥S4 → S5-A/B/C 구현·검증·아카이브).

## 산출물: 계획·설계·태스크 경로
- 활성 계획 목차: [docs/plan/session-id-messaging/README.md](../../../../plan/session-id-messaging/README.md)
- UI/UX 설계(ASCII 3종): [DESIGN_UI.md](../../../../plan/session-id-messaging/DESIGN_UI.md)
- 데이터/네트워크/보안 설계: [DESIGN_DATA.md](../../../../plan/session-id-messaging/DESIGN_DATA.md)
- TDD 유닛·격리 통합 테스트: [TESTS.md](../../../../plan/session-id-messaging/TESTS.md)
- 사용자 시나리오(에이전트+사람 GUI): [SCENARIOS.md](../../../../plan/session-id-messaging/SCENARIOS.md)
- 실행 태스크 명세: [TASKS_CORE.md](../../../../plan/session-id-messaging/TASKS_CORE.md), [TASKS_HOST_UI.md](../../../../plan/session-id-messaging/TASKS_HOST_UI.md)

## 확정 요구사항·설계와 결정 출처
- **사용자 직접 확정**:
  - `1B`(재시작 시 `session_id` 유지 + `run_generation += 1`), `2A`(6개 `DDG_*` env + 소켓 RPC + 브리지 CLI), `3A`(숨김 백그라운드 룸 + `session_move` 승격), 2계층 서버 소유(`ddg-terminal-server` + `ddg-agents-host`).
  - `Q1=A1` & `Q6`(단일/크로스 호스트 메시징·`read_screen`·`write_input` 허용, GUI `ControlMode::Control` 유지), `Q2=A2`(숨김 세션 종료 시 즉시 삭제 + 부모 인박스 알림), `Q3=A3`(v1 `catalog.json.v1.bak.<ts>` `0o600` 백업 후 v2 초기화 + 생존 v1 고아 PTY 정리), `Q4+Q5=A4+Option 1`(Hub 단독 중계, 원격 호스트 타 호스트 자격증명 0개).
  - 역할 분리: 구현은 Pi `openai/gpt-6.2-sol`, 계획·관리·리뷰·설계는 Antigravity Neon.

## 요구사항·가정 옵션과 설계 옵션
- **선택한 옵션·이유·근거·영향**:
  - **Hub opt-in (`DDG_HUB_ENABLED=1`) + 단일 양방향 `host.relay_attach`**: 원격 호스트의 키/설정 노출을 0으로 유지하고 양방향 통신(`hop == 1`)을 단일 채널로 단순화.
  - **단일 리셋 알림 경로 (`CatalogSubscribeResult.reset_notice`)**: `HostHelloResult` 중복 경로 제거로 GUI 상태 불일치 방지.
  - **단일 재어태치 경로 (`sync_machine_rooms`)**: `HostBridge::restart_tab` 중복 `tab_attach` 제거로 이중 스트림/깜박임 방지.
  - **RAM 전용 토큰 + CSPRNG fail-closed + `ct_eq`/zeroize**: 카탈로그·로그 유출 및 타이밍 공격 차단.
- **다른 방법과 제외 이유**:
  - `1A`(재시작 새 ID): 진행 중 에이전트 통신 단절로 제외.
  - 풀 메시/원격 키 배포(`Option 2`): 원격 침해 시 측면 이동 위험으로 제외.
  - v1/v2 듀얼 스키마 호환: RPC·클라이언트 복잡도 급증으로 제외.
  - 독립 `host.relay_exec` RPC / `session.purge_mailbox` RPC / `pending_recovery_exits`: 공격면·중복 상태 축소를 위해 리뷰 반영 후 제외.
- **필수 범위 밖 작업 검토**: 숨김 룸 GUI 관리 화면, 메시지 영속 DB, 자동 GPUI 클릭 E2E는 필수 범위 밖으로 제외(범위 내 대안: 브리지 CLI `session list/move` + 사람 GUI 시나리오 2).

## 태스크 순서·의존관계·병렬 그룹
- 순차/병렬 DAG: `S0(조사·동기화 확인)` → `S1(proto + OpenRPC)` → **`[S2-A(terminal-server) ∥ S2-B(transport)]`** → `S3-A(host core/token/plugin)` → **`[S3-B(host cli/hub) ∥ S4(native GPUI)]`** → `S5-A(워크스페이스+Docker 시나리오 1)` → `S5-B(사람 GUI 시나리오 2)` → `S5-C(최종 보고·아카이브)`.
- 병렬 근거: `S2-A`∥`S2-B`와 `S3-B`∥`S4`는 크레이트·파일·공유 상태가 완전히 분리되어 충돌 0.

## 비가역 실행의 결정 기록
- **실제 실행 여부**: **미실행 (0건)** — 본 플래닝 세션에서는 문서만 작성함.
- **향후 실행 세션 주의 비가역 경계**:
  - **대상**: 사용자 실운영 `~/.local/state/ddg/catalog.json` v1 리셋, `~/.config/ddg/hosts.json`·`~/.ssh` 수정, `smaxh-mac` 원격 파일 동기화.
  - **근거·복구·승인 조건**: 모든 테스트·시나리오는 스크래치 `DDG_*_DIR` 격리를 강제하여 실운영 데이터를 건드리지 않음. 실운영 환경 적용 또는 `smaxh-mac` 동기화/로컬 `pi` 예외 실행은 실행 세션 S0에서 사용자 명시 승인 후 수행(본 보고서는 승인을 대체하지 않음).

## 역할별 결과·리뷰와 검증 증거
- **수행된 14개 독립 Antigravity (Gemini 4 Neon) 서브에이전트**:
  - Phase B: `user-requirements`, `data-requirements`, `validation-requirements` (완료, Q1–Q6 사용자 확정 완료).
  - Phase C: `ux-designer`, `ux-reviewer`(F-1..F-7), `security-reviewer`(S-1..S-8) — 전건 설계 반영.
  - Phase D: `presentation-layer-designer`, `data-network-layer-designer`, `presentation-layer-reviewer`(P-1..P-7), `data-network-layer-reviewer`(DN-1..DN-12), `security-reviewer`(D-1..D-8) — 전건 설계 반영.
  - Phase E: `test-designer`, `review-conductor`(판정: **`진행 가능`** + `plugin_host.rs` `pty.list`/`pty.kill` 숨김 격리 보완), `test-reviewer`(F-01..F-11 + `native/src/main.rs:2423` 순수 테스트 패턴 사실 보완) — 전건 테스트/태스크 반영.
- **설계 확정 후 변경·갱신**: Phase E 리뷰에서 도출된 `plugin_host` `Hidden`/`Background` PTY 필터링 및 음성 계약 테스트·주입 시계·`all` 호스트 ID 차단·스크래치 격리 규칙을 [DESIGN_DATA.md](../../../../plan/session-id-messaging/DESIGN_DATA.md)·[TESTS.md](../../../../plan/session-id-messaging/TESTS.md)·[SCENARIOS.md](../../../../plan/session-id-messaging/SCENARIOS.md)·태스크에 일관 반영 완료.

## 차단·미결정·미검증 사항과 남은 위험
- **실행 세션 착수 전 차단 1건 (환경 확인 사실)**:
  - `hc machine ls`에서 `smaxh-mac`은 `connected`, `pi auth check --model openai/gpt-6.2-sol`은 로컬·`smaxh-mac` 모두 `ready`이나, **`smaxh-mac`에 `/Users/dotdot/Workspace/dotdotgod-agents` 디렉터리가 존재하지 않음**(`No such file or directory`).
- **미검증 사항**: 본 세션은 플래닝 전용이므로 코드 빌드·유닛 테스트·Docker E2E·사람 GUI 실기 전체가 실행 세션의 미검증 범위임.
- **남은 위험**: (1) `ddg-agents-host` 재기동 시 RAM 토큰 무효화(문서화된 한계), (2) `DDG_TERMINAL_SOCK` 동일 OS UID 신뢰 경계, (3) `libghostty-vt` 빌드 시 `GHOSTTY_VT_LIB_DIR` 경로 의존.

## 다음 실행 세션의 착수 조건
1. **S0 결정**: (A) 현재 워크스페이스를 `smaxh-mac:/Users/dotdot/Workspace/dotdotgod-agents`로 동기화 후 정책대로 `hc run --bg --on smaxh-mac`로 `pi` 실행(권장), 또는 (B) 사용자 명시 승인으로 로컬 `pi --provider openai --model gpt-6.2-sol --thinking high` 실행.
2. 각 구현 태스크(S1–S4)는 `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`를 `pi`에 전달하고, 매 태스크 완료 시 Antigravity Neon이 `diff` 및 테스트 출력을 독립 검수한다.

## 아카이브·main 머지 기록
- **실제 보고서**: `docs/archive/report/2026-11-25/session-id-messaging/PLANNING.md` (본 문서)
- **보고 목차**: [docs/archive/report/2026-11-25/session-id-messaging/README.md](README.md)
- **완료 계획**: 구현 전이므로 활성 경로 유지 (`docs/plan/session-id-messaging/`, S5-C 완료 후 `docs/archive/plan/session-id-messaging/`로 이관 예정)
- **상호 링크·README 인덱스 갱신**: `docs/plan/README.md`, `docs/archive/README.md`, `docs/plan/session-id-messaging/README.md`, `docs/archive/report/2026-11-25/session-id-messaging/README.md` 갱신 완료.
- **main 머지 상태·커밋**: **미수행 (현재 워크스페이스는 Git 저장소가 아님)**. 향후 Git 저장소 구성 및 머지 시 커밋 본문에 보고서/계획 경로, `1B·2A·3A·Q1–Q6` 선택 근거·제외 대안·검증 결과·남은 위험을 자족적으로 포함할 것.

## 수행·미수행 작업과 다음 착수점
- **수행 완료**: Phase A–G 플래닝, 14개 독립 역할 설계/리뷰, 계획·설계·테스트·시나리오·태스크·플래닝 보고서 작성 및 인덱스 갱신.
- **미수행**: 코드/테스트 구현(S1–S4), 통합·Docker·GUI 검증(S5-A/B), 완료 계획 아카이브 이관(S5-C), Git 머지.
- **다음 착수점**: 새 실행 세션에서 **태스크 S0** (`smaxh-mac` 동기화 vs 로컬 `pi` 승인) 확정 후 **S1 (`ddg-agents-proto`)** 착수.
- **이력 갱신 (`2026-11-25 15:52`)**: 사용자 명시 승인으로 `smaxh-mac` 관련 흔적(`t28w`·스냅샷) 정리 완료 및 본 프로젝트(S1–S5) **로컬 인터랙티브 `pi` (`openai/gpt-6.2-sol`, `--thinking high`)** 실행 전환 확정(S0 완료).
