# 실행 태스크: `ui-base-scale-85`

입력: [확정 요구](README.md), [설계](DESIGN.md), [테스트](TESTS.md), [별도 컨텍스트 반례 R1–R7](../../archive/report/2026-11-27/ui-base-scale-85/PLANNING.md). 아래 명령은 **후속 실행 세션 전용**, 저장소 루트 기준이다. 유형별로 결과/명령/실패·미검증/위험을 기록한다. 계획 보고는 실행 승인이 아니다.

**계획 작성 및 R1–R7 정정 완료 · 실행 세션(T1/T2/V1/C1) 착수 대기 (구현·검증·머지 미수행).**

## R0 · 문서 정정 완료 / 실행 세션 사전 확인 대기

- **문서 확인 완료 (2026-11-27):** Neon이 R1–R7와 해당 역할 리뷰를 갱신했고 Astra가 설계·테스트·이 표·현행 소스를 별도 대조했다. 아래 작업 트리·환경 확인은 실행 세션이 T1 전에 수행한다.
- 승인된 실행 세션: `git status --short`, `git diff -- native`와 **미추적 파일의 실물**을 확인. `rg -n 'scale\.px|\bpx\(|sidebar_base|overlay_panel|tab_bar_button|BASE_UI_SCALE|BASE_TERM_FONT_PX' native/src`로 아래 목록과 대조. 행이 이동하면 함수/문맥으로 식별한다. 추가 크기/다른 편집자 발견 시 겹치는 변경만 멈춰 담당자와 조정한다. 현재 트리가 기준이며 HEAD 복원·일괄 치환·전체 포맷·stash/clean 금지.
- 환경: `native/README.md`의 해당 플랫폼 Ghostty 경로를 `GHOSTTY_VT_LIB_DIR`로 export. 툴체인·동적 라이브러리·GUI 권한 미비는 Infra 차단으로 보고(테스트 통과 아님). T1→T2는 순차: T2의 기대값이 T1 새 배율에 의존. 단일 작성자가 각 태스크의 제품 코드 **및 테스트**를 함께 소유한다.

## T1 · 구현 + UT-1/2/3 — R0 및 별도 실행 배정 후

- 역할: `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`를 먼저 읽고 적용. `pi --provider openai --model gpt-6.2-sol --thinking medium`. `terminal_canvas.rs`, `terminal_interaction.rs`, `terminal_input.rs`, `antigravity.svg` 등 기존 미커밋 변경 보존. T1 쓰기: `native/src/{ui_scale,layout}.rs`만; 공유 파일 동시 편집 금지.
- RED→GREEN: 아래 A 행의 새 수치에 맞춰 기존 테스트를 먼저 바꾼 뒤 `BASE_UI_SCALE`과 그 곱을 제거. f32 타입은 유지하되 기준값은 정수. `rem=14×factor`, `term=max(12×factor,7)`; `px(base)=max(round(base×factor),1)` 유지. 배너는 대응하는 렌더/오프셋과 함께 T2에서 변경한다.
- 독립 기대값: pct=`50,90,100,150,200` 순서로 factor=`0.5,0.9,1,1.5,2`, `px(3)=2,3,3,5,6`, rem=`7,12.6,14,21,28`, term=`7,10.8,12,18,24`; `px(0.1)=1`. 픽셀은 정확히, 실수는 기존 허용오차로 비교. 정규화·스텝·권한·저장 실패 복구 유지. UT-2에 유효 v2 JSON + 공백으로 **정확히 65,536 bytes**인 비기본값 파일은 load 성공, 공백 한 byte 추가 시 `load` 오류 확인(잘못된 JSON 테스트로 대신하지 않음).
- UT-3 입력 `(10,20,205,104)`: 패인 `(10,20,71,104)/(84,20,131,61)/(84,84,131,40)`, hit `(81,20,3,104)/(84,81,131,3)`, parent extent `202/101`. tiny/leaf 보존.
- 명령: `cargo test -p ddg-agents-native ui_scale::tests` 및 `cargo test -p ddg-agents-native layout::tests`. 필터가 실제 테스트를 선택했고 해당 모듈 전부 green이어야 T1 완료. 미전환 뷰의 기대값은 T2가 소유하며 T1 단독으로 전체 완료 아님.

## T2 · 구현 + UT-4..7 및 통합 — T1 후

- 역할: `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`를 먼저 읽고 적용. `pi --provider openai --model gpt-6.2-sol --thinking medium`. `terminal_canvas.rs`, `terminal_interaction.rs`, `terminal_input.rs`, `antigravity.svg` 등 미커밋 작업과 최신 UI 문구를 보존한다. `main.rs` 및 그 내부 GPUI 테스트는 **같은 작성자**; 다른 세션과 병렬 수정하지 않는다.
- 쓰기: `native/src/{main,update_modal,terminal_canvas}.rs`, `native/src/icons.rs` 테스트만, `native/src/config_watch.rs` 테스트만(필요 시), `native/README.md`. 측정·저장·단축키 회귀는 아래 범위에서만. CLI/crates, SVG, 입력 모듈, 실제 설정과 스키마 금지. 실패가 외부 미완성 작업 때문이면 그 diff를 고치지 말고 원인/명령 기록.

### 기준값 전수표 (A=T1, B–F=T2)

원값에 **한 번** `round(원값×0.85)` 적용한 결과를 소스에 직접 할당. 이후 런타임에는 논리 px를 `UiScale::px`로 한 번만 변환. 아래 `main.rs` 행은 조사 시점의 `native/src/` 기준이다.

| 구분 | 원값 → 새 값 | 적용 위치 |
|---|---|---|
| A | `BASE_UI_SCALE .9→삭제`; rem `16→14`, term `14→12`; `PANE_DIVIDER 4→3` | `ui_scale.rs::factor`, `layout.rs`. min/max/step/default `50/200/10/100` 불변 |
| B | `SIDEBAR_MIN/MAX 160/420→136/357`, initial `224→190`, `DIVIDER_WIDTH 6→5`, `RESET_BANNER_HEIGHT 32→27` | `main.rs:60–65,1355`, old-host/reset 공용. `TAB_BAR_HEIGHT_REM=2`와 `PANE_INSET=1` 불변 |
| B | `22→19`, `58→49`, `185→157` | `PANE_HEADER_HEIGHT`(:7358, body/grid/오버레이), `PEEK_BANNER_HEIGHT`(:5874/7506), `SIDEBAR_COMPACT_MAX`(:3073/3471/7661/7679); 각각 렌더와 조건·오프셋 한 원본 |
| B | `240→204` | :7680 추가 설정 레이블 분기. `≤scale.px(157)` 아이콘만, 초과~`≤scale.px(204)` “Agents”, 초과 “Agent Settings”. 경계 포함 유지 |
| B | `UPDATE_BANNER_HEIGHT 32→27`, 버전 행 `200→170`, 모달 `520→442` | `update_modal.rs:19/548,476,1092`. :476은 **이미** `self.ui_scale.px(200.)`임. 감싸지 말고 피연산자만 교체. 모달 폭은 한 번 감싼다 |
| C | `2→2`; `3→3` | :3229,3519,3620,3666,4049,4096,4415; :3234,7628 |
| C | `4/7/11→3/6/9` | :3200–01 `3 + min(depth,3) × (compact ? 6 : 9)`를 한 번 스케일; 깊이 3단계 유지 |
| C | `6→5`; `7→6`; `8/16→7/14` | :2882,3361; :3623; :3421 비들여쓰기/들여쓰기 |
| C | `10→9`; `11→9`; `12→10` | :7624; :3166,5658; :3143,3440,3549,3630,4243,7489 |
| C | `13→11`; `14→12` | :2855,3106,5716,7838; :3145,3491,3580,3607,4493,4740,5624,6007,7660,7678 |
| C | `15→13`; `16→14`; `18→15` | :5779; :3629,4877,6149,7627 및 :3421; :5176 |
| C | `20→17`; `24→20` | :3433,3485,3535,4487,5576,7483; :7616 패딩 경계 |
| C | `112→95`; `160→136`; `275→234`; `360→306`; `460→391`; `480→408` | :5652 탭 이름; :5729 배지; :4840 런처 **리스트 max_h**; :5475 rename 확인; :4462 머신 모달; :7727 blocked/empty 카드 |
| D | `505→429`; `640→544` 두 곳; `130→111` | `overlay_panel` 내부 :4719 단일 스케일. :4917 런처 **사전 스케일 제거**; :5095/5423 설정 복구/일반 모두 원시 544 전달. :5130 선택기는 한 번 스케일 |
| D | `360→306` | `AgentTooltip::render` :739의 **max_w**. `round(306 × window.rem_size()/BASE_REM_PX)`; 기존 세 생성자에 상태 추가 없음 |
| E | fallback advance `8.43 at 14px`의 비율 유지 | `main.rs::font_grid` 측정 실패 때만 유효 font × `(8.43/14)`; 새 base 12를 분모로 쓰지 말 것. `1.15` leading/ceil 유지 |
| E | grid `8/16/14→7/14/12` | `terminal_canvas.rs::FontGrid::default` font는 공유 `BASE_TERM_FONT_PX`. preedit hairline `1→1`, cursor `unit×3/2→동일` (`unit=effective font/base`, 100% 두께 2); cell·scrollbar·IME는 측정 좌표 그대로 |
| F | 재배율 금지 | `tab_bar_button` close/add :5658/5716에 원시 `9/11`, 내부 `scale.px(size)`; `icons.rs::icon`, 이미지/스피너 `px(size)`는 이미 실제 크기. viewBox 24, 512px 안전 한계, 셀 수·화면 좌표·기기 dpi·55% viewport cap·45% list cap·rem 유틸 불변 |

- `native/README.md`: “새 100%=이전 90%” 교체. 기존 v2 `90`은 `0.9×`, rem 12.6/term 10.8; 원할 때만 Cmd/Ctrl+0. 이미 100이면 리셋 불필요. v1 거부·다른 v2 필드 유지. 자동 재작성/언어 편집 없음.
- **같은 T2에서 RED→GREEN:** UT-4 실제 initial 및 `sidebar_px(224,…)` 두 fixture를 190으로, 기대 `95/190/285/380`; 120창 cap 66과 드래그 7500bp 유지. UT-5 `peek_layout_and_shortcuts` 이전 22 기대 두 곳을 독립값 `10/19/38`로; GPUI `ui_zoom_resizes_the_tab_bar_and_sidebar` 100/150/90 탭 높이 `28/42/25`, 배너 스택/Peek 발생 시 실제 body·terminal bounds와 top 비교(R3). 100% 탭+update+reset 시 82, Peek만 추가 시 +49(존재하는 상태별 구성). 단일 pane 헤더 0.
- UT-6: 넓은 viewport에서 launcher/settings(정상·복구)/update **실제 패널 폭** 100% `429/544/442`, 150% `644/816/663`; selector `111/167`, 길게 감기는 툴팁 **최대** `306/459` 확인. 짧은 텍스트 실제 폭과 혼동 금지. 테스트 셀렉터 필요 시 기존 GPUI 패턴 재사용. fallback 분기를 측정 성공 경로와 구분해 `≈7.225714`, `4.215`(허용오차 0.001) 확인. 별도 렌더러/서비스 mock 없음.
- UT-7: `terminal_canvas::tests` 기본값 `(7,14,12)`, dirty/full `12×3` fixture `84×42`, 기존 스크롤 배지 회귀 유지. `icons::tests::antigravity_resvg_rasters_keep_the_mask_and_color`에는 `[12,24]` 샘플 **추가**, 14/16/28/32·decode 48 유지. 일회용 파일로 실제 `apply_zoom_action(Reset)`과 로드/재로드를 연결해 90→100 및 sort/pins/manual/collapsed 비기본값 보존; no-op 리셋 시 저장 없음, 일시 배지 유지. watcher Invalid/Missing/unchanged 기존 검증 유지.
- 명령(각 필터 0건이면 실패): `cargo test -p ddg-agents-native scaled_layout_keeps_sidebar_and_split_dividers_in_pointer_coordinates`, `cargo test -p ddg-agents-native peek_layout_and_shortcuts`, `cargo test -p ddg-agents-native ui_zoom_resizes_the_tab_bar_and_sidebar`, `cargo test -p ddg-agents-native update_banner_and_reset_notice_both_offset_the_canvas`. 신규/확장 테스트와 위 경로 전부 포함하는 최종 `cargo test -p ddg-agents-native` 필수. green + 보존 diff 검토 후 V1 인계; 실창 결과를 대체하지 않는다.

## V1 · 검증 — T2 통합 후, 제품 코드 쓰기 없음

- 사람/실창 검증자, [US-01](TESTS.md)와 [R1–R7](../../archive/report/2026-11-27/ui-base-scale-85/PLANNING.md) 적용. 격리 가능한 macOS/실제 디스플레이에서 실행; 가능하면 1x/2x와 Linux/Windows도. OS·폰트·기기 배율·viewport 기록. 준비: `cargo build --release -p ddg-agents-native -p ddg-agents-host -p ddg-terminal-server`. 로컬 테스트 바이너리만 사용.
- 앱 실행 **전**, 임시 계정/VM 또는 짧은 전용 temp root에 `umask 077`; `DDG_CONFIG_DIR`, `DDG_STATE_DIR`, `DDG_RUN_DIR`, `DDG_PLUGINS_DIR`를 각기 그 아래 디렉터리로 **export**. 실사용 경로·원격 프로필·플러그인·세션을 가리키지 않는지 확인. 기본 로컬 소켓과 카탈로그도 격리됐을 때만 `target/release/ddg-agents-native` 실행. 임시 update.json `{"auto_check":"off"}`, US-01 v2 90 fixture(비기본 collapsed도 포함) 사용. 업데이트 적용/롤백/릴리스 금지. 격리가 불확실하면 실창 단계 차단으로 둔다.
- 처음 90, reset 직후 100 배지 → 1.5초 후 사라짐. 재리셋 전후 bytes·mtime/권한 비교, 재시작 확인. 50/90/100/150/200%, 좁은/넓은 창에서 바·헤더·배너·모달 닫기·스크롤·선택·IME·마지막 행과 선명도 확인. 다시 **100%**에서 156/157/158, 169/170/171(표시 가능한 업데이트 상태일 때), 203/204/205px 경계 확인. 시간제한 상태를 실제 업데이트 실행으로 만들지 말 것. 격리 fixture/상태 미확보면 해당 조합만 미검증으로 기록.
- 성공: 실제 측정과 UT oracle 일치, 필드 유실/잘림/오조작 없음. 9px close glyph의 17px 버튼(50% 버튼 9px) 조작은 별도 관찰. 기기/상태 미확보 또는 실패 시 증거·영향 전달, 축소율/스펙을 임의 변경하지 않는다. 비밀 없는 화면·diff·stat 보존 후 **자체 임시 세션/분리 데몬만** 종료 확인하고 해당 임시 root만 정리한다.

## C1 · 전체 종료/통합 인계 — V1 판정 후

- 실행 총괄/`completion-reporter`: UT·GPUI 명령과 실제 결과, US-01 기기별 결과/미검증·잔여 위험, 승인·변경 근거를 실행일 `docs/archive/report/YYYY-MM-DD/ui-base-scale-85/FINAL.md`에 남긴다(계획 보고 덮어쓰기 금지). 미완은 부분 완료·활성 계획 유지. 배치 보고가 필요할 때만 `BATCH_<ID>.md`.
- 전체 범위 완료 시 `docs/plan/ui-base-scale-85/` → `docs/archive/plan/ui-base-scale-85/`. 활성/아카이브/날짜별 README와 계획↔보고 링크를 이동 후 경로로 고치고 링크·문서 한도를 재검사. 먼저 비가역 승인 없이도 diff·보고·머지 본문 초안 준비 가능.
- `main` 머지 또는 squash는 **명시 승인 후만**. 본문에 계획·보고 경로, 결정/대안/범위 편차·근거, 승인, UT·실기 결과와 미검증 위험 명시. 승인된 변경만 포함; archive가 무시되면 강제 추가하지 말고 본문을 자족적으로 남긴다. push/release는 별도 명시 승인 없이는 하지 않는다. 승인 대기 중 머지 SHA·배포 성공을 기입하지 않는다.
