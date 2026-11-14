#!/usr/bin/env bash
# Installs hako for the current user. Works from a source checkout (builds
# first) and from an extracted release tarball (uses its prebuilt bin/ and lib/).
# HAKO_SKIP_UDEV=1 skips the sudo step (the in-app updater sets it).
# Exit 3: installed, but the daemon did not restart.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [ -x "$ROOT/bin/hakod" ] && [ -x "$ROOT/bin/hako" ]; then
  BIN="$ROOT/bin"
  FX="$ROOT/lib/libhako_fx.so"
else
  source "$HOME/.cargo/env" 2>/dev/null || true
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
  BIN="$ROOT/target/release"
  FX="$BIN/libhako_fx.so"
fi
ICONDIR="$HOME/.local/share/icons/hicolor/256x256/apps"
LIBDIR="$HOME/.local/lib/hako"
mkdir -p "$HOME/.local/bin" "$LIBDIR" "$HOME/.config/systemd/user" "$HOME/.local/share/applications" "$ICONDIR"

# hakod's effects library (it looks in ../lib/hako) and both binaries are
# staged next to their targets, then renamed into place, the library first:
# running mixer strips keep the old file mapped, and no new hakod runs without
# it. A failed rename puts the earlier files back, so a failure never leaves a
# half-written file, a new hakod with an old hako, or an old hakod with the
# new library.
DEST="$HOME/.local/bin"
FILES=("$LIBDIR/libhako_fx.so" "$DEST/hakod" "$DEST/hako")
staged() { echo "$(dirname "$1")/.$(basename "$1").$2"; }
trap 'for f in "${FILES[@]}"; do rm -f "$(staged "$f" new)" "$(staged "$f" old)"; done' EXIT
install -m 644 "$FX" "$(staged "$LIBDIR/libhako_fx.so" new)"
for b in hakod hako; do
  install -m 755 "$BIN/$b" "$(staged "$DEST/$b" new)"
done
for f in "${FILES[@]}"; do
  if [ -e "$f" ] || [ -L "$f" ]; then cp -a "$f" "$(staged "$f" old)"; fi
done
replaced=()
for f in "${FILES[@]}"; do
  if ! mv -f "$(staged "$f" new)" "$f"; then
    for r in "${replaced[@]}"; do
      old="$(staged "$r" old)"
      if [ -e "$old" ] || [ -L "$old" ]; then
        mv -f "$old" "$r"
      else
        rm -f "$r"
      fi
    done
    echo "Could not install $(basename "$f"); the previous files were kept." >&2
    exit 1
  fi
  replaced+=("$f")
done
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

# The files are in place: a failed restart is a warning with its own exit
# code (the in-app updater reports it), not a failed install.
# restart (not just start) so an upgrade replaces a running daemon
if ! { systemctl --user daemon-reload &&
  systemctl --user enable hakod.service &&
  systemctl --user restart hakod.service; }; then
  echo "Installed, but restarting the daemon failed: run systemctl --user restart hakod" >&2
  exit 3
fi
echo "Done. Launch UI with: hako"
