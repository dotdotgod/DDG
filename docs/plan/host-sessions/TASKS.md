# 실행 태스크 (`ddg-terminal-server` · `ddg-agents-host` · Docker)

[범위](README.md) · [설계 계약](DESIGN.md)

| ID | 상태 | 유형 / 영역 | 선행 | 산출물과 통과 기준 |
|---|---|---|---|---|
| **D1** | 완료 | 구현 / 프로토콜·VT | 없음 | Cargo Workspace 구성(`crates/ddg-agents-proto`, `crates/ddg-agents-vt`, `native/`). 4종 9B 프레이밍, 최소 `pty.*` · `host.*` · `ext.*` 타입 및 OpenRPC 1.3 스키마 자동 생성(`schema/*.openrpc.json`). Ghostty 1.0.1 `paint()`(alt screen·scrollback·continuation)·쿼리 필터 유닛 TDD 통과. |
| **D2** | 완료 | 구현 / 터미널 서버 | D1 | `crates/ddg-terminal-server`(`holder`, `core`, `connect`, `health`). 실제 PTY 통합 테스트: Core `SIGKILL` 후 동일 PID 유지 + `tail` 복구, 256KiB/4MiB 느린 구독자 재동기화, 단일 제어권(`control_epoch`)·구 세대 입력 거부, `pty.kill` `reap` 확인 통과. |
| **D3** | 완료 | 구현 / 에이전트 호스트 | D2 | `crates/ddg-agents-host`(`serve`, `connect`, `health`). 로컬 `terminal.sock` 직결, 룸/탭 16개 한도, 스페이스(`cwd`)·실행기 실행, `tab_close` `reap` 확인 후 제거·빈 룸 정리, `RegistryWriter` 원자 저장 및 손상 파일 `.corrupt` 보존, 재부팅 후 비활성 복원(자동 재실행 금지), 바이너리 스트림 zero-parse 패스스루 통합 테스트 통과. |
| **D4** | 완료 | 구현 / 4종 트랜스포트 | D1·D3 | `crates/ddg-agents-transport`(클라이언트 측 공용 크레이트). 로컬 소켓, 원격 SSH(`StrictHostKeyChecking=yes` + `HostKeyMismatch` 즉시 차단 + `AuthRequired` 분리), `docker exec -i`(컨테이너명 인젝션 차단), 컨테이너 SSHD 연결·`host_id` 검증·단절 중 입력 비큐잉·제어권 전환 통지 테스트 통과. |
| **D5** | 완료 | 구현 / GPUI 클라이언트 | D3·D4 | `native/`(`ddg-agents-native`)를 `ddg-agents-transport` 기반 `ddg-agents-host` 클라이언트로 전환(`native/src/pty.rs` 제거, `native/src/host_bridge.rs` 추가). 호스트 전환 드롭다운, 연결/읽기전용/재연결/호스트 키 오류 배너, `[제어 요청]`, 재부팅 후 `[새 세션 시작]`, 창 닫기/`⌘+Q` detach, 탭 `×`/`⌘+W` 명시 종료. |
| **D6** | 완료 | 구현·검증 / Docker 실기 | D2–D5 | `docker/Dockerfile`(Zig 0.17.1 `libghostty-vt 1.0.1` + Rust release + 비루트 `ddg` 3-stage, 압축 80.2MB), `entrypoint.sh`, `sshd_config`, `docker-compose.yml`, `examples/Dockerfile.pi`. Linux arm64 컨테이너 실기 E2E(`crates/ddg-agents-transport/tests/docker_live.rs`) 통과: `docker exec -i` 다중 클라이언트 제어권 전환·detach/reattach 동일 PID 유지, 컨테이너 SSHD `first_paint` 재현 + `HostKeyMismatch`/`AuthRequired` 차단, `docker restart` 영속 호스트 키 + `StoppedAfterReboot` + `tab_restart`. |
| **D7** | 완료 | 구현·문서 / 플러그인 & 동기화 | D3·D6 | `crates/ddg-agents-host/src/plugin_host.rs`(`plugin.toml`/`plugin.json` 로더, `rpc.discover` 병합, `ext.<name>.*` 라우팅, 역호출 `capabilities` 권한 차단 `PermissionDenied`, 크래시 격리 `PluginUnavailable`) + `examples/demo_plugin.py` + `tests/plugin_integration.rs` 통과. 문서(`requirements.md`, `native/README.md`, `README.md`, `docs/HANDOFF.md`) 동기화 완료. |

격리 환경(`/tmp/ddg-*` 소켓·임시 Docker 컨테이너/볼륨·임시 SSH 키)에서만 검증했으며 활성 사용자 세션·실제 `~/.ssh` 설정·호스트 재부팅은 건드리지 않았다. 이 작업 디렉터리는 Git 저장소가 아니므로 별도 커밋/머지 단계는 수행하지 않았다.
