# Data · Network Layer 설계 및 보안 정책

[계획 목차](README.md) · [UI·UX 설계](DESIGN_UI.md) · [테스트 설계](TESTS.md)

## 1. 전역 고유 ID · 공통 유틸 (`crates/ddg-agents-proto/src/id.rs`)
- **ID 형식**: 128-bit 시간 정렬 Crockford Base32 (`48-bit ms + 80-bit OS CSPRNG`, 대소문자 비허용, 소문자 정규화).
  - 일반 룸 `rm_<26>`, 호스트 숨김 배경 룸 `rm_bg_<host_prefix>`(호스트당 최대 1개), 세션 `ses_<26>`, 메시지 `msg_<26>`.
- **CSPRNG Fail-Closed**: `libc::getentropy`(macOS/BSD) / `libc::getrandom`(Linux) 직접 호출(`ddg-agents-proto/Cargo.toml`에 `libc.workspace = true` 추가). 실패 시 **타임스탬프·PID 폴백 금지**, 오류 반환.
- **공통 보안 헬퍼**:
  - `decode_base64_bounded(s, max_bytes)`: 디코드 전 문자열 길이 `<= ((max_bytes + 2) / 3) * 4` 사전 검사(64 KiB 기준 `<= 87_384` bytes), 엄격 RFC4648 패딩 검증.
  - `sanitize_display_text(s, max_chars)`: C0/C1, CR/LF, 전체 Bidi 제어문자(`U+061C`, `U+200E`, `U+200F`, `U+202A..=U+202E`, `U+2066..=U+2069`) 제거.

## 2. 2계층 소유권 및 핵심 데이터 모델
### 2-1. `ddg-terminal-server` (저수준 세션 실행 · `mailbox.rs` · `holder.rs`)
- **Holder 영속 (`holder.rs`)**: `HolderSpawn`·`HeldPty`·`HeldTerm`에 `session_id: String`, `run_generation: u64`(`#[serde(default)]`, `HOLDER_PROTO_VERSION = 1`) 보존. Core `SIGKILL` 복구 시 `held.exit.is_some()`이면 즉시 `RELEASE`(별도 `pending_recovery_exits` 제거; `HostService`가 `pty.list`로 조정).
- **`mailbox.rs` (RAM FIFO)**: 메시지당 `64 KiB`, 세션당 `256개 / 1 MiB`, 서버 전역 `32 MiB` 상한. 일반 메시지 가득 시 최오래된 일반 메시지 축출, 시스템 `session_exited` 알림은 축출 보호. 소비/퍼지 시 전역 바이트 정확 차감. PTY 종료·`run_generation` 증가·`pty.kill` 시 내부 자동 퍼지(외부 `session.purge_mailbox` RPC 미노출).
- **신규/확장 RPC**: `pty.*`에 `session_id`/`run_generation` 추가; `session.send`, `session.recv`(`wait_ms`, 빈 큐 `[]`), `session.inbox_subscribe`(9B Credit 스트림), `session.read_screen`(`lines: 1..=2000`, `max_bytes: 1..=262144`, `0`은 `invalid_params`, UTF-8 문자 경계 절단), `session.write_input`(`1..=65536`B, `expect_run_generation`, `ControlMode::Control` 미탈취, 메타데이터 전용 `PtyEvent::ExternalInput` 발행).

### 2-2. `ddg-agents-host` (룸 · Catalog v2 · `token_store.rs` · `hub.rs` · `cli.rs`)
- **Catalog v2 (`CATALOG_SCHEMA_VERSION = 2`)**: `RoomKind { Standard, Background }`, `RoomVisibility { Visible, Hidden }`. `Standard` 룸 탭 `1..=16`(`Visible`), `Background` 룸 최대 1개·탭 `0..=64`(`Hidden`, 빈 상태 허용). `TabRecord`에 `run_generation: u64`(>=1), `parent_session_id: Option<String>` 추가(`TabCreateParams`에는 **미포함**, 서버가 토큰에서 단독 파생).
- **v1 리셋 (`catalog_store.rs`, `Q3`)**: v1(`u64`) 감지 시 충돌 방지 파일명 `catalog.json.v1.bak.<ts>`(`0o600`)로 백업 후 빈 v2 초기화. `CatalogSubscribeResult.reset_notice`에 basename만 노출(첫 가시 `Standard` 룸 생성까지 유지). `HostService::start()`에서 `session_id` 없는 생존 v1 고아 PTY는 `pty.kill`로 정리. 재부팅 복원 시 미실행 `Background` 탭 자동 삭제.
- **메모리 전용 토큰 (`token_store.rs`)**: 형식 `<ses_id>.<gen>.<64-hex>`, OS CSPRNG fail-closed(기존 `paths::random_hex()` 사용 금지). 상수시간 비교(`ct_eq`), Drop 시 `write_volatile`+`compiler_fence(SeqCst)` zeroize. 구 세대=`token_revoked`, 위조=`token_invalid`. `Catalog`·`ExecutorSpec.env`·로그·브로드캐스트·CLI `--token` 인자에 절대 저장/출력 금지.
- **동시성·보상 (`host_service.rs`)**: 멱등 키 `InFlight` 시 즉시 `Conflict` 반환(`Arc<Notify>` 대기 금지). 룸별 `in_flight_creates`와 세션별 `in_flight_restarts`/`in_flight_closes`로 `16`/`64` 한도 레이스 방지; 스폰·커밋 실패 시 `pty.kill` 보상 및 잠금 해제.
- **Q2 숨김 세션 종료 순서**: `host.session_exited` 브로드캐스트 + 부모 세션(`parent_session_id`) 인박스 알림 → 카탈로그 즉시 삭제 → 토큰 폐기 & 메일박스 퍼지.
- **`TermConn` 재연결**: `boot_generation` 기반 자동 재연결·재조정. 감사 로그 `host.log`는 `OpenOptions::mode(0o600)`+`set_permissions(0o600)`로 메타데이터만 기록(페이로드·토큰 금지, 개행/Bidi 정제).
- **`plugin_host.rs` 격리**: 기존 로컬 8개 메서드만 문자열 ID로 이전(`session_*`/`msg_*`/`relay_*`/원격 차단). `host.catalog_get`에서 `Hidden` 룸 제외, `pty.list`/`pty.kill`에서 `Hidden`/`Background` 소속 PTY 필터링·거부, `room_rename`/`tab_create`의 `Background` 변조 및 `Hidden` IDOR 차단.

## 3. 크로스 호스트 Hub Relay (`hub.rs`, `Q4+Q5`) & CLI (`cli.rs`)
- **Opt-in Hub (`DDG_HUB_ENABLED=1`, 기본 OFF)**: 원격 `connect` 서브커맨드에서는 절대 기동하지 않음. `ddg-agents-host/Cargo.toml`에 `ddg-agents-transport` 추가(비순환). `Local { sock_path: None }` 및 자기 `host_id` 연결 스킵.
- **핀닝·단일 양방향 채널**: `DialPolicy::RequirePinnedHostId`(`None`/공백 `expected_host_id` → `host_security_blocked`), SSH에 `LC_ALL=C`/`LANG=C` 강제 및 오류 정제(`sanitize_dial_error`). 원격 호스트는 타 호스트 설정/키를 전혀 보유하지 않으며 단일 활성 `host.relay_attach` 채널로만 업스트림 통신(독립 `host.relay_exec` RPC 미존재).
- **보안 불변식**: `relay_attach`는 토큰 세션 및 활성 Hub 인스턴스에서 거부. `Close(RelayReplaced)` 수신 시 무음 재연결 루프 금지(경고/차단 표시). 릴레이 `hop == 1` 고정, 9개 허용 메서드(`msg_send`, `session_create/list/move/close/restart/screen/write`, `host_list`)만 허용, Hub가 인증된 연결에서 `from_host_id` 강제 덮어쓰기. `host.list_hosts`는 `{ host_id, label, status }`만 노출(키/경로/전송 설정 0개).
- **브리지 CLI (`cli.rs`)**: `host list`, `session create|list|move|close|restart|screen|write` (`--enter`, `--stdin`, `--expect-gen`), `msg send|recv|subscribe`. `--token` 인자 거부(환경변수 `DDG_SESSION_TOKEN`만 허용), 쓰기/변조 커맨드에서 `--host all` 거부(`session list --host all`만 읽기 전용 부분 성공 허용).
- **프로필 검증 (`ddg-agents-transport/src/profile.rs`)**: `HostsConfig::validate()`에서 대소문자/공백 정규화 후 `id == "all"` 및 중복 `id` 거부.

## 4. 표준 오류 코드 (16종, `ErrorData.code`)
`room_tab_limit`, `room_not_found`, `session_not_found`, `session_not_running`, `token_revoked`, `token_invalid`, `generation_mismatch`, `host_not_found`, `host_offline`, `relay_timeout`, `host_security_blocked`, `host_auth_required`, `hub_unavailable`, `invalid_params`, `missing_env`, `permission_denied`.

## 5. 신뢰 경계 · 알려진 한계 · 재검토 조건
- **신뢰 경계**: (1) `DDG_TERMINAL_SOCK`은 동일 OS UID 신뢰 경계(같은 UID 프로세스의 직접 소켓 접근은 OS 권한 모델 범위), (2) `host` 세션 RPC는 `session_token`으로 호출자 신원 확정, (3) 원격 경계는 OpenSSH `StrictHostKeyChecking=yes` + `expected_host_id` 핀닝.
- **알려진 한계 및 재검토 조건**:
  1. 토큰이 RAM 전용이므로 `ddg-agents-host` 재기동 시 기존 실행 중 세션의 토큰이 무효화됨 → 재기동 빈도가 높아 무중단 에이전트 연속성이 필요해지면 프로세스 메모리 봉인/재발급 프로토콜 재검토.
  2. 메일박스가 RAM 전용(전역 32 MiB) → 대용량 아티팩트 교환 요구 발생 시 파일 경로 참조 프로토콜 확장 재검토.
  3. Hub 단일 장애점(SPOF) → 멀티 허브/고정 라우팅 요구 시 `hop > 1` 루프 방지와 함께 재검토.
