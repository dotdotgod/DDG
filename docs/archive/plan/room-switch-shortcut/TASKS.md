# 실행 세션 인계 · room-switch-shortcut

**상태:** 명세 확정, T0–T3 모두 미실행. 저장소 `/Users/dotdot/Workspace/dotdotgod-agents`. [결정 A1–A5·AC1–AC6](README.md), [UI §2.2](DESIGN_UI.md), [데이터·권한 계약](DESIGN_DATA.md), [UT 표·리뷰](TESTS.md), [H1/H2](SCENARIOS.md)가 기준이다. 예전 초안의 `is_connected()` 단독 필터를 쓰지 않는다.

**공통 운영:** 실행 세션은 프로젝트 지침과 아래 역할 정의를 읽는다. 역할 파일 루트는 `/Users/dotdot/.agents/skills/ddg-planning/roles/`. 새 코드·테스트는 T1에서만 작성한다. 기존 작업 트리의 사용자 변경을 되돌리거나 일괄 스테이지하지 않는다. 각 결과에 기준 리비전/작업 트리, 자기 변경, 실제 관측·명령/종료 상태, 미검증·남은 위험·차단 사유를 적는다. 키·상태·카운트만 수집하고 터미널 본문·토큰은 남기지 않는다. 설계와 다른 변경이 필요하면 근거·대안·영향을 Neon과 Astra에 전달한다. 범위 밖 수정이나 성공 기준 완화로 해결하지 않는다.

## T0 · 조사/사전 준비 — 실행 지시 후 조사 가능

- **담당·범위:** Astra (`task-planner.md`) / 읽기 전용. 선행 태스크 없음. 새 유닛 테스트·설치·소스 편집·머지 없음. 결과는 T1 실행 세션에 전달한다.
- **확인:** `pi auth check --model openai/gpt-6.2-sol`로 해당 모델이 준비됐는지 확인한다. `GHOSTTY_VT_LIB_DIR`가 실제 플랫폼/아키텍처에 맞는 Ghostty VT 라이브러리를 가리키고 읽을 수 있는지 `native/README.md`와 대조한다. 인증정보 값은 출력/기록하지 않는다.
- **앵커:** `git status --short`, `git diff`로 기존 변경과 충돌 구간을 분리한다. `native/src/main.rs`의 `Desktop::key`에서 **`KeyRoute::ResetNotice` 처리 직후·`tab_action` 직전**을 재확인한다(설계 당시 6569행 부근; 줄 번호보다 분기가 기준). `terminal_shortcut`의 루트 포커스 검사, 기존 Peek의 `catalog_seq.is_some()`와 `native/src/terminal_input.rs`의 커밋 대상 검사를 읽는다. `native/README.md:32–46` 표의 현재 위치도 확인한다.
- **판정:** 모델·라이브러리·앵커·현재 파일에서 설계 계약이 성립하는지 각각 기록. 누락·기준선 충돌 시 T1만 차단하고 원인과 준비된 항목을 보고한다. 다른 모델로 묵시 대체하거나 설치를 강행하지 않는다.

## T1 · 구현 + 유닛/GPUI 헤드리스 TDD — T0 통과 후

**수행자:** Sol. 새 구현 세션은 **`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`를 먼저 읽고 적용**한다. 접근 불가 시 전달을 요청한다. 지정 세션은 `pi --provider openai --model gpt-6.2-sol --thinking medium`. 오케스트레이션 시 프로젝트의 `hc --skill` 지침을 따른다. 이 문서가 세션 실행 승인은 아니다.

**쓰기:** `native/src/main.rs`(기능과 같은 파일의 테스트), `native/README.md`(표·행동 안내)만. 서버/프로토콜·CLI·VT·설정 스키마·새 의존성·단축키 편집기·자동화 E2E는 제외. 같은 파일을 둘이 나눠 쓰지 않는다. 출발 근거: C–D/E 설계 검수 완료, 경계·증거 수준은 [최종 UT 명세](TESTS.md)에 고정했다.

1. UT를 먼저 작성해 의도된 실패를 확인하고 최소 변경으로 통과시킨다. `RoomNavDir`, `room_nav_action`: macOS Cmd+Option / 비Mac Ctrl+Alt + 정확한 `[`·`]`만. Shift/Fn/다른 primary, 스마트 따옴표 및 중괄호 별칭 금지. `is_held`인 키다운도 한 단계다.
2. `navigable_rooms`, `next_room_target`: 설정 머신 순서 × visible `m.rooms` 순서, `is_connected() && catalog_seq.is_some()`만. 0개 먼저 `None`; 유일 후보 이미 활성은 no-op. 없던 활성 룸은 머신 위치를 기준으로 앞/뒤 룸 있는 머신에 진입; 머신 위치조차 없으면 방향별 첫/끝. 캐시 필드·조회 RPC 없음.
3. `switch_room`: 드래그/대상 없음이면 부작용 없이 반환. **활성 복합 키 갱신 → 즉시 이전 입력/마우스 상태 sync → 접혀 있던 `section:rooms`와 대상 머신 펼침 → 기존 attach/읽음·루트 focus**. 대상의 선택 그룹/패널/확대는 그대로. 설정 경로 없음은 RAM 전용, 실제 저장 실패는 기존 notice(전환 유지). `ResetNotice`, `hovered_room`, `pending_select`는 건드리지 않는다.
4. `Desktop::key`의 T0 앵커에서 `peek.is_none() && terminal_has_keyboard() && focus.is_focused(window)`와 매처로 진입한다. `input_target` 필요 조건 금지(Readonly/빈·차단 머신 출발 허용). `had_marked_ime` 기록 후 pre-sync, 유효 paste/marked는 기존 `terminal_key`로 위임. 성공 또는 허용 no-op/드래그는 소비, 성공 시 notify, no-op에서 stale paste 또는 기존 marked 엔티티가 정리됐으면 notify([UI §2.2](DESIGN_UI.md)). 비활성/미포커스에서 배너 보존을 새로 강제하지 않는다(`F-R1`).
5. `native/README.md:32–46` 기존 탭·패널 매핑 옆에 두 OS의 이전/다음 **룸**과 전체 머신 순환·자동 펼침·적용 문맥·클릭 대체 경로를 설명한다. 서버/세션 이동으로 표현하지 않는다.

**같은 태스크의 필수 테스트:** 아래는 판정 요약이며 [UT-1–UT-5 표](TESTS.md)의 전체 예시·부정 케이스를 실행한다.

| ID | 요구·독립 관측 |
|---|---|
| UT-1 | 양 플랫폼 정확한 매칭/거부, Shift+대괄호·화살표·`Ctrl+]` 비충돌. |
| UT-2 | `Connected/Some[A,B] → 빈/오프라인/차단/Connected-None → Connected/Some[C,D]`: `B→C`, `C→B`, `D→A`, 역순환. BG 제외, 빈/단일/선택 없음 구분. |
| UT-3 | 각 host/channel에 VT·제어 가능한 입력 sink를 준비. 일반 키 양성 대조 후 단축키 유래 양쪽 0바이트 **및 소비**. 왕복·반복·Readonly 유지·탭/패널/확대·자동 펼침, 전환 뒤 구 IME commit 거부, 단일 룸 no-op. |
| UT-4 | 폼/모달/런처/Peek/AgentList·드래그, 유효 paste 대 stale paste, marked IME. 비활성/루트 미포커스는 미전환·단축키 유래 0바이트. 배너 보존은 유효·활성 문맥에서만. |
| UT-5 | RAM 전용/실파일 재로드/결정적 저장 실패, `HostKeyMismatch` 유일 후보 상황에서 선택·attach·펼침 무변경, 삭제 및 재구독 전후 후보 회복. |

**실행/독립 게이트:** T0에서 확인한 `GHOSTTY_VT_LIB_DIR`를 내보낸 동일 환경에서 아래 순서로 실행한다. 기존 테스트를 삭제·완화하지 않는다.

```sh
cargo test --locked -p ddg-agents-native
cargo build -p ddg-terminal-server -p ddg-agents-host
cargo test --workspace
```

Sol은 변경 목록·자기 diff와 **실제 테스트 출력/종료 코드**를 전달한다. Neon은 독립적으로 `git status --short`, `git diff`, 실제 출력, 기존 변경 대비 범위와 A1–A5/UT-1–5를 대조한다. 코드만 있거나 로그가 없으면 완료 아님. 실패·불일치는 T1에서 수정/재검수. 전체 회귀 이상 없음과 Neon 적합 판정 후 같은 검수본을 T2에 전달한다. 비Mac 매처 테스트는 비Mac 실창 증거가 아니다.

## T2-A ∥ T2-B · 검증 — T1 독립 검수 후

**사람 실기**, 설계 책임 Neon의 [SCENARIOS.md](SCENARIOS.md) 준수, Astra 취합. `test-reviewer.md`의 증거 기준을 적용한다. 추가 소스·유닛 테스트는 없음: OS 키 전달/IME/WM은 T1 하네스로 대체할 수 없다. 각 검증자는 **별개 임시 `DDG_CONFIG_DIR`, `DDG_STATE_DIR`, `DDG_RUN_DIR`**, 전용 소켓/원격 호스트/세션만 사용한다. 테스트 전 실제 접속 대상 확인; 실사용 카탈로그 접근 시 중지한다. 쓰기는 격리 환경과 자기 실측 기록뿐. 코드·공용 보고 파일은 병렬 수정하지 않는다.

| 태스크 | 환경·수행 | 판정·산출물 |
|---|---|---|
| **T2-A / H1** | macOS arm64 US/ABC·한글 두벌식. 로컬 A/B와 전용 원격 C, 캐시 X를 먼저 수신한 뒤 끊은 머신, 키 불일치 머신. 양방향 순환·X에서 탈출·자동 펼침과 `ui.json`, Control/Readonly, 반복, IME 조합 전/중/후·paste·모달·기존 탭/패널 키 확인. | H1 항목별 실측 Pass/Fail. 다른 룸 전환·단축키 입력 유입·조합 훼손은 Fail. 화면 무출력은 정성 자료이며 바이트 보장은 UT-3 sink와 분리한다. H1을 실행 못 하면 전체 검증 완료 아님. |
| **T2-B / H2** | Windows 또는 GUI 가용 Linux(X11/Wayland). 룸 A/B에서 Ctrl+왼쪽 Alt 조합(AltGr 일반 문자와 구별), 기존 탭/분할, WM 선점 시 클릭 확인. **DDG 창 제어 터미널 안에서** `ddg session attach <session_id>` → `Ctrl+]`; 별도 터미널은 세션 생존 조회에만 사용. | H2는 룸 불변 detach·프롬프트 복귀·세션 생존, 키 전달·무누수·기존 키 결과를 OS/배치별로 기록. 환경 부재 시 해당 항목 `Unverified`와 사유/잔여 위험을 허용한다(Pass 아님). |

각자 해당 실행에서 만든 창·세션·데몬·경로만 정리한다. 한쪽 실패/미가용은 다른 쪽 조사를 막지 않는다. 실패 재현은 Sol·Neon으로 반환하고, 변경본은 T1 게이트와 영향받은 시나리오를 다시 통과시킨다.

## T3 · 종료 검증/보고·승인 대상 통합 — 결과 수집 후

- **담당:** Astra `completion-reporter.md` / 보고·목차·계획 이관. 코드를 추가하지 않으므로 새 유닛 없음. 입력: T1 로그·Neon 판정, T2-A 실측, T2-B 결과 또는 `Unverified` 근거. 선행 미충족이면 부분 보고·승인 자료 준비만 하고 전체 완료로 표시하지 않는다.
- **문서:** 완료일의 `docs/archive/report/YYYY-MM-DD/room-switch-shortcut/FINAL.md` 및 해당 `README.md` 작성. 실제 결과·리비전·미검증·승인 상태 기재, 기존 `PLANNING.md` 보존. 구현·필수 검증이 끝난 계획만 `docs/archive/plan/room-switch-shortcut/`으로 이동; 활성/아카이브 목차와 계획↔날짜별 보고 링크를 모두 재검증. 실행 결과와 승인 대기를 구분한다.
- **승인 전:** 변경 파일·범위, 병합 대상/방식·원격 목적지, 테스트 증거·잔여 위험·복구 방법을 준비한다. `main`과 공유 원격 반영은 미승인 상태로 대기한다. 계획 보고 또는 테스트 통과는 승인 아님.
- **비가역 단계:** 명시적 사용자 승인과 권한이 있는 대상에 한해 `main` 머지 또는 squash. **`git push`는 원격·브랜치까지 승인 범위에 포함될 때만** 수행(머지 승인만이면 push 금지). 선택 이유는 검수본의 기준선 통합; 미승인이면 작업 브랜치 유지가 대안. 원격 공개는 회수 보장이 없으므로 사고 복구는 revert, 강제 push/이력 재작성 금지. 타 작업 변경을 섞지 않는다.
- **커밋 본문:** 최종 보고서·계획 경로, A1–A5 및 DN-1 선택 이유·대안·범위 변경, 승인 범위, native/workspace·H1/H2 결과(`Unverified` 사유), 남은 위험. archive가 무시돼도 강제 추가하지 않고 본문만으로 판단 가능하게 한다. 실제 커밋·push 상태를 보고하고, 자기 SHA를 넣으려고 커밋을 반복 수정하지 않는다.
