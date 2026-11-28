# UX 및 Presentation Layer 설계 — 룸 전환 단축키

- **역할**: `design-conductor` · `ux-designer` · `presentation-layer-designer` (Antigravity Gemini 4 Neon)
- **근거**: 확정 결정 `A1`–`A5`(`A5`: 연결·카탈로그 동기화 완료 머신만 순환), `native/src/main.rs`, `native/src/terminal_input.rs`
- **리뷰 반영**: Phase C–D 독립 리뷰(`UX-1`, `PL-1`–`PL-4`, `DN-1`, `SEC-1`), Phase E 리뷰(`F-C2`, `F-C3`), Astra `validation-requirements` 2차 반영 완료.

## 1. UX 설계 (Phase C)

### 1.1 사용자 흐름
1. **진입**: 활성 DDG 네이티브 창의 루트 포커스가 일반 룸 터미널 존(Control 또는 Readonly)에 있을 때 `⌘+Option+[` / `]`(macOS) 또는 `Ctrl+Alt+[` / `]`(Windows/Linux)를 입력한다.
2. **결과**:
   - 연결되고 최신 카탈로그를 수신한(`is_connected() && catalog_seq.is_some()`) 머신들의 표시 룸(`m.rooms`)을 사이드바 순서대로 평탄화한 목록에서 이전(`[`) 또는 다음(`]`) 룸으로 화면이 전환된다.
   - 마지막 연결 머신의 마지막 룸에서 `]`를 누르면 첫 연결 머신의 첫 룸으로, 반대 방향은 끝으로 순환한다.
   - 접혀 있던 `룸` 섹션(`section:rooms`)이나 대상 머신 그룹(`machine.collapsed`)은 전환과 함께 자동 펼쳐져 새 활성 룸 하이라이트가 드러난다.
   - 대상 룸이 기억하던 선택 탭 그룹·포커스 패널·패널 확대(`zoomed`) 상태는 그대로 유지된다.
3. **다음 행동**: 루트 터미널 존 포커스가 유지되어 즉시 터미널 입력이나 연속 룸/탭 전환을 이어갈 수 있다. 키 반복(`is_held: true`)은 기존 탭 전환과 같이 `KeyDownEvent`당 1단계 이동한다.

### 1.2 상태별 상호작용
| 상태 | 동작 | 사용자 피드백 / 복구 |
|---|---|---|
| 정상 전환 (2개 이상 적격 룸) | 이전/다음 적격 룸으로 전환, 접힌 `section:rooms`·대상 머신 자동 펼침 | 사이드바 활성 강조 이동, 탭 바·본문 갱신, 단축키 유래 PTY 바이트 0 |
| 비연결·재연결 중·보안 차단 머신 존재 | 후보에서 제외하고 다음 적격 머신으로 건너뜀 | 필요 시 사이드바 클릭으로 비연결 상태 화면 열람 가능 |
| 비연결 룸 · 차단 화면(`blocked_msg`) · `ResetNotice` 빈 머신 선택 중 | 현재 머신 위치 기준으로 `]`는 이후 첫 룸이 있는 적격 머신의 첫 룸, `[`는 이전 마지막 룸이 있는 적격 머신의 마지막 룸으로 순환 진입 | 차단 해제·재연결 시도 없이 단축키 한 번으로 정상 룸 탈출 가능 |
| 후보 0개 / 유일 후보 이미 활성 | 무동작(no-op), 키 소비(단축키 유래 PTY 바이트 0) | 접힘·`ui.json`·attach·포커스 변경 없음 |
| 분할 경계 드래그(`pane_drag`) 중 | 무동작(no-op), 키 소비(단축키 유래 PTY 바이트 0) | 드래그 상태 유지 |
| 제외 문맥 (비활성 창·루트 미포커스·AgentList·Peek·모달·런처·유효 `pending_paste`·IME 조합 중) | 룸 전환 미실행, 기존 문맥 키 처리에 위임 | 기존 입력·확인 배너·조합 문자열 무손상 |
| `ui.json` 저장 실패 (`section:rooms` 자동 펼침 시) | 화면 전환·RAM 펼침은 유지, 사이드바 하단 기존 `notice` 배너 표시 | 작업 중단 없음; 다음 기동에만 이전 디스크 선호 적용 가능 |

> 적용 제외 상태: 로딩 스피너·확인 모달·취소 토스트·룸 생성 흐름은 본 요구(`A1`)에 해당하지 않으므로 추가하지 않는다.

## 2. Presentation Layer 설계 (Phase D)

### 2.1 화면 표현 및 전환 ASCII 아트
> 주의: 아래 ASCII 아트는 상태 전이 표현이며, 기존 사이드바의 `Rooms`/머신 헤더 마크업(우측 상태 점·chevron 위치 등)은 변경하지 않는다.

```text
[상태 1: 멀티 머신 전환 — 로컬 룸 B -> 원격(접힘) 룸 C (⌘⌥] / Ctrl+Alt+])]
+-----------------------+----------------------------------------------+
| Agents                | [1: shell] [2: pi*]                   [100%] |
| ...                   +----------------------------------------------+
| Rooms                 | $ cargo test                                 |
|   로컬           ● v  |   (로컬 룸 B · Control 또는 Readonly)        |
|     룸 A              |                                              |
|   > 룸 B   <active>   |   -- 입력: ⌘+Option+] (or Ctrl+Alt+]) -->    |
|   오프라인       ○ v  |   * 오프라인/재연결 중 머신 건너뜀           |
|     캐시 룸 X         |   * 원격 머신 자동 펼침 (collapsed=false)    |
|   원격           ● >  |   * 단축키 유래 양쪽 PTY 바이트: 0           |
+-----------------------+----------------------------------------------+
                              |
                              v
+-----------------------+----------------------------------------------+
| Agents                | [1: logs] [2: server*]                [100%] |
| ...                   +----------------------------------------------+
| Rooms                 | (원격 룸 C 기존 탭 2·분할 포커스·zoomed 유지)|
|   로컬           ● v  |                                              |
|     룸 A / 룸 B       |                                              |
|   오프라인       ○ v  |                                              |
|   원격           ● v  |                                              |
|   > 룸 C   <active>   |                                              |
| [notice: UI 설정 저장 실패 시 사이드바 하단 기존 위치에 표시]        |
+-----------------------+----------------------------------------------+

[상태 2: 단일 머신 평면 목록 (!show_machine_sections) 및 'Rooms' 섹션 접힘 복원]
Before: | Rooms [접힘]       |  -- ⌘⌥] -->  After: | Rooms [펼침]     |
        | (하위 행 미표시)   |                     |   룸 A           |
                                                   | > 룸 B  <active> |

[상태 3: 제외 문맥 (유효 pending_paste / IME 조합 중 / 모달 / AgentList / Peek)]
+----------------------------------------------------------------------+
| [붙여넣기 확인] 여러 줄 입력을 보낼까요?  [붙여넣기] [취소]          |
| 또는 preedit 조합 표시: [한] (terminal_input.marked != "")           |
+----------------------------------------------------------------------+
  * ⌘⌥[ / ⌘⌥] 입력 시 룸 전환 안 함 -> 기존 terminal_key / 문맥으로 위임
```

### 2.2 키 매처·라우팅·상태 소유권 (`native/src/main.rs`)
- **순수 매처**:
  - `enum RoomNavDir { Previous, Next }`
  - `fn room_nav_action(key: &str, m: Modifiers, mac: bool) -> Option<RoomNavDir>`
  - `let primary = m.platform == mac && m.control != mac; if !primary || !m.alt || m.shift || m.function { return None; }` 후 `"["` / `"]"`만 매칭.
- **파생 조회 및 대상 계산** (후보 캐시 필드 미생성):
  - `Desktop::navigable_rooms(&self) -> Vec<RoomKey>`: `self.machines.iter().filter(|m| m.is_connected() && m.catalog_seq.is_some())` 순서로 `m.rooms`를 평탄화.
  - `Desktop::next_room_target(&self, dir: RoomNavDir) -> Option<RoomKey>`:
    1. `rooms.is_empty()`이면 즉시 `None` 반환.
    2. 현재 `(active_host_id, active_room_id)`가 `rooms`의 `idx`에 있으면: `rooms.len() == 1` → `None`, `rooms.len() >= 2` → 순환 이웃(`(idx + len - 1) % len` / `(idx + 1) % len`).
    3. 현재 룸이 `None`이거나 비적격 머신이라 `rooms`에 없으면: `self.machines` 내 `active_host_id` 위치 `mi` 기준으로, `Next`는 `mi` 이후 첫 **룸이 있는** 적격 머신의 첫 룸(없으면 `rooms[0]`), `Previous`는 `mi` 이전 마지막 **룸이 있는** 적격 머신의 마지막 룸(없으면 `rooms[len - 1]`). `mi`가 `None`이면 `Next` → `rooms[0]`, `Previous` → `rooms[len - 1]`.
- **전환 실행 (`Desktop::switch_room(&mut self, dir: RoomNavDir, window: &mut Window, cx: &mut Context<Self>) -> bool`)**:
  1. `self.pane_drag.is_some()`이면 `false`.
  2. `let Some(target) = self.next_room_target(dir) else { return false; };`
  3. `self.active_host_id = target.host_id.clone(); self.active_room_id = Some(target.room_id);`
  4. `self.sync_terminal_interaction(Some(window), cx);` (이전 룸 마우스 해제·IME 엔티티·paste 무효화를 새 attach 전에 확정)
  5. `if self.section_collapsed("section:rooms") { self.toggle_section("section:rooms"); }`
  6. `if let Some(machine) = self.machine_mut(&target.host_id) { machine.collapsed = false; }`
  7. `self.ensure_selected_attached(); self.focus.focus(window); true`
  - `ResetNotice`, `hovered_room`, `pending_select`는 기존 클릭과 동일하게 인위적으로 건드리지 않는다.
- **`Desktop::key` 라우팅 위치** (`KeyRoute::ResetNotice` 처리 직후 · `tab_action` 분기 직전, 현 작업 트리 `main.rs:6569` 기준):
  - 진입 조건: `self.peek.is_none() && self.terminal_has_keyboard() && self.focus.is_focused(window)` && `let Some(dir) = room_nav_action(&key.key, m, mac)` (`main.rs:7301`의 `terminal_shortcut` 패턴 재사용; `input_target().is_some()`은 요구하지 않아 Readonly·빈 머신·비연결 출발 유지).
  - `had_marked_ime` 기록 → `stale_paste = self.sync_terminal_interaction(Some(window), cx)` → `has_marked_text` 확인.
  - `self.pending_paste.is_some() || has_marked_text`이면 `stale_paste` 시 `cx.notify()` 후 `self.terminal_key(event, window, cx)`로 위임.
  - 그 외: `self.switch_room(dir, window, cx)` 실행 후 `cx.stop_propagation()`; 성공 시 `notify(cx, "keyboard")`, no-op 시 `stale_paste || (had_marked_ime && self.terminal_input.is_none())`이면 `cx.notify()`.
- **사용자 문서 갱신**: `native/README.md:32-46` 단축키 표에 이전/다음 룸 행 추가(클릭 대체 경로 안내 유지).
- **API 연결 지점**: 기존 `ensure_selected_attached()` → `HostBridge::attach_tab(..., want_control = true)` 및 `mark_active_seen()` → `HostBridge::mark_seen(...)` 재사용.
