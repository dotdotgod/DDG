# T6-A 실환경 통합 시나리오 검증 보고 (`US-03`–`US-06`)

- **검증 일시**: `2026-11-26` | **실행 환경**: macOS arm64 로컬 실바이너리 (`target/debug/{ddg,ddg-agents-host,ddg-terminal-server}`) + Docker Desktop (`ddg-agents-host:test`, `rust:1-bookworm`)
- **비고**: 에이전트 수행 기능·계약·보안 경계 검증 결과이며, 실제 사용자 사용성 평가를 대체하지 않는다. 모든 임시 디렉터리(`/tmp/ddg-t6a-*`)와 테스트 컨테이너(`ddg-us04-*`)는 종료 시 삭제 완료.

## 요약 매트릭스

| 시나리오 | 상태 | 핵심 관측 증거 |
|---|---|---|
| `US-04` (Docker `ContainerSsh` / `DockerExec` 차단) | **통과 (PASS)** | 실 컨테이너(`ddg-agents-host:test`) 4개 조합 모두 exit 1 + 단일 JSON `container_update_unsupported` + 단일 라인 메시지; 컨테이너 내부 바이너리 SHA-256 100% 불변 |
| `US-05` (실 OS 보안 부정 경계 4종) | **통과 (PASS)** | 실 `/usr/bin/codesign -R` ad-hoc 거부(exit 1), 실 PTY→`host-admin.sock` `permission_denied`, `GITHUB_TOKEN` 누출 0건, 헬퍼 심볼릭 TOCTOU 차단·`job.json` 폐기·원본 해시 불변 |
| `US-06` (CLI 에이전트 `--json` · `-H local` · 오프라인) | **통과 (PASS)** | `default_host_id="remote-prod"`에서 `-H` 생략 시 `host == "local"`, TCP trap 접속 0건, `--json` 단일 파싱, `--version`·`--skill` 5개 서브커맨드 확인 |
| `US-03` (원격 Linux SSH 실호스트 무중단 교체·롤백) | **부분/미검증** | Bookworm ELF 빌드·`GLIBC ≤ 2.36` 게이트·로컬 실프로세스 `daemon_handoff`·SSH 스텁(`update_ssh.rs`) 통과; **실 원격 Linux `sshd` 호스트 + `v0.1.0` 실바이너리 업그레이드는 `미검증(원격 호스트 미확보)`** |

## 세부 증거

### 1. `US-04` (라이브 Docker 컨테이너 차단 + 무변경)
- **컨테이너**: `ddg-agents-host:test` 기동 후 `/usr/local/bin/ddg-agents-host` 사전·사후 SHA-256 일치: `4431323d095f052e18b6cf903b395ee27cab332d1f6a2cdf962ee14a107278ff`.
- **실행 결과**:
  - `ddg --json update apply -H ctr-exec --yes` → exit `1`, `error.code == "container_update_unsupported"`
  - `ddg --json update rollback -H ctr-exec --yes` → exit `1`, `error.code == "container_update_unsupported"`
  - `ddg --json update apply -H ctr-ssh --yes` → exit `1`, `error.code == "container_update_unsupported"`
  - `ddg --json update rollback -H ctr-ssh --yes` → exit `1`, `error.code == "container_update_unsupported"`
- **회귀 방지**: 기존 `install_remote_host`의 `ContainerSsh` 부트스트랩은 `ddg-agents-transport::tests::update_ssh`에서 무회귀 통과.

### 2. `US-05` (실 OS 보안 부정 경계)
1. **실 `/usr/bin/codesign` 5요소 `-R` 검증**: ad-hoc 서명 번들(`AdHoc.app`) 대상 leaf OID `1.2.840.113635.100.6.1.13` 포함 5요소 요구식 검증 → exit `1` (fail-closed 거부).
2. **실 프로세스 간 `host-admin.sock` 차단**: 격리 `DDG_RUN_DIR`(`0700`)의 `host-admin.sock`(`0600`)에 실행 중 PTY 자식 프로세스가 `prepare_reload` 요청 → `permission_denied` 응답 및 토큰 문자열 0건 노출.
3. **시크릿 누출 0건**: `GITHUB_TOKEN=ghp_TestSecretToken1234567890` 주입 상태로 오류 경로(`update check` 잘못된 레포+블랙홀 프록시) 실행 후 stdout/stderr/상태 파일 전수 검사 → 누출 `0`건.
4. **헬퍼 TOCTOU 심볼릭 링크 차단**:
   - 심볼릭 `job.json` → 커널 `O_NOFOLLOW` 거부(`io_error`), 원본 `Target.app` SHA-256 불변.
   - 정규 `0600` `job.json` 내 심볼릭 `staging_app_path` → `host_security_blocked`, 일회성 `job.json` 즉시 삭제, 원본 `Target.app` SHA-256 불변.

### 3. `US-06` (CLI 에이전트 오프라인 워크플로)
- `hosts.json`에 `default_host_id: "remote-prod"`(로컬 TCP trap 포트 지정) + `HTTP_PROXY`/`HTTPS_PROXY` 동일 trap 포트 주입.
- `ddg --json update config --auto-check off` → `config.auto_check == "off"`.
- `ddg --json update status` → `host == "local"`, `last_checked_at == null`.
- `ddg --json machine ls`, `ddg --json agent ls` → stderr 공백, 업데이트 문구 0건.
- `ddg --version` → `ddg 0.1.0`; `ddg --skill` → `update status|check|apply|rollback|config` 5종 포함.
- TCP trap accept 카운트: **`0`**.

### 4. `US-03` 잔여 미검증 항목
- 사전 확인 완료(보조 증거): Debian Bookworm arm64 `release-binaries` 빌드 + `scripts/release/check-glibc.sh`(`GLIBC ≤ 2.36`) 통과, `daemon_handoff.rs`(실프로세스 Core Detach·2단계 직접 인계·자동 롤백) 및 `update_ssh.rs`(SSH 스텁·호스트 핀·3종 해시·`GITHUB_TOKEN` 격리) 통과.
- **`미검증`**: 사용자 준비 원격 Linux(`glibc ≤ 2.36`) 실 `sshd` 호스트 및 실제 배포된 `v0.1.0` 레거시 바이너리 최초 1회 업그레이드.
