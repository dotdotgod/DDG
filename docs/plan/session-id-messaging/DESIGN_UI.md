# UX · Presentation Layer 설계

[계획 목차](README.md) · [데이터·네트워크 설계](DESIGN_DATA.md) · [테스트 설계](TESTS.md)

## 1. UX 설계 원칙 (확정 조합: 1B · 2A · 3A · Q1–Q6)
- **숨김 백그라운드 룸 비노출 (`3A`)**: `visibility == Hidden` 룸은 GUI 사이드바·탭바 어디에도 렌더하지 않는다. 에이전트가 `host.session_move`로 가시 룸에 승격할 때만 GUI에 노출된다.
- **기존 룸/탭/머신 UX 보존**: 멀티 머신 섹션(2대 이상), 단일 머신 플랫 리스트, 드래그 파일 첨부, Add Machine 모달 동작은 v2 문자열 ID로 무중단 유지한다.
- **v1 리셋 단일 비파괴 배너 (`Q3`)**: 서버 `CatalogSubscribeResult.reset_notice`만 단일 출처로 사용(`HostHelloResult`에서 제거). 백업 파일은 basename(`catalog.json.v1.bak.<ts>`)만 표시하며, 사용자가 첫 가시 룸을 생성하거나 `×`로 닫기 전까지 유지한다.
- **외부 입력 표시 (`Q1`/`Q6`)**: 에이전트가 활성 탭에 `write_input`을 주입해도 GUI `ControlMode::Control`을 빼앗지 않는다. 활성 탭·일치 `run_generation`에 한해 상단 3초 배지(`외부 입력` 또는 `원격 입력 · <label>`)만 표시하고 원문 텍스트·원시 `ses_*` ID는 노출하지 않는다.
- **에이전트/브리지 CLI UX (`2A`)**: 6개 환경변수(`DDG_SESSION_ID`, `DDG_ROOM_ID`, `DDG_HOST_ID`, `DDG_HOST_SOCK`, `DDG_TERMINAL_SOCK`, `DDG_SESSION_TOKEN`) 자동 주입. `ddg-agents-host` 서브커맨드는 기본 JSON stdout, 오류 stderr+비0 종료코드, 빈 `msg recv`는 `exit 0` + `[]`.

## 2. Presentation Layer ASCII 아트 (3종)

### 2-1. 멀티 머신 사이드바 + v1 리셋 배너 + 활성 탭 외부 입력 배지
```text
+------------------------------------------------------------------------------+
| [사이드바 220px]         | [탭바: 활성 룸 "api-work"]                         |
| + 머신 추가              | [1 bash x] [2 claude x] [+]    [원격 입력 · build] |
|--------------------------+---------------------------------------------------|
| v ● local (현재 선택) +  | +-----------------------------------------------+ |
|   > api-work             | | i 기존 v1 카탈로그를 백업했습니다             | |
|     docs-review          | |   (catalog.json.v1.bak.1795580000)         [x]| |
| > ● build-mac         +  | +-----------------------------------------------+ |
| > ○ prod-box          +  | $ cargo test --workspace                          |
|                          | ... (GUI ControlMode::Control 유지, 키 입력 가능) |
| (rm_bg_* 숨김 룸 미표시) |                                                   |
+--------------------------+---------------------------------------------------+
```

### 2-2. 숨김 세션 승격(`host.session_move`) 0→1 가시 룸 전이
```text
[승격 전: build-mac 가시 룸 0개, 접힘]    [승격 후: NewRoom("agent-run") 도착]
+------------------------+                +------------------------+
| > ● build-mac        + |   session_move | v ● build-mac        + |
|   (가시 룸 없음)       |  ===========>  |     agent-run          |
|                        |  collapsed=false |   [1 worker x]       |
+------------------------+                +------------------------+
* 규칙: active_room_id.is_none()일 때만 자동 선택; 다른 룸 작업 중이면 포커스 유지.
```

### 2-3. 빈 머신 헤더 클릭 → v1 리셋 배너 도달 + `tab_restart` 단일 재어태치
```text
[빈 머신 클릭 시]                       [tab_restart (1B, run_generation 1 -> 2)]
+------------------------+              1. GUI -> host.tab_restart(room_id, tab_id)
| v ● local (선택됨)   + |              2. Server -> CatalogUpdated(gen=2)
|   (룸 없음)            |              3. sync_machine_rooms():
+------------------------+                 - 구 channel_id -> detach_channel()
| [i v1 백업 안내 ... x] |                 - Vt 스크린/상태 리셋, tab.attaching=true
| [+ 새 룸 만들기]       |                 - 단일 host.tab_attach() 요청
+------------------------+              4. TabAttached(gen=2) 수신 -> 스트림 재개
                                           (gen < tab.run_generation 패킷은 드롭)
```

## 3. 상태 소유권 및 불변식
| 상태 필드 | 소유 위치 | 불변식 및 갱신 규칙 |
|---|---|---|
| `rooms: Vec<RoomView>` | `MachineView` | `visibility == Visible`만 보관(`Hidden` 즉시 필터링); ID는 `String` |
| `collapsed: bool` | `MachineView` | `session_move`로 가시 룸 `0 -> 1` 전이 시 `false`로 자동 펼침 |
| `reset_notice: Option<ResetNotice>` | `MachineView` | 호스트별 독립 유지; `dismissed_notice_ids: HashSet<String>` 또는 첫 가시 룸 생성 시 해제 |
| `run_generation: u64`, `attaching: bool` | `TabView` | `run_generation` 증가 시 구 채널 `detach_channel` + `Vt` 리셋; `TabAttachFailed` 시 `attaching=false` |
| `external_input_indicator: Option<ExternalInputIndicator>` | `AppState` | 활성 `(machine_idx, room_id, tab_id, run_generation)` 일치 시에만 갱신; 주입 시계 기준 `3.000s` 만료 |
| `connected_host_id: Option<String>` | `MachineView` | `HostHelloResult.host_id` 저장; 원격 입력 배지 라벨 매핑(`expected_host_id` 우선, 없으면 `connected_host_id`) |

## 4. GPUI `=0.2.2` 검증 제약 및 `HostBridge` 계약
- **검증된 GPUI API**:
  - `ElementId`: 문자열 ID 충돌 방지를 위해 `stable_elem_id(kind: &'static str, parts: &[&str]) -> ElementId::NamedInteger(kind.into(), fnv1a_64_delimited(parts))` 사용(구분자 `0xFF`로 경계 충돌 방지).
  - 레이아웃: 배지 우측 정렬은 `.ml_auto().flex_shrink_0()` 사용(`shrink_0()` 미존재); 머신 헤더 `+` 버튼은 `overflow_hidden` 바깥의 `flex_none` 슬롯에 배치해 긴 라벨에서도 클릭 가능 유지.
  - 제어문자/Bidi 방어: `room.name`, `tab.title`, 머신 라벨, `backup_file` 모두 `ddg_agents_proto::id::sanitize_display_text` 통과 후 렌더.
- **`HostBridge` 변경 계약 (`native/src/host_bridge.rs`)**:
  - 모든 `RoomId`/`TabId` 인자 `u64 -> String` 전환.
  - `HostBridge::restart_tab`: **중복 `tab_attach` 호출 제거**(단일 경로: `CatalogUpdated`의 `sync_machine_rooms`가 `run_generation` 변화를 감지해 1회 재어태치).
  - 신규 `BridgeEvent`: `TabAttachFailed { room_id, tab_id, message }`, `ExternalInput { session_id, pty_id, run_generation, byte_len, from_host_id, is_remote }`, `SessionExited { session_id, room_id, exit_code }`, `CatalogSubscribed { catalog, reset_notice }`.
  - `TabAttached` 이벤트는 `TabStreamHandle.run_generation`을 전달하며, `handle.run_generation < tab.run_generation`이면 즉시 구 채널 detach 후 폐기한다.
  - `sync_machine_rooms`는 기존 `native/src/main.rs:2423` 패턴과 동일하게 GPUI `Window` 없이 동작하는 순수 함수로 유지하여 `Vec<u32>`(detach할 구 채널)와 상태 전이 결과를 반환한다.
