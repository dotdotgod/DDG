# 사용자 시나리오 검증 설계

[계획 목차](README.md) · [테스트 설계](TESTS.md) · [태스크 S3–S5](TASKS_HOST_UI.md)

> **공통 원칙 (`Unverified` 규칙)**: 실제 환경 제약(Docker 미기동, GUI 디스플레이 부재 등)으로 실행하지 못한 항목은 통과로 간주하지 않고 반드시 `Unverified` + 구체 사유로 보고한다. 모든 시나리오는 사용자 실운영 `~/.local/state/ddg`·`~/.config/ddg`를 건드리지 않고 격리 스크래치 디렉터리(`mktemp -d`)를 사용한다.

---

# 사용자 시나리오 1: 단일 호스트 + Docker 크로스 호스트 에이전트 협업 E2E

## 확인할 요구사항과 보안 조건
- 전역 고유 ID(`rm_*`, `rm_bg_*`, `ses_*`), `1B` 재시작 세대 증가 및 구 토큰 폐기, `2A` 6개 `DDG_*` env + 브리지 CLI, `3A`/`Q2` 숨김 세션 생성→승격→종료 즉시 삭제, `Q1`/`Q6` 제어권 유지 `write_input`/`read_screen`, `Q4`/`Q5` Hub 단독 중계(원격 컨테이너에 타 호스트 설정/키 0개), `host.log` `0o600`·토큰/페이로드 미노출.

## 수행 방식: 에이전트

## 실제 지원 도구·환경과 필요한 권한
- 로컬 빌드 바이너리(`ddg-terminal-server`, `ddg-agents-host`), Docker daemon + Linux arm64 `ddg-host` 이미지, `ssh-keygen`(임시 키).
- 네트워크/인프라 변경 없음(로컬 루프백 포트 및 임시 컨테이너만 사용).

## 초기 데이터·계정·세션 및 병렬 격리
- `WORK="$(mktemp -d /tmp/ddg-scn1-XXXXXX)"` 생성 및 `trap`으로 종료 시 컨테이너·임시 프로세스·`$WORK` 완전 정리.
- 로컬 Hub: `DDG_STATE_DIR=$WORK/hub-state`, `DDG_RUNTIME_DIR=$WORK/hub-run`, `DDG_CONFIG_DIR=$WORK/hub-cfg`, `DDG_HUB_ENABLED=1`.
- Docker 원격 컨테이너(`ddg-scn1-remote`): 임시 SSH 공개키만 주입, `~/.config/ddg/hosts.json` 미생성(타 호스트 설정 0개). Hub의 `$WORK/hub-cfg/hosts.json`에 원격 `expected_host_id` 핀닝 등록.

## 수행할 행동
1. Hub `ddg-terminal-server` + `ddg-agents-host` 기동, 원격 Docker 컨테이너 기동 및 Hub `host.list_hosts`에서 원격 `connected` 확인.
2. Hub에 가시 룸(`rm_*`)과 터미널 세션 A(`ses_A`) 생성 → 세션 A 내부 셸에서 `env | grep ^DDG_`로 6개 변수 확인.
3. 세션 A에서 브리지 CLI로 숨김 워커 세션 B 생성(`ddg-agents-host session create`) → `session list`에서 `rm_bg_*` 소속 확인.
4. 세션 A ↔ B 간 `msg send`/`msg recv` 양방향 왕복 후, 세션 B에 `write_input --enter` 및 `read_screen` 수행.
5. 세션 B에 `session restart` 실행(`1B`: `ses_B` 유지, `run_generation == 2`), 재시작 전 캡처한 구 토큰으로 호출 시 `token_revoked` 확인.
6. 세션 B에서 `session create --host <remote_id>`로 원격 숨김 세션 C 생성 → 원격 세션 C 내부에서 `msg send --host <hub_id> --to <ses_A>` 역방향 메시징 및 `read_screen`/`write_input` 수행.
7. 원격 세션 C를 `session move --new-room "remote-promoted"`로 승격(`3A`), 별도 숨김 세션 D 종료 후 카탈로그 즉시 삭제 및 부모 인박스 `session_exited` 수신 확인(`Q2`).

## 기대 화면·상태·저장 결과
- 모든 ID가 `rm_*`/`rm_bg_*`/`ses_*` 형식; 원격 컨테이너 내부에 `hosts.json`/타 호스트 SSH 키 0개; `host.log` 권한 `600` 및 토큰 문자열(`ses_*.1.*`)·메시지 본문 0건; `catalog.json`에 `session_token` 0건.

## 관측 지점과 성공·실패 기준
- CLI JSON 출력·종료코드(빈 `msg recv`=`0`+`[]`), `stat -f %Lp`/`stat -c %a`(`600`), `grep -c`(`0`), `docker exec ls ~/.config/ddg`(부재). 모든 단계 기대값 일치 시 Pass, 1건이라도 불일치 시 Fail.

## 수집할 증거
- 단계별 CLI JSON 로그(`$WORK/evidence.log`), `stat` 출력, `grep` 결과(0건 확인), `docker exec` 무자격증명 확인 출력, 최종 잔존 PID `0` 확인.

## 종료 후 정리
- `trap`: `docker rm -f ddg-scn1-remote`, Hub 서버/홀더 PID `SIGTERM`→`SIGKILL` 확인, `rm -rf "$WORK"`.

## 미검증 항목과 남은 위험
- Docker daemon 미가동 시 원격 구간(단계 1 원격·6·7)은 `Unverified`로 기록하며 단일 호스트 구간 결과와 분리 보고한다.

---

# 사용자 시나리오 2: 사람 GUI 실기 검증 (v1 리셋 · 숨김 격리 · 승격 · 외부 입력 배지)

## 확인할 요구사항과 보안 조건
- `Q3` v1 백업 리셋 단일 배너(basename 표시·첫 가시 룸 생성 또는 `×`로 해제), `3A` 숨김 룸 사이드바 비노출 및 `session_move` 0→1 자동 펼침, `1B` 탭 재시작 후 VT 초기화 및 키 입력 정상, `Q1`/`Q6` 외부 `write_input` 시 GUI 제어권 유지 + 3초 배지 표시(원문 미노출).

## 수행 방식: 사람 (에이전트는 스크래치 환경과 외부 CLI 트리거만 보조)

## 실제 지원 도구·환경과 필요한 권한
- macOS 데스크톱 환경, 빌드된 `ddg-agents-native` 앱, 터미널 1개(보조 CLI 트리거용).

## 초기 데이터·계정·세션 및 병렬 격리
- `WORK="$(mktemp -d /tmp/ddg-scn2-XXXXXX)"`; `$WORK/state/catalog.json`에 v1(`u64`) 픽스처 배치.
- 앱과 데몬 모두 `DDG_STATE_DIR=$WORK/state DDG_RUNTIME_DIR=$WORK/run DDG_CONFIG_DIR=$WORK/cfg`로 실행(실운영 `~/.local/state/ddg` 완전 격리).

## 수행할 행동
1. 스크래치 환경으로 로컬 데몬 + `ddg-agents-native` 실행.
2. **(체크 1: v1 리셋 배너)** 앱에서 `catalog.json.v1.bak.<ts>`(basename만) 안내 배너 확인 → `+`로 첫 가시 룸 생성 시 배너 자동 해제 확인.
3. **(체크 2: 숨김 룸 비노출)** 보조 CLI로 숨김 백그라운드 세션 생성 → 사이드바에 `rm_bg_*`가 나타나지 않음 확인.
4. **(체크 3: `session_move` 승격)** 보조 CLI로 해당 숨김 세션을 새 가시 룸(`"promoted-ui"`)으로 승격 → 사이드바에 즉시 표시(접힌 머신이면 자동 펼침, 다른 룸 작업 중이면 포커스 미탈취) 확인.
5. **(체크 4: `1B` 재시작)** 탭 재시작 실행 → 화면 초기화 후 새 셸 프롬프트에 키 입력 정상 동작 확인.
6. **(체크 5: `Q1`/`Q6` 외부 입력 배지)** 활성 탭에 포커스 둔 채 보조 CLI로 `session write --enter "echo hello"` 실행 → 터미널에 출력 표시, 상단 `"외부 입력"` 배지 약 3초 후 소멸, 원문·`ses_*` 미표시, 사용자가 즉시 키보드 타이핑 가능(제어권 유지) 확인.

## 기대 화면·상태·저장 결과
- 5개 체크 전항목 시각·동작 일치; `$WORK/state/catalog.json.v1.bak.*` 권한 `0600` 존재; `$WORK/state/catalog.json` `schema_version: 2`.

## 관측 지점과 성공·실패 기준
- 각 체크(1~5)별 객관적 Pass/Fail 기록(전체 5/5 Pass 시 통과).

## 수집할 증거
- 체크 1~5 결과 표, 배너/배지 스크린샷(선택), `$WORK/state` `ls -la` 출력.

## 종료 후 정리
- 앱 종료, `$WORK` 내 스크래치 데몬/홀더 종료, `rm -rf "$WORK"`.

## 미검증 항목과 남은 위험
- 헤드리스 실행 세션에서는 본 시나리오 전체를 `Unverified (사람 GUI 실기 미수행)`로 남기고 완료로 표기하지 않는다.
