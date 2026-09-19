#!/usr/bin/env bash
# macOS: downloads the official MistServer build (universal x86_64+arm64)
# from mistserver.org into bench/.cache and verifies the SHA-256 published
# on https://mistserver.org/download.
# Linux (GitHub Actions runners; never run locally): builds MistServer from
# its public source (github.com/DDVTECH/mistserver, Unlicense) at the pinned
# git tag with Meson/Ninja, release build, and only the binaries this
# benchmark runs (see BINARIES below) -- SRT and RIST support are compiled
# out (bench/mistserver.json has no TSSRT connector) since neither protocol
# is in the bench matrix; WebRTC data channels are compiled out too (WHEP is
# recvonly media, no data channel).
# Pinned version either way. Prints the directory that holds MistController
# and the MistIn*/MistOut* binaries.
set -euo pipefail
VERSION="${MISTSERVER_VERSION:-3.11.2}"
SHA256="${MISTSERVER_SHA256:-026d216aa8824a638252243cd7ce1b9cd2cf832fad37cd66c2fa1241e192b217}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CACHE="$HERE/.cache"
dir="$CACHE/mistserver-$VERSION"
mkdir -p "$CACHE"

# MistController scans its own directory for sibling Mist* binaries, so only
# these need to exist there: the controller, the live-stream buffer input,
# and one output per protocol this benchmark exercises (HTTP/CMAF/HLS for
# LL-HLS, RTMP for publish+play, RTSP, WebRTC for WHEP), plus MistSession
# (spawned per viewer connection by the outputs above).
BINARIES=(MistController MistInBuffer MistOutHTTP MistOutCMAF MistOutHLS MistOutRTMP MistOutRTSP MistOutWebRTC MistSession)

if [ -x "$dir/MistController" ]; then
  echo "$dir"
  exit 0
fi

os="$(uname -s)"
case "$os" in
Darwin)
  asset="mistserver_mach64V${VERSION}.zip"
  cd "$CACHE"
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
  ;;
Linux)
  src="$CACHE/mistserver-src-$VERSION"
  if [ ! -d "$src/.git" ]; then
    rm -rf "$src"
    git clone --branch "$VERSION" --depth 1 https://github.com/DDVTECH/mistserver "$src"
  fi
  build="$src/build"
  rm -rf "$build"
  meson setup "$build" "$src" --buildtype=release \
    -DNOSRT=true -DNORIST=true -DNOUSRSCTP=true -DWITH_THREADNAMES=true
  ninja -C "$build" "${BINARIES[@]}"
  rm -rf "$dir.tmp"
  mkdir -p "$dir.tmp"
  for b in "${BINARIES[@]}"; do
    cp "$build/$b" "$dir.tmp/"
  done
  mv "$dir.tmp" "$dir"
  rm -rf "$build"
  ;;
*)
  echo "fetch-mistserver.sh: unsupported OS $os" >&2
  exit 1
  ;;
esac
echo "$dir"
