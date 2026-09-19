#!/usr/bin/env bash
# Downloads the official MediaMTX release for this machine into bench/.cache
# and verifies it against the release's checksums.sha256. Pinned version.
set -euo pipefail
VERSION="${MEDIAMTX_VERSION:-v1.21.0}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CACHE="$HERE/.cache"
os="$(uname -s | tr '[:upper:]' '[:lower:]')"
arch="$(uname -m)"; [ "$arch" = "x86_64" ] && arch=amd64; [ "$arch" = "aarch64" ] && arch=arm64
asset="mediamtx_${VERSION}_${os}_${arch}.tar.gz"
base="https://github.com/bluenviron/mediamtx/releases/download/${VERSION}"
mkdir -p "$CACHE/mediamtx-$VERSION"
cd "$CACHE"
if [ ! -x "mediamtx-$VERSION/mediamtx" ]; then
  curl -fsSL -o "$asset" "$base/$asset"
  curl -fsSL -o "checksums-$VERSION.sha256" "$base/checksums.sha256"
  grep -E " \*?$asset\$" "checksums-$VERSION.sha256" | shasum -a 256 -c -
  tar -xzf "$asset" -C "mediamtx-$VERSION"
fi
echo "$CACHE/mediamtx-$VERSION/mediamtx"
