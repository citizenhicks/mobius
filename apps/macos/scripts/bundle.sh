#!/bin/bash
set -euo pipefail
app_dir="$(cd "$(dirname "$0")/.." && pwd)"
core_dir="${MOBIUS_CORE_DIR:-$app_dir/../..}"
output_dir="${1:-$app_dir/target/bundle}"
profile="${MOBIUS_BUILD_PROFILE:-release}"
identity="${MOBIUS_SIGNING_IDENTITY:--}"
cd "$app_dir"
if [[ -n "${MOBIUS_PROVISIONING_PROFILE:-}" ]]; then
    [[ "$identity" != - ]] || { echo 'A provisioning profile requires Developer ID signing, not ad-hoc signing' >&2; exit 1; }
    python3 "$app_dir/scripts/provision-profile.py" "$MOBIUS_PROVISIONING_PROFILE" "$app_dir/platform/Info.plist"
fi
# MOBIUS_FEATURES builds development variants, such as the snapshot harness.
cargo build --locked --profile "$profile" --workspace ${MOBIUS_FEATURES:+--features "$MOBIUS_FEATURES"}
{ read -r target_dir; read -r version; read -r build_number; read -r cef_version; } < <(cargo metadata --locked --format-version 1 |
    python3 -c 'import json,sys; m=json.load(sys.stdin); p={p["name"]: p for p in m["packages"]}; app=p["mobius-desktop"]; print(m["target_directory"]); print(app["version"]); print(app["metadata"]["bundle"]["build-number"]); print(p["cef-dll-sys"]["version"].split("+")[1])')
# Sparkle orders independent build numbers while users see Cargo.toml's version.
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Cargo.toml version $version is not X.Y.Z" >&2; exit 1; }
[[ "$build_number" =~ ^[1-9][0-9]{0,3}$ ]] || { echo 'The bundle build number must be an integer from 1 to 9999' >&2; exit 1; }
binary_dir="$target_dir/$profile"
if [[ "$profile" == dev ]]; then binary_dir="$target_dir/debug"; fi
repository_key="$(printf '%s' "$app_dir" | shasum -a 256 | cut -c 1-16)"
bundle_dir="$HOME/Library/Caches/dev.thinkingsand.mobius/bundles/$repository_key/$profile"
mkdir -p "$output_dir" "$bundle_dir"
# Keep signed bundles outside Documents: FileProvider restores FinderInfo after cleanup.
staging_dir="$(mktemp -d "$bundle_dir/.build.XXXXXX")"
replaced_dir="$(mktemp -d "$output_dir/.previous.XXXXXX")"
trap 'rm -rf "$staging_dir" "$replaced_dir"' EXIT
app="$staging_dir/möbius.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources/icons"
cp "$app_dir/platform/Info.plist" "$app/Contents/Info.plist"
plist=(/usr/libexec/PlistBuddy "$app/Contents/Info.plist")
"${plist[@]}" -c "Set :CFBundleShortVersionString $version" -c "Set :CFBundleVersion $build_number"
if [[ -n "${MOBIUS_UPDATE_FEED:-}" ]]; then
    "${plist[@]}" -c "Set :SUFeedURL $MOBIUS_UPDATE_FEED"
elif [[ "$profile" == dev ]]; then
    # Development bundles never replace themselves with a release.
    "${plist[@]}" -c "Delete :SUFeedURL"
fi
cp "$binary_dir/mobius-desktop" "$app/Contents/MacOS/"
# The local gateway runs from this executable and starts cloudflared beside it for tunnels.
cloudflared_dir="$("$app_dir/scripts/cloudflared.sh")"
cp "$cloudflared_dir/cloudflared" "$app/Contents/MacOS/"
# Chromium: its framework, and a helper bundle per process kind, each running the small helper.
cef_arch="$(uname -m)"
[[ "$cef_arch" == arm64 ]] && cef_arch=aarch64
cef_framework="${CEF_PATH:-$app_dir/target/cef}/$cef_version/cef_macos_$cef_arch/Chromium Embedded Framework.framework"
[[ -d "$cef_framework" ]] || { echo "Chromium is missing at $cef_framework" >&2; exit 1; }
ditto "$cef_framework" "$app/Contents/Frameworks/Chromium Embedded Framework.framework"
cp "$(dirname "$cef_framework")/CREDITS.html" "$app/Contents/Resources/Chromium-CREDITS.html"
helper_kinds=("" " (GPU)" " (Renderer)" " (Plugin)" " (Alerts)")
for kind in "${helper_kinds[@]}"; do
    helper="$app/Contents/Frameworks/Mobius Helper$kind.app"
    suffix="$(printf '%s' "$kind" | tr -d ' ()' | tr '[:upper:]' '[:lower:]')"
    mkdir -p "$helper/Contents/MacOS"
    cp "$binary_dir/mobius-browser-helper" "$helper/Contents/MacOS/Mobius Helper$kind"
    cp "$app_dir/platform/resources/BrowserHelper.plist" "$helper/Contents/Info.plist"
    /usr/libexec/PlistBuddy -c "Set :CFBundleExecutable Mobius Helper$kind" \
        -c "Set :CFBundleName Mobius Helper$kind" \
        -c "Set :CFBundleIdentifier app.mobius.desktop.helper${suffix:+.$suffix}" \
        -c "Set :CFBundleShortVersionString $version" -c "Set :CFBundleVersion $build_number" \
        "$helper/Contents/Info.plist"
done
# Packaged launches must load their bundled bridge, never a developer's Cargo output.
while IFS= read -r rpath; do
    if [[ "$rpath" == /*/out ]]; then
        install_name_tool -delete_rpath "$rpath" "$app/Contents/MacOS/mobius-desktop"
    fi
done < <(otool -l "$app/Contents/MacOS/mobius-desktop" | awk '$1 == "cmd" && $2 == "LC_RPATH" { getline; getline; print $2 }')
# Build the same bridge explicitly, avoiding stale libraries from other Cargo profiles.
native_dir="$staging_dir/native"
"$app_dir/scripts/build-native.sh" "$native_dir" "$core_dir"
cp "$native_dir/libMobiusNative.dylib" "$app/Contents/Frameworks/"
ditto "$native_dir/LiveKitWebRTC.framework" "$app/Contents/Frameworks/LiveKitWebRTC.framework"
sparkle="$app/Contents/Frameworks/Sparkle.framework"
ditto "$native_dir/Sparkle.framework" "$sparkle"
# The XPC services exist for sandboxed hosts; this app installs through Autoupdate.
rm -rf "$sparkle/XPCServices" "$sparkle/Versions/B/XPCServices"
xcrun actool "$app_dir/platform/resources/AppIcon.icon" \
    --compile "$app/Contents/Resources" --platform macosx --minimum-deployment-target 26.0 \
    --target-device mac --app-icon AppIcon --output-partial-info-plist "$native_dir/icon-info.plist" >/dev/null
/usr/libexec/PlistBuddy -c "Merge '$native_dir/icon-info.plist'" "$app/Contents/Info.plist"
cp "$core_dir/LICENSE" "$core_dir/NOTICE" "$app/Contents/Resources/"
cp "$app_dir/platform/resources/WebRTC-LICENSE.txt" "$app/Contents/Resources/"
cp "$cloudflared_dir/LICENSE" "$app/Contents/Resources/cloudflared-LICENSE"
cp "$app_dir/platform/resources/hi.mousePointer01.svg" "$app/Contents/Resources/icons/"
cp "$app_dir/platform/resources/"*.wav "$app/Contents/Resources/"
cp "$app_dir/assets/NOTICE" "$app/Contents/Resources/ASSETS_NOTICE"
cp "$app_dir/assets/desktop-viewer/novnc/LICENSE.txt" "$app/Contents/Resources/noVNC-LICENSE.txt"
cp "$app_dir/assets/desktop-viewer/novnc/vendor/pako/LICENSE" "$app/Contents/Resources/pako-LICENSE.txt"
ditto "$app_dir/assets/fonts" "$app/Contents/Resources/fonts"
xattr -cr "$app"
signing=(--force --sign "$identity")
if [[ "$identity" != '-' ]]; then signing+=(--options runtime --timestamp); fi
codesign "${signing[@]}" "$app/Contents/Frameworks/LiveKitWebRTC.framework"
# Sign Sparkle's helpers inside out, as Sparkle documents for Developer ID.
codesign "${signing[@]}" "$sparkle/Versions/B/Autoupdate"
codesign "${signing[@]}" "$sparkle/Versions/B/Updater.app"
codesign "${signing[@]}" "$sparkle"
codesign "${signing[@]}" "$app/Contents/Frameworks/libMobiusNative.dylib"
codesign "${signing[@]}" "$app/Contents/MacOS/cloudflared"
# Chromium inside out: its libraries, the framework, then helpers. Renderers compile
# JavaScript and the GPU process may compile shaders, so only their helpers may map
# executable memory; Chromium runs its GPU process from the base helper.
chromium="$app/Contents/Frameworks/Chromium Embedded Framework.framework"
# A Chromium update must never restore process-wide Objective-C class collisions.
python3 "$app_dir/scripts/check-objc-classes.py" "$chromium/Chromium Embedded Framework" \
    "$app/Contents/Frameworks/LiveKitWebRTC.framework/LiveKitWebRTC"
for library in "$chromium/Libraries/"*.dylib; do
    codesign "${signing[@]}" "$library"
done
codesign "${signing[@]}" "$chromium"
for kind in "${helper_kinds[@]}"; do
    case "$kind" in
    "" | " (GPU)" | " (Renderer)")
        codesign "${signing[@]}" --entitlements "$app_dir/platform/resources/BrowserHelper-JIT.entitlements" \
            "$app/Contents/Frameworks/Mobius Helper$kind.app"
        ;;
    *) codesign "${signing[@]}" "$app/Contents/Frameworks/Mobius Helper$kind.app" ;;
    esac
done
# Remove imported metadata without changing nested Mach-O signatures or resources.
xattr -cr "$app"
entitlements="$app_dir/platform/Mobius.entitlements"
if [[ -n "${MOBIUS_PROVISIONING_PROFILE:-}" ]]; then
    entitlements="$staging_dir/Mobius.entitlements"
    python3 "$app_dir/scripts/provision-profile.py" "$MOBIUS_PROVISIONING_PROFILE" "$app/Contents/Info.plist" \
        --signed-code "$app/Contents/Frameworks/libMobiusNative.dylib" --output "$entitlements"
fi
codesign "${signing[@]}" --entitlements "$entitlements" "$app"
codesign --verify --deep --strict "$app"
# Publish a complete fresh bundle, then keep the familiar build path as a link.
cached_app="$bundle_dir/möbius.app"
if [[ -d "$cached_app" ]]; then
    mv "$cached_app" "$staging_dir/previous.app"
fi
mv "$app" "$cached_app"
codesign --verify --deep --strict "$cached_app"
if [[ -e "$output_dir/möbius.app" || -L "$output_dir/möbius.app" ]]; then
    # A rename within the same FileProvider directory avoids hydrating the old bundle.
    mv "$output_dir/möbius.app" "$replaced_dir/previous.app"
fi
ln -s "$cached_app" "$output_dir/möbius.app"
codesign --verify --deep --strict "$output_dir/möbius.app"
printf 'Built %s -> %s\n' "$output_dir/möbius.app" "$cached_app"
