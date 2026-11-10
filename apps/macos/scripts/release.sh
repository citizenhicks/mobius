#!/bin/bash
# Builds the signed, notarized release zip and its updated Sparkle appcast in dist/.
# Publishing is separate (.github/workflows/desktop-release.yml); see docs/updates.md.
set -euo pipefail
app_dir="$(cd "$(dirname "$0")/.." && pwd)"
cd "$app_dir"
repository="${MOBIUS_RELEASE_REPOSITORY:-citizenhicks/mobius}"
identity="${MOBIUS_SIGNING_IDENTITY:?Set MOBIUS_SIGNING_IDENTITY to a Developer ID Application identity}"
[[ "$identity" != - ]] || { echo 'A release needs a Developer ID identity, not ad-hoc signing' >&2; exit 1; }
provisioning_profile="${MOBIUS_PROVISIONING_PROFILE:?Set MOBIUS_PROVISIONING_PROFILE to the desktop Developer ID provisioning profile}"
python3 "$app_dir/scripts/provision-profile.py" "$provisioning_profile" "$app_dir/platform/Info.plist"
if [[ -n "${MOBIUS_NOTARY_KEY:-}" ]]; then
    notary=(--key "$MOBIUS_NOTARY_KEY" --key-id "${MOBIUS_NOTARY_KEY_ID:?}" --issuer "${MOBIUS_NOTARY_ISSUER:?}")
elif [[ -n "${MOBIUS_NOTARY_PROFILE:-}" ]]; then
    notary=(--keychain-profile "$MOBIUS_NOTARY_PROFILE")
else
    notary=(--apple-id "${MOBIUS_NOTARY_APPLE_ID:?Set notarization credentials, see docs/updates.md}"
        --team-id "${MOBIUS_NOTARY_TEAM_ID:?}" --password "${MOBIUS_NOTARY_PASSWORD:?}")
fi

version="$(cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "mobius-desktop"))')"
tag="mobius-desktop-v$version"
dist="$app_dir/dist"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$dist"

MOBIUS_BUILD_PROFILE=release "$app_dir/scripts/bundle.sh" "$work/bundle"
app="$(readlink "$work/bundle/möbius.app")"
info="$app/Contents/Info.plist"
sign_update="$("$app_dir/scripts/swiftpm-cache.sh")/artifacts/sparkle/Sparkle/bin/sign_update"
[[ -x "$sign_update" ]] || { echo "Sparkle signer is missing: $sign_update" >&2; exit 1; }
key="$(plutil -extract SUPublicEDKey raw "$info")"
[[ "$(printf '%s' "$key" | base64 -d 2>/dev/null | wc -c | tr -d ' ')" == 32 ]] ||
    { echo 'Paste the Sparkle public key into SUPublicEDKey in platform/Info.plist first' >&2; exit 1; }
feed="$(plutil -extract SUFeedURL raw "$info")"
minimum="$(plutil -extract LSMinimumSystemVersion raw "$info")"
build_number="$(plutil -extract CFBundleVersion raw "$info")"
codesign -dv "$app" 2>&1 | grep 'flags=.*runtime' >/dev/null || { echo 'The bundle lacks the hardened runtime' >&2; exit 1; }

notarize() {
    local submission status id
    submission="$(xcrun notarytool submit "$1" "${notary[@]}" --wait --output-format json)"
    status="$(printf '%s' "$submission" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')"
    if [[ "$status" != Accepted ]]; then
        id="$(printf '%s' "$submission" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
        xcrun notarytool log "$id" "${notary[@]}" >&2 || true
        echo "Notarization of $1 finished with status $status" >&2
        exit 1
    fi
}

# Notarize the exact bundle, staple its ticket, then archive it for Sparkle.
ditto -c -k --keepParent "$app" "$work/notarize.zip"
notarize "$work/notarize.zip"
xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl --assess --type execute --verbose=2 "$app"
archive="mobius-desktop-$version.zip"
rm -f "$dist/$archive"
ditto -c -k --sequesterRsrc --keepParent "$app" "$dist/$archive"

# The download people install from: a disk image to drag möbius into Applications, itself
# signed, notarized and stapled so it opens offline without a Gatekeeper prompt.
image="mobius-desktop-$version.dmg"
rm -f "$dist/$image"
uvx --from dmgbuild==1.6.7 dmgbuild -s "$app_dir/platform/resources/dmg-settings.py" \
    -D app="$app" möbius "$dist/$image"
codesign --force --sign "$identity" --timestamp "$dist/$image"
notarize "$dist/$image"
xcrun stapler staple "$dist/$image"
xcrun stapler validate "$dist/$image"
spctl --assess --type open --context context:primary-signature --verbose=2 "$dist/$image"

# Sparkle's EdDSA signature: SPARKLE_PRIVATE_KEY in CI, otherwise the login Keychain.
if [[ -n "${SPARKLE_PRIVATE_KEY:-}" ]]; then
    signature="$(printf '%s' "$SPARKLE_PRIVATE_KEY" | "$sign_update" --ed-key-file - -p "$dist/$archive")"
else
    signature="$("$sign_update" -p "$dist/$archive")"
fi
# A key that does not match SUPublicEDKey would publish an update no one can install.
swift "$app_dir/scripts/verify-update.swift" "$key" "$signature" "$dist/$archive" ||
    { echo 'The Sparkle private key does not match SUPublicEDKey in Info.plist' >&2; exit 1; }

curl --fail --silent --location --output "$work/appcast.xml" "$feed" || rm -f "$work/appcast.xml"
python3 "$app_dir/scripts/appcast.py" --existing "$work/appcast.xml" --output "$dist/appcast.xml" \
    --version "$version" --build-number "$build_number" --minimum-system "$minimum" \
    --url "https://github.com/$repository/releases/download/$tag/$archive" \
    --length "$(stat -f %z "$dist/$archive")" --signature "$signature" \
    --notes-url "https://github.com/$repository/releases/tag/$tag"
(cd "$dist" && shasum -a 256 "$archive" > "$archive.sha256" && shasum -a 256 "$image" > "$image.sha256")
printf 'Release %s: %s, %s and %s\n' "$tag" "$dist/$image" "$dist/$archive" "$dist/appcast.xml"
