#!/bin/bash
# Prints the directory holding the pinned cloudflared and its license. The gateway the app
# runs looks for cloudflared beside its executable, as the core's gateway release bundles
# it; keep the version and checksums in step with the core's release workflow.
set -euo pipefail
version=2026.10.7
asset=cloudflared-darwin-arm64.tgz
checksum=3586ccf397e98e5f437e60e7c307ca6ac463e110580503af8bb8e6f9ebf831bd
license_checksum=58d1e17ffe5109a7ae296caafcadfdbe6a7d176f0bc4ab01e12a689b0499d8bd
[[ "$(uname -m)" == arm64 ]] || { echo 'möbius for macOS bundles the Apple silicon cloudflared' >&2; exit 1; }
cache="$HOME/Library/Caches/dev.thinkingsand.mobius/cloudflared-$version"
if [[ ! -x "$cache/cloudflared" || ! -s "$cache/LICENSE" ]]; then
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    curl --fail --location --silent --show-error --output "$work/$asset" \
        "https://github.com/cloudflare/cloudflared/releases/download/$version/$asset"
    printf '%s  %s\n' "$checksum" "$work/$asset" | shasum -a 256 --check >/dev/null
    tar -C "$work" -xzf "$work/$asset"
    curl --fail --location --silent --show-error --output "$work/LICENSE" \
        "https://raw.githubusercontent.com/cloudflare/cloudflared/$version/LICENSE"
    printf '%s  %s\n' "$license_checksum" "$work/LICENSE" | shasum -a 256 --check >/dev/null
    mkdir -p "$cache"
    install -m 755 "$work/cloudflared" "$cache/cloudflared"
    cp "$work/LICENSE" "$cache/LICENSE"
fi
printf '%s\n' "$cache"
