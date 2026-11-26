# 실행 태스크 명세: Phase S0 – S2 (기반 · 코어 · 전송)

[계획 목차](README.md) · [태스크 S3–S5](TASKS_HOST_UI.md) · [설계(데이터)](DESIGN_DATA.md) · [테스트](TESTS.md)

> **전 구현 태스크 공통 실행 규칙**
> - **구현 역할 정의(필수 숙독)**: `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`
> - **구현 모델 및 실행 방식**: **로컬 인터랙티브 `pi --provider openai --model gpt-6.2-sol --thinking high`** (`hc run --bg --name ddg-<task> --cwd /Users/dotdot/Workspace/dotdotgod-agents`; 사용자 명시 승인으로 `smaxh-mac` 대신 로컬 실행 전환, Antigravity Neon이 매 태스크 독립 `diff`·테스트 검수 후 `hc kill && hc rm`).
> - **공통 빌드 env**: `GHOSTTY_VT_LIB_DIR=/tmp/ddg-vt.t10r26/ghostty-vt.xcframework/macos-arm64_x86_64` (확인 완료).
> - **비가역 실행 공통 금지**: 실운영 `~/.local/state/ddg`, `~/.config/ddg`, `~/.ssh` 수정 및 미승인 원격 배포/푸시 금지. (현재 워크스페이스는 Git 저장소가 아님.)

---

## 태스크 S0: 실행 환경 확정 및 `smaxh-mac` 정리
- **목표**: `smaxh-mac` 관련 세션/로그 정리 및 로컬 `pi` 실행 환경(`cargo 1.100.0`, `GHOSTTY_VT_LIB_DIR`, `docker 29.9.3`, `pi auth check`=`ready`) 확정.
- **유형 / 상태**: 조사 / **완료** (`2026-11-25`, 사용자 승인으로 로컬 `pi` 전환 확정).
- **수행 역할**: Lead (Antigravity / Gemini 4 Neon) — 코드 쓰기 없음.
- **확정 내용**: `smaxh-mac/t28w` 및 스냅샷 제거 완료(`smaxh-mac` 내 `dotdotgod-agents` 흔적 0건), 로컬 `pi`·`cargo`·`libghostty-vt`·`docker` 정상 확인.
- **산출물 / 완료 기준**: `smaxh-mac` 정리 검증 + 로컬 `pi` 실행 전환 확정.

---

## 태스크 S1: `ddg-agents-proto` v2 계약 및 OpenRPC
- **목표**: 전역 고유 ID·보안 헬퍼·Catalog v2·16종 오류 코드·세션/릴레이 RPC 타입을 `crates/ddg-agents-proto`에 구현하고 OpenRPC 1.3 스키마를 갱신한다.
- **유형 / 상태**: 구현 / S0 완료 후 착수 가능.
- **수행 역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`), `pi --provider openai --model gpt-6.2-sol --thinking high`.
- **선행 태스크**: S0 (계약 루트 크레이트이므로 S2/S3/S4 모두 S1 완료를 대기 — 공유 스키마 충돌 방지).
- **참조 설계**: [DESIGN_DATA.md §1, §2, §4](DESIGN_DATA.md), [TESTS.md §2-1](TESTS.md).
- **쓰기 허용 / 금지**: 허용=`crates/ddg-agents-proto/**`, `schema/ddg-terminal-server.openrpc.json`, `schema/ddg-agents-host.openrpc.json` / 금지=타 크레이트 소스(`Cargo.toml` 루트 미수정).
- **구현 착수 근거 및 변경 내용**:
  - `crates/ddg-agents-proto/Cargo.toml`: `libc.workspace = true` 추가.
  - `src/id.rs` 신규: `rm_`/`rm_bg_`/`ses_`/`msg_` 128-bit Crockford Base32, `libc::getentropy`/`getrandom` fail-closed, `decode_base64_bounded`, `sanitize_display_text`(전체 Bidi 포함).
  - `src/catalog.rs`, `src/rpc.rs`, `src/error.rs`, `src/openrpc.rs`: `CATALOG_SCHEMA_VERSION = 2`, `RoomKind`, `RoomVisibility`, `TabRecord.run_generation`/`parent_session_id`(`TabCreateParams` 제외), `CatalogSubscribeResult.reset_notice`(`HostHelloResult` 제외), `TabAttachResult.run_generation`, `ExternalInput`/`SessionExited`, 16종 `ErrorData.code`, `host.relay_attach`(`host.relay_exec` 미정의).
  - `DDG_UPDATE_SCHEMA=1 cargo test -p ddg-agents-proto`로 OpenRPC JSON 갱신.
- **테스트 (필수, 동일 태스크 구현)**: [TESTS.md §2-1](TESTS.md) 4개 그룹(CSPRNG fail-closed, base64/Bidi 정규 매트릭스, Catalog v2 불변식, OpenRPC+음성 계약 테스트).
  - 실행: `cargo test -p ddg-agents-proto` — 경고 0, 신규+기존 전 테스트 통과.
- **비가역 실행**: 없음.
- **병렬 주의**: 단독 선행 단계(하위 크레이트 S2-A/S2-B는 S1 완료 전 착수 금지).
- **독립·통합 완료 기준**: `cargo test -p ddg-agents-proto` 통과 + OpenRPC diff 일치 + Antigravity 독립 리뷰 통과.

---

## 태스크 S2-A: `ddg-terminal-server` 세션 ID 및 `mailbox.rs` *(S2-B와 병렬 가능)*
- **목표**: Holder `session_id`/`run_generation` 영속, `mailbox.rs` RAM FIFO, `session.*` 5개 RPC(`send/recv/inbox_subscribe/read_screen/write_input`) 구현.
- **유형 / 상태**: 구현 / S1 완료 후 착수 가능.
- **수행 역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`), `pi --provider openai --model gpt-6.2-sol --thinking high`.
- **선행 태스크**: S1 (`ddg-agents-proto` v2 타입 필요).
- **참조 설계**: [DESIGN_DATA.md §2-1](DESIGN_DATA.md), [TESTS.md §2-2](TESTS.md).
- **쓰기 허용 / 금지**: 허용=`crates/ddg-terminal-server/**` / 금지=`crates/ddg-agents-proto/**`, `crates/ddg-agents-transport/**`, `crates/ddg-agents-host/**`, `native/**`.
- **구현 착수 근거 및 변경 내용**:
  - `holder.rs`: `HolderSpawn`/`HeldPty`/`HeldTerm`에 `session_id`, `run_generation`(`#[serde(default)]`) 보존; `held.exit.is_some()` 즉시 `RELEASE`(`pending_recovery_exits` 미추가).
  - `mailbox.rs` 신규: `64 KiB`/msg, `256개`/`1 MiB`/세션, `32 MiB` 전역 상한, `session_exited` 축출 보호, 소비/퍼지 시 전역 바이트 정확 차감, PTY 종료/`run_generation` 증가/`pty.kill` 자동 퍼지(`session.purge_mailbox` RPC 미노출).
  - `server.rs`/`pty.rs`: `pty.*`에 `session_id`/`run_generation` 연동 + `session.send`/`recv`/`inbox_subscribe`(9B Credit)/`read_screen`(`lines: 1..=2000`, `max_bytes: 1..=262144`, `0` 거부, UTF-8 경계 절단)/`write_input`(`1..=65536`B, `expect_run_generation`, `ControlMode::Control` 미탈취, 메타데이터 전용 `PtyEvent::ExternalInput`).
- **테스트 (필수, 동일 태스크 구현)**: [TESTS.md §2-2](TESTS.md) 3개 그룹 + 기존 `ddg-terminal-server` 테스트 갱신.
  - 실행: `cargo test -p ddg-terminal-server` — 경고 0, 전 테스트 통과.
- **비가역 실행**: 없음.
- **병렬 주의 (S2-B와 공유 파일 없음)**: 크레이트 경계가 완전 분리되어 S2-B와 안전 병렬 가능(공유 `Cargo.lock` 변경 없음).
- **독립·통합 완료 기준**: `cargo test -p ddg-terminal-server` 통과 + Antigravity 독립 리뷰 통과.

---

## 태스크 S2-B: `ddg-agents-transport` v2 클라이언트 및 핀닝 정책 *(S2-A와 병렬 가능)*
- **목표**: `HostProfile.id` 검증, `DialPolicy::RequirePinnedHostId`, `LC_ALL=C` SSH 오류 정제, `client.rs` v2 문자열 ID·`run_generation`·`relay_attach` 지원.
- **유형 / 상태**: 구현 / S1 완료 후 착수 가능.
- **수행 역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`), `pi --provider openai --model gpt-6.2-sol --thinking high`.
- **선행 태스크**: S1.
- **참조 설계**: [DESIGN_DATA.md §3](DESIGN_DATA.md), [TESTS.md §2-3](TESTS.md).
- **쓰기 허용 / 금지**: 허용=`crates/ddg-agents-transport/**` / 금지=타 크레이트 및 `native/**`.
- **구현 착수 근거 및 변경 내용**:
  - `profile.rs`: `HostsConfig::validate()`에서 대소문자/공백 정규화 후 `id == "all"` 및 중복 `id` 거부.
  - `dial.rs`: `DialPolicy::RequirePinnedHostId`(`None`/빈/공백 `expected_host_id` → `host_security_blocked`), `build_ssh_command`에 `.env("LC_ALL", "C").env("LANG", "C")`, `sanitize_dial_error()`.
  - `client.rs`: v2 `String` ID, `TabStreamHandle.run_generation`, `reset_notice`, `HostEvent::ExternalInput`/`SessionExited`, `relay_attach` 양방향 채널 클라이언트.
- **테스트 (필수, 동일 태스크 구현)**: [TESTS.md §2-3](TESTS.md) 3개 그룹 + 기존 `ddg-agents-transport` 테스트 갱신.
  - 실행: `cargo test -p ddg-agents-transport` — 경고 0, 전 테스트 통과.
- **비가역 실행**: 없음.
- **병렬 주의 (S2-A와 공유 파일 없음)**: S2-A와 완전 병렬 실행 가능.
- **독립·통합 완료 기준**: `cargo test -p ddg-agents-transport` 통과 + Antigravity 독립 리뷰 통과.
