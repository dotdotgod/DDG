# 완료 보고: GitHub Releases 기반 크로스플랫폼 업데이트 시스템

- **작성 일시**: `2026-11-26` | **공식 레포**: [`dotdotgod/DDG`](https://github.com/dotdotgod/DDG) (`main`, 커밋 `846c450`)
- **활성 계획**: [docs/plan/github-update-system/README.md](../../../../plan/github-update-system/README.md) · **배치 보고**: [BATCH_T0_T6A.md](BATCH_T0_T6A.md)
- **최종 상태**: **`Phase 0–6A` (`T0-A`–`T6-A`) + `GATE-01` + 3플랫폼 실 CI(`darwin-arm64` · `windows-x86_64` · `linux-x86_64`) 전체 통과 완료** — Apple Developer ID 자격증명 준비 전까지 `GATE-02`–`GATE-05`(Ed25519·서명·공증·첫 태그 릴리스) 및 실기 `US-01`–`US-03`은 명시적 보류·`미검증` 상태로 인계.

## 1. 구현·검증 결과 요약

### A. 완료 범위 (`T0-A`–`T6-A`, `GATE-01`, 3플랫폼 실 CI)
- **공식 배포 소스 확정 (`GATE-01`)**: `DEFAULT_UPDATE_REPO = "dotdotgod/DDG"`, 1차 무결성 모드 = HTTPS + SHA-256 (`RELEASE_PUBKEYS = &[]`, `manifest.json.sig` Ed25519 슬롯 선반영), `git init -b main` → Public GitHub 원격 연결 및 푸시 완료.
- **전 크레이트 구현 (`T0-A`–`T5-B`)**:
  - `ddg-agents-proto` / `ddg-agents-os`: 프로토콜 확장(`HelloResult`/`HostHelloResult`, `PROTOCOL_INCOMPATIBLE`, OpenRPC 동기화) 및 POSIX + Windows OS 추상화(`BCryptGenRandom`, Named Pipe SID DACL/`GetNamedPipeClientProcessId`/`READ_CONTROL`+`SE_KERNEL_OBJECT`, `LockFileEx`, `.old.<nonce>` 2회차 교체·GC).
  - `ddg-agents-update`: 31종 `UpdateErrorCode`, 4배리언트 `ApplyOutcome`, HTTPS:443 5호스트·SHA-256·`manifest.json.sig`·Rust `zip`/`tar`, `update.lock` RMW·Dist GC·macOS 5요소 `codesign -R`·`renamex_np`·N-1 롤백.
  - `ddg-terminal-server` / `ddg-agents-host` / `ddg-agents-transport`: `DN-02` Core Detach, `SEC-D-01` 2단계 익명 파이프 직접 인계(`--restore-tokens-pipe`), Windows ConPTY(`STARTF_USESTDHANDLES`) + JobObject, `HostTransport::Ssh` 원격 업데이트·수동 롤백, 컨테이너 인플레이스 차단(`A16`) 및 `ContainerSsh` 부트스트랩 무회귀.
  - `ddg-agents-cli` / `native/` / CI·패키징: `ddg update` 5서브커맨드(`-H local` 고정, 5종 레닥션), GPUI 단일 `UpdateModal`·A17 즉시 재시작·A18 세션 닫기, `.github/workflows/{ci,release}.yml`, `scripts/release/*`, `assets/macos/*`, `docker/Dockerfile`(`release-binaries`).
- **실환경 시나리오 검증 (`T6-A`, [T6-A-report.md](../../../../plan/github-update-system/verification/T6-A-report.md))**: `US-04`(실 Docker `ddg-agents-host:test` 차단·무회귀) **PASS**, `US-05`(실 `codesign -R` ad-hoc 거부, PTY `host-admin.sock` `permission_denied`, 토큰 0건 유출, symlink TOCTOU 차단) **PASS**, `US-06`(`-H` 미지정 로컬 고정·0 TCP·단일 JSON·`--skill`) **PASS**.

### B. 3플랫폼 실 GitHub Actions CI 통과 증거 (Run [`45793163847`](https://github.com/dotdotgod/DDG/actions/runs/45793163847), 커밋 `846c450`)

| 플랫폼 (Runner) | Job ID | 소요 | 테스트 결과 (워크스페이스 + OpenRPC) | Clippy (`-D warnings`) | 바이너리 스모크 |
|---|---|---|---|---|---|
| `darwin-arm64` (`macos-15`) | `139817028010` | `12m41s` | **305 passed, 0 failed, 1 ignored** | **PASS (0 warnings)** | `ddg-agents-native 0.1.0` (`--release`) |
| `windows-x86_64` (`windows-latest` MSVC) | `139817028038` | `22m13s` | **215 passed, 0 failed, 1 ignored** | **PASS (0 warnings)** | `ddg-agents-native 0.1.0` (`--release` + FXC 셰이더) |
| `linux-x86_64` (`Bookworm` 컨테이너) | `139817027700` | `6m45s` | **241 passed, 0 failed, 1 ignored** | **PASS (0 warnings)** | `ddg 0.1.0`, `ddg-agents-host 0.1.0`, `ddg-terminal-server 0.1.0` |

> **실 CI 검증으로 신규 확정된 항목**: `windows-latest` 실 MSVC 환경에서 Zig 0.17.1 `ghostty-vt-static.lib` + `gpui = "=0.2.2"` `--release` 실링크, `#[cfg(windows)]` 런타임 유닛 테스트 전량(`BCryptGenRandom`, Named Pipe SID DACL+`GetNamedPipeClientProcessId`+`READ_CONTROL`/`SE_KERNEL_OBJECT`, `LockFileEx`, 실행 중 `.exe` 2회차 `.old.<nonce>` 교체·GC, ConPTY+JobObject 자손 프로세스 정리, Native GPUI 59건)이 모두 실기 러너에서 통과했다.

### C. 실 CI 과정에서 발견·수정한 실제 버그 (커밋 `71e77ad` · `aea6d98` · `1cdd861` · `846c450`)
1. **Windows ConPTY 리디렉션 stdio 상속 버그**: `pty_windows.rs`에 `STARTF_USESTDHANDLES`(null 핸들, `bInheritHandles = false`) 적용 — 데몬/CI처럼 부모 stdio가 파이프·로그로 리디렉션된 환경에서 ConPTY 출력이 누락되던 운영 결함 수정.
2. **Windows 관리자 권한 러너 `TokenOwner` 및 Named Pipe 보안 조회**: `windows.rs`/`fs.rs`/`ipc_windows.rs`에서 `TokenOwner` 병행 허용, 클라이언트 파이프 `READ_CONTROL` + `SE_KERNEL_OBJECT` 조회, `SetSecurityInfo` ACE 분할 검증 및 `rebind` 커널 해제 대기 반영.
3. **macOS `Holder` 초기 기동 유예 레이스 & `flock` fork 윈도우**: `holder.rs`에서 최초 `Core` 연결 전(`generation == 0`) 최소 5초 유예 보장, `connect.rs` 기동 대기 15초 상향, `replace.rs` fork 윈도우 재시도 반영.
4. **Windows CRLF 체크아웃·Linux Bookworm 범위·크로스 Clippy**: `.gitattributes` LF 고정, `SKILL.md`/OpenRPC CRLF 정규화, Linux CI를 1차 범위(headless+CLI 3종)로 정렬, `pty.rs` `openpty` 및 Windows 전용 Clippy 경고 전량 제거.

## 2. 정직한 `미검증` 항목 및 남은 범위

1. **`US-01` Apple Developer ID 실서명·공증·`spctl` (`미검증 — Apple 자격증명 및 GATE-03/04 대기`)**: 현재 `EXPECTED_APPLE_TEAM_ID = "PLACEHOLDER_TEAM_ID"`이며, 실제 Developer ID Application 서명 체인·`notarytool` 공증·CI `stapler validate`·실서명 `.app` 인플레이스 교체는 미실행.
2. **`US-02` Windows 실기 데스크톱 GUI 및 타 로컬 계정 차단 (`미검증 — 대화형 Windows 실기 대기`)**: Windows MSVC 빌드와 `#[cfg(windows)]` 런타임 UT 215건은 실 CI에서 통과했으나, 대화형 GPUI 창·트레이·실제 2계정 Named Pipe/ACL 차단 시나리오는 미실행.
3. **`US-03` 실 원격 Linux `sshd` 호스트 및 `v0.1.0` 레거시 바이너리 업그레이드 (`미검증 — 실 원격 호스트 미확보`)**: `ddg-agents-transport` SSH 스텁·프로토콜 검증과 Bookworm `glibc ≤ 2.36` 검사는 통과했으나, 실제 원격 Linux 서버의 `sshd` 대상 무중단 교체·롤백은 미실행.
4. **2차 Ed25519 운영 서명 (`GATE-02`, 선택 전환)**: 현재 1차 SHA-256 모드(`RELEASE_PUBKEYS = &[]`)로 동작.

## 3. 다음 단계 체크리스트 (Apple Developer ID 준비 시)

1. **`GATE-03` GitHub Secrets 등록** (`dotdotgod/DDG` Settings → Secrets and variables → Actions):
   - `APPLE_CERTIFICATE_P12_BASE64`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, `APPLE_NOTARY_KEY_P8_BASE64`, `APPLE_NOTARY_KEY_ID`, `APPLE_NOTARY_ISSUER_ID`
   - *(2차 Ed25519 전환 시 `GATE-02`)*: Secret `RELEASE_ED25519_PRIVATE_KEY`, Variable `RELEASE_ED25519_KEY_ID`, 코드 `RELEASE_PUBKEYS` 갱신
2. **`GATE-04` Team ID 치환**: `crates/ddg-agents-update/src/macos.rs`의 `EXPECTED_APPLE_TEAM_ID`를 실제 Apple Team ID로 교체 후 커밋·푸시.
3. **`GATE-05` 첫 Draft 릴리스 발행**: `git tag v0.1.0 && git push origin v0.1.0` → `Release` 워크플로우가 `darwin-arm64` 서명·공증·`stapler validate`, `linux-x86_64/aarch64` Bookworm, `windows-x86_64` 빌드 및 `manifest.json`/`SHA256SUMS` 포함 Draft 생성 → `US-01`·`US-02` 검수 후 Public 게시.
