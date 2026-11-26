# UX · Presentation Layer 설계 (확정본)

[계획 목차](README.md) · [데이터·네트워크 설계](DESIGN_DATA.md) · [테스트 설계](TESTS.md) · [사용자 시나리오](SCENARIOS.md)

## 1. UX 흐름과 확정 결정 (A4, A9, A12–A13, A16–A18)
- **진입점 3곳 → 단일 `UpdateModal` 수렴**: ① 사이드바 최하단 버전 행(`SIDEBAR_MIN=160px` 대응), ② macOS 전용 앱 메뉴 `Ddg → 업데이트 확인...` + `Ddg 종료`, ③ 캔버스 상단 1슬롯 업데이트 배너(`reset_banner`와 독립). 별도 설정 센터·중첩 모달·원격 전용 모달 배제.
- **즉시 재시작(A17)**: `PendingRestart` 상태·배너·사이드바 배지 전면 삭제. 로컬 GUI `[업데이트 적용]`은 다운로드·검증·스테이징 후 분리 헬퍼에 인계(`HandedOffToHelper`) → GUI `quitting_for_update_restart = true` 후 `cx.quit()` → 헬퍼가 스왑·Core Detach·재실행 완료(실패 시 자동 N-1 롤백 후 재실행된 GUI가 지속 `AutoRolledBack` 배너 표시).
- **세션 한정 배너 닫기(A18)**: `UpdateConfig.skipped_version` 및 `[이 버전 건너뛰기]` 삭제. 인메모리 `dismissed_available_version: Option<String>` 단일 원본만 유지(사이드바 점·모달에서 언제든 재진입).
- **원격 호스트 정책(A9, A15, A16)**: 인플레이스 업데이트·수동 롤백은 `HostTransport::Ssh` 전용. `DockerExec` 및 `ContainerSsh`는 모두 `container_update_unsupported` + 단일 라인 복사 전용 이미지 갱신 안내(`docker/Dockerfile`·`entrypoint.sh` 무변경).
- **커스텀 레포(A12–A13)**: 추가 강제 플래그(`--allow-custom-source`) 없이 경고 배너(`[커스텀 소스]`, `owner/repo`, 출처[설정/환경변수], 설정 출처 시 `[기본 소스로 복원]`) 표시만으로 적용 허용. `owner/repo` 검증·SHA-256·macOS 공식 DDG Team ID(`const`)는 fail-closed 유지.
- **자동 확인 opt-out(A4)**: `auto_check = off` 시 백그라운드 및 채널 전환 모두 네트워크 요청 0건 보장.

## 2. GUI ASCII 아트 및 화면 규격

### 2.1 사이드바 버전 행 (`SIDEBAR_MIN=160px` 대응) & macOS 메뉴
```text
[일반 폭: sidebar >= 200px]              [최소폭: sidebar = 160px]     ┌ Ddg ──────────────────┐
+----------------------------------+     +--------------------+        │ 업데이트 확인...       │
| [🖥] 머신 추가                    |     | [🖥] 머신 추가     |        ├───────────────────────┤
|   v0.1.0                         |     |   v0.1.0           |        │ Ddg 종료           ⌘Q │
|   v0.1.0           [● 업데이트]   |     |   v0.1.0        ●  |        └───────────────────────┘
|   v0.1.0           [● 적용 중]    |     |   v0.1.0        ●  |        (#[cfg(target_os="macos")])
|   v0.1.0           [● 롤백됨]     |     |   v0.1.0        ●  |
+----------------------------------+     +--------------------+
```
- `vX.Y.Z`: `flex_1 min_w_0 truncate`; 점(7px)/배지: `flex_none whitespace_nowrap`. 우선순위: `롤백됨`(amber) > `적용 중`(amber) > `업데이트`(blue).

### 2.2 캔버스 상단 1슬롯 배너 (`UPDATE_BANNER_HEIGHT=32`, `self.canvas.y` 합산)
- 우선순위: ① 로컬 `Applying`/`AutoRolledBack`/`PendingHolderRestart` > ② 활성 머신 원격 진행/원격 자동롤백/동일 프로토콜 버전 불일치(`BlockedCard::ProtocolIncompatible` 활성 시 제외) > ③ 로컬 `UpdateAvailable`(세션 미닫힘).
- 비노출: `Idle`, `UpToDate`, 백그라운드 `Checking`, 백그라운드 자동 확인 오류. 터미널 포커스 중 `Escape`는 PTY로 전달.
```text
(1) | ⟳ v0.2.0 적용 중... 완료 시 앱이 자동 재시작됩니다          [상세 보기] |
    | ⚠ v0.2.0 실패로 v0.1.0 자동 롤백되었습니다         [자세히]    [확인]   |
    | ⚠ 로컬 호스트(Holder) 재시작 필요 (세션 영향)               [상세 보기] |
(2) | ℹ [gpu-1] 버전 불일치 (원격 v0.1 / 로컬 v0.2) [원격 호스트 업데이트][×] |
    | ℹ [dev-ctr] 버전 불일치 (원격 v0.1 / 로컬 v0.2)        [안내 보기]  [×] |
(3) | ↑ 새 버전 v0.2.0 사용 가능                              [자세히]    [×] |
```

### 2.3 통합 `UpdateModal` (`520px`, `max_w_full max_h_full overflow_y_scroll`) & `BlockedCard`
```text
+----------------------------------------------------------------------+
| 업데이트                                                         [×] |
+----------------------------------------------------------------------+
| [커스텀 소스] acme/ddg-fork · 출처: 설정          [기본 소스로 복원]  |
+----------------------------------------------------------------------+
| 로컬 앱 · 새 버전 사용 가능 · v0.1.0 → v0.2.0 (stable)               |
| ┌ 릴리스 노트 (평문, lines().take(12) -> sanitize -> 1,200자) ─────┐ |
| │ - 원격 SSH 무중단 업데이트 및 N-1 롤백 지원                       │ |
| └──────────────────────────────────────────────────────────────────┘ |
| ℹ 다운로드·검증 후 앱이 자동 재시작됩니다 (실행 중 세션 유지)         |
| [릴리스 노트 열기 ↗]                                 [업데이트 적용] |
+----------------------------------------------------------------------+
| [인라인 2단계 확인: Holder 세션 종료 / 다운그레이드 / 수동 롤백]      |
| ⚠ 확인 필요 · Enter 키로 실행되지 않습니다. 버튼을 클릭하세요.        |
|                                   [취소 (Esc)] [세션 종료 후 적용]   |
| (schema_incompatible_downgrade 시: 확인 버튼 제거 + 차단 사유만)      |
+----------------------------------------------------------------------+
| 설정: 자동 확인 [● 켜짐] · 채널 [stable][prerelease] · [지금 확인]    |
|       이전 백업: v0.0.9                  [이전 버전으로 롤백(v0.0.9)] |
+----------------------------------------------------------------------+
| 원격(Ssh): gpu-1 · v0.1.0 · [원격 호스트 업데이트][원격 호스트 롤백]  |
| 원격(DockerExec/ContainerSsh): 이미지 갱신 안내 [▾ 안내 보기]         |
|   [ docker pull <image>:v0.2.0 && ... (단일 라인, 읽기 전용) ][복사]  |
+----------------------------------------------------------------------+
```
```text
[BlockedCard::ProtocolIncompatible (중앙 차단 카드)]
+----------------------------------------------------------------------+
| ⚠ 프로토콜 버전 비호환 (원격 p1 / 로컬 p2)                            |
|   • 원격 구버전 Ssh           -> [원격 호스트 업데이트]               |
|   • 원격 구버전 Docker/CtrSsh -> [안내 보기]                          |
|   • 로컬 구버전               -> [로컬 업데이트 확인]                 |
+----------------------------------------------------------------------+
```

## 3. 상태 소유권 · 키보드 · 종료 보호 · 오류 카드 매핑
- **원본 소유권**: `Desktop.update: UpdateController`(로컬 버전·설정·`rollback_version`·`phase`·`active_req_id`·`dismissed_available_version`·`quit_requested_during_apply`·`quitting_for_update_restart`·`applying_guard: Arc<AtomicBool>`), `Desktop.update_modal: Option<UpdateModalState>`(`inline_confirm`, `container_guide_expanded`, `copied_hint`만), `MachineView.remote_update: RemoteMachineUpdateState`(머신별 독립 상태·`req_id`·`install_req_id`와 상호배타).
- **모달·키보드(`Desktop::key()`)**: `zoom_action` → `update_modal`(`stop_propagation`, `Escape`는 인라인 확인 취소→모달 닫기→`self.focus` 복구, **`Enter` 고위험 2단계 실행 금지**) → `machine_modal` → `rename` → `reset_notice` → PTY. 양방향 모달 상호배타(`TestState::Installing` 중 `update_modal` 오픈 차단).
- **종료 안전성**: `Quit`/`on_window_should_close`는 `WeakEntity<Desktop>`/`applying_guard` 조회 → `quitting_for_update_restart == true`면 즉시 통과, `is_applying()` 중이면 보류+안내 표시; `on_app_quit`은 OS 강제종료 대비 best-effort 최대 5초 대기.
- **표시·복사 보안**: 릴리스 노트는 `lines().take(12)` 먼저 분리→줄별 `sanitize_display_text`+`\u{2028}`/`\u{2029}` 제거→1,200자 절단. `[릴리스 노트 열기]`는 결정적 `https://github.com/{owner}/{repo}/releases/tag/{percent_encoded_tag}`만 허용(API `html_url` 무시). 컨테이너 템플릿은 단일 라인 복사 전용(개행·Bidi 제거, 플랫폼 셸 이스케이프, 자동 실행 금지).
- **`UpdateErrorCode` 31종 Exhaustive 매핑 (문자열 비교 금지)**:
  - **일시 오류 카드(`[다시 시도]`)**: `network_error`, `lock_busy` (2종)
  - **Rate Limit 카드(Reset 시각 + 분리된 정적 `copy_hint` `GITHUB_TOKEN=<token> ddg update check` + `[명령 복사]`)**: `rate_limited` (1종; `error.message`에 `=` 미포함)
  - **구조적·보안·정책 카드(`[다시 시도]` 미노출)**: 나머지 28종 (`update_disabled`, `invalid_repo`, `disallowed_url_or_host`, `redirect_limit_exceeded`, `response_too_large`, `asset_not_found`, `manifest_invalid`, `checksum_mismatch`, `signature_invalid`, `signature_missing`, `codesign_invalid`, `notarization_invalid`, `app_translocated`, `install_dir_not_writable`, `insufficient_disk_space`, `archive_unsafe`, `confirmation_required`, `downgrade_denied`, `schema_incompatible_downgrade`, `holder_restart_required`, `failed_version_blocked`, `rollback_unavailable`, `rollback_verification_failed`, `daemon_swap_failed_rolled_back`, `daemon_swap_failed_rollback_failed`, `container_update_unsupported`, `protocol_incompatible`, `host_security_blocked`).

## 4. CLI (`crates/ddg-agents-cli`) 계약
- **서브커맨드(`args.rs`)**: `Action::{UpdateStatus, UpdateCheck, UpdateApply, UpdateRollback, UpdateConfig}` + `HELP`·`SKILL.md`·`docs/spec/requirements.md` 정합화.
- **`-H` 고정 불변식**: `ddg update` 계열은 `main::selected()`(`default_host_id`) **미사용** — `-H` 생략 시 항상 `"local"` 고정. `status`/`check`는 `-H <id>`·`-H all` 허용, `apply`/`rollback`은 `-H <id>` 허용·**`-H all` 거부**, `config`는 **`-H` 자체 거부**.
- **플래그·안전**: `apply [--channel <stable|prerelease>] [--version <tag>] [--allow-downgrade] [--restart-sessions] [--force] [--yes]`, `rollback [--restart-sessions] [--yes]`, `config [--auto-check <on|off>] [--channel ...] [--repo <owner/repo> | --clear-repo]`. `--token` 글로벌 차단, 비-TTY 프롬프트 금지(fail-closed).
- **출력·레닥션**: `--json`은 stdout **단일 JSON 객체 + `\n`**(진행 로그는 plain-text `stderr` 전용, NDJSON 미사용), 타 CLI 명령은 업데이트 알림·네트워크 0건. 예외 없는 레닥션(`github_token=`, `gh_token=`, `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`/`github_pat_`, `authorization:`, `bearer `, `://user:pass@`)을 CLI `output.rs`와 GUI 오류 카드 모두에 적용(`GITHUB_TOKEN=<token>` 우회 예외 삭제).
