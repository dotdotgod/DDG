# 테스트 및 사용자 시나리오 설계: `ui-base-scale-85`

작성·리뷰: Antigravity Gemini 4 Neon (`high`, `test-designer` · `test-reviewer` · `security-reviewer` · `review-conductor`). TDD는 선정된 유닛·헤드리스 GPUI 테스트에만 적용하며 유지용 자동화 E2E 스위트는 제외한다. Astra 반례 R1–R7을 반영해 갱신했다. 계획 단계에서는 테스트를 실행하지 않는다.

## 1. 유닛 및 헤드리스 GPUI 테스트 설계 (`test-designer`)

기대값은 구현 식을 복사하지 않고 확정 명세(`factor = pct/100`, `rem = 14`, `term = 12`, `PANE_DIVIDER = 3`, `sidebar_base = 190`, `PANE_HEADER_HEIGHT = 19`)에서 독립 산출한다.

| ID | 대상 파일 · 테스트 | 관측 지점 · 독립 기대값 근거 | 잡는 문제 / 놓치는 문제 |
|---|---|---|---|
| **UT-1** | `native/src/ui_scale.rs::tests::scales_logical_pixels_and_type` | `50%`→`(0.5, 2, 7.0, 7.0)`, `90%`→`(0.9, 3, 12.6, 10.8)`, `100%`→`(1.0, 3, 14.0, 12.0)`, `150%`→`(1.5, 5, 21.0, 18.0)`, `200%`→`(2.0, 6, 28.0, 24.0)`, `px(0.1)==1.0`. | `BASE_UI_SCALE` 잔존·반올림 오류·50% 폰트 하한 누락 포착. 실 OS 폰트·가독성은 미검증. |
| **UT-2** | `native/src/ui_scale.rs::tests::{clamps_and_snaps_percent, steps_without_drift_*, prefs_*}` | 기존 `50..=200` 클램프·10%p 스냅·무드리프트·Unix `0600`/`0700` 회귀 유지 + **(R6) 유효 v2 JSON 공백 패딩 `65,536 bytes` load 성공 / `65,537 bytes` `Err` 경계 추가**. | 64 KiB 경계·v2 호환성·권한 회귀 포착. |
| **UT-3** | `native/src/layout.rs::tests::nested_layout_accounts_for_origins_rounding_and_dividers` | 입력 `(10,20,205,104)` + `PANE_DIVIDER=3` → 패인 `(10,20,71,104)`, `(84,20,131,61)`, `(84,84,131,40)`, hit `(81,20,3,104)`(가용 202)·`(84,81,131,3)`(가용 101). | 분할선 두께 불일치·1px 오차 포착. |
| **UT-4** | `native/src/main.rs` `scaled_layout_keeps_sidebar_and_split_dividers_in_pointer_coordinates` | 초기 폭 `190` 기준 `[50,100,150,200%]` → `[95,190,285,380]`, 120px 창 `66px` 55% 상한, 역변환 드래그 `/factor`, 7500bp 히트 일치. | 사이드바/분할선 포인터 좌표 어긋남 포착. |
| **UT-5** | `native/src/main.rs` `peek_layout_and_shortcuts`, `ui_zoom_resizes_the_tab_bar_and_sidebar`, `update_banner_and_reset_notice_both_offset_the_canvas` | **(R3) 우측 컬럼 비이중 합산 검증:** `tab-bar` 높이 `100/150/90%`→`28/42/25` 스냅; `100%` update(27)+tab(28)+reset(27) 시 `tab.bottom=55`, `canvas.y=82`(109 아님); Peek 시 `+49`; 분할 헤더 `50/100/200%`→`10/19/38`, 분할 시 body는 헤더·인셋 제외 bounds. | 배너 이중 합산·90% 스냅 회귀·분할 body 원점 오차 포착. |
| **UT-6** | `native/src/main.rs` (신규/확장) 모달·툴팁·폴백 비율 테스트 | **(R7)** 넓은 viewport에서 (a) `overlay_panel` 단일 스케일(Launcher `429→644`, AgentSettings 정상·복구 `544→816`, 선택기 `111→167`), (b) 독립 렌더 경로 Update 모달 `442→663` 및 **긴 줄바꿈 텍스트 `AgentTooltip` `max_w` `306→459`** (`100%`/`150%`), (c) `font_grid` 측정 성공과 **폴백 분기** 분리 검증(`12px→7.226`, `7px→4.215`, 허용오차 `0.001`). | 이중 스케일링·미스케일·`BASE_TERM_FONT_PX` 변경 시 폴백 열 폭 왜곡 포착. |
| **UT-7** | `native/src/terminal_canvas.rs::tests::*`, `native/src/icons.rs::antigravity_resvg_rasters_keep_the_mask_and_color`, `config_watch.rs::tests::*` | `FontGrid::default()` `(7,14,12)` 및 `12×3` 뷰포트 `84×42` oracle 일치; Antigravity 래스터 `[12,24]` 추가(기존 `14,16,28,32`·decode 48 유지); 일회용 v2 `ui.json`(`90` + 비기본 sort/pins/manual/collapsed) 로드→`apply_zoom_action(Reset)`→재로드 시 타 필드 보존·재리셋 무쓰기·live invalid/NotFound 기존 검증 유지. | 캔버스 기본 그리드·12px 아이콘 마스크·`ui.json` 필드 유실·watcher 분류 회귀 포착. |

### 제외 근거
- **유지용 자동화 GUI E2E 스위트 제외:** 실창 폰트/Retina·1x 가독성은 헤드리스로 대체 불가하므로 수동 시나리오 `US-01`로 분리한다. 임의 mock·새 추상화는 추가하지 않는다.

## 2. 실제 환경에서만 확인할 조건

- macOS Retina(2x) 및 가능한 경우 비-Retina(1x) 실창에서 `12px` 한글/영문 모노스페이스, `10.5px` 보조 텍스트(`text_xs`), `12px` Antigravity 아이콘, `17px` 닫기 버튼(`9px` 글리프; `50%`에서 `9px` 박스)의 실제 가독성과 클릭 조작성.
- 실제 `MesloLGS NF` / `Menlo` 폰트 측정, 한글 IME 조합·후보창 위치, 분할 패인 마지막 행 클리핑 여부.

## 3. 사용자 시나리오: 실창 배율 · 레이아웃 · `ui.json` 격리 검증 (`US-01`, R1–R5 반영)

### 확인할 요구사항과 보안 조건
- `100% = 1.0x` 기준 크기(`rem 14`, `term 12`, 탭 바 `28`, 사이드바 `190`, 헤더 `19`, 배너 `27`/`49`) 및 `50%..=200%` 정렬.
- 완전 격리된 임시 환경에서만 실행해 실제 사용자 설정·`host.sock`·`catalog.json`·원격 세션을 절대 건드리지 않는다.

### 수행 방식: 사람 (또는 GUI 실창 제어 가능한 검증자)
### 실제 지원 도구·환경과 필요한 권한
- macOS(및 가능한 Linux/Windows), `GHOSTTY_VT_LIB_DIR` 설정된 `release` 빌드(`ddg-agents-native`, `ddg-agents-host`, `ddg-terminal-server`). 실제 업데이트 적용/롤백/릴리스 금지.

### 초기 데이터·계정·세션 및 병렬 격리 (R5 반영)
- `umask 077` 후 짧은 전용 임시 루트 `T=$(mktemp -d)` 아래 4개 경로를 모두 생성·**export**:
  `DDG_CONFIG_DIR="$T/cfg"`, `DDG_STATE_DIR="$T/state"`, `DDG_RUN_DIR="$T/run"`, `DDG_PLUGINS_DIR="$T/plugins"`.
- `$DDG_CONFIG_DIR/update.json`에 `{"auto_check":"off"}` 작성, `$DDG_CONFIG_DIR/ui.json`에 `{"schema_version":2,"ui_scale_pct":90,"agent_sort":"manual","pinned_agents":["local:t1"],"manual_agent_order":["local:t1"],"collapsed_sections":["section:rooms"]}` 작성.
- 기본 로컬 소켓·카탈로그가 `$T` 아래로 격리된 것을 확인한 뒤에만 `target/release/ddg-agents-native`를 실행한다(격리 불확실 시 차단).

### 수행할 행동 (R1·R2·R4 반영)
1. 앱 실행 후 초기 `90%` 배지 표시 확인 → `⌘+0`(Windows/Linux `Ctrl+0`)으로 `100%` 리셋 → 리셋 직후 일시 `100%` 배지가 약 1.5초 뒤 사라지는지 확인.
2. 다시 `⌘+0` 입력 후 `ui.json` 바이트·mtime·권한(`0600`) 불변 확인, 앱 재시작 시 `100%` 및 타 v2 필드 유지 확인.
3. `50%`·`90%`·`100%`·`150%`·`200%` 및 좁은/넓은 창에서 탭 바·배너·분할 헤더·모달(업데이트/머신/런처/에이전트 설정/복구 카드)·긴 툴팁 닫기·스크롤 확인.
4. **반드시 `100%`로 복귀한 뒤** 사이드바를 `156/157/158px`(compact 경계), `203/204/205px`(`"Agents"` ↔ `"Agent Settings"` 경계), 그리고 표시 가능한 비-Idle 업데이트 상태가 있을 때 `169/170/171px`(`● <status>` ↔ `●` 경계; 평상시 UpToDate는 배지 없음 정상)에서 확인.
5. 분할 패인에서 한글 IME 조합·후보창, 드래그 선택, 마지막 행 출력, `12px` Antigravity 아이콘(1x/2x), `17px` 닫기 버튼(`50%` `9px`) 조작 확인.

### 기대 화면·상태·저장 결과
- 시작 `90%` 유지, 리셋 후 `ui.json`에 `ui_scale_pct: 100`과 기존 4개 v2 필드 보존(`0600`), 재리셋 시 파일 무변경.
- 모든 줌에서 배너·탭 바·body·분할선·선택·IME 좌표가 일치하고 모달/툴팁 이중 축소·잘림이 없다.

### 관측 지점과 성공·실패 기준
- **성공:** 시각 클리핑·겹침·좌표 오차 0건, `ui.json` 필드 손실 0건, 모든 줌에서 모달 닫기/스크롤·`100%` 3개 사이드바 경계 일치.
- **실패:** 배너/탭 바 이중 오프셋, 마지막 행 잘림, 모달 이중 축소, `ui.json` 필드 유실, `12px` 아이콘 깨짐, 실사용 경로 오염.

### 수집할 증거
- `50/90/100/150/200%`·사이드바 경계·모달/분할 패인 스크린샷(비밀정보 제외), 리셋 전후 임시 `ui.json` diff 및 `stat` 권한.

### 종료 후 정리
- 테스트 앱 및 `$T`에서 기동된 임시 로컬 데몬 종료 확인 후 임시 루트 `$T` 삭제.

### 미검증 항목과 남은 위험
- 계획 단계에서는 `UT-1..7` 및 `US-01` 모두 미실행(`Unverified`). 비-Retina 1x·Linux/Windows·비-Idle 업데이트 상태 미확보 조합은 미검증으로 남긴다.

## 4. 테스트 · 보안 리뷰 결과 (`test-reviewer`, `security-reviewer`, `review-conductor`, R1–R7 반영 갱신)

- **`test-reviewer` (재검토 완료):** R3(`canvas.y` 비이중 합산), R4(`100%` 복귀 후 3개 경계·비-Idle 조건), R6(`65,536`/`65,537` 바이트 경계), R7(긴 툴팁 `max_w`·넓은 창·독립 렌더 경로·폴백 분리) 반영 완료.
- **`security-reviewer` (재검토 완료):** R5(`CONFIG/STATE/RUN/PLUGINS` 4경로 export + `umask 077` + `update.json` `auto_check: off`)와 R6(`64 KiB` 경계 테스트) 반영 완료.
- **`review-conductor` (통합 완료):** `DESIGN.md`·`TESTS.md`·`TASKS.md` 간 반례 R1–R7 전부 해소 확인.
