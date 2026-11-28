# 작업 결과 보고서

## 보고 대상: 계획 작성

**최신 상태:** 계획 작성 및 R1–R7 정정 완료 · 실행 세션(T1/T2/V1/C1) 착수 대기 (구현·검증·머지 미수행). 이하 최초 보고는 당시의 반례와 조치 요청을 보존한 이력이다. 최신 판정은 문서 끝의 종결 기록을 따른다.

`ui-base-scale-85` · 2026-11-27 · **계획 작성 완료. 전체 구현·검증·머지 미수행(실행 세션 인계).** 설계·테스트 문서에 대한 별도 컨텍스트 반례가 있다. 문서가 존재한다는 뜻의 완료이지, 반례 해소·실행 승인 또는 제품 검증 완료가 아니다.

## 대상 태스크·배치 ID와 포함 범위

Phase B 사용자·데이터·검증 기준, Neon C/D/E 설계·리뷰 결과 수집, Astra 별도 반례 조사 및 F(`task-planner`), G(`completion-reporter`). 이 보고는 `PLANNING.md`이며 구현 배치/`FINAL.md`가 아니다.

## 해당 범위 상태: 완료

요구·범위·원값 매핑·UT/실기 판정·실행 명세·인계를 문서화했다. 세션 분리 점검에서 확인한 불일치는 아래에 증거와 담당자를 남겼다. 이번 세션은 Neon 소유의 설계·테스트 파일을 편집하지 않았다.

## 전체 계획 상태와 남은 범위

활성 계획 유지. **다음은 Neon의 R0 정정 및 해당 리뷰 갱신**이다. T1/T2 구현 및 테스트, V1 실창, C1 종료와 승인된 통합은 모두 미수행. 확정된 사용자 결정에 대한 새 질문은 없다.

## 산출물: 계획·설계·태스크 경로

[계획 목차](../../../../plan/ui-base-scale-85/README.md) · [Neon 설계](../../../../plan/ui-base-scale-85/DESIGN.md) · [Neon 테스트 설계](../../../../plan/ui-base-scale-85/TESTS.md) · [실행 태스크/기준값 전수표](../../../../plan/ui-base-scale-85/TASKS.md) · [보고 목차](README.md). `dotdotgod config . --json`에서 `active-task-intent=docs/plan/**` 확인. 실행 세션에 로컬 scratch 파일은 필요 없다.

## 확정 요구사항·설계와 결정 출처

사용자가 Phase B와 F/G 지시에서 확정: ① 숨은 `BASE_UI_SCALE=0.9`를 삭제해 100%=1× ② 모든 UI·터미널 원 기준값을 `round(×0.85)` 정수로 직접 할당 ③ 줌 min/max/step/default `50/200/10/100` 유지 ④ 미커밋 작업과 사용자 설정 보존. 기존 v2의 90은 90으로 열며 자동 보정·스키마 변경 없음. 앱 기본값과 현재 파일 값을 혼동하지 않는다. `native/src/{ui_scale,main,layout,update_modal,terminal_canvas,icons,config_watch}.rs` **현재 작업 트리**, `native/README.md`, `docs/spec/requirements.md`, 기존 활성·보관 계획과 교차 점검했다.

## 요구사항·가정 옵션과 설계 옵션

| 채택 | 검토 후 제외한 방법과 영향 |
|---|---|
| factor=`pct/100`; 원값 한 번만 반올림 | `BASE_UI_SCALE=0.81`(기존 0.9에 0.9 추가) 또는 `0.80`은 100%에서 각각 0.81×/0.80×라서 표시와 실제 배율이 다르다. 런타임 0.85를 또 곱하지 않는다. |
| 전체 UI와 터미널을 한 기준으로 | 터미널만 축소하면 사이드바·모달·hit 영역·텍스트 밀도가 목표와 달라진다. `overlay_panel`에서 한 번만 변환, 뷰에 두 번째 scale 상태를 만들지 않는다. |
| 최소 50%, 터미널 글꼴 하한 7px 유지 | 최소 60%로 올리면 기존 사용자의 50% 선택을 없앤다. 1x 한글 문제는 실기 결과로 별도 재검토하며 현재 스펙을 미리 바꾸지 않는다. |
| 기존 v2 퍼센트 존중 | 자동 90→100 재작성·v3 마이그레이션은 기존 선택을 없애고 다른 agent 필드를 훼손할 수 있다. 사용자가 원할 때만 reset. 새 DB·락·강제 복구 없음. |
| 기준값만 정수, 측정된 문자 폭은 실수 | fallback의 `8.43 @ 14px` 보정 비율 고정; 새 base 12를 분모로 쓰면 default 글자 폭이 그대로 8.43이 된다. SVG 소스/512px 안전 한계, 터미널 프로토콜·입력 좌표는 변경하지 않는다. |

## 태스크 순서·의존관계·병렬 그룹

R0 정정·작업 트리 대조 → T1 core/layout + UT-1..3 → T2 모든 UI·터미널 연결부/문서 + UT-4..7/전체 native → V1(US-01) → C1(실행일 전체 보고·이동·승인 경계). 구현과 그 유닛/GPUI 테스트는 **같은 담당·같은 태스크**. 공유 `main.rs` 및 수치 계약 때문에 병렬 편집하지 않는다. 읽기 전용 환경·기기 준비는 독립 가능하다.

## 비가역 실행의 결정 기록

**계획 중 없음.** 체크인·머지·push·release·사용자 설정 변경·실서비스 업데이트 모두 미수행. 후속 C1에서 `main` 통합은 공유 기준선을 바꾸므로 검증 증거·범위 확인 후 **별도 명시 승인**을 받아야 한다. merge/squash는 승인된 정책 선택, 대안은 검토 브랜치에 보류. 로컬 변경 재작업과 달리 공유 이력은 단순 삭제로 복구하지 않고 필요시 후속 revert; push된 이력 전파·배포된 바이너리는 로컬 revert로 회수되지 않는다. `git push` 및 release도 각각 별도 명시 승인 전 금지, 이번 범위의 필수 산출물이 아니다. 보고서가 권한을 대신하지 않는다.

## 역할별 결과·리뷰와 검증 증거

| 단계/역할 | 요청 / 실제 | 상태·증거 |
|---|---|---|
| B `user-requirements`, `data-requirements`, `validation-requirements` | Pi `openai/gpt-6.2-astra` high / 동일 | 각각 검토 완료. 파일/권한·reload·누락 호출부 확인. B handoff `ui-scale-85-phase-b.md`의 결정과 원값을 현행 [태스크](../../../../plan/ui-base-scale-85/TASKS.md)로 이관. |
| C/D/E 설계·리뷰 지휘, UX/Presentation/Data/Test 설계 및 영역별·보안·전체 리뷰 | Antigravity Gemini 4 Neon / Gemini 4 Neon `high` (기존 세션 설정, 작성자 표기 기준; Astra가 외부 런타임을 측정한 값은 아님) | `DESIGN.md` §§1–5, `TESTS.md` §§1–4 수집. **동일 Neon 세션 내 역할 분리**, 독립 리뷰 아님. |
| 반례 조사·F·G | Pi `openai/gpt-6.2-astra` `high` / 동일 (`PI_PROVIDER`, `PI_MODEL`, `PI_REASONING_LEVEL`) | Neon과 **다른 컨텍스트**에서 두 원문·실제 소스/호출/테스트 대조. F·G 산출물 작성 완료. 별도 에이전트를 새로 생성하지 않음. |
| 후속 구현 | Pi `openai/gpt-6.2-sol` `medium` 지정 / **미실행** | 실제 모델/테스트 결과 없음. 실기 검증자·일정 미정. |

### Astra 별도 점검: Neon에게 되돌리는 반례와 수정 기준

다음은 **소스 확인 및 거기서 계산한 반례**이지 실창 관측이 아니다. 명시된 원인과 판정만 고치고 사용자 결정을 재해석하거나 현재 동작을 잘못된 테스트에 맞추지 않는다.

| ID · 분류 | 문서 ↔ 근거 / 반례 | Neon 조치 및 검증 기준 |
|---|---|---|
| R1 · 범위 누락 | `DESIGN §2/§5`의 전수 일치 주장은 현행 `main.rs:7679–85`와 다름. compact 185 외에 **240**이 “Agents”/“Agent Settings”를 나눈다. `update_modal.rs:476`의 200도 이제 raw가 아니라 이미 `scale.px(200.)`다. | `240→204`, `200→170` 피연산자만 교체(이중 스케일 금지). US-01에 100% 203/204/205 및 기존 두 경계 포함. 최신 영문 문구 보존. |
| R2 · UX 오판정 | `DESIGN §1`/`TESTS US-01`: reset 즉시 배지 소멸/무변경 리렌더 없음으로 읽힌다. `main.rs::apply_zoom_action`은 변경 여부와 무관하게 apply + notify; `apply_ui_scale`은 1500ms indicator를 설정. `zoom_badge_pct`, `indicator_deadlines_wake_without_host_events`가 근거다. | 시작 100% 무배지와 **리셋 피드백** 구분. reset 직후 100 배지, 마지막 입력 후 약 1.5초 뒤 사라짐. 무변경 때 **쓰기 없음**이지 notify 없음이 아니다. `PaneCache::prepare`에 해당하는 `terminal_canvas::prepare`가 `(grid,window.scale_factor())` 키로 lazy 재생성; 명시적 전체 캐시 flush 추가 금지. |
| R3 · 잘못된 기하 oracle | `TESTS UT-5`의 `tab_bar.bottom + banners`는 update가 보일 때 이중 합산 가능. `main.rs:7500–23,7712–17`: 오른쪽 열은 update→tab→reset→old-host→peek. `DESIGN` ASCII도 배너가 사이드바 위 전폭인 것처럼 보인다. 분할 시 terminal **body**는 canvas 전체와 같지 않다(헤더/인셋). | 탭 없을 때도 각 visible 높이를 한 번만 합산. 예: 100% update+tab+reset만 있으면 tab.bottom=55, canvas.y=82, **109 아님**. 창/열 원점과 terminal bounds를 같은 좌표계에서 대조. 그림·테스트 정정. |
| R4 · 실창 시나리오 반례 | `TESTS US-01`은 200%까지 올린 직후 절대 157/170px로 드래그한다. 그 줌의 실제 compact/update 경계는 314/340px이고 넓은 창 최소폭도 272px. `update_sidebar_row`에는 Idle/UpToDate의 `● 최신` 자체가 없고 상태가 `Some`일 때만 배지가 있다. | 100%로 복귀하거나 경계값도 줌 적용. 안전한 표시 상태(예: UpdateAvailable)에서 text/dot, 평상시는 둘 다 없음. 테스트 목적으로 실제 적용·롤백하지 않는다. 9px는 close **glyph**, 100% 클릭 상자는 17px (`tab_bar_button`). |
| R5 · 시나리오 격리 결함 | `TESTS US-01`의 `DDG_CONFIG_DIR=$(mktemp -d)` 단독은 자식 프로세스로 export되지 않을 수 있고, export해도 Unix `transport::dial::default_local_host_sock`과 `host::paths::from_env`의 `host.sock`/`catalog.json`/plugins는 다른 경로다. native는 로컬 host/core를 자동 기동할 수 있다. | V1에 private config/**run/state/plugins** 또는 일회용 계정/VM, 로컬 프로필·권한·daemon 종료 확인 명시. `umask 077`로 시작하고 실제 사용자 디렉터리에 fixture를 만들지 않는다. 격리되지 않으면 실창 검증 차단. |
| R6 · 증거 과장/실패 분류 | `TESTS §4`가 UT/US에서 64 KiB 보호를 확인한다고 했지만 `ui_scale.rs::prefs_rejects_bad_files_and_normalizes_percent`에는 길이 경계 입력이 없다. `DESIGN §3`의 reload 실패 시 이전 값 유지는 NotFound에는 해당하지 않음(`config_watch.rs::reload`, `reloads_only_valid_changes_and_resets_deleted_files`). | UT-2에 유효 65,536/65,537 bytes 경계 추가 또는 실측 주장 삭제. 시작 오류→default, live invalid→마지막 정상값, 삭제→default를 분리. 새 런타임 Validation 필요 없음. |
| R7 · 측정 전제 누락 | `TESTS UT-6`: `AgentTooltip`은 fixed width가 아닌 **max_w**, modal은 `max_w_full`. 짧은 레이블/좁은 viewport의 실측 폭이 306/544가 아니어도 정상이다. update와 tooltip은 `overlay_panel` 호출자도 아니다. | 긴 툴팁·넓은 창·독립 렌더 경로로 100/150%를 판정하고 좁은 창은 가용 영역/조작 가능성으로 판정. `font_grid` 측정 성공과 fallback을 분리. |

UT-1 숫자·floor, UT-3 origin/분할 반올림, UT-4 cap/역변환, 단일 변환 설계, v2 저장 계약은 유지 판단. 위 정정안은 [실행 명세](../../../../plan/ui-base-scale-85/TASKS.md)에 반영했다. **원본 수정 및 영향받은 Neon 리뷰 갱신은 대기**이며, 미수행 리뷰를 승인으로 기록하지 않는다.

## 차단·미결정·미검증 사항과 남은 위험

실행 전 R0 동기화 필요. 50% text_xs=5.25px, 100% 1x 한글 10.5px, 최소 클릭 영역, 폰트별 IME/마지막 행, 기기 배율/상태별 모달은 미검증이다. 실기로만 확인할 품질과 수학적 기준을 구분한다. 여러 프로세스가 v2 스냅샷을 동시에 저장하면 마지막 교체가 우선하는 기존 한계는 그대로다.

## 다음 실행 세션의 착수 조건

Neon 정정/재검토 → Astra 대조, 명시적인 별도 실행 배정, 현행 미커밋·미추적 파일 소유/충돌 확인, Ghostty 환경 확보. T1/T2는 `implementer.md`를 적용해 TDD와 함께 수행. 실사용 파일·원격 세션을 검증 대상으로 쓰지 않는다.

## 아카이브·main 머지 기록

실제 보고: `docs/archive/report/2026-11-27/ui-base-scale-85/PLANNING.md`; [보고 목차](README.md)와 [현재 계획](../../../../plan/ui-base-scale-85/README.md) 양방향 연결. 실행 종료 후에만 실행일 `FINAL.md`·관련 README를 작성하고 `docs/archive/plan/ui-base-scale-85/`로 계획 이동, 이동 후 링크 갱신. **main 머지·커밋 없음.** 향후 승인된 merge/squash 본문에 보고/계획 경로, 선택·제외 이유, 범위 변경, 승인, 시행한 검증·잔여 위험을 기록한다. archive 무시 시 강제 추가하지 않는다.

## 수행·미수행 작업과 다음 착수점

수행: 문서/소스 점검, 계획·보고·인덱스 작성, Markdown 한도/상대 링크 검사. 미수행: 제품 코드/설정 변경, 빌드·테스트·실창, Sol/하위 에이전트 실행, 계획 이동·커밋·머지·push·release. **당시 다음 담당: Neon, R0.**

## 2026-11-27 23:38 KST · R0 문서 정정 종결

Neon이 [설계](../../../../plan/ui-base-scale-85/DESIGN.md)와 [테스트](../../../../plan/ui-base-scale-85/TESTS.md)를 정정하고 영향받은 역할 리뷰를 같은 세션에서 갱신했다. Astra는 별도 컨텍스트에서 두 원본, [태스크](../../../../plan/ui-base-scale-85/TASKS.md), 현재 작업 트리 소스를 다시 대조했다. 아래는 **문서·소스 검증**이며 제품 테스트 통과 기록이 아니다.

| 반례 | Astra가 재확인한 종결 근거 |
|---|---|
| R1 | 설정 레이블 240→204, compact 185→157. 업데이트 행의 기존 `scale.px(200.)`는 피연산자만 170으로 바꾼다. |
| R2 | reset 시 100% 임시 배지 약 1.5초·notify는 유지하고 무변경이면 저장하지 않는다. 캐시는 grid/device scale 키로 갱신한다. |
| R3 | 우측 열은 update→tab→reset→old-host→peek. 각 표시 높이를 한 번만 합산: 100% update+tab+reset=82, 탭 하단=55. 탭 미표시 시 탭 기여는 0, 분할 body와 전체 canvas는 구분한다. |
| R4 | US-01은 100% 복귀 후 157/170/204 양옆과 경계를 확인한다. 상태가 있는 업데이트만 text/dot을 비교하며, 닫기 glyph 9와 클릭 상자 17을 구분한다. |
| R5 | US-01이 임시 CONFIG/STATE/RUN/PLUGINS 경로 export, private 생성·auto_check off, 로컬 socket/catalog 사전 확인과 전용 데몬 종료를 요구한다. 격리 불확실 시 실행 금지. |
| R6 | 시작 실패→기본값, live invalid→직전 정상, 삭제→기본값. UT-2에 유효 v2 JSON의 65,536 byte 수락/65,537 byte 거부가 명시됐다. |
| R7 | UT-6은 넓은 창의 실제 패널 폭과 긴 툴팁의 최대 폭을 구분한다. update·tooltip은 독립 경로, font advance 성공과 fallback은 별도 판정한다. |

**판정: R1–R7 전부 종결.** 다음은 별도 배정된 실행 세션의 R0 작업 트리·충돌·Ghostty 환경 확인 후 T1→T2→V1→C1이다. 구현·제품 검증·머지 미수행; 실기 위험과 명시 승인 조건은 그대로다.
