# 실행 태스크 명세: T0–T3 (사전 준비 · `proto` · `vt` · `transport` · `terminal-server`)

관련 문서: [계획 개요](README.md) · [UX·UI 설계](DESIGN_UI.md) · [데이터·네트워크·보안 설계](DESIGN_DATA.md) · [TDD 유닛 테스트](TESTS.md) · [사용자 시나리오](SCENARIOS.md) · [태스크 T4–T6](TASKS_HOST_UI.md)

> **실행 세션 공통 규율**:
> - 모든 구현 태스크(`T1`–`T5`)는 새 실행 세션에서 **로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`**가 `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`를 직접 읽고 수행한다([글로벌 오케스트레이션 정책](file:///Users/dotdot/.gemini/config/rules/agent-orchestration-policy.md)).
> - **Antigravity (Gemini 4 Neon High)**는 각 태스크 완료마다 `git status --short`, `git diff`, 실제 테스트 출력을 독립 검수한다.
> - 빌드 환경(로컬 Mac): `CARGO_HOME=/tmp/ddg-cargo`, `RUSTUP_HOME=/tmp/ddg-rustup`, `PATH=/tmp/ddg-cargo/bin:$PATH`, `GHOSTTY_VT_LIB_DIR=/tmp/ddg-vt.t10r26/ghostty-vt.xcframework/macos-arm64_x86_64` (환경 경로가 달라졌으면 `T0`에서 확인·기록).

---

# 태스크: T0 — 사전 환경 확인 및 `docs/spec/requirements.md` §1 동기화

## 목표
구현 착수 전 로컬 실행 환경(`pi` auth, `cargo`, `GHOSTTY_VT_LIB_DIR`, `docker`)을 확인하고, Phase B 확정 결정(C1/C2)에 따라 `docs/spec/requirements.md` §1의 CLI 부재 문구를 공식 갱신한다.

## 태스크 유형: 조사 / 문서
## 수행 역할과 역할 정의의 정확한 경로
- Antigravity Lead (`/Users/dotdot/.agents/skills/ddg-planning/roles/design-conductor.md`, 문서 작성 권한 보유)

## 실행 상태: 착수 가능
## 선행 태스크와 착수 조건
- 없음 (Phase B~E 플래닝 확정 완료).

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음.
- 허용된 승인 준비·조건 확인 작업: `pi auth check --model openai/gpt-6.2-sol`, `cargo --version`, `docker info`, `GHOSTTY_VT_LIB_DIR` 존재 확인.
- 대기 중 계속할 독립 작업: `docs/spec/requirements.md` §1(L7, L10) 문구 갱신.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- C1(`requirements.md:7`): "GUI가 주 관리·검토 환경이며, `ddg` CLI는 세션·에이전트 실행 및 자동화/운영자 인터페이스 역할을 담당한다"로 개정(A1 확정).
- C2(`requirements.md:10`): "제품 하드코딩 기본값은 두지 않으며, 사용자 설정(`agents.json`) 프리셋만 허용"으로 명확화.

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [README.md](README.md#요구사항-충돌과-결정-기록), `docs/spec/requirements.md`, `/Users/dotdot/.gemini/config/rules/agent-orchestration-policy.md`.

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `docs/spec/requirements.md`
- 쓰기 금지: `crates/**`, `native/**`, `schema/**`

## 수행 내용
- `pi auth check --model openai/gpt-6.2-sol` ready 확인(fail-closed: 미준비 시 중단·보고).
- `docs/spec/requirements.md` §1 갱신 및 기준선 `cargo test --workspace` 확인.

## 필수 Validation과 실패 처리
- `pi` 인증 실패 시 타 모델/직접 구현으로 폴백하지 않고 차단 보고.

## 테스트 (필수)
- 확인할 요구사항·완료 기준: 기준선 워크스페이스 빌드/테스트 정상 여부 및 `requirements.md` §1 정합성.
- 같은 태스크에서 작성·수정할 유닛 테스트: 없음(조사·문서 태스크; 제외 근거: 코드 변경 없음).
- 실행할 테스트와 명령: `pi auth check --model openai/gpt-6.2-sol && cargo test --workspace`
- 테스트 데이터·환경과 독립적인 기대값 근거: 기존 32개+ 테스트 전부 PASS, `pi` `ready`.
- 통과 기준과 남는 검증 범위: 기준선 통과 후 `T1` 착수 가능.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-A`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- 프로덕션 코드·스키마 변경 금지.

## 산출물과 독립·통합 완료 기준
- 갱신된 `docs/spec/requirements.md` + 환경 점검 결과 기록.

## 병렬 작업 주의사항과 공유 파일
- 단독 선행 태스크(`T1`의 선행 조건).

## 차단 조건과 처리 방법
- `pi` 미준비 시 즉시 차단 보고; `GHOSTTY_VT_LIB_DIR` 부재 시 `native/README.md` 절차로 복구 확인.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험을 실행 세션 보고에 기록.

---

# 태스크: T1 — `ddg-agents-proto` 활동/아이콘/Holder 계약 확장 및 OpenRPC 1.3 스냅샷 갱신

## 목표
모든 하위 크레이트가 공유하는 프로토콜·도메인 타입(`sanitize_display_text` 보강, `AgentState`/`BlockedKind`/`AgentStatus`/`ForegroundHint`/`TabActivity`, `TabRecord` v2 선택 필드, `HolderWelcome.features`/`QUERY_FG_PGRP`/`FG_PGRP`, `host.tab_mark_seen`, `HostEvent` 델타, OpenRPC 1.3 스냅샷)을 TDD로 구현한다.

## 태스크 유형: 구현
## 수행 역할과 역할 정의의 정확한 경로
- `implementer` — `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` (로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`)

## 실행 상태: 착수 가능 (`T0` 완료 후)
## 선행 태스크와 착수 조건
- `T0` 완료.

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음.
- 허용된 승인 준비·조건 확인 작업: `crates/ddg-agents-proto/src/*.rs` 기존 테스트 확인.
- 대기 중 계속할 독립 작업: 없음.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `RC-02`: `AgentState::{ Idle, Working, Blocked, Done, Error }`, `BlockedKind::{ Permission, Question, Auth }` 단일 규범; `turn` 삭제.
- `DNL-05`: 중복 `host.tab_activity_snapshot` RPC 미추가(`CatalogSubscribeResult.activities` 단일화).

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [DESIGN_DATA.md](DESIGN_DATA.md#1-필수-validation-및-신뢰-경계-확정-표-13개-경계) (경계 #1, #4, #5, #7)
- [DESIGN_DATA.md](DESIGN_DATA.md#23-catalogjson-v2-유지--ram-전용-tabactivity)
- [TESTS.md](TESTS.md#1-cratesddg-agents-proto-4그룹) (`UT-PROTO-01`..`UT-PROTO-04`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `crates/ddg-agents-proto/src/{id.rs, terminal.rs, host.rs, holder.rs, openrpc.rs}`, `schema/ddg-terminal-server.openrpc.json`, `schema/ddg-agents-host.openrpc.json`
- 쓰기 금지: 그 외 크레이트 (`vt`, `terminal-server`, `host`, `transport`, `cli`, `native`)

## 수행 내용
1. `id.rs::sanitize_display_text`에 `U+2028`/`U+2029` 제거 통합.
2. `terminal.rs`: `AgentState`, `BlockedKind`, `AgentStatusRecord`, `AgentStatus` (`turn` 미포함), `ForegroundHint`, `PtyInfo` 선택 필드(`agent_status`, `foreground`), `PtyEvent::{ AgentStatusChanged, ForegroundChanged }`.
3. `host.rs`: `TabRecord` 선택 필드(`agent_profile`, `parent_host_id` — v2 유지, `skip_serializing_if = "Option::is_none"`), `Catalog::validate` 슬러그·`parent_host_id => parent_session_id` 검증, `TabActivity`, `CatalogSubscribeResult.activities`, `RoomCreateParams`/`TabCreateParams`/`TabSplitParams`/`SessionCreateParams` 선택 필드 `agent_profile`(`deny_unknown_fields` 유지 + `skip_serializing_if = "Option::is_none"`), `TabMarkSeenParams`/`TabMarkSeenResult`(`deny_unknown_fields`), `HostEvent::{ TabActivityChanged, TabSeenChanged }`.
4. `holder.rs`: `QUERY_FG_PGRP = 8`, `FG_PGRP = 108`, `HolderFgPgrp`, `HolderFgPgrpBatch`, `HolderWelcome.features` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`).
5. `openrpc.rs`: 신규 메서드/이벤트/타입 등록 후 `DDG_UPDATE_SCHEMA=1 cargo test -p ddg-agents-proto openrpc`로 스냅샷 갱신.

## 구현 착수 근거
- 변경 대상: `ddg-agents-proto` 5개 파일 + `schema/*.openrpc.json` 2개.
- 계약: `CATALOG_SCHEMA_VERSION = 2` 유지, `TabCreateParams`/`TabSplitParams` `deny_unknown_fields` 및 `parent_session_id` 비노출 유지.
- 검증 방법: `UT-PROTO-01..04` TDD.
- 관련 리뷰 결과: `RC-02`, `DNL-04`, `DNL-05`, `DNL-08`, `F-01`, `F-03`, `F-04` 반영.

## 필수 Validation과 실패 처리
- `DESIGN_DATA.md` 경계 #1, #4, #5, #7 준수(`Catalog::validate` 슬러그 `^[a-z0-9._-]{1,64}$`, `parent_host_id` non-empty ≤128자 제어금지).

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `UT-PROTO-01..04` 전부 통과 + 기존 `ddg-agents-proto` 테스트 무회귀.
- 같은 태스크에서 작성·수정할 유닛 테스트: `UT-PROTO-01`, `UT-PROTO-02`, `UT-PROTO-03`, `UT-PROTO-04` (`TESTS.md` §1).
- 실행할 테스트와 명령: `DDG_UPDATE_SCHEMA=1 cargo test -p ddg-agents-proto && cargo test -p ddg-agents-proto`
- 테스트 데이터·환경과 독립적인 기대값 근거: Unicode UAX #9, OSC 7501 규범, `holder.rs` 9B 프레임 헤더 명세, `openrpc.rs` 스냅샷 규약.
- 통과 기준과 남는 검증 범위: 경고 0개 + 전체 테스트 PASS; 실제 와이어 호환은 `T2-B`/`T6-A(A3)`에서 검증.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-A`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- `CATALOG_SCHEMA_VERSION`을 3으로 올리지 않는다. `TabCreateParams`에 `parent_session_id`/`parent_host_id`를 추가하지 않는다. `host.tab_activity_snapshot` 및 `AgentStatus.turn`을 추가하지 않는다.

## 산출물과 독립·통합 완료 기준
- `ddg-agents-proto` 컴파일·테스트 통과 + `schema/*.openrpc.json` 동기화 완료.

## 병렬 작업 주의사항과 공유 파일
- 후속 병렬 그룹 A(`T2-A` ∥ `T2-B`)의 선행 태스크이므로 `T1` 완료 후 동시 착수.

## 차단 조건과 처리 방법
- 기존 `openrpc.rs` 스냅샷 외 하위 크레이트 컴파일 영향이 있으면 구조체 생성자 기본값 영향 범위 확인 후 보고(하위 크레이트 수정은 각 크레이트 태스크에서 수행하되, 워크스페이스 컴파일이 깨지면 최소 필드를 `#[serde(default)]` + 호환 생성자 유지).

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.

---

# 태스크: T2-A — `ddg-agents-vt` OSC 7501 파서 및 단일 FIFO `Relay` 구현 (병렬 그룹 A)

## 목표
공식 OSC 7501 파서(`osc7501.rs`)와 단일 FIFO 응답 순서(`vt`→`7501;?`→후속 `DA1`)·4096B 초과 drain-and-discard·`flush()` 비유출·`replay=true` 응답 억제를 보장하는 `Relay` 확장을 TDD로 구현한다.

## 태스크 유형: 구현
## 수행 역할과 역할 정의의 정확한 경로
- `implementer` — `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` (로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`)

## 실행 상태: 착수 가능 (`T1` 완료 후, `T2-B`와 병렬 가능)
## 선행 태스크와 착수 조건
- `T1` 완료.

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음.
- 허용된 승인 준비·조건 확인 작업: `crates/ddg-agents-vt/src/{relay.rs, terminal.rs}` 기존 테스트 확인.
- 대기 중 계속할 독립 작업: `T2-B`와 파일 비충돌 병렬 진행 가능.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `DNL-01`: `pi-tui`가 `7501;?`와 `DA1`을 단일 write로 보내므로 `Relay`가 단일 FIFO 응답 버퍼로 순서를 보존해야 함.
- `DNL-03`: `title`/`msg`는 무패딩 허용 표준 Base64 전용(평문 폴백 금지).

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [DESIGN_DATA.md](DESIGN_DATA.md#1-필수-validation-및-신뢰-경계-확정-표-13개-경계) (경계 #2)
- [DESIGN_DATA.md](DESIGN_DATA.md#31-osc-7501--단일-fifo-응답-버퍼-ddg-agents-vt)
- [TESTS.md](TESTS.md#2-cratesddg-agents-vt-2그룹) (`UT-VT-01`, `UT-VT-02`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `crates/ddg-agents-vt/src/{lib.rs, osc7501.rs, relay.rs}`
- 쓰기 금지: `crates/ddg-agents-proto/**`, `crates/ddg-agents-transport/**`, 그 외 크레이트

## 수행 내용
1. `osc7501.rs`: `MAX_OSC_7501_BYTES=4096`, `MAX_STATUS_RECORDS=16`, `id` 화이트리스트(`^[A-Za-z0-9._-]{1,64}$`, `..` 금지), 무패딩 허용 표준 RFC 4648 Base64 전용 디코더(평문·URL-safe 거부) → 엄격 UTF-8 → `sanitize_display_text`, `AgentState`/`BlockedKind` 파싱, `state=clear`, `OSC 133;A` 관찰 클리어, 우선순위 집계(`Blocked > Error > Working > Done > Idle`).
2. `relay.rs`: `ESC]7501;` 감지 시 전용 `4105B` 상한 상태 진입(일반 OSC는 `MAX_STRING=512` 유지), 4096B 초과 시 종결자(`BEL`/`ST`/`CAN`/`SUB`/비-`\` `ESC`)까지 drain-and-discard, 완료 7501은 `viewer_bytes`·`Vt`에서 완전 스트립, `Relay::flush()` 시 부분/drain 7501 `vt.write()` 생략, `Relay::feed_with_events(..., now_ms, replay)` 단일 FIFO 응답 버퍼(`vt.replies()` drain → `\x1b]7501;?\x1b\\` → 후속 바이트), `replay=true` 응답 억제.

## 구현 착수 근거
- 변경 대상: `crates/ddg-agents-vt/src/{lib.rs, osc7501.rs, relay.rs}`.
- 계약: `D-SEC-05`, `DNL-01`, `DNL-02`, `DNL-03`, `F-08`, `E-SEC-05`.
- 검증 방법: `UT-VT-01`, `UT-VT-02` TDD.
- 관련 리뷰 결과: `data-network-layer-reviewer`, `security-reviewer`, `test-reviewer` 전원 반영.

## 필수 Validation과 실패 처리
- 불량/초과 OSC 7501은 레코드 단위 Fail-closed 폐기, PTY 스트림은 Fail-open 유지.

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `UT-VT-01`, `UT-VT-02` 및 기존 `ddg-agents-vt` 9개 테스트 전부 PASS.
- 같은 태스크에서 작성·수정할 유닛 테스트: `UT-VT-01`, `UT-VT-02` (`TESTS.md` §2).
- 실행할 테스트와 명령: `cargo test -p ddg-agents-vt`
- 테스트 데이터·환경과 독립적인 기대값 근거: RFC 4648 Section 4, ECMA-48, OSC 133, `pi-tui` DA1 센티넬.
- 통과 기준과 남는 검증 범위: 경고 0개 + 전체 PASS; 실제 `pi`/`codex` 실측은 `T6-A(A1-live-agents)`.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-A`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- OSC 7501 평문 폴백 금지; `OSC 133;A` 스트립 금지(관찰만).

## 산출물과 독립·통합 완료 기준
- `crates/ddg-agents-vt` 테스트 전부 PASS.

## 병렬 작업 주의사항과 공유 파일
- **병렬 그룹 A**: `T2-B`(`ddg-agents-transport`)와 공유 파일 0개, 안전 병렬 실행 가능.

## 차단 조건과 처리 방법
- `libghostty-vt` 환경 누락 시 `GHOSTTY_VT_LIB_DIR` 확인.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.

---

# 태스크: T2-B — `ddg-agents-transport` 공용 설정(`agents.json`/`programs.json`) · `split_safe_tokens` · 협상 · 안전 업그레이드 (병렬 그룹 A)

## 목표
CLI와 GUI가 단일 진실 원천으로 공유하는 `agents.json` v1(`agent.rs`), `programs.json` v1(`programs.rs`), 보강된 `pub fn split_safe_tokens`(`profile.rs`), `HostClient` `"agent_activity_v1"` feature-gating(`client.rs`), 및 `expected_host_id` 보존·`host`→`core` 안전 재시작 업그레이드(`install.rs`)를 TDD로 구현한다.

## 태스크 유형: 구현
## 수행 역할과 역할 정의의 정확한 경로
- `implementer` — `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` (로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`)

## 실행 상태: 착수 가능 (`T1` 완료 후, `T2-A`와 병렬 가능)
## 선행 태스크와 착수 조건
- `T1` 완료.

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음.
- 허용된 승인 준비·조건 확인 작업: `crates/ddg-agents-transport/src/{profile.rs, client.rs, install.rs}` 확인.
- 대기 중 계속할 독립 작업: `T2-A`와 병렬 실행.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `RC-03`: `AgentProfile.argv: Vec<String>` 통일, `color`/`program_id` 삭제, `host_overrides.env: BTreeMap<String, Option<String>>`(D-4).
- `DNL-11`: `sha2` 크레이트 미추가, `flock(agents.lock)` 내부 원본 바이트 직접 비교 CAS + `(mtime_nanos, len, ino)`.
- `D-3`: 동적 로더 4종(`LD_PRELOAD`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, `DYLD_FORCE_FLAT_NAMESPACE`) 차단(`LD_LIBRARY_PATH` 허용).

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [DESIGN_DATA.md](DESIGN_DATA.md#1-필수-validation-및-신뢰-경계-확정-표-13개-경계) (경계 #8, #9, #10)
- [DESIGN_DATA.md](DESIGN_DATA.md#21-configddgagentsjson-schema-v1--ddg-agents-transportagent)
- [TESTS.md](TESTS.md#5-cratesddg-agents-transport-3그룹) (`UT-TRANS-01`..`UT-TRANS-03`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `crates/ddg-agents-transport/src/{lib.rs, agent.rs, programs.rs, profile.rs, client.rs, install.rs}`
- 쓰기 금지: `crates/ddg-agents-vt/**`, `crates/ddg-agents-proto/**`, 그 외 크레이트

## 수행 내용
1. `profile.rs`: `split_safe_tokens`를 `pub` 승격하고 루프 최상단(따옴표 안팎)에서 `\0`/`\r`/`\n`/`U+2028`/`U+2029`/제어문자/Bidi 전 구간 거부 + 종료 시 `in_single || in_double || escaped` 거부.
2. `agent.rs`: `AgentsConfig`/`AgentProfile`/`AgentHostOverride` v1, `MAX_AGENTS_BYTES=512KiB`, `O_NOFOLLOW` + `0o700` 디렉터리 보정 + `0o600` `create_new` 원자 저장 + `.corrupt.<ts>[.<n>]` 백업, `flock(agents.lock)`(주입 가능 타임아웃) 내부 바이트 CAS, `DDG_*`·시크릿 키·동적 로더 4종 차단(`LD_LIBRARY_PATH` 허용), 값 비에코 오류, `resolve_executor`(부모 `std::env` 미복사).
3. `programs.rs`: `ProgramsConfig` v1 Fail-open 로더 + 내장 기본 매핑 테이블(`claude-code`, `codex`, `pi`, `gemini`, `aider`, `opencode`, `cursor`, `nvim`, `cargo`, `zsh`/`bash`/`fish` 등).
4. `client.rs`: `ConnectionState::Connected { host_id, conn_id, features }`, `HostClient::supports_feature("agent_activity_v1")`, 구버전 호스트 대상 `tab_create`/`tab_split`/`room_create` 시 선택 필드 `None` 생략(`deny_unknown_fields` 호환) + `tab_mark_seen` 스킵, `catalog_subscribe` 채널 `TabActivityChanged`/`TabSeenChanged` 수신 처리.
5. `install.rs`: `expected_host_id` 선검증(`RequirePinnedHostId`)·보존(불일치 Fail-closed, 미덮어쓰기), 원격 재시작 스크립트(`host`→`core` 종료 순서, PID `[ ! -L ] && [ -f ]` + 양의 정수 `!= 1` + `ps` comm/argv 검증, PID 부재 구버전 `ps -u "$(id -u)" -o pid=,args=` 정확 매칭 `ddg-agents-host serve`/`ddg-terminal-server core`만·`$$`/`holder` 제외, 종료/소켓 해제 대기), 사후 `hello.features` `"agent_activity_v1"` 검증.

## 구현 착수 근거
- 변경 대상: `ddg-agents-transport` 6개 파일.
- 계약: `D-SEC-01`, `D-SEC-02`, `D-SEC-08`, `DNL-07`, `DNL-10`, `DNL-11`, `RC-03`, `RC-05`, `RC-06`.
- 검증 방법: `UT-TRANS-01..03` TDD.
- 관련 리뷰 결과: `data-network-layer-reviewer`, `security-reviewer`, `test-reviewer` 전원 반영.

## 선택한 해법의 한계·재검토 조건
- `DESIGN_DATA.md` §4 (#3 `DockerExec` 수동 업그레이드, #5 오프라인 호스트 `host_overrides` 고아 키).

## 필수 Validation과 실패 처리
- `DESIGN_DATA.md` 경계 #8(Fail-closed 비에코), #9(Fail-open), #10(신원/사후 feature Fail-closed, 미지원 feature Fail-open).

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `UT-TRANS-01..03` 및 기존 `ddg-agents-transport` 테스트 PASS.
- 같은 태스크에서 작성·수정할 유닛 테스트: `UT-TRANS-01`, `UT-TRANS-02`, `UT-TRANS-03` (`TESTS.md` §5).
- 실행할 테스트와 명령: `cargo test -p ddg-agents-transport`
- 테스트 데이터·환경과 독립적인 기대값 근거: POSIX `O_NOFOLLOW`/`flock`/`0600`, `ld.so`/`dyld` 동적 로더 목록, `install.rs` 셸 스텁 패턴.
- 통과 기준과 남는 검증 범위: 경고 0개 + 전체 PASS; 실제 Linux 컨테이너 업그레이드는 `T6-A(A3)`.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-A`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- `sha2` 외부 크레이트 추가 금지. `profile.rs:190`의 비원자 `fs::write(&tmp)` 패턴을 `agent.rs`에 복제 금지. `expected_host_id` 덮어쓰기 금지.

## 산출물과 독립·통합 완료 기준
- `crates/ddg-agents-transport` 컴파일·테스트 PASS.

## 병렬 작업 주의사항과 공유 파일
- **병렬 그룹 A**: `T2-A`와 공유 파일 0개, 병렬 실행 가능.

## 차단 조건과 처리 방법
- macOS/Linux `flock`·`O_NOFOLLOW` 분기는 `libc`로 깔끔히 처리.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.

---

# 태스크: T3 — `ddg-terminal-server` OSC 7501 통합 · `FG_PGRP` · `proc_info` · 코얼레싱 · `core.pid`

## 목표
`ddg-terminal-server`의 PTY 환경 정리(`TERM_PROGRAM=ddg` + 외부 변수 제거), `Holder` `fg_pgrp_v1`/`QUERY_FG_PGRP`, `Core` `proc_info.rs`(macOS + Linux), 0ms/200ms/1000ms 코얼레싱·폴링, `SIGKILL` 1 MiB `tail` 재생 복구, `SIGTERM` `GOODBYE::Detach`→`BYE` 대기 및 `core.pid`를 TDD로 구현한다.

## 태스크 유형: 구현
## 수행 역할과 역할 정의의 정확한 경로
- `implementer` — `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` (로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`)

## 실행 상태: 착수 가능 (`T1`, `T2-A` 완료 후)
## 선행 태스크와 착수 조건
- `T1`, `T2-A` 완료.

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음.
- 허용된 승인 준비·조건 확인 작업: `crates/ddg-terminal-server/src/{pty.rs, holder.rs, core.rs, server.rs}` 확인.
- 대기 중 계속할 독립 작업: 없음.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `DNL-08`: 구 `Holder` 생존 시 무응답 행 방지를 위해 `HolderWelcome.features` `"fg_pgrp_v1"` 확인 후에만 폴링.
- `DNL-15`: `TERM_PROGRAM=ghostty` 스푸핑 금지(`pi` Kitty 이미지 오예약 방지), `TERM_PROGRAM=ddg` + 외부 `TMUX`/`KITTY_WINDOW_ID`/`GHOSTTY_RESOURCES_DIR` 등 `env_remove`.

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [DESIGN_DATA.md](DESIGN_DATA.md#1-필수-validation-및-신뢰-경계-확정-표-13개-경계) (경계 #3, #4)
- [DESIGN_DATA.md](DESIGN_DATA.md#3-osc-7501-fifo-relay--os-foreground--프로토콜rpc-계약)
- [TESTS.md](TESTS.md#3-cratesddg-terminal-server-4그룹) (`UT-TERM-01`..`UT-TERM-04`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `crates/ddg-terminal-server/src/{pty.rs, holder.rs, proc_info.rs, core.rs, server.rs, main.rs}`
- 쓰기 금지: 그 외 크레이트

## 수행 내용
1. `pty.rs`: `TERM_PROGRAM=ddg`, `TERM_PROGRAM_VERSION=<ver>` 기본 설정 + 외부 터미널 변수(`TMUX`, `TMUX_PANE`, `ZELLIJ*`, `KITTY_WINDOW_ID`, `GHOSTTY_RESOURCES_DIR`, `WEZTERM_PANE`, `ITERM_SESSION_ID`, `TERM_PROGRAM_VERSION`) `env_remove`(사용자 프로파일 명시값 우선); `PtyHandle::foreground_pgrp(&self) -> Option<u32>` (`tcgetpgrp`).
2. `holder.rs`: `HolderWelcome.features = ["fg_pgrp_v1"]`, `QUERY_FG_PGRP(8)` 수신 시 live PTY 배치 `FG_PGRP(108)` 반환("no VT parsing, no policy" 유지).
3. `proc_info.rs`: 순수 파서(macOS `KERN_PROCARGS2` + Linux `/proc` `cmdline`/`stat`/`status`) 및 OS 조회(SID/PGID/UID 교차 검증, `argc` 상한·`envp` 미접근, 허용 인터프리터만 스캔·`env`→`None`, `-c`/`-e`/`--eval`/`-p`/`-m`/`-`/미지 `-*` 즉시 중단, `=`/`://`/`@`/공백/제어 폐기, basename 64자 살균).
4. `core.rs`/`server.rs`: `HelloResult.features`에 `"agent_activity_v1"` 추가, `Relay::feed_with_events` FIFO 응답 전송, 0ms 상태 전이 + 주입 시계 기반 200ms trailing-edge 코얼레싱 + 1,000ms 락 외부 배치 `foreground` 폴링(`fg_pgrp_v1` 지원 시만), `recover_ptys()` `replay=true` 1 MiB `tail` 재생 복구, `SIGTERM` `GOODBYE::Detach`→`BYE` 대기(최대 200ms), `terminal.lock` 획득 후 `core.pid` (`0o600`, `O_NOFOLLOW`) 기록·종료 삭제.

## 구현 착수 근거
- 변경 대상: `ddg-terminal-server` 6개 파일.
- 계약: `D-SEC-06`, `DNL-07`, `DNL-08`, `DNL-15`, `F-09`, `E-SEC-06`.
- 검증 방법: `UT-TERM-01..04` TDD + 기존 6개 테스트(실제 PTY + Core `SIGKILL` 복구 포함).

## 선택한 해법의 한계·재검토 조건
- `DESIGN_DATA.md` §4 (#1 1 MiB tail 한계, #2 구 Holder `foreground=None`, #4 1초 미만 단명 프로세스).

## 필수 Validation과 실패 처리
- `DESIGN_DATA.md` 경계 #3(Fail-open `None`), #4.

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `UT-TERM-01..04` 및 기존 `ddg-terminal-server` 테스트 PASS.
- 같은 태스크에서 작성·수정할 유닛 테스트: `UT-TERM-01`..`UT-TERM-04` (`TESTS.md` §3).
- 실행할 테스트와 명령: `cargo test -p ddg-terminal-server`
- 테스트 데이터·환경과 독립적인 기대값 근거: XNU `KERN_PROCARGS2`, Linux `proc(5)`, POSIX `tcgetpgrp`, 주입 `Instant` 타이밍 계약.
- 통과 기준과 남는 검증 범위: 경고 0개 + 전체 PASS; 실제 `SIGKILL`·`pi`/`codex`는 `T6-A(A1/A2)`.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-A`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- `Holder`에 VT/OSC 파싱이나 정책 로직 추가 금지. `Core` 락 보유 중 OS I/O(`proc_info`/소켓 송수신) 금지.

## 산출물과 독립·통합 완료 기준
- `crates/ddg-terminal-server` 컴파일·테스트 PASS.

## 병렬 작업 주의사항과 공유 파일
- 완료 시 후속 병렬 그룹 B(`T4-A` ∥ `T4-B`) 착수 가능.

## 차단 조건과 처리 방법
- Linux `/proc` 파서 유닛 테스트는 macOS에서도 순수 바이트 파서로 항상 실행되도록 플랫폼 독립 모듈 분리.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.
