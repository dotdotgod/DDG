# 구현 인계 · 내장 CLI 프리셋

**미실행 계획.** [범위·A1–A4](README.md) / [설계](DESIGN.md) / [테스트 원문](TESTS.md). Sol은 사용자 착수 승인 전 소스·스냅샷을 변경하지 않는다. `UT-*`는 **예정된 테스트 ID**다. 각 작업은 실패하는 단위 테스트 → 최소 구현 → 해당 크레이트 회귀 → Neon 검수 순서. 실환경 S1–S5는 유닛 TDD와 별도로 실제 결과를 기록한다.

| 순서 | 선행 | 병렬 가능 / 인계물 |
|---|---|---|
| T0 기준선·반례 검수 | 착수 승인 | Astra/Sol 기준선, Neon의 보존·경계 판정. 다른 세션 편집분은 건드리지 않는다. |
| T1 wire 계약 | T0 | **단독**. proto 타입·feature·빈 요청 및 두 OpenRPC 스냅샷 동결. |
| T2-A 터미널 ∥ T2-B transport | T1 | A는 `ddg-terminal-server`만, B는 `ddg-agents-transport`만 소유. T1 파일 변경 필요 시 직렬로 T1 재개. |
| T3-A host ∥ T3-B CLI | T2-A/B 완료 | A는 `ddg-agents-host`만, B는 `ddg-agents-cli`만 소유. B는 mock/격리 연결로 검증, 실제 전 구간은 T5. |
| T4 native | T3-A/B | 시드 이력·CAS·UI 상태 통합. `native/src/main.rs` 한 명 소유, 다른 기능과 같은 파일 동시 편집 금지. |
| T5 통합·실기·제품 문서 | T4 | 실제 버전 조합·두 파일 보존·종료 동작과 회귀 확인. 실행 보고와 설계 완료를 구분. |

동일 병렬 그룹은 **쓰기 파일 교집합이 없다**. 테스트는 원칙적으로 소유 파일의 기존 `#[cfg(test)]` 또는 아래 명시한 해당 크레이트 테스트에 둔다. 전역 fmt·스키마 재생성·Cargo.lock 일괄 변경을 병렬 작업에서 돌리지 않는다. 아래에서 **신설**이라고 표기한 항목만 현재 없는 파일/심볼이다.

## T0 · 착수 전 기술 검수

**유형:** 조사/리뷰. 기존 `git status --short`와 관련 diff를 보존하고 `native/src/main.rs`, `native/src/ui_scale.rs`, `native/src/host_bridge.rs`, 터미널 `core.rs`/`holder.rs` 등 타 작업과 겹치는 수정 범위·작업 트리를 먼저 조율한다. Neon의 수정 설계/테스트에 반영된 아래 조건을 작업별로 인수한다. 추가로 명시가 필요한 CLI 일괄 결과·충돌 재시도는 구현 전 Neon이 확인한다. 검수 항목을 실행 결과 ‘통과’로 바꾸지 않는다.

| 게이트 | 반례와 통과 조건 |
|---|---|
| R1 · 두 파일의 실패·동시성 | `agents.json` 저장 성공 → `ui.json` 저장 실패/프로세스 종료 → CLI로 해당 프로필 삭제 → 앱 재시작. **이 경우도 삭제된 자동 시드가 되살아나면 실패**. 단지 다음 시작에 같은 프로필이 있으면 중복되지 않는다는 테스트로 대체하지 않는다. `UiPrefs::save`는 파일 하나의 전체 교체이며 기존 scale/sort 저장자도 있다(`native/src/ui_scale.rs:130–156,159–200`, `native/src/main.rs:1459,2824–2829`); ‘최신 값 읽고 합집합 후 쓰기’만으로 stale writer를 막을 수 없다. 모든 지원 writer의 이력 보존, 손상 prefs fail-closed, 두 파일 사이 실패 주입을 확인한다. **채택 설계:** 모든 `UiPrefs` 저장자가 같은 락으로 read–modify–write하고 시드 이력을 합집합으로 보존한다. 이력을 먼저 지속(예약)하고 그 뒤 프로필을 CAS 추가. 중간 실패로 **덜** 등록된 경우 명시 추가는 가능해야 한다. 락 안에서 새로 예약한 ID만 이번 호출 소유로 인정한다. 충돌 재시도가 다른 호출의 예약 또는 이미 관측했던 동시 삭제를 복구하지 않는지 검수한다. GUI 사용 전 CLI 추가→삭제처럼 이력 자체가 없던 경우, 또는 사용자가 이력 파일을 지운 경우의 보장 범위는 별도로 명시한다. |
| R2 · 판정·표시 | 구형 호스트(feature 없음) / 신형 호스트+구형 Holder(전부 unknown) / 전부 not_found / 부분 실패를 분리. `Found == 0 && Unknown > 0`은 빈 설치 목록이 아니다. CLI 일괄 추가도 ‘판정 불가’와 ‘확정 0개’를 구분하는 결과/오류를 정의하고 둘 다 파일은 그대로 둔다. 용량 127/128, 다섯 건 일괄 추가 중 제한 도달, 기본값 변경 없음, 같은 ID/다른 argv를 포함해 처리·스킵의 의미를 고정한다. |
| R3 · 접근·취소 | operator RPC/허용된 relay/플러그인 reverse API를 구분한다. API 비노출이 동일 사용자의 OS 소켓 격리라는 주장은 하지 않는다(`crates/ddg-agents-host/src/server.rs:179–186,225–248,558–581`; `crates/ddg-terminal-server/src/server.rs:105–106,124–138`). Host와 terminal의 탐지 요청 모두 **빈 객체**만 받고 Holder도 5개 고정 이름만 조사한다. 동기 Holder 명령 루프에서 파일 I/O 금지. 타임아웃·늦은 응답·호출자 취소에서 모든 계층의 대기자를 정리한다. |

**완료:** 구현자가 고정 명령·응답 예·지원 표시/승인 기준을 설명할 수 있고 각 반례의 담당 태스크가 정해져 있다. T1–T5의 파일 목록은 이 기능의 소유 범위이지 사용자/다른 세션 변경을 교체할 권한이 아니다.

## T1 · proto + OpenRPC

- **쓰기 범위:** `crates/ddg-agents-proto/src/lib.rs`, `crates/ddg-agents-proto/src/holder.rs`, `crates/ddg-agents-proto/src/terminal.rs`, `crates/ddg-agents-proto/src/host.rs`, `crates/ddg-agents-proto/src/openrpc.rs`, `schema/ddg-terminal-server.openrpc.json`, `schema/ddg-agents-host.openrpc.json`. 기존 앵커: `HolderWelcome`, `encode_frame`, `HostHelloResult`, `terminal_server_openrpc`, `agents_host_openrpc`, `openrpc_schemas_are_valid_and_up_to_date`.
- **신설:** proto 공유 `BUILTIN_AGENT_PRESET_BINS`, `QUERY_BINS`/`BINS_RESULT`(현행 미사용 9/109), 상관 ID와 결과 payload, `pty.probe_bins {}` / `host.agent_presets_detect {}` 및 `found/not_found/unknown`. `HolderWelcome.features`/`HostHelloResult.features` 기존 확장점 사용. 기존 실행 타입·프로토콜 major는 그대로 둔다. 요청에 `bins`, 경로, 명령 또는 기타 키가 들어오면 거부한다.
- **TDD:** `UT-PROTO-01/02`. 고정 5개, enum 직렬화, 빈 요청에서 임의 키 거부, 왕복·기존 hello 역호환, 양쪽 OpenRPC 메서드/스냅샷 확인.
- **완료:** `DDG_UPDATE_SCHEMA=1 cargo test -p ddg-agents-proto openrpc_schemas_are_valid_and_up_to_date`로 두 **ddg- 접두어가 있는** 파일만 재생성한 뒤 환경변수 없이 `cargo test -p ddg-agents-proto` 통과. T2 이후는 이 계약을 소비한다.

## T2-A · Holder 환경에서 조회, Core에서 전달

- **쓰기 범위:** `crates/ddg-terminal-server/src/holder.rs` (`HolderWelcome` 구성, `commands`), `crates/ddg-terminal-server/src/core.rs` (`Core`, `recover_ptys`, `handle_holder_frame`), `crates/ddg-terminal-server/src/server.rs` (`dispatch_json`), `crates/ddg-terminal-server/tests/integration.rs`. **참조만:** `crates/ddg-terminal-server/src/pty.rs`, `crates/ddg-terminal-server/src/pty_windows.rs` — 현행 실행 의미를 바꾸지 않는다.
- **신설 동작:** `probe_bins_v1`를 기존 Holder 기능에 **추가**. Holder 자신의 PATH에서만 5개 basename을 파일 메타데이터·실행 권한으로 검사; 셸/후보 실행, 반환 경로·env 없음. Unix는 현 사용자 실행 가능 일반 파일/유효 링크, Windows는 실제 직접 실행 가능한 `.exe` 기준, shim만 있거나 판정 불가 시 unknown. 상대 PATH 등 검사에서 제외한 요소 때문에 단정할 수 없으면 unknown. 구형 Holder에는 새 프레임 전송 금지.
- Core가 ID별 응답을 매핑한다. 한 호스트의 느린 파일 조회가 기존 PTY input/resize를 막지 않게 하고, 워커·대기자 수를 제한한다. 3초 기한/끊김/취소/세대 교체에서 pending 정리; 늦은 응답을 다른 요청이나 PTY로 전달하지 않는다.
- **TDD/통합:** `UT-TERM-01/02`, `UT-TERM-03` 실연결 왕복. fake 실행 파일의 마커 **미생성**, 권한/디렉터리/링크/상대·빈 PATH, Windows shim, 지연 중 PTY 입력, 구형 Holder 무전송·unknown를 확인한다. **완료:** `cargo test -p ddg-terminal-server` 통과 및 동시 probe로 PTY가 생성되거나 기존 명령 루프가 정지하지 않음.

## T2-B · transport 카탈로그·병합·클라이언트

- **쓰기 범위:** `crates/ddg-agents-transport/src/presets.rs` **신설**, `crates/ddg-agents-transport/src/lib.rs`, `crates/ddg-agents-transport/src/client.rs`. **참조:** `crates/ddg-agents-transport/src/programs.rs` (`ProgramsConfig`), `crates/ddg-agents-transport/src/agent.rs` (`AgentProfile`, `AgentsConfig`, `ConfigStamp`, `MAX_AGENT_PROFILES`). 이들 사용자 파일 형식은 변경하지 않는다.
- **신설:** `BUILTIN_AGENT_PRESETS`, `to_profile`, `merge_auto_detected_presets`, `HostClient::detect_agent_presets`. UI와 CLI가 같은 표를 사용하되, GUI의 자동 시드 기억과 CLI의 **명시적 재추가**를 동일 정책으로 취급하지 않는다. 128개 한도에서는 새 추가만 제한한다. **기존 프로필인데 아직 시드 이력에 없는 ID까지 놓치지 않도록** 용량 검사 때문에 뒤 항목 전체를 중단하는 반례를 테스트한다. CAS 실패 후 미커밋 config·seeded 집합을 성공 결과처럼 재사용하지 않는다.
- feature 확인과 RPC는 같은 활성 연결에서 한다(`crates/ddg-agents-transport/src/client.rs:454–475,825–881`의 `call_on` 경로). 미지원은 무호출, 단절/timeout은 미지원과 구별. 4초 deadline, 최대 32개, 중복 거부·미지 ID 무시·누락 unknown, 원격 argv 사용 금지.
- **TDD:** `UT-PRESET-01/02/03`. 정확한 5종·순수 argv와 proto/아이콘 표 정합, 기존 커스텀 값과 `default_agent_id` 보존, 삭제 억제 및 부분 용량, 응답 검증·구형 peer 무전송. **완료:** `cargo test -p ddg-agents-transport` 통과. T3에서 호출할 typed API/오류가 확정됨.

## T3-A · 운영자 Host RPC

- **쓰기 범위:** `crates/ddg-agents-host/src/server.rs` (`dispatch_rpc`), `crates/ddg-agents-host/src/host_service.rs` (`HostService`, `term`), `crates/ddg-agents-host/src/term_conn.rs` (`TermConn`, `call_with_stream`), `crates/ddg-agents-host/src/host_service/tests.rs`. 경계 검증 위치: `crates/ddg-agents-host/src/hub.rs` (`allowed`), `crates/ddg-agents-host/src/plugin_host.rs` (`handle_plugin_reverse_call`) — 테스트만, 허용 메서드 추가 금지.
- `hello`에 `agent_presets_v1` 광고, operator만 새 RPC dispatch. `pty.probe_bins {}` 결과를 고정 bin→preset ID로 변환한다. `term()` 없음/구형 Core method-not-found/구형 Holder/timeout은 다섯 unknown. 정상이더라도 경로·버전·자식 env를 반환하지 않는다. 토큰이 붙은 요청·이미 토큰 경로로 쓰인 연결 및 relay/reverse API 관련 검증은 R3에서 승인된 경계로 시험한다.
- Host→Core deadline을 **pending을 소유하는 곳**에서 적용하거나 drop-safe cleanup을 보장한다. 현재 TermConn의 기본 15초 및 제거 지점은 `crates/ddg-agents-host/src/term_conn.rs:276–330`; 바깥 3초 timeout만 추가해 내부 정리가 우회되지 않게 한다.
- **TDD:** `UT-HOST-01/02` + timeout 후 pending 잔여 없음. **완료:** `cargo test -p ddg-agents-host` 통과, 구형 구성요소에서도 기존 hello/세션 실행 회귀 없음.

## T3-B · CLI 조회·단건/일괄 추가

- **쓰기 범위:** `crates/ddg-agents-cli/src/args.rs` (`Action`, `parse`, `HELP`), `crates/ddg-agents-cli/src/main.rs` (동기 `agent::run` dispatch, `selected`, `connect`), `crates/ddg-agents-cli/src/agent.rs` (`change`, `run`), `crates/ddg-agents-cli/src/SKILL.md`. **참조:** `crates/ddg-agents-cli/src/run.rs`만. 이후 T5가 `crates/ddg-agents-cli/tests/cli_e2e.rs`를 소유한다.
- 기존 `AgentAdd | AgentSet`은 위치 ID 1개, 기존 add는 `-- argv` 필수(`args.rs:316–332,445–463,512–515`). `set`을 느슨하게 만들지 말고 **수동 add / `--preset` / `--detected`** 세 형식을 상호 배타로 파싱한다. 새 모드에서 지원하지 않는 기존 override/unset 옵션을 무시하지 말고 usage 오류. 새 조회·탐지만 비동기 연결 사용, `--no-detect`/단건/기존 ls는 오프라인으로 동작. 글로벌 `-H/--host`, `--config`, `--json`과 기존 기본 머신 선택을 따른다.
- **TDD:** `UT-CLI-01/02/03`. 텍스트·JSON, 정적/미지원/단절, 중복/없는 ID, 금지 옵션 조합, explicit `--default`, 한 번의 CAS, 추가 0건·충돌 때 파일 바이트/부재 보존. 유효한 기존 default가 있는 경우와 없는 경우 모두 시험한다. `--detected`는 의도적 재추가이므로 UI 시드 이력에 의존하지 않는다.
- **완료:** `cargo test -p ddg-agents-cli` 통과, help/내장 skill에 세 명령과 삭제·기본값 동작 설명. `agent ls --json` 계약·수동 add/set와 `ddg run`의 기존 오류/실행 변함없음.

## T4 · 네이티브 시작 시드·설정 모달

- **쓰기 범위:** `native/src/ui_scale.rs` (`UiPrefs::load`, `UiPrefs::save`), `native/src/config_watch.rs` (`ConfigReloadState`, `load_agents_snapshot`), `native/src/host_bridge.rs` (`HostBridge`, `BridgeEvent`, `is_current`), `native/src/agent_modal.rs` (`AgentSettingsModal`, `commit_fields`, `delete`, `save`), `native/src/main.rs` (`apply_bridge_message`, `open_agent_settings`, `agent_settings_view`, `save_ui_prefs`). 기존 `native/src/agent_launcher.rs`, `native/src/layout.rs`, 아이콘 SVG는 수정하지 않는다.
- **상태:** 전용 탐지 머신/요청 ID와 `modal.host`(오버라이드 편집) 분리. startup과 modal refresh가 한 RPC를 공유해도 startup 결과는 한 번 처리하고 모달 간 stale 결과는 폐기. 연결 이벤트의 `ConnectionState::Connected`에는 `boot_generation`이 없다(`crates/ddg-agents-transport/src/client.rs:47–54`). 현재 host profile 키·bridge generation·같은 연결의 hello(`crates/ddg-agents-proto/src/host.rs:465–472`)를 바인딩. 다른 세대, 닫았다 다시 연 모달, 지연 응답 시험. 원격 자동 시드 금지.
- **저장:** R1의 write-ahead 예약 절차를 구현. legacy v2 prefs의 누락 필드만 빈 이력으로 취급, 손상과 파일 부재 구별. 모든 prefs 저장/리로드가 이력을 보존. 기본값·커스텀 env·호스트별 값·사용자가 편집 중인 모달 초안 보존. `agents.json`은 기존 validate/lock/CAS, 중복 저장 금지. 최초 충돌은 최신 상태로 재병합, 재충돌·읽기/권한 실패는 중단. 파일·락 대기는 UI 입력 루프 밖. 명시적 삭제/재추가와 시작 자동 시드는 별개이며 **취소한 모달 편집은 파일에 쓰지 않는다**.
- **표시:** 모달에서만 `Detected`/머신/refresh·Found/Added 및 R2 상태 표시. 추가하면 현재 입력 확정 후 초안 선택·명령 포커스, Save 전에는 프로필 파일 불변. 키보드·포커스·저장/취소 내비게이션 검증. 자동 등록 완료 후 런처에는 **저장 행만** 보인다.
- **TDD:** `UT-NATIVE-01/02/03` + R1 중간 실패→삭제→재시작, 늦은 prefs writer, startup+refresh 경합, R2 부분 성공. **완료:** `cargo test -p ddg-agents-native` 통과 및 손상 파일/다른 편집자/사용자 default를 덮지 않음.

## T5 · 통합·실환경 검증 및 인수

- **쓰기 범위:** 필요한 통합 회귀는 `crates/ddg-agents-cli/tests/cli_e2e.rs`의 기존 `Sandbox`에 둔다. 동작 설명은 `docs/spec/requirements.md` §1과 `native/README.md`에 맞춘다. 새 범용 E2E 프레임워크나 제품 데이터 마이그레이션은 없다.
- **격리:** `crates/ddg-agents-cli/tests/cli_e2e.rs:47–71`처럼 `DDG_CONFIG_DIR`, `DDG_RUN_DIR`, `DDG_STATE_DIR`, `DDG_PLUGINS_DIR` 모두 별도 임시 경로, 해당 빌드의 host/terminal 바이너리 및 새 Holder 사용. 실사용 소켓·프로필·PTY 재사용/재시작 금지. 특정 CLI 설치를 가정하지 말고 fixture PATH와 실제 사용 환경을 구분한다.
- **S1–S2:** 빈 상태에서 로컬 Found분만 최초 등록, `agent ls --json`상 기본 `null`(디스크에서는 선택 키가 생략될 수 있음). 삭제→Save→재시작 비부활, 모달에서 다시 추가. 기본값 지정/해제·기존 ID 커스텀 값·동시 CLI 편집·실패 주입 R1 확인.
- **S3:** 로컬·원격/컨테이너 PATH를 의도적으로 다르게 구성. 전환/재연결/구형 호스트·Holder에서 stale/unknown 표시와 원격 자동 쓰기 금지 확인. 실제 장비가 없으면 해당 조합은 `Unverified`.
- **S4–S5:** S1의 시드된 설정을 재사용하지 않고 독립 CLI fixture에서 세 명령과 `ddg run pi` 검증. 관리 PTY에서 Pi·Codex 입력, detach/reattach, 종료 및 자식의 세션 귀속 확인; 다른 3종도 출시 지원을 주장하는 OS에서 시도 여부와 결과를 분리 기록. 로그에 환경값·세션 토큰·사용자 PATH를 싣지 않는다. Codex 수명 문제가 있으면 승인 생략/daemon 옵션을 자동 추가하지 않고 결과를 Neon에 반환한다.
- **최종 게이트:** `cargo test --workspace` 및 `cargo clippy --workspace --all-targets -- -D warnings`, OpenRPC 스냅샷 비갱신 재검사. native/Ghostty 의존성을 갖춘 환경에서 전체를 실행한다. Linux CI의 native 제외 테스트(`.github/workflows/ci.yml:90–103`)만으로 전체 통과라고 보고하지 않는다. 지원 플랫폼별 `통과 / 실패 / 미검증`, 미검증 사유, 양쪽 사용자 파일의 전후 결과, 각 `UT-*`/`S*` 증거를 별개로 인계한다. 실패·미검증을 조용히 출시 승인으로 대체하지 않는다.
