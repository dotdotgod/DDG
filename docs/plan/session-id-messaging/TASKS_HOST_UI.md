# 실행 태스크 명세: Phase S3 – S5 (Host · Native UI · 검증 · 종료)

[계획 목차](README.md) · [태스크 S0–S2](TASKS_CORE.md) · [UI 설계](DESIGN_UI.md) · [데이터 설계](DESIGN_DATA.md) · [시나리오](SCENARIOS.md)

> 공통 실행 규칙(`implementer.md`, `pi --provider openai --model gpt-6.2-sol --thinking high`, 비가역 금지)은 [TASKS_CORE.md](TASKS_CORE.md) 상단과 동일하다.

---

## 태스크 S3-A: `ddg-agents-host` 카탈로그 v2·토큰·세션·플러그인 격리
- **목표**: `catalog_store.rs`(v1 백업 리셋), `token_store.rs`(RAM CSPRNG+zeroize), `host_service.rs`(세션·메시지·동시성·보상·Q2·감사 로그), `plugin_host.rs`(숨김/배경 격리)를 구현한다.
- **유형 / 상태**: 구현 / S2-A 완료 후 착수 가능.
- **수행 역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`), `pi --provider openai --model gpt-6.2-sol --thinking high`.
- **선행 태스크**: S1, S2-A (`ddg-terminal-server` 신규 RPC·홀더 계약 필요).
- **참조 설계**: [DESIGN_DATA.md §2-2, §4, §5](DESIGN_DATA.md), [TESTS.md §2-4, §3](TESTS.md).
- **쓰기 허용 / 금지**: 허용=`crates/ddg-agents-host/**`, `examples/demo_plugin.py` / 금지=`crates/ddg-agents-proto/**`, `crates/ddg-terminal-server/**`, `crates/ddg-agents-transport/**`, `native/**`.
- **구현 착수 근거 및 변경 내용**:
  - `catalog_store.rs`: v1 → `catalog.json.v1.bak.<ts>`(`0o600`, 충돌 방지) + 빈 v2 + basename `reset_notice`(첫 가시 룸 생성 시 해제); 재부팅 시 미실행 `Background` 탭 제거.
  - `token_store.rs` 신규: `<ses_id>.<gen>.<64-hex>`, OS CSPRNG fail-closed(`paths::random_hex()` 금지), `ct_eq`, `write_volatile`+`compiler_fence(SeqCst)` zeroize, `token_revoked` vs `token_invalid`.
  - `host_service.rs`: 스폰 시 6개 `DDG_*` env 주입, `session_token` 필수 검증 및 `from_session_id`/`parent_session_id` 서버 파생, `InFlight` 즉시 `Conflict`, `in_flight_creates/restarts/closes`+보상 `pty.kill`, Q2 순서(알림→삭제→폐기), `session_move` 원자 롤백, `TermConn` `boot_generation` 재연결, 시작 시 v1 고아 PTY `pty.kill`, `host.log` `0o600` 메타데이터 감사 로그, 표시 텍스트 정제.
  - `plugin_host.rs` + `examples/demo_plugin.py`: v2 문자열 ID 이전, `catalog_get` `Hidden` 제외, `pty.list`/`pty.kill` `Hidden`/`Background` 차단, `Background` 변조 및 `Hidden` IDOR 차단.
- **테스트 (필수, 동일 태스크 구현)**: [TESTS.md §2-4 #11–#14](TESTS.md) + 기존 통합 테스트(`local_transport`, `core_restart`, `host_reboot`, `plugin_integration`) + 신규 통합 테스트 3종(`v1_reset_and_orphan_kill`, `session_lifecycle_1b_3a_q2`, `session_messaging_and_io`).
  - 실행: `cargo test -p ddg-agents-host` — 경고 0, 전 테스트 통과.
- **비가역 실행**: 없음(테스트는 스크래치 `DDG_*_DIR`만 사용).
- **병렬 주의**: `host_service.rs` 공유로 S3-B와 순차 진행.

---

## 태스크 S3-B: `ddg-agents-host` 브리지 CLI(`cli.rs`) 및 Hub Relay(`hub.rs`)
- **목표**: `ddg-agents-host`에 `ddg-agents-transport` 의존성을 연결하고 브리지 CLI(`cli.rs`)와 opt-in Cross-Host Hub Relay(`hub.rs`)를 구현한다.
- **유형 / 상태**: 구현 / S2-B 및 S3-A 완료 후 착수 가능.
- **수행 역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`), `pi --provider openai --model gpt-6.2-sol --thinking high`.
- **선행 태스크**: S2-B (`DialPolicy::RequirePinnedHostId`, `relay_attach`), S3-A (`host_service.rs`, `token_store.rs`).
- **참조 설계**: [DESIGN_DATA.md §3, §5](DESIGN_DATA.md), [TESTS.md §2-4 #15–#16, §3](TESTS.md).
- **쓰기 허용 / 금지**: 허용=`crates/ddg-agents-host/**`, `Cargo.lock` / 금지=타 크레이트 소스, `native/**`.
- **구현 착수 근거 및 변경 내용**:
  - `crates/ddg-agents-host/Cargo.toml`: `ddg-agents-transport` 의존성 추가(비순환).
  - `hub.rs` 신규: `DDG_HUB_ENABLED=1` opt-in(기본 OFF, `connect`에서 미기동), `Local { sock_path: None }`·셀프 `host_id` 스킵, `RequirePinnedHostId`, 단일 양방향 `host.relay_attach`(토큰 세션/활성 Hub 거부, `Close(RelayReplaced)` 재연결 루프 금지), `hop == 1`, 9개 허용 메서드, `from_host_id` 덮어쓰기, `host.list_hosts` 최소 필드 3개.
  - `cli.rs` 신규 + `main.rs` 연결: `host list`, `session create|list|move|close|restart|screen|write`, `msg send|recv|subscribe`; `--token` 거부, 쓰기 커맨드 `--host all` 거부, `session list --host all` 부분 성공, 빈 `msg recv`=`exit 0`+`[]`.
- **테스트 (필수, 동일 태스크 구현)**: [TESTS.md §2-4 #15–#16](TESTS.md) + 신규 통합 테스트 3종(`bridge_cli_e2e`, `hub_relay_cross_host`, `security_negative_boundaries`).
  - 실행: `cargo test -p ddg-agents-host` — 경고 0, 전 테스트 통과.
- **비가역 실행**: 없음.
- **병렬 주의**: S3-B 완료 후 S4는 독립 크레이트(`native/`)이므로 S3-B 막바지 검수와 중첩 가능하나, `HostBridge`가 `ddg-agents-transport`만 의존하므로 S2-B 완료 직후 S4 선행 착수도 가능(권장 순서: S3-B와 S4 병렬 실행 가능 — 공유 파일 0개!).

---

## 태스크 S4: `native/` GPUI v2 전환 및 최소 UX *(S3-B와 병렬 가능)*
- **목표**: 공식 GPUI 앱(`native/`)을 v2 문자열 ID, 숨김 룸 비노출, 단일 재어태치, v1 리셋 배너, 3초 외부 입력 배지로 전환한다.
- **유형 / 상태**: 구현 / S2-B 완료 후 착수 가능(S3-B와 공유 파일 없어 병렬 가능).
- **수행 역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`), `pi --provider openai --model gpt-6.2-sol --thinking high`.
- **선행 태스크**: S1, S2-B (`ddg-agents-transport` v2 `client.rs`).
- **참조 설계**: [DESIGN_UI.md §1–§4](DESIGN_UI.md), [TESTS.md §2-5](TESTS.md).
- **쓰기 허용 / 금지**: 허용=`native/**` / 금지=`crates/**`, `schema/**`, `docker/**`.
- **구현 착수 근거 및 변경 내용**:
  - `native/src/host_bridge.rs`: `u64 -> String` ID, `restart_tab` 중복 `tab_attach` 제거, `BridgeEvent::{TabAttachFailed, ExternalInput, SessionExited, CatalogSubscribed}`, `TabAttached.run_generation`.
  - `native/src/main.rs`: `stable_elem_id`, `Hidden` 필터링, `sync_machine_rooms`(구 채널 반환 + `run_generation` VT 리셋), stale `TabAttached` 드롭, `session_move` 0→1 `collapsed=false` + 조건부 선택, 빈 머신 헤더 클릭 선택 + 호스트별 `reset_notice` 배너, 주입 시계 기반 3초 외부 입력 배지(`.ml_auto().flex_shrink_0()`), `+` 버튼 `flex_none`, 표시 문자열 `sanitize_display_text`.
- **테스트 (필수, 동일 태스크 구현)**: [TESTS.md §2-5 #17](TESTS.md) — 기존 `native/src/main.rs:2423` 순수 함수 패턴 준수(GPUI 목킹 금지, 주입 시계 `2.999s`/`3.000s`).
  - 실행: `GHOSTTY_VT_LIB_DIR=... cargo test -p ddg-agents-native` — 경고 0, 전 테스트 통과.
- **비가역 실행**: 없음.

---

## 태스크 S5-A / S5-B / S5-C: 통합 검증 · 시나리오 · 아카이브 종료
- **S5-A (검증 — 에이전트 시나리오 1 + 전체 워크스페이스)**:
  - 선행: S3-B, S4 완료. 역할: `implementer`(실행) + Lead(독립 검수).
  - 실행: `cargo test --workspace` + Docker 이미지 재빌드 및 `docker_live` + [SCENARIOS.md 시나리오 1](SCENARIOS.md#사용자-시나리오-1-단일-호스트--docker-크로스-호스트-에이전트-협업-e2e) 전 단계 증거 수집(`stat 600`, `grep 0`, 원격 무자격증명, 잔존 PID 0). Docker 불가 시 원격 구간 `Unverified` 기록.
- **S5-B (검증 — 사람 GUI 시나리오 2)**:
  - 선행: S4, S5-A 완료. 역할: **사용자(사람)** + Lead(스크래치 픽스처·CLI 보조 및 결과 기록).
  - 수행: [SCENARIOS.md 시나리오 2](SCENARIOS.md#사용자-시나리오-2-사람-gui-실기-검증-v1-리셋--숨김-격리--승격--외부-입력-배지) 5개 체크 수행. 미수행 시 `Unverified` 기록(완료 오기 금지).
- **S5-C (종료 — 최종 보고 및 아카이브 이관)**:
  - 선행: S5-A 완료(S5-B 결과 포함, 미수행 시 부분 완료 + `Unverified` 명시). 역할: Lead (`completion-reporter`).
  - 산출물: `docs/archive/report/YYYY-MM-DD/session-id-messaging/FINAL.md` 작성, 완료 시 `docs/plan/session-id-messaging/` → `docs/archive/plan/session-id-messaging/` 이동 및 `docs/plan/README.md`·`docs/archive/README.md`·상호 링크 갱신.
  - **Git 머지 조건**: 현재 디렉터리는 Git 저장소가 아니므로 `main` 머지 미수행(`Git 저장소 아님`); 향후 Git 초기화·머지 시 커밋 본문에 보고서/계획 경로, 선택 근거·대안·검증·남은 위험을 포함하도록 인계.
