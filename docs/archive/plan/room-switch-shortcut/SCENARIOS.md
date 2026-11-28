# 실제 환경 사용자 시나리오 검증 — 룸 전환 단축키

- **역할**: `test-designer` (Antigravity Gemini 4 Neon) · 독립 리뷰(`F-T5`, Astra `validation-requirements` 2차 P1/P2) 반영
- **근거**: `DESIGN_UI.md`, `DESIGN_DATA.md`, `TESTS.md`, `native/README.md`, `crates/ddg-agents-host/src/paths.rs`
- **구분**: 본 문서는 자동화 E2E 코드가 아닌 **실제 OS GUI 환경의 수동(사람) 검증 절차**다. 미실행 시 `Unverified`로 남긴다.

---

# 사용자 시나리오: H1 — macOS 실창 (US/ABC 및 한글 두벌식) 멀티 머신 룸 순환·문맥 격리

## 확인할 요구사항과 보안 조건
- `AC1`–`AC4`, `AC5`(비연결 룸 탈출 복귀; 0/1후보 no-op은 `UT-2`/`UT-3` 담당), `AC6`, `A1`–`A5`: `⌘+Option+[` / `]` 양방향 순환, 캐시 룸 보유 비연결·`HostKeyMismatch` 머신 건너뛰기, `룸` 섹션·머신 자동 펼침, Control/Readonly PTY 무유입, 한글 IME 조합·붙여넣기 배너·모달·AgentList·Peek 격리.

## 수행 방식: 사람
## 실제 지원 도구·환경과 필요한 권한
- macOS arm64 로컬 개발 환경, `GHOSTTY_VT_LIB_DIR` 설정, `cargo run -p ddg-agents-native` 실행 권한.
- 전용 테스트 Docker 컨테이너(또는 테스트 원격 SSH 호스트) 2대(1대는 연결 유지, 1대는 연결 후 단절 재현용) + 호스트 키 불일치 테스트 프로파일 1개.

## 초기 데이터·계정·세션 및 병렬 격리
- 사용자 실설정·실호스트를 오염시키지 않도록 임시 루트 아래 `DDG_CONFIG_DIR`, `DDG_STATE_DIR`, `DDG_RUN_DIR` 3종을 모두 전용 경로로 지정해 격리된 로컬 데몬·GUI·CLI를 기동하고, 시작 전 `$DDG_RUN_DIR/host.sock`이 임시 경로인지 확인한다.
- 머신 순서: `로컬`(연결, 룸 A[탭 2 중 2번]·룸 B[좌우 분할 오른쪽 포커스]) → `중단 원격`(최초 연결해 룸 X 수신 확인 후 컨테이너 중지해 비연결 캐시 상태 유도) → `호스트 키 불일치`(룸 미표시 차단) → `정상 원격`(연결, 룸 C[탭 2 중 1번]).

## 수행할 행동
1. **기본 순환·보존 (US/ABC)**: 로컬 룸 B에서 `⌘+Option+]` → `중단 원격`(캐시 룸 X)과 `호스트 키 불일치` 머신을 건너뛰고 정상 원격 룸 C → `⌘+Option+]` → 로컬 룸 A → `⌘+Option+[` → 정상 원격 룸 C 왕복 후 각 룸의 탭·분할 포커스 확인.
2. **비연결 캐시 룸 복귀**: 사이드바에서 `중단 원격`의 캐시 룸 X를 클릭해 연 상태에서 `⌘+Option+]` 입력 → 정상 원격 룸 C 진입, 다시 룸 X 클릭 후 `⌘+Option+[` → 로컬 룸 B 진입 확인.
3. **자동 펼침**: 사이드바 `Rooms` 섹션과 `정상 원격` 머신을 접은 상태에서 로컬 룸 B에서 `⌘+Option+]` 입력.
4. **PTY 무유입 (Control & Readonly)**: 양쪽 룸 터미널에서 raw/no-echo 바이트 카운터(`stty raw -echo; dd bs=1 count=1` 또는 `python3` termios raw 리더) 또는 일반 셸 프롬프트(정성 관측)를 띄우고, Control 및 Readonly 룸에서 `⌘+Option+[`/`]` 단발 및 길게 누르기(key repeat) 수행.
5. **한글 두벌식 & 제외 문맥**: 입력 소스를 한글 두벌식으로 전환 후 ① 조합 없음 상태에서 `⌘+Option+]` ② `ㅎㅏ` 조합 중(밑줄 표시) `⌘+Option+]` ③ 멀티라인 붙여넣기 확인 배너 ④ `⌘+Shift+L`(AgentList) ⑤ Peek ⑥ 룸 이름 변경·머신·에이전트 설정·업데이트 모달·런처에서 입력.
6. **기존 단축키 회귀**: `⌘+Shift+[`/`]`(탭), `⌘+Option+방향키`(분할 포커스), `Option+[`/`]` 단독 입력 확인.

## 기대 화면·상태·저장 결과
- 1–2: 연결·동기화 완료 머신의 룸만 순환, 캐시 룸 보유 비연결 머신 및 `HostKeyMismatch` 머신 건너뜀, 각 룸 탭/분할 포커스 보존.
- 3: `Rooms` 섹션과 `정상 원격` 머신 자동 펼침, 룸 C 하이라이트, `$DDG_CONFIG_DIR/ui.json`의 `collapsed_sections`에서 `"section:rooms"` 제거 확인.
- 4: 정량 보장은 `UT-3` sink 기준이며, 실기 raw 리더(또는 프롬프트 정성 관측)에서도 양쪽 룸에 스마트 따옴표(`“`/`‘`)·제어 바이트 유입 0건.
- 5: ① 정상 전환 ② 조합 중 미전환·조합 유지 ③–⑥ 룸 미전환·기존 문맥 동작 유지.
- 6: 기존 탭·분할·단독 Option 회귀 없음.

## 관측 지점과 성공·실패 기준
- 사이드바 활성 하이라이트·접힘 상태, 탭 바 활성 탭, raw 입력 바이트 수(0바이트) 또는 프롬프트 출력 없음, `$DDG_CONFIG_DIR/ui.json` diff.
- 단 1건이라도 PTY 문자 유입·잘못된 룸 전환·모달/배너 파괴 발생 시 실패.

## 수집할 증거
- 시나리오별 Pass/Fail 표, 키/modifier·소비 여부 및 raw 바이트 0 확인 기록, 임시 `ui.json` diff(터미널·폼 본문 및 민감 정보 미수집).

## 종료 후 정리
- 이번 테스트에서 기동한 `ddg-agents-native` 창과 격리 로컬 데몬·전용 Docker 컨테이너만 중지/제거하고 임시 `DDG_CONFIG_DIR`/`DDG_STATE_DIR`/`DDG_RUN_DIR` 삭제.

## 미검증 항목과 남은 위험
- OS 전역 핫키 앱(Rectangle/Raycast 등) 사용자 커스텀 선점 환경, 독일어 등 비US 물리 배치.

---

# 사용자 시나리오: H2 — Windows / Linux 실창 매핑 및 CLI `Ctrl+]` 비충돌 확인

## 확인할 요구사항과 보안 조건
- `AC1`, `AC4`, `AC6`: `Ctrl+Alt+[` / `]`(왼쪽 `Alt` 기준) 룸 전환, 단축키 유래 PTY 바이트 0, 테스트 DDG 창 내부에서 `ddg session attach <session_id>`의 `Ctrl+]` detach 비충돌, OS/WM 선점 시 클릭 대체 경로.

## 수행 방식: 사람
## 실제 지원 도구·환경과 필요한 권한
- Windows(x86_64) 또는 GUI 가용 Linux(X11/Wayland) 환경에 빌드된 `ddg-agents-native` 및 `ddg` CLI. (가용 환경 부재 시 `Unverified` 유지.)

## 초기 데이터·계정·세션 및 병렬 격리
- 임시 `DDG_CONFIG_DIR`, `DDG_STATE_DIR`, `DDG_RUN_DIR` 3종을 격리 설정하고 룸 2개 이상(룸 A, 룸 B)과 attach 대상 격리 세션 1개를 준비한다. 별도 터미널은 대상 세션 ID 조회 및 생존 확인용으로만 사용한다.

## 수행할 행동
1. `ddg-agents-native` 룸 A 터미널 존(raw/no-echo 입력 카운터 또는 일반 프롬프트)에서 `Ctrl+왼쪽 Alt+[` / `]` 입력.
2. `Ctrl+Shift+[` / `]`(탭 전환) 및 `Ctrl+Alt+방향키`(분할 포커스) 확인.
3. **테스트 DDG GUI 창의 제어 가능한 룸 A 터미널 안에서** `ddg session attach <session_id>` 실행 후 `Ctrl+]` 입력.
4. 별도 터미널에서 `ddg session ls`로 대상 세션 생존 확인.
5. WM/OS가 `Ctrl+Alt+[`/`]`를 선점하는 경우 사이드바 클릭 전환 확인.

## 기대 화면·상태·저장 결과
- 1: 룸 A ↔ 룸 B 전환 성공, 양쪽 PTY 단축키 유래 바이트 0.
- 2: 탭·분할 단축키 정상 동작.
- 3–4: 룸 A 유지(룸 전환 없음), `ddg session attach`가 `0x1d`로 detach되어 룸 A 셸 프롬프트로 복귀, 대상 세션 생존.
- 5: 사이드바 클릭으로 룸 전환 정상 동작.

## 관측 지점과 성공·실패 기준
- 활성 룸 전환 여부, raw 리더 0바이트(또는 프롬프트 무출력), GUI 내부 attach의 프롬프트 복귀 및 `ddg session ls` 생존 상태.

## 수집할 증거
- OS/디스플레이 서버(Windows / X11 / Wayland), 키보드 배치, 항목별 Pass/Fail 또는 `Unverified` 사유 기록.

## 종료 후 정리
- 테스트 창·격리 데몬·임시 세션 및 임시 `DDG_CONFIG_DIR`/`DDG_STATE_DIR`/`DDG_RUN_DIR` 삭제.

## 미검증 항목과 남은 위험
- Linux CI는 `ddg-agents-native`를 빌드·실행하지 않으므로 Linux 실창 미수행 시 해당 플랫폼은 `Unverified`로 남긴다. AltGr 레이아웃의 논리 대괄호 입력은 본 범위 한계로 기록한다.
