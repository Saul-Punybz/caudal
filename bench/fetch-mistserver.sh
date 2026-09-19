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
# recvonly media, no data channel). SSL/DTLS (needed for WHEP) stays on: the
# CI workflow deliberately does not install libmbedtls-dev/libsrtp2-dev, so
# Meson always builds its own pinned mbedtls (3.6.6) and libsrtp2 from their
# subprojects/*.wrap -- Ubuntu 24.04's system mbedtls is 2.28, an API
# mismatch that failed with "'ssl_conf' was not declared" in lib/socket.cpp.
# Pinned version either way. Prints the directory that holds MistController
# and the MistIn*/MistOut* binaries.
#
# -x so every command (and meson/ninja's own output) lands in the CI log;
# a build failure must be diagnosable from the log, not just "exit 1".
set -euxo pipefail
VERSION="${MISTSERVER_VERSION:-3.11.2}"
SHA256="${MISTSERVER_SHA256:-026d216aa8824a638252243cd7ce1b9cd2cf832fad37cd66c2fa1241e192b217}"
# `git tag 3.11.2` is an annotated tag object (a7cbd0d...), not a commit;
# this is the commit it points to (`git ls-remote` .../3.11.2^{}). A shallow
# clone of an annotated tag prints "is not a commit!" and detaches at this
# commit anyway -- harmless, but verified below rather than trusted blind.
MISTSERVER_COMMIT="${MISTSERVER_COMMIT:-51b50c59d75e9937b2ce2660bfa97437bd64b6b5}"
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
  got="$(git -C "$src" rev-parse HEAD)"
  if [ "$got" != "$MISTSERVER_COMMIT" ]; then
    echo "fetch-mistserver.sh: $src is at $got, expected $VERSION ($MISTSERVER_COMMIT)" >&2
    exit 1
  fi
  build="$src/build"
  rm -rf "$build"
  dump_meson_log() {
    status=$?
    log="$build/meson-logs/meson-log.txt"
    if [ "$status" -ne 0 ] && [ -f "$log" ]; then
      echo "---- tail of $log (build failed) ----" >&2
      tail -n 300 "$log" >&2
    fi
  }
  trap dump_meson_log EXIT
  meson setup "$build" "$src" --buildtype=release \
    -DNOSRT=true -DNORIST=true -DNOUSRSCTP=true -DWITH_THREADNAMES=true
  ninja -C "$build" "${BINARIES[@]}"
  trap - EXIT
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
