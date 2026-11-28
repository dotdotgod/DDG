# 내장 외부 CLI 프리셋 · 실행 계획

**상태: 구현 완료 · Neon 최종 리뷰 통과.** 워크스페이스 테스트 419개 통과·실패 0개·1개 제외. Clippy와 OpenRPC 재검사, macOS arm64 실환경 S1·S2(실창 마우스 `Delete → Confirm Delete → Save`, 재시작 비부활, 마우스 `+ Pi → Save` 포함)·S3·S4·S5 전체 통과. Windows·Linux 데스크톱 실환경 및 Antigravity/OpenCode/Claude Code 실바이너리 기동은 미검증이다.

앱을 처음 열었을 때 **로컬 DDG 터미널에서 실행할 수 있는 CLI**를 일반 프로필로 등록한다. 사람이 일일이 명령을 입력할 필요는 없지만, 무엇을 기본으로 실행할지는 여전히 사람이 정한다. 이 문서는 구현·출시 승인이 아니다. 데이터 보존 반례를 반영한 설계와 검증 항목은 아래 T0 및 `UT-NATIVE-03`에 연결했다.

## 결정 기록

아래 A1–A4는 사용자가 최종 확인한 범위다. 이전 요구사항 브리프의 ‘선택해야만 저장’, ‘빈 런처에서 추천’, ‘CLI 후속’ 제안은 **대체되었다**.

| 결정 | 제품 동작 / 수용 기준 |
|---|---|
| A1 · 5종 | 아래 다섯 외부 실행기만 동봉한다. 탐지 결과에는 내장 ID와 상태만 쓰며 원격에서 받은 명령을 실행하지 않는다. |
| A2 · 시작 시 자동 등록 | 앱 시작 후 로컬 호스트 연결에서 `found`인 미처리 항목만 운영자의 `agents.json` v1에 추가한다. 같은 ID의 사용자 프로필, 다른 커스텀 프로필, 호스트 오버라이드, 기존/해제된 `default_agent_id`는 보존한다. 처리한 ID는 `ui.json`의 `seeded_preset_ids`에 기억해, 사용자가 삭제해도 다음 시작 때 되살리지 않는다. 원격 조회·재조회 자체는 자동 등록하지 않는다. |
| A3 · 화면 구분 | **미등록 후보는 `AgentSettingsModal`에서만** 표시한다. 머신을 골라 발견 항목을 초안에 추가한 뒤 저장하거나 수동 프로필을 만들 수 있다. 런처는 저장된 프로필 전용이므로 시작 시 **저장된** 프리셋이 보이는 것은 정상이다. |
| A4 · CLI도 P0 | `ddg agent presets`는 조회, `ddg agent add --preset <id>`는 한 건 명시 추가, `ddg agent add --detected`는 선택 머신에서 발견된 항목의 명시적 일괄 추가다. `--no-detect` 조회와 `--preset`은 호스트 접속을 요구하지 않는다. `--default`를 단건 추가에 명시한 경우 외에는 기본을 바꾸지 않는다. 기존 `agent ls`/`run`에 탐색이나 자동 등록을 끼워 넣지 않는다. |

| ID | 표시명 | 리터럴 argv | 탐지 이름 | 번들 아이콘 |
|---|---|---|---|---|
| `pi` | Pi | `["pi"]` | `pi` | `pi.svg` |
| `antigravity` | Antigravity | `["agy"]` | `agy` | `antigravity.svg` |
| `codex` | Codex | `["codex"]` | `codex` | `codex.svg` |
| `opencode` | OpenCode | `["opencode"]` | `opencode` | `opencode.svg` |
| `claude-code` | Claude Code | `["claude"]` | `claude` | `claude-code.svg` |

순서는 표시 순서일 뿐이다. 5종의 인식용 원본은 `crates/ddg-agents-transport/src/programs.rs:40–69`, 이미지 키는 `native/src/icons.rs:31–82`. 실제 실행 주체는 Host가 아니라 Holder다(`crates/ddg-terminal-server/src/core.rs:979–988`, `crates/ddg-terminal-server/src/holder.rs:400–402`).

## 실행 인계

| 문서 | 용도 |
|---|---|
| [DESIGN.md](DESIGN.md) | Neon의 UX·데이터·통신 계약: Holder → Core → Host → client, 설정 모달과 CLI |
| [TESTS.md](TESTS.md) | TDD 명세 `UT-*`, 통합/실기 `S1–S5`, 최종 워크스페이스 게이트 |
| [TASKS.md](TASKS.md) | Sol의 작업 순서·파일 소유권·검증 방법. T0의 반례를 설계와 대조하고 T1 → T2 병렬 → T3 병렬 → T4 → T5 순서로 인계 |

**Astra — Lead:** 확정 범위·우선순위·수용 기준 및 작업 분해를 소유한다. **Neon — Design & Review:** 설계/테스트 명세와 T0 반례 처리를 소유하며, 병합 전 계약·보안·UX 일치를 검토한다. **Sol — Implementation:** 별도 착수 승인 후에만 단위 테스트·구현·검증을 수행하고 결과를 보고한다. 이번 계획에서 구현, 테스트 실행, 상위 계획 인덱스 갱신은 하지 않는다.

## 비목표와 한계

- 설치·업데이트·인증·모델·추론 강도·역할/프롬프트 구성·승인 생략 옵션·자동 실행을 제공하지 않는다. `agents.json` 스키마, 셸 생성, 저장된 사용자 명령의 의미는 바꾸지 않는다. §1의 ‘사용자 설정만 실행 구성’은 내장 목록 그 자체를 실행 프로필로 간주하지 않고 **실제 v1 파일에 등록**하는 것으로 유지한다(`docs/spec/requirements.md:7–10,32–35`).
- `found`는 검사 시점에 Holder 환경에서 실행기 이름이 발견되었다는 뜻이지, 공급자 신원·인증·실제 실행 성공을 보장하지 않는다. Finder/서비스/SSH의 PATH 차이, Windows 래퍼뿐인 설치, 구형 구성요소는 별도 상태로 처리한다. 로그인 셸을 실행해 보충하지 않는다. Codex는 플래그를 추측하지 않고 관리 PTY에서 수명을 확인한다.
- **데이터 보존의 한계:** 서로 다른 JSON을 각각 rename해도 두 파일 전체가 원자적이지는 않다. 따라서 `ui.json`의 처리 이력을 락으로 병합·예약한 뒤 `agents.json`에 CAS 추가한다. 예약 뒤 중단되면 프로필이 덜 등록될 수 있으나 삭제된 항목을 묵시적으로 되살리지 않는다. 이때는 모달이나 명시적 CLI 추가로 복구한다. [T0/R1](TASKS.md#t0--착수-전-기술-검수)은 이 순서와 동시 편집에서의 보장 범위를 확인한다.
- 운영자/플러그인/릴레이 **API 경계**와 같은 OS 사용자의 소켓 접근 격리는 구별한다. 구형 호스트·Holder, 응답 지연·누락을 ‘설치 0개’로 표시하지 않는다. 기존 사용자의 `ui.json` 삭제·외부 롤백처럼 이력 자체가 사라진 경우에는 영구 삭제 여부를 추론할 수 없다.
