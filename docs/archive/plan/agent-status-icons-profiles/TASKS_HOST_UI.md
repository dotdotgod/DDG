# 실행 태스크 명세: T4–T6 (`host` · `cli` · `native` GPUI · 검증 · 종료)

관련 문서: [계획 개요](README.md) · [UX·UI 설계](DESIGN_UI.md) · [데이터·네트워크·보안 설계](DESIGN_DATA.md) · [TDD 유닛 테스트](TESTS.md) · [사용자 시나리오](SCENARIOS.md) · [태스크 T0–T3](TASKS_CORE.md)

---

# 태스크: T4-A — `ddg-agents-host` RAM `TabActivity` · `tab_mark_seen` · 플러그인 격리 · `host.pid` (병렬 그룹 B)

## 목표
`ddg-agents-host`에 RAM 전용 `HostState.activities`(오퍼레이터 채널 `Standard` + 실행 중 `rm_bg_*` 포함, `catalog.json` 저장 비트리거), `catalog_subscribe` `after_reply` 델타 채널, `host.tab_mark_seen`, 플러그인 5개 반환 경로 공통 `redact_for_plugin` 및 `tab_create` idempotency 스코프 격리, 대소문자 `DDG_*` 차단, `parent_host_id` 기록·`finish_exit` 가드, `host.pid`를 TDD로 구현한다.

## 태스크 유형: 구현
## 수행 역할과 역할 정의의 정확한 경로
- `implementer` — `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` (로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`)

## 실행 상태: 착수 가능 (`T3` 완료 후, `T4-B`와 병렬 가능)
## 선행 태스크와 착수 조건
- `T1`, `T3` 완료.

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음.
- 허용된 승인 준비·조건 확인 작업: `crates/ddg-agents-host/src/{host_service.rs, plugin_host.rs, catalog_store.rs, server.rs, hub.rs}` 확인.
- 대기 중 계속할 독립 작업: `T4-B`와 크레이트 분리 병렬 진행 가능.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `RC-01`: `HostState.activities`와 오퍼레이터 구독에는 `Standard` + 실행 중 `rm_bg_*` 모두 포함(종료 시 즉시 제거); `plugin_host.rs`·`hub.rs` 경계에서만 `rm_bg_*` 격리.
- `DNL-06`: `TabActivityChanged`/`TabSeenChanged`를 `catalog_subscribe` 전용 채널(`st.subs`, `after_reply`)로 송신해 스냅샷 역전 레이스 차단.

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [DESIGN_DATA.md](DESIGN_DATA.md#1-필수-validation-및-신뢰-경계-확정-표-13개-경계) (경계 #5, #6, #7)
- [DESIGN_DATA.md](DESIGN_DATA.md#32-ddg-terminal-server--ddg-agents-host-rpc--openrpc-13)
- [TESTS.md](TESTS.md#4-cratesddg-agents-host-3그룹) (`UT-HOST-01`..`UT-HOST-03`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `crates/ddg-agents-host/src/{host_service.rs, catalog_store.rs, plugin_host.rs, hub.rs, server.rs, paths.rs}`
- 쓰기 금지: `crates/ddg-agents-cli/**`, `native/**`, 그 외 크레이트

## 수행 내용
1. `host_service.rs`:
   - `HostState.activities: HashMap<TabId, TabActivity>` RAM 전용 관리(`AgentStatusChanged`/`ForegroundChanged` 수신 시 `save_catalog()` 미호출).
   - `CatalogSubscribeResult.activities` 원자 스냅샷 + `st.subs`(`after_reply`) 채널로 `TabActivityChanged`/`TabSeenChanged` 송신.
   - `tab_mark_seen`: `run_generation` 일치 검증 + `seen_seq = seen_seq.max(req.seen_seq.min(activity_seq))`.
   - 라이프사이클: `tab_restart`(세대+1·활동 리셋), `tab_close`/`rm_bg_*` `Exited`(즉시 삭제), 가시 탭 `Exited`(`Working`/`Blocked`/`foreground` 제거, `exit_code == 0`→`Done` / `!= 0`→`Error`), `tab_move` 승격(활동·`agent_profile` 보존).
   - `tab_create_with_visibility(..., visible_only)` 분리 + idempotency 스코프 `"operator"` vs `"plugin"` 격리.
   - `ExecutorSpec::Command.env` 대소문자 무관 `DDG_*` 전면 거부 + `agent_profile` 슬러그 검증.
   - 원격 `host.session_create`: 인증 `caller`로부터 `parent_session_id` + `parent_host_id` 유도(위조 파라미터 무시), `finish_exit()` 로컬 `parent_host_id` 일치 가드.
2. `catalog_store.rs`: 레거시 로드 `strip_session_env`를 대소문자 무관 `DDG_*` 제거로 확장.
3. `plugin_host.rs` & `hub.rs`: `redact_for_plugin`을 5개 반환 경로(`catalog_get`, `room_create`, `room_rename`, `tab_create`, `tab_restart`) + idempotency 히트 전부에 적용(`executor.env.clear()`, `Visible`만); `host.tab_mark_seen` 플러그인 및 `hub::allowed()` 제외.
4. `server.rs`: `HostHelloResult.features` `"agent_activity_v1"` 추가; `host.lock` 획득 후 `host.pid` (`0o600`, `O_NOFOLLOW`) 기록·종료 삭제.

## 구현 착수 근거
- 변경 대상: `ddg-agents-host` 6개 파일.
- 계약: `RC-01`, `D-SEC-03`, `D-SEC-07`, `DNL-06`, `DNL-12`, `DNL-13`, `F-02`, `F-07`, `E-SEC-03`.
- 검증 방법: `UT-HOST-01..03` TDD + 기존 5개 통합 테스트.

## 필수 Validation과 실패 처리
- `DESIGN_DATA.md` 경계 #5, #6, #7 준수.

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `UT-HOST-01..03` + 기존 `ddg-agents-host` 테스트 PASS.
- 같은 태스크에서 작성·수정할 유닛 테스트: `UT-HOST-01`..`UT-HOST-03` (`TESTS.md` §4).
- 실행할 테스트와 명령: `cargo test -p ddg-agents-host`
- 테스트 데이터·환경과 독립적인 기대값 근거: `seen_seq` 단조 수식, `plugin_host.rs:403-442` 5개 반환 경로 목록, `catalog.json` mtime/해시 불변.
- 통과 기준과 남는 검증 범위: 경고 0개 + 전체 PASS; 멀티클라이언트·원격은 `T6-A`.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-A`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- 활동 변경 시 `save_catalog()` 호출 금지. `plugin_host`/`hub`에 `rm_bg_*` 활동·`tab_mark_seen` 노출 금지.

## 산출물과 독립·통합 완료 기준
- `crates/ddg-agents-host` 컴파일·테스트 PASS.

## 병렬 작업 주의사항과 공유 파일
- **병렬 그룹 B**: `T4-B`(`ddg-agents-cli`)와 파일 비충돌 병렬 구현 가능(통합 `cli_e2e`는 `T4-A`+`T4-B` 완료 후 실행).

## 차단 조건과 처리 방법
- 기존 `plugin_host` 데모 테스트 호환 유지.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.

---

# 태스크: T4-B — `ddg-agents-cli` `ddg run` · `ddg agent` · `--agent` 구현 (병렬 그룹 B)

## 목표
`ddg run [agent]`, `ddg agent <add|set|ls|show|default|rm>`, 기존 `room create`/`session create`/`session split` `--agent <id>` 옵션, `ls`/`session ls` 활동 상태 출력을 TDD로 구현한다.

## 태스크 유형: 구현
## 수행 역할과 역할 정의의 정확한 경로
- `implementer` — `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` (로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`)

## 실행 상태: 착수 가능 (`T1`, `T2-B` 완료 후, `T4-A`와 병렬 가능)
## 선행 태스크와 착수 조건
- `T1`, `T2-B` 완료. (`cli_e2e` 통합 테스트는 `T4-A` 완료 후 실행).

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음.
- 허용된 승인 준비·조건 확인 작업: `crates/ddg-agents-cli/src/{args.rs, main.rs, room.rs, session.rs, output.rs, SKILL.md}` 확인.
- 대기 중 계속할 독립 작업: `T4-A`와 병렬 진행.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `D-3`: `ddg agent show` 기본 `env` 값 마스킹, `--reveal-env` 시 평문 출력.
- `DNL-14` / `F-05`: 승격된 탭의 불변 `DDG_ROOM_ID=rm_bg_*` 함정을 피하기 위해 `DDG_HOST_ID == hello.host_id`일 때 `DDG_SESSION_ID`로 라이브 카탈로그 조회해 실제 가시 룸 확인; 원격 호스트에는 로컬 `current_dir()` 절대 미전송.

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [DESIGN_UI.md](DESIGN_UI.md#1-확정-ux-5대-흐름-요약-phase-c--리뷰-반영-확정) (흐름 1)
- [DESIGN_DATA.md](DESIGN_DATA.md#1-필수-validation-및-신뢰-경계-확정-표-13개-경계) (경계 #11)
- [TESTS.md](TESTS.md#6-cratesddg-agents-cli-2그룹) (`UT-CLI-01`, `UT-CLI-02`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `crates/ddg-agents-cli/src/{args.rs, agent.rs, run.rs, main.rs, room.rs, session.rs, machine.rs, output.rs, SKILL.md}`, `crates/ddg-agents-cli/tests/cli_e2e.rs`
- 쓰기 금지: 그 외 크레이트

## 수행 내용
1. `args.rs`: `multi_flags` 추가, `ddg run [agent]`(`--room`/`--new-room` 불리언+`--name`/`--bg`·`--background`/`--cwd`/`--title`/`--cols`/`--rows`/`-- <extra-args>`), `ddg agent <add|set|ls|show|default|rm>`(`--env`, `--unset-env`, `--host-override`, `--reveal-env`), `room create`/`session create`/`session split` `--agent <id>`, `--token` 비에코 차단 유지.
2. `agent.rs`: `ddg-agents-transport::agent` 공용 모듈 호출(검증 중복 금지), `show` 기본 마스킹/`--reveal-env`, 오류 시 값 비에코.
3. `run.rs`: 라이브 카탈로그 기반 현재 룸 조회, 외부 셸 새 가시 룸 생성 + 즉시 attach(`Ctrl-]` detach 안내), `--bg`/`--background` 백그라운드 반환, 비-TTY/`--json` 즉시 attach 거부, 원격 CWD 규칙, 부모 `std::env` 미복사.
4. `machine.rs:348`: 업그레이드 완료 시 이미 핀된 `expected_host_id` 보존(불일치 Fail-closed).
5. `room.rs`/`session.rs`/`main.rs`/`SKILL.md`: `--agent` 연동, `ls`/`session ls`에 `agent_profile`/`agent_status`/`foreground` 표시(`--json` 포함), `SKILL.md` 매뉴얼 갱신.

## 구현 착수 근거
- 변경 대상: `ddg-agents-cli` 소스 및 `cli_e2e.rs`.
- 계약: `A1`, `A2`, `D-3`, `DNL-14`, `F-03`, `F-05`, `E-SEC-07`.
- 검증 방법: `UT-CLI-01`, `UT-CLI-02` TDD + `T4-A` 완료 후 `cli_e2e`.

## 필수 Validation과 실패 처리
- `DESIGN_DATA.md` 경계 #11 준수(Fail-closed, 값 비에코).

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `UT-CLI-01`, `UT-CLI-02` + 기존 `cli_e2e` PASS.
- 같은 태스크에서 작성·수정할 유닛 테스트: `UT-CLI-01`, `UT-CLI-02` (`TESTS.md` §6).
- 실행할 테스트와 명령: `cargo test -p ddg-agents-cli` (`T4-A` 완료 후 전체 `cli_e2e` 포함 재확인).
- 테스트 데이터·환경과 독립적인 기대값 근거: 확정 CLI 문법·`host_service.rs:1493` 불변 env 계약·비에코 원칙.
- 통과 기준과 남는 검증 범위: 경고 0개 + 전체 PASS; 실제 `pi`/`codex`·Docker는 `T6-A`.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-A`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- `ddg` 무인자 `HELP` 및 `-- <cmd>` 생략 `room create`/`session create`의 `DefaultShell` 기본 동작을 변경하지 않는다. `agents.json` 검증 로직을 CLI에 중복 구현하지 않는다.

## 산출물과 독립·통합 완료 기준
- `crates/ddg-agents-cli` 단위·통합 테스트 PASS.

## 병렬 작업 주의사항과 공유 파일
- **병렬 그룹 B**: `T4-A`와 파일 비충돌 병렬 구현.

## 차단 조건과 처리 방법
- 없음.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.

---

# 태스크: T5 — `native/` GPUI 클라이언트 아이콘 · 상태 · 에이전트 리스트 · Peek · 런처 · 설정 모달 구현

## 목표
확정 UX([DESIGN_UI.md](DESIGN_UI.md) ASCII 아트 5종)와 GPUI 0.2.2 제약을 준수해 `icons.rs`, `agent_list.rs`, `agent_launcher.rs`, `agent_modal.rs`, `ui_scale.rs`, `config_watch.rs`, `host_bridge.rs`, `main.rs`를 TDD와 함께 구현한다.

## 태스크 유형: 구현
## 수행 역할과 역할 정의의 정확한 경로
- `implementer` — `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` (로컬 `pi --provider openai --model gpt-6.2-sol --thinking high`)

## 실행 상태: 착수 가능 (`T2-B`, `T4-A` 완료 후)
## 선행 태스크와 착수 조건
- `T2-B`, `T4-A` 완료.

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음.
- 허용된 승인 준비·조건 확인 작업: `native/src/{main.rs, host_bridge.rs, layout.rs, ui_scale.rs, config_watch.rs, machine_modal.rs}`, `native/Cargo.toml` 확인(`roxmltree` 의존성 확인).
- 대기 중 계속할 독립 작업: 없음.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `D-SEC-04`: 로컬 SVG 검증에 워크스페이스 `Cargo.lock`에 이미 포함된 `roxmltree` XML AST 화이트리스트 채택(`native/Cargo.toml`에 `roxmltree` 직접 의존성 추가).
- `RC-04`: `ui_scale.rs:108` 및 `config_watch.rs:47,86`의 `UiPrefs` 필드 유실 버그를 in-place 정규화로 함께 수정.
- `PL F-08`: 4개 오버레이(`MachineModal`, `AgentSettingsModal`, `RenameModal`, `AgentLauncher`)를 단일 `ActiveOverlay` enum 통합.

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [DESIGN_UI.md](DESIGN_UI.md) (전 섹션 · ASCII 아트 5종)
- [DESIGN_DATA.md](DESIGN_DATA.md#1-필수-validation-및-신뢰-경계-확정-표-13개-경계) (경계 #12, #13)
- [TESTS.md](TESTS.md#7-native-ddg-agents-native-4그룹) (`UT-NATIVE-01`..`UT-NATIVE-04`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `native/Cargo.toml`, `native/assets/icons/**`, `native/src/{icons.rs, agent_list.rs, agent_launcher.rs, agent_modal.rs, ui_scale.rs, config_watch.rs, host_bridge.rs, main.rs}`
- 쓰기 금지: `crates/**`, `schema/**`

## 수행 내용
1. `icons.rs` + `native/assets/icons/`: `DdgAssetSource`(`Application::new().with_assets`), 내장 SVG(`include_bytes!`: 주요 에이전트·도구·UI 스피너/터미널/핀/설정), 로컬 `~/.config/ddg/icons/` 단일 평면 파일명 + 단일 fd `O_NOFOLLOW` + `fstat` 일반파일 + `≤128 KiB` + `roxmltree` XML AST 화이트리스트(`512px` OOM 가드, `version="1.1"` 비오탐) + `custom/<name>#<fnv64>` 해시 캐시, 원격 세션 내장 키 전용.
2. `agent_list.rs`: `priority`/`grouped`/`manual` 3종 정렬, 핀/비핀 섹션 내부 서브트리 단위 수동 이동, 자동 `manual` 전환 스냅샷, `Blocked > Error > Working > Done(unseen) > Idle` 집계·긴급 점프, 실행 중 `rm_bg_*` 포함·종료 즉시 제거, 순환 `parent` 평탄화.
3. `agent_launcher.rs`: 4지선다(`[현재 룸 새 탭]|[분할]|[새 룸]|[백그라운드]`), 필터(`1..9` 텍스트 입력), `Cmd/Ctrl+1..9` 실행, 빈/오류 변형.
4. `agent_modal.rs`: `AgentSettingsModal`(`w(640).max_w_full()`, `ddg-agents-transport::agent`·`split_safe_tokens` 직접 호출, `argv` 칩 프리뷰, `env` 마스킹/토글·동적로더 4종 경고, 원격 있을 때 `host_overrides` 3상태, 인라인 2단계 삭제/초기화, CAS 외부 변경 충돌 배너, `.corrupt.<ts>` 복구 카드).
5. `ui_scale.rs` & `config_watch.rs`: `UiPrefs` v2 확장(`agent_sort`, `pinned_agents`, `manual_agent_order`, `collapsed_sections`), `load_or_default`/`config_watch` in-place 정규화로 필드 보존, `agents.json`/`programs.json` 감시 연동(일시 구문 오류 시 마지막 유효 스냅샷 유지).
6. `host_bridge.rs`: `attach_tab(..., want_control: bool)`, `release_control`, `move_tab`, `mark_seen`, `create_*_with_executor`, `ConnectionState::Connected { host_id, conn_id, features }`.
7. `main.rs`: `ActiveOverlay`, `KeyboardFocusZone::{ Terminal, AgentList }`, 2단 사이드바(45% cap + `rooms` 접힘 시 해제 + `sidebar <= scale.px(185.)` 축약), 분할 패인 22px 헤더(`UiScale` 반영 차감 + `Readonly`/`Stopped` 오프셋 + 구버전 배너 `top` 반영), Peek 모드(`want_control=false`, `[제어 요청]`, `Readonly` `Esc` 닫기, `Control` `Esc` 터미널/`Cmd+W` 닫기 + `release_control` 3분기 + `FrameKind::Close` + 종료 스냅샷 보존), `observe_window_activation` 기반 `seen`, 8Hz/0Hz `Working` 스피너 게이트, `Ctrl+Shift+,`/`<` 폴딩, `finish_machine_install` `expected_host_id` 보존 + `Ssh|ContainerSsh` 인라인 2단계 확인.

## 구현 착수 근거
- 변경 대상: `native/` 8개 소스 + 내장 SVG 에셋.
- 계약: `DESIGN_UI.md` 5종 ASCII 아트, `D-SEC-04`/`09`, `PL F-01`..`F-11`, `RC-01`..`RC-06`.
- 검증 방법: `UT-NATIVE-01..04` TDD + `cargo test --workspace` + `--check-vt`.

## 선택한 해법의 한계·재검토 조건
- `DESIGN_UI.md` §3.4 (`modal_text_input` 한글 IME 조합·중간 커서 제외).

## 필수 Validation과 실패 처리
- `DESIGN_DATA.md` 경계 #12(Fail-open 폴백+1회 경고), #13(Fail-open).

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `UT-NATIVE-01..04` + 기존 `ddg-agents-native` 테스트 + `cargo test --workspace` + `--check-vt` PASS.
- 같은 태스크에서 작성·수정할 유닛 테스트: `UT-NATIVE-01`..`UT-NATIVE-04` (`TESTS.md` §7).
- 실행할 테스트와 명령: `cargo test --workspace && cargo run -p ddg-agents-native -- --check-vt`
- 테스트 데이터·환경과 독립적인 기대값 근거: W3C SVG AST 화이트리스트, FNV-1a 결정성, `UiScale` 배율 수식, `D-SEC-09` 3분기.
- 통과 기준과 남는 검증 범위: 경고 0개 + 전체 PASS; 실창 시각·키보드·SVG 래스터화는 `T6-B`.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-B`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- `with_animation().repeat()` 상시 스피너 금지. 원격 세션의 로컬 `icons/` 조회 금지. `agents.json` 검증 로직 중복 구현 금지. `Image::from_bytes(ImageFormat::Svg, ...)` 직접 호출 금지(`DdgAssetSource` 사용).

## 산출물과 독립·통합 완료 기준
- 전체 워크스페이스 빌드·테스트·`--check-vt` PASS.

## 병렬 작업 주의사항과 공유 파일
- 단독 구현 태스크(완료 후 `T6-A`/`T6-B` 검증 진입).

## 차단 조건과 처리 방법
- GPUI 0.2.2 API 제약 발견 시 `gpui-0.2.2` 소스 직접 확인 후 최소 우회·기록.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.

---

# 태스크: T6-A — 워크스페이스 통합 검증 및 에이전트 실환경 시나리오(`A1`–`A3`) 수행

## 목표
전 크레이트 회귀 테스트(`cargo test --workspace`) 및 `SCENARIOS.md`의 에이전트 수행 시나리오 `A1-core`, `A1-live-agents`(`pi 1.6.1`/`codex 0.166.0`), `A2`(`SIGKILL` 1 MiB tail + `catalog.json` 불변), `A3`(Docker 멀티호스트 + 구버전 업그레이드 `holder` 보존)를 완전 격리 스크래치 환경에서 수행하고 증거를 수집한다.

## 태스크 유형: 검증
## 수행 역할과 역할 정의의 정확한 경로
- Antigravity Lead + 검증 실행 (`/Users/dotdot/.agents/skills/ddg-planning/roles/test-designer.md`, `/Users/dotdot/.agents/skills/ddg-planning/roles/completion-reporter.md`)

## 실행 상태: 착수 가능 (`T4-B`, `T5` 완료 후)
## 선행 태스크와 착수 조건
- `T4-A`, `T4-B`, `T5` 완료 및 Antigravity `git diff` 독립 검수 통과.

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음(모든 실행은 `mktemp -d` 격리 + 루프백 Docker `-p 127.0.0.1:<port>:22` + `trap` 정리).
- 허용된 승인 준비·조건 확인 작업: `SCENARIOS.md` 공통 격리 프리앰블 적용.
- 대기 중 계속할 독립 작업: `T6-B`(사람 GUI 실기)와 병렬 가능.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `E-SEC-09`: `HOME`/`DDG_DIST_DIR`/`SSH_AUTH_SOCK`/부모 `DDG_*` 완전 격리 및 루프백 전용 Docker 바인딩 필수.

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [SCENARIOS.md](SCENARIOS.md) (`A1-core`, `A1-live-agents`, `A2`, `A3`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `/tmp/ddg-scn-*` 스크래치, `docs/archive/report/YYYY-MM-DD/agent-status-icons-profiles/BATCH_*.md`
- 쓰기 금지: 프로덕션 코드 수정 금지(실패 발견 시 별도 수정 루프로 회귀)

## 수행 내용
- `cargo check --workspace`, `cargo test --workspace`, `cargo run -p ddg-agents-native -- --check-vt` 실행.
- `A1-core`, `A1-live-agents`, `A2`, `A3` 순차 실행 및 증거 로그 수집, 잔존 프로세스/임시 컨테이너 0개 확인.

## 필수 Validation과 실패 처리
- 환경 제약(Docker 미가동, `pi`/`codex` 인증 부재) 시 절대 통과 처리하지 않고 해당 항목만 `Unverified` + 사유 기록.

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `A1-core`·`A2`·`A3` Pass + `A1-live-agents` 실측 표 확보.
- 같은 태스크에서 작성·수정할 유닛 테스트: 없음(검증 태스크).
- 실행할 테스트와 명령: `cargo test --workspace` + `SCENARIOS.md` `A1`–`A3` 절차.
- 테스트 데이터·환경과 독립적인 기대값 근거: `SCENARIOS.md` 각 시나리오 관측 지점·합격 기준.
- 통과 기준과 남는 검증 범위: `A1-core`/`A2`/`A3` PASS; GUI 시각·접근성은 `T6-B`.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-B`.

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- 실운영 `~/.config/ddg`, `~/.local/share/ddg`, `~/.ssh/config`, 외부 원격 서버 접근 금지.

## 산출물과 독립·통합 완료 기준
- 배치 검증 보고서(`BATCH_*.md`) + 잔존 프로세스/컨테이너 0개 증적.

## 병렬 작업 주의사항과 공유 파일
- `T6-B`와 병렬 수행 가능.

## 차단 조건과 처리 방법
- 실패 발견 시 실패 원인을 구현 태스크로 환류.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.

---

# 태스크: T6-B — 사람 GUI 실기 검증 (`H1`–`H2`)

## 목표
유닛 테스트에서 제외한 GPUI 실창 시각·레이아웃·키보드·SVG 핫스왑·`observe_window_activation`·0Hz 유휴를 사람이 직접 실행해 판정한다.

## 태스크 유형: 검증
## 수행 역할과 역할 정의의 정확한 경로
- 사용자(사람) + Antigravity 기록 (`/Users/dotdot/.agents/skills/ddg-planning/roles/test-designer.md`)

## 실행 상태: 착수 가능 (`T5` 완료 후)
## 선행 태스크와 착수 조건
- `T5` 완료.

## 단계별 승인 조건과 승인 충족 준비 작업
- 비가역 단계·대기 조건: 없음(격리 환경 GUI 실행).
- 허용된 승인 준비·조건 확인 작업: `SCENARIOS.md` `H1`·`H2` 체크리스트 및 실행 명령 준비.
- 대기 중 계속할 독립 작업: `T6-A`와 병렬 가능.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- 사람 수행 완료 전까지 상태를 반드시 `Unverified`로 표기한다.

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- [SCENARIOS.md](SCENARIOS.md) (`H1`, `H2`)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: 검증 체크리스트 결과 기록(보고서).
- 쓰기 금지: 코드 수정 금지.

## 수행 내용
- `SCENARIOS.md` `H1`(2단 사이드바·22px 헤더·0Hz 유휴·런처 4지선다·정렬/핀/Peek) 및 `H2`(`AgentSettingsModal`·로컬 SVG 핫스왑·손상 복구·160px 축약) 수행.

## 필수 Validation과 실패 처리
- 미수행 항목은 `Unverified` 유지.

## 테스트 (필수)
- 확인할 요구사항·완료 기준: `H1`·`H2` 전 체크리스트 확인.
- 같은 태스크에서 작성·수정할 유닛 테스트: 없음(사람 실기 검증 태스크).
- 실행할 테스트와 명령: `SCENARIOS.md` `H1`·`H2` 절차(`cargo run -p ddg-agents-native`).
- 테스트 데이터·환경과 독립적인 기대값 근거: `DESIGN_UI.md` ASCII 아트 5종 및 `SCENARIOS.md` 판정 기준.
- 통과 기준과 남는 검증 범위: 시각·키보드·유휴 CPU·`ui.json` 재시작 복원 이상 없음.
- 사용자 시나리오·실제 환경 검증 의존 태스크: 없음(최종 실기).

## 비가역 실행
- 없음.

## 제외 범위와 변경 금지 계약
- 실운영 `~/.config/ddg` 오염 금지.

## 산출물과 독립·통합 완료 기준
- `H1`·`H2` 실기 판정 기록(또는 `Unverified` 사유).

## 병렬 작업 주의사항과 공유 파일
- `T6-A`와 병렬 가능.

## 차단 조건과 처리 방법
- 헤드리스 환경이면 `Unverified`로 기록하고 `T6-C`에서 부분 완료/남은 위험으로 보고.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.

---

# 태스크: T6-C — 최종 완료 보고서(`FINAL.md`) · 아카이브 이관 · `main` 머지 인계

## 목표
구현·검증 완료 후 최종 보고서(`FINAL.md`) 작성, 완료된 계획의 `docs/archive/plan/agent-status-icons-profiles/` 이동 및 상호 링크/README 인덱스 갱신, 사용자 승인 시 `main` 머지/squash 커밋 본문 작성을 완료한다.

## 태스크 유형: 검증 / 종료
## 수행 역할과 역할 정의의 정확한 경로
- `completion-reporter` — `/Users/dotdot/.agents/skills/ddg-planning/roles/completion-reporter.md` (Antigravity Lead)

## 실행 상태: 착수 가능 (`T6-A` 완료 후; `T6-B` 미수행 시 부분 완료 보고로 분기)
## 선행 태스크와 착수 조건
- `T1`–`T5`, `T6-A` 완료(전체 완료·아카이브 이관은 `T6-B` 포함 또는 사용자 명시 확인 후 수행).

## 단계별 승인 조건과 승인 충족 준비 작업
- **비가역 단계·대기 조건**: `main` 머지 및 `git push`는 **사용자 명시 승인 전까지 대기**(`agent-orchestration-policy.md` §2-6).
- 허용된 승인 준비·조건 확인 작업: `FINAL.md` 초안·머지 커밋 본문 초안 작성, `git status`/`git diff` 정리, 상호 링크 검증.
- 대기 중 계속할 독립 작업: 보고서 목차 갱신.

## 요구·설계 변경의 옵션·선택 근거·대안·영향
- `T6-B`가 `Unverified`이면 계획을 `docs/archive/plan/`으로 성급히 이동하지 않고 `BATCH_*.md`로 부분 완료 보고 후 `T6-B` 완료 시 `FINAL.md` + 이관 수행.

## 필수 범위 밖 작업의 필요성과 범위 내 다른 방법
- 해당 없음.

## 참조할 확정 요구사항·설계·프로젝트 지침
- `/Users/dotdot/.agents/skills/ddg-planning/references/TEMPLATES.md#최종-완료-보고서-양식`
- [PLANNING.md](../../report/2026-11-26/agent-status-icons-profiles/PLANNING.md)

## 작업 영역·쓰기 허용/금지 범위
- 쓰기 허용: `docs/archive/report/YYYY-MM-DD/agent-status-icons-profiles/{README.md, FINAL.md}`, `docs/archive/plan/agent-status-icons-profiles/**`, `docs/plan/README.md`, `docs/archive/README.md`
- 쓰기 금지: 프로덕션 코드 변경 금지

## 수행 내용
1. `FINAL.md` 작성 및 날짜별 `README.md` 목차 갱신(`PLANNING.md` 보존, 덮어쓰기 금지).
2. 전체 범위 완료 시 `docs/plan/agent-status-icons-profiles/` → `docs/archive/plan/agent-status-icons-profiles/` 이동 + 상대 링크·`docs/plan/README.md`·`docs/archive/README.md` 갱신.
3. 사용자 승인 시 `main` 머지/squash 커밋 본문에 보고서·계획 경로, 결정 근거(`A1`–`A6`, `UX-1`–`3`, `D-1`–`4`, `RC-01`–`06`), 제외 대안, 승인·검증 결과 및 남은 위험(`R-01`–`R-04`)을 자족적으로 포함.

## 필수 Validation과 실패 처리
- 미실행 검증을 완료로 표기하지 않는다. `archive`가 `.gitignore`에 걸려 있으면 강제 `git add -f` 하지 않고 커밋 본문에 핵심 요약 유지.

## 테스트 (필수)
- 확인할 요구사항·완료 기준: 모든 상대 링크 유효성 및 `git status` 청결 확인.
- 같은 태스크에서 작성·수정할 유닛 테스트: 없음(종료 보고 태스크).
- 실행할 테스트와 명령: 링크 점검 + `git status --short`.
- 테스트 데이터·환경과 독립적인 기대값 근거: `TEMPLATES.md` 필수 섹션 전 항목 존재.
- 통과 기준과 남는 검증 범위: 깨진 링크 0개, 승인 없는 비가역 실행 0건.
- 사용자 시나리오·실제 환경 검증 의존 태스크: `T6-A`, `T6-B`.

## 비가역 실행
- 대상·영향과 비가역인 이유: `main` 브랜치 머지 및 원격 `git push`(히스토리 공개·브랜치 변경).
- 선택 이유와 근거·결정 출처: 완료된 구현·검증 결과를 기준 브랜치에 반영.
- 다른 옵션과 제외 이유: 로컬 작업 브랜치에 영구 방치 시 후속 마일스톤과 분기.
- 복구 가능 범위: `git revert` 가능하나 원격 푸시 후 히스토리 재작성 불가.
- 실행 전 승인·권한 조건: **사용자의 명시적 머지/푸시 승인 필수**(보고서 작성은 승인이 아님).

## 제외 범위와 변경 금지 계약
- 승인 없는 `git push`·배포 금지. `PLANNING.md` 덮어쓰기 금지.

## 산출물과 독립·통합 완료 기준
- `FINAL.md` + 갱신된 목차/인덱스 + (승인 시) `main` 머지 커밋.

## 병렬 작업 주의사항과 공유 파일
- 최종 직렬 종료 태스크.

## 차단 조건과 처리 방법
- 사용자 머지 승인 미도착 시 `main 머지 상태: 미수행(승인 대기)`로 기록하고 종료.

## 결과 보고
- 변경 내용 / 검증 결과 / 미검증 항목 / 남은 위험 보고.
