# 호스트 세션 · 원격/Docker DDG

[설계](DESIGN.md) · [태스크](TASKS.md) · [초기 플래닝 보고](../../archive/report/2026-11-22/host-sessions/PLANNING.md)

**구현 완료 (D1–D7, 2026-11-25)**: 단일 호스트 최소 스펙 `ddg-terminal-server` · 단일 호스트 표준 진입점 `ddg-agents-host` · 멀티 호스트 클라이언트 `ddg-agents-transport` / `ddg-agents-native` · 3-stage 최소 비루트 Docker 런타임(`docker/`) · `ext.<name>.*` 커스텀 백엔드 플러그인 호스트까지 구현 및 통합 검증을 완료했다.

- **각 호스트(로컬 Mac · 원격 서버 · Docker 컨테이너)마다 `ddg-terminal-server` + `ddg-agents-host` 한 쌍 실행**:
  1. `crates/ddg-terminal-server` (최소 스펙): `terminal.sock`/`stdio` + 4종 9B 프레임 + Credit(256 KiB / 4 MiB repaint resync) + Holder/Core 3-Tier + `pty.*` 7개 RPC + 단일 제어권(`control_epoch`).
  2. `crates/ddg-agents-host` (표준 접속 진입점): 로컬 `terminal.sock` 직결, `catalog.json` 원자 저장(`.corrupt` 보존), 스페이스(`cwd`)·실행기 실행, zero-parse 바이너리 스트림 패스스루, 재부팅 후 `stopped_after_reboot` 목록 복원, `ext.<name>.*` 백엔드 플러그인(`plugin_host.rs`).
- **클라이언트 측 (`crates/ddg-agents-transport` + `native/`)**:
  - 4종 접속 경로(로컬 Unix 소켓, 원격 OpenSSH + `StrictHostKeyChecking=yes` / `HostKeyMismatch` 차단 / `AuthRequired` 분리, `docker exec -i`, 컨테이너 영속 SSHD) 구현.
  - 자동 생성 OpenRPC 1.3 스키마([schema/ddg-terminal-server.openrpc.json](../../../schema/ddg-terminal-server.openrpc.json), [schema/ddg-agents-host.openrpc.json](../../../schema/ddg-agents-host.openrpc.json)) 제공.
  - 공식 GPUI 앱(`native/`)이 직접 PTY 소유에서 벗어나 `ddg-agents-host` 클라이언트로 전환됨.
- **Docker 패키징 (`docker/`)**: `Dockerfile`(Zig 0.17.1 `libghostty-vt 1.0.1` + Rust release + 비루트 `ddg` + `tini` + 선택형 강화 `sshd`), `entrypoint.sh`, `sshd_config`, `docker-compose.yml`, `examples/Dockerfile.pi` 구축 및 Linux arm64 실기 E2E 통과.
