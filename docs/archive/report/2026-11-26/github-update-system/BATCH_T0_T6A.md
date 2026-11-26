# 구현·검증 배치 보고 (`T0-A`–`T6-A`): GitHub Releases 기반 업데이트 시스템

- **작성 일시**: `2026-11-26` | **활성 계획**: [docs/plan/github-update-system/README.md](../../../../plan/github-update-system/README.md)
- **상태**: **`T0-A`–`T6-A` 전 항목 구현 및 로컬·Docker·크로스체크 검증 완료** — 비가역 외부 승인 게이트(`T7-A`: `GATE-01`–`GATE-05`, `T8-A`: `GATE-06`) 직전 정지.

## 1. 완료 태스크 및 독립 검증 요약

| Phase / Task | 범위 | 주요 산출물 | 독립 검증 결과 |
|---|---|---|---|
| `Phase 0` (`T0-A`, `T0-B`) | 정적 스파이크 | [T0-A](../../../../plan/github-update-system/spikes/T0-A-windows-msvc-spike.md), [T0-B](../../../../plan/github-update-system/spikes/T0-B-packaging-release-infra.md) | 완료 (`windows-sys 0.61.2`, `ghostty-vt-static.lib`, Bookworm `glibc 2.36`, macOS 서명 순서 확정) |
| `Phase 1` (`T1-A`) | `proto` + `os` | `HelloResult`(5)/`HostHelloResult`(6), `PROTOCOL_INCOMPATIBLE`, OpenRPC 동기화, 신규 `crates/ddg-agents-os` | `cargo test -p ddg-agents-proto -p ddg-agents-os`: **42 passed, 0 failed** |
| `Phase 2` (`T2-A`, `T2-B`) | `ddg-agents-update` | 31종 `UpdateErrorCode`, 4배리언트 `ApplyOutcome`, HTTPS:443 5호스트·SHA-256·`manifest.json.sig` 슬롯·Rust `zip`/`tar`, `update.lock` RMW·Dist GC·macOS 5요소 `codesign -R`·`renamex_np`·N-1 롤백 | `--no-default-features` **17 passed**, `--features network` **27 passed**, `clippy -D warnings` 0건 |
| `Phase 3` (`T3-A`, `T3-B`) | 데몬 + 전송 | `DN-02` Core Detach/`terminal-admin`, `SEC-D-01` `host-admin` 2단계 익명 파이프 직접 인계(`--restore-tokens-pipe`)·`__apply-daemon-swap`/`__rollback-daemon-swap`, `ProtocolIncompatible` 비재시도, `HostTransport::Ssh` 전용 원격 업데이트/롤백·컨테이너 차단(`A16`) | `terminal-server` **23**, `host` **54**, `transport` **39** passed (1 ignored `docker_live`), `host` 의존성 트리 HTTP/zip/tar **0건** |
| `Phase 4` (`T4-A`, `T4-B`) | CLI + Native GUI | `ddg --version`, `ddg update {status,check,apply,rollback,config}`, `__update-apply-helper`, `-H local` 고정, 5종 레닥션, `docs/spec/requirements.md` 정합화; GPUI 단일 `UpdateModal`·A17 즉시 재시작·A18 세션 닫기·31종 카드·`Desktop::route_key`·종료 보호 | `ddg-agents-cli` **36**, `ddg-agents-native` **61** passed; `ddg-agents-native --version` → `ddg-agents-native 0.1.0` |
| `Phase 5` (`T5-A`, `T5-B`) | Windows 포팅 + 패키징/CI | `BCryptGenRandom`, Named Pipe SID DACL/`GetNamedPipeClientProcessId`, `LockFileEx`, `.old.<nonce>` 2회차 교체·GC, ConPTY+JobObject, `vt/build.rs`, Console VT; `.github/workflows/{ci,release}.yml`, `scripts/release/*`, `assets/macos/*`, `docker/Dockerfile`(`release-binaries`), `.gitignore` | macOS `cargo test --workspace`: **303 passed, 0 failed, 1 ignored**; `cargo clippy --workspace --all-targets -- -D warnings`: **전 9크레이트 0 warnings**; `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets`: **전 9크레이트 exit 0**; `actionlint` 0오류; 무서명 macOS dry-run + 실제 Rust 업데이터 파서/Ed25519 서명/14개 해시 + Bookworm `check-glibc.sh` 통과 |
| `Phase 6` (`T6-A`) | 실환경 검증 | [T6-A-report.md](../../../../plan/github-update-system/verification/T6-A-report.md) | `US-04` **PASS**, `US-05` **PASS**, `US-06` **PASS** (`US-03` 실 원격 SSH 호스트는 `미검증`) |

## 2. 정직한 `미검증` 항목 및 남은 위험
1. **Windows 런타임 및 MSVC 실링크 (`미검증(Windows 환경 미실행)`)**: `#[cfg(windows)]` 유닛 테스트(`UT-PROTO-02` Windows 분기, `UT-OS-01..03`, `UT-HOST-03` ConPTY+JobObject), `windows-latest` Zig 0.17.1 `ghostty-vt-static.lib` + `gpui = "=0.2.2"` `--release` 실링크, `US-02` 실기 2회차 `.old.<nonce>` 교체·타 계정 DACL 차단.
2. **Apple Developer ID 실서명·공증·`spctl` (`미검증(자격증명·GATE-04 대기)`)**: `EXPECTED_APPLE_TEAM_ID` 실값 반영, 실제 Apple 체인 5요소 `codesign -R`·`notarytool`·CI `stapler validate`·`US-01` 실서명 `.app` 교체 및 재실행.
3. **원격 Linux 실 SSH 호스트 (`미검증(원격 호스트 미확보)`)**: `US-03` 실 `sshd` 호스트 대상 무중단 교체·롤백 및 실제 배포된 `v0.1.0` 레거시 바이너리 최초 1회 업그레이드.
4. **2차 Ed25519 운영 서명 전환**: 현재 1차 SHA-256 모드(`RELEASE_PUBKEYS = &[]`)이며, `GATE-02` 승인 후 운영 공개키 내장 및 CI 비공백 가드 활성화 예정.

## 3. 비가역 승인 게이트 체크리스트 (`T7-A` · `T8-A` — 승인 전 미실행)
- **`GATE-01`**: `git init`, 공식 `DEFAULT_UPDATE_REPO`(`owner/repo`) 확정, Public GitHub 원격 연결 및 초기 `git push` (`.git/` 미초기화 유지 중, `.gitignore` 민감파일 차단 적용 완료).
- **`GATE-02`** *(2차 전환 시)*: Ed25519 운영 키쌍 생성 및 `const RELEASE_PUBKEYS` 커밋.
- **`GATE-03`**: GitHub Secrets 등록(값은 사용자가 직접 설정) — `APPLE_CERTIFICATE_P12_BASE64`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, `APPLE_NOTARY_KEY_P8_BASE64`, `APPLE_NOTARY_KEY_ID`, `APPLE_NOTARY_ISSUER_ID` (+ 2차 `RELEASE_ED25519_PRIVATE_KEY`, 변수 `RELEASE_ED25519_KEY_ID`).
- **`GATE-04`**: 실제 Apple Developer ID 서명 및 `notarytool` 공증 제출 (`EXPECTED_APPLE_TEAM_ID` 실값 치환 포함).
- **`GATE-05`**: 태그(`vX.Y.Z`) 푸시 및 Draft 릴리스 검수 후 첫 Public GitHub Release 발행.
- **`GATE-06` (`T8-A`)**: `T6-B`(`US-01`·`US-02`) 완료 후 `FINAL.md` 작성·아카이브 이관 및 `main` 머지/푸시.
