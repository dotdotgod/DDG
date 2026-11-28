# 설계 및 리뷰: `BASE_UI_SCALE` 제거 및 전 기준 크기 `round(× 0.85)` 정수화

작성·리뷰: Antigravity Gemini 4 Neon (`high`, `design-conductor` · `ux/presentation/data-network-designer` · 역할 분리 리뷰). 같은 세션 내 설계·리뷰이므로 독립 검증으로 표기하지 않는다. Astra 별도 컨텍스트 반례 R1–R7을 반영해 정정·재검토했다.

## 1. UX 설계 (`ux-designer`)

- **진입·정상 흐름:** 시작 시 `~/.config/ddg/ui.json`의 `ui_scale_pct`(없으면 `100`)를 적용한다. 시작 시 `100%`이면 숨은 배율 없이 `1.0x`(기준 `rem 14px`, 터미널 `12px`, 탭 바 `28px`, 초기 사이드바 `190px`)로 렌더링되며 탭 바 상시 줌 배지가 표시되지 않는다.
- **확대·축소·리셋 피드백 (R2 반영):** `⌘++`/`⌘+-`/`⌘+0`(Windows/Linux `Ctrl++`/`Ctrl+-`/`Ctrl+0`) 또는 탭 바 배율 배지 클릭으로 `50%..=200%`를 `10%p` 단위로 조절한다. 리셋 직후에는 `apply_ui_scale`의 일시 배지(`100%`)가 약 1.5초 표시된 뒤 사라진다. 실제 값이 바뀐 경우에만 `ui.json`에 저장하며, 이미 `100%`에서 재리셋할 때는 **파일 쓰기가 발생하지 않는다**(피드백용 UI 갱신·일시 배지는 기존 동작 유지).
- **기존 `ui.json` 보존:** 기존 v2 파일에 `90` 등 100 외 값이 저장돼 있으면 시작 시 자동 덮어쓰기 없이 저장된 퍼센트(`0.9x`)로 열린다. 이미 `100`인 사용자는 리셋할 필요가 없고, `100` 외 값 사용자만 원할 때 `⌘+0`/`Ctrl+0`으로 전환한다.
- **오류·복구·좁은 창:** 시작 시 `ui.json` 손상/미존재면 메모리 기본값(`100%`, 자동 repair-write 금지). 좁은 창에서는 사이드바 `55%` 뷰포트 상한이 최소 폭(`136px`)보다 우선하고, 모달은 `max_w_full`/`max_h_full`로 스크롤·닫기를 유지한다.

## 2. Presentation Layer 설계와 ASCII 아트 (`presentation-layer-designer`)

```text
+--------------------+--+------------------------------------------------------+
| Ddg (icon 14, g 9) |  | [Update Banner: 27px, 조건부 — 우측 컬럼 최상단]     |
|--------------------|  +------------------------------------------------------+
| 에이전트    [+11]  |  | [1 [12px] Pi ×(9/box17)] [2 [12px] Agy]  [+11/box17] | <- Tab Bar (2rem=28px)
|  > [12] Antigravity|D +------------------------------------------------------+
|    (indent 3+d*9)  |I | [Reset / Old-Host Banner: 27px, 조건부]              |
|--------------------|V | [Peek Banner: 49px, 조건부]                          |
| 룸          [+12]  |5 +---------------------------+--+-----------------------+
|  * dotdotgod       |p | [Split Header: 19px]      |P | [Split Header: 19px]  |
|--------------------|x +---------------------------+D +-----------------------+
| [v0.x (상태 시 ●)] |  | Terminal Body (12px)      |3 | Terminal Body (12px)  |
| (>=170 text/< dot) |  | grid = font_grid(12px)    |p | cursor = 2px @ -3px   |
| [서버12][설정12*]  |  | selection / IME preedit   |x | scroll thumb / badge  |
+--------------------+--+---------------------------+--+-----------------------+
  Sidebar: 136..357px (init 190, compact <=157, pad >=20)
  *Settings label (R1): <=157 아이콘만 | 158..=204 "Agents" | >204 "Agent Settings"
  Overlays (1x scale): Update 442px (독립) | Machine 391px | Launcher 429px(h234)
                       AgentSettings 544px(sel 111px) | Confirm 306px | Tooltip max_w 306px (독립)
```

- **우측 컬럼 수직 스택 계약 (R3 반영):** 우측 컬럼은 위에서 아래로 `update_banner(27) → tab_bar(2rem=28, 90% 스냅 25) → reset/old_host(27) → peek_banner(49) → terminal canvas` 순서다. 따라서 `tab_bar.bottom`에는 이미 `update_banner`가 포함되며, `canvas.y`는 `tab_bar.bottom + reset/old_host + peek`와 일치한다(이중 합산 금지). 분할 시 실제 터미널 body는 `canvas`에서 `PANE_HEADER_HEIGHT(19)`와 `PANE_INSET(1)`을 제외한 영역이다.
- **상태 소유권 & 생명주기 (R2 반영):** `Desktop.ui_scale: UiScale`이 단일 원본이다. `apply_ui_scale`이 `window.set_rem_size(px(scale.rem_px()))`, `font_grid(window, scale)` 재계산, 마우스·드래그 정리를 수행한다. `terminal_canvas::prepare`가 `(grid, window.scale_factor())` 키로 lazy 재생성하므로 별도 강제 전체 캐시 flush를 추가하지 않는다.
- **단일 스케일 계약 (R1 반영, 이중 축소 금지):**
  - `UiScale::factor(self) = f32::from(self.0) / 100.0` (`BASE_UI_SCALE` 삭제). `BASE_REM_PX = 14.0`, `BASE_TERM_FONT_PX = 12.0`.
  - `overlay_panel` 내부(`main.rs:4719`)에서 `self.ui_scale.px(width)`를 단 한 번 적용: Launcher 호출부의 사전 스케일을 제거해 `429.` 전달, AgentSettings 일반·복구 두 분기 모두 `544.` 전달, 선택기 `w(px(self.ui_scale.px(111.)))`.
  - 독립 렌더 경로: `update_modal` 본체 `w(px(self.ui_scale.px(442.)))`, `update_sidebar_row` 분기점은 이미 `self.ui_scale.px(...)`이므로 피연산자만 `200.→170.` 교체(이중 스케일 금지), `AgentTooltip`은 `window.rem_size()` 비율 `round(306 × rem / BASE_REM_PX)`로 `max_w` 계산(상태 추가 없음).
  - 사이드바 분기점: `SIDEBAR_COMPACT_MAX = 157.`(`<=`/`>` 유지), 설정 라벨 중간 분기 `240.→204.`(`main.rs:7680` 기존 영문 문구 `"Agents"`/`"Agent Settings"` 보존), `PANE_HEADER_HEIGHT = 19.`, `PEEK_BANNER_HEIGHT = 49.`.
  - `font_grid` 측정 폴백(`main.rs:7859`)은 `scale.term_font_px() * (8.43 / 14.0)` 보정 비율 고정. `terminal_canvas::FontGrid::default()`는 `(7., 14., super::BASE_TERM_FONT_PX)`.

## 3. Data Network Layer 설계 (`data-network-layer-designer`)

- **DB·네트워크·프로토콜 변경 없음 (제외 근거):** UI 배율은 로컬 뷰어 전용 표시 설정이며 원격 호스트·`ddg-terminal-server`·RPC 스키마에 전달되지 않는다(창 크기 변경 결과만 기존 PTY `Resize`로 전달).
- **로컬 저장 계약 (`~/.config/ddg/ui.json`, v2 유지):** `UiPrefs { schema_version: 2, ui_scale_pct: u16, agent_sort, pinned_agents, manual_agent_order, collapsed_sections }`. 퍼센트와 타 v2 필드만 유지.
- **필수 Validation 및 실패 분류 (R6 반영):**
  - `UiPrefs::load`: regular file, Unix `O_NONBLOCK`, `≤ 64 KiB` (`65,536` bytes), schema v2, `u16` percent → `UiScale::from_percent`(`50..=200` clamp 후 10단위 반올림).
  - 실패 분류: **시작 시 오류/미존재** → 기본값(`100%`, 자동 저장 없음) / **감시 중 invalid** → 마지막 정상 스냅샷 유지 / **감시 중 파일 삭제(`NotFound`)** → 기본값(`100%`) 복귀.
  - 내부 위젯의 중복 검증·추가 락·마이그레이션은 넣지 않는다.

## 4. 적용 범위 · 알려진 한계 · 재검토 조건

- **적용 범위:** `native/` 기준 크기·테스트·`native/README.md`. CLI/서버/호스트/SVG 원본/기존 미커밋 작업(`terminal_{canvas,interaction,input}.rs`, `antigravity.svg`) 제외·보존.
- **알려진 한계:** `50%`에서 `rem_px = 7.0`(`text_xs = 5.25px`)인 반면 `term_font_px`만 `7.0px` 하한에 고정된다. 닫기 버튼은 `100%` 기준 `17px` 박스(`9px` 글리프), `50%` 기준 `9px` 박스다. 비-Retina(1x) 한글 `10.5px` 가독성은 실기 전 미검증.
- **재검토 조건:** 실창 검수(`US-01`)에서 `50%` 버튼 조작성 또는 1x 모니터 한글 보조 텍스트 가독성 문제가 관측되면 별도 작업으로 재검토한다.

## 5. 영역별 · 보안 · 전체 리뷰 결과 (`review-conductor`, R1–R7 반영 갱신)

> 동일 Neon 세션의 역할 분리 재검토 결과(직접 소스 확인 기준)이며 독립 리뷰로 표기하지 않는다.

| 리뷰 역할 | 반영한 반례 | 최종 판정 및 조치 |
|---|---|---|
| `ux-reviewer` | R2, R4 | 시작 100% 무배지와 리셋 1.5초 피드백 분리, 이미 100인 사용자 조건부 안내, `update_sidebar_row` 비-Idle 상태 조건 명시. |
| `presentation-layer-reviewer` | R1, R2, R3, R7 | `240→204` 추가, `update_modal.rs:476` 피연산자만 `170` 교체, 우측 컬럼 수직 스택 ASCII·`canvas.y` 공식 정정, `PaneCache` lazy 키 유지(불필요 flush 제거), `overlay_panel`과 독립 렌더러(update/tooltip) 구분. |
| `data-network-layer-reviewer` | R6 | 시작 오류/live invalid/live `NotFound` 3분기 정정. v2 저장·원자 교체·무락 계약 유지. |
| `security-reviewer` | R5, R6 | `64 KiB` 경계 테스트(UT-2) 및 실창 4경로(`CONFIG/STATE/RUN/PLUGINS`)·`umask 077` 격리(US-01)를 `TESTS.md`에 반영. |
| `review-conductor` | R1–R7 전체 | `DESIGN.md`·`TESTS.md`·`TASKS.md` 수치·기하·격리 계약 일치 확인. 미결정 사용자 질문 0건. |
