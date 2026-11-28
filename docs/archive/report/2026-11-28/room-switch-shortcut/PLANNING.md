# 계획 작성 완료 보고 · 룸 전환 단축키

**보고 대상:** `/ddg-planning` Phase A–G, `room-switch-shortcut`, 2026-11-28(로컬 날짜). **계획 작성 완료. 제품 구현·빌드·테스트·사람 실기·머지·push는 미수행.** ‘진행 가능’은 최종 설계를 실행 세션에 넘길 수 있다는 판정이지 구현 승인이나 테스트 통과가 아니다.

## 대상 범위와 산출물

| 산출물 | 인계 내용 |
|---|---|
| [활성 마스터 계획](../../../plan/room-switch-shortcut/README.md) | 사용자 확정 A1–A5, AC1–AC6, 소유권·범위·선행 조건 |
| [UX·Presentation](../../../plan/room-switch-shortcut/DESIGN_UI.md) / [Data Network·Validation](../../../plan/room-switch-shortcut/DESIGN_DATA.md) | 화면 전이·ASCII, 키 처리/입력 무효화 순서, 재구독·설정·권한 경계 |
| [테스트·리뷰 기록](../../../plan/room-switch-shortcut/TESTS.md) / [사람 시나리오](../../../plan/room-switch-shortcut/SCENARIOS.md) | UT-1–UT-5와 독립 판정 근거, H1/H2 실행 환경·관측·정리 |
| [실행 태스크 명세](../../../plan/room-switch-shortcut/TASKS.md) | T0 조사, T1 구현/TDD·독립 검수, T2-A/B 실기, T3 종료·승인 게이트 |

## 확정 결정·대안과 영향

| 출처 | 선택한 동작 | 제외한 대안과 이유 |
|---|---|---|
| Round 1 · A1 | 현재 **보는 룸**만 전환하고 각 룸의 탭/패널을 보존한다. | 세션 승격/이동, 룸 정렬은 카탈로그 쓰기를 수반하며 사용자의 목적이 아니다. |
| A2 | 머신 설정 순서와 카탈로그 룸 순서를 이어 양방향 순환한다. | 같은 머신에 한정하거나 끝에서 멈추면 키로 다음 머신에 갈 수 없다. 에이전트 핀/정렬은 다른 목록이다. |
| A3 | 접힘을 무시해 후보를 찾고 실제 전환 시 섹션과 대상 머신만 펼친다. | 보이는 행만 순환하면 접힌 머신을 건너뛴다. 머신 펼침은 RAM, `section:rooms`만 기존 선호 저장을 사용한다. |
| A4 | 일반 터미널 존에서 Readonly도 허용. 폼·모달·Peek·AgentList·유효 paste/IME를 침범하지 않는다. | 앱 전역 키는 입력을 가로채고, PTY 제어권을 요구하면 관찰자가 이동할 수 없다. 허용 문맥에서 정확한 Ctrl+Alt 조합만 비Mac PTY보다 우선한다. |
| Round 2 · A5 | 사용자 선택은 **현재 연결된 머신만 방문**. 접속 불능 머신의 캐시된 룸은 지나간다. | Neon 초기안은 목록에 남은 비연결 룸도 방문: 사이드바 순서와 일치하지만 작업 불가 룸에서 멈추므로 채택하지 않았다. |
| 설계 조정 · DN-1 | A5의 구현 경계는 **`is_connected() && catalog_seq.is_some()`**. transport 연결과 첫 snapshot 사이에는 이전 룸이 남고 일반 attach가 실행될 수 있다. 기존 Peek 경계(`native/src/main.rs:1860`) 재사용. | Astra D-2차 대안은 `Connected/None`도 허용하고 한계를 문서화하는 것. `DN-1`/`SEC-2` 검토로 stale attach 방지를 우선했다. 새 RPC·상태 없이 재수신 후 후보에 복귀. 이 조정은 사용자 답변 자체가 아닌 설계 결정이며 DATA·UT-2/5가 최종 기준이다. |

필수 범위를 넓힐 이유는 없다. GPUI 논리키와 기존 클릭·attach·설정 경로로 충족한다. 물리 키코드 훅, 키맵 편집기, OS 전역 등록, 자동 스크롤, 신규 DB/서버 락·프로토콜, 자동화 E2E는 추가하지 않는다. `ui.json` 오류 시 RAM 상태 유지·기존 notice로 충분하며 두 상태를 원자적으로 묶지 않는다.

## 역할·모델·검토 결과

| 단계 | 실제 담당 / 모델 | 수행·증거 수준 |
|---|---|---|
| A·총괄 | Overall Lead: Pi `openai/gpt-6.2-astra`, `high`, `t63j`; Antigravity Gemini 4 Neon 설계 세션과 인계 | 작업 트리·GPUI·룸/입력 경계 조사. Phase A에서 해석한 `active-plan=docs/plan/**`, Markdown 한도 200행/10,000자 사용. |
| B | Astra `t63j` high: `user-requirements` → `data-requirements` → `validation-requirements` | **같은 세션에서 순차** 산출. Round 1 A1–A4, Round 2 A5 답변으로 종료. |
| C–E 설계 | Antigravity Gemini 4 Neon **본 설계 세션**: `design-conductor`, `ux-designer`, `presentation-layer-designer`, `data-network-layer-designer`, `test-designer` | UX·UI·데이터·테스트·실기 4문서 작성·수정. 설계자 자신의 작업을 독립 리뷰로 세지 않는다. |
| C–D 독립 리뷰 | 별도 Neon 서브에이전트 **1** (Antigravity Gemini 4 Neon): `ux-reviewer`, `presentation-layer-reviewer`, `data-network-layer-reviewer`, `security-reviewer` | 수정본 재검증 후 **4역할 모두 통과**(인계 판정). |
| D-2차 | Astra `t63j` high: `validation-requirements` | 같은 Astra 세션의 정적 반례 검토. P1 2건·P2 4건과 작은 정정 반영; DN-1 상충은 위 결정으로 종결. |
| E 독립 리뷰 | 별도 Neon 서브에이전트 **2** (Antigravity Gemini 4 Neon): `test-reviewer`, `security-reviewer`, `review-conductor` | 최초 지적 **12건 해소**, 재검토 `F-R1` 문구까지 `TESTS.md:14`에 반영. 최종 **진행 가능**(인계 결과와 TESTS 리뷰표 대조). |
| F → G | Astra `t63j` high: `task-planner` → `completion-reporter` | 본 세션에서 **순차**로 실행 명세와 마스터 계획을 작성하고 그 결과를 보고. 새 자식 세션 없음. |
| 후속 구현 | 예정: Pi `openai/gpt-6.2-sol`, `medium`, `implementer` | **미실행**. 인증/실행 환경은 T0에서 확인한다. Neon 설계 세션은 후속 실제 diff·테스트 증거도 독립 검수한다. |

**최종본에 적용된 주요 수정:** PL-1/SEC-1은 활성 창/루트 focus 검사를 추가하되 Readonly·빈 머신 출발을 살렸다. PL-2는 활성 키 변경 **직후**, attach 전에 이전 마우스·paste·IME를 정리하며, no-op에도 stale marked가 사라지면 화면을 갱신한다. F-R1은 비활성/미포커스의 UT-4 기대값을 ‘미전환·단축키 유래 PTY 0바이트’로 고쳤다. 유효한 활성 배너를 보존해야 한다는 요구와 비활성화 시 정상 정리를 구분한 것이다. 그 밖에 후보 0개 조기 반환, 세 경로 격리, 실제 GUI 내부 CLI detach, 멀티 머신 입력 sink와 양성 대조를 반영했다. 위 판정은 **이 수정들이 반영된 설계/검증 명세**에 대한 것이다.

## 다음 세션: 작업 순서와 통과 조건

1. **T0:** 실행 착수 지시 후 Sol 모델 인증, `GHOSTTY_VT_LIB_DIR`, 기존 변경과 키 처리 앵커를 확인한다. 이번 세션에서 확인 명령이나 빌드를 대신 실행하지 않았다.
2. **T1:** Sol이 역할 정의와 확정 명세를 읽고 native 코드·UT·안내 표를 함께 변경한다. `cargo test --locked -p ddg-agents-native`와 workspace 회귀의 실제 출력·작업 diff를 Neon이 검수한 뒤에만 실기로 넘긴다.
3. **T2-A ∥ T2-B:** 같은 검수 리비전에서 사람이 macOS H1과 Windows/Linux H2를 분리된 config/state/run·전용 호스트로 검증한다. H2 환경 부재는 사유를 붙여 `Unverified`, Pass로 승격하지 않는다. H1 미수행이나 실제 실패는 전체 검증 완료가 아니다.
4. **T3:** 결과·미검증·승인 상태 취합, 완료일 `FINAL.md`, 완료 계획 이관·양방향 링크와 목차 갱신. T1은 공유 파일이라 직렬, 실기만 독립 병렬이다. 제품 결정 대기는 없으나 실행 일정·사람·장비는 미정이다.

## 비가역 실행·아카이브 기록

- **T3 `main` 머지 및 원격 `git push`: 미승인·미수행, 커밋 없음.** 통합의 이점은 검수본을 공통 기준선에 반영하는 것. 미승인 시에는 별도 작업 브랜치와 검증 자료만 유지한다. 공유 히스토리 변경/원격 전파는 단순 취소 불가하며 `revert`로 후속 복구는 가능하나 이미 배포한 내용을 회수하지는 못한다.
- 대상 변경·브랜치·검증 결과를 제시해 사용자 명시 승인과 권한을 확인한다. `main` 머지 승인을 원격 push 승인으로 확대하지 않는다. 머지/squash 본문에는 보고서·계획 경로, 결정 이유·대안·범위 변경, 승인, 실제 테스트/실기 결과와 잔여 위험을 남긴다. archive가 Git에서 무시되면 강제 추가하지 않는다.
- 이 보고서와 [보고 목차](README.md), [활성 계획 목차](../../../../plan/README.md), [아카이브 목차](../../../README.md)를 연결했다. **지금은** `docs/plan/room-switch-shortcut/`에 둔다. `docs/archive/plan/room-switch-shortcut/` 이동 및 `FINAL.md` 작성은 T3의 일이다.

**문서 검증 증거:** 읽기 전용 Python 검사로 계획 디렉터리 6개·보고 디렉터리 2개·공유 목차 2개, 총 10개 Markdown이 각각 200행/10,000자 이하이고 파일 상대 링크 49개가 모두 존재함을 확인했다. 본 작업의 쓰기는 새 문서 4개와 기존 목차 2개뿐이다. 작업 전부터 dirty였고 검증 중 다른 경로의 소스 변경도 관측되어 전체 `git status`가 문서만 표시하는 상태는 아니다. 이 변경은 포함·복원하지 않았다.

**잔여 위험:** 실제 키 도달·IME·AltGr·WM 선점·GUI 포커스·PTY 무유입은 미검증이다. 비Mac 매처 통과만으로 실기 통과라 하지 않는다. 기존 미커밋 변경이 많아 T0에서 출발 상태를 보존해야 한다. 이번 단계에서 검증한 것은 문서의 범위·크기·링크이며 다음 착수점은 T0다.
