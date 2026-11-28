# 작업 결과 보고서: 탭 아이콘 · OSC 7501 에이전트 상태 · 에이전트 리스트 · `ddg` 기본 에이전트 설정

## 보고 대상: 계획 작성
## 대상 태스크·배치 ID와 포함 범위
- **대상**: `/ddg-planning` Phase A–G 전체 (Herdr vs DDG Agents 설계 비교, 요구사항·UX·UI·데이터/네트워크·보안·테스트·시나리오·실행 태스크 `T0`–`T6-C` 확정)
- **포함 범위**: 계획·설계·리뷰·태스크 명세 작성 완료. **프로덕션 코드 구현·테스트 실행·커밋·`main` 머지는 미수행** (계획 작성 완료와 전체 작업 완료를 명확히 구분).

## 해당 범위 상태: 완료 (계획 작성 완료 · 구현 착수 가능)
## 전체 계획 상태와 남은 범위
- **전체 계획 상태**: 플래닝 완료 (`2026-11-26`)
- **남은 범위**: `T0`(사전 환경 확인 및 `docs/spec/requirements.md` §1 갱신) → `T1`–`T5`(7개 크레이트 TDD 구현) → `T6-A`(에이전트 실환경 시나리오 `A1`–`A3`) · `T6-B`(사람 GUI 실기 `H1`–`H2`) → `T6-C`(`FINAL.md`·아카이브 이관·승인 시 `main` 머지)

## 산출물: 계획·설계·태스크 경로
- 활성 계획서(목차): [docs/plan/agent-status-icons-profiles/README.md](../../../plan/agent-status-icons-profiles/README.md)
- UX·Presentation Layer 설계 (필수 ASCII 아트 5종): [docs/plan/agent-status-icons-profiles/DESIGN_UI.md](../../../plan/agent-status-icons-profiles/DESIGN_UI.md)
- Data Network Layer·13개 신뢰 경계·제외 근거: [docs/plan/agent-status-icons-profiles/DESIGN_DATA.md](../../../plan/agent-status-icons-profiles/DESIGN_DATA.md)
- TDD 유닛 테스트 (7개 크레이트 22그룹): [docs/plan/agent-status-icons-profiles/TESTS.md](../../../plan/agent-status-icons-profiles/TESTS.md)
- 실제 환경 사용자 시나리오 (`A1`–`A3`, `H1`–`H2`): [docs/plan/agent-status-icons-profiles/SCENARIOS.md](../../../plan/agent-status-icons-profiles/SCENARIOS.md)
- 실행 태스크 명세: [TASKS_CORE.md](../../../plan/agent-status-icons-profiles/TASKS_CORE.md) (`T0`–`T3`), [TASKS_HOST_UI.md](../../../plan/agent-status-icons-profiles/TASKS_HOST_UI.md) (`T4-A`–`T6-C`)

## 확정 요구사항·설계와 결정 출처
- **Phase B 사용자 확정 (`A1`–`A6`)**:
  - `A1`: 전용 진입점(`ddg run [agent]`, `ddg agent ...`, 기존 명령 `--agent`) + 실행기 프로파일(`argv`/비민감 `env`), 기존 `DefaultShell` 100% 호환, `docs/spec/requirements.md` §1 CLI 조항 공식 갱신
  - `A2`: `ddg run` 기본 = 가시 룸 세션 생성 + 즉시 인터랙티브 attach (`--bg`/`--background` 숨김 백그라운드 비대화 생성)
  - `A3`: 로컬 통합 설정 `~/.config/ddg/agents.json`(+`host_overrides`) CLI·GUI 공유, 일반 `+` 탭은 셸 유지 + 별도 에이전트 런처 제공
  - `A4`: OSC 7501 보고 + `ddg` 에이전트 실행 세션 리스트 포함, `rm_bg_*`는 실행 중일 때만 트리 표시(종료 시 즉시 제거)
  - `A5`: 내장 SVG + OSC 7501 `app` + OS `foreground` 최소 식별자(전체 `argv` 비노출) + 로컬 `~/.config/ddg/icons/`·`programs.json` 오버라이드 1차 포함
  - `A6`: P0 + P1(핀·정렬·수동순서·GUI 설정 모달) 단일 마일스톤 일괄 계획
- **Phase C 사용자 확정 (`UX-1`–`UX-3`)**:
  - `UX-1`: 백그라운드 Peek 기본 `want_control = false`(`Readonly`) + 배너에 `[제어 요청]` 임시 키 입력 허용 + 닫기 시 자동 반납
  - `UX-2`: 전체 리스트 수동 순서 이동 허용(이동 시 `manual` 자동 전환)
  - `UX-3`: 사이드바 접이식 2단 섹션(상단 `에이전트` 최대 45% + 하단 `룸`) + 외부 셸 `ddg run`은 현재 `cwd` 새 가시 룸 생성
- **Phase D 사용자 확정 (`D-1`–`D-4`)**:
  - `D-1`: Peek `Control` 모드 `Esc`=터미널 전달, `Cmd/Ctrl+W`·`[닫기]`=Peek 종료(+ `release_control`/`detach_channel`)
  - `D-2`: 런처 4지선다(`[현재 룸 새 탭]|[분할]|[새 룸]|[백그라운드]`) + 핀/비핀 섹션 내부끼리 서브트리 단위 수동 이동
  - `D-3`: `ddg agent show` 기본 `env` 마스킹(`--reveal-env` 평문) + 동적 로더 4종(`LD_PRELOAD`/`LD_AUDIT`/`DYLD_INSERT_LIBRARIES`/`DYLD_FORCE_FLAT_NAMESPACE`) 차단(`LD_LIBRARY_PATH` 허용)
  - `D-4`: `host_overrides.env: BTreeMap<String, Option<String>>` 상속 키 제거 전용(와이어 변경 없음) + 구버전 원격 호스트 `ps` 인자 정확 매칭(`ddg-agents-host serve`/`ddg-terminal-server core`만, `holder` 제외) 1회 안전 재시작 후 `hello.features` 검증

## 요구사항·가정 옵션과 설계 옵션
- **선택한 옵션·이유·근거·영향**:
  - **활동 상태 RAM 전용 + `catalog.json` v2 유지**: 고빈도 OSC 7501/`foreground`로 인한 `fsync` 증폭 및 v3 비호환 `.corrupt` 초기화를 방지하고, `TabActivityChanged`/`TabSeenChanged`를 `catalog_subscribe` 전용 채널(`after_reply`)로 전달해 스냅샷-델타 역전 차단.
  - **`Relay` 단일 FIFO 응답 버퍼 + `TERM_PROGRAM=ddg`**: 로컬 설치 `pi 1.6.1` 실측에서 `7501;?` 응답이 `DA1`보다 먼저 도착해야 활성화되고 `TERM_PROGRAM=ghostty` 스푸핑 시 Kitty 이미지 오예약이 발생함을 확인해 FIFO 버퍼와 외부 터미널 변수 scrub 채택.
  - **로컬 SVG `roxmltree` XML AST 화이트리스트 + `DdgAssetSource` + 콘텐츠 해시 키**: GPUI `0.2.2` 소스 검증에서 `Image::from_bytes(Svg)`의 BGRA/1.0x 결함과 `Resource::Embedded` 영구 캐시·`Pixmap` OOM 위험을 확인해 정적 AST 화이트리스트(`128 KiB`, `512px`)와 해시 키(`custom/<name>#<fnv64>`) 채택.
  - **`AgentProfile.argv: Vec<String>` + 바이트 비교 CAS (`flock`)**: 기존 `ExecutorSpec::Command`와 통일하고 `sha2` 신규 의존성 및 `color`/`program_id`/`turn`/`tab_activity_snapshot` 중복·AI slop 제거.
- **다른 방법과 제외 이유**:
  - `catalog.json` v3 마이그레이션 및 활동 영속화 → 구버전 호스트 `.corrupt` 초기화 위험 + 디스크 `fsync` 폭주.
  - 문자열 블랙리스트 기반 SVG 필터 → `<SCRIPT>`/`<svg:script>`/`data:image/svg+xml` 우회 및 `version="1.1"` 오탐.
  - `with_animation().repeat()` GPUI 스피너 → 60/120Hz 상시 배터리 소모(125ms/0Hz `drain` 게이트로 대체).
  - 원격 OS `cmd.env_remove` 와이어 확장 → 전 계층 프로토콜 변경 대비 상속 키 제거(`Option<String>`)로 요구 충족.
- **필수 범위 밖 작업의 필요성과 범위 내 대안**:
  - 기존 `native/src/ui_scale.rs:108` 및 `native/src/config_watch.rs:47,86`의 `UiPrefs` 필드 유실 버그 수정과 `plugin_host.rs` 5개 반환 경로 `redact_for_plugin`·`tab_create` idempotency 스코프 분리는 새 `ui.json`/`executor.env` 계약의 필수 전제이므로 `T4-A`·`T5`에 포함했다. 프로젝트 로컬 `.ddg/config.json` 및 7501 미지원 래퍼 자동 주입은 범위 밖(P2)으로 보류.

## 태스크 순서·의존관계·병렬 그룹
- `T0` → `T1` (`proto` + OpenRPC) → **병렬 그룹 A** (`T2-A` `vt` ∥ `T2-B` `transport`) → `T3` (`terminal-server`) → **병렬 그룹 B** (`T4-A` `host` ∥ `T4-B` `cli`) → `T5` (`native` GPUI) → **병렬 검증** (`T6-A` 에이전트 시나리오 ∥ `T6-B` 사람 GUI 실기) → `T6-C` (최종 보고·아카이브·승인 시 `main` 머지).

## 비가역 실행의 결정 기록
- **관련 태스크**: `T6-C` (`main` 머지 및 원격 `git push`) — 계획 작성 시점 **미수행**.
- **대상·영향과 비가역인 이유**: 기준 브랜치 히스토리 변경 및 원격 공개.
- **선택 이유·근거와 결정 출처**: 구현·검증 완료 후 작업 통합(`agent-orchestration-policy.md` §2-6).
- **다른 옵션과 제외 이유**: 별도 브랜치 장기 방치 시 메인라인과 괴리.
- **복구 가능 범위**: 머지 후 `git revert` 가능(푸시 후 강제 히스토리 재작성 불가).
- **실행 전 승인·권한 조건과 실제 실행 여부**: **사용자 명시 승인 필수** / **실제 실행 여부: 미실행** (계획 보고는 승인이 아님).

## 역할별 결과·리뷰와 검증 증거
| 페이즈 | 역할 | 실행 모델 | 상태 | 핵심 산출물·조정 결과 |
|---|---|---|---|---|
| **A** | 환경 확인 (`dotdotgod config`, 도구·모델) | Gemini 4 Neon (High) | 완료 | `invoke_subagent` 병렬 지원 확인, 계획 루트 `docs/plan/` 확정 |
| **B** | `user-requirements` ∥ `data-requirements` ∥ `validation-requirements` | Gemini 4 Neon (High) | 완료 | Herdr vs DDG 비교, C1–C5 식별, Round 1 질문 `A1`–`A6` 사용자 확정 |
| **C** | `ux-designer` → `ux-reviewer` ∥ `security-reviewer` | Gemini 4 Neon (High) | 완료 | 5대 UX 흐름, UX 결함 8건·보안 결함 8건 반영, `UX-1`–`UX-3` 사용자 확정 |
| **D** | `presentation-layer-designer` ∥ `data-network-layer-designer` → `pl-reviewer` ∥ `dnl-reviewer` ∥ `security-reviewer` ∥ `validation-requirements(2차)` | Gemini 4 Neon (High) | 완료 | ASCII 5종·GPUI 0.2.2 검증·`pi`/`codex` 실측, PL 11건·DNL 15건·보안 9건(`D-SEC-01`–`09`)·13개 경계 표 반영, `D-1`–`D-4` 사용자 확정 |
| **E** | `test-designer` → `test-reviewer` ∥ `security-reviewer(테스트)` ∥ `review-conductor` | Gemini 4 Neon (High) | 완료 | 7크레이트 22그룹 TDD + 5종 시나리오, `RC-01`–`06`·`F-01`–`13`·`E-SEC-01`–`09` 동기화 완료, 전체 판정 **`진행 가능`** |
| **F–G** | `task-planner` · `completion-reporter` | Gemini 4 Neon (High) | 완료 | `docs/plan/agent-status-icons-profiles/` 7개 문서 + `PLANNING.md` + 목차 갱신 완료 |

- **설계 확정 후 변경 내용·갱신 결과**: `validation-requirements` 2차 표의 `HostState.activities` 필터 문구(`Standard` 전용)를 `review-conductor` `RC-01` 및 `test-reviewer` `F-02` 합의로 "오퍼레이터 채널=`Standard`+실행 중 `rm_bg_*`, 격리는 `plugin_host.rs`·`hub.rs` 경계"로 통일 반영하고, `MAX_ICON_BYTES=128 KiB`·`.corrupt.<ts>`·`AgentState`/`BlockedKind` 명칭을 전 문서·테스트에 동기화했다.

## 차단·미결정·미검증 사항과 남은 위험
- **차단/미결정 사항**: 없음.
- **미검증 사항**: 본 단계는 플래닝 전용이므로 코드 빌드·유닛 테스트·실환경 시나리오(`A1`–`A3`, `H1`–`H2`)는 아직 미실행(`Unverified`).
- **남은 위험**: `R-01`(macOS vs Linux OS `foreground` 권한/래퍼 차이), `R-02`(`codex` v0.166.0 OSC 7501 방출 조건 실측 필요 및 로컬 `claude` 미설치 `Unverified`), `R-03`(GPUI 0.2.2 2단 사이드바 45% 스크롤·Peek `Control` 키 라우팅 실기 확인), `R-04`(원격 비표준 로그인 셸에서의 `ps` 정확 매칭).

## 다음 실행 세션의 착수 조건
1. `T0` 수행: 로컬 `pi auth check --model openai/gpt-6.2-sol` `ready` 확인 및 `docs/spec/requirements.md` §1 갱신.
2. 각 구현 태스크(`T1`–`T5`)는 새 로컬 `pi` 세션(`hc run --bg --name <slug> --cwd /Users/dotdot/Workspace/dotdotgod-agents -- pi --provider openai --model gpt-6.2-sol --thinking high`)에 `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`와 해당 태스크 명세를 전달해 실행하고, 완료마다 Antigravity가 `git diff`·테스트를 독립 검수한다.

## 아카이브·main 머지 기록
- **실제 보고서**: [docs/archive/report/2026-11-26/agent-status-icons-profiles/PLANNING.md](PLANNING.md)
- **보고 목차**: [docs/archive/report/2026-11-26/agent-status-icons-profiles/README.md](README.md)
- **완료 계획**: 현재 활성 경로 유지 — [docs/plan/agent-status-icons-profiles/README.md](../../../plan/agent-status-icons-profiles/README.md) (Phase G에서는 archive로 이동하지 않음)
- **상호 링크·관련 README 인덱스 갱신**: `docs/plan/README.md`, `docs/archive/README.md`, `docs/archive/report/2026-11-26/agent-status-icons-profiles/README.md` 갱신 완료
- **main 머지 상태·커밋**: 미수행 (계획 작성 단계)
- **향후 머지/squash 커밋 본문에 남길 요약 항목**: 계획 경로(`docs/archive/plan/agent-status-icons-profiles/README.md`), 최종 보고서 경로(`docs/archive/report/YYYY-MM-DD/agent-status-icons-profiles/FINAL.md`), 확정 조합(`A1`–`A6`, `UX-1`–`3`, `D-1`–`4`, `RC-01`–`06`), 주요 제외 대안(v3 마이그레이션·문자열 SVG 블랙리스트·`sha2`·`turn`/`tab_activity_snapshot`·60Hz 스피너), 검증 결과(`cargo test --workspace`, `A1`–`A3`, `H1`–`H2` 또는 `Unverified` 사유) 및 잔여 위험(`R-01`–`R-04`).

## 수행·미수행 작업과 다음 착수점
- **수행 완료**: Phase A–G 플래닝, 14개 역할 교차 설계·보안·테스트·통합 리뷰, 계획/설계/테스트/시나리오/태스크/플래닝 완료 보고서 작성.
- **미수행**: `docs/spec/requirements.md` §1 갱신(`T0`), 코드·테스트 구현(`T1`–`T5`), 실환경 검증(`T6-A`/`T6-B`), 아카이브 이관 및 `main` 머지(`T6-C`).
- **다음 착수점**: 사용자 착수 지시 시 `T0` → `T1` 순으로 실행 세션 진행.
