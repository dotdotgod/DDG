# `ddg-terminal-server` · `ddg-agents-host` · 클라이언트 계약

~~~text
[호스트: 작업 Mac / Docker ▾]   [연결됨 / 읽기 전용 / 재연결 중 / 호스트 키 불일치]
 룸 A                           [1] [2 ×] [+]
 룸 B                           [터미널 화면 / 제어 요청]
                                [재부팅 후: 실행 중 아님 · 새 세션 시작]
~~~

## 1. 3계층 책임 분리 (단일 호스트 데몬 + 멀티 호스트 클라이언트)

- **`ddg-terminal-server` (단일 호스트 · 연결 · PTY/VT 최소 스펙 서버)**:
  - 내부 3-Tier 구조: `Holder`(OS PTY, 자식 프로세스, 1 MiB 원시 `tail`, `reap`, Core 자동 재기동) + `Core`(로컬 `terminal.sock` + `connect`(`stdio`), Ghostty VT 1.0.1 `first_paint()` 스냅샷, PTY당 명시적 단일 제어권, Credit 흐름 제어).
  - 최소 RPC: `hello`/`ping` + `pty.spawn`, `pty.list`, `pty.subscribe`, `pty.attach`, `pty.claim_control`, `pty.release_control`, `pty.kill`. `pty.spawn`은 상위가 넘긴 `cwd`/`argv`를 해석 없이 OS에 전달만 하며, 스페이스·실행기·룸·탭·플러그인·원격 네트워크를 전혀 모른다.
- **`ddg-agents-host` (단일 호스트 · 표준 접속 진입점 · 플러그인 허브)**:
  - 자기 호스트의 `terminal.sock`에만 직결한다(원격 SSH/Docker 코드 없음). 어디서 접속하든 같은 호스트의 룸·탭 카탈로그, 스페이스, 실행기, 플러그인이 동일하게 노출된다.
  - 룸·탭 카탈로그(`catalog.json`), 스페이스(로컬 `cwd` 검증), 실행기(Pi/Claude Code/Codex) 직접 실행, 재부팅/컨테이너 재시작 목록 복원(자동 재실행 금지)을 소유한다. `schema_version` 있는 로컬 파일에 전용 스레드(`RegistryWriter`)로 원자 저장하며, 손상 파일은 덮어쓰지 않고 `.corrupt.<ts>`로 보존한다(SQLite 제외).
  - 터미널 스트림 9B 프레임(`DATA`/`CREDIT`/`RESIZE`)은 하위 `terminal.sock` 채널로 zero-parse 패스스루한다.
  - **커스텀 백엔드 플러그인 스펙**: 외부 프로세스가 `stdio`로 동일한 프로토콜을 사용해 `ext.<plugin>.*` 네임스페이스를 등록한다. 매니페스트의 최소 `capabilities`(`host.read/write`, `pty.observe/control`) 밖의 호출을 차단하고, 플러그인 크래시/타임아웃을 격리한다.
- **`ddg-agents-transport` + `ddg-agents-client` (멀티 호스트 접속 + 공식 GPUI / 커스텀 클라이언트)**:
  - `ddg-agents-transport`가 호스트 프로필(`hosts.json`)과 4종 접속 경로(호스트 키 변경 차단, 재연결 백오프, 단절 중 입력 비큐잉)를 공통 처리한다.
  - 클라이언트는 화면 표시·선택·입력 전달만 소유하며, `schema/ddg-terminal-server.openrpc.json`과 `schema/ddg-agents-host.openrpc.json`을 자동 생성해 누구나 언어 무관 커스텀 클라이언트를 만들 수 있게 한다.

## 2. 프로토콜 · 제어권 · 흐름 제어

- **와이어 프로토콜**: Plain NDJSON JSON-RPC 2.0 → `hello {protocol_version: 1, framed: true}` 응답 직후 9바이트 바이너리 프레이밍(`[channel u32 LE][kind u8][len u32 LE][payload]`, 최대 16 MiB)으로 전환. 프레임은 **4종만** 사용: `0 DATA`, `1 CREDIT`, `2 CLOSE`, `3 RESIZE`.
- **명시적 단일 제어권**: PTY당 한 연결(`(conn_id, control_epoch)`)만 입력(`DATA`)과 `RESIZE`를 수행한다. 비제어·구 세대 입력은 서버 수신 경계에서 거부한다. 읽기 전용 연결은 출력과 `RESIZE` 알림을 수신하며 `claim_control`으로 제어를 요청할 수 있다. 제어 연결 단절/detach 시 즉시 해제된다.
- **흐름 제어 & 스냅샷 연속성**: 초기 크레딧 `256 KiB`, 청크 `16 KiB`, 미전송 상한 `4 MiB`. 느린 구독자가 상한을 넘으면 백로그를 폐기하고 최신 `first_paint()`(제한된 `4 MiB` RAM 스크롤백 + `continuation`)로 재동기화하며 누락을 알린다. Core VT가 응답한 쿼리는 뷰어 스트림에서 제거하고, `OSC 52` 등 호스트 부작용은 기본 차단한다.
- **Detach vs 명시 종료**: 창 닫기/`⌘+Q`/연결 단절은 detach. `×`/`⌘+W`는 `host.tab_close {tab_id, request_id}`로 종료를 요청하고 `ddg-terminal-server`의 `pty.kill` + `reap` 확인 후에만 탭과 마지막 빈 룸을 제거한다. 응답 유실은 재조회와 `request_id` 멱등성으로 해결하며 실패 시 탭을 유지한다. 재접속 중 입력은 보내거나 큐에 쌓지 않는다.

## 3. 4종 접속 경로 및 최소 베이스 Docker

모든 클라이언트는 아래 4종 경로로 대상 호스트의 **`ddg-agents-host`**에 접속한다.

1. **로컬 Unix Socket**: `$RUN_DIR/host.sock` (디렉터리 `0700`, 소켓 `0600`, 소유자 계정 전용).
2. **원격 SSH**: `ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes ... "exec ddg-agents-host connect"`. 호스트 키 불일치는 일반 단절과 분리해 `host_key_mismatch`로 즉시 차단. 인증 실패(`auth_required`)는 자동 재시도 중단.
3. **Docker Exec**: `docker exec -i <ctr> ddg-agents-host connect` (원격 Docker는 SSH 뒤에서 동일 실행). `-t` 금지, 컨테이너명 화이트리스트 검증.
4. **컨테이너 SSHD**: `ssh -T -p <port> ... ddg@<host> "exec ddg-agents-host connect"`. 영속 볼륨(`/etc/ddg/ssh`)에 호스트 키와 읽기 전용 `authorized_keys`를 보관하고, `PermitRootLogin no` / `PasswordAuthentication no` / `AllowTcpForwarding no`로 하드닝한다.

- **Docker 베이스 이미지**: Multi-stage(Zig 0.17 `libghostty-vt 1.0.1` 정적 빌드 → Rust `ddg-terminal-server` + `ddg-agents-host` 빌드 → 최소 `debian:bookworm-slim` + `tini` + `zsh`/`bash`/`git` + 선택적 `openssh-server`). 에이전트 실행기(Pi/Claude Code/Codex)는 확장 `Dockerfile`로 분리한다.
