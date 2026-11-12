#!/usr/bin/env bash
# Installs hako for the current user. Works from a source checkout (builds
# first) and from an extracted release tarball (uses its prebuilt bin/).
# HAKO_SKIP_UDEV=1 skips the sudo step (the in-app updater sets it).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [ -x "$ROOT/bin/hakod" ] && [ -x "$ROOT/bin/hako" ]; then
  BIN="$ROOT/bin"
else
  source "$HOME/.cargo/env" 2>/dev/null || true
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
  BIN="$ROOT/target/release"
fi
ICONDIR="$HOME/.local/share/icons/hicolor/256x256/apps"
mkdir -p "$HOME/.local/bin" "$HOME/.config/systemd/user" "$HOME/.local/share/applications" "$ICONDIR"

install -m 755 "$BIN/hakod" "$HOME/.local/bin/hakod"
install -m 755 "$BIN/hako" "$HOME/.local/bin/hako"
install -m 644 "$ROOT/packaging/systemd/hakod.service" "$HOME/.config/systemd/user/hakod.service"
install -m 644 "$ROOT/packaging/hako.desktop" "$HOME/.local/share/applications/hako.desktop"
install -m 644 "$ROOT/client/assets/icon.png" "$ICONDIR/hako.png"
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true

if [ -z "${HAKO_SKIP_UDEV:-}" ]; then
  echo "Installing udev rules (needs sudo)…"
  if sudo install -m 644 "$ROOT/udev/70-hako.rules" /etc/udev/rules.d/70-hako.rules; then
    sudo udevadm control --reload-rules
    sudo udevadm trigger
  else
    echo "Skipped udev (no sudo). hidraw may be root-only until rules are installed."
  fi
fi

systemctl --user daemon-reload
systemctl --user enable hakod.service
# restart (not just start) so an upgrade replaces a running daemon
systemctl --user restart hakod.service
echo "Done. Launch UI with: hako"
