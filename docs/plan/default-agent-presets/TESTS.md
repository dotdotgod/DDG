# 테스트 설계: 내장 외부 CLI 에이전트 프리셋 (`default-agent-presets`)

- **작성자**: Antigravity (Gemini 4 Neon — Design & Technical Review Owner)
- **연계 설계**: [DESIGN.md](DESIGN.md)

---

## 1. TDD 유닛 테스트 명세

### 1.1 `ddg-agents-transport` (`presets.rs` · `client.rs`)
| ID | 대상 | 검증 내용 |
|---|---|---|
| `UT-PRESET-01` | `BUILTIN_AGENT_PRESETS` 정합성 | 정확히 5종(`pi`, `antigravity`, `codex`, `opencode`, `claude-code`), ID 대소문자 무시 유일·유효 slug, `detect_bin == argv[0]`, 순수 basename 규칙, `ProgramsConfig::default()`의 `(id, label, icon)`과 1:1 일치, 모든 `to_profile()`이 `AgentsConfig::validate()` 통과, 금지 플래그(승인 생략·모델 고정) 부재 |
| `UT-PRESET-02` | `merge_auto_detected_presets` 신규·중복·삭제 보존 | (a) 빈 설정 + `["pi", "codex"]` → 2건 추가, `default_agent_id == None`, `seeded_ids`에 2건 추가; (b) 이미 커스텀 `pi`(별도 `argv`/`env`/`host_overrides`) 존재 → `pi` 미수정, `seeded_ids`에 `pi` 기록; (c) 사용자가 `codex` 삭제 후 재호출(`seeded_ids`에 `codex` 존재) → 재추가 안 됨(zombie 방지); (d) 모르는 ID `"unknown-cli"` 무시; (e) `MAX_AGENT_PROFILES`(128) 도달 시 신규 미등록 항목은 추가·시드 기록하지 않되, **뒤따르는 기존 등록 프리셋 ID는 계속 `seeded_ids`에 기록** |
| `UT-PRESET-03` | `HostClient::detect_agent_presets` 계약·하위 호환 | (a) `hello.features`에 `"agent_presets_v1"` 없음 → RPC 미호출, `Unsupported` 반환; (b) 정상 응답 5종 매핑; (c) 응답에서 일부 ID 누락 → `Unknown` 보완; (d) 응답에 알 수 없는 ID 포함 → 무시; (e) 중복 ID 또는 32건 초과 응답 → `Protocol` 오류 거부 |

### 1.2 `ddg-agents-proto` (`openrpc.rs` · `holder.rs` · `host.rs`)
| ID | 대상 | 검증 내용 |
|---|---|---|
| `UT-PROTO-01` | 직렬화·엄격 파싱 | `HostAgentPresetsDetectParams` 및 `PtyProbeBinsParams`에 임의 키(`{"bins":["sh"]}`) 전달 시 `#[serde(deny_unknown_fields)]` 거부; `HolderProbeBins`/`HolderProbeBinsResult` 프레임 인코딩·디코딩 왕복; `PresetDetectStatus` snake_case(`found`/`not_found`/`unknown`); `BUILTIN_AGENT_PRESET_BINS` 5종 상수 검증 |
| `UT-PROTO-02` | OpenRPC 스냅샷 동기화 | `schema/ddg-agents-host.openrpc.json`에 `host.agent_presets_detect`, `schema/ddg-terminal-server.openrpc.json`에 `pty.probe_bins` 반영 및 기존 스냅샷 테스트 통과 |

### 1.3 `ddg-terminal-server` (`holder.rs` · `core.rs`)
| ID | 대상 | 검증 내용 |
|---|---|---|
| `UT-TERM-01` | Holder OS 비실행 PATH 탐지 (Unix) | 임시 디렉터리 PATH에서: (a) 실행 시 마커 파일을 생성하는 가짜 실행 스크립트 `pi` → `Found`이면서 **마커 파일 미생성**(절대 실행 안 함 증명); (b) 권한 `0o644` 비실행 파일 → `NotFound`; (c) 소유자/유효 권한 없는 실행 비트(`access(X_OK)` 실패) → `NotFound`; (d) 동명 디렉터리 → `NotFound`; (e) 깨진 symlink → `NotFound`; (f) 유효 symlink → `Found`; (g) 상대 PATH 요소(`.`) 포함 시 미발견 항목은 `Unknown`(확정 `NotFound` 아님); (h) `PATH` 미설정/빈 문자열 → `Unknown` |
| `UT-TERM-02` | `pty.probe_bins` 구형 Holder·취소 정리 | Holder가 `"probe_bins_v1"` 미광고 시 5종 모두 `Unknown` 반환; 타임아웃 또는 호출자 Future 취소 시 `pending_probes` 대기자 맵 엔트리 즉시 제거(누수 없음); Holder 동기 PTY 입력 루프가 probe I/O로 차단되지 않음 |
| `UT-TERM-03` | Holder ↔ Core 통합 왕복 | 실제 Holder + Core 연결에서 `pty.probe_bins {}` 호출 시 Holder의 실제 PATH 기준으로 5종 `Found`/`NotFound` 정확 반환 |

### 1.4 `ddg-agents-host` (`server.rs` · `host_service.rs`)
| ID | 대상 | 검증 내용 |
|---|---|---|
| `UT-HOST-01` | `hello` 기능 광고 및 `host.agent_presets_detect` | `HostHelloResult.features`에 `"agent_presets_v1"` 포함; 운영자 연결에서 `host.agent_presets_detect {}`가 고정 5종 결과 반환; `term()` 미가용 시 5종 모두 `Unknown` 반환(`NotFound` 아님) |
| `UT-HOST-02` | 보안 경계 | 비어 있지 않은 파라미터 거부; `plugin_host` / 세션 토큰 호출 경로에서 `host.agent_presets_detect` 호출 불가 확인 |

### 1.5 `ddg-agents-cli` (`args.rs` · `agent.rs`)
| ID | 대상 | 검증 내용 |
|---|---|---|
| `UT-CLI-01` | `ddg agent presets` 파싱·출력 | `--no-detect --json`으로 5종 정적 출력(`registered` 정확 반영); 호스트 연결 시 `found`/`not_found`/`unknown` 출력; 구버전 호스트(`agent_presets_v1` 없음)에서 `detection_supported: false` 정상 출력; 기존 `ddg agent ls --json` 출력 바이트/스키마 100% 무변경 |
| `UT-CLI-02` | `ddg agent add --preset` | `--preset pi` 성공 + v1 저장(`default_agent_id` 미변경); `--preset pi --default --label "My Pi"` 오버라이드 반영; 없는 프리셋 → `preset_not_found`; 이미 등록된 ID → `agent_exists`(기존 값 무변경); `--preset pi -- echo hi` 상호 배타 usage 오류 |
| `UT-CLI-03` | `ddg agent add --detected` | `found` 항목만 일괄 추가, 이미 등록된 항목 `skipped`(기존 값 무변경), `default_agent_id` 미변경; 추가 0건 시 `agents.json` 미생성/원본 바이트 보존; 미지원 호스트 시 `presets_unavailable` 오류 및 파일 미수정 |

### 1.6 `native` (`ui_scale.rs` · `config_watch.rs` · `agent_modal.rs` · `host_bridge.rs`)
| ID | 대상 | 검증 내용 |
|---|---|---|
| `UT-NATIVE-01` | `UiPrefs` `seeded_preset_ids` 하위 호환 | `seeded_preset_ids` 키가 없는 기존 v2 `ui.json` 정상 로드(빈 집합); 저장 후 라운드트립 보존; `config_watch` 리로드 시 필드 보존 |
| `UT-NATIVE-02` | `AgentSettingsModal` 탐지 상태·`add_preset` | 프리셋 추가 시 초안 추가·선택·`Field::Command` 포커스, `default_agent_id` 미변경, 기존 `modal.host` 오버라이드 상태 미간섭; `commit_fields()` 실패 중 호출 시 안전 중단; 이미 존재하는 ID 중복 추가 방지; `Save` 전 취소 시 파일 미변경; **5종 모두 `Unknown` 응답은 `Unavailable`로 표시(`No presets found`로 오보 금지)**; 일부 `Found` + 일부 `Unknown`은 `Found` 표시 + 불완전 안내 |
| `UT-NATIVE-03` | 시작 자동 시드·실패 주입 보호 불변식 | (a) 파일 없음 + 발견 2종 → `ui.json` 예약 후 `agents.json` 2종 저장 + `default_agent_id == None`; (b) 손상된 `agents.json` → 자동 시드 중단, 원본 무변경; (c) **손상된 `ui.json` → 빈 집합 폴백 금지, 자동 시드 중단(fail-closed)**; (d) `ui.json` 예약 실패 주입 → `agents.json` 미생성/미수정; (e) **`ui.json` 예약 + `agents.json` 저장 직후 프로세스 종료 시뮬레이션 → CLI로 해당 프로필 삭제 → GUI 재시드 시 부활하지 않음**; (f) 다른 창/UI 설정 저장과 병행 시 `seeded_preset_ids` 비파괴 합집합 보존; (g) stale `(conn_id, boot_generation)` 응답 폐기 |

---

## 2. 통합 · 실환경 검증 시나리오

> **격리 필수 조건**: `S1`–`S5` 모든 실환경 검증은 `DDG_CONFIG_DIR`, `DDG_RUN_DIR`, `DDG_STATE_DIR`, `DDG_PLUGINS_DIR`를 모두 별도 임시 디렉터리로 격리해 실행하며, 사용자의 기존 라이브 `ddg-agents-host` / `ddg-terminal-server` / `holder`를 절대 건드리지 않는다.

| 시나리오 | 검증 절차 및 합격 기준 |
|---|---|
| `S1` 로컬 클린 부팅 자동 시드 | 완전 격리된 4개 디렉터리(`CONFIG`/`RUN`/`STATE`/`PLUGINS`)에서 앱 실행 → 로컬 호스트 연결 시 이 Mac에 설치된 프리셋(`pi`, `codex`, `antigravity` 등 Holder PATH 기준 발견분)이 `agents.json`에 자동 추가되고 `default_agent_id`는 `null` 유지. 런처에 즉시 표시됨. |
| `S2` 삭제 후 비부활 & 설정 모달 재추가 | `S1` 이후 설정 모달에서 1개 프로필(예: `codex`) 삭제 후 `Save` → 앱 재실행 시 `codex`가 다시 추가되지 않음 확인. 설정 모달 `Detected` 섹션에서 `[+ Codex]` 클릭 → 초안 확인 후 `Save` 시 정상 재등록 확인. |
| `S3` 멀티호스트 분리 탐지 | 로컬(A)과 상이한 PATH를 가진 원격/Docker 호스트(B) 연결 → 설정 모달에서 탐지 대상 머신을 A ↔ B 전환할 때 각 머신에 실제 존재하는 프리셋만 표시되고 서로 섞이지 않음. B의 지연 응답 중 A로 전환 시 B 응답 폐기 확인. |
| `S4` CLI P0 워크플로우 | 완전 격리된 4개 디렉터리에서 `ddg agent presets --json` → `ddg agent add --preset pi --json` → `ddg agent add --detected --json` → `ddg agent ls --json` → `ddg run pi` 실행 및 종료 확인. |
| `S5` Codex/Pi 실기 PTY 확인 | 완전 격리된 환경에서 `ddg run pi` 및 `ddg run codex`를 DDG 관리 PTY에서 실행해 정상 기동·입력·종료 확인(미실행 항목은 `Unverified` 명시). |

---

## 3. 자동 검증 명령 게이트
구현 완료 시 아래 명령 전체 통과를 필수로 한다:
```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
(OpenRPC 변경 시 `crates/ddg-agents-proto` 스냅샷 테스트 포함)
