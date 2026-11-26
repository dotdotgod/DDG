#!/usr/bin/env bash
set -euo pipefail

[[ $(uname -m) == arm64 ]] || { echo 'Expected a macOS arm64 runner' >&2; exit 1; }
vt=$(mktemp -d "${RUNNER_TEMP:?}/ghostty-vt.XXXXXX")
curl -fsSL --retry 3 https://release.files.ghostty.org/libghostty-vt/1.0.1/ghostty-vt-1.0.1.xcframework.zip -o "$vt/vt.zip"
printf '%s  %s\n' c0f9631277f726154c8f521954d17cdc41aac4626fc0df8cf925a19ee58e829b "$vt/vt.zip" | shasum -a 256 -c -
unzip -q "$vt/vt.zip" -d "$vt"
lib="$vt/ghostty-vt.xcframework/macos-arm64_x86_64"
test -f "$lib/libghostty-vt.a"
printf 'GHOSTTY_VT_LIB_DIR=%s\nMACOSX_DEPLOYMENT_TARGET=11.0\n' "$lib" >> "${GITHUB_ENV:?}"
if ! xcrun -sdk macosx metal --version; then
    xcodebuild -downloadComponent MetalToolchain
    xcrun -sdk macosx metal --version
fi
