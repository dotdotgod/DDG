# 작업 결과 보고서: 네이티브 UI 기준 크기 85%

## 보고 대상: 구현 배치

- 대상 태스크·배치 ID: **T1–T2**. R0에서 환경·작업 트리를 확인한 뒤 T1 기준값·UT-1..3, T2 뷰 통합·UT-4..7과 전체 native 테스트를 순차 수행했다.
- 해당 범위 상태: **완료 (배치 완료 · 전체 계획 V1/C1 잔여)**
- 전체 계획 상태: T1·T2 구현·자동 검증 완료(95/95) · V1 실창 검증 및 C1 대기. Neon의 독립 판정은 **설계 적합 · T1/T2 수락 · 시정 요청 0건**이다.

## 산출물·확정 요구사항과 설계 결정

[활성 계획](../../../../plan/ui-base-scale-85/README.md) · [설계](../../../../plan/ui-base-scale-85/DESIGN.md) · [테스트·US-01](../../../../plan/ui-base-scale-85/TESTS.md) · [태스크 A–F](../../../../plan/ui-base-scale-85/TASKS.md) · [계획 보고·R1–R7 종결](PLANNING.md)

사용자 확정안은 원래 논리 크기를 한 번만 85%로 반올림해 정수 기준값으로 만들고, 실행 중에는 사용자의 줌만 한 번 적용하는 것이다. 숨은 0.9 배율이나 v2 자동 마이그레이션은 쓰지 않았다. 100%는 1×이고, 기존 90% 설정과 다른 v2 필드는 사용자 리셋 전까지 그대로다. 범위·설계 변경 없음.

| 태스크 | 실제 변경 파일 | 배치 소유 변경 |
|---|---|---|
| T1 | `native/src/ui_scale.rs` | 숨은 배율 삭제, rem 14·터미널 12, 7px 하한과 50/200/10/100 계약 유지. 유효 v2 JSON 65,536바이트 수락·65,537바이트 거부 테스트. |
| T1 | `native/src/layout.rs` | 분할선 4→3. 중첩 패인·히트 영역과 부모 가용 길이 202/101 검증. |
| T2 | `native/src/main.rs` | 사이드바 최소/초기/최대 136/190/357, 레이블 경계 157/204, 헤더 19·Peek 49·리셋 27. 런처/설정 폭 429/544는 패널 안에서 한 번 변환. 선택기 111, 툴팁 최대 306, 글꼴 측정 실패 때만 유효 글꼴 × (8.43/14). 실제 뷰의 경계·좌표·리셋 저장 테스트. |
| T2 | `native/src/update_modal.rs` | 배너 27, 상태 레이블은 기존 비교식 피연산자만 200→170, 모달 442 한 번 변환. |
| T2 | `native/src/terminal_canvas.rs` | 기존 미추적 파일에서 기본 그리드 (7,14,12)와 12×3 dirty/full 검증 영역 84×42만 변경. |
| T2 | `native/src/icons.rs` | 테스트만 수정: 기존 샘플·48px 디코드를 유지하면서 12/24px 래스터 추가. |
| T2 | `native/README.md` | 줌 문단만 갱신: v2 90%=0.9×(12.6/10.8px), 선택적 리셋, 이미 100%이면 리셋 불필요. |

## 태스크 순서·공유 트리 보존

R0 → T1의 두 모듈 통과 → 같은 Sol이 T2 구현·검증 → Astra 재검증 → Neon 독립 리뷰 순서였다. R0에서 변경·미추적·삭제 및 쓰기 허용 경로 62개의 해시/부재 상태를 기록했다. 파일 드롭 담당자의 검증이 끝난 뒤의 `main.rs`를 비교 기준으로 사용했고 파일 드롭 회귀 3건도 통과했다. `terminal_input.rs`는 그 인계 시점과 동일했고, `terminal_interaction.rs`·`antigravity.svg`·`config_watch.rs`는 R0와 같았다. 기존 미추적 캔버스는 실제 파일 내용을 대조했다.

Intel 설치 작업의 crates·워크플로·`host_bridge.rs`·native README 도입부와 룸 전환 계획·목차는 별도 작업이다. 보존했으며 Sol의 산출물에 포함하지 않는다.

## 역할별 결과·리뷰와 검증 증거

Sol(`openai/gpt-6.2-sol`, medium)은 구현 및 테스트, Astra(`openai/gpt-6.2-astra`, high)는 A–F별 diff·실제 테스트 로그 대조와 재실행을 담당했다. Sol/Astra 환경은 Rust/Cargo 1.100.0 stable, `GHOSTTY_VT_LIB_DIR=/tmp/ddg-vt.t10r26/ghostty-vt.xcframework/macos-arm64_x86_64`였다. T2 최종 검증은 별도 `CARGO_TARGET_DIR=/tmp/ddg-ui-scale85-target.wttQw0`를 사용했다.

Sol의 필터 명령은 `cargo test -p ddg-agents-native <필터>`다. 아래는 최종 통과 수이며 모두 종료 코드 0, 실패·무시 0건이다. 0건 선택으로 통과 처리한 필터는 없다.

| 필터 | 통과 |
|---|---:|
| `ui_scale::tests` | 7 |
| `layout::tests` | 5 |
| `scaled_layout_keeps_sidebar_and_split_dividers_in_pointer_coordinates` | 1 |
| `peek_layout_and_shortcuts` | 2 |
| `ui_zoom_resizes_the_tab_bar_and_sidebar` | 1 |
| `update_banner_and_reset_notice_both_offset_the_canvas` | 1 |
| `modal_and_tooltip_widths_scale_once` | 1 |
| `font_grid_uses_measured_advance_or_calibrated_fallback` | 1 |
| `resetting_zoom_preserves_v2_preferences_and_skips_unchanged_write` | 1 |
| `terminal_canvas::tests` | 3 |
| `antigravity_resvg_rasters_keep_the_mask_and_color` | 1 |
| `config_watch::tests` | 3 |

전체 명령: **`cargo test -p ddg-agents-native`**. 같은 95건을 세 번 실행한 결과이며 서로 다른 285건이 아니다.

| 실행자 | 통과 / 실패 / 무시 / 필터 제외 | 종료 코드 | 테스트 소요 |
|---|---|---:|---:|
| Sol 최종 재실행 (t650) | 95 / 0 / 0 / 0 | 0 | 0.83s |
| Astra 별도 재실행 (t651) | 95 / 0 / 0 / 0 | 0 | 0.90s |
| Antigravity Gemini 4 Neon 독립 재실행 | 95 / 0 / 0 / 0 | 0 | 0.86s |

T1 최초 RED는 ui_scale 6통과/1실패, layout 4통과/1실패였다. T2 중간의 외부 bootstrap 컴파일 오류는 테스트 실행 전 차단으로 분류했다. 이후 실제 뷰 경계 fixture와 150% 선택기 폭을 수정해 전체 93/95 → 94/95 → 95/95가 됐다. 사용자 설정·crates를 고쳐 통과시킨 것이 아니다.

**Neon 독립 리뷰:** `git status --short -- native`와 일곱 파일의 실제 변경을 대조했다. 구현 경로에 잔존 `BASE_UI_SCALE`이나 교체 누락 리터럴이 없고, 전체 테스트와 `git diff --check -- native`(종료 0)를 확인해 T1·T2를 수락했다. Astra의 같은 diff 검사도 종료 0. 수정 재위임 없이 구현자 `ui-scale-85-impl`(t642)만 종료·제거했다.

## 미검증 사항·다음 실행 세션의 착수 조건

1. **V1 대기:** 격리된 config/state/run/plugins와 실제 디스플레이에서 [US-01](../../../../plan/ui-base-scale-85/TESTS.md)을 수행한다. 1x/2x 한글·아이콘 가독성, 50% 닫기 조작, IME 후보창·마지막 행 잘림, 100%에서 157/170/204px 전후 동작은 헤드리스 결과로 판정하지 않는다. 확보하지 못한 기기·상태는 미검증으로 기록한다.
2. **C1 대기:** V1 결과와 미검증 조합을 받은 뒤 전체 판정·최종 보고를 작성한다. 전체 범위가 완료된 경우에만 계획을 보관 위치로 옮긴다.

## 비가역 실행·아카이브·main 머지 기록

비가역 실행 없음. 실제 보고는 [이 배치 보고](BATCH_T1_T2.md)이며 [보고 목차](README.md)와 [활성 계획](../../../../plan/ui-base-scale-85/README.md)을 연결했다. 계획 보고를 덮어쓰지 않았고 계획 이동·`FINAL.md` 작성·커밋·머지·push·release 모두 미수행이다. **커밋·머지·push는 각각 사용자 명시 승인 전 금지**이며, 배치 수락은 통합·배포 승인이 아니다.
