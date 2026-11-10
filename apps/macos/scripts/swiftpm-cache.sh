#!/bin/bash
# Shared by native builds and release signing; keep SwiftPM out of FileProvider.
set -euo pipefail
app_dir="$(cd "$(dirname "$0")/.." && pwd)"
repository_key="$(printf '%s' "$app_dir" | shasum -a 256 | cut -c 1-16)"
printf '%s\n' "$HOME/Library/Caches/dev.thinkingsand.mobius/swiftpm/$repository_key"
