# 탭 프로세스 이름 & Herdr 표준 아이콘 설계 (`DESIGN.md`)

**작성자:** Antigravity (Gemini 4 Neon — Design & Technical Review Owner)  
**작성일:** 2026-11-28  
**상태:** 설계 확정 (Astra 리드 태스크 분해 → Sol 구현 → Neon 최종 검증 대기)

---

## 1. 목표 및 배경

1. **문제**:
   - 구버전 로컬 `ddg-terminal-server` / `ddg-agents-host` 실행 시 `foreground`·`agent_status`가 `null`로 내려오는데 로컬 머신에는 구버전 안내 배너가 숨겨져 있어(`lacks_activity()`의 `Local` 제외), `codex`·`opencode`·`agy`·`zsh` 탭이 모두 `"Terminal"` + 기본 터미널 아이콘으로 보인다.
   - 최신 데몬에서도 현재 [main.rs](../../../native/src/main.rs)의 `tab_display_name()`이 13개 내장 테이블의 제품명(`"Antigravity"`, `"Pi"`, `"Zsh"`)만 쓰고 미등록 프로세스는 `"Terminal"`로 폴백하며, `proc_info.rs`는 `pi`처럼 `process.title`로 `argv[0]`만 바꾼 Node 프로세스를 식별하지 못한다.
   - 내장 SVG는 13종뿐이며 `antigravity.svg`를 제외한 다수가 임시 단순 선 아이콘이고, 단색 `tint` vs 풀컬러 `img` 렌더링 구분이 없다.
2. **목표**:
   - **상단 탭 텍스트**: 현재 실행 중인 **짧은 프로세스(명령) 이름**(`agy`, `pi`, `zsh`, `codex`, `opencode`, `git`, `htop`, 미등록 명령 포함)을 표시한다.
   - **프로세스 아이콘**: Herdr의 3종 오픈소스 표준(**LobeHub Icons(MIT) + Simple Icons(CC0 1.0) + Lucide(ISC) + oh-my-pi(MIT)**)을 따라 **넓은 내장 카탈로그(Herdr 124종 + DDG 기존 13종 ID 100% 하위호환)**와 단색(`tint: true`) / 풀컬러(`tint: false`) 이중 렌더링을 도입한다.
   - **보안 경계 유지**: 원격/로컬 모두 `ForegroundHint { pid, comm, script_hint? }` 최소 식별자만 전송하며, 원시 `argv` 전체 노출 및 원격 로컬 아이콘 파일 조회는 계속 금지한다.

---

## 2. 데이터 & 서버 계층 설계

### 2.1 `crates/ddg-terminal-server/src/proc_info.rs` — 인터프리터 `argv[0]` 언래핑 보완
- 기존 `ForegroundHint { pid, comm, script_hint }` 와이어 계약은 그대로 유지한다(프로토콜 파괴 변경 없음).
- `interpreter(comm)` 목록을 Herdr 수준으로 확장:
  `node`, `nodejs`, `bun`, `bunx`, `deno`, `python`, `python3`, `npx`, `pnpm`, `yarn`, `uv`, `uvx`, `ruby`, `bundle`, `perl`, `sh`, `bash`, `zsh`, `fish`, `dash`, `tsx`, `env`, `sudo`, `nice`, `time`, `watch`, `nodemon`.
- **`classify_argv(comm, argv)` 보완**:
  1. `comm`이 인터프리터가 아니면 `None` 반환(기존과 동일).
  2. `argv` 첫 원소(`argv[0]`)를 먼저 확인:
     - 경로(`/`, `\`) 마지막 성분 추출.
     - `-`로 시작하는 `argv[0]`은 **실제 로그인 셸이 `comm`과 일치하는 경우(`-zsh` ↔ `zsh`)만 통과시켜 `argv[1]` 검사로 넘기고**, 그 외 플래그 형태(`-bad` 등)는 즉시 중단(`None`)해 절대 `script_hint`로 보고하지 않는다.
     - 대소문자 무관 `.exe` 제거 후 안전성 검사(비어있지 않음, `-` 시작 금지, `=`, `@`, `://`, 공백/제어문자 금지).
     - 정규화된 `argv[0]`가 `comm`과 대소문자 무시 비교로 **다르고**, 동일 런타임 별칭쌍(`node`↔`nodejs`)이 아니면 `Some(sanitized)` 반환.
     - 예: macOS에서 `pi` 실행 시 `comm = "node"`, `argv = ["pi", "", ...]` → `script_hint = Some("pi")`.
  3. 그렇지 않으면 기존대로 `argv[1]`만 검사하고 동일한 안전성 필터(플래그 `-`, `=`, `@`, `://`, 공백/제어문자에서 즉시 중단) 후 `display_basename(arg)` 반환.
  4. **기존 테스트 호환**: `classify_argv("python3", ["python3", "/work/app.py"])` → `Some("app.py")`, `classify_argv("env", ["env", "SECRET=1", ...])` → `None` 등 기존 보안/유닛 테스트는 100% 그대로 통과해야 한다.
- `display_basename(name)`: 기존 `/` 외에 `\` 경로 분리와 선행 로그인 셸 `-` 제거(`-zsh` → `zsh`)를 적용해도 안전한가?
  - `comm` 생성 시 `normalize_comm(name)`에서 선행 `-` 제거(`-zsh` → `zsh`)를 적용해 로그인 셸이 항상 `zsh`/`bash`/`fish`로 정규화되도록 한다.

### 2.2 `crates/ddg-agents-transport/src/programs.rs` — Herdr 124종 확장 카탈로그 & 정규화 헬퍼
- **스키마 하위호환 (`schema_version = 1`)**:
  - `ProgramDefinition`에 모두 `#[serde(default)]` 선택 필드를 추가해 기존 사용자 `~/.config/ddg/programs.json`이 그대로 로드되도록 보장한다:
    ```rust
    pub struct ProgramDefinition {
        pub id: String,
        pub label: String,
        pub icon: String,
        #[serde(default)]
        pub command: Option<String>, // 짧은 기본 명령어명 (예: antigravity -> "agy", claude-code -> "claude")
        #[serde(default)]
        pub tint: bool,              // true = 단색 SVG mask, false = 풀컬러 SVG image
        #[serde(default)]
        pub color: Option<String>,   // 단색 틴트용 #rrggbb (어두운 탭 바 가독성 보장)
        #[serde(default)]
        pub shell: bool,             // 셸 여부 (zsh, bash, fish, sh 등)
        #[serde(default)]
        pub matchers: ProgramMatchers,
    }
    ```
  - `ProgramsConfig::read()` 검증에 추가:
    - `command`: 존재 시 `1..=64`자, `unsafe_text_char` 금지.
    - `color`: 존재 시 정확히 7바이트 `#` + ASCII hex 6자리(`#rrggbb`).
- **내장 카탈로그 (`crates/ddg-agents-transport/src/builtin_programs.json`)**:
  - `include_str!("builtin_programs.json")`으로 로드하고, `ProgramsConfig::default()`가 이를 반환한다(파싱 실패 불가: 컴파일 타임/유닛 테스트 검증).
  - **기존 13종 ID 및 matcher 100% 보존** (`claude-code`, `codex`, `pi`, `gemini`, `antigravity`, `aider`, `opencode`, `cursor`, `nvim`, `cargo`, `zsh`, `bash`, `fish`) — `default-agent-presets` 및 기존 테스트와의 호환성 유지.
    - `antigravity`: `id: "antigravity"`, `label: "Antigravity"`, `command: Some("agy")`, `icon: "antigravity"`, `tint: false`, `matchers: ["antigravity", "agy"]`
    - `claude-code`: `id: "claude-code"`, `label: "Claude Code"`, `command: Some("claude")`, `icon: "claude-code"`, `tint: false`, `matchers: ["claude", "claude-code"]`
    - `codex`: `id: "codex"`, `label: "Codex"`, `command: Some("codex")`, `icon: "codex"`, `tint: false`, `matchers: ["codex"]`
    - `pi`: `id: "pi"`, `label: "Pi"`, `command: Some("pi")`, `icon: "pi"`, `tint: true`, `color: Some("#e89c46")`, `matchers: ["pi"]`
    - `opencode`: `id: "opencode"`, `label: "OpenCode"`, `command: Some("opencode")`, `icon: "opencode"`, `tint: true`, `color: Some("#e6e8eb")`, `matchers: ["opencode"]`
    - `zsh`, `bash`, `fish`: `shell: true`, `tint: true`, `icon: "zsh"`/`"bash"`/`"fish"` (각각 `shell.svg`와 동일한 깔끔한 Lucide 터미널 셸 글리프 또는 고유 셸 컬러 `#9daaff` / `#95c9a9` / `#7cc8e8` 적용), 기타 12종 셸(`sh`, `dash`, `nu`, `pwsh`, `powershell`, `cmd`, `elvish`, `xonsh`, `tcsh`, `csh`, `ksh`, `ion`)은 `id: "shell"`, `shell: true`, `icon: "shell"`, `tint: true`, `color: Some("#4ec95f")`로 매칭.
  - **Herdr 124종 전체 포함**: `amp`, `ansible`, `apt`, `astro`, `biome`, `brew`, `bun`, `caddy`, `cmake`, `command-code`, `composer`, `copilot`, `crush`, `curl`, `dart`, `deno`, `django-admin`, `dnf`, `docker`, `dotnet`, `dsh-tui`, `electron`, `emacs`, `erl`, `esbuild`, `eslint`, `expo`, `fastapi`, `ffmpeg`, `flask`, `flutter`, `fly`, `gcloud`, `gh`, `ghci`, `git`, `glab`, `go`, `goose`, `gradle`, `grok`, `helm`, `hermes`, `htop`, `hx`, `iex`, `java`, `jest`, `julia`, `jupyter`, `kimi`, `kiro`, `kotlin`, `kubectl`, `lazygit`, `less`, `lua`, `make`, `mastracode`, `mongosh`, `muse`, `mvn`, `mysql`, `nano`, `netlify`, `next`, `nginx`, `nix`, `node`, `npm`, `nuxt`, `ocaml`, `ollama`, `omp`, `pacman`, `perl`, `php`, `pig`, `pnpm`, `podman`, `poetry`, `prettier`, `prime-agent`, `prisma`, `psql`, `pytest`, `python`, `qoder`, `r`, `redis-cli`, `rg`, `ruby`, `ruff`, `scala`, `sqlite3`, `ssh`, `supabase`, `swift`, `tauri`, `terraform`, `tmux`, `tsc`, `turbo`, `uv`, `vercel`, `vim`, `vite`, `vitest`, `webpack`, `wrangler`, `yarn`, `zig` + 일반 폴백 `shell`, `program`, `agent`.
  - **명시 키 우선 규칙 (Herdr 표준)**: `find_by_token()` 조회 시 **비-generic 명시 `id` 일치 → 비-generic `matchers` 별칭 일치 → generic 폴백(`agent`, `program`, `shell`)** 순서로 매칭한다. 이에 따라 `agent` 토큰은 `cursor`의 alias(`["cursor", "cursor-agent", "agent"]`)로 계속 매칭되어 기존 호환성이 유지된다.
- **공용 정규화 함수**:
  - `pub fn normalize_process_name(raw: &str) -> Option<String>`:
    - `/` 및 `\` 기준 마지막 성분 추출
    - 선행 `-` 제거 (`-zsh` → `zsh`)
    - ASCII 대소문자 무관 `.exe` 제거 (`codex.exe` → `codex`)
    - 제어문자·양방향 제어문자 제거(`sanitize_display_text` 수준, 최대 64자) 및 trim
  - `pub fn strip_script_extension(name: &str) -> &str`:
    - ASCII 대소문자 무관 `.js`, `.mjs`, `.cjs`, `.ts`, `.mts`, `.cts`, `.py`, `.rb` 접미사 제거(매처 조회 전용, 제거 결과가 비면 원문 유지).
  - `pub fn is_interpreter_name(name: &str) -> bool`:
    - `node`, `nodejs`, `bun`, `bunx`, `deno`, `python`, `python3`, `npx`, `pnpm`, `yarn`, `uv`, `uvx`, `ruby`, `bundle`, `perl`, `tsx`, `env`, `sudo`, `nice`, `time`, `watch`, `nodemon`.

---

## 3. UX & Presentation Layer 설계 (`native/src/icons.rs`, `native/src/main.rs`)

### 3.1 번들 SVG 자산 & 이중 렌더링 (`native/src/icons.rs`)
- **배치 경로**:
  - 모든 내장 프로그램 SVG는 `native/assets/icons/programs/<icon_key>.svg`에 배치한다.
  - 출처: 로컬 `~/.config/herdr-app/icons/*.svg` (LobeHub MIT / Simple Icons CC0 / Lucide ISC / oh-my-pi MIT) + `claude-code.svg`(= `claude.svg`), `aider.svg`(유지), `zsh.svg`/`bash.svg`/`fish.svg`(= `shell.svg` 또는 기존 셸 글리프).
  - **GPUI 0.2.2 SVG 정규화 필수 규칙**:
    - `width="1em"` / `height="1em"` → `width="24" height="24"` 치환 (15개 LobeHub SVG 포함).
    - `kiro.svg`의 `width="1200" height="1200"` → `width="24" height="24"` 치환 (2x 래스터 시 2400×2400 OOM 방지).
    - `tint: true` 단색 SVG의 `currentColor`는 `#000`으로 정규화해 GPUI `svg()` 마스크가 OS/resvg 환경과 무관하게 알파 255로 래스터되도록 보장.
    - 모든 내장 SVG는 `viewBox`를 갖고, `width`/`height`가 1..=64 범위의 숫자여야 하며, 단위 테스트에서 전수 검증한다.
  - **자산 로딩**:
    - `native/build.rs`에서 `native/assets/icons/programs/*.svg`를 스캔해 정렬된 정적 `match` 테이블(`program_asset(key: &str) -> Option<(&'static str, &'static [u8])>`)을 `OUT_DIR/program_assets.rs`로 생성하고 `cargo:rerun-if-changed=assets/icons/programs`를 출력한다.
    - `AppAssets::resolve_icon_key()`의 기존 로컬 커스텀 SVG/PNG 검증(`O_NOFOLLOW`, `roxmltree` 화이트리스트, 128 KiB / 512px 제한, 원격 격리)은 100% 유지한다.
- **이중 렌더링 (`display_agent_icon` in `native/src/main.rs`)**:
  - 아이콘 해석 결과 `ResolvedProgramIcon { key: String, tint: bool, tint_color: u32 }`:
    - **커스텀 로컬 파일(`custom/...`) 또는 `tint == false`** (풀컬러 로고: `antigravity`, `claude-code`, `codex`, `amp`, `crush`, `kimi`, `kiro`, `pig`):
      `img(ImageSource::Resource(Resource::Embedded(path))).size(px(size)).flex_none()`
    - **`tint == true` 또는 폴백 `Icon::Terminal`**:
      `svg().path(path).size(px(size)).flex_none().text_color(rgb(tint_color))`
  - **어두운 탭 바(`#191919` / `#373737`) 대비 보장**:
    - `tint_color`는 `ProgramDefinition.color` 파싱값(없으면 기본 `0xc8ccd0`, 폴백 `Icon::Terminal`은 `0xababab`).
    - Herdr `tile`이 밝은 브랜드 색(`git` `#f03c2e`, `docker` `#2496ed`, `node` `#5fa04e`, `python` `#3776ab`, `nvim` `#57a143`, `rust`/`cargo` `#e57324`, `go` `#00add8` 등)인 항목은 그 브랜드 색(어두운 배경 대비가 충분하도록 보정된 값)을 `color`로 지정하고, `tile`이 어두운 항목(`#1c1d21`)은 밝은 글리프 색(`#e6e8eb` / `#4ec95f` / `#e89c46` 등)을 지정한다.
- **라이선스 준수 (`THIRD_PARTY_NOTICES.txt`)**:
  - Simple Icons (CC0 1.0), Lucide & Feather (ISC / MIT), LobeHub Icons (MIT), oh-my-pi (MIT) 전문/고지 및 상표권 면책을 `THIRD_PARTY_NOTICES.txt`에 추가한다.

### 3.2 상단 탭 이름 & 아이콘 단일 해석 규칙 (`native/src/main.rs`)
이름과 아이콘이 항상 **같은 패인**에서 일관되게 도출되도록 `resolve_tab_identity(&self, machine, tab) -> TabIdentity { name: String, icon_key: String, tint: bool, tint_color: u32 }`로 통합한다.

#### 우선순위 표 (확정)

| 상황 | 탭 텍스트 (`name`) | 아이콘 (`icon_key` / 방식) |
|---|---|---|
| **1. 포그라운드가 셸** (`zsh`, `bash`, `fish`, `sh` 등, `script_hint == None`) | 정규화 셸명 (`zsh`, `bash`, `fish`, `sh` 등) | 매칭된 셸 아이콘 (`zsh`/`bash`/`fish`/`shell`, `tint: true`). **구 `status.app`·`agent_profile` 무시** (에이전트 종료 후 복귀 보장) |
| **2. 포그라운드에 `script_hint` 존재** (예: `node` + `pi`, `python3` + `app.py`) | `normalize_process_name(script_hint)` (단, 매칭 프로그램이 있으면 짧은 명령명 예: `pi`, `vite`; 미등록이면 `app.py` 등 basename) | `script_hint` → `strip_script_extension` 매칭 프로그램 아이콘; 미등록이면 `program`/`terminal` 폴백 |
| **3. 포그라운드가 비-인터프리터 구체 프로세스** (예: `agy`, `codex`, `opencode`, `git`, `nvim`, `htop`, `my-cli`) | `normalize_process_name(comm)` (`agy`, `codex`, `opencode`, `git`, `nvim`, `htop`, `my-cli`) | `comm` 매칭 프로그램 아이콘 (`antigravity`, `codex`, `opencode`, `git`, `nvim`, `htop`); 미등록이면 기본 `terminal` 폴백 (**옛 `argv0`/`agent_profile`로 떨어지지 않음**) |
| **4. 포그라운드가 bare 인터프리터** (`node`, `bun`, `python3` 등, `script_hint == None`) | ① 라이브 `status.app` 있으면 매칭 프로그램의 짧은 `command`(또는 `status.app`)<br>② 없으면 `normalize_process_name(comm)` (`node`, `python3`) | ① 라이브 `status.app` 매칭 아이콘 (예: `pi`, `claude-code`)<br>② 없으면 인터프리터 자체 아이콘 (`node`, `bun`, `python`) |
| **5. 포그라운드 없음** (`foreground == None`: 구버전 holder, Windows, 기동 직후) | ① 라이브 `status.app` → 매칭 프로그램 `command`(없으면 `status.app`)<br>② `tab.argv0` → `normalize_process_name(argv0)` (`agy`, `codex`, `zsh`)<br>③ `tab.agent_profile` → 프로필 `label`(커스텀 프로필) 또는 매칭 `command`<br>④ `tab.title.starts_with("π - ")` → `"pi"`<br>⑤ 최후 폴백 → `"Terminal"` | 동일 순서로 프로그램/프로필 아이콘 매칭 (커스텀 로컬 프로필 아이콘 포함), 없으면 기본 `terminal` |

- **커스텀 프로필 아이콘 예외**:
  - `tab.agent_profile`에 사용자가 명시적 `icon`을 지정한 경우, 포그라운드가 없거나(5번) 현재 포그라운드가 해당 프로필의 실행기/인터프리터와 일치할 때만 프로필 커스텀 아이콘을 우선 적용한다. 사용자가 다른 명령(`zsh`, `htop`, `agy`)으로 전환하면 라이브 프로세스 아이콘을 따른다.
- **에이전트 리스트(`build_agent_rows`) / 패인 헤더 / Peek 배너**:
  - 에이전트 작업 제목(`status.title` / `tab.title`) 표시는 기존대로 유지하고, 아이콘 렌더링만 새 `display_agent_icon` (단색 `tint` vs 풀컬러 `img`)을 공통 사용한다.
- **탭 폭**:
  - 상단 탭 텍스트 `max_w(px(scale.px(95.)))` 유지, 아이콘 크기 `scale.px(12.)` (`13.` in Peek/sidebar).

### 3.3 로컬 구버전 데몬 감지 배너 (`native/src/main.rs`)
- `MachineView::lacks_activity(&self)`에서 `!matches!(self.profile.transport, HostTransport::Local { .. })` 제외 조건을 제거한다.
- `start_machine_install()`은 기존대로 `HostTransport::Ssh | HostTransport::ContainerSsh`만 허용한다(로컬 자동 재시작/세션 강제 종료 방지).
- `old_host_banner(&self, cx)`:
  - `HostTransport::Local`일 때 안내 문구:
    `"Older local daemon: Process names & agent status unavailable · Restart ddg-agents-host and ddg-terminal-server"`
  - 원격 SSH / ContainerSsh / DockerExec 동작 및 버튼은 기존과 100% 동일하게 유지한다.

---

## 4. 테스트 설계 (`TESTS`)

1. **`crates/ddg-terminal-server/src/proc_info.rs`**:
   - `classify_argv("node", [b"pi", b"", b""]) == Some("pi".into())` (macOS `process.title` 덮어쓰기 케이스)
   - `classify_argv("node", [b"/usr/local/bin/pi", b""]) == Some("pi".into())`
   - `classify_argv("node", [b"node", b"--eval=secret"]) == None` (기존 보안 경계 유지)
   - `classify_argv("node", [b"-bad", b"x"]) == None` (플래그 형태 `argv[0]` 차단)
   - `normalize_comm("-zsh") == Some("zsh")`
2. **`crates/ddg-agents-transport/src/programs.rs`**:
   - `ProgramsConfig::default()`가 기존 13종 ID(`claude-code`, `codex`, `pi`, `gemini`, `antigravity`, `aider`, `opencode`, `cursor`, `nvim`, `cargo`, `zsh`, `bash`, `fish`) + Herdr 핵심 확장 항목(`git`, `node`, `python`, `docker`, `htop`, `vim`, `bun`, `go`, `ssh`, `tmux`, `shell`, `program`, `agent` 등 총 120+종)을 모두 포함하며 중복 `id`가 없음.
   - `normalize_process_name`: `"/bin/agy" -> "agy"`, `"-zsh" -> "zsh"`, `"C:\\bin\\codex.exe" -> "codex"`.
   - `strip_script_extension`: `"vite.js" -> "vite"`, `"codex.mjs" -> "codex"`, `"app.py" -> "app"`.
   - `ProgramsConfig::read()`에서 `tint`/`color`/`command`/`shell` 선택 필드 미지정 구형 `programs.json` 역직렬화 성공, 잘못된 `color` (`"red"`, `"#12345"`) 거부 후 기본 폴백.
3. **`native/src/icons.rs`**:
   - `builtin_programs_resolve_on_local_and_remote`: `ProgramsConfig::default()`의 **모든** 프로그램 아이콘이 `Icon::Terminal`이 아닌 번들 SVG로 해석되고, 유효한 XML + `viewBox` + 숫자 `width`/`height <= 64`를 만족함.
   - `antigravity_resvg_rasters_keep_the_mask_and_color`: 이동된 `programs/antigravity.svg`가 48px 및 12..=32px에서 마스크·다색 정상 래스터.
   - GPUI 테스트로 `tint: true` 대표 아이콘(`pi`, `zsh`, `opencode`, `git`, `shell`)과 `tint: false` 대표 아이콘(`antigravity`, `claude-code`, `codex`, `kiro`)이 모두 `resvg`에서 정상 래스터되고 비어있지 않은 알파 픽셀을 생성하며 `kiro`가 2400px로 폭증하지 않음(`<= 128px`).
4. **`native/src/main.rs`**:
   - `top_tab_name_and_icon_follow_profile_and_program_evidence` 갱신 및 확장:
     - `foreground = agy` → name `"agy"`, icon `"antigravity"`, `tint == false`
     - `foreground = { comm: "node", script_hint: Some("pi") }` → name `"pi"`, icon `"pi"`, `tint == true`
     - `foreground = codex` → name `"codex"`, icon `"codex"`, `tint == false`
     - `foreground = opencode` → name `"opencode"`, icon `"opencode"`, `tint == true`
     - `foreground = zsh` (구 `status.app = Some("antigravity")` 잔존 상태에서도!) → name `"zsh"`, icon `"zsh"`, `tint == true`
     - `foreground = htop` → name `"htop"`, icon `"htop"`
     - `foreground = my-unlisted-tool` (이전 `argv0 = Some("/bin/agy")` / `agent_profile = Some("neon")` 존재 시에도!) → name `"my-unlisted-tool"`, icon `""` (기본 terminal 폴백)
     - `foreground = None`, `argv0 = Some("/bin/agy")` → name `"agy"`, icon `"antigravity"`
     - `foreground = None`, `title = "π - legacy holder"` → name `"pi"`, icon `"pi"`
     - `foreground = None`, 임의 OSC title → name `"Terminal"`, icon `""`
     - local + remote(`Ssh`) 양쪽 동일 검증
   - `local_outdated_daemon_shows_banner_without_remote_install`: `HostTransport::Local`에서 `"agent_activity_v1"` 부재 시 `lacks_activity() == true`이고 `start_machine_install`은 차단됨.

---

## 5. 파일 변경 범위 (Sol 소유)

- `crates/ddg-terminal-server/src/proc_info.rs`
- `crates/ddg-agents-transport/src/programs.rs`
- `crates/ddg-agents-transport/src/builtin_programs.json` (신규)
- `native/assets/icons/programs/*.svg` (신규, Herdr 아이콘 정규화 복사본; 기존 `native/assets/icons/` 프로그램 SVG 정리)
- `native/build.rs` (신규 또는 기존 확장 — 확인 후 결정)
- `native/src/icons.rs`
- `native/src/main.rs`
- `THIRD_PARTY_NOTICES.txt`

> **비목표 / 금지사항**:
> - 실행 중인 로컬 `ddg-terminal-server` / `ddg-agents-host` 프로세스를 자동 종료·재기동하지 않는다(사용자 승인 후 별도 수행).
> - `git push` 등 되돌릴 수 없는 작업 금지.
> - `docs/plan/default-agent-presets/` 등 타 계획 문서는 건드리지 않는다.
