# Data · Network · OS · 보안 · CI 설계 (확정본)

[계획 목차](README.md) · [UX·UI 설계](DESIGN_UI.md) · [테스트 설계](TESTS.md) · [사용자 시나리오](SCENARIOS.md)

## 1. 크레이트 아키텍처 및 의존성 선택
```text
ddg-agents-proto  (wire types, OpenRPC, IDs + Windows BCryptGenRandom, no tokio)
       ↑
ddg-agents-os     (paths, flock/LockFileEx, 0700/0600 & SID DACL, write_atomic,
                   LocalListener/Stream [Unix sock / Named Pipe], peer PID/UID, detached spawn)
   ↑    ↑    ↑
   │    │    └── ddg-terminal-server (+ ddg-agents-vt)
   │    ├─────── ddg-agents-transport (+ ddg-agents-proto)
   │    ├─────── ddg-agents-update (default-features=false: swap/rollback core, sha2, semver만)
   │    │               ↑
   │    ├─────── ddg-agents-host (depends on ddg-agents-update default-features=false)
   └────┴─────── ddg-agents-update (default=["network"]: + reqwest[rustls-tls-native-roots,Policy::none],
                        ↑           ed25519-dalek, flate2, tar, zip[deflate-flate2], percent-encoding)
                 ddg-agents-cli, native
```
- **선택 근거**: `ddg-agents-os`로 Unix/Windows 보안·IPC를 단일화하고, `ddg-agents-update`를 `default-features=false` 코어(데몬 공유)와 `network` feature(`cli`/`native` 전용)로 분리해 데몬·Docker 이미지에 HTTP/TLS/zip/tar 의존성 0개를 유지한다.
- **단일 파서 아카이브 추출(`SEC-D-02`)**: macOS `ditto` 추출 제거. `.zip`(macOS `.app.zip`/Windows `.zip`)은 `zip` (`deflate-flate2`), `.tar.gz`(Linux)는 `flate2+tar` 단일 파서로 실제 스트리밍 압축해제 바이트 `≤ 512 MiB`, 엔트리 `≤ 2,048`, 중복 경로·`__MACOSX`·`._*`·`..`·절대경로·외부 symlink·tar hardlink/FIFO·Windows ADS(`:`)·예약장치명(`CON`..`LPT9`)을 전면 거부한다.

## 2. 저장 · 스키마 · 프로토콜 · SSOT 계약
- **경로·권한**:
  - Unix: Config `~/.config/ddg/`(`0700`/`0600`), State `~/.local/state/ddg/`, Dist `~/.local/share/ddg/dist/<version>/<platform>/` (`DDG_*_DIR`/`XDG_*` 우선).
  - Windows: Config `%APPDATA%\ddg\`, State `%LOCALAPPDATA%\ddg\state\`, Dist `%LOCALAPPDATA%\ddg\dist\`, Bin `%LOCALAPPDATA%\ddg\bin\` — 생성 시점 `SECURITY_ATTRIBUTES`로 현재 사용자 SID 전용 `PROTECTED_DACL_SECURITY_INFORMATION` 적용.
- **파일 스키마**:
  - `update.json` (`schema_version: 1`): `auto_check (on|off)`, `channel (stable|prerelease)`, `repo: Option<String>`(`DEFAULT_UPDATE_REPO` 기본, `DDG_UPDATE_REPO` 최우선), `check_interval_secs (21600, 900..=604800)`. **`skipped_version` 없음(A18)**.
  - `update-state.json` (`schema_version: 1`): 모든 RMW는 `update.lock` 하에 수행(백그라운드 체크는 `try_lock` 실패 시 무음 스킵). `last_checked_at`, `etag`, `cached_latest`, `current_version`, `previous_version`, `rollback: Option<RollbackRecord>`, `failed_versions`, `pending_holder_restart`, `auto_rolled_back`, `in_progress`. `failed_versions`는 자동/백그라운드 재시도만 차단하며 명시적 수동 재시도(`--force` 또는 GUI 명시 재적용)는 허용(성공 시 삭제; `--force`는 무결성·서명·스키마·컨테이너 차단 우회 불가).
  - **Dist Cache GC**: 플랫폼별 최근 2개 SemVer 유지 + 활성 `N`, `rollback/prev` `N-1`, `in_progress`/`pending_holder_restart` 핀 보호. 런타임 `SHA256SUMS` 다운로드 및 `verified.json` 마커 **삭제**(적용·롤백 직전 실제 바이트 SHA-256 직접 재검증).
- **분리 서명 (`manifest.json.sig`)**: 1차는 컴파일 타임 `const RELEASE_PUBKEYS: &[(&str, [u8; 32])] = &[];`(SHA-256 필수 모드, 런타임 env 오버라이드 불가). 2차 공개키 내장 시 `manifest.json.sig` 부재·키 불일치·서명 실패 즉시 fail-closed + CI 비공백 키 가드 적용.
- **프로토콜 & OpenRPC 확장 (`#[serde(default, skip_serializing_if = "Option::is_none")]`)**:
  - 4종 바이너리 `--version` 지원 (`native`는 GPUI 초기화 전 즉시 출력).
  - `HelloResult` (신규 5필드): `app_version`, `platform`, `holder_protocol_version`, `holder_instance`(Core 보존), `active_pty_count`.
  - `HostHelloResult` (신규 6필드): `app_version`, `platform`, `terminal_server_version`, `holder_protocol_version`, `active_pty_count`, `rollback_version` (`rollback_available` 삭제).
  - 비호환 오류: `rpc::error_code::PROTOCOL_INCOMPATIBLE`, `DialErrorKind::ProtocolIncompatible`(비재시도), `ConnectionState::ProtocolIncompatible { local_protocol, remote_protocol, remote_version, message }`. `schema/*.openrpc.json` 동기화. **`host.sock`/`hub::allowed`에는 업데이트·핸드오프 RPC 절대 미추가**.
- **공통 `ApplyOutcome` SSOT (4배리언트)**: `Completed { from_version, to_version, tokens_preserved }`, `HandedOffToHelper { helper_pid, to_version }`, `PendingHolderRestart { to_version, active_pty_count }`, `AutoRolledBack { attempted_version, restored_version, reason }`.
- **공통 `UpdateErrorCode` SSOT**: 31종 `snake_case` enum ([DESIGN_UI.md](DESIGN_UI.md#3-상태-소유권--키보드--종료-보호--오류-카드-매핑)와 1:1 동일).

## 3. GitHub HTTP · 원격 SSH · 신뢰 경계
- **GitHub HTTP**: `auto_check=off` 자동 체크 시 HTTP 클라이언트 미초기화(요청 0건). HTTPS 포트 443 전용(userinfo·IP 리터럴 금지), 허용 호스트 5종(`api.github.com`, `github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`, `github-releases.githubusercontent.com`), 수동 리다이렉트 최대 5회 + 크로스 호스트 `Authorization` 제거, `GITHUB_TOKEN`은 `api.github.com` 초기 요청 한정(원격 SSH 비전달). JSON `≤ 2 MiB`, 아카이브 `≤ 256 MiB`, 여유 공간 `>= 3 * archive_size` 선검사, `ETag`/`304`, `403`/`429`→`rate_limited`. 결정적 웹 URL만 사용.
- **원격 업데이트 (`HostTransport::Ssh` 전용, A16)**: `ContainerSsh`·`DockerExec`은 `container_update_unsupported` 차단(단, 기존 `ddg machine install`의 `ContainerSsh` 부트스트랩은 무회귀 유지). 사전/사후 `DialPolicy::RequirePinnedHostId` + `StrictHostKeyChecking=yes`. 원격 `$HOME/.local/state/ddg/update/staging/<nonce>`(`umask 077`, `0700`), `wc -c` + `sha256sum`/`shasum -a 256`/`openssl dgst -sha256` 3종 중 1종 64-hex 일치(모두 부재·불일치 시 스테이징 삭제 후 fail-closed), `--version` 스모크, `--install-dir "$HOME/.local/bin"` 명시. `install.rs` 다운로드는 `auto_check`와 분리 + `dist/` `0700`·SHA-256 검증.

## 4. 핵심 교체·핸드오프·플랫폼 시퀀스
### 4.1 `SEC-D-01` 2단계 Ready-to-Bind 토큰 직접 인계 & `DN-02` Core Detach
- `host-admin`/`terminal-admin`은 `host.sock`과 물리 분리, 피어 PID(`SO_PEERCRED`/`LOCAL_PEERPID`/`GetNamedPipeClientProcessId`)로 알려진 플러그인·PTY 자식 PID 거부, **어떤 요청에도 `TokenSnapshot` 외부 미반환**.
1. `update.lock` 획득 → N-1 백업(`rollback/prev/` + `rollback.json`; `EXDEV` 시 임시 복사+`fsync`+SHA-256+원자 rename) → `host-admin` `prepare_reload`(`draining=true`, `ops` 2s 드레인, `watch_terminal` 중단, `catalog.json` 플러시, 30s 워치독).
2. 디스크 바이너리 원자 교체(Unix `rename`, Windows `<bin>.exe` → `<bin>.exe.old.<nonce>` + `MoveFileExW`) → `terminal-admin` `detach`(Core가 `GOODBYE::Detach` 후 Holder `BYE` 동기 2s 대기, `terminal.lock` 해제 확인) → 정규 `--bin-path`로 신 Core 기동(Holder도 `--bin-path` 사용·`60s` 유예 상한) → `hello`/`ping`/`app_version`/`holder_instance` 검증(실패 시 N-1 복원 + `abort_reload`로 구 Host 무손실 재개).
3. `host-admin` `execute_reload`: **구 Host가 직접 정규 신 Host(`serve --restore-tokens-pipe`)를 익명 `stdin`/`stdout` 파이프로 스폰**(`O_CLOEXEC` / Windows `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`) → **Phase 1**: `TokenSnapshot`(`active`+`revoked`) 전송, 신 Host 읽기 실패 시 fail-closed 종료, `reconcile_with_live_ptys`로 종료 탭 토큰 즉시 `revoke` 후 `tokens_loaded` 송신 → **Phase 2**: 구 Host `host.sock`/`host.lock` 해제 후 `bind_now` 송신 → 신 Host 바인드 후 `ready` 송신 → 구 Host `Zeroize` 종료(실패 시 구 Host 재바인드 + `abort_reload` 무손실 재개).
4. **레거시 `v0.1.0` 최초 1회**: `host-admin` 부재 시 피어 PID → `SIGTERM` → 교체·기동(`tokens_preserved: false` 1회 안내).

### 4.2 macOS `.app` 서명·공증·원자 스왑 & Windows 전체 포팅 & CI
- **macOS `.app`**: `AppTranslocation`·쓰기 권한 선검사. `stapler validate`는 **CI 공증 직후 전용**(클라이언트 제거). 클라이언트 및 `__update-apply-helper` 스왑 직전 이중 검증:
  ```text
  /usr/bin/codesign --verify --deep --strict -R 'anchor apple generic and identifier "<EXPECTED_BUNDLE_ID>" and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = "<EXPECTED_APPLE_TEAM_ID>"' "<app>"
  ```
  + `/usr/sbin/spctl --assess --type execute --verbose=4`. 헬퍼는 `lstat` 경로 검증 + 동일 부모 디렉터리 `renamex_np(RENAME_SWAP)` + N-1 `.app` 전체 `rollback/prev/` 보존(`EXDEV` 복사+검증 폴백) + `~/.local/bin` 링크 검증 + `open` 재실행.
- **Windows 전체 포팅 (17+2 파일)**: `proto/id.rs`(`BCryptGenRandom`), `ddg-agents-os`(Named Pipe `first_pipe_instance(true)` 최초 바인드만 + `FILE_FLAG_OVERLAPPED`/Tokio async + 생성 시점 SID DACL + `LockFileEx`), `terminal-server`(ConPTY + JobObject + `--bin-path`), `host`(Named Pipe + `folder_basename` `\`/`/`), `transport`(OpenSSH + `%APPDATA%`), `cli/attach.rs`(Console VT), `vt/build.rs`(`ghostty-vt.lib`), `native/ui_scale.rs`, 2회차 연속 업데이트 대응 `.old.<nonce>` + startup GC.
- **CI (`.github/workflows/release.yml`)**: `windows-latest` `ghostty-vt.lib`+GPUI 선행 스파이크 게이트 → `macos-arm64`(inside-out 서명·`notarytool`·`stapler staple`/`validate`·`ditto --norsrc` `.app.zip`), `windows-x86_64`(`.zip` 4종), `linux-x86_64`/`aarch64`(Debian Bookworm `glibc ≤ 2.36` 3종 `.tar.gz`) → `manifest.json` + `SHA256SUMS` 발행.

## 5. 적용 범위 · 알려진 한계 · 재검토 조건
- **한계**: ① `v0.1.0` 최초 1회 업그레이드 시 세션 토큰 1회 미승계(`>= v0.2.0`부터 100% 승계), ② Windows 1차 Authenticode 미서명(SmartScreen 가능), ③ Linux 1차 헤드리스+CLI 3종, ④ 원격 호스트 해시 도구 3종 모두 부재 시 fail-closed, ⑤ 1차 `RELEASE_PUBKEYS` 빈 슬라이스(SHA-256 모드).
- **재검토 조건**: ① 2차 Ed25519 키 CI 도입·안정 릴리스 배포 완료 후 `RELEASE_PUBKEYS` 주입 + CI 비공백 가드로 서명 필수 전환, ② Windows SmartScreen 설치 이탈 관측 시 Authenticode 서명 추가, ③ Linux 데스크톱 GUI 수요 확정 시 `ddg-agents-native` Linux 패키징 추가.
