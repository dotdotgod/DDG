#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
usage() {
    echo "Usage: $0 --bin-dir DIR --version VERSION --out-dir DIR [--bundle-id ID] [--platform darwin-arm64|darwin-x86_64] (--sign | --unsigned-dry-run)" >&2
    exit 2
}

bin_dir= version= out_dir= mode= platform=darwin-arm64 bundle_id=com.dotdotgod.agents
while (($#)); do
    case "$1" in
        --platform) [[ $# -ge 2 ]] || usage; platform=$2; shift ;;
        --bin-dir) bin_dir=${2:?}; shift ;;
        --version) version=${2:?}; shift ;;
        --out-dir) out_dir=${2:?}; shift ;;
        --bundle-id) bundle_id=${2:?}; shift ;;
        --sign|--unsigned-dry-run) [[ -z $mode ]] || usage; mode=$1 ;;
        *) usage ;;
    esac
    shift
done
[[ -n $bin_dir && -n $out_dir && -n $mode ]] || usage
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?(\+[a-zA-Z0-9.-]+)?$ && $version != *..* ]] || usage
[[ $bundle_id =~ ^[a-zA-Z0-9.-]+$ ]] || usage

case "$platform" in
    darwin-arm64) expected_arch=arm64 ;;
    darwin-x86_64) expected_arch=x86_64 ;;
    *) usage ;;
esac

mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)
staging=$(mktemp -d "$out_dir/.macos.XXXXXX")
trap 'rm -rf "$staging"' EXIT
app="$staging/DDG Agents.app"
plist="$root/assets/macos/Info.plist.template"
binaries=(ddg ddg-agents-host ddg-terminal-server ddg-agents-native)
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$staging/AppIcon.iconset"
for bin in "${binaries[@]}"; do
    install -m 755 "$bin_dir/$bin" "$app/Contents/MacOS/$bin"
    candidate="$app/Contents/MacOS/$bin"
    [[ $(lipo -archs "$candidate") == "$expected_arch" && $(file -b "$candidate") == "Mach-O 64-bit executable $expected_arch"* ]] || {
        echo "$bin: expected thin $expected_arch Mach-O" >&2; exit 1
    }
    deps=$(otool -L "$candidate")
    # Skip otool's first line (the inspected file's own path).
    if grep -Eq '^[[:space:]].*/(opt/homebrew|usr/local)/' <<< "$deps"; then
        echo "$bin: runner-local dylib dependency" >&2; exit 1
    fi
done
install -m 644 "$root/THIRD_PARTY_NOTICES.txt" "$app/Contents/Resources/THIRD_PARTY_NOTICES.txt"
# Apple bundle versions are numeric, even for prerelease archives.
bundle_version=${version%%[-+]*}
sed -e "s/@BUNDLE_ID@/$bundle_id/g" -e "s/@VERSION@/$bundle_version/g" "$plist" > "$app/Contents/Info.plist"
# The numeric bundle versions discard prerelease suffixes; seal the exact release in the signature.
plutil -insert DDGReleaseVersion -string "$version" "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$root/assets/ddg-icon.png" --out "$staging/AppIcon.iconset/icon_${size}x${size}.png" >/dev/null
    sips -z "$((size * 2))" "$((size * 2))" "$root/assets/ddg-icon.png" --out "$staging/AppIcon.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$staging/AppIcon.iconset" -o "$app/Contents/Resources/AppIcon.icns"

if [[ $mode == --sign ]]; then
    : "${APPLE_SIGNING_IDENTITY:?}" "${APPLE_TEAM_ID:?}" "${APPLE_NOTARY_KEY_FILE:?}" "${APPLE_NOTARY_KEY_ID:?}" "${APPLE_NOTARY_ISSUER_ID:?}"
    [[ $APPLE_TEAM_ID =~ ^[A-Z0-9]{10}$ ]] || { echo 'Invalid Apple Team ID' >&2; exit 1; }
    signing=(--force --options runtime --timestamp --entitlements "$root/assets/macos/entitlements.plist" --sign "$APPLE_SIGNING_IDENTITY")
    for bin in "${binaries[@]}"; do
        codesign "${signing[@]}" "$app/Contents/MacOS/$bin"
    done
fi

# Seal final nested executable bytes, not just their signing identities, for offline bootstrap.
host_hash=$(shasum -a 256 < "$app/Contents/MacOS/ddg-agents-host")
terminal_hash=$(shasum -a 256 < "$app/Contents/MacOS/ddg-terminal-server")
plutil -insert DDGBootstrapSHA256 -string "${host_hash%% *}:${terminal_hash%% *}" "$app/Contents/Info.plist"

if [[ $mode == --sign ]]; then
    codesign "${signing[@]}" "$app"
    codesign --verify --deep --strict --verbose=2 \
        -R "anchor apple generic and identifier \"$bundle_id\" and certificate 1[field.1.2.840.113635.100.6.2.6] and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = \"$APPLE_TEAM_ID\"" "$app"
    ditto -c -k --keepParent --norsrc "$app" "$staging/notarize.zip"
    xcrun notarytool submit "$staging/notarize.zip" --key "$APPLE_NOTARY_KEY_FILE" \
        --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER_ID" --wait
    xcrun stapler staple "$app"
    xcrun stapler validate "$app"
    /usr/sbin/spctl --assess --type execute --verbose=4 "$app"
fi

archive="ddg-agents-$version-$platform.app.zip"
ditto -c -k --keepParent --norsrc "$app" "$staging/$archive"
mv -f "$staging/$archive" "$out_dir/$archive"
printf '%s\n' "$out_dir/$archive"
