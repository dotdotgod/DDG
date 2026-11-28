# DDG Agents — 개발·빌드·테스트·Docker 가이드

macOS arm64의 GPUI 개발 실행과 멀티 호스트(`ddg-terminal-server` · `ddg-agents-host` · `docker/`) 빌드·검증 안내다. GPUI가 유일한 데스크톱 앱 실행 경로다.

## macOS 릴리스 설치

Apple Silicon용 `darwin-arm64.app.zip` 또는 Intel용 `darwin-x86_64.app.zip`으로 끝나는 릴리스를 풀고, 쓰기 가능한 Applications 폴더로 `DDG Agents.app`을 옮긴다. 선언된 최소 macOS는 11.0이지만 Ghostty VT의 Intel 슬라이스는 13.0을 선언하므로 Intel 실기 검증 전에는 지원 하한을 보장하지 않는다.

Apple Silicon에서 Intel Mac으로 `ddg machine add --install`, `ddg machine install` 또는 GUI 머신 설치를 실행하면 같은 버전의 릴리스에서 두 데몬만 SSH로 전송한다. 전송 전에 체크섬·앱 서명·Team ID·Gatekeeper·아키텍처를 검사한다. 오프라인에서도 검증을 통과하는 같은 버전 캐시가 필요하며, 미발행 버전이나 서명 없는 개발 바이너리로 우회할 수 없다.

## 검증 환경

- macOS 26.6.2 arm64, Rust 1.100.0, Xcode 27.0 및 Metal Toolchain, GPUI 0.2.2 (Apache-2.0), Ghostty VT 1.0.1 (MIT)
- Linux 컨테이너 검증: Docker 29.9.3 (linux/arm64), Zig 0.17.1 정적 빌드 `libghostty-vt 1.0.1`, Debian Bookworm slim + `tini` + 비루트 `openssh-server`
- `xcrun -sdk macosx metal --version`이 실패하면 `xcodebuild -downloadComponent MetalToolchain`으로 설치한다.

## Ghostty VT 1.0.1 준비 및 로컬 실행

[공식 설치 안내](https://lib.ghostty.org/vt/installing.html)의 `ghostty-vt-1.0.1.xcframework.zip`을 다운로드한다(공식 SHA256: `c0f9631277f726154c8f521954d17cdc41aac4626fc0df8cf925a19ee58e829b`). 압축을 푼 뒤 프로젝트 루트에서 실행한다.

```sh
export GHOSTTY_VT_LIB_DIR=/absolute/path/to/ghostty-vt.xcframework/macos-arm64_x86_64
cargo build -p ddg-terminal-server -p ddg-agents-host
cargo run -p ddg-agents-native
```

`ddg-agents-native`를 실행하면 기본 프로파일(`~/.config/ddg/hosts.json`, 미설정 시 `로컬`)의 `ddg-agents-host`에 자동 연결하며, 로컬 호스트가 꺼져 있으면 `ddg-agents-host serve` 및 `ddg-terminal-server core`를 분리 세션(`setsid`)으로 기동한다.

## UI 동작 및 단축키

- **설정 자동 반영**: 실행 중에 `hosts.json`이나 `ui.json`을 수정하면 재시작 없이 반영된다. 머신 이름이나 순서만 바꾸면 기존 연결과 열린 룸·탭을 유지한다. 잘못된 파일은 마지막 정상 설정을 유지하고, 파일을 삭제하면 해당 설정만 기본값으로 돌아간다.
- **룸·멀티 머신 사이드바 및 머신 추가/편집**: `~/.config/ddg/hosts.json`에 머신이 1개(기본 로컬)일 때는 사이드바에 `룸` 목록과 `+` 버튼만 깔끔하게 표시한다. 사이드바 최하단 `머신 추가`에서 `SSH`(대상 또는 `ssh -i ... -p ... user@host` 접속 명령어 붙여넣기 + `~/.ssh/config` 빠른 선택), `Docker`(실행 중 컨테이너 자동 감지 + `docker exec` 명령어 붙여넣기), `컨테이너 SSH`를 추가할 수 있으며 `[연결 테스트]`로 사전 검증(실패 시 원인별 안내·확인 명령 복사)할 수 있다. 머신이 2개 이상 등록되면 `룸` 아래에 머신별 접기/펼치기 섹션이 나타나며, 각 섹션에서 머신별 `+`(룸 생성), 호버 `✎`(설정·제거), 접속 상태 원형 점(녹색 연결됨 / 노랑 연결·재연결 중 / 빨강 인증·호스트 키 오류 / 회색 연결 끊김)을 제공한다. 머신 제거는 목록에서만 분리(detach)하며 원격 세션을 종료하지 않는다. 룸 호버 연필 아이콘(`✎`)으로 이름을 변경한다(빈 이름은 첫 탭 경로의 폴더 이름을 사용).
- **화면 분할**: 아래 표의 키보드 단축키로 현재 패널을 좌우 또는 상하로 분할해 새 세션을 열거나, 패널을 확대·축소하거나, 선택한 패널을 독립 탭으로 분리한다. 패널 확대·축소는 현재 창에만 적용된다. 분할된 탭에는 패널 수가 표시된다. 구분선을 드래그하면 즉시 크기가 바뀌고, 놓으면 호스트에 저장된다.
- **파일 드래그 앤 드롭 첨부**: Finder 등에서 파일이나 폴더를 패널에 놓으면 해당 패널에 포커스가 이동하고, 제어권이 있는 경우 셸 특수문자를 이스케이프한 경로가 그 패널에 입력된다.
- **단일 제어권·세션 수명**: 다른 클라이언트가 제어 중인 패널에는 `읽기 전용` 배너와 `[제어 요청]` 버튼이 표시된다. 창 닫기/`⌘+Q`/네트워크 단절은 **detach**만 수행해 호스트의 PTY가 계속 실행된다. 탭 `×`/`⌘+W`는 현재 포커스된 패널만 종료하고, 마지막 패널이 닫히면 탭을 제거한다.
- **재부팅 후 복원**: 호스트 재부팅 또는 컨테이너 재시작 뒤 기존 탭은 `재부팅 후 중단됨 (자동 재실행 안 함)` 상태로 복원되며, `[새 세션 시작]` 버튼으로 새 PTY를 시작한다.

| 동작 | macOS | Windows/Linux (매핑 검증) |
|---|---|---|
| 새 룸 | `⌘+N` | `Ctrl+N` |
| 새 탭 / 현재 탭 닫기 | `⌘+T` / `⌘+W` | `Ctrl+T` / `Ctrl+W` |
| 이전 / 다음 탭 (순환) | `⌘+Shift+[` / `⌘+Shift+]` | `Ctrl+Shift+[` / `Ctrl+Shift+]` |
| 이전 / 다음 룸 (순환) | `⌘+Option+[` / `⌘+Option+]` | `Ctrl+Alt+[` / `Ctrl+Alt+]` |
| 1–9번째 탭 선택 | `⌘+1`–`9` | `Ctrl+1`–`9` |
| 전체 UI 확대 / 축소 / 100%로 재설정 | `⌘++` / `⌘+-` / `⌘+0` | `Ctrl++` / `Ctrl+-` / `Ctrl+0` |
| 좌우 분할 | `⌘+D` | `Ctrl+Shift+D` |
| 상하 분할 | `⌘+Shift+D` | `Ctrl+Shift+E` |
| 분할 패널 간 이동 | `⌘+Option+방향키` | `Ctrl+Alt+방향키` |
| 패널 확대 / 축소 | `⌘+Shift+Enter` | `Ctrl+Shift+Enter` |
| 패널을 독립 탭으로 분리 | `⌘+Shift+U` | `Ctrl+Shift+U` |

`Ctrl+Shift+Tab` / `Ctrl+Tab`도 이전 / 다음 탭으로 순환한다.

룸 단축키는 연결되어 현재 카탈로그를 받은 모든 머신의 표시 룸을 사이드바 순서대로 순환한다. 전환하면 `Rooms` 섹션과 대상 머신을 자동으로 펼치고, 대상 룸의 탭 선택·패널 포커스·확대 상태를 유지한다. 세션을 다른 룸이나 머신으로 옮기지는 않는다.

활성 창의 일반 터미널에 포커스가 있을 때만 동작하며, 읽기 전용에서도 사용할 수 있다. AgentList, Peek, 모달·폼, 런처, 유효한 붙여넣기 확인, IME 조합 중에는 기존 키 처리를 따르고, 분할 경계를 드래그하는 동안에는 전환하지 않는다. OS나 키보드 배치에서 단축키를 사용할 수 없으면 사이드바에서 룸을 클릭한다.

전체 UI 확대/축소는 터미널 글꼴을 포함해 50%–200% 범위에서 10%p씩 조절한다. `+` 대신 `=`도 사용할 수 있으며, 탭 바 오른쪽의 배율 표시를 클릭하면 기본값인 100%로 돌아간다(UI 글꼴 15.4px, 터미널 13.2px). 기존 v2에 저장된 배율은 그대로 적용된다. 기존 v2의 다른 환경설정은 유지되며, v1은 읽지 않고 기본값을 사용한다. 배율은 `~/.config/ddg/ui.json`에 저장된다(`DDG_CONFIG_DIR` 또는 `XDG_CONFIG_HOME`을 설정한 경우 해당 설정 디렉터리 사용).

## 내장 에이전트 프리셋

- 로컬 호스트 연결 시 이 앱은 발견한 Pi, Antigravity, Codex, OpenCode, Claude Code를 `agents.json`에 자동 등록한다. 기존 프로필과 기본 에이전트는 그대로 둔다. 원격 머신에는 자동 등록하지 않는다.
- Agent Settings의 `Detected`에서 탐지할 머신을 선택하거나 새로고침한다. 이 선택은 Per-Machine Overrides의 편집 대상과 별개다. 발견한 항목은 모달 초안에 추가해 수정·취소할 수 있으며 `Save` 후에만 런처에 표시된다. 런처에는 저장된 프로필만 나온다. 구형 호스트 또는 판정 불가는 확정된 미발견과 구분한다.
- GUI가 `ui.json`에 이미 기록한 프리셋은 삭제 후 재시작해도 자동 복구하지 않으며, `Detected`나 CLI로 명시적으로 재등록할 수 있다. GUI를 처음 열기 전에 CLI에서 추가했다가 삭제했거나 `ui.json`의 이력을 지운 경우에는 다시 자동 등록될 수 있다. 반대로 자동 등록 도중 이력만 저장되었다면 명시적으로 추가해야 한다.

```sh
ddg agent presets --host local --json  # 탐지 결과와 등록 여부 조회
ddg agent presets --no-detect          # 호스트 연결 없이 내장 목록 조회
ddg agent add --preset pi              # 탐지 없이 단일 프로필 등록
ddg agent add --detected --host local  # 발견된 미등록 프로필만 일괄 등록
ddg agent ls                           # 저장된 프로필만 표시
ddg run pi                             # 저장된 Pi 프로필을 대화형으로 실행
```

`--detected`는 기존 프로필을 덮어쓰거나 기본 에이전트를 변경하지 않는다. 발견 항목이 없고 일부 또는 전부를 확인할 수 없으면 오류를 반환한다. `--preset`에서 `--default`를 명시하면 해당 프로필이 기본값이 된다. 탐지 대상 호스트는 검사 위치일 뿐, 등록된 프로필의 실행 머신을 고정하지 않는다.

탐지는 실제 실행을 담당하는 Holder의 `PATH`에서 절대 경로 항목만 검사한다. 로그인 셸 설정을 읽거나 후보 바이너리를 실행하지 않으며, 설치·로그인·정상 실행을 보장하지 않는다. Finder/Spotlight로 시작한 앱의 Holder가 터미널과 다른 `PATH`를 상속하면 터미널에서 실행되는 CLI도 발견되지 않을 수 있다. Windows에서는 직접 실행 가능한 `.exe`를 찾으며 `.cmd`/`.bat`/`.ps1` 래퍼만 있으면 `unknown`으로 표시될 수 있다.

## 테스트 및 스키마 확인

```sh
# 전체 워크스페이스 테스트 (proto · vt · terminal-server · host · plugin · transport · native)
cargo build -p ddg-terminal-server -p ddg-agents-host
cargo test --workspace

# libghostty-vt 초기화만 단독 확인
cargo run -p ddg-agents-native -- --check-vt

# OpenRPC 스키마 갱신 (프로토콜 변경 시)
DDG_UPDATE_SCHEMA=1 cargo test -p ddg-agents-proto
```

## Docker 런타임 빌드 및 실기 검증

```sh
# 3-stage 비루트 최소 이미지 빌드 (Zig 0.17.1 + libghostty-vt 1.0.1 + Rust release)
docker build -f docker/Dockerfile -t ddg-agents-host:latest .

# docker-compose 실행 (기본: docker exec -i 전용, SSHD 활성화 시 DDG_ENABLE_SSHD=1)
docker compose -f docker/docker-compose.yml up -d

# 컨테이너 E2E 통합 테스트 (docker exec -i + 컨테이너 SSHD + 호스트 키 차단 + docker restart 복원)
DDG_DOCKER_IMAGE=ddg-agents-host:latest \
  cargo test -p ddg-agents-transport --test docker_live -- --ignored --nocapture
```

## 남은 실기 확인 항목

- GUI 창에서의 텍스트 선택 렌더링, 한글 IME 조합, VoiceOver 접근성, release 빌드 프레임 시간 측정은 수동 실기 확인 대상으로 남아 있다.
