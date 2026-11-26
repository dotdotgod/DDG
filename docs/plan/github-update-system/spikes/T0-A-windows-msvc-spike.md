# T0-A 스파이크 보고: Windows `x86_64-windows-msvc` 링크 · `windows-sys` · GPUI

- **조사 일시**: 2026-11-26 | **상태**: 정적 조사 완료 (Windows 실링크 검증은 `windows-latest` CI 게이트 필요)
- **확인 소스**: `Cargo.lock`, `native/Cargo.toml`, `gpui-0.2.2/build.rs`, `crates/ddg-agents-vt/{build.rs,src/ffi.rs}`, `docker/Dockerfile`, `crates/ddg-agents-proto/src/id.rs`, `crates/ddg-terminal-server/src/pty.rs`, `crates/ddg-agents-host/src/paths.rs`, `crates/ddg-agents-cli/src/attach.rs`

## 1. 확정 근거 (소스·락파일 직접 확인)
- **`windows-sys = "0.61.2"` 기존 잠금 완료**: `tokio 1.54.1`·`mio`·`socket2`·`notify`·`tempfile` 등이 이미 `0.61.2`를 사용 → 워크스페이스에 `windows-sys = "0.61"` 추가 시 신규 메이저 중복 0건.
- **`gpui = "=0.2.2"` Windows `--release` 필수 도구**: `gpui-0.2.2/build.rs`가 `--release`에서 `fxc.exe`(`GPUI_FXC_PATH` 또는 Windows SDK `x64` PATH)와 `rc.exe`(`windows-manifest`)를 호출하며, 미탐지 시 `panic!("Failed to find fxc.exe")`. `windows-latest` CI에 `ilammy/msvc-dev-cmd` + `GPUI_FXC_PATH` 주입 필수.
- **`tokio::net::windows::named_pipe::ServerOptions` 내장 지원**: `first_pipe_instance(bool)`, `reject_remote_clients(bool)`, `FILE_FLAG_OVERLAPPED`, `create_with_security_attributes_raw(addr, attrs)` 직접 제공.
- **Ghostty VT 1.0.1 + Zig 0.17.1 (`docker/Dockerfile`)**:
  - `libghostty-vt-1.0.1.tar.gz` SHA256 `333dfce97641ef9b536bb9e08907ad86abaaada50e8f7417b70d9284adef37aa`
  - `libghostty-vt-1.0.1-deps.tar.gz` SHA256 `91a4e10960a52bf5bf4b15aa2a2cddaff8a89d6cd903d3055bda4fd17de3356d`
  - `ffi.rs`는 순수 C ABI(`extern "C"`), POSIX 전용 타입 없음.

## 2. 결론 및 권고안

### 2.1 Ghostty VT Windows MSVC 빌드 · CRT 정합 · `build.rs`
- **아카이브명**: Ghostty `build.zig`는 Windows에서 DLL import lib(`ghostty-vt.lib`)과 구분하기 위해 정적 라이브러리를 `ghostty-vt-static.lib`로 출력한다. 공식 프리빌드(`x86_64-windows-gnu`)는 MSVC `link.exe`와 `LNK1143` 충돌 위험이 있으므로 Zig 0.17.1로 직접 빌드한다:
  ```powershell
  zig build -Dtarget=x86_64-windows-msvc -Demit-lib-vt -Demit-exe=false -Doptimize=ReleaseFast -Dcpu=baseline --system <deps_dir> --prefix <out>
  ```
- **CRT 정합**: Rust `x86_64-pc-windows-msvc` 및 GPUI 모두 동적 CRT(`/MD`) 사용 → `+crt-static` 미사용. C++ SIMD(`simdutf`/Highway)에서 `/MT` 충돌 관측 시 `-Dsimd=false` 적용.
- **`crates/ddg-agents-vt/build.rs`**: `CARGO_CFG_TARGET_OS`/`CARGO_CFG_TARGET_ENV` 런타임 분기로 전환. Windows에서 `GHOSTTY_VT_LINK_MODE=dylib`이면 `dylib=ghostty-vt`, 기본 정적 모드는 `ghostty-vt-static.lib` 우선(`static=ghostty-vt-static`), 없으면 `static=ghostty-vt` + `msvc` 필요 시스템 라이브러리(`ntdll`, 필요 시 스파이크 로그 기준 최소 추가).

### 2.2 `windows-sys = "0.61"` 크레이트별 최소 Feature 목록
| 크레이트 | `windows-sys = "0.61"` 최소 features |
|---|---|
| `ddg-agents-proto` | `Win32_Foundation`, `Win32_Security_Cryptography` (`BCryptGenRandom`) |
| `ddg-agents-os` | `Win32_Foundation`, `Win32_Security`, `Win32_Security_Authorization`, `Win32_Storage_FileSystem`, `Win32_System_IO`, `Win32_System_Memory`, `Win32_System_Pipes`, `Win32_System_SystemServices`, `Win32_System_Threading` |
| `ddg-terminal-server` | `Win32_Foundation`, `Win32_Security`, `Win32_System_Console`, `Win32_System_JobObjects`, `Win32_System_Pipes`, `Win32_System_Threading` |
| `ddg-agents-cli` | `Win32_Foundation`, `Win32_System_Console` |

*(호스트와 `native/src/ui_scale.rs`는 `ddg-agents-os`만 소비.)*

## 3. 로컬 macOS 미확인 항목 (`windows-latest` 게이트) 및 대안
- **미검증 4건**: ① Zig `x86_64-windows-msvc` 실제 출력명·미해결 심볼, ② `/MD` vs `/MT` 및 `simdutf` C++ 런타임 정합, ③ `gpui = "=0.2.2"` `--release`(`ilammy/msvc-dev-cmd` + `GPUI_FXC_PATH`) 및 `--check-vt` 기동, ④ Named Pipe SID DACL + `GetNamedPipeClientProcessId` + ConPTY/JobObject 실기 테스트(`UT-OS-01..03`, `UT-HOST-03`).
- **실패 시 대안**:
  1. **옵션 A(1순위)**: Zig `-Dsimd=false` 또는 누락 시스템 lib 추가(`rust-lld` 검토).
  2. **옵션 B(2순위)**: `GHOSTTY_VT_LINK_MODE=dylib` 전환 + Windows 배포 `.zip`에 `ghostty-vt.dll` 동봉(`.old.<nonce>` 스왑 대상 포함).
  3. **옵션 C(3순위)**: `gpui = "=0.2.2"` Windows 빌드 불가 시 1차는 CLI+데몬 3종 배포, `native` Windows는 GPUI 업그레이드 후 이관(사용자 재결정).
