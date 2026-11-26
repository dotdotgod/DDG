# 실제 환경 사용자 시나리오 검증 설계 (확정본)

[계획 목차](README.md) · [UX·UI 설계](DESIGN_UI.md) · [데이터·네트워크 설계](DESIGN_DATA.md) · [테스트 설계](TESTS.md)

> 에이전트 수행 시나리오(`US-03` 실행부, `US-04`–`US-06`)는 기능·계약·보안 경계 검증이며 **실제 사용자 사용성 평가로 표현하지 않는다**. 실기·자격증명 부재로 미실행된 항목은 반드시 **`미검증`**으로 보고한다.

---

# 사용자 시나리오: `US-01` macOS `.app` 로컬 무중단 업데이트 · A17/A18 · 실서명 · N-1 롤백

## 확인할 요구사항과 보안 조건
- macOS `.app` 사전검사(`AppTranslocation`/쓰기 권한), 5요소 `codesign -R`(leaf OID `1.2.840.113635.100.6.1.13`) + `spctl`, `renamex_np(RENAME_SWAP)` + N-1 `.app` 보존, A17 즉시 재시작, `Applying` 중 `Cmd+Q` 보류, A18 배너 세션 한정 닫기(`skipped_version` 부재·재기동 재표시), `DN-02`/`SEC-D-01` PTY 생존, 롤백 전 N-1 `codesign -R` 재검증.
## 수행 방식: 사람
## 실제 지원 도구·환경과 필요한 권한
- **[로컬 픽스처 범위]** macOS arm64 실기, `/usr/bin/codesign`, `/usr/sbin/spctl`, 격리 `~/Applications/DDG-Test/`.
- **[실기·실서명 필수 릴리스 게이트]** Apple Developer ID 실서명·공증·`stapler validate` 통과 `.app` 2개 버전(`N`, `N+1`).
## 초기 데이터·계정·세션 및 병렬 격리
- `DDG_{CONFIG,STATE,RUN,DIST}_DIR=/tmp/ddg-us01-<ts>/`, `~/Applications/DDG-Test/DDG Agents.app`(버전 `N`), 활성 PTY `sh -c 'echo ALIVE_US01; sleep 300'`.
## 수행할 행동
1. `N+1` 배너 확인 → `[×]`로 닫기(A18) → `update.json`/`update-state.json`에 `skipped_version` 부재 확인 → 앱 재시작 후 배너 재표시 확인.
2. `[업데이트 적용]` 클릭 → `Applying` 중 `Cmd+Q` 입력(보류 확인) → 완료 즉시 자동 재시작(A17) 및 PTY `ALIVE_US01`/PID 생존 확인.
3. 모달에서 N-1 롤백(키보드 `Enter` 미실행 확인 → 버튼 클릭) → 버전 `N` 복귀 및 PTY 생존 확인.
4. `com.apple.quarantine` 부여 후 `AppTranslocation` 실행 차단 확인 + ad-hoc 서명 번들의 `codesign -R` 거부 확인.
## 기대 화면·상태·저장 결과
- A18 세션 닫기·재기동 재표시, `Applying` 중 `Cmd+Q` 보류, 재시작 후 버전 `N→N+1→N`, PTY PID 불변, N-1 `.app` 보존, `AppTranslocation`/ad-hoc 차단.
## 관측 지점과 성공·실패 기준
- **성공**: `ps -p <pty_pid>` 불변, `codesign -R`·`spctl` exit 0(정상)·non-zero(ad-hoc), `update-state.json` `rollback_version` 정합. **실패**: PTY 종료, 서명 우회, `Enter` 고위험 실행.
## 수집할 증거
- 전후 `ps` 출력, `codesign -dvv`/`spctl -vv` 로그, `update.json`/`update-state.json`, 화면 캡처.
## 종료 후 정리
- 테스트 앱·데몬 종료, `~/Applications/DDG-Test/` 및 `/tmp/ddg-us01-<ts>/` 삭제.
## 미검증 항목과 남은 위험
- Apple Developer ID 자격증명 부재 시 실서명·공증·`spctl` 통과 게이트는 **`미검증(자격증명 대기)`**으로 남긴다.

---

# 사용자 시나리오: `US-02` Windows ConPTY · Named Pipe DACL · 2회차 연속 `.old.<nonce>` 업데이트

## 확인할 요구사항과 보안 조건
- `windows-latest` `ghostty-vt.lib`+GPUI 빌드 실동작, ConPTY+JobObject 고아 0건, 생성 시점 `PROTECTED_DACL_SECURITY_INFORMATION` SID DACL, Named Pipe, **2회차 연속 업데이트(`v1→v2→v3`) 중 v1 Holder 생존 시 `.old.<nonce1>`/`.old.<nonce2>` 공존 + startup GC 잠긴 툼스톤 무음 스킵 + 종료 후 최종 GC**.
## 수행 방식: 사람
## 실제 지원 도구·환경과 필요한 권한
- **[자동화/CI 범위]** `windows-latest` 스파이크 및 `#[cfg(windows)]` UT(`UT-OS-01..03`, `UT-HOST-03`).
- **[실기 필수 게이트]** Windows 10/11 x64 실기, PowerShell(`icacls`, `Get-Acl`, `Get-Process`), 표준 계정 A(+선택 계정 B).
## 초기 데이터·계정·세션 및 병렬 격리
- `%LOCALAPPDATA%\ddg-us02-<ts>\`에 버전 `v1` 4종 `.exe` 설치, 전용 `DDG_*_DIR`, v1 활성 ConPTY 세션 1개(`ping -n 600 127.0.0.1`).
## 수행할 행동
1. ConPTY 입출력·리사이즈 및 `Get-Acl`/`icacls`로 디렉터리·Named Pipe가 `PROTECTED` + 계정 A SID 한정인지 확인(계정 B 접속 거부 확인).
2. v1 ConPTY 세션 유지 상태로 `v1 → v2` 업데이트 적용(즉시 재시작) → `.old.<nonce1>` 생성 및 v1 세션 생존 확인.
3. v1 ConPTY 세션이 여전히 살아 있는 상태에서 연속 `v2 → v3` 업데이트 적용 → `.old.<nonce1>`·`.old.<nonce2>` 공존, startup GC가 잠긴 `.old.<nonce1>`을 오류 없이 스킵하는지 확인.
4. v1 세션 종료 후 `ping.exe` 고아 0건 확인 → 앱 재기동 후 모든 `.old.*` 파일 GC 완료 확인.
## 기대 화면·상태·저장 결과
- 2회차 연속 교체 `ERROR_ACCESS_DENIED` 0건, v1 세션 생존, 종료 후 고아 0건·`.old.*` 0건, DACL 격리 충족.
## 관측 지점과 성공·실패 기준
- **성공**: 버전 `v1→v2→v3`, `.old.*` 공존→종료 후 0건, DACL 타 계정 ACE 0건. **실패**: 2회차 잠금 실패, GC 오류, 고아 프로세스 잔존.
## 수집할 증거
- 단계별 디렉터리 목록, `icacls`/`Get-Acl` 출력, `Get-Process` 로그, 화면 캡처.
## 종료 후 정리
- 테스트 프로세스 종료, `%LOCALAPPDATA%\ddg-us02-<ts>\` 삭제.
## 미검증 항목과 남은 위험
- macOS 개발 환경에서는 실행 불가 — Windows 실기 실행 전 반드시 **`미검증(Windows 환경 미실행)`**으로 보고한다.

---

# 사용자 시나리오: `US-03` 원격 SSH(`HostTransport::Ssh`) 무중단 업데이트 · 실패 복구 · 롤백 · `v0.1.0` 레거시

## 확인할 요구사항과 보안 조건
- `HostTransport::Ssh` 전용, 사전/사후 `RequirePinnedHostId`, 원격 `0700` 스테이징 + 해시 3종 검증, `DN-02`+`SEC-D-01` 원격 PTY·토큰 연속성, 바인드 실패 시 구 Host 무손실 재개, 수동 롤백(`ddg update rollback -H`), `v0.1.0` 레거시 최초 1회 업그레이드, `glibc ≤ 2.36` 실행.
## 수행 방식: 사람(사전 준비: 원격 Linux 호스트·SSH 키·격리 `hosts.json`) + 에이전트(CLI 실행·관측)
## 실제 지원 도구·환경과 필요한 권한
- 로컬 `/usr/bin/ssh`, `ddg`, 원격 Linux(`glibc ≤ 2.36`) SSH 테스트 호스트.
## 초기 데이터·계정·세션 및 병렬 격리
- 원격 격리 `DDG_{STATE,RUN}_DIR`에 `N`(및 변형 `v0.1.0`) 기동, 로컬 임시 `hosts.json`에 `expected_host_id` 핀된 `work-test`, 활성 PTY `sh -c 'echo REMOTE_US03; sleep 300'`.
## 수행할 행동
1. `ddg update check -H work-test --json` → `ddg update apply -H work-test --json`.
2. 원격 PTY PID·`REMOTE_US03`·기존 세션 토큰 `ddg session read` 및 원격 스테이징 `0700` 확인.
3. 신 Host 기동 실패 조건 주입 → 구 Host 무손실 재개 및 `failed_versions` 기록 확인.
4. `ddg update rollback -H work-test --yes --json` → 버전 `N` 복귀 및 PTY 생존 확인 (+ `v0.1.0` 레거시 최초 1회 업그레이드 확인).
## 기대 화면·상태·저장 결과
- 정상 적용·롤백 모두 PTY PID·토큰 유지, 실패 시 구 Host 유지, `--json` 단일 객체.
## 관측 지점과 성공·실패 기준
- **성공**: 원격 `ps` PID 불변, `session read` 성공, `stat` `700`, `N→N+1→N`. **실패**: PTY 단절, 토큰 무효화(>=v0.2), 호스트 다운.
## 수집할 증거
- CLI `--json` stdout/stderr, 원격 `ps`/`stat`/`readelf -V` 로그, `update-state.json`.
## 종료 후 정리
- 원격 데몬 종료 및 격리 디렉터리·임시 `hosts.json` 삭제.
## 미검증 항목과 남은 위험
- 원격 Linux(`glibc ≤ 2.36`) 호스트 미확보 시 **`미검증`** 유지.

---

# 사용자 시나리오: `US-04` 컨테이너(`ContainerSsh` / `DockerExec`) 업데이트 차단 및 단일 라인 안내

## 확인할 요구사항과 보안 조건
- `ContainerSsh`·`DockerExec` 모두 `container_update_unsupported` 거부, 컨테이너 내부 파일 무변경, 단일 라인 수동 갱신 안내.
## 수행 방식: 에이전트 (기능·보안 계약 검증)
## 실제 지원 도구·환경과 필요한 권한
- 로컬 `/usr/local/bin/docker`(데몬 실행), `ddg`.
## 초기 데이터·계정·세션 및 병렬 격리
- 임시 `DDG_CONFIG_DIR/hosts.json`에 `DockerExec`(`ctr-exec`)·`ContainerSsh`(`ctr-ssh`) 등록, 테스트 컨테이너 `ddg-us04-<ts>`.
## 수행할 행동
1. `ddg update apply -H ctr-exec --json`, `rollback -H ctr-exec --json`, `apply -H ctr-ssh --json` 실행.
2. 컨테이너 내부 `ddg-agents-host` 전후 `sha256sum` 비교.
## 기대 화면·상태·저장 결과
- 모두 non-zero + 단일 JSON `code == "container_update_unsupported"`, 단일 라인 안내, 해시 불변.
## 관측 지점과 성공·실패 기준
- **성공**: 코드 일치·JSON 파싱 통과·컨테이너 해시 100% 일치. **실패**: 푸시 시도·컨테이너 변경·비JSON 출력.
## 수집할 증거
- 명령 exit code/stdout/stderr, 전후 `sha256sum`.
## 종료 후 정리
- `docker rm -f ddg-us04-<ts>`, 임시 `DDG_*_DIR` 삭제.
## 미검증 항목과 남은 위험
- Docker 데몬 미가동 시 라이브 해시 비교는 `미검증`; GUI 복사 버튼 시각 확인은 사람 확인 영역.

---

# 사용자 시나리오: `US-05` 실환경 보안 부정 경계 (OS `codesign` · 프로세스 간 `host-admin` · 시크릿 잔존 · TOCTOU)

## 확인할 요구사항과 보안 조건
- 유닛으로 입증 불가한 실바이너리·OS 경계: 실제 `codesign -R` ad-hoc/변조 거부, 실제 OS 프로세스 간 PTY/플러그인의 `host-admin` 차단, 실행 중 프로세스·로그·파일시스템 시크릿 평문 0건, 실제 symlink 치환 TOCTOU 거부.
## 수행 방식: 에이전트 (실환경 보안 부정 검증)
## 실제 지원 도구·환경과 필요한 권한
- 로컬 `ddg`, `ddg-agents-host`, `ddg-terminal-server`, `/usr/bin/codesign`, `python3`. 일반 사용자 권한.
## 초기 데이터·계정·세션 및 병렬 격리
- `/tmp/ddg-us05-<ts>/`, 더미 `GITHUB_TOKEN=ghp_TestSecretToken1234567890`, ad-hoc 서명 테스트 번들, 실행 중 로컬 호스트+PTY+플러그인.
## 수행할 행동
1. ad-hoc/변조 `.app` 대상 실제 `codesign --verify --deep --strict -R ...` 거부 및 스테이징 파일 0건 잔존 확인.
2. PTY 내부 셸 및 플러그인 프로세스에서 `host-admin.sock` 리로드 요청 → 거부 및 토큰 미노출 확인.
3. 더미 `GITHUB_TOKEN` 주입 상태에서 오류 경로 실행 후 stdout/stderr/`host.log`/`update-state.json` 전체 `grep` 검사.
4. 헬퍼 실행 직전 스테이징 경로를 심볼릭 링크로 치환해 `lstat` TOCTOU 거부 및 원본 무손상 확인.
## 기대 화면·상태·저장 결과
- 4건 모두 fail-closed 거부, 시크릿 평문 0건, 원본 바이너리 해시 불변.
## 관측 지점과 성공·실패 기준
- **성공**: 4건 기대 거부 코드 + `grep` 0건 + 원본 해시 불변. **실패**: 어느 1건 통과 또는 시크릿 1바이트 노출.
## 수집할 증거
- 각 명령 출력, `grep` 결과(0건), 전후 바이너리 SHA-256.
## 종료 후 정리
- 테스트 데몬 종료 및 `/tmp/ddg-us05-<ts>/` 삭제.
## 미검증 항목과 남은 위험
- 타 OS 사용자 계정 간 소켓 격리는 별도 계정 없이는 정적 `0600`/`0700` 확인으로 제한된다.

---

# 사용자 시나리오: `US-06` CLI 에이전트 워크플로 (`--json` 단일 계약 · `-H` 기본 `local` · 오프라인 무네트워크)

## 확인할 요구사항과 보안 조건
- `--json` stdout 단일 JSON, `default_host_id`가 원격이어도 `-H` 생략 시 `local` 고정, `auto_check=off` 및 타 명령(`ls`/`status`/`session ls`) 네트워크 요청·업데이트 알림 0건, `ddg --skill` 갱신.
## 수행 방식: 에이전트 (CLI 계약 검증)
## 실제 지원 도구·환경과 필요한 권한
- 로컬 `ddg`, `python3`, 블랙홀 프록시(`HTTP_PROXY=http://127.0.0.1:1 HTTPS_PROXY=http://127.0.0.1:1`) 또는 로컬 trap 리스너.
## 초기 데이터·계정·세션 및 병렬 격리
- `/tmp/ddg-us06-<ts>/`, `hosts.json`에 `default_host_id: "remote-prod"`(연결 불가 trap 주소) 설정.
## 수행할 행동
1. 블랙홀 프록시 환경에서 `ddg update config --auto-check off --json` → `ddg update status --json` 실행.
2. `ddg ls --json`, `ddg status --json`, `ddg --skill` 실행.
## 기대 화면·상태·저장 결과
- 모든 `--json` 출력 `python3 -m json.tool` 단일 객체 통과, `host == "local"`, trap 접속 0건, `last_checked_at` 미갱신, 타 명령 출력에 업데이트 문구 0건, `--skill`에 `update` 5서브커맨드 포함.
## 관측 지점과 성공·실패 기준
- **성공**: 단일 JSON 파싱·trap 접속 0건·네트워크 오류 없이 성공. **실패**: 비JSON 혼입·원격 다이얼·네트워크 호출 발생.
## 수집할 증거
- raw stdout/stderr, `json.tool` 결과, trap 리스너 accept 카운트(0).
## 종료 후 정리
- 데몬 종료 및 `/tmp/ddg-us06-<ts>/` 삭제.
## 미검증 항목과 남은 위험
- 실제 GitHub API 응답 지연/Rate Limit은 `US-01`/`US-03` 실릴리스 연동에서 보완한다.
