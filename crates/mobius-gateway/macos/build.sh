#!/bin/bash
set -euo pipefail

project_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$project_dir/../../.." && pwd)"
output_dir="${1:-$repo_dir/target/macos}"
mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
app="$output_dir/möbius-app.app"

# Keep both Apple clients on the exact gateway transport contract.
python3 - "$repo_dir" <<'PY'
import pathlib, re, sys
root = pathlib.Path(sys.argv[1])
sources = {
    'Rust gateway': root / 'crates/mobius-gateway/src/wire.rs',
    'iOS app': root / 'mobius-app/apple/Sources/MobiusApp/Gateway/Wire/GatewayWire.swift',
    'macOS menu bar': root / 'crates/mobius-gateway/macos/Sources/MobiusGatewayMenuBar/GatewayWire.swift',
}
texts = {name: path.read_text() for name, path in sources.items()}
versions = {
    'Rust gateway': re.search(r'PROTOCOL_VERSION: u16 = (\d+)', texts['Rust gateway'])[1],
    **{
        name: re.search(r'gatewayProtocolVersion = (\d+)', text)[1]
        for name, text in texts.items() if name != 'Rust gateway'
    },
}
frame_limits = {
    'Rust gateway': re.search(r'MAX_FRAME_BYTES: usize = ([^;]+);', texts['Rust gateway'])[1],
    **{
        name: re.search(r'maximumGatewayFrameBytes = ([^\n]+)', text)[1]
        for name, text in texts.items() if name != 'Rust gateway'
    },
}
normalize = lambda expression: re.sub(r'\s+', '', expression)
assert len(set(versions.values())) == 1, f'Gateway protocol versions differ: {versions}'
assert len({normalize(value) for value in frame_limits.values()}) == 1, \
    f'Gateway frame limits differ: {frame_limits}'
PY

gateway="${MOBIUS_GATEWAY_BINARY:-$repo_dir/target/release/mobius-gateway}"
if [[ -z "${MOBIUS_GATEWAY_BINARY:-}" ]]; then
    cargo build --manifest-path "$repo_dir/Cargo.toml" --release --locked -p mobius-cli --bin mobius-gateway
fi
test -x "$gateway"
swift build --package-path "$project_dir" -c release --disable-automatic-resolution \
    -Xswiftc -warnings-as-errors -Xlinker -rpath -Xlinker '@executable_path/../Frameworks'
bin_dir="$(swift build --package-path "$project_dir" -c release --show-bin-path)"

mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources"
cp "$project_dir/Resources/Info.plist" "$app/Contents/Info.plist"
cp "$bin_dir/MobiusGatewayMenuBar" "$gateway" "$app/Contents/MacOS/"
cp "$project_dir/Resources/MobiusLogo.svg" "$app/Contents/Resources/"
xcrun actool "$repo_dir/mobius-app/apple/Sources/MobiusApp/Assets.xcassets" \
    "$repo_dir/mobius-app/apple/Sources/MobiusApp/AppIcon.icon" \
    --compile "$app/Contents/Resources" --platform macosx \
    --minimum-deployment-target 26.0 --target-device mac --app-icon AppIcon \
    --output-partial-info-plist "$output_dir/icon-info.plist"
/usr/libexec/PlistBuddy -c "Merge '$output_dir/icon-info.plist'" "$app/Contents/Info.plist"
ditto "$bin_dir/WebRTC.framework" "$app/Contents/Frameworks/WebRTC.framework"
cp "$repo_dir/LICENSE" "$repo_dir/NOTICE" "$app/Contents/Resources/"
cp "$project_dir/Resources/WebRTC-LICENSE.txt" "$app/Contents/Resources/"

if [[ -n "${MOBIUS_CLOUDFLARED_BINARY:-}" ]]; then
    cp "$MOBIUS_CLOUDFLARED_BINARY" "$app/Contents/MacOS/cloudflared"
    cp "$(dirname "$MOBIUS_CLOUDFLARED_BINARY")/cloudflared-LICENSE" "$app/Contents/Resources/"
fi
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$project_dir/../Cargo.toml" | head -1)"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"

identity="${MOBIUS_SIGNING_IDENTITY:--}"
xattr -cr "$app"
signing=(--force --sign "$identity")
if [[ "$identity" != '-' ]]; then
    signing+=(--options runtime --timestamp)
else
    signing+=(--options 0)
fi
codesign "${signing[@]}" "$app/Contents/Frameworks/WebRTC.framework"
codesign "${signing[@]}" "$app/Contents/MacOS/mobius-gateway"
if [[ -f "$app/Contents/MacOS/cloudflared" ]]; then
    codesign "${signing[@]}" "$app/Contents/MacOS/cloudflared"
fi
codesign "${signing[@]}" --entitlements "$project_dir/Resources/MobiusGateway.entitlements" "$app"
codesign --verify --deep --strict "$app"
printf 'Built %s\n' "$app"
if [[ "$identity" == '-' ]]; then
    printf 'Development signature only; distribution requires Developer ID signing and notarization.\n'
fi
