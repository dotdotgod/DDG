# DDG Agents 작업 인수인계

## 목표와 현재 기준

DDG Agents는 네이티브 터미널을 기반으로 Pi·Claude Code·Codex 같은 기존 실행기를 사용하는 봇·팀 오케스트레이터다. 현재는 macOS arm64 개발 실행이 우선이며 **Rust GPUI가 유일한 앱 경로**다. Tauri/WebView/Vite 복구는 범위 밖이다. 이 작업 디렉터리는 Git 저장소가 아니다.

## 구현 완료: D1–D7 (`ddg-terminal-server` · `ddg-agents-host` · `ddg-agents-transport` · `native/` · `docker/`)

[활성 계획](plan/host-sessions/README.md) · [설계 계약](plan/host-sessions/DESIGN.md) · [D1–D7 완료 기록](plan/host-sessions/TASKS.md)

- **`crates/ddg-agents-proto`**: 4종 9B 바이너리 프레임(`DATA`/`CREDIT`/`CLOSE`/`RESIZE`), JSON-RPC 2.0, `pty.*`·`host.*`·`ext.*` 계약 및 OpenRPC 1.3 스키마 자동 생성([schema/ddg-terminal-server.openrpc.json](../schema/ddg-terminal-server.openrpc.json), [schema/ddg-agents-host.openrpc.json](../schema/ddg-agents-host.openrpc.json)).
- **`crates/ddg-agents-vt`**: Ghostty VT 1.0.1 C ABI 래퍼(`Vt::paint()`, `continuation`, `snapshot`/`from_snapshot`, `write_until_ground`, `Relay` 쿼리 필터·mode-2048 차단).
- **`crates/ddg-terminal-server`**: 단일 호스트 최소 스펙 터미널 서버(`holder`, `core`, `connect`, `health`). Holder + Core 3-Tier(Core `SIGKILL` 시 동일 PID 유지 + tail 복구), 256 KiB 초기 Credit / 4 MiB repaint resync, 단일 제어권(`control_epoch`), `pty.*` 7개 RPC.
- **`crates/ddg-agents-host`**: 단일 호스트 표준 진입점(`serve`, `connect`, `health`). 로컬 `terminal.sock` 직결, `catalog.json` 원자 저장 및 손상 파일 `.corrupt.<ts>` 보존, 룸/탭 16개 한도·빈 룸 정리, 스페이스(`space_cwd`)·실행기(`DefaultShell`/`Command`) 실행, 재부팅 후 `stopped_after_reboot` 목록 복원(자동 재실행 금지), 클라이언트별 독립 `terminal.sock` 연결을 통한 zero-parse 바이너리 스트림 패스스루 및 `host.tab_control_changed` 중계, 샌드박스 백엔드 플러그인 호스트(`plugin_host.rs`, `ext.<name>.*`, `capabilities` 강제, 크래시 격리) + `examples/demo_plugin.py`.
- **`crates/ddg-agents-transport`**: 클라이언트 측 멀티 호스트 트랜스포트 라이브러리(로컬 Unix 소켓 자동 기동, 원격 OpenSSH `StrictHostKeyChecking=yes` + `HostKeyMismatch`/`AuthRequired` 분리, `docker exec -i` 인젝션 차단, 컨테이너 SSHD, `host_id` 핀닝, 단절 중 입력 비큐잉, 자동 `CREDIT` 반환).
- **`native/` (`ddg-agents-native`)**: 직접 PTY 소유(`native/src/pty.rs`)를 제거하고 `native/src/host_bridge.rs`로 `ddg-agents-transport`에 연결. 호스트 선택 드롭다운(`~/.config/ddg/hosts.json`), 연결/읽기 전용/호스트 키 불일치 차단 배너, `[제어 요청]`, 재부팅 후 `[새 세션 시작]`, 창 닫기/`⌘+Q` detach, 탭 `×`/`⌘+W` 명시 종료. 기존 단축키(`⌘/Ctrl+N/T/W`, `0`–`9`, `Shift+[`/`]`, `Ctrl(+Shift)+Tab`) 및 룸 이름 모달 유지.
- **`docker/`**: 3-stage 비루트 최소 베이스 이미지(`Dockerfile`, `entrypoint.sh`, `sshd_config`, `docker-compose.yml`, `examples/Dockerfile.pi`). Zig 0.17.1로 `libghostty-vt 1.0.1` 정적 빌드(SHA256 검증) + Rust release 바이너리 + `tini` + 비루트 `ddg` 사용자 + 선택형 강화 `sshd`(`DDG_ENABLE_SSHD=1`).

## 빌드와 검증

[개발 가이드](../native/README.md)의 Ghostty 환경을 설정하고 프로젝트 루트에서 실행한다. 이 머신에서 사용한 환경: `CARGO_HOME=/tmp/ddg-cargo`, `RUSTUP_HOME=/tmp/ddg-rustup`, `PATH=/tmp/ddg-cargo/bin:$PATH`, `GHOSTTY_VT_LIB_DIR=/tmp/ddg-vt.t10r26/ghostty-vt.xcframework/macos-arm64_x86_64`.

```sh
cargo build -p ddg-terminal-server -p ddg-agents-host
cargo test --workspace
cargo run -p ddg-agents-native -- --check-vt
```

- **검증 결과 (2026-11-25)**:
  - `cargo check --workspace`: 경고 0개
  - `cargo test --workspace`: 전체 32개 통과 (`ddg-agents-proto` 5, `ddg-agents-vt` 9, `ddg-terminal-server` 6 [실제 PTY + Core `SIGKILL` 복구 포함], `ddg-agents-host` 5 [다중 클라이언트 + 재부팅 복원 + 플러그인 샌드박스/크래시 격리 포함], `ddg-agents-transport` 5, `ddg-agents-native` 2)
  - `cargo run -p ddg-agents-native -- --check-vt`: `libghostty-vt initialized` 통과
  - Docker 실기 E2E(`crates/ddg-agents-transport/tests/docker_live.rs`): Linux arm64 이미지(`80.2MB` compressed) 빌드 후 `docker exec -i` 다중 클라이언트 제어권 전환·detach/reattach 동일 PID 유지, 컨테이너 SSHD `first_paint` 재현 + `HostKeyMismatch`/`AuthRequired` 차단, `docker restart` 영속 호스트 키 + `StoppedAfterReboot` + `tab_restart` 전 항목 통과
  - 잔여 백그라운드 프로세스·임시 컨테이너/볼륨 0개 확인
- **남은 수동 실기 확인 항목**: GUI 창에서의 한글 IME(이름 모달 포함), 텍스트 선택 렌더링, VoiceOver 접근성, release 프레임 시간 측정.

## 후속 방향

- **봇과 실행기 분리**: 현재 `ddg-agents-host`는 `ExecutorSpec::Command`로 임의 실행기(Pi 등)를 스페이스(`space_cwd`)에서 실행할 수 있으며, `ext.<name>.*` 플러그인 프로토콜로 봇/팀 오케스트레이션 확장을 얹을 준비가 되어 있다. 다음 단계에서는 Pi 실행기 어댑터, 봇 역할·스킬·입출력 계약, 순차·루프 팀 워크플로우를 `ddg-agents-host` 또는 전용 백엔드 플러그인으로 붙인다.
- **Herdr App 포크는 보류 상태**이며, 현재는 자체 GPUI 앱(`native/`)과 `ddg-terminal-server` + `ddg-agents-host` 스택이 기준 구현이다.
