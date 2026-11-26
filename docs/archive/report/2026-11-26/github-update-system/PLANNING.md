# 작업 결과 보고서: GitHub Releases 기반 크로스플랫폼 업데이트 시스템

## 보고 대상: 계획 작성
## 대상 태스크·배치 ID와 포함 범위
- **대상**: `/ddg-planning` Phase A–G 전체 (요구사항·UX/UI·Data/Network/OS/보안·22 UT·6 US·태스크 `T0-A`–`T8-A` 확정).
- **포함 범위**: 계획·설계·리뷰·태스크 명세 작성 완료. **구현·빌드·테스트·시나리오·외부 승인(`GATE-01`–`GATE-06`)·커밋·`main` 머지 전량 미수행**(문서 검토를 구현·테스트 통과로 표현 금지).

## 해당 범위 상태: 완료 (계획 작성 완료 · 조사 및 POSIX 구현 착수 가능)
## 전체 계획 상태와 남은 범위
- **전체 상태**: 플래닝 완료 (`2026-11-26`) — 구현·검증·외부 승인 미착수.
- **남은 범위**: `PG-0`(`T0-A`∥`T0-B`) 및 `T1-A`→`T2-A`→`T2-B`→`PG-3`(`T3-A`∥`T3-B`)→`PG-4`(`T4-A`∥`T4-B`) → `T5-A`→`T5-B`(`T0-A/B` 선행) → `T6-A`(`US-03..06`)·`T7-A`(`GATE-01..05`) → `T6-B`(`US-01..02`) → `T8-A`(`FINAL.md`·아카이브 이관·`GATE-06`).

## 산출물: 계획·설계·태스크 경로
- 활성 계획서: [README.md](../../../../plan/github-update-system/README.md) (`docs/plan/github-update-system/`)
- 설계: [DESIGN_UI.md](../../../../plan/github-update-system/DESIGN_UI.md)(UX·UI·ASCII 아트) · [DESIGN_DATA.md](../../../../plan/github-update-system/DESIGN_DATA.md)(Data·Network·OS·보안·CI)
- 검증: [TESTS.md](../../../../plan/github-update-system/TESTS.md)(22 UT) · [SCENARIOS.md](../../../../plan/github-update-system/SCENARIOS.md)(6 US)
- 태스크: [TASKS_CORE.md](../../../../plan/github-update-system/TASKS_CORE.md)(`T0-A`–`T3-B`) · [TASKS_UI_RELEASE.md](../../../../plan/github-update-system/TASKS_UI_RELEASE.md)(`T4-A`–`T8-A`)

## 확정 요구사항·설계와 결정 출처
- **Phase B (`A1`–`A11`, 사용자 확정)**: `A1` Public GitHub Releases 단일 원천 · `A2` 로컬 4종(`native`/`ddg`/`host`/`terminal-server`)+원격 `Ssh` 푸시 · `A3+A7` `macOS arm64`+`Linux x86_64/aarch64`+`Windows x86_64` 전체(ConPTY 포함) · `A4` 자동 확인(opt-out 0-네트워크)+승인 적용 · `A5` Core Detach 무중단 교체+N-1 자동 롤백 · `A6` 1차 HTTPS+SHA-256(`manifest.json.sig` Ed25519 슬롯 선반영) · `A8` macOS `.app` Developer ID 서명·공증 · `A9` 동일 프로토콜 경고/비호환 차단 · `A10` `stable`+`prerelease`(opt-in)+N-1 보존+캐시 최근 2개 · `A11` `DEFAULT_UPDATE_REPO` 플레이스홀더+오버라이드.
- **Phase C (`A12`–`A15`, UX/보안 리뷰 확정)**: `A12` 커스텀 레포도 공식 DDG Team ID 고정 · `A13` 커스텀 레포 추가 강제 플래그 없음(오픈소스, 경고 배너) · `A14`(→`A16` 대체) 초기 `ContainerSsh` 인플레이스 검토 · `A15` 원격 수동 롤백 1차 포함.
- **Phase D (`A16`–`A18`, Rev.2 확정)**: `A16`(`A14` 대체) `ContainerSsh`·`DockerExec` 인플레이스 차단(`container_update_unsupported`)+이미지 안내(기존 `machine install` `ContainerSsh` 무회귀) · `A17` GUI 즉시 재시작(`HandedOffToHelper`→`cx.quit()`, `PendingRestart` 삭제) · `A18` 배너 세션 한정 닫기(`dismissed_available_version`, `skipped_version` 삭제).

## 요구사항·가정 옵션과 설계 옵션
- **선택한 옵션·이유·근거·영향**:
  - `ddg-agents-os`+`ddg-agents-update`(`default-features=false` 코어/`network` 분리): OS IPC·잠금·DACL 단일화, 데몬·Docker HTTP/TLS/zip/tar 의존성 0개.
  - `DN-02` Core Detach(동기 `BYE`)+`SEC-D-01` 2단계 익명 파이프 직접 인계(`tokens_loaded`→`bind_now`→`ready`): `host-admin` 토큰 미반환, 종료 탭 `revoke`, 실패 시 구 Host 무손실 재개.
  - 단일 Rust `zip`(`deflate-flate2`)/`tar.gz`(`SEC-D-02`)+5요소 `codesign -R`(leaf OID `1.2.840.113635.100.6.1.13`, `SEC-E-01`)+`spctl`(클라이언트 `stapler` 제거·CI 전용)+`renamex_np`(`DN-06`)+Windows `.old.<nonce>`/GC(`DN-05`).
  - CLI `-H` `local` 고정+예외 없는 5종 레닥션+31종 `UpdateErrorCode`/4배리언트 `ApplyOutcome` SSOT.
- **다른 방법과 제외 이유**: `ContainerSsh` 인플레이스(`A14`)→`docker/entrypoint.sh:51` PID 1 `tini` 직속으로 컨테이너 종료(`DN-01`) · `host-admin` 토큰 반환→동일 UID PTY/플러그인 탈취(`SEC-D-01`) · `PendingRestart`/`skipped_version`→상태 중복·데드 UI·GUI↔CLI 불일치(`F-01`/`F-05`) · `zip`+`ditto`·`verified.json` 마커·DI Mock Trait→이중 파서·TOCTOU·AI slop(`DN-10`) · 무인 강제 업데이트/Docker 재생성·GHCR/Windows SSH 수신·Authenticode·MSI/Homebrew/델타/Linux GUI/신규 E2E→1차 제외.
- **필수 범위 밖 작업의 필요성과 범위 내 대안**: `docs/spec/requirements.md` CLI 정합화(`T4-A`)와 기존 `install.rs` `auto_check` 분리·`dist/` `0700`·SHA-256 보강(`T3-B`)은 계약 충돌·미검증 경로 방지를 위해 필수 포함(방치 시 스펙 모순·보안 공백). 2차 Ed25519 키·Windows Authenticode·Linux GUI는 재검토 조건으로 보류.

## 태스크 순서·의존관계·병렬 그룹
- `PG-0` 조사(`T0-A`∥`T0-B`) *(POSIX `T1-A`와 병렬)* → `T1-A`(`proto`+`os`) → `T2-A`→`T2-B`(`update`) → `PG-3`(`T3-A` `terminal-server`+`host` ∥ `T3-B` `transport`) → `PG-4`(`T4-A` `cli`+`requirements.md` ∥ `T4-B` `native`) → `T5-A`(Windows OS, `T0-A` 선행)→`T5-B`(CI/패키징, `T0-A/B` 선행) → `T6-A`(`US-03..06`, `T4` 직후 가능)·`T7-A`(`GATE-01..05`) → `T6-B`(`US-01..02`) → `T8-A`(`FINAL.md`·아카이브·`GATE-06`).

## 비가역 실행의 결정 기록
> 계획 보고는 승인이 아니며 **6건 모두 실제 실행 여부 = `미수행`**.

| 게이트(태스크) | 대상·영향·비가역 이유 | 선택 근거·출처 | 제외 대안 | 복구 가능 범위 | 승인 조건 / 실행 여부 |
|---|---|---|---|---|---|
| `GATE-01`(`T7-A`) | `git init`·`DEFAULT_UPDATE_REPO` 확정·Public 원격·초기 `push`(이력 영구 공개) | `A1`/`A11` Public 원천 | Private(비인증 불가) | 삭제 가능, 외부 캐시 회수 불가 | 사용자 서면 승인+GitHub 권한 / **미수행** |
| `GATE-02`(`T7-A`) | 2차 Ed25519 키쌍 생성·`const RELEASE_PUBKEYS` 커밋(유출/분실 리스크) | `A6` 2차 서명 전환 | 런타임 env 키(덮어쓰기) | 신규 키 교체(구 클라이언트 회수 불가) | 2차 별도 승인+오프라인 키 보관 / **미수행** |
| `GATE-03`(`T7-A`) | GitHub Secrets 주입(Developer ID `.p12`/Notary Key/2차 키; 값 미기재) | `A8` CI 자동 서명·공증 | 로컬 수동 서명(재현 저하) | 시크릿 회전·폐기(기서명물 회수 불가) | 사용자 승인+Repo Admin 직접 주입 / **미수행** |
| `GATE-04`(`T7-A`) | 실제 Apple Developer ID 서명·`notarytool` 공증(Apple 외부 기록 영구 등록) | `A8` Gatekeeper/`spctl` | 미서명/ad-hoc(차단) | 재서명·재공증만 가능(이력 삭제 불가) | 사용자 승인+Apple Developer 자격 / **미수행** |
| `GATE-05`(`T7-A`) | 태그(`vX.Y.Z`) 푸시·첫 Public Release 발행(전 세계 즉시 수신) | `A1` 공식 배포 | Draft 유지(접근 불가) | 철회·패치 가능(기수신 캐시 회수 불가; Draft 선검증) | `T5-B`·`T6-A` 완료+사용자 승인 / **미수행** |
| `GATE-06`(`T8-A`) | 완료 후 `main` 머지·원격 `push`(기준 브랜치 이력 변경·공개) | 글로벌 정책 §2-6·`T8-A` | 장기 방치(괴리) | `git revert` 가능(푸시 후 강제 재작성 불가) | `T6-A`·`T6-B`·`T7-A` 완료+사용자 승인 / **미수행** |

## 역할별 결과·리뷰와 검증 증거
| 페이즈 | 역할 (15개 역할 원보고 전량 수집 완료) | 상태 | 핵심 산출물·리뷰 증거 |
|---|---|---|---|
| **B** | `user-requirements` ∥ `data-requirements` ∥ `validation-requirements` | 검토 완료 | 6하원칙·데이터 인벤토리·경계 Validation 표; `A1`–`A11` 확정 |
| **C** | `ux-designer` → `ux-reviewer` ∥ `security-reviewer`(1차) | 검토 완료 | 3진입점·`UpdateModal`·CLI; `UX-01..11`·`SEC-UX-01..06` 반영, `A12`–`A15` 확정 |
| **D** | `presentation-layer-designer` ∥ `data-network-layer-designer` → `presentation-layer-reviewer` ∥ `data-network-layer-reviewer` ∥ `security-reviewer`(2차) ∥ `validation-requirements`(2차) | 검토 완료 | ASCII·31종 매핑·OS/프로토콜/CI; `F-01..14`·`DN-01..16`·`SEC-D-01..06`·`VAL-4.6` 반영, `A16`–`A18` Rev.2 통합 |
| **E** | `test-designer` → `test-reviewer` ∥ `security-reviewer`(3차) ∥ `review-conductor` | 검토 완료 | 22 UT+6 US; `test-reviewer F-01..10`, `SEC-E-01..05`(실기 `codesign -dr -` leaf OID 확인), `review-conductor` 감사 통과·9건 동기화 |
| **F–G** | `task-planner` · `completion-reporter` | 검토 완료 | `docs/plan/github-update-system/` 7문서(`T0-A`–`T8-A`) 교차 검증 및 `PLANNING.md`·목차 작성(구현·테스트 미실행) |

- **설계 확정 전/중 변경 및 리뷰 갱신**: Phase D 리뷰(`DN-01`/`F-01`/`F-05`/`SEC-D-01`)를 `A14→A16`·`A17`·`A18`·2단계 익명 파이프 인계·`DN-02` 동기 `BYE`·`DN-05` `.old.<nonce>`·`DN-06` `renamex_np`·`SEC-D-02` 단일 Rust `zip`으로 개정 후 Phase E 3자 리뷰에서 최종본을 재검증해 9건(`ApplyOutcome` 4배리언트, 5요소 `codesign -R`, `EXDEV` 복사 폴백, `failed_versions` 수동 재시도, `install_remote_host` `ContainerSsh` 무회귀 등)을 전 문서에 동기화했다.

## 차단·미결정·미검증 사항과 남은 위험
- **차단**: ① `T0-A/B` 미조사로 `T5-A`·`T5-B` 차단, ② `T7-A`(`GATE-01..05`)·`T8-A`(`GATE-06`) 외부 승인 대기 *(POSIX `T1-A`–`T4-B`·`T6-A`는 즉시 진행 가능)*.
- **미검증·남은 위험 (`R-01`–`R-05`)**: `R-01` `windows-latest` `ghostty-vt.lib`+`gpui = "=0.2.2"` 링크(`T0-A`) · `R-02` 공식 `owner/repo`/Apple Developer ID·Notary 시크릿 대기(`US-01` 실서명 `미검증`) 및 1차 SHA-256 모드 · `R-03` `US-02`·`#[cfg(windows)]` UT(`UT-PROTO-02` Windows 분기·`UT-OS-01..03`·`UT-HOST-03`) Windows 미실행 · `R-04` 원격 Linux `glibc ≤ 2.36`·`v0.1.0` 실바이너리(`US-03`) · `R-05` 계획 단계 코드·테스트·시나리오 미실행.

## 다음 실행 세션의 착수 조건
1. **즉시 착수**: `T0-A` ∥ `T0-B`(조사) ∥ `T1-A`(POSIX 구현) — `T0-A/B`·`T7-A` 대기 없이 POSIX 경로(`T1-A`–`T4-B`→`T6-A`) 병행 가능.
2. **구현·검수 규율**: 구현 태스크는 `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md` 필수 준수. 로컬 `pi auth check --model openai/gpt-6.2-sol` `ready` 선확인 후 `hc run --bg --name <slug> --cwd /Users/dotdot/Workspace/dotdotgod-agents -- pi --provider openai --model gpt-6.2-sol --thinking medium`(서브에이전트 금지) 실행, 완료마다 Antigravity Lead(`Gemini 4 Neon`)가 `git status`/`git diff`/실제 테스트 출력을 독립 검수.

## 아카이브·main 머지 기록
- **실제 보고서**: [PLANNING.md](PLANNING.md) (`docs/archive/report/2026-11-26/github-update-system/PLANNING.md`)
- **보고 목차**: [README.md](README.md) (`docs/archive/report/2026-11-26/github-update-system/README.md`)
- **완료 계획**: 현재 활성 경로 유지 — [docs/plan/github-update-system/README.md](../../../../plan/github-update-system/README.md) (Phase G에서는 archive로 미이동)
- **상호 링크·인덱스 갱신**: `docs/plan/README.md`, `docs/archive/README.md`, 본 보고 목차 모두 갱신 완료
- **main 머지 상태·커밋**: **미수행** (계획 작성 단계, `.git` 미초기화; `GATE-01`·`GATE-06` 승인 대기)
- **머지/squash 커밋 본문 템플릿** (`T8-A` 인계; archive가 `.gitignore` 대상이면 강제 추가 금지):
  - 경로: `docs/archive/plan/github-update-system/README.md`, `docs/archive/report/YYYY-MM-DD/github-update-system/FINAL.md`
  - 결정·대안·범위: `A1`–`A18`(`A14→A16`·`A17`·`A18`), `SEC-D-01` 익명 파이프, `DN-02` Core Detach, `SEC-D-02` 단일 `zip`, `SEC-E-01` 5요소 `codesign -R`, `requirements.md` 정합화 / 제외: `ContainerSsh` 인플레이스·`host-admin` 토큰 반환·`PendingRestart`/`skipped_version`·`ditto`
  - 승인·검증·위험: `GATE-01`–`GATE-06` 승인 기록, `cargo test --workspace`(22 UT)·`US-01`–`US-06` 실제 결과(`미검증` 구분), 잔여 위험 `R-01`–`R-05`

## 수행·미수행 작업과 다음 착수점
- **수행 완료**: Phase A–G 플래닝, 15개 역할 설계·보안·테스트·통합 리뷰, 계획/설계/테스트/시나리오/태스크(`T0-A`–`T8-A`) 및 `PLANNING.md` 작성.
- **미수행**: 조사(`T0-A/B`), 코드·UT 구현(`T1-A`–`T5-B`), 실환경 검증(`T6-A/B`), 외부 승인(`T7-A`), 종료·이관·머지(`T8-A`).
- **다음 착수점**: 새 실행 세션에서 로컬 `pi` pre-flight 확인 후 `T0-A` ∥ `T0-B` ∥ `T1-A` 즉시 착수.
