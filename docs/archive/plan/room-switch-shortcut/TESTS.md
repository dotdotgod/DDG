# 테스트 설계 및 독립 리뷰 결과 — 룸 전환 단축키

- **역할**: `test-designer` (Antigravity Gemini 4 Neon) · 독립 리뷰 반영(`test-reviewer`, `security-reviewer`, `review-conductor`, `validation-requirements` 2차)
- **근거**: 확정 요구 `AC1`–`AC6`, `A1`–`A5`, `DESIGN_UI.md`, `DESIGN_DATA.md`
- **원칙**: TDD는 아래 유닛·GPUI 헤드리스 테스트에만 적용하며 자동화 E2E 스위트는 제외한다. 기존 테스트를 삭제·약화하지 않는다.

## 1. 유닛 및 GPUI 헤드리스 테스트 목록

| ID | 하네스 / 관측 지점 | 확인 요구·보안 | 독립 기대값 근거 | 잡는 문제 / 놓치는 문제 |
|---|---|---|---|---|
| `UT-1` | `#[test]` `room_nav_action(key, m, mac)` 표 (`mac = true/false`) | `AC1`, `AC6` | OS별 Primary+`Alt`+`"["`\|`"]"`만 허용; 반대·양쪽 primary, `shift`, `function`, `{}`·`“`/`‘`, `Ctrl+]`/`Ctrl+[`, `Alt+[` 단독, `Primary+Alt+방향키` 모두 `None` | 수정키 오매칭·기존 탭/패널/CLI 키 회귀 차단 / OS IME·레이아웃 번역 미검증 |
| `UT-2` | `#[gpui::test]` `next_room_target` 순환·필터·경계 (`catalog_seq = Some(1)` 설정) | `AC2`, `AC5`, `A5`, 보안 | ① 단일 머신 `[A,B,C]` 순환 ② 멀티 머신 `로컬(Connected/Some)[A,B]` / `빈 머신(Connected/Some)[]` / `재연결중(Connected/None)[R]` / `오프라인(Disconnected)[X]` / `차단(HostKeyMismatch)[Y]` / `인증필요(AuthRequired)[Z]` / `미설치(HostNotInstalled)[W]` / `원격(Connected/Some)[C,D]`에서 `B→]→C`, `C→[→B`, `D→]→A`, `A→[→D` ③ `m.background` 제외 ④ 오프라인→빈→연결 머신 진입(정·역방향), 전부 빈/비연결 시 `None`, 유일 후보가 미선택이면 진입 vs 이미 활성이면 `None` | 머신 경계 오프바이원·비연결/재연결 stale/보안 차단·hidden 유입 차단 / 렌더·키 소비 미검증 |
| `UT-3` | `#[gpui::test]` `VisualTestContext` 왕복 전환·보존·자동 펼침·보안/PTY 0바이트 (`cfg!(target_os = "macos")` 키 분기) | `AC1`–`AC4`, `A3`, 보안 | 기존 `observe_input` 대신 모든 `(host, channel, bytes)`를 수집하는 멀티 머신 `test_input_sink` 사용. 2개 연결 머신(`catalog_seq = Some(1)`)·각 2룸(서로 다른 `selected_group`·분할 포커스·`zoomed = true`)을 모두 `Control`+`channel`+`vt`+`is_running` 무장 후 ① 일반 키 Control sink 도달 양성 대조 ② `section:rooms`·원격 머신 접힘에서 왕복 시 활성 `(host, room)` 변경, 탭·분할·`zoomed` 보존, `section:rooms`·대상 머신만 자동 펼침, 단축키 유래 양쪽 sink **0바이트** 및 이벤트 소비 ③ `is_held: true` 1단계 ④ `Readonly` 대상 전환 후 `ControlMode::Readonly` 유지·0바이트 ⑤ 전환 직후 구 `TerminalInput` 핸들에 `replace_text_in_range(None, "stale", ...)` 호출 시 양쪽 sink 0바이트·`terminal_input` 무효화 ⑥ 유일 활성 룸 no-op 시 `collapsed_sections`·`ui_prefs` 미변경·0바이트 | 라우팅·포커스·자동 펼침·교차 룸 IME 유입·PTY 누수 통합 검증 / 단일 OS 실행 시 반대 OS `Desktop::key`→`terminal_key` 폴스루 경로(`macOS m.platform` 마스킹) 및 실창 OS 키 전달 미검증(비Mac은 `UT-1`+Windows/Linux 러너·`H2` 보완) |
| `UT-4` | `#[gpui::test]` `VisualTestContext` 제외 문맥·포커스 격리 (`window_active = true`, 루트 포커스, `Control` 전제) | `AC4`, `A4`, 보안 | AgentList, Peek, Update/Machine/Rename/AgentSettings 모달, AgentLauncher, 유효 `pending_paste`(차단·배너 보존) vs stale `pending_paste`(정리 후 정상 전환), `terminal_input.marked != ""`(조합 유지·미전환), `pane_drag`(미전환·PTY 0바이트), `!window_active` 및 `!focus.is_focused(window)`(미전환·단축키 유래 PTY 0바이트) | 우선순위 역전·비활성 창 오동작·배너/IME 파괴 차단 / 실제 OS IME 후보창 미검증 |
| `UT-5` | `#[gpui::test]` `ui.json` 영속/실패 · 보안 차단 무부작용 · 카탈로그 삭제/재구독 | `AC3`, `AC5`, 보안, Validation | ① `ui_prefs_path = None` RAM 전용 ② 임시 `ui.json` 정상 저장 후 `UiPrefs::load_or_default`로 `section:rooms`만 복원(`machine.collapsed` 미영속) ③ 일반 파일을 부모 경로로 둔 결정적 저장 실패 시 전환·RAM 펼침 유지 + `notice` 설정 ④ `HostKeyMismatch` 유일/접힌 머신에서 `switch_room` 시 `collapsed`·`active_*`·`attach_request`·`ui_prefs` 불변 ⑤ 카탈로그 삭제·재구독(`Reconnecting`→`Connected/None`→`CatalogSubscribed`) 직후 다음 키 안전 전환 | 저장 오류·보안 차단 부작용·삭제 룸 재선택 방지 / 실파일 권한 외 외부 편집 경합 미검증 |

### 1.1 테스트 제외 근거
- **자동화 E2E GUI 스위트 제외**: GPUI 0.2.2 실창 OS 키 이벤트·한글 IME 조합·WM 선점은 헤드리스로 대체 불가하며 `SCENARIOS.md`(`H1`, `H2`)로 보완한다.
- **타 크레이트 신규 테스트 제외**: 프로토콜·호스트·CLI 변경이 없으므로 기존 워크스페이스 회귀 실행으로 충분하다.

## 2. 실행 명령 및 합격 기준
- **단위·헤드리스 TDD**: `GHOSTTY_VT_LIB_DIR=<xcframework-arch-dir> cargo test --locked -p ddg-agents-native` — `UT-1`–`UT-5` 및 기존 `native` 테스트 전수 통과.
- **워크스페이스 회귀**: `GHOSTTY_VT_LIB_DIR=<xcframework-arch-dir> cargo build -p ddg-terminal-server -p ddg-agents-host && GHOSTTY_VT_LIB_DIR=<xcframework-arch-dir> cargo test --workspace` — 전 크레이트 회귀 0건.
- **실제 환경 전용 검증**: macOS US/ABC·한글 두벌식 조합 전/중/후, Windows 왼쪽 Alt 대 AltGr, Linux X11/Wayland 실창 및 WM 선점 여부 (`SCENARIOS.md`).

## 3. 영역별·보안 리뷰 및 전체 리뷰 결과 (Phase C–E)

독립 Gemini 4 Neon 리뷰어 2기(Phase C–D, Phase E) 및 Astra `validation-requirements` 2차(별도 정적 반례 검토) 결과를 모두 반영했다.

| 출처 | 주요 발견 (계약 위반 / 개선 제안 / 미확인 위험) | 증거 수준 | 조치 및 조정 결과 |
|---|---|---|---|
| `PL-1` · `SEC-1` · Astra P2 | `Desktop::key` 진입 시 `window_active`·`focus.is_focused(window)` 경계 누락 | 소스 확인 | **수정**: `terminal_has_keyboard() && focus.is_focused(window)` 추가 (`main.rs:7301` 재사용), `UT-4`에 검증 추가 |
| `DN-1` · `SEC-2` vs Astra P2 | 재연결 `Connected + catalog_seq: None` 구간 stale 캐시 룸 `attach_tab` 발송 | 소스 확인 | **조정 채택(`DN-1`)**: 기존 Peek 계약(`main.rs:1860`)대로 `is_connected() && catalog_seq.is_some()` 채택, `UT-2`/`UT-5` 반영 |
| `PL-2` | `switch_room` 호출 순서(`sync_terminal_interaction` 선행) 및 no-op stale IME `cx.notify()` | 소스 확인 | **수정**: `DESIGN_UI.md` §2.2 순서 정렬 및 `had_marked_ime` notify 보완 |
| Astra P1 · `F-C3` · `PL-4` | `rooms.is_empty()` 조기 반환, `mi: None` fallback, 줄번호(`6569`, `3457-3466`) 정정 | 소스 확인 | **수정**: `DESIGN_UI.md` · `DESIGN_DATA.md` 반영 |
| `DN-2` · `F-C1` · `PL-3` · `UX-1` | `AuthRequired`/`HostNotInstalled` 정정, ASCII 아트 실제 마크업 일치·단일 머신·`notice` 보완, 차단 화면 탈출 명시 | 소스 확인 | **수정**: `DESIGN_UI.md` · `DESIGN_DATA.md` 반영 |
| `F-T1`–`F-T4` · `F-S1`–`F-S3` · `SEC-4` · `F-C4` | 멀티 머신 `test_input_sink`, `zoomed`/`is_held`/`Readonly` 유지, 지연 IME 커밋 0바이트, `HostKeyMismatch` 무부작용, `UT-5` 정상 영속/재구독, `GHOSTTY_VT_LIB_DIR` 표기 | 소스 확인 | **수정**: `TESTS.md` §1–§2 전면 보강 |
| Astra P1/P2 · `F-T5` | `H1`/`H2` 3종 경로 격리(`CONFIG`/`STATE`/`RUN`), 캐시 룸 생성 후 단절 실기, `ddg session attach` GUI 내부 실행, PTY 정량/정성 관측 구분 | 소스 확인 | **수정**: `SCENARIOS.md` 전면 보강 |
| `SEC-3` · `F-S4` | IME 조합 중 비Mac `terminal_key` 기존 동작 비대칭(미확인 위험) | 소스 확인 | **유지**: 임의 차단 가드 미추가(`AC4`), `DESIGN_DATA.md` §3 한계 기록 |
| `F-R1` (재검토) | `UT-4`의 `!window_active`/`!focus`에서 `pending_paste` 미훼손 단언이 기존 `terminal_key` → `sync_terminal_interaction` 정리 계약과 충돌 | 소스 확인 | **수정**: `!window_active`/`!focus` 기대값을 `미전환·단축키 유래 PTY 0바이트`로 정정 (유효 배너 보존은 활성·포커스 케이스에서 검증). 최종 판정: **진행 가능** |
