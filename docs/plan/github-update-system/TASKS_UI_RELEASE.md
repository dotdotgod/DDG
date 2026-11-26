# 태스크 명세 (Phase 4–8: CLI/UI · Windows/CI · 검증 · 승인 게이트 · 종료)

[계획 목차](README.md) · [UI·UX](DESIGN_UI.md) · [데이터·네트워크](DESIGN_DATA.md) · [시나리오](SCENARIOS.md) · [선행 태스크(Phase 0–3)](TASKS_CORE.md)

---

## T4-A: `ddg-agents-cli` `ddg update` 5서브커맨드 · `--json` 단일 객체 · `docs/spec/requirements.md` 정합화
- **유형 / 상태**: `구현` / `착수 가능` (`T1-A`, `T2-B`, `T3-A`, `T3-B` 완료 시) | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: Phase 3 완료 필수 · **`PG-4` (`T4-B`와 병렬)** — 쓰기 허용 `crates/ddg-agents-cli/**`, `docs/spec/requirements.md` (루트 `Cargo.toml`·`native/**` 금지).
- **구현 착수 근거**:
  - 변경 대상: `crates/ddg-agents-cli/{Cargo.toml,src/{args.rs,main.rs,output.rs,update.rs(신규),SKILL.md}}`, `docs/spec/requirements.md`.
  - 계약: `Action::Update{Status,Check,Apply,Rollback,Config}` + `__update-apply-helper` 진입점, `-H` 생략 시 **`main::selected()` 미사용·무조건 `local` 고정**, `status`/`check` `-H all` 허용, `apply`/`rollback` `-H all` 거부, `config` `-H` 거부, `--token` 글로벌 비에코 차단, 비-TTY 프롬프트 금지(fail-closed), `PendingHolderRestart`에서 `apply/rollback --yes` 단독 거부 + `--restart-sessions` 필수, `--json` stdout 단일 JSON(NDJSON 금지), 예외 없는 5종 레닥션(`GITHUB_TOKEN=<token>` 예외 삭제), 타 명령 알림/네트워크 0건, `HELP`/`SKILL.md`/`docs/spec/requirements.md` 갱신.
  - 리뷰 반영: `F-02/06/14`, `SEC-UX-04/05/06`, `SEC-D-04/06`, `SEC-E-02/03`.
- **테스트 (TDD)**: `UT-CLI-01`(기존 `args::tests`/`skill_frontmatter_and_operator_commands` 확장 포함), `UT-CLI-02`. 명령: `cargo test -p ddg-agents-cli`. **비가역**: 없음.

## T4-B: `native` GPUI 업데이트 UX (즉시 재시작 A17 · 세션 닫기 A18 · 31종 카드 · 키 트랩)
- **유형 / 상태**: `구현` / `착수 가능` (`T1-A`, `T2-B`, `T3-A`, `T3-B` 완료 시) | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: Phase 3 완료 필수 · **`PG-4` (`T4-A`와 병렬)** — 쓰기 허용 `native/**`만(루트 `Cargo.toml`·`crates/**` 금지).
- **구현 착수 근거**:
  - 변경 대상: `native/{Cargo.toml,src/{update_controller.rs(신규),update_modal.rs(신규),main.rs,host_bridge.rs,machine_modal.rs}}`.
  - 계약: `native --version` GPUI 초기화 전 즉시 출력, `UpdateController`(즉시 재시작 A17 `HandedOffToHelper` → `quitting_for_update_restart = true` → `cx.quit()`, 세션 한정 배너 닫기 A18 `dismissed_available_version`; `PendingRestart`/`skipped_version` 전면 배제), 31종 `UpdateErrorCode` exhaustive 3종 카드 매핑, 1슬롯 배너(`reset_banner` 독립 `self.canvas.y` 합산·`BlockedCard` 중복 제외), 사이드바 `160px`/`200px` 반응형, macOS 메뉴 `"업데이트 확인..."` + `"Ddg 종료"`(`WeakEntity<Desktop>`), `Desktop::route_key` 순수 추출 및 순서(`zoom_action`→`update_modal`→`machine_modal`→`rename`, `Enter` 고위험 실행 금지), 양방향 모달·`install_req_id`↔`remote_update` 상호배타, `Applying` 종료 보류 + `on_app_quit` best-effort 5s, 릴리스 노트 줄별 정제·결정적 `open_url`·GUI 5종 레닥션·단일 라인 컨테이너 복사 템플릿(Bidi/개행 제거).
  - 리뷰 반영: `A16–A18`, `F-01..F-14`, `SEC-D-04/06`, `SEC-E-03/04`.
- **테스트 (TDD)**: `UT-NATIVE-01`, `UT-NATIVE-02`. 명령: `GHOSTTY_VT_LIB_DIR=<path> cargo test -p ddg-agents-native`. 통합 게이트(`PG-4` 종료): `cargo test --workspace`. **비가역**: 없음.

## T5-A: Windows `x86_64` 전체 OS 포팅 (ConPTY · JobObject · Named Pipe DACL · `BCryptGenRandom` · `.old.<nonce>`)
- **유형 / 상태**: `구현` / `조사 필요` (**`T0-A` 완료 전 착수 금지**) | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: **`T0-A`**, `T3-A`, `T3-B` 완료 필수 · 직렬(17+2 파일 OS 경계 전수 수정).
- **구현 착수 근거**:
  - 변경 대상: `Cargo.toml`(`cfg(windows)` `windows-sys`), `crates/ddg-agents-proto/src/id.rs`(`BCryptGenRandom`), `crates/ddg-agents-os/**`(Named Pipe `first_pipe_instance(true)` 최초 바인드 한정 + `FILE_FLAG_OVERLAPPED`/Tokio async + 생성 시점 `SECURITY_ATTRIBUTES` SID DACL + `PIPE_REJECT_REMOTE_CLIENTS` + `GetNamedPipeClientProcessId`, `LockFileEx`, `MoveFileExW`+`.old.<nonce>`+startup GC, `DETACHED_PROCESS`), `crates/ddg-agents-vt/build.rs`(`ghostty-vt.lib`), `crates/ddg-terminal-server/**`(ConPTY+JobObject+`--bin-path`), `crates/ddg-agents-host/**`(Named Pipe+`folder_basename` `\`/`/`+`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`), `crates/ddg-agents-transport/**`(%APPDATA%+OpenSSH), `crates/ddg-agents-cli/src/attach.rs`(Console VT), `native/src/ui_scale.rs`.
  - 리뷰 반영: `DN-05/07/14/16`, `SEC-D-06`, `VAL-4.6 #3`, `test-reviewer F-02/03`.
- **테스트 (TDD)**: `UT-PROTO-02`(Windows `BCryptGenRandom` 분기), `UT-OS-01`, `UT-OS-02`, `UT-OS-03`(2회차 연속 `.old.<nonce1>`/`.old.<nonce2>` 공존 + GC 스킵), `UT-HOST-03`(ConPTY+JobObject). macOS에서는 POSIX 무회귀 + 순수 SDDL 테스트 실행(`#[cfg(windows)]` 런타임은 Windows 러너/실기 실행 전 `미검증` 보고). **비가역**: 없음.

## T5-B: 패키징 스크립트 · `manifest.json(.sig)` 도구 · `.github/workflows/{ci,release}.yml`
- **유형 / 상태**: `구현` / `조사 필요` (**`T0-A`, `T0-B` 완료 전 착수 금지**) | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: `T0-A`, `T0-B`, `T4-A`, `T4-B`, `T5-A` 완료 필수 · 직렬(`T6-A`와 병렬 가능).
- **구현 착수 근거**:
  - 변경 대상: `.github/workflows/{ci.yml,release.yml}`, `scripts/release/*`, `assets/macos/{Info.plist.template,entitlements.plist}`, `.gitignore`.
  - 계약: `macos-arm64`(inside-out 서명→`notarytool`→`stapler staple`/`validate`→`ditto -c -k --keepParent --norsrc`), `windows-x86_64`(`.zip` 4종), `linux-x86_64`/`aarch64`(Debian Bookworm `glibc ≤ 2.36` 심볼 검증 + 3종 `.tar.gz`), `manifest.json`+`SHA256SUMS`(+2차 `manifest.json.sig` 키 비공백 가드), 비밀값 하드코딩 금지.
  - 리뷰 반영: `DN-06/10/15`, `SEC-D-02/03`.
- **테스트**: 신규 UT 제외(파싱/서명은 `UT-UPD-01..08`에 완비). 검증: `cargo test --workspace` + 미서명 macOS dry-run 패키징 + 임시 Ed25519 키로 `manifest.json(.sig)` 생성·`ddg-agents-update` 검증 통과 + YAML 정적 검증. **비가역**: 없음(`git init`/푸시/시크릿은 `T7-A`).

## T6-A: 로컬·Docker·원격 SSH 통합 사용자 시나리오 검증 (`US-03`, `US-04`, `US-05`, `US-06`)
- **유형 / 상태**: `검증` / `착수 가능` (`T4-A`, `T4-B` 완료 시 — `T5-A`/`T7-A` 비의존!) | **역할**: 검증 담당(코드 수정 금지)
- **수행·통과 기준**: [SCENARIOS.md](SCENARIOS.md)의 `US-03`(사람 준비+에이전트 실행), `US-04`(Docker 차단+`machine install` 무회귀), `US-05`(실환경 보안 부정 경계), `US-06`(오프라인 CLI `--json`/`local` 고정) 전수 실행 및 증거 채증(`docs/plan/github-update-system/verification/T6-A-report.md`). 미확보 환경 항목은 `미검증` 보고. **비가역**: 없음.

## T7-A: 비가역 실행 및 외부 승인 게이트 (`GATE-01`–`GATE-05`)
- **유형 / 상태**: `비가역/외부 승인` / `차단(사용자 서면 승인 대기)` | **선행**: `T5-B`, `T6-A` 완료 + 항목별 사용자 명시 승인
- **단계별 승인 조건 / 준비 / 독립 작업**:
  - **대기 조건**: 아래 `GATE-01..05`는 계획 보고로 승인되지 않으며 항목별 명시 승인 전 실행 금지.
  - **허용 준비 작업**: `.gitignore` 민감파일 최종 점검, Draft 릴리스 초안 텍스트 준비, 필요한 GitHub Secrets 이름표 체크리스트 전달(값 미기재).
  - **대기 중 독립 작업**: `T0-A`–`T6-A` 전부 승인 대기 없이 완결 가능.
- **비가역 항목 상세**:
  - `GATE-01`: `git init` · 실제 `DEFAULT_UPDATE_REPO` 확정 · GitHub Public 원격 연결 · 초기 `git push` (공개 이력 영구 노출; 대안 사설 레포는 A1 제외; 복구: 삭제 가능하나 공개 캐시 회수 불가).
  - `GATE-02`: 2차 전환 시 Ed25519 운영 키쌍 생성·`const RELEASE_PUBKEYS` 커밋 (1차는 SHA-256 모드 A6; 운영 키 분실/유출 리스크).
  - `GATE-03`: GitHub Secrets 주입(Apple Developer ID `.p12`/Notary API Key/2차 Ed25519 개인키 — 값 절대 미기재).
  - `GATE-04`: 실제 Apple Developer ID 서명 및 `notarytool` 공증 제출(외부 Apple 기록; 재공증만 가능).
  - `GATE-05`: 태그(`vX.Y.Z`) 푸시 및 첫 Public GitHub Release 발행(Draft 선검증 권고; 이미 받은 클라이언트 캐시 회수 불가).

## T6-B: 실서명 macOS · 실기 Windows 사용자 시나리오 검증 (`US-01`, `US-02`)
- **유형 / 상태**: `검증` / `차단(T5-A/T5-B/T7-A 완료 대기)` | **역할**: 사람(실기·실서명) + CI 러너
- **수행·통과 기준**: [SCENARIOS.md](SCENARIOS.md)의 `US-01`(실서명 `.app` `codesign -R`/`spctl`/`AppTranslocation`/A17/A18/PTY 생존/롤백) 및 `US-02`(Windows 실기 ConPTY/DACL/2회차 `.old.<nonce>` 공존·GC) 실행 및 증거 채증(`docs/plan/github-update-system/verification/T6-B-report.md`). 미실행 항목은 **`미검증`** 명시. **비가역**: 없음.

## T8-A: 전체 작업 종료 · `FINAL.md` 보고 · 아카이브 이관 · `main` 머지 인계 (`GATE-06`)
- **유형 / 상태**: `검증/종료(비가역 머지 승인 포함)` / `차단(T6-A·T6-B·T7-A 완료 및 GATE-06 승인 대기)` | **역할**: `completion-reporter` (`/Users/dotdot/.agents/skills/ddg-planning/roles/completion-reporter.md`) + Lead
- **수행 내용**:
  1. `docs/archive/report/YYYY-MM-DD/github-update-system/FINAL.md` 작성 및 같은 디렉터리 `README.md` 갱신.
  2. 구현·검증 범위 완료 확인 후 `docs/plan/github-update-system/` → `docs/archive/plan/github-update-system/` 이동, 상대 링크 및 `docs/plan/README.md`·`docs/archive/README.md` 갱신.
  3. **`GATE-06` (`main` 머지/푸시 — 사용자 명시 승인 필수)**: 승인 시 머지/squash 커밋 본문에 보고서·계획 경로, 선택 근거·대안(A1–A18, A14→A16 변경 포함), 승인·검증 결과 및 `미검증` 위험을 자족적으로 기록(archive가 Git 무시 대상이면 강제 `git add -f` 금지).
