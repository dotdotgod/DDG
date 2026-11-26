# 작업 계획: GitHub Releases 기반 크로스플랫폼 업데이트 시스템

[UX·UI 설계(ASCII 아트)](DESIGN_UI.md) · [데이터·네트워크·보안 설계](DESIGN_DATA.md) · [유닛 테스트(22 UT)](TESTS.md) · [사용자 시나리오(6 US)](SCENARIOS.md) · [태스크(Phase 0–3)](TASKS_CORE.md) · [태스크(Phase 4–8)](TASKS_UI_RELEASE.md) · [플래닝 완료 보고](../../archive/report/2026-11-26/github-update-system/PLANNING.md)

## 상태와 확정 기록
- **상태**: **`T0-A`–`T6-A` + `GATE-01`(`dotdotgod/DDG`) + 3플랫폼 실 CI 전체 통과 완료 (`2026-11-26`)** — Apple 자격증명 준비 후 `GATE-02`–`GATE-05`·`US-01..03` 후속 실행 대기 ([최종 보고](../../archive/report/2026-11-26/github-update-system/FINAL.md) · [배치 보고](../../archive/report/2026-11-26/github-update-system/BATCH_T0_T6A.md) · [T6-A 증거](verification/T6-A-report.md))
- **확정 조합 (A1–A18)**: `A1` Public GitHub Releases · `A2` 로컬 4종 + 원격 `Ssh` 푸시 · `A3+A7` `macOS arm64` + `Linux x86_64/aarch64` + Windows `x86_64` 전체(ConPTY 포함) · `A4` 자동 확인(opt-out 0-네트워크) + 승인 적용 · `A5` Core Detach 무중단 교체 + N-1 자동 롤백 · `A6` HTTPS+SHA-256 우선(`manifest.json.sig` 분리 Ed25519 슬롯 선반영) · `A8` macOS `.app` + Apple Developer ID 서명·공증 · `A9` 동일 프로토콜 경고 / 비호환 차단 · `A10` `stable`+`prerelease`(opt-in) + N-1 보존 + 캐시 최근 2개 · `A11` 레포 플레이스홀더+오버라이드 · `A12` 커스텀 레포도 공식 DDG Team ID 고정 · `A13` 커스텀 레포 추가 강제 플래그 없음(오픈소스) · `A15` 원격 수동 롤백 포함 · `A16`(A14 대체) `ContainerSsh`·`DockerExec` 인플레이스 차단+이미지 안내 · `A17` 즉시 재시작(`PendingRestart` 삭제) · `A18` 배너 세션 한정 닫기(`skipped_version` 삭제).
- **역할·모델 분리**: 계획·설계·리뷰·최종 검수 = **Antigravity (Gemini 4 Neon)** (독립 서브에이전트 15개 역할 병렬/순차 수행); 구현 = 실행 세션에서 로컬 `pi --provider openai --model gpt-6.2-sol --thinking medium` + `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`.

## 배경과 목적
DDG Agents(`Cargo.toml` v0.1.0, `.git` 미초기화)는 현재 로컬·원격 바이너리 버전 조회와 공식 릴리스 업데이트 수단이 없고, 전 크레이트가 Unix 전용 API에 묶여 있다. Public GitHub Releases를 단일 배포 원천으로 삼아 macOS/Linux/Windows 전 플랫폼의 안전한 업데이트·무중단 PTY 유지 교체·N-1 자동/수동 롤백 체계를 설계한다.

## 6하원칙으로 정리한 요구사항
- **Why**: 활성 터미널(PTY) 손실 없이 로컬 앱·CLI·로컬/원격 호스트를 안전하게 최신화·롤백하고, Windows 전체 스택까지 일관된 배포 경험을 제공.
- **Who**: macOS(`arm64`)/Windows(`x86_64`) 데스크톱 사용자, Linux(`x86_64`/`aarch64`)/macOS 원격 SSH 운영자, `ddg --json` AI 에이전트, 릴리스 발행자.
- **What**: 로컬 4종(`ddg-agents-native`, `ddg`, `ddg-agents-host`, `ddg-terminal-server`) + 원격 `HostTransport::Ssh` 업데이트/자동·수동 롤백, `ContainerSsh`/`DockerExec` 버전 감지·이미지 갱신 안내, Windows 전체 ConPTY 포팅, macOS `.app` 서명·공증, CI 릴리스 워크플로우, `docs/spec/requirements.md` CLI 정합화.
- **When**: 앱 기동/주기 자동 확인(opt-out 0-네트워크) + 수동 확인; 적용은 사용자 명시 승인 시에만; 외부 Git/시크릿/공증/릴리스는 승인 게이트 후 실행.
- **Where**: GPUI 단일 `UpdateModal`·사이드바 버전 행·상단 1슬롯 배너·macOS 메뉴, `ddg update` 5서브커맨드, Unix(`~/.config`/`~/.local`) & Windows(`%APPDATA%`/`%LOCALAPPDATA%`).
- **How**: `ddg-agents-os` + `ddg-agents-update`(`default-features=false` 코어 공유), HTTPS:443 5호스트·SHA-256·`manifest.json.sig` 슬롯·Rust `zip` 단일 파서, `DN-02` Core Detach + `SEC-D-01` 2단계 익명 파이프 직접 인계.

## 우선순위와 포함·제외 범위
- **필수 (1차 MVP)**: Phase 0 조사(`T0-A/B`) → Phase 1 Proto/OS(`T1-A`) → Phase 2 Update 엔진(`T2-A/B`) → Phase 3 데몬 무중단 인계·원격 SSH(`T3-A/B`) → Phase 4 CLI·GPUI(`T4-A/B`) → Phase 5 Windows 포팅·CI/패키징(`T5-A/B`) → Phase 6 시나리오 검증(`T6-A/B`) → Phase 7–8 외부 승인·종료(`T7-A/T8-A`).
- **제외 범위**: 무인 자동 강제 업데이트, Docker 컨테이너 자동 재생성 및 GHCR 자동 발행, Windows 원격 SSH 푸시 수신 대상(PowerShell), Windows Authenticode 서명/MSI·MSIX, macOS Homebrew Cask, 델타 패치, Linux GUI 데스크톱 패키징, 신규 자동화 E2E 테스트 스위트.

## 요구사항 충돌과 결정 기록
| 항목 | 확정 결정 | 제외 대안 및 근거 |
|---|---|---|
| `ContainerSsh` (`A14→A16`) | `DockerExec`과 동일하게 인플레이스 차단(`container_update_unsupported`) + 이미지 안내 | `docker/entrypoint.sh:51`의 `exec ddg-agents-host serve`가 PID 1 `tini` 직속이라 종료 시 컨테이너 전체가 죽음(`DN-01`); 기존 `machine install`은 무회귀 유지 |
| GUI 재시작 수명 (`A17`) | 즉시 재시작(`HandedOffToHelper` → `cx.quit()`); `PendingRestart` 삭제 | 2단계 대기는 `.app` 4종 동봉 구조와 상태 중복·데드 UI 충돌(`F-01`, `DN-04`) |
| 배너 닫기 (`A18`) | 인메모리 세션 한정(`dismissed_available_version`); `skipped_version` 삭제 | 영속 설정과 인메모리 이중 소유권·GUI↔CLI 불일치 제거(`F-05`) |
| `TokenStore` 인계 (`SEC-D-01`) | `host-admin` 토큰 미반환 + 구→신 Host 직접 익명 파이프 2단계(`tokens_loaded`→`bind_now`→`ready`) | `host-admin`이 토큰 반환 시 동일 UID PTY/플러그인의 일괄 토큰 탈취 가능(`SEC-D-01`) |
| 커스텀 레포 (`A12–A13`) | 추가 강제 플래그 없음(오픈소스) + 공식 DDG Team ID 고정 유지 | 강제 플래그는 오픈소스 사용성 저해, Team ID 완화는 타인 서명 앱 설치 통로 개방 |
| 아카이브/공증 (`DN-06`, `SEC-D-02`) | Rust `zip` 단일 파서(`ditto` 제거), `stapler validate`는 CI 전용(클라이언트 `codesign -R`+`spctl`) | `zip`+`ditto` 이중 파서 불일치 및 비개발자 Mac CLT 스텁 오류 방지 |

## 역할·책임·작업 영역
- **계획·설계·리뷰 (15개 원보고 전량 수집 완료)**: `user-requirements`, `data-requirements`, `validation-requirements`, `ux-designer`, `ux-reviewer`, `security-reviewer`(×3), `presentation-layer-designer/reviewer`, `data-network-layer-designer/reviewer`, `test-designer/reviewer`, `review-conductor`, `task-planner`, `completion-reporter`.
- **구현 인계**: 새 실행 세션이 각 태스크 명세와 `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`를 적용해 수행.

## 필수 Validation과 저장·조회·상태 정책
- **경계별 출처·검증 책임·보장·실패**: [DESIGN_DATA.md](DESIGN_DATA.md#2-저장--스키마--프로토콜--ssot-계약) 및 [DESIGN_UI.md](DESIGN_UI.md#3-상태-소유권--키보드--종료-보호--오류-카드-매핑) 참조 — (1) 외부 GitHub HTTPS:443·5호스트·수동 리다이렉트≤5·크로스호스트 `Authorization` 제거·2MiB/256MiB·SHA-256·`manifest.json.sig`·Rust `zip`/`tar` 512MiB/2,048 엔트리, (2) macOS 5요소 `codesign -R`(leaf OID `1.2.840.113635.100.6.1.13`, `subject.OU`, `identifier`)+`spctl`+`AppTranslocation`/쓰기 권한+헬퍼 `lstat`/재검증, (3) 로컬 `update.lock`(`flock`/`LockFileEx`) RMW·Dist Cache 최근 2개+핀 보호·롤백 전 N-1 SHA-256/`codesign -R` 재검증, (4) `host-admin`/`terminal-admin` 피어 PID 차단+토큰 미반환, (5) CLI `-H` `local` 고정·`-H all`/`config -H`/`--token` 차단·예외 없는 5종 레닥션.
- **내부 코드 신뢰**: 경계를 통과해 검증된 도메인 타입(`RepoSlug`, `VerifiedManifest`, `StagedPackage`, `VerifiedRollbackSet`, `RestoredTokenStore`)은 내부 코디네이터/UI에서 중복 재검증이나 추측성 폴백 금지(단, 시간차·프로세스 분리 경계인 `__update-apply-helper`와 `rollback`은 스왑 직전 재검증 유지).

## 설계 · 테스트 · 리뷰 상세 링크
- **UX · Presentation Layer (ASCII 아트 포함)**: [DESIGN_UI.md](DESIGN_UI.md)
- **Data · Network · OS · 보안 · 한계 및 재검토 조건**: [DESIGN_DATA.md](DESIGN_DATA.md)
- **TDD 유닛 테스트 (22 UT) & 제외 근거**: [TESTS.md](TESTS.md)
- **실환경 사용자 시나리오 (6 US)**: [SCENARIOS.md](SCENARIOS.md)

## 영역별·보안 리뷰와 전체 리뷰 결과
- **Phase C (UX/보안)**: `UX-01..11`, `SEC-UX-01..06` → `A12–A15` 확정 및 수정 반영(`-H` `local` 고정, `auto_check=off` 0-네트워크, 모달 상호배타·키 트랩, 결정적 URL, `copy_hint` 안전성, `RequirePinnedHostId` 사전 핸드셰이크).
- **Phase D (Presentation/Data/보안/검증)**: `F-01..F-14`, `DN-01..DN-16`, `SEC-D-01..06`, `VAL-4.6` → `A16–A18` 확정 및 Rev.2 통합(`SEC-D-01` 직접 파이프, `DN-02` 동기 `BYE`, `DN-05` `.old.<nonce>`, `DN-06` `renamex_np`, `DN-10` AI slop 삭제).
- **Phase E (테스트/보안/전체 리뷰)**: `test-reviewer F-01..F-10`, `SEC-E-01..05`(실기 `codesign -d -r -`로 leaf OID `1.2.840.113635.100.6.1.13` 직접 확인), `review-conductor` 최종 감사 통과 및 9건 동기화 완료(`ApplyOutcome` 4배리언트, 5요소 `codesign -R`, `EXDEV` 복사 폴백, `failed_versions` 수동 재시도, `install_remote_host` `ContainerSsh` 무회귀).

## 페이즈별 실행 계획
1. **Phase 0 (`PG-0`, 조사)**: `T0-A`(Windows MSVC 스파이크) ∥ `T0-B`(패키징·Bookworm·서명 조사) → [TASKS_CORE.md](TASKS_CORE.md#t0-a-windows-x86_64-windows-msvc-ghostty-vtlib--gpui-022-링크-스파이크) *(POSIX 구현 `T1-A`와 병렬 가능)*
2. **Phase 1–2 (직렬 구현)**: `T1-A`(`ddg-agents-proto`+`ddg-agents-os`) → `T2-A`→`T2-B`(`ddg-agents-update`) → [TASKS_CORE.md](TASKS_CORE.md#t1-a-ddg-agents-proto-프로토콜-확장--openrpc-동기화--ddg-agents-os-posix-기반-신설)
3. **Phase 3 (`PG-3`, 병렬 구현)**: `T3-A`(`ddg-terminal-server`+`ddg-agents-host`) ∥ `T3-B`(`ddg-agents-transport`) → [TASKS_CORE.md](TASKS_CORE.md#t3-a-ddg-terminal-server--ddg-agents-host--dn-02-core-detach--sec-d-01-2단계-직접-인계--레거시-v010)
4. **Phase 4 (`PG-4`, 병렬 구현)**: `T4-A`(`ddg-agents-cli`+`requirements.md`) ∥ `T4-B`(`native/` GPUI) → [TASKS_UI_RELEASE.md](TASKS_UI_RELEASE.md#t4-a-ddg-agents-cli-ddg-update-5서브커맨드----json-단일-객체--docsspecrequirementsmd-정합화)
5. **Phase 5 (직렬 구현, `T0-A/B` 선행)**: `T5-A`(Windows 전체 OS 포팅) → `T5-B`(패키징·CI/Release YAML) → [TASKS_UI_RELEASE.md](TASKS_UI_RELEASE.md#t5-a-windows-x86_64-전체-os-포팅-conpty--jobobject--named-pipe-dacl--bcryptgenrandom--oldnonce)
6. **Phase 6–8 (검증 · 비가역 승인 · 종료)**: `T6-A`(`US-03..06`, `T4` 직후 즉시 가능) · `T7-A`(외부 승인 `GATE-01..05`) → `T6-B`(`US-01..02`) → `T8-A`(`FINAL.md`·아카이브 이동·`GATE-06` `main` 머지 인계) → [TASKS_UI_RELEASE.md](TASKS_UI_RELEASE.md#t6-a-로컬docker원격-ssh-통합-사용자-시나리오-검증-us-03-us-04-us-05-us-06)

## 일정과 수행자 · 위험 및 미결정 사항
- **일정/수행자**: 착수 시점 미정(새 실행 세션 착수 시 진행). 구현 = 로컬 `pi (openai/gpt-6.2-sol, --thinking medium)`, 검수 = `Antigravity (Gemini 4 Neon)`.
- **남은 위험·외부 선행 조건**: ① `R-01` Windows `ghostty-vt.lib`+GPUI 링크(`T0-A` 선행 게이트), ② `R-02` 공식 `owner/repo`·Apple Developer ID/Notary 시크릿·2차 Ed25519 키(`T7-A` `GATE-01..05`), ③ `R-03` Windows 실기(`US-02`·`#[cfg(windows)]` UT는 실행 전 `미검증`), ④ `R-04` 원격 Linux `glibc ≤ 2.36`·`v0.1.0` 실바이너리 업그레이드(`US-03`), ⑤ `R-05` 계획 단계 미실행.
- **최종 완료 보고서 경로**: [FINAL.md](../../archive/report/2026-11-26/github-update-system/FINAL.md)
