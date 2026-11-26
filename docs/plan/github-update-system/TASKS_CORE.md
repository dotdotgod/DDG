# 태스크 명세 (Phase 0–3: 조사 · Proto/OS · Update 코어 · 데몬/전송)

[계획 목차](README.md) · [UI·UX](DESIGN_UI.md) · [데이터·네트워크](DESIGN_DATA.md) · [테스트](TESTS.md) · [후속 태스크(Phase 4–8)](TASKS_UI_RELEASE.md)

> **구현 세션 공통 지침**: 모든 구현 태스크는 `/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`를 읽고 따른다. 프로젝트 글로벌 정책에 따라 Antigravity Lead 감독 하에 로컬 `pi --provider openai --model gpt-6.2-sol --thinking medium`으로 구현·유닛 테스트를 실행하고, 완료마다 Lead가 `git diff`/테스트를 독립 검수한다. 각 태스크는 `UT-*` 유닛 테스트 작성·실행을 같은 세션에서 완결한다.

---

## T0-A: Windows `x86_64-windows-msvc` `ghostty-vt.lib` + GPUI 0.2.2 링크 스파이크
- **유형 / 상태**: `조사` / `조사 필요` | **역할**: 조사 담당(`COMMON.md`, 프로덕션 쓰기 금지)
- **선행 / 병렬**: 선행 없음 · `PG-0`(`T0-B`·`T1-A`와 병렬 가능) | **승인 게이트**: 없음(독립 POSIX 구현 `T1-A`–`T4-B`·`T6-A`는 대기 없이 계속 진행)
- **목표·수행**: `windows-latest`에서 Zig 0.17.1 `x86_64-windows-msvc` `ghostty-vt.lib` + `ddg-agents-vt` + `native`(`gpui = "=0.2.2"`) 링크 가능성, CRT(`/MD` vs `/MT`), `windows-sys` feature 목록 실증. 실패 시 옵션(링커 플래그 조정 / DLL 전환 / 범위 재결정) 근거 기록.
- **쓰기 허용/금지**: 허용 `docs/plan/github-update-system/spikes/T0-A-windows-msvc-spike.md`·임시 스파이크 디렉터리 / 금지 `Cargo.toml`, `crates/**`, `native/**`, `.github/**`.
- **테스트·완료 기준**: 조사 전용으로 프로덕션 UT 제외(`UT-OS-01..03`은 `T5-A`). 링크 오류 0건 또는 차단 로그+대안 보고서 완성. **비가역**: 없음.

## T0-B: 패키징 · macOS `.app` 서명/공증 · Debian Bookworm(`glibc ≤ 2.36`) · `.gitignore` 조사
- **유형 / 상태**: `조사` / `조사 필요` | **역할**: 조사 담당(`COMMON.md`)
- **선행 / 병렬**: 선행 없음 · `PG-0`(`T0-A`·`T1-A`와 병렬 가능) | **승인 게이트**: `git init`/시크릿 주입은 `T7-A`로 분리(본 태스크 금지)
- **목표·수행**: `DDG Agents.app` 레이아웃(`Info.plist`, `entitlements.plist`, `iconutil`), inside-out 서명→`notarytool`→`stapler staple`/`validate` 순서, `debian:bookworm-slim` `ldd --version` ≤ 2.36 확인, 필요 GitHub Secrets 이름표(값 미기재), `.gitignore` 점검.
- **쓰기 허용/금지**: 허용 `docs/plan/github-update-system/spikes/T0-B-packaging-release-infra.md` / 금지 `.git/**`, `.github/**`, 소스 일체.
- **테스트·완료 기준**: 조사 전용으로 UT 제외(`T5-B` dry-run/`US-01` 연계). 보고서 완성. **비가역**: 없음.

## T1-A: `ddg-agents-proto` 프로토콜 확장 · OpenRPC 동기화 + `ddg-agents-os` POSIX 기반 신설
- **유형 / 상태**: `구현` / `착수 가능` | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: 선행 없음(직렬 — `Cargo.toml`·`proto`·`schema/` 선점; `T0-A`/`T0-B`와 병렬 가능)
- **구현 착수 근거**:
  - 변경 대상: `Cargo.toml`, `Cargo.lock`, `crates/ddg-agents-proto/src/{host.rs,terminal.rs,rpc.rs,openrpc.rs}`, `schema/{ddg-agents-host,ddg-terminal-server}.openrpc.json`, 신규 `crates/ddg-agents-os/**`.
  - 계약: `HelloResult` optional 5필드(`app_version`, `platform`, `holder_protocol_version`, `holder_instance`, `active_pty_count`), `HostHelloResult` optional 6필드(`app_version`, `platform`, `terminal_server_version`, `holder_protocol_version`, `active_pty_count`, `rollback_version`; `rollback_available` 미추가), `rpc::error_code::PROTOCOL_INCOMPATIBLE`, **`host.sock`/OpenRPC에 업데이트·핸드오프 RPC 절대 미추가(`SEC-D-01`)**. `ddg-agents-os` POSIX 구현(`0700`/`0600`, `flock`, `write_atomic`, `SO_PEERCRED`/`LOCAL_PEERPID`, `setsid`, 표준 경로).
  - 리뷰 반영: `DN-09`, `SEC-D-01`, `review-conductor` §4.2 #7.
- **필수 Validation / 제외**: `#[serde(default, skip_serializing_if = "Option::is_none")]` 누락 금지; Windows OS 분기(`BCryptGenRandom`/Named Pipe/DACL)는 `T5-A`로 분리.
- **테스트 (TDD)**: `UT-PROTO-01`, `UT-PROTO-02`(기존 `openrpc_schemas_are_valid_and_up_to_date` 확장), `UT-OS-01`/`UT-OS-02`(POSIX 분기). 명령: `DDG_UPDATE_SCHEMA=1 cargo test -p ddg-agents-proto && cargo test -p ddg-agents-os && cargo check --workspace`. 통과 기준: 전 테스트 100%·경고 0. **비가역**: 없음.

## T2-A: `crates/ddg-agents-update` 코어(`default-features=false`) + `network` GitHub HTTPS·무결성·아카이브
- **유형 / 상태**: `구현` / `착수 가능` (`T1-A` 완료 시) | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: `T1-A` 완료 필수 · 직렬(`Cargo.toml`·`ddg-agents-update/**` 소유)
- **구현 착수 근거**:
  - 변경 대상: `Cargo.toml`, `Cargo.lock`, `crates/ddg-agents-update/src/{lib.rs,error.rs,config.rs,version.rs,redact.rs,release.rs,github.rs,integrity.rs,archive.rs}`.
  - 계약: 31종 `UpdateErrorCode` SSOT, 4배리언트 `ApplyOutcome` SSOT(`Completed`, `HandedOffToHelper`, `PendingHolderRestart`, `AutoRolledBack`), `UpdateConfig`(`skipped_version` 없음, A18), `DEFAULT_UPDATE_REPO`+`DDG_UPDATE_REPO`, 예외 없는 5종 시크릿 레닥션(`F-06`/`SEC-D-04`), SemVer 2.0.0·`stable`/`prerelease`·다운그레이드/스키마 차단, 릴리스 노트 줄 단위 정제(12줄/1,200자)·결정적 URL, `reqwest(rustls-tls-native-roots, Policy::none)` HTTPS:443·5호스트·수동 리다이렉트 ≤5·크로스호스트 `Authorization` 제거·`GITHUB_TOKEN` `api.github.com` 한정·2MiB/256MiB 상한·`ETag`/`304`·`403`/`429`·`auto_check=off` 0건, 스트리밍 SHA-256 + `manifest.json.sig` 분리 Ed25519(`const RELEASE_PUBKEYS = &[]` 1차 SHA-256 모드), 단일 파서 Rust `zip`(`deflate-flate2`)/`tar.gz`(512MiB·2,048 엔트리·위조 헤더 폭탄·중복/`__MACOSX`/`._*`/외부 symlink/ADS·예약명 차단) + `insufficient_disk_space`.
  - 리뷰 반영: `DN-08/10/13`, `SEC-D-02/04/06`, `SEC-E-02/03/04`, `review-conductor` §4.1 #1·§4.2 #5.
- **테스트 (TDD)**: `UT-UPD-01`..`UT-UPD-06`. 명령: `cargo test -p ddg-agents-update --no-default-features && cargo test -p ddg-agents-update --features network`. 통과 기준: 6개 UT 전수 통과·임시파일 잔존 0건. **비가역**: 없음.

## T2-B: `crates/ddg-agents-update` 상태·캐시 GC·macOS `.app` `codesign -R`/헬퍼·스왑/롤백 엔진
- **유형 / 상태**: `구현` / `착수 가능` (`T2-A` 완료 시) | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: `T2-A` 완료 필수 · 직렬(`crates/ddg-agents-update/**`)
- **구현 착수 근거**:
  - 변경 대상: `crates/ddg-agents-update/src/{state.rs,cache.rs,macos.rs,swap.rs,rollback.rs,manager.rs}`.
  - 계약: `update.lock` 하 `update-state.json` RMW(백그라운드 `try_lock` 무음 스킵), Dist Cache 플랫폼별 최근 2개 GC + `N`/`N-1`/`in_progress`/`pending_holder_restart` 핀 보호, `failed_versions` 자동 차단·수동 허용(성공 시 삭제), macOS `AppTranslocation`/쓰기 권한 거부 + 5요소 `codesign -R`(leaf OID `1.2.840.113635.100.6.1.13`, `subject.OU`, `identifier`) + `/usr/sbin/spctl`(클라이언트 `stapler` 제거) + 헬퍼 `lstat`/재검증/`job.json` 폐기 + `renamex_np(RENAME_SWAP)` + `EXDEV` 복사·`fsync`·SHA-256 폴백 + N-1 `.app` 전체 보존 + 롤백 전 N-1 `codesign -R`/SHA-256 재검증, Windows `.old.<nonce>` 툼스톤·startup GC 인터페이스, `UpdateManager` 파사드.
  - 리뷰 반영: `DN-05/06/10/11`, `SEC-D-03`, `SEC-E-01`, `review-conductor` §4.1 #2·§4.2 #6·#8.
- **테스트 (TDD)**: `UT-UPD-07`, `UT-UPD-08`. 명령: `cargo test -p ddg-agents-update --no-default-features && cargo test -p ddg-agents-update --features network`. **비가역**: 없음.

## T3-A: `ddg-terminal-server` + `ddg-agents-host` — `DN-02` Core Detach · `SEC-D-01` 2단계 직접 인계 · 레거시 `v0.1.0`
- **유형 / 상태**: `구현` / `착수 가능` (`T1-A`, `T2-B` 완료 시) | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: `T1-A`, `T2-B` 완료 필수 · **`PG-3` (`T3-B`와 병렬)** — 쓰기 범위 `crates/ddg-terminal-server/**`, `crates/ddg-agents-host/**`만 허용(루트 `Cargo.toml` 편집 금지).
- **구현 착수 근거**:
  - 변경 대상: `crates/ddg-terminal-server/src/{main.rs,core.rs,holder.rs,server.rs,paths.rs}`, `crates/ddg-agents-host/{Cargo.toml,src/{main.rs,server.rs,host_service.rs,token_store.rs,paths.rs,admin.rs(신규)}}`.
  - 계약: 양 바이너리 `--version`, `HelloResult`(5필드·`holder_instance` 보존)/`HostHelloResult`(6필드·`rollback_version`), `ddg-agents-host`는 `ddg-agents-update` `default-features=false`만 링크(`__apply-daemon-swap`/`__rollback-daemon-swap --install-dir`), `terminal-admin`(`detach` 동기 `BYE` 2s 대기 + `terminal.lock` 해제 + Holder 정규 `--bin-path` + `60s` 유예), `host-admin`(**토큰 미반환** + 플러그인/PTY 피어 PID 차단 + `prepare_reload`/`abort_reload`/`execute_reload` 2단계 `tokens_loaded`→`bind_now`→`ready` 익명 파이프 직접 인계 + `reconcile_with_live_ptys` 종료 탭 토큰 `revoke` + 실패 시 구 Host 무손실 재개), `v0.1.0` 레거시 최초 1회 경로.
  - 리뷰 반영: `SEC-D-01`, `DN-02/03/08/09`, `VAL-4.6`.
- **테스트 (TDD + 통합)**: `UT-HOST-01`(기존 `security_negative_boundaries.rs` 확장 포함), `UT-HOST-02`(순수 FSM TDD + `HostSandbox` 통합 분리). 명령: `cargo test -p ddg-terminal-server && cargo test -p ddg-agents-host`. **비가역**: 없음.

## T3-B: `ddg-agents-transport` — `ProtocolIncompatible` · 원격 `Ssh` 업데이트/롤백 · 컨테이너 차단 · `install.rs` 분리
- **유형 / 상태**: `구현` / `착수 가능` (`T1-A`, `T2-B` 완료 시) | **역할**: `implementer` (`/Users/dotdot/.agents/skills/ddg-planning/roles/implementer.md`)
- **선행 / 병렬**: `T1-A`, `T2-B` 완료 필수 · **`PG-3` (`T3-A`와 병렬)** — 쓰기 범위 `crates/ddg-agents-transport/**`만 허용(루트 `Cargo.toml` 편집 금지).
- **구현 착수 근거**:
  - 변경 대상: `crates/ddg-agents-transport/{Cargo.toml,src/{dial.rs,client.rs,install.rs,remote_update.rs(신규),lib.rs}}`.
  - 계약: `DialErrorKind::ProtocolIncompatible`(비재시도), `ConnectionState::ProtocolIncompatible { local_protocol, remote_protocol, remote_version, message }`, `HostEvent::VersionInfo`, 원격 인플레이스 업데이트/수동 롤백 **`HostTransport::Ssh` 전용**(`ContainerSsh`/`DockerExec`은 `container_update_unsupported`; **기존 `install_remote_host`의 `ContainerSsh` 부트스트랩 무회귀 유지**), 사전/사후 `RequirePinnedHostId`+`StrictHostKeyChecking=yes`, 원격 `$HOME/.local/state/ddg/update/staging/<nonce>`(`0700`) + `wc -c` + 3종 해시 fail-closed + 셸 메타문자 차단, `GITHUB_TOKEN` 원격 비전달, `install.rs` `auto_check` 분리 + `dist/` `0700`·SHA-256 검증.
  - 리뷰 반영: `A16`, `DN-01/04/08`, `SEC-D-05`, `SEC-E-03/05`, `test-reviewer F-08`.
- **테스트 (TDD)**: `UT-TRANS-01`, `UT-TRANS-02`. 명령: `cargo test -p ddg-agents-transport`. 통합 게이트(`PG-3` 종료): `Cargo.lock` 정합 후 3크레이트 전체 `cargo test`. **비가역**: 없음.
