# 실제 환경 사용자 시나리오 검증 명세 (`SCENARIOS.md`)

관련 문서: [계획 개요](README.md) · [UX·UI 설계](DESIGN_UI.md) · [데이터·네트워크·보안 설계](DESIGN_DATA.md) · [TDD 유닛 테스트](TESTS.md)

> **공통 격리 프리앰블 (`E-SEC-09` 필수 준수 — 실운영 `~/.config/ddg`, `~/.local/share/ddg/dist`, `~/.ssh/config`, `SSH_AUTH_SOCK`, 실운영 `host.sock` 완전 격리)**:
> ```sh
> WORK="$(mktemp -d /tmp/ddg-scn-XXXXXX)"
> mkdir -p -m 0700 "$WORK/home" "$WORK/cfg" "$WORK/state" "$WORK/run" "$WORK/plugins" "$WORK/dist" "$WORK/xdg-config" "$WORK/xdg-state" "$WORK/xdg-run"
> export HOME="$WORK/home" DDG_CONFIG_DIR="$WORK/cfg" DDG_STATE_DIR="$WORK/state" DDG_RUN_DIR="$WORK/run"
> export DDG_PLUGINS_DIR="$WORK/plugins" DDG_DIST_DIR="$WORK/dist"
> export XDG_CONFIG_HOME="$WORK/xdg-config" XDG_STATE_HOME="$WORK/xdg-state" XDG_RUNTIME_DIR="$WORK/xdg-run"
> unset SSH_AUTH_SOCK DDG_HUB_ENABLED DDG_SESSION_TOKEN DDG_HOST_SOCK DDG_TERMINAL_SOCK DDG_SESSION_ID DDG_ROOM_ID DDG_HOST_ID
> export DDG_HUB_ENABLED=0
> ```
> **미검증(`Unverified`) 원칙**: 환경 제약(Docker 미가동, 외부 CLI 미설치/미인증, 헤드리스 GUI 불가)으로 실행하지 못한 항목은 절대 통과로 표기하지 않고 `Unverified` + 구체 사유로 남긴다.

---

# 사용자 시나리오: A1-core — 로컬 CLI 에이전트 관리 · `ddg run` · OSC 7501 · 2-프로세스 `flock`/CAS 경합

## 확인할 요구사항과 보안 조건
- `A1`–`A4`, `D-3`, `D-SEC-01`/`07`/`08`, `F-11`: `ddg agent add/ls/show/default/rm`, `0o600` 권한, 기본 `env` 값 마스킹 vs `--reveal-env`, `DDG_*`·시크릿 키·동적 로더 4종 차단 + `LD_LIBRARY_PATH` 허용 + 오류 값 비에코, `ddg run` 기본 가시 룸+attach 및 `--bg` 백그라운드 즉시 제거, `TERM_PROGRAM=ddg` 및 외부 터미널 변수 제거, 2-프로세스 동시 `agent set` 경합 시 파일 무손상.

## 수행 방식: 에이전트

## 실제 지원 도구·환경과 필요한 권한
- 로컬 빌드 바이너리(`ddg`, `ddg-terminal-server`, `ddg-agents-host`), 표준 POSIX 도구(`stat`, `grep`, `kill`). 로컬 스크래치 디렉터리만 사용.

## 초기 데이터·계정·세션 및 병렬 격리
- 상단 공통 격리 프리앰블 적용. 부모 환경에 의도적으로 `TMUX=/tmp/fake`, `KITTY_WINDOW_ID=99`, `GHOSTTY_RESOURCES_DIR=/tmp/g`, `OPENAI_API_KEY=sk-parent-leak` 주입.

## 수행할 행동
1. 격리 `ddg-terminal-server core` + `ddg-agents-host serve` 기동.
2. `ddg agent add mock --env CUSTOM=visible-val --env LD_LIBRARY_PATH=/tmp/lib --default -- /bin/sh` 실행 및 `stat` 권한(`0600`)·상위 디렉터리(`0700`) 확인.
3. 보안 부정 입력: `--env LD_PRELOAD=/tmp/x.so`, `--env DDG_HOST_SOCK=/tmp/s`, `--env OPENAI_API_KEY=sk-secret-999` → 전부 non-zero 종료 + stderr에 `sk-secret-999` `0`회 등장 확인.
4. `ddg agent show mock` (값 마스킹 확인) vs `ddg agent show mock --reveal-env` (평문 확인).
5. `ddg run --bg -- -c 'env; printf "\033]7501;?\007"; printf "\033]7501;state=working:app=mock\033\\"; sleep 1'` 실행:
   - `TERM_PROGRAM=ddg` 존재, `TMUX`/`KITTY_WINDOW_ID`/`GHOSTTY_RESOURCES_DIR`/`sk-parent-leak` `0`건 확인.
   - `ddg session ls --json`에서 실행 중 `rm_bg_*`의 `Working` 상태 관측 후 1.5초 뒤 프로세스 종료 시 목록에서 즉시 제거됨을 확인.
6. 비-TTY 파이프(`ddg run --json < /dev/null`) 실행 시 즉시 attach 거부 확인.
7. 2개 병렬 서브셸에서 동시에 `ddg agent set mock --label A` / `--label B` 실행 → `agents.json`이 유효 JSON 유지(무손상) 및 CAS/락 동작 확인.

## 기대 화면·상태·저장 결과
- `agents.json` `0600`, 보안 차단 3건 실패+비에코, 마스킹/평문 분기, 외부 변수·부모 시크릿 0건, 백그라운드 종료 즉시 목록 0건, 2-프로세스 경합 후 JSON 유효.

## 관측 지점과 성공·실패 기준
- CLI exit code, `--json` stdout, `stat -f %Lp`, `grep -c` 결과 전항목 일치 시 Pass.

## 수집할 증거
- `$WORK/evidence-a1-core.log` (명령·출력·권한·JSON 스냅샷).

## 종료 후 정리
- `trap '...' EXIT INT TERM`: `$WORK/run/host.pid`·`$WORK/run/core.pid` 및 격리 `holder` 종료 확인 → `rm -rf "$WORK"`.

## 미검증 항목과 남은 위험
- 실제 외부 AI 에이전트 바이너리 동작은 `A1-live-agents`에서 분리 검증한다.

---

# 사용자 시나리오: A1-live-agents — 실제 `pi` (1.6.1) · `codex` (0.166.0) OSC 7501 방출 조건 실측

## 확인할 요구사항과 보안 조건
- `DNL-01`, `DNL-15`, `R-02`: 실제 설치된 `pi` (`1.6.1`)가 `Relay`의 FIFO `7501;?` 응답(DA1 선행)으로 OSC 7501을 활성화하는지, `codex` (`0.166.0`)의 OSC 7501 방출 조건(`features.psp`/`TERM_PROGRAM`) 및 미방출 시 OS `foreground` 폴백이 정상 동작하는지 실측.

## 수행 방식: 에이전트

## 실제 지원 도구·환경과 필요한 권한
- 로컬 설치 확인 완료: `/Users/dotdot/.local/bin/pi` (`1.6.1`), `/Users/dotdot/.local/bin/codex` (`0.166.0`). (`claude`는 본 머신 미설치 → 본 시나리오에서 `Unverified` 처리).

## 초기 데이터·계정·세션 및 병렬 격리
- 상단 공통 격리 프리앰블 적용(단, 실측 대상 CLI가 읽기 전용 인증을 필요로 할 경우 인증 부재 시 기동 프로브·`foreground` 폴백까지만 확인하고 라이브 LLM 턴은 `Unverified` 분리 기록).

## 수행할 행동
1. 격리 데몬 기동 후 `ddg run`으로 `pi` 기동 → 원시 프로브 및 `ddg session ls --json`에서 `7501;?` 응답 수신 후 `AgentState` 활성화 여부 및 `foreground`(`comm`/`script_hint`) 확인.
2. 동일하게 `codex` 기동 → 초기 대기/`features.psp` 여부별 OSC 7501 방출 실측 및 미방출 시 `foreground`(`codex`) 폴백 확인.

## 기대 화면·상태·저장 결과
- `pi`: `TERM_PROGRAM=ddg` + FIFO `7501;?` 응답으로 OSC 7501 상태 활성화 및 Kitty 이미지 오예약 없음.
- `codex`: 방출 조건(또는 미방출 + `foreground=codex` 폴백)이 객관 로그로 확정됨.

## 관측 지점과 성공·실패 기준
- `ddg session ls --json`의 `agent_status` / `foreground` 필드 및 캡처 로그.

## 수집할 증거
- `$WORK/evidence-a1-live.log` (`pi`/`codex` 버전·실측 표·인증 미보유 시 `Unverified` 항목 분리).

## 종료 후 정리
- `trap`: 격리 데몬·테스트 프로세스 종료 및 `rm -rf "$WORK"`.

## 미검증 항목과 남은 위험
- `claude` CLI 미설치 및 외부 유료 LLM 턴은 인증·비용 여부에 따라 `Unverified`로 남긴다.

---

# 사용자 시나리오: A2 — Core `SIGKILL` 1 MiB `tail` 복구 · `SIGTERM` 무중단 Detach · RAM 전용 `catalog.json`

## 확인할 요구사항과 보안 조건
- `UT-TERM-04`와 역할 분리: 실제 OS `SIGKILL` 후 Holder 1 MiB `tail` 재생(`replay=true`)으로 마지막 OSC 7501 `Blocked` 상태 복구 + PTY 중복 `7501;?` 응답 0바이트; `SIGTERM` 시 `GOODBYE::Detach`→`BYE` 후 종료(PTY 생존, `core.pid` 정리); 활동 10회 갱신 동안 `catalog.json` `(mtime, sha256)` 100% 불변; 손상 `agents.json` → `.corrupt.<ts>` (`0o600`).

## 수행 방식: 에이전트

## 실제 지원 도구·환경과 필요한 권한
- 로컬 빌드 바이너리 + POSIX `kill`, `stat`, `shasum`.

## 초기 데이터·계정·세션 및 병렬 격리
- 상단 공통 격리 프리앰블 적용.

## 수행할 행동
1. 격리 `core`/`host` 기동 → `$WORK/run/core.pid`·`host.pid` 확인.
2. 가시 룸 세션에서 `7501;?` 응답 카운터 기록 + `state=blocked:kind=permission:app=pi` 방출.
3. `$WORK/state/catalog.json` `(mtime, sha256)` 기록 → OSC 7501 10회 방출 → `catalog.json` 불변 확인.
4. `kill -9 "$(cat "$WORK/run/core.pid")"` → 자식 PTY `kill -0` 생존 확인 → `core` 재기동 → `Blocked` 상태 복구 + `7501;?` 응답 카운터 여전히 1회(재생 중 0B) 확인.
5. `kill -TERM "$(cat "$WORK/run/core.pid")"` → PTY 생존 + `core.pid` 삭제 확인.
6. `$WORK/cfg/agents.json`에 `{broken` 기록 → `ddg agent ls` 실패 + `agents.json.corrupt.*` (`0600`) 보존 확인.

## 기대 화면·상태·저장 결과
- `catalog.json` 불변, `SIGKILL` 후 `Blocked` 복구·중복 응답 0B, `SIGTERM` 후 PTY 생존·PID 삭제, `.corrupt.*` `0600`.

## 관측 지점과 성공·실패 기준
- `stat`/`shasum`, `kill -0`, 응답 카운터 파일, `ddg session ls --json`, `ls -la`. 전항목 일치 시 Pass.

## 수집할 증거
- `$WORK/evidence-a2.log`.

## 종료 후 정리
- `trap`: 격리 `host`/`core`/`holder` 종료 + `rm -rf "$WORK"`.

## 미검증 항목과 남은 위험
- 1 MiB `tail` 초과 출력 후 `SIGKILL` 시 직전 OSC 7501 유실은 설계상 알려진 한계.

---

# 사용자 시나리오: A3 — Docker 멀티호스트(`host_overrides` · 원격 `ddg run`) & 구버전 원격 호스트 안전 업그레이드

## 확인할 요구사항과 보안 조건
- `D-4`, `D-SEC-02`, `DNL-07`, `E-SEC-09`: `expected_host_id` 불일치 시 Fail-closed + `hosts.json` 미변경; 일치 시 `expected_host_id` 보존 + `host`→`core` 종료 + PID 부재 구버전 `ps` 정확 매칭(`holder` 및 살아있는 PTY PID 100% 생존) + 사후 `hello.features` `"agent_activity_v1"` 검증; `host_overrides.env` `null` 제거 및 원격 CWD 규칙; `parent_host_id` 원격 종료 안전성.

## 수행 방식: 에이전트

## 실제 지원 도구·환경과 필요한 권한
- Docker (`29.9.3`), `ssh-keygen`, 로컬 빌드 `ddg` 및 Linux 컨테이너 바이너리.

## 초기 데이터·계정·세션 및 병렬 격리
- 상단 공통 격리 프리앰블 적용 (`HOME`, `DDG_DIST_DIR`, `SSH_AUTH_SOCK` 완전 격리).
- 컨테이너 `ddg-scn-a3-$$`는 반드시 **루프백 전용 `-p 127.0.0.1:<ephemeral-port>:22`** 로 바인딩. 임시 SSH 키(`chmod 600`), `UserKnownHostsFile="$WORK/known_hosts"`.
- 컨테이너 내부에 구버전(`"agent_activity_v1"` 미광고) 데몬 + 살아있는 테스트 PTY 기동 후 `host.pid`/`core.pid` 삭제.

## 수행할 행동
1. `$WORK/cfg/hosts.json`에 틀린 `expected_host_id: "wrong-id"` 설정 → 원격 업그레이드 시도 → Fail-closed 실패·데몬 미재시작·`hosts.json` 불변 확인.
2. 실제 `host_id` 핀 설정 → 업그레이드 전 `holder` PID·테스트 PTY PID 기록 → 업그레이드 실행.
3. 업그레이드 후 확인: `holder` PID·PTY PID **100% 동일**, `host`·`core` PID 교체, `expected_host_id` 보존, `ddg machine test <remote> --json` `features`에 `"agent_activity_v1"` 포함.
4. `agents.json` `env: {"KEEP":"1","DROP_ME":"x"}` + `host_overrides.env: {"DROP_ME":null,"REMOTE":"y"}`로 원격 `ddg run --bg` → 원격 `env`에 `KEEP=1`/`REMOTE=y` 존재, `DROP_ME` 0건, `pwd`가 로컬 `/Users/...`가 아님 확인.
5. 원격 백그라운드 세션 종료 후 오류 없이 `finish_exit` 정리 확인.

## 기대 화면·상태·저장 결과
- 불일치 업그레이드 거부, 정상 업그레이드 시 PTY 무중단+`agent_activity_v1` 활성화, 원격 `env`/`pwd` 규칙 충족.

## 관측 지점과 성공·실패 기준
- `docker exec` 전후 `ps -eo pid,args`, `ddg machine test --json`, `hosts.json` diff, 원격 PTY 출력.

## 수집할 증거
- `$WORK/evidence-a3.log`.

## 종료 후 정리
- `trap`: `docker rm -f "ddg-scn-a3-$$" 2>/dev/null || true`, 격리 데몬 종료, `rm -rf "$WORK"`.

## 미검증 항목과 남은 위험
- Docker 미가동 또는 Linux 크로스 빌드 부재 시 `Unverified` 보고. 실제 WAN SSH 패킷 손실은 로컬 루프백 Docker로 미재현.

---

# 사용자 시나리오: H1 — 사람 GUI 실기: 2단 사이드바 · 22px 분할 헤더 · 0Hz 유휴 · 런처 4지선다 · 정렬/핀/Peek

## 확인할 요구사항과 보안 조건
- `UX-1`~`UX-3`, `D-1`, `D-2`, `F-13`: 접이식 2단 사이드바(45% cap + `룸` 접힘 시 해제), `ui.json` v2 `collapsed_sections` 및 `ui_scale_pct` 재시작 보존, 분할 패인 22px 헤더(50%·100%·150% 줌에서 하단 행 미잘림 + `Readonly` 배너 미겹침), `observe_window_activation` 창 복귀 시 `seen` 해제, 유휴 시 0Hz 스피너 정지, 런처 4지선다 + `Cmd/Ctrl+1..9` + 일반 `1..9` 필터, `priority`/`grouped`/`manual` + 핀/비핀 내부 서브트리 이동, 백그라운드 Peek `Readonly`(`Esc` 닫기) → `[제어 요청]`(`Control`에서 `Esc` 터미널 전달, `Cmd/Ctrl+W` Peek 닫기·세션 생존·제어권 반납) → `[새 룸으로 승격]`.

## 수행 방식: 사람

## 실제 지원 도구·환경과 필요한 권한
- macOS 데스크톱 GUI (`cargo run -p ddg-agents-native`, `GHOSTTY_VT_LIB_DIR` 설정), 격리 `DDG_CONFIG_DIR`/`DDG_STATE_DIR`/`DDG_RUN_DIR`.

## 초기 데이터·계정·세션 및 병렬 격리
- 상단 공통 격리 프리앰블로 빈 환경에서 앱 실행.

## 수행할 행동
1. 일반 `+` 탭(셸 유지) vs `Cmd+Shift+P` 런처 4지선다(필터에 `gpt-4` 입력 시 숫자키 실행 안 됨 확인, `Cmd+1` 실행 확인) 테스트.
2. 가시 룸 분할(`Cmd+D`) 후 22px 패인 헤더·하단 프롬프트 미잘림·50%/150% 줌 스케일 확인.
3. 백그라운드 에이전트 실행 → 사이드바 트리·`bg` 배지 확인 → 클릭해 Peek(`Readonly`) 진입 → `Esc` 닫기(세션 생존 확인) → 재진입 후 `[제어 요청]` → `Esc`가 터미널로 전달됨 확인 → `Cmd+W`로 Peek 닫기(세션 생존·제어권 반납 확인) → 재진입 후 `[새 룸으로 승격]` 확인.
4. 다른 앱으로 창 전환(비활성) → 백그라운드에서 `Done`/`Blocked` 발생 → 앱 창 복귀 시 포커스 패인 `Done` 미확인 점 자동 해제(`Blocked` `!` 배지는 유지) 확인.
5. `Cmd+Shift+L` 리스트 포커스 → `P` 핀, `Alt+↑/↓` 서브트리 이동(`manual` 자동 전환), `룸` 섹션 접기 시 상단 45% 제한 해제 확인 → 앱 재시작 후 `ui.json` 상태 및 줌 보존 확인.

## 기대 화면·상태·저장 결과
- 전 항목 시각 겹침·잘림·오작동 없음, 유휴 시 CPU 스파이크 없음, 재시작 후 `ui.json` 완전 복원.

## 관측 지점과 성공·실패 기준
- 육안 확인 + Activity Monitor/프로파일러 유휴 CPU 확인 + `$WORK/cfg/ui.json` 검사.

## 수집할 증거
- 화면 캡처 및 체크리스트 판정 로그(실행 전 상태: `Unverified`).

## 종료 후 정리
- 앱 종료(`⌘+Q`), 격리 데몬 종료, `rm -rf "$WORK"`.

## 미검증 항목과 남은 위험
- 사람 실행 전까지 `Unverified` 유지; Linux X11/Wayland 실기는 macOS 검증과 별도.

---

# 사용자 시나리오: H2 — 사람 GUI 실기: `AgentSettingsModal` · 로컬 커스텀 SVG 핫스왑 · 손상 복구 · 구버전 호스트 배너

## 확인할 요구사항과 보안 조건
- `A5`, `A6`, `D-3`, `D-4`, `D-SEC-04`/`08`, `F-13`: `Cmd+,`(Linux `Ctrl+Shift+,`) 모달 열기, `argv` 칩 프리뷰·셸 연산자 차단·비에코 오류, `env` 마스킹/동적 로더 4종 차단, CLI 동시 수정 시 외부 변경 충돌 배너(`[외부 변경 불러오기]` Primary), `agents.json` 손상 시 Fail-closed 복구 카드(`.corrupt.<ts>`), 로컬 `~/.config/ddg/icons/` SVG 핫스왑(해시 키 캐시 무효화) 및 악성 SVG 안전 폴백, 원격 세션 내장 아이콘 전용, 좁은 사이드바(`160px`) 1줄 축약 및 툴팁(`세션 보고: `).

## 수행 방식: 사람

## 실제 지원 도구·환경과 필요한 권한
- macOS 데스크톱 GUI (`ddg-agents-native`), 터미널(`ddg` CLI), 격리 스크래치 디렉터리.

## 초기 데이터·계정·세션 및 병렬 격리
- 상단 공통 격리 프리앰블 + `$WORK/cfg/icons/custom-bot.svg` 정상 SVG 및 악성 `<script>` SVG 준비.

## 수행할 행동
1. `Cmd+,`로 `AgentSettingsModal` 오픈 → `claude --model "opus-4"` 입력(칩 3개 확인) → `; rm -rf /` 입력 시 오류+저장 비활성화 확인 → `LD_PRELOAD=x` 차단 확인.
2. 모달 열어둔 채 별도 터미널에서 `ddg agent add ext -- /bin/sh` 실행 → 모달 `[저장]` 클릭 시 외부 변경 충돌 배너 표시 및 `[외부 변경 불러오기]` 동작 확인.
3. `$WORK/cfg/agents.json` 손상 후 모달 재오픈 → 복구 카드(`[다시 불러오기]`/`[경로 복사]`/`[백업 후 초기화]`) 확인.
4. 로컬 커스텀 아이콘 `custom-bot.svg` 표시 → 파일 색상을 변경·저장 시 앱 재시작 없이 아이콘 갱신 확인 → `<script>` 삽입 시 크래시 없이 기본 터미널 SVG 폴백 확인.
5. 사이드바를 최소 폭(`160px`)까지 드래그 → 1줄 축약 및 호버 툴팁(`세션 보고: ...`) 확인.

## 기대 화면·상태·저장 결과
- 크래시·데이터 유실·시크릿 에코 0건, SVG 핫스왑 및 폴백 정상, 좁은 사이드바 가독성 유지.

## 관측 지점과 성공·실패 기준
- 육안 확인 + `$WORK/cfg/agents.json.corrupt.*` 확인.

## 수집할 증거
- 화면 캡처 및 체크리스트 판정 로그(실행 전 상태: `Unverified`).

## 종료 후 정리
- 앱 종료, 격리 데몬 종료, `rm -rf "$WORK"`.

## 미검증 항목과 남은 위험
- 사람 실행 전까지 `Unverified` 유지; `modal_text_input`의 한글 IME 조합/중간 커서는 알려진 한계로 제외.
