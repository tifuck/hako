#!/usr/bin/env bash
# Builds hako_<version>_<arch>.deb from release binaries.
#   packaging/deb/build-deb.sh <version> <dir with hako + hakod> <output dir>
# Needs dpkg-dev (dpkg-deb, dpkg-shlibdeps).
set -euo pipefail
VERSION="$1"
BIN="$2"
OUT="$3"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ARCH="$(dpkg --print-architecture)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
PKG="$WORK/hako_${VERSION}_${ARCH}"

install -Dm755 "$BIN/hako" "$PKG/usr/bin/hako"
install -Dm755 "$BIN/hakod" "$PKG/usr/bin/hakod"
sed 's|%h/.local/bin/hakod|/usr/bin/hakod|' "$ROOT/packaging/systemd/hakod.service" \
  | install -Dm644 /dev/stdin "$PKG/usr/lib/systemd/user/hakod.service"
install -Dm644 "$ROOT/packaging/hako.desktop" "$PKG/usr/share/applications/hako.desktop"
install -Dm644 "$ROOT/client/assets/icon.png" "$PKG/usr/share/icons/hicolor/256x256/apps/hako.png"
install -Dm644 "$ROOT/udev/70-hako.rules" "$PKG/usr/lib/udev/rules.d/70-hako.rules"
install -Dm644 "$ROOT/LICENSE" "$PKG/usr/share/doc/hako/copyright"
install -Dm644 "$ROOT/NOTICE" "$PKG/usr/share/doc/hako/NOTICE"
for s in postinst prerm postrm; do
  install -Dm755 "$ROOT/packaging/deb/$s" "$PKG/DEBIAN/$s"
done

# Linked libraries from the binaries; the window system libraries are loaded
# at runtime (winit / glutin dlopen them), so they are listed by hand.
mkdir -p "$WORK/debian"
printf 'Source: hako\n\nPackage: hako\nArchitecture: any\n' > "$WORK/debian/control"
LINKED="$(cd "$WORK" && dpkg-shlibdeps -O "$PKG/usr/bin/hako" "$PKG/usr/bin/hakod" | sed 's/^shlibs:Depends=//')"
rm -rf "$WORK/debian"
RUNTIME="libegl1, libgl1, libwayland-client0, libwayland-cursor0, libwayland-egl1, libxkbcommon0, libxkbcommon-x11-0, libx11-6, libx11-xcb1, libxcursor1, libxi6, libxrandr2"

cat > "$PKG/DEBIAN/control" <<EOF
Package: hako
Version: $VERSION
Architecture: $ARCH
Maintainer: hako maintainers <173309113+tifuck@users.noreply.github.com>
Installed-Size: $(du -sk --exclude=DEBIAN "$PKG" | cut -f1)
Depends: $LINKED, $RUNTIME, systemd, udev
Recommends: pipewire-bin, wireplumber, alsa-utils, curl, pkexec | policykit-1
Suggests: lsp-plugins-ladspa
Section: utils
Priority: optional
Homepage: https://github.com/tifuck/Hako
Description: control panel for gaming keyboards, mice and headsets
 hako replaces vendor tools (Synapse, G HUB, iCUE, GG, NGENUITY, Armoury
 Crate) with one background service and one native window: lighting, DPI,
 polling rate, key bindings and macros, headset battery, sidetone, ChatMix
 and line-in monitoring for Razer, Logitech, SteelSeries, Corsair, HyperX,
 ASUS and more.
EOF

mkdir -p "$OUT"
dpkg-deb --build --root-owner-group -Zxz "$PKG" "$OUT/" >/dev/null
echo "$OUT/hako_${VERSION}_${ARCH}.deb"
