# 최종 실행 보고 · 이전/다음 룸 전환 단축키

**2026-11-28 | 실행 결과:** T0 사전 확인, T1 구현·독립 리뷰, T2-A macOS 실창 검증을 마쳤다. macOS 한글 조합 중 동작 한 건은 원래 기대와 달랐고 사용자가 수용했다. **T2-B Windows/Linux 실창은 `Unverified`다.** T3 문서 이관은 통합 승인이 아니다. **`main` 커밋·머지, 원격 push 모두 미승인·미수행.**

[보관 계획](../../../plan/room-switch-shortcut/README.md) · [태스크·승인 조건](../../../plan/room-switch-shortcut/TASKS.md) · [계획 당시 보고](PLANNING.md) · [보고 목차](README.md)

## 실행한 범위와 근거

기준은 로컬 `main`의 `87c816640872bdaf3dde34c501472e4d449cf5c2`. **검수본은 이 SHA 자체가 아니다.** T0 전에 이미 추적 경로 52개와 기본 status 기준 미추적 항목 12개가 변경돼 있었다. 아래 증감은 HEAD가 아니라 T0 작업 파일 스냅샷과 비교한 **T1만의 diff**다.

| 단계 | 증거와 판정 |
|---|---|
| T0 · Astra | Sol 인증, Darwin arm64 Ghostty VT 라이브러리, `ResetNotice` 직후·`tab_action` 직전 키 처리 위치, 루트 포커스·카탈로그·IME 입력 대상 검사 및 README 표 확인. 기존 변경과 기준 파일 해시 보존. **PASS**. |
| T1 · Sol TDD | `native/src/main.rs` **+912/−0**(테스트·픽스처 789행 포함), `native/README.md` **+5/−0**(키 표·클릭 대안). 기존 테스트 삭제·완화 없음. 먼저 테스트 하네스 컴파일 오류, 이후 전환·소비 단언 4건의 실제 실패를 확인했다. 페인트 이후 새 입력 핸들러를 보던 단언은 dispatch 직후로 바로잡고, 구 핸들러 커밋이 양쪽 입력 sink에 도달하지 않는지도 확인했다. |
| T1 · Neon 독립 검수 | T0 스냅샷 대비 두 파일 외 변경 없음, A1–A5·DN-1 및 UT-1–UT-5(새 테스트 6개)의 diff·실제 로그 확인. 키 충돌, 머신/룸 순서·무효 후보, 양쪽 입력과 소비, 제외 문맥·오래된 IME/paste, 저장 실패·재구독 경로 포함. **PASS**. |
| 최종 자동 검증 | `cargo test --locked -p ddg-agents-native`: **101 passed**, 실패·ignored 0, exit 0. `cargo build -p ddg-terminal-server -p ddg-agents-host`: exit 0. `cargo test --workspace`: **371 top-level passed**, 실패 0, 기존 ignored 1, exit 0(자기 재실행 테스트의 중복 출력 제외). |
| 기존 미실행 테스트 | `live_docker_exec_sshd_control_takeover_and_restart_restore`는 `ddg-agents-host:test` 이미지가 필요한 기존 ignored 테스트로 별도 활성화하지 않았다. H1에서 Docker를 사용한 사실은 이 테스트의 통과 증거가 아니다. |

T0 → T1 독립 검수 뒤 동일 검수본에서 H1/H2를 별개 환경으로 확인하는 계획이었다. H1은 실제 프로세스에 이벤트를 주입해 확인했고 H2는 환경이 없어 실행하지 않았다. T3는 당시 로그와 결과를 대조한 문서 작업이며 테스트를 다시 돌린 단계가 아니다.

## 플랫폼별 실측과 사용자 결정

H1은 격리한 macOS arm64 Aqua 앱(`ddg-agents-native` SHA-256 `dce42bad442a5f388e5e9a55f100eb6b8b564519c4d22e829b1f2da671ee97bd`), 별도 config/state/run·소켓, 전용 컨테이너 2개와 키 불일치 호스트에서 수행했다. CGEvent를 보내 실제 ABC/두벌식 입력기, 7개 raw/no-echo PTY, 화면 및 실제 `ui.json`을 관측했다.

| 시나리오 | 결과 |
|---|---|
| H1-1 / H1-2 | **PASS.** `B(BRight) → C(Tab1) → D(Readonly) → A(Tab2)` 및 역순환. 끊긴 머신·호스트 키 불일치 머신은 목적지에서 제외했고, 끊긴 캐시 X에서 다음은 C, 이전은 B였다. 일반 입력은 예상 PTY에서만 관측됐다. |
| H1-3 / H1-4 | **PASS.** 대상 머신·Rooms 섹션이 펼쳐지고 `ui.json`의 `section:rooms`가 제거됐다. Control/Readonly에서 단발·길게 누른 반복(정·역 각 4회) 모두 7개 PTY에서 **단축키 유래 0바이트**. Readonly에서 일반 입력도 차단됐다. |
| H1-5 ①, ③–⑥ / H1-6 | **PASS.** 두벌식 비조합 상태에서 정상 전환. 유효 paste 확인·AgentList·Peek·Launcher·설정 모달에서 룸 불변. 기존 탭 키·분할 포커스 키 정상, 단독 `Option+[`는 전환하지 않고 기존 터미널 입력으로 전달됐다. |
| H1-5 ② | **원안과 다른 관측, 사용자 수용(2026-11-28, 옵션 1).** 한글 두벌식 조합 중 `⌘+Option+]`를 누르면 GPUI 0.2.2의 macOS `platform/mac/window.rs:1696–1734`에서 AppKit의 `[inputContext handleEvent:]`가 `Desktop::key`보다 먼저 실행된다. IME가 조합을 **출발 PTY `local_A2`에 확정**하고 `marked`를 지운 뒤 룸이 전환됐다. 목적지로 넘어간 입력·단축키 유래 입력은 0바이트. 기존 `⌘+Shift+[` 탭 전환과 같았다. **밑줄 유지·룸 불변을 확인한 것이 아니다.** 앱에 marked가 남아 있을 때 기존 입력기로 위임하는 UT-4와도 구분한다. 이 관측을 기록하고 플랫폼 키 처리 변경은 하지 않기로 했다. |
| H2 · Windows/Linux | **`Unverified`(2026-11-28 사용자 수용).** Windows 또는 X11/Wayland GUI가 없어 실제 `Ctrl+왼쪽 Alt+[`/`]` 전달·OS/WM 선점, AltGr 문자 입력, DDG 창 안 `ddg session attach` 후 `Ctrl+]` detach(룸 불변·프롬프트 복귀·세션 생존)를 확인하지 못했다. UT-1의 비Mac 매처 통과는 실창 결과가 아니다. 키가 도달하지 않으면 사이드바 클릭이 대안이다. |

실측 후 전용 프로세스·컨테이너·임시 디렉터리를 정리하고 입력기·클립보드를 복원했다. 원본 대조 자료: 실행 세션 scratch(`326b8a1f-7642-4609-8648-51a30d463fd4`)의 `t0_report.md`, `t1_handoff.md`, `t1_test_logs.txt`, `t1_main.diff`, `t1_readme.diff`, `neon_t1_review.md`, `t2a_h1_live_report.md`.

## 승인 요청: 무엇을 어디에 반영할 것인가

1. **범위:** 제품은 위 두 T1 diff만. 문서는 이 기능의 계획 6개를 `docs/plan/room-switch-shortcut/`에서 `docs/archive/plan/room-switch-shortcut/`로 이관하고, 해당 보고 3개와 활성·아카이브 목차의 해당 항목만 포함한다. `PLANNING.md`는 과거 보고로 보존하며 이관 대상 6개 링크의 목적지만 수정했다. archive가 무시되는 환경에서는 강제 추가하지 않는다.
2. **대상과 방법(제안, 미실행):** 명시 승인·권한 확인 후 깨끗한 별도 작업 트리에서 두 T1 패치를 기준 스냅샷과 대조해 기능 단위로 분리한다. **T1 뒤 동시 작업이 `native/src/main.rs`를 더 수정했고 README도 T1 해시와 다르다. 현재 파일 전체 또는 HEAD 대비 전체 diff를 커밋하지 않는다.** 공용 목차도 해당 문단만 본다. 기준선의 미커밋 선행 코드가 필요하거나 충돌하면 중지하고 의존 범위를 재검토한다. 타 작업을 딸려 넣지 않는다. 분리본에서 diff 재검수와 T1 검증을 반복하고 동작 변경 시 해당 H1도 재검증한 뒤, 승인된 단일 squash 커밋으로 **로컬 `main`**에 반영하는 안이다. 미승인 시 검수 자료만 유지한다.
3. **원격은 별도 결정:** 목적지는 `origin` = `git@github.com:dotdotgod/DDG.git`, `refs/heads/main`. 로컬 `main` 통합 승인은 원격 반영 승인이 아니다. 원격·브랜치와 push 권한까지 명시 승인되고 fast-forward 가능한 경우에만 일반 push. 기준선이 달라졌으면 먼저 재검토한다.
4. **복구:** 통합 전이면 분리본 작업만 중단하고 기존 dirty 트리는 보존한다. 통합 후 문제가 있으면 해당 단일 커밋을 `git revert <통합 SHA>`로 되돌려 검증한다. 공유 원격 반영은 별도 승인된 일반 push로만 한다. 이미 가져간 복제본·배포물은 회수할 수 없다. 강제 push·reset·이력 재작성은 사용하지 않는다.

### 제안 커밋 메시지 (승인·검증 후 실제 승인 범위를 기입)

```text
feat(native): 키보드로 이전·다음 룸 보기

A1: 세션 이동·재정렬이 아니라 보는 룸만 바꿔 기존 탭·분할·확대를 보존한다.
A2: 한 머신/끝에서 멈추지 않고 설정 머신 순서 × 카탈로그 룸 순서로 양방향 순환한다.
A3: 접힌 행도 방문할 수 있게 실제 전환 때만 Rooms와 대상 머신을 펼친다.
A4: 전역 훅이나 PTY 제어권 요구 대신 활성 창·루트 포커스·일반 터미널에만 적용한다.
Readonly도 이동 가능하며 폼·모달·Peek·AgentList·유효 paste/marked 입력은 침범하지 않는다.
macOS는 Cmd+Option, Windows/Linux는 Ctrl+Alt와 대괄호만 사용한다.
A5: 방문해도 작업할 수 없는 비연결 캐시는 건너뛴다. DN-1: Connected 단독으로는
이전 카탈로그에 attach할 수 있어 기존 Peek처럼 이번 연결의 catalog_seq도 요구한다.
새 RPC·서버 변경, 키맵 편집기는 없다. 키 미전달 시 사이드바를 클릭할 수 있다.

T0 main 87c816640872bdaf3dde34c501472e4d449cf5c2의 dirty 기준선 대비
native/src/main.rs +912(테스트 포함), native/README.md +5. 타 작업은 제외한다.
Sol TDD·Neon 독립 PASS. native 101 통과, 두 데몬 빌드 성공, workspace 371 통과,
기존 Docker/SSHD 1 ignored. H1 macOS 실창: 순환·제외·펼침·기존 키 확인,
7개 PTY에서 단축키 유래 0바이트. H1-5② 두벌식 조합은 AppKit이 출발 PTY에 먼저
확정한 뒤 전환한다(기존 탭과 같음). 조합 유지가 아니며 목적지 유출 없음.
사용자(2026-11-28)는 이 동작과 H2 Unverified 기록을 수용했다. 비Mac 매처만 통과;
GUI 부재로 OS/WM 선점·AltGr·DDG 창 안 Ctrl+] detach는 미검증이다.

보고: docs/archive/report/2026-11-28/room-switch-shortcut/FINAL.md
계획: docs/archive/plan/room-switch-shortcut/README.md
통합 승인: <별도 명시 승인 일시·로컬 main 범위>. 원격 push: <별도 승인 없으면 미승인>.
분리본 재검증: <실제 결과>. 복구는 후속 revert로 하며 이력을 재작성하지 않는다.
```
