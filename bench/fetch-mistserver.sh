#!/usr/bin/env bash
# Downloads the official MistServer macOS build (universal x86_64+arm64)
# from mistserver.org into bench/.cache and verifies the SHA-256 published
# on https://mistserver.org/download. Pinned version. Prints the directory
# that holds MistController and the MistIn*/MistOut* binaries.
set -euo pipefail
VERSION="${MISTSERVER_VERSION:-3.11.2}"
SHA256="${MISTSERVER_SHA256:-026d216aa8824a638252243cd7ce1b9cd2cf832fad37cd66c2fa1241e192b217}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CACHE="$HERE/.cache"
[ "$(uname -s)" = "Darwin" ] || { echo "fetch-mistserver.sh: only the macOS build is wired up" >&2; exit 1; }
asset="mistserver_mach64V${VERSION}.zip"
dir="$CACHE/mistserver-$VERSION"
mkdir -p "$CACHE"
cd "$CACHE"
if [ ! -x "$dir/MistController" ]; then
  curl -fsSL --max-time 900 -o "$asset" "https://r.mistserver.org/dl/$asset"
  echo "$SHA256  $asset" | shasum -a 256 -c - >&2
  rm -rf "$dir" "$dir.tmp"
  mkdir -p "$dir.tmp"
  unzip -q "$asset" -d "$dir.tmp"
  ctl="$(find "$dir.tmp" -name MistController -type f | head -1)"
  [ -n "$ctl" ] || { echo "no MistController in $asset" >&2; exit 1; }
  mv "$(dirname "$ctl")" "$dir"
  rm -rf "$dir.tmp"
  chmod +x "$dir"/Mist* 2>/dev/null || true
  xattr -dr com.apple.quarantine "$dir" 2>/dev/null || true
fi
echo "$dir"
