# T0-B 스파이크 보고: 패키징 · macOS `.app` 서명/공증 · Debian Bookworm(`glibc ≤ 2.36`) · `.gitignore`

- **조사 일시**: 2026-11-26 | **상태**: 완료 (읽기 전용 조사, `.git`/프로덕션 파일 무변경)
- **확인 소스**: `Cargo.toml`, `assets/ddg-icon.png`, `THIRD_PARTY_NOTICES.txt`, `.gitignore`, `docker/Dockerfile`, `crates/ddg-agents-transport/src/{dial.rs,install.rs}`, `crates/ddg-agents-host/src/{paths.rs,connect.rs}`, `crates/ddg-terminal-server/src/holder.rs`, `native/src/{main.rs,icons.rs}`, `native/target/Ddg.app/Contents/Info.plist`
- **로컬 환경 실측**: macOS 26.6.2 arm64, `/usr/bin/{codesign,iconutil,sips,ditto}`, `/usr/sbin/spctl`, Xcode `notarytool`·`stapler` 존재 확인, `.git/` 미초기화 확인

## 1. macOS `.app` 번들 규격 · 서명/공증 · Ed25519 분리 서명

### 1.1 바이너리 배치 및 `current_exe()` sibling 정합성
- 산출물 4종: `ddg-agents-native`, `ddg`, `ddg-agents-host`, `ddg-terminal-server`.
- `dial.rs`, `host/paths.rs`, `install.rs`, `host/connect.rs`, `holder.rs` 모두 `current_exe().parent()` 기준 sibling 탐색 → 4종을 `Contents/MacOS/`에 평면 동봉 시 앱/CLI 어느 쪽에서 기동해도 정합 (`~/.local/bin/ddg` 심볼릭 링크는 번들 내부 실경로로 해석).
- **`otool -L` / `LC_BUILD_VERSION` 실측**: 비시스템 dylib 0건 (`minos 11.0`). UI 에셋은 `include_bytes!` 내장. Apache-2.0 준수를 위해 `THIRD_PARTY_NOTICES.txt`를 `Contents/Resources/` 및 Linux/Windows 아카이브 루트에 동봉.

```text
DDG Agents.app/Contents/
├── Info.plist
├── MacOS/{ddg-agents-native, ddg, ddg-agents-host, ddg-terminal-server} (0755)
└── Resources/{AppIcon.icns, THIRD_PARTY_NOTICES.txt}
```

### 1.2 `Info.plist` · 아이콘 변환 · `entitlements.plist`
- **`Info.plist`**: `CFBundleExecutable=ddg-agents-native`, `CFBundleIdentifier=<BUNDLE_ID>`, `CFBundleName=DDG Agents`, `CFBundleDisplayName=DDG Agents`, `CFBundleShortVersionString=<X.Y.Z>`, `CFBundleVersion=<X.Y.Z>`, `CFBundlePackageType=APPL`, `CFBundleIconFile=AppIcon`, `LSMinimumSystemVersion=11.0`, `NSHighResolutionCapable=true`, `LSApplicationCategoryType=public.app-category.developer-tools`.
- **`sips`/`iconutil`**: `assets/ddg-icon.png` 실측 `512×512` RGBA PNG → `AppIcon.iconset/` 10종(`16`..`512@2x`[1024 업스케일]) 생성 후 `iconutil -c icns`.
- **`entitlements.plist`**: Hardened Runtime 최소 권한 빈 `<dict/>` (`app-sandbox` 미설정 — PTY/SSH/소켓/`~/.config` 접근 필수, 비시스템 dylib 0건이므로 JIT/DYLD 예외 불필요).

### 1.3 Inside-Out 서명 → 공증 → 스테이플 → 배포 ZIP 순서
1. 임시 키체인 생성(`openssl rand -hex 24`) 및 `.p12` 임포트.
2. **Inside-out 개별 서명**(`--deep` 서명 금지): `Contents/MacOS/{ddg,ddg-agents-host,ddg-terminal-server,ddg-agents-native}` 각각 서명 후 마지막에 `DDG Agents.app` 서명(`--force --options runtime --timestamp --entitlements assets/macos/entitlements.plist`), `codesign --verify --deep --strict --verbose=2` 즉시 검증.
3. 공증: `ditto -c -k --keepParent --norsrc "DDG Agents.app" notarize.zip` → `xcrun notarytool submit ... --wait`.
4. 스테이플·Gatekeeper(CI 전용): `xcrun stapler staple` → `xcrun stapler validate` → `/usr/sbin/spctl --assess --type execute --verbose=4`.
5. **최종 배포 ZIP 재생성**(staple 이후, `--norsrc`로 `__MACOSX`/`._*` 차단): `ditto -c -k --keepParent --norsrc "DDG Agents.app" "ddg-agents-<ver>-darwin-arm64.app.zip"`.
6. **클라이언트/헬퍼 5요소 `codesign -R` 검증(SSOT)**:
```sh
/usr/bin/codesign --verify --deep --strict \
  -R 'anchor apple generic and identifier "<EXPECTED_BUNDLE_ID>" and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = "<EXPECTED_APPLE_TEAM_ID>"' \
  "<app>"
```
7. **Ed25519 분리 서명(`manifest.json.sig`)**: 1차 `const RELEASE_PUBKEYS = &[];`(SHA-256 모드); 2차 전환 시 CI 비공백 키 가드 후 `manifest.json` 원문 바이트 서명 첨부.

## 2. Linux Debian Bookworm (`glibc ≤ 2.36`) 빌드 전략
- **실측**: `debian:bookworm-slim` `ldd 2.36`, 기존 `ddg-agents-host:test` 이미지 심볼 최대 `GLIBC_2.34`/`GLIBC_2.36`.
- **Docker 타깃**: 기존 `--target binaries`(2종, `install.rs` 호환) 유지 + `ddg` CLI 포함 3종(`ddg`, `ddg-agents-host`, `ddg-terminal-server`)을 export하는 `release-binaries` 스테이지 추가.
- **러너**: 권장 `ubuntu-24.04`(x86_64) + `ubuntu-24.04-arm`(aarch64) 네이티브 buildx (대안: `ubuntu-24.04`에서 기존 `$BUILDPLATFORM` 크로스 빌드).
- **`GLIBC_2.37+` 0건 게이트**:
```sh
for bin in ddg ddg-agents-host ddg-terminal-server; do
  { objdump -T "$bin"; readelf -V "$bin"; } | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -u \
    | awk -F'[_.]' '($2 > 2) || ($2 == 2 && $3 > 36) { print; bad=1 } END { exit bad }'
done
```

## 3. 필요 GitHub Secrets 이름표 (`T7-A` `GATE-03`) 및 `.gitignore` 점검
- **1차 필수 Secrets**: `APPLE_CERTIFICATE_P12_BASE64`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, `APPLE_NOTARY_KEY_P8_BASE64`, `APPLE_NOTARY_KEY_ID`, `APPLE_NOTARY_ISSUER_ID` (+ 자동 `GITHUB_TOKEN`).
- **2차 (`GATE-02`)**: `RELEASE_ED25519_PRIVATE_KEY`.
- **`.gitignore` 보강 (`T5-B`, `GATE-01` 전 적용)**: 기존 4줄(`/target/`, `/native/target/`, `/.dotdotgod/`, `.DS_Store`)에 `*.p12`, `*.p8`, `*.cer`, `*.pem`, `*.key`, `*.keychain*`, `*.sig`, `*.asc`, `.env`, `.env.*`, `!.env.example`, `/dist/`, `*.app/`, `*.icns`, `*.iconset/`, `notarize*.zip`, `SHA256SUMS`, `._*`, `*.tmp`, `*.log` 추가 권장.

## 4. 미확인 항목 (`T7-A` / `T6-B` 게이트)
1. 공식 Apple Team ID 및 최종 `CFBundleIdentifier` (프로토타입 `dev.dotdotgod.agents` 임시 사용 중).
2. 공식 Public GitHub 저장소 `owner/repo` (`GATE-01`).
3. 실서명·공증·Hardened Runtime 빈 `entitlements.plist` 하 GPUI/Metal/PTY 실동작(`GATE-04` + `US-01`).
4. 2차 Ed25519 운영 키쌍(`GATE-02`) 및 1024px 원본 아이콘(현재 512px 업스케일).
