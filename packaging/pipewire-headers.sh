#!/usr/bin/env bash
# Build-time PipeWire headers for the release runner (Ubuntu 22.04).
#
# pipewire-rs needs PipeWire >= 1.0 headers to compile, but the release links
# the runner's own libpipewire 0.3.48 so the binaries keep loading on the
# oldest supported systems (the linker rejects any function 0.3.48 lacks).
# Fetches Ubuntu 24.04's pinned -dev packages into DIR, writes pkg-config
# files (newer headers, system library) and prints the env to use:
#
#   packaging/pipewire-headers.sh "$RUNNER_TEMP/pw" >> "$GITHUB_ENV"
set -euo pipefail
DIR="${1:?usage: $0 DIR}"
POOL=http://archive.ubuntu.com/ubuntu/pool/main/p/pipewire
VER=1.0.5-1ubuntu3.3
declare -A SUMS=(
  [libpipewire-0.3-dev]=1a0267989f02a8729cdb4df36d9acf3a5ac1a525200d428baffe8961ccb0e0ba
  [libspa-0.2-dev]=93a60e7656fb35c97a78159fd0556842c62c2d8b9aff096009f9a20c64c1f272
)
mkdir -p "$DIR/pc"
for pkg in "${!SUMS[@]}"; do
  deb="$DIR/${pkg}_${VER}_amd64.deb"
  curl -fsSL --retry 3 -o "$deb" "$POOL/${pkg}_${VER}_amd64.deb"
  echo "${SUMS[$pkg]}  $deb" | sha256sum -c --quiet
  dpkg-deb -x "$deb" "$DIR/tree"
done
cat >"$DIR/pc/libspa-0.2.pc" <<EOF
Name: libspa
Description: SPA headers ${VER}
Version: 1.0.5
Cflags: -I$DIR/tree/usr/include/spa-0.2 -D_REENTRANT
EOF
cat >"$DIR/pc/libpipewire-0.3.pc" <<EOF
Name: libpipewire
Description: PipeWire ${VER} headers, system library
Version: 1.0.5
Requires: libspa-0.2
Libs: -lpipewire-0.3
Cflags: -I$DIR/tree/usr/include/pipewire-0.3 -D_REENTRANT
EOF
echo "PKG_CONFIG_PATH=$DIR/pc"
echo "PKG_CONFIG_LIBDIR=$DIR/pc"
