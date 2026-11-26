# 테스트 설계 (TDD 유닛 테스트 & 격리 통합 테스트)

[계획 목차](README.md) · [UI·UX 설계](DESIGN_UI.md) · [데이터·네트워크 설계](DESIGN_DATA.md) · [사용자 시나리오](SCENARIOS.md)

## 1. 테스트 전략과 적용 범위
- **TDD 유닛 테스트**: 구현과 같은 태스크에서 먼저 실패 테스트 작성 후 구현. 크레이트별 중복 매트릭스를 제거하고 정규 소유 크레이트에서 1회 집중 검증한다(`base64`·`Bidi`=`proto`, `RequirePinnedHostId`=`transport`, 릴레이 정책=`host`, UI 상태 전이=`native` 순수 함수).
- **자동화 E2E 제외 범위**: GPUI 실창 UI 클릭 자동화는 제외하고 [SCENARIOS.md](SCENARIOS.md)의 사람 GUI 시나리오로 검증한다. 서버·CLI·Docker 크로스 호스트 검증은 스크래치 격리 통합 테스트 + 에이전트 시나리오 1로 검증한다.

## 2. 크레이트별 TDD 유닛 테스트 (구현 태스크에 동시 배정)

### 2-1. `crates/ddg-agents-proto` (태스크 S1)
1. **`id.rs` 코덱·CSPRNG**: `rm_`/`rm_bg_`/`ses_`/`msg_` 정규성·대문자 거부·시간 단조 정렬·유일성; CSPRNG 오류 주입 시 fail-closed 에러 반환(폴백 금지).
2. **`decode_base64_bounded` & `sanitize_display_text` 정규 매트릭스**: `65_536`B 통과, `65_537`B·문자열 길이 `> 87_384` 사전 거부, 비정상 패딩 거부; C0/C1·CR/LF·전체 Bidi(`U+061C`, `U+200E`, `U+200F`, `U+202A..=U+202E`, `U+2066..=U+2069`) 제거.
3. **Catalog v2 불변식**: `Standard` `1..=16` `Visible`, `Background` 최대 1개·`0..=64` `Hidden`(빈 상태 허용), `run_generation >= 1`.
4. **OpenRPC + 음성(Negative) 계약 테스트**: `schema/ddg-terminal-server.openrpc.json`·`schema/ddg-agents-host.openrpc.json` 동기화 검증 + 독립 음성 단언: `TabCreateParams`에 `parent_session_id` 필드 부재, `host.relay_exec` 메서드 부재.

### 2-2. `crates/ddg-terminal-server` (태스크 S2-A)
5. **`holder.rs`**: `session_id`·`run_generation` 라운드트립, `#[serde(default)]` 역직렬화, Core `SIGKILL` 복구 시 종료 PTY 즉시 `RELEASE`.
6. **`mailbox.rs`**: `64 KiB`/메시지, `256개`/`1 MiB`/세션, `32 MiB` 전역 상한; 일반 메시지 가득 시 최오래된 일반 메시지 축출 + `session_exited` 보호; 소비·퍼지 후 전역 바이트 `0` 복원(할당량 누수 0); PTY 종료/`run_generation` 증가/`pty.kill` 자동 퍼지.
7. **`session.read_screen` & `write_input`**: `lines == 0`·`max_bytes == 0`·상한 초과 → `invalid_params`; 멀티바이트 UTF-8 문자 경계 안전 절단; `expect_run_generation` 불일치 → `generation_mismatch`; `write_input` 후 GUI `ControlMode::Control` 유지 및 메타데이터 전용 `PtyEvent::ExternalInput` 발행(원문 미포함).

### 2-3. `crates/ddg-agents-transport` (태스크 S2-B)
8. **`profile.rs`**: `HostProfile.id == "all"`(`"ALL"`, `" all "` 정규화 포함) 거부, 중복 `id` 거부.
9. **`dial.rs` 정규 매트릭스**: `DialPolicy::RequirePinnedHostId`에서 `None`/빈 문자열/공백 `expected_host_id` → `host_security_blocked`; `build_ssh_command`에 `LC_ALL=C`·`LANG=C` 포함; `sanitize_dial_error`에서 로컬 경로·사용자명·키 경로 마스킹.
10. **`client.rs`**: v2 문자열 ID, `TabStreamHandle.run_generation`, `reset_notice`, `HostEvent::ExternalInput`/`SessionExited`, `relay_attach` 프레임 파싱.

### 2-4. `crates/ddg-agents-host` (태스크 S3-A, S3-B)
11. **`catalog_store.rs`**: v1 `catalog.json` → `catalog.json.v1.bak.<ts>`(`0o600`, 이름 충돌 안전) 백업 + 빈 v2 초기화 + basename 전용 `reset_notice`; 첫 가시 `Standard` 룸 생성 시 `reset_notice` 해제; 재부팅 시 미실행 `Background` 탭 삭제.
12. **`token_store.rs`**: CSPRNG 토큰 생성, `ct_eq` 검증, 구 세대=`token_revoked` vs 위조=`token_invalid`, **타 세션 토큰 교차 오용(`ses_A` 토큰으로 `ses_B` 행세) 거부**, Drop 후 메모리 zeroize 확인.
13. **`host_service.rs` 동시성·보상·Q2**: `InFlight` 멱등 키 즉시 `Conflict`; 16/64 한도 경합 시 초과 차단 + 실패 시 `pty.kill` 보상 및 `in_flight` 잠금 해제(재시도 가능 검증); Q2 숨김 종료 순서(알림→삭제→폐기); `session_move` 16탭 초과 시 원자 롤백; `host.log` `0o600` 권한 및 토큰·페이로드 미포함.
14. **`plugin_host.rs` 격리**: `host.catalog_get` 숨김 룸 제외, `pty.list`/`pty.kill`에서 `Hidden`/`Background` PTY 차단, `Background` `room_rename`/`tab_create` 및 `Hidden` IDOR 거부, `session_*`/`msg_*`/`relay_*` 거부.
15. **`hub.rs`**: `DDG_HUB_ENABLED` 기본 OFF, `hop != 1` 거부, 자기 `host_id` 셀프 릴레이 차단, 9개 허용 메서드 외 거부, 토큰 세션·활성 Hub의 `relay_attach` 거부, `Close(RelayReplaced)` 재연결 루프 중단, `host.list_hosts` 민감 필드 0개, `from_host_id` 강제 덮어쓰기.
16. **`cli.rs`**: `--token` 인자 거부, 쓰기 커맨드 `--host all` 거부, `session list --host all` 부분 성공 JSON, 빈 `msg recv` → `exit 0` + `[]`, `--enter`/`--stdin`/`--expect-gen` 파싱.

### 2-5. `native/` (태스크 S4 — 기존 `main.rs:2423` 순수 함수 패턴 준수)
17. **GPUI 비의존 순수 상태 단위 테스트**: `visibility == Hidden` 필터링, `sync_machine_rooms`의 `run_generation` 증가 시 구 채널 반환+VT 리셋, `TabAttachFailed`의 `attaching=false` 복구, `handle.run_generation < tab.run_generation` 드롭, `session_move` 0→1 `collapsed=false` & 조건부 자동 선택, 호스트별 `reset_notice` 해제, **주입 시계(`Instant` 파라미터) 기준 `2.999s` 유지 / `3.000s` 만료** 배지, `stable_elem_id` 경계 충돌 방지(`("a","bc") != ("ab","c")`).

## 3. 격리 통합 테스트 (스크래치 `DDG_*_DIR` 전용)
- **기존 7개 통합 테스트 v2 문자열 ID 이전**: `local_transport`, `core_restart`, `host_reboot`, `plugin_integration`(숨김 격리 케이스 추가), `docker_live`(크로스 호스트 Hub 릴레이 추가), `examples/demo_plugin.py` 동기화.
- **신규 6개 통합 테스트 (`crates/ddg-agents-host/tests/`)**:
  1. `v1_reset_and_orphan_kill.rs`: v1 카탈로그 백업(`0o600`) + `session_id` 없는 생존 고아 PTY 자동 `pty.kill`.
  2. `session_lifecycle_1b_3a_q2.rs`: `1B` 재시작 세대 증가·구 토큰 폐기, `3A` `session_move` 승격, `Q2` 숨김 종료 즉시 삭제 및 부모 인박스 알림.
  3. `session_messaging_and_io.rs`: 2개 세션 간 `msg send/recv/subscribe`, `read_screen`, `write_input`(제어권 미탈취 + `ExternalInput` 메타데이터).
  4. `bridge_cli_e2e.rs`: 자동 주입 `DDG_*` env로 실제 셸 내부에서 `ddg-agents-host` CLI 왕복.
  5. `hub_relay_cross_host.rs`: 2개 로컬 `ddg-agents-host` 인스턴스(Hub + Remote, Remote 설정 0개) 간 양방향 `relay_attach` 메시징·`read_screen`·`write_input`·`--host all`.
  6. `security_negative_boundaries.rs`: 토큰 오용, `RequirePinnedHostId` 차단, `plugin_host` `Hidden` IDOR 차단, 감사 로그 `0o600`·토큰 부재.
