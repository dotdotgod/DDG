# UX 및 Presentation Layer 설계 (`DESIGN_UI.md`)

관련 문서: [계획 개요](README.md) · [데이터·네트워크·보안 설계](DESIGN_DATA.md) · [TDD 유닛 테스트](TESTS.md) · [사용자 시나리오](SCENARIOS.md)

---

## 1. 확정 UX 5대 흐름 요약 (Phase C + 리뷰 반영 확정)

1. **흐름 1 (`ddg` CLI 에이전트 실행 & 기본 에이전트 설정)**:
   - `ddg agent add/set/ls/show/default/rm`, `ddg run [agent] [--room <rm> | --new-room [--name <name>] | --bg/--background] [--cwd <dir>] [--title <t>] [--cols <n>] [--rows <n>] [-- <extra-args...>]`, 기존 `room create`/`session create`/`session split`에 `--agent <id>`(+ `-- <extra-args>` append).
   - 기본 `ddg run`: DDG 가시 세션 내부(동일 `host_id` 라이브 카탈로그 검증)면 현재 룸에 새 탭, 외부 셸/`rm_bg_*`/타 호스트면 현재 `cwd`(원격은 로컬 `cwd` 절대 자동 전달 금지)로 새 가시 룸 생성 + 즉시 인터랙티브 attach(`Ctrl-]` detach 시 세션 유지 안내 출력). 비-TTY/`--json`에서는 `--bg` 요구 오류(Fail-closed).
   - `ddg agent show`: 기본 `env` 값 마스킹, `--reveal-env` 명시 시에만 평문 출력(D-3).
2. **흐름 2 (탭 아이콘 & 상태·집계·읽음)**:
   - 아이콘 우선순위: `TabRecord.agent_profile` / OSC 7501 `app` → OS `foreground` 최소 식별자(`script_hint` → `comm`) → 실행기 `argv[0]` → 기본 터미널 SVG.
   - 상태 규범: `AgentState::{ Idle, Working, Blocked, Done, Error }` + `BlockedKind::{ Permission, Question, Auth }`.
   - 집계 우선순위: **`Blocked` > `Error` > `Working` > `Done`(미확인) > `Done`(확인)/`Idle`** (`StoppedAfterReboot` 별도).
   - 읽음(`seen`): 호스트 RAM `activity_seq`/`seen_seq` 원본. `window_active == true` + 포커스 패인(또는 Peek 진입) + 창 비활성→활성 복귀(`observe_window_activation`) 시 `host.tab_mark_seen` 1회 발송. `Blocked`는 미확인 펄스만 해제하고 `!` 배지 자체는 상태 해소까지 유지. 호스트 단절 시 스피너 즉시 정지(0Hz).
3. **흐름 3 (에이전트 리스트 · 서브에이전트 트리 · 백그라운드 Peek)**:
   - 사이드바 접이식 2단 섹션: 상단 `에이전트`(최대 높이 45% + 자체 스크롤, `룸` 접힘 시 45% 상한 해제) + 하단 `룸`.
   - 포함 대상: OSC 7501 보고 세션 + `ddg` 에이전트 프로필 실행 세션(일반 셸 제외). `rm_bg_*`는 실행 중일 때만 `bg` 배지+트리로 노출, 종료 시 즉시 제거.
   - 정렬 3종: `priority` / `grouped` / `manual`. 핀(`📌`)은 최상단 고정; 수동 순서 이동(`Alt+↑/↓` 또는 호버 버튼)은 핀 섹션 내부·비핀 섹션 내부끼리 서브트리 단위로 이동하며, `priority`/`grouped`에서 첫 이동 시 현재 표시 순서를 `manual_agent_order`에 스냅샷 후 `manual` 자동 전환(D-2).
   - 백그라운드 Peek(UX-1, D-1): `active_room_id` 비덮어쓰기 `peek: Option<PeekState>`. 기본 `want_control = false`(`Readonly`), `[제어 요청]`으로 승격 없이 임시 키 입력 가능. `Readonly`에서 `Esc` = Peek 닫기; `Control`에서 `Esc` = 터미널 전달 & `Cmd/Ctrl+W`·`[닫기]` = Peek 닫기(절대 `close_tab` 금지; Peek이 직접 제어권을 잡았고 메인 페인이 기존 제어자가 아니었을 때만 `release_control` 후 `detach_channel`). Peek 중 종료 시 마지막 VT 버퍼 위 종료 배너 + `[닫기]`만 표시.
4. **흐름 4 (GUI 런처 4지선다 & `AgentSettingsModal`)**:
   - 일반 `+` 탭 버튼은 `DefaultShell` 100% 유지. 별도 에이전트 분할 버튼(`[⚡▾]`) 및 `Cmd/Ctrl+Shift+P`로 단일 통합 `AgentLauncher` 팝오버 열기(실행 위치 4지선다: `[현재 룸 새 탭]|[분할]|[새 룸]|[백그라운드]`).
   - `AgentSettingsModal` (`Cmd+,` macOS / `Ctrl+Shift+,` Linux): `argv` 한 줄 입력(`split_safe_tokens` 공유) + 칩 프리뷰, `env` 행 편집(기본 마스킹+보기 토글, `DDG_*`·시크릿 키·동적 로더 4종 차단), 원격 머신 존재 시 접이식 `host_overrides`(`argv` + `env` 3상태: 상속 유지/덮어쓰기/`null` 제거), 외부 변경 충돌 배너(`[외부 변경 불러오기]` Primary / `[내 변경으로 덮어쓰기]` Secondary), 인라인 2단계 확인(중첩 팝업 금지).
5. **흐름 5 (오류·복구)**:
   - `agents.json` 손상 시 Fail-closed 복구 카드(`[다시 불러오기]` / `[경로 복사]` / `[백업 후 초기화]` → `agents.json.corrupt.<ts>` `0o600`), 일반 셸 격리.
   - `programs.json`/로컬 SVG 손상 시 내장 카탈로그로 안전 폴백 + 세션당 키별 1회 경고.
   - 구버전 원격 호스트(`!"agent_activity_v1"`): `Connected` + `Ssh|ContainerSsh`에서만 `[원격 업데이트]` 인라인 2단계 확인(`expected_host_id` 핀 보존); `DockerExec`는 수동 업데이트 안내만.

---

## 2. 필수 ASCII 아트 화면 설계 (5종 — 전 리뷰 체크리스트 반영)

### (1) 메인 레이아웃 전체 (2단 사이드바 · 서브에이전트 트리 · 탭 바 · 분할 패인 22px 헤더 · 구버전 배너)

```text
+-------------------------------------+------------------------------------------------------------------+
| [Ddg] Ddg                           | [1 [C] claude * x] [2 [G] codex ! ] [3 [>_] zsh] [+] [⚡v] [100%]|
+-------------------------------------+------------------------------------------------------------------+
| v 에이전트 (2* 1! 1x) [우선v] [+][⚙] | [!] 구버전 원격 호스트(gpu-box): 상태/아이콘 제한 [원격 업데이트][x]|
| +---------------------------------+ | +-----------------------------+--+-----------------------------+ |
| | ★ [C] claude-main           [*] | | | [C] claude-main [*] 실행 중 |  | [G] codex-rev  [!] 승인대기 | |
| |     api-server · 로컬            | | | (22px 패인 헤더, #5c8aff)   |  | (22px 패인 헤더, 비포커스)  | |
| |     세션 보고: 테스트 3/5...     | | +-----------------------------+  +-----------------------------+ |
| |   ├─ [P] pi-sub          [*] BG | | | $ claude                    |  | [읽기 전용]     [제어 요청] | |
| |   └─ [P] pi-lint         [x] BG | | | > Running cargo test...     |  | > Approve git push? (y/n):  | |
| |---------------------------------| | |   test proto ... ok         |  |                             | |
| | [!] [G] codex-review     [P][↑↓]| | |   test native ...           |  |                             | |
| |     web-app · gpu-box           | | |                             |  |                             | |
| |     세션 보고: 승인 대기 (git)   | | |                             |  |                             | |
| | [v] [C] claude-docs          •  | | |                             |  |                             | |
| |     docs · 로컬                  | | |                             |  |                             | |
| +--- (최대 45%, 룸 접힘시 해제) --+ | |                             |  |                             | |
|-------------------------------------| |                             |  |                             | |
| v 룸                                | |                             |  |                             | |
|  [=] 로컬                   [+] ● v | |                             |  |                             | |
|    api-server             [* 1] [✎] | |                             |  |                             | |
|    docs                   [• 1]     | |                             |  |                             | |
|  [=] gpu-box                [+] ● v | |                             |  |                             | |
|    web-app                [! 1]     | |                             |  |                             | |
|  [=] old-box                [+] ● v | |                             |  |                             | |
|    dev-shell                        | |                             |  |                             | |
|  (남은 55%+ 공간, 자체 스크롤)       | |                             |  |                             | |
|-------------------------------------| |                             |  |                             | |
| [=] 머신 추가      [⚙] 에이전트 설정 | |                             |  |                             | |
+-------------------------------------+ +-----------------------------+--+-----------------------------+
```

- **분할 패인 22px 헤더**: `count > 1 && zoomed_tab_id.is_none()`일 때만 표시. `scaled_pane_grid` 가용 높이에서 `scale.px(22.)` 차감, `Readonly`/`Stopped` 오버레이는 헤더 아래 배치. 구버전 원격 호스트 배너 높이도 `self.canvas.y` (`top`) 계산에 합산해 세로 분할 드래그 오차 방지.
- **0룸(`show_empty`) 진입점**: 탭 바가 숨겨져도 사이드바 `에이전트` 헤더의 `[+]`와 빈 화면의 `[기본 에이전트 시작]` 버튼으로 런처 즉시 접근.
- **비대화형 `msg`**: 3행 `≤80자` 단일 줄(`#8a8a8a`, 버튼/링크/시스템 배너 스타일 금지), 호버 툴팁 `≤240자` + `세션 보고: ` 접두사.

### (2) 좁은 사이드바(`sidebar <= scale.px(185.)`) 반응형 축약 상태

```text
+--------------------+--------------------------------------------------------------+
| [Ddg] Ddg          | [1 [C] claude * x] [2 [G] codex !] [+] [⚡v]                 |
+--------------------+--------------------------------------------------------------+
| v 에이전트 (4) [⇅]+|                                                              |
| +----------------+ |  (터미널 캔버스)                                              |
| | ★ [C] claude..*| |                                                              |
| |  └[P] pi-sub * | |   * 논리 폭 기준 축약 (sidebar <= scale.px(185.)):            |
| | ! [G] codex-.. | |     - 1줄 압축 (상태 + 14px 아이콘 + 제목 truncate)           |
| | v [C] docs   • | |     - 2행(룸/머신)·3행(세션 보고) -> 호버 툴팁 통합 표시      |
| +----------------+ |     - 트리 들여쓰기 6~8px 축소 (시각 깊이 최대 3단계 클램프)  |
|--------------------|     - [BG]는 좌측 2px 점선 보더로 표기                        |
| v 룸               |     - 룸 행 집계는 최우선 상태 점 1개로 축약                  |
|  [=] 로컬   [+]● v |     - 하단 푸터: `[+ 머신] [⚙]` 가로 2버튼 압축               |
|    api-server   *  |                                                              |
|    docs         •  |                                                              |
|--------------------|                                                              |
| [+ 머신]       [⚙] |                                                              |
+--------------------+--------------------------------------------------------------+
```

### (3) 백그라운드 Peek 모드 (정상 `Readonly`/`Control` 전환 + Peek 중 종료 상태)

```text
[3a: 정상 Peek — 기본 Readonly, [제어 요청] 시 Control 임시 키 입력]
+---------------------------+--------------------------------------------------------------------+
| [Ddg] Ddg                 | [BG] [P] pi-sub · 로컬  [읽기 전용 | Esc: 닫기]                    |
|---------------------------|   [제어 요청] [현재 룸으로 이동] [새 룸으로 승격] [닫기 (Esc)]     |
| v 에이전트 (1* 1!) [우선v]|--------------------------------------------------------------------+
| +-----------------------+ |  (Control 획득 시 배너 문구 전환:)                                 |
| | [C] claude-main   [*] | |  [BG] [P] pi-sub · 로컬 [임시 제어 중 | Esc: 터미널 · Cmd+W: 닫기] |
| |===> [P] pi-sub   [BG] | |    [제어 반납] [현재 룸으로 이동] [새 룸으로 승격] [닫기 (Cmd+W)]  |
| |     백그라운드 · 로컬  | |--------------------------------------------------------------------+
| +-----------------------+ |  $ pi --provider openai ...                                        |
|---------------------------|  > Approve tool call? (y/n): ▏                                     |
| v 룸                      |  * Readonly: Esc / Cmd+W -> Peek 닫기 + detach_channel             |
|  · api-server (유지)      |  * Control : Esc -> 터미널 전달 / Cmd+W·[닫기] -> Peek 닫기        |
|                           |    (+ Peek 단독 획득 제어권만 release_control 후 detach_channel)   |
|                           |  * 절대 close_tab 금지 / Peek 중 분할 단축키 비활성                |
+---------------------------+--------------------------------------------------------------------+

[3b: Peek 중 백그라운드 세션 종료 상태]
+---------------------------+--------------------------------------------------------------------+
| [Ddg] Ddg                 | [BG] [P] pi-sub · 로컬  [종료됨]                      [닫기 (Esc)] |
|---------------------------|--------------------------------------------------------------------+
| v 에이전트 (1*)    [우선v]| +----------------------------------------------------------------+ |
| +-----------------------+ | | [i] 백그라운드 세션이 종료되었습니다 (종료 코드 0)        [닫기] | |
| | [C] claude-main   [*] | | +----------------------------------------------------------------+ |
| | (pi-sub는 A4에 따라   | |  $ pi --provider openai ...                                        |
| |  트리에서 즉시 제거됨)| |  > Done.  (마지막 VT 버퍼 유지 · 스크롤 읽기 가능)                 |
| +-----------------------+ |  ([제어 요청]·[이동]·[승격] 버튼 숨김 · Esc/Cmd+W -> 기존 룸 복귀) |
+---------------------------+--------------------------------------------------------------------+
```

### (4) 단일 통합 `AgentLauncher` 팝오버 (`Cmd/Ctrl+Shift+P` 또는 `[⚡▾]` / 사이드바 `[+]`)

```text
  [1 [C] claude *] [+] [⚡v]
                        +------------------------------------------------+
                        | 에이전트 실행                      대상: 로컬 v |
                        |------------------------------------------------|
                        | 위치: ([현재 룸 새 탭])[분할][새 룸][백그라운드]|
                        |------------------------------------------------|
                        | [🔍 이름 또는 명령어 필터...▏                  ] |
                        |------------------------------------------------|
                        | > ⌘1  [C] Claude Code                 ★ 기본   |
                        |       claude --permission-mode default         |
                        |   ⌘2  [G] Codex CLI                            |
                        |       codex                                    |
                        |   ⌘3  [P] Pi Agent               (gpu-box 재정의)|
                        |       /opt/bin/pi --provider openai ...        |
                        |------------------------------------------------|
                        | [⚙ 에이전트 설정... (Cmd+,)]      Enter: 실행   |
                        +------------------------------------------------+

[변형 A: 빈 카탈로그]                       [변형 B: agents.json 로드 오류 (Fail-closed)]
+--------------------------------------+   +--------------------------------------+
| 등록된 에이전트가 없습니다.           |   | [!] agents.json을 불러올 수 없습니다 |
| 기본 에이전트를 추가해 실행하세요.    |   |     JSON 형식 또는 스키마 오류       |
|        [+ 에이전트 추가하기]          |   |     (원본 입력값은 노출하지 않음)    |
+--------------------------------------+   |   [에이전트 설정에서 복구 열기]      |
                                           +--------------------------------------+
```

- 일반 숫자키 `1..9`는 항상 필터 텍스트 입력으로 사용(`gpt-4`, `o3` 검색 가능), 빠른 실행은 `↑`/`↓`+`Enter` 또는 `Cmd/Ctrl+1..9`.
- `active_room_id.is_none()` 또는 활성 룸 `tabs.len() >= 16`이면 `[현재 룸 새 탭]`·`[분할]` 비활성, `[새 룸]` 자동 선택. `peek.is_some()`이면 `[분할]` 비활성.

### (5) `AgentSettingsModal` (`Cmd+,` macOS / `Ctrl+Shift+,` Linux, 폭 `w(640).max_w_full()`)

```text
[5a: 정상 편집 + 외부 변경 충돌 배너 + env/host_overrides 3상태 + 인라인 2단계 삭제]
+----------------------------------------------------------------------------------+
| 에이전트 설정 (~/.config/ddg/agents.json)                                    [x] |
+----------------------------------------------------------------------------------+
| [!] agents.json이 외부(CLI 등)에서 변경되었습니다.                               |
|     [외부 변경 불러오기 (Primary)]     [내 변경으로 덮어쓰기 (Secondary)]         |
+----------------------+-----------------------------------------------------------+
| 에이전트 목록 [+ 추가]| ID: [ claude              ]  이름: [ Claude Code▏       ] |
|----------------------| 실행 명령어 (argv 직접 실행 · 셸 연산자/개행 금지):        |
| > [C] Claude Code ★  | [ claude --permission-mode "default"                    ] |
|   [G] Codex CLI      | argv 칩: [claude] [--permission-mode] [default]           |
|   [P] Pi Agent       | 아이콘: ([자동/내장: claude v]) [C] 미리보기  [x] 기본     |
|                      |-----------------------------------------------------------|
|                      | 환경변수 (비비밀 전용 · DDG_* 및 동적로더 4종 차단) [+ 행]|
|                      |  CLAUDE_DISABLE_TELEMETRY = [ •••• ] [👁] [×]             |
|                      |  [!] 주의: API 키·토큰·LD_PRELOAD/LD_AUDIT/DYLD_INSERT_  |
|                      |      LIBRARIES/DYLD_FORCE_FLAT_NAMESPACE 차단(LD_LIBRARY  |
|                      |      _PATH 허용). 설정값은 호스트 catalog.json에 저장됨.  |
|                      |-----------------------------------------------------------|
|                      | ▾ 머신별 재정의 (host_overrides · 원격 머신 있을 때만)    |
|                      |   대상: [gpu-box v]  argv: [/opt/bin/claude             ] |
|                      |   env: CLAUDE_DISABLE_TELEMETRY [상속 유지|덮어쓰기|제거] |
|                      |-----------------------------------------------------------|
|                      | [!] 오류: 허용되지 않는 셸 연산자 (원문 값 비에코)        |
+----------------------+-----------------------------------------------------------+
| [삭제] -> 인라인: [정말 삭제? [확인] [취소]]                 [취소]      [저장]   |
+----------------------------------------------------------------------------------+

[5b: agents.json 손상 Fail-closed 복구 카드 (저장 비활성화)]
+----------------------------------------------------------------------------------+
| 에이전트 설정 (~/.config/ddg/agents.json)                                    [x] |
+----------------------------------------------------------------------------------+
|   +--------------------------------------------------------------------------+   |
|   | [!] agents.json을 안전하게 읽을 수 없습니다 (자동 덮어쓰기 차단됨).       |   |
|   | 원인: JSON 구문 오류 (라인 14) 또는 스키마 오류 (파일 원문 비에코)       |   |
|   |   [다시 불러오기]   [경로 복사]   [백업 후 초기화]                        |   |
|   |   * 인라인 확인: agents.json.corrupt.<ts> (0o600)로 백업 후 초기화할까요? |   |
|   |     [백업 후 초기화 실행]  [취소]                                        |   |
|   +--------------------------------------------------------------------------+   |
|                                                                       [닫기]     |
+----------------------------------------------------------------------------------+
```

---

## 3. 화면 상태 소유권 · GPUI 0.2.2 구현 전략 · 모듈 구성

### 3.1 원본 소유권 vs 파생값
| 상태 | 원본 소유자 | 저장·수명주기 계약 |
|---|---|---|
| 가시 룸·실행 중 `rm_bg_*` 탭·`TabActivity`·`seen_seq` | 각 `ddg-agents-host` | `MachineView.rooms` + `MachineView.background_tabs: Vec<TabView>`. `StateChanged(!Connected)` 시 휘발 `TabActivity` 초기화 후 재연결 `CatalogSubscribed.activities`로 재병합. 로컬 중복 `seen_marks` 금지(비행 중 중복 발송 방지용 휘발 `pending_seen_seq: u64`만 `TabView`에 보유). |
| 현재 보고 있는 룸 vs Peek 대상 | `Desktop` | `active_host_id`/`active_room_id`와 `peek: Option<PeekState>` 완전 분리. `active_terminal_target()` 통합 조회자로 키/스크롤/리사이즈/보임 판정 단일화. Peek 중 종료 시 `tab.vt.take()`를 `peek.exited_snapshot`으로 이동해 마지막 버퍼 보존. |
| 오버레이 상호 배타성 | `Desktop` | 기존 분산 `Option` 대신 단일 **`active_overlay: Option<ActiveOverlay>`** (`MachineModal` \| `AgentSettingsModal` \| `RenameModal` \| `AgentLauncher`). |
| `agents.json` / `programs.json` | 로컬 디스크 (`~/.config/ddg/`) | `AgentsConfigState::{ Loaded { config, stamp }, Corrupted { path, summary } }`. 모달 초안은 열 때 1회 복사, 저장 시 `flock` 내부 바이트 CAS 검증. `config_watch.rs` 라이브 감시 중 일시 구문 오류는 마지막 유효 스냅샷 유지. |
| `ui.json` (v2 확장) | 로컬 디스크 (`~/.config/ddg/ui.json`) | `UiPrefs { schema_version: 2, ui_scale_pct, agent_sort, pinned_agents, manual_agent_order, collapsed_sections }`. **`ui_scale.rs:108` 및 `config_watch.rs:47,86`의 필드 유실 버그를 in-place 정규화(`prefs.normalized()`)로 동시 수정**. |
| 에이전트 트리·룸/머신 집계 배지·긴급 점프 대상 | **파생값 (비저장)** | `agent_list::build_agent_tree()` / `summarize_room()` 순수 함수로 매 렌더 결정론적 계산. |

### 3.2 GPUI 0.2.2 핵심 제약 대응
1. **아이콘 렌더링 (`native/src/icons.rs`)**:
   - `Application::new().with_assets(icons.asset_source())` 단일 `DdgAssetSource` 등록.
   - 컬러 SVG/PNG는 `img(Resource::Embedded(...))`(GPUI `ImageAssetLoader`가 2.0x + `swap_rgba_pa_to_bgra` 처리), 단색 UI 아이콘은 `svg().path(...)` 틴팅.
   - 로컬 커스텀 아이콘 캐시 무효화: 콘텐츠 해시 키 **`custom/<name>#<fnv64>`** 사용(파일 수정 시 즉시 리로드).
   - 원격 호스트 세션(`!HostTransport::Local`): 내장 아이콘 키 테이블만 매칭, 로컬 `~/.config/ddg/icons/` 절대 미조회.
2. **스피너 8Hz / 0Hz 유휴 게이트**:
   - `with_animation().repeat()` 금지(60/120Hz 배터리 소모).
   - `has_visible_working_spinner(&self) -> bool`: `self.window_active && (에이전트 섹션 펼침 && 연결된 머신에 Working 에이전트 존재 || 활성 룸 탭 바/분할 헤더/Peek에 Working 탭 존재)`일 때만 `Desktop::drain()`에서 125ms(8Hz)마다 `spinner_frame` 증가. 유휴/비활성/단절 시 **0Hz**.
3. **창 활성화 기반 `seen` 동기화**:
   - `cx.observe_window_activation(window, ...)` 구독으로 `Desktop.window_active: bool` 유지.
   - `mark_seen` 트리거 4종: (a) 탭/룸/패인 포커스 변경, (b) Peek 진입, (c) `window_active: false → true` 복귀, (d) `window_active == true` 상태에서 포커스 패인의 `TabActivityChanged` 수신. 분할 패인에서는 포커스 패인(`focused_tab_id`)만 자동 `mark_seen`.

### 3.3 `native/src/` 모듈 구성
- `icons.rs`: `DdgAssetSource`, 내장 SVG(`include_bytes!`), `roxmltree` XML AST 화이트리스트 검증, 단일 fd `O_NOFOLLOW` 로더, 128 KiB / 512px OOM 가드, `custom/<name>#<fnv64>` 해시 캐시, 1회 경고.
- `agent_list.rs`: `AgentSortMode`, `AgentTreeEntry`, 3종 정렬·핀·수동순서(서브트리 단위·자동 `manual` 스냅샷)·집계·긴급 점프 순수 함수.
- `agent_launcher.rs`: 4지선다 팝오버 상태 + 필터 + `Cmd/Ctrl+1..9`.
- `agent_modal.rs`: `AgentSettingsModal` 상태(공용 `ddg-agents-transport::agent`·`split_safe_tokens` 직접 호출, 중복 검증 금지).
- `ui_scale.rs` & `config_watch.rs`: `UiPrefs` v2 확장 + 필드 보존 정규화 + `agents.json`/`programs.json` 감시 연동.
- `host_bridge.rs`: `attach_tab(..., want_control: bool)`, `release_control`, `move_tab`, `mark_seen`, `create_*_with_executor`, `ConnectionState::Connected { host_id, conn_id, features }`.
- `main.rs`: `ActiveOverlay`, `KeyboardFocusZone::{ Terminal, AgentList }`, 2단 사이드바, 22px 패인 헤더, Peek 모드, 단축키 라우팅.

### 3.4 키보드 단축키 및 포커스 존
- **키 이벤트 우선순위**: `1. Zoom` → `2. ActiveOverlay (Modal/Launcher)` → `3. KeyboardFocusZone::AgentList` → `4. 글로벌 단축키` → `5. PeekMode` → `6. TabAction & Terminal`.
- **주요 단축키**:
  - `Cmd/Ctrl+Shift+P`: 통합 에이전트 런처 토글 (`Cmd/Ctrl+K` 삭제)
  - `Cmd+,` (macOS) / `Ctrl+Shift+,` (Linux, GPUI `("<", true)` 폴딩 양쪽 매칭): `AgentSettingsModal`
  - `Cmd/Ctrl+Shift+L`: `AgentList` 포커스 토글(포커스 링 표시, 터미널 커서 숨김; 내부 전용 `↑/↓/←/→/Enter/P/Alt+↑↓/M/Esc`)
  - `Cmd/Ctrl+Shift+A`: 긴급 에이전트 점프(`Blocked > Error > Done(미확인) > Working` 순환)
  - `Cmd/Ctrl+Shift+T` / `Cmd/Ctrl+Shift+N`: 기본 에이전트로 새 탭 / 새 룸 (미설정 시 런처/모달 오픈)
  - `Cmd/Ctrl+1..9` (런처 열림 시): 해당 번호 에이전트 즉시 실행(일반 `1..9`는 필터 입력)
  - Peek 모드: `Readonly`에서 `Esc`/`Cmd+W` = Peek 닫기; `Control`에서 `Esc` = 터미널 전달, `Cmd/Ctrl+W` = Peek 닫기(절대 `close_tab` 금지).
- **알려진 한계**: `AgentSettingsModal` 입력은 기존 `MachineModal`의 단일키 입력기를 재사용하므로 한글 IME 조합·중간 커서 이동은 1차 제외(재검토 조건: 다국어/긴 명령 편집 불편이 실기에서 반복 관측될 때 GPUI 텍스트 입력 위젯 도입).
