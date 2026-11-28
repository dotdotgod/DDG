# Data Network Layer 및 필수 Validation 설계 — 룸 전환 단축키

- **역할**: `data-network-layer-designer` (Antigravity Gemini 4 Neon) · `validation-requirements` 1·2차(Astra) 반영
- **근거**: 확정 결정 `A1`–`A5`, `native/src/main.rs`, `native/src/ui_scale.rs`, `native/src/terminal_input.rs`, `native/src/host_bridge.rs`, `crates/ddg-agents-transport/src/client.rs`

## 1. Data Network Layer 설계 (Phase D)

### 1.1 신규 DB 스키마 · RPC · 프로토콜 제외 근거
- 본 기능은 클라이언트 창 내부의 **활성 룸 선택 전환(`A1`)**이며, 호스트 카탈로그·세션 소속·룸 순서·PTY 수명·OpenRPC 스키마·CLI 프로토콜을 변경하지 않는다.
- 따라서 **신규 DB 테이블·마이그레이션·OpenRPC 메서드·와이어 필드·서버 측 락/재시도 정책은 모두 적용 제외**한다.

### 1.2 기존 데이터·통신 계약 재사용 명세
| 경로 | 방향 / 소유권 | 재사용 계약 | 변경 여부 |
|---|---|---|---|
| `Desktop.machines` (`Vec<MachineView>`) | 클라이언트 RAM (`hosts.json` 순서) | `m.is_connected() && m.catalog_seq.is_some()` 필터 후 `m.rooms` 순서대로 `RoomKey { host_id, room_id }` 조회 | 읽기 전용 |
| `active_host_id` · `active_room_id` | 클라이언트 창 RAM | 룸 클릭(`main.rs:3457-3466`)과 동일한 복합 키 갱신. 대상 `RoomView`의 `selected_group`·포커스·`zoomed` 미변경 | 갱신 경로 추가 |
| `ui_prefs.collapsed_sections` (`ui.json`) | 로컬 설정 파일 (`~/.config/ddg/ui.json`) | 실제 전환으로 `section:rooms`가 접힘→펼침될 때만 기존 `toggle_section("section:rooms")` → `save_ui_prefs()`(atomic write) 호출 | 기존 경로 재사용 |
| `machine.collapsed` | 클라이언트 RAM (`MachineView`) | 실제 전환 대상 머신만 `false` 설정. 디스크·호스트 미저장 | 기존 경로 재사용 |
| `HostBridge::attach_tab` | 클라이언트 → 호스트 RPC | `ensure_selected_attached()` 경유, 대상 머신 연결 시 미부착 가시 패널만 `want_control = true` attach (서버 `ControlState::on_attach`에서 타 제어자 존재 시 `Readonly` 유지, 강제 claim 없음) | 기존 경로 재사용 |
| `HostBridge::mark_seen` | 클라이언트 → 호스트 RPC | `ensure_selected_attached()` → `mark_active_seen()` 경유, 활성 창·터미널 존일 때 미열람 시퀀스 보고 | 기존 경로 재사용 |

### 1.3 접근 제어·권한 격리 및 리뷰 조정 결정
- **접근 격리**: `m.is_connected() && m.catalog_seq.is_some()` 조건은 `ConnectionState::Connected { .. }`이면서 현재 연결의 초기 `CatalogSubscribed` 스냅샷을 수신한 머신만 허용하므로 `HostKeyMismatch`, `AuthRequired`, `HostNotInstalled`, `ProtocolIncompatible`, `Disconnected`, `Reconnecting`, `Connecting` 및 재연결 직후 stale 캐시 구간을 단일 조건으로 모두 배제한다.
- **리뷰 상충 조정 (`DN-1` vs `validation-requirements` 2차 P2)**:
  - `validation-requirements` 2차는 재연결 중 `Connected + catalog_seq: None` 구간을 허용하되 UI 최종 수신 상태임을 명시하는 안을 제시했고, `data-network-layer-reviewer`(`DN-1`)와 `security-reviewer`(`SEC-2`)는 해당 구간 진입 시 삭제·재시작되었을 수 있는 stale 캐시 탭에 `attach_tab` RPC가 즉시 발송되는 문제를 지적하며 기존 Peek 계약(`main.rs:1860`)과 동일한 `catalog_seq.is_some()` 포함을 권고했다.
  - **조정 결과**: `DN-1` 채택(`m.is_connected() && m.catalog_seq.is_some()`). 새 상태·RPC 없이 기존 Peek 계약을 재사용해 `A5`(연결 끊김/재연결 중 캐시 룸 건너뛰기)와 stale `attach_tab` 차단을 동시에 만족한다.
- **제어권 격리**: 룸 전환은 열람·일반 부착만 수행하며, 실제 PTY 쓰기는 기존 `Desktop::control_target()`(`main.rs`/`terminal_input.rs:21-34`)이 통제한다.

## 2. 필수 Validation 및 신뢰 경계 정책

| 출처 / 검증 위치 | 허용·거부 조건 | 통과 후 보장 / 실패 처리 |
|---|---|---|
| OS/GPUI 키 이벤트 → `Desktop::key` · `room_nav_action` | `peek.is_none() && terminal_has_keyboard() && focus.is_focused(window)`, `Primary+Alt` + `"["`\|`"]"`, `shift=false`, `function=false`, 유효 `pending_paste` 없음, `terminal_input.marked` 비어 있음, `pane_drag` 없음 | 최대 1단계 전환. 허용 문맥의 성공·no-op·드래그는 `stop_propagation()`으로 소비해 **단축키 유래 PTY 바이트 0개**(기존 TUI mouse release 등 정상 상호작용 정리 바이트는 방해하지 않음). 비매칭·제외 문맥은 기존 키 경로로 위임 |
| `hosts.json` → `config_watch.rs` / `ui.json` → `ui_scale.rs` (기존) | `hosts.json` 프로필·빈 목록 검증(`config_watch.rs:36-58`), `ui.json` 일반 파일·64KiB·버전 검증(`ui_scale.rs:135-157`) | 키 처리는 검증 완료된 `self.machines`와 `self.ui_prefs`를 그대로 신뢰 |
| 호스트 카탈로그 → `proto/host.rs` · `transport/client.rs` · `apply_catalog` (기존) | 디스크 도메인 검증(`proto/host.rs:367-405,449-462`), wire 역직렬화(`transport/client.rs:1339-1346`), `generation`·증가 `catalog_seq` 및 `sync_machine_rooms`의 `Hidden` 분리(`m.background`) | 키 처리는 `m.rooms`의 ID·가시성·중복을 재검증하지 않음(내부 코드 신뢰) |
| 현재 뷰 → `navigable_rooms` · `next_room_target` | `m.is_connected() && m.catalog_seq.is_some()` 머신의 `m.rooms`만 후보. 후보 0개 또는 유일 후보 이미 활성이면 `None` | 유효한 단일 `RoomKey` 또는 `None`. 낡은 인덱스·표시 이름·타 머신 동일 룸 ID 오적용 방지 |
| 입력기/PTY → `sync_terminal_interaction` · `TerminalInput` | `active_*` 갱신 직후 `sync_terminal_interaction`으로 이전 `terminal_input`/`terminal_mouse`/`pending_paste` 무효화. 커밋은 `(host, tab, run_generation, channel)` + `entity_id` 일치 시에만 전송 | 이전 룸의 지연 IME 커밋·마우스 드래그·붙여넣기가 새 룸 PTY로 유입되지 않음 |
| 로컬 선호 저장 → 기존 `save_ui_prefs` | `section:rooms`가 실제 전환으로 접힘→펼침일 때만 호출 (`ui_prefs_path == None`이면 RAM 전용 무오류) | 성공 시 atomic 교체. 실제 쓰기 `Err`는 기존 `notice`만 표시하고 전환·RAM 펼침 유지(재시도·롤백 없음) |

### 2.1 동시성·락 및 중복 검증 제외 근거
- **DB 락/트랜잭션 제외**: 호스트 공유 DB를 수정하지 않으며, GPUI 키 이벤트와 브리지 이벤트는 단일 UI 루프에서 순차 적용된다. `ui.json` 저장도 기존 atomic 교체로 충분하다.
- **내부 중복 검증 제외**: 호스트·transport·`sync_machine_rooms` 경계를 통과한 `m.rooms`의 가시성·ID·중복을 키마다 재검사하지 않으며, `m.is_connected()`가 `ConnectionState::Connected`만 허용하므로 `!m.is_security_blocked()`를 중복 검사하지 않는다.

## 3. 적용 범위 · 알려진 한계 · 재검토 조건

- **적용 범위**: GPUI 0.2.2가 `key == "[" | "]"` 논리키를 전달하는 키보드 배치(US/ABC, 한글 두벌식 등) 및 OS/WM이 해당 조합을 선점하지 않는 환경.
- **알려진 한계**:
  1. 독일어 등 `[`/`]` 입력에 AltGr·별도 물리 위치가 필요한 배치에서는 논리키 기준이 물리 위치와 다를 수 있다.
  2. OS/WM·원격 데스크톱이 `⌘+Option+[`/`]` 또는 `Ctrl+Alt+[`/`]`를 선점하면 앱에 키가 도달하지 않는다(사이드바 클릭 대체 경로 유지).
  3. IME 조합 중(`!marked.is_empty()`) `terminal_key`로 위임될 때, 기존 OS별 `terminal_key` 동작(macOS `m.platform` 조기 반환 vs Windows/Linux VT 제어 바이트 전달)이 그대로 유지되며 임의 차단 가드를 추가하지 않는다(`AC4` 준수).
  4. 사이드바 룸 목록이 길어 선택 행이 뷰포트 밖에 있을 때 자동 스크롤은 수행하지 않는다(`room_sidebar`에 `ScrollHandle` 미보유).
- **재검토 조건**:
  - 비US 레이아웃·WM 선점으로 실사용 불가 피드백이 보고되면 커스터마이징 키맵 또는 대체 바인딩을 별도 과제로 검토한다.
  - 오프스크린 하이라이트 확인 불편이 관측되면 `room_sidebar` `ScrollHandle` 도입을 별도 과제로 검토한다.
