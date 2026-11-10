#!/bin/bash
set -euo pipefail
app_dir="$(cd "$(dirname "$0")/.." && pwd)"
output_dir="$1"
core_dir="$(cd "${2:-$app_dir/../..}" && pwd)"
mkdir -p "$output_dir"
# Native capabilities share the gateway contract; they do not implement another transport.
python3 "$core_dir/.github/sync-gateway-contract.py" --check \
    --swift "$app_dir/platform/desktop/GatewayWire.swift"
# Keep generated sources out of FileProvider, which can recreate conflict copies.
scratch_dir="$("$app_dir/scripts/swiftpm-cache.sh")"
swift_build=(swift build --package-path "$app_dir/platform" --scratch-path "$scratch_dir" -c release --jobs "${NUM_JOBS:-2}")
"${swift_build[@]}" -Xswiftc -warnings-as-errors
bin_dir="$("${swift_build[@]}" --show-bin-path)"
# Replace rather than overwrite: rewriting a signed Mach-O in place leaves the kernel's
# cached signature stale, and the next launch is killed with "Code Signature Invalid".
cp "$bin_dir/libMobiusNative.dylib" "$output_dir/.libMobiusNative.dylib.new"
mv -f "$output_dir/.libMobiusNative.dylib.new" "$output_dir/libMobiusNative.dylib"
for framework in LiveKitWebRTC Sparkle; do
    rm -rf "$output_dir/.$framework.framework.new"
    ditto "$bin_dir/$framework.framework" "$output_dir/.$framework.framework.new"
    rm -rf "$output_dir/$framework.framework"
    mv "$output_dir/.$framework.framework.new" "$output_dir/$framework.framework"
done
