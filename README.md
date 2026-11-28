# DDG Agents

GPUI와 네이티브 `libghostty-vt 1.0.1`을 사용하는 멀티 호스트 데스크톱 터미널·에이전트 워크스페이스입니다. 각 호스트(로컬 Mac · 원격 서버 · Docker 컨테이너)에서 `ddg-terminal-server` + `ddg-agents-host` 한 쌍이 실행되어 PTY와 룸·탭 카탈로그를 지속 보관하며, 공식 GPUI 클라이언트(`native/`)나 OpenRPC 기반 커스텀 클라이언트가 4종 트랜스포트(로컬 소켓 · OpenSSH · `docker exec -i` · 컨테이너 SSHD)로 접속합니다.

## macOS 설치

Apple Silicon은 `ddg-agents-<버전>-darwin-arm64.app.zip`, Intel Mac은 `ddg-agents-<버전>-darwin-x86_64.app.zip`을 선택하세요. 압축을 풀고 `DDG Agents.app`을 쓰기 가능한 Applications 폴더(예: `~/Applications`)로 옮긴 뒤 실행하세요. Intel 최소 macOS는 아직 실기 검증 전입니다(앱 선언 11.0, Ghostty VT Intel 슬라이스 13.0).

Apple Silicon에서 빈 Intel Mac에 SSH 설치하면 CLI와 GUI 모두 클라이언트와 같은 릴리스를 받아 검증합니다. 오프라인에서는 같은 버전의 캐시를 다시 검증하며, 서명되지 않은 바이너리로 대체하지 않습니다.

## 워크스페이스 구성

- `crates/ddg-agents-proto`: 9B 바이너리 프레이밍, JSON-RPC 2.0, `pty.*`·`host.*`·`ext.*` 계약 및 OpenRPC 1.3 스키마 자동 생성([schema/ddg-terminal-server.openrpc.json](schema/ddg-terminal-server.openrpc.json), [schema/ddg-agents-host.openrpc.json](schema/ddg-agents-host.openrpc.json))
- `crates/ddg-agents-vt`: Ghostty VT 1.0.1 C ABI 래퍼(`paint()`, `continuation`, `snapshot`, `Relay` 쿼리 필터)
- `crates/ddg-terminal-server`: 단일 호스트 최소 스펙 터미널 서버(Holder + Core 3-Tier, Credit 흐름 제어, 단일 제어권)
- `crates/ddg-agents-host`: 단일 호스트 표준 진입점(룸·탭 카탈로그 `catalog.json`, 스페이스/실행기, zero-parse 스트림 패스스루, `ext.<name>.*` 백엔드 플러그인 호스트)
- `crates/ddg-agents-transport`: 클라이언트 측 4종 멀티 호스트 트랜스포트 크레이트
- `native/`: 공식 GPUI 클라이언트(`ddg-agents-native`)
- `docker/`: 3-stage 최소 비루트 Docker 런타임(`Dockerfile`, `entrypoint.sh`, `sshd_config`, `docker-compose.yml`, `examples/Dockerfile.pi`)

## 문서

- [설치·개발 실행·테스트·Docker](native/README.md)
- [작업 인수인계](docs/HANDOFF.md)
- [제품 요구사항](docs/spec/requirements.md)
- [호스트 세션 설계·실행 기록](docs/plan/host-sessions/README.md)
