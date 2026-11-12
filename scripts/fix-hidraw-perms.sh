#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RULES="$ROOT/udev/70-hako.rules"
sudo install -m 644 "$RULES" /etc/udev/rules.d/70-hako.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw
# Immediate fix without unplug (rules should set group on trigger; if not, force).
# Supported vid:pid pairs come from the generated rules file.
ids=$(grep -o 'ATTRS{idVendor}=="[0-9a-f]*", ATTRS{idProduct}=="[0-9a-f]*"' "$RULES" \
  | sed -E 's/.*"([0-9a-f]+)".*"([0-9a-f]+)"/\1:\2/')
for hr in /sys/class/hidraw/hidraw*; do
  name=$(basename "$hr")
  uevent="$hr/device/uevent"
  [ -f "$uevent" ] || continue
  # HID_ID=0003:00001532:0000026B -> 1532:026b
  id=$(grep -m1 '^HID_ID=' "$uevent" | cut -d= -f2 || true)
  IFS=: read -r _ v p <<<"$id"
  key=$(printf '%s:%s' "${v: -4}" "${p: -4}" | tr 'A-F' 'a-f')
  if grep -qx "$key" <<<"$ids"; then
    sudo chgrp plugdev "/dev/$name" || true
    sudo chmod 660 "/dev/$name" || true
    echo "fixed /dev/$name ($key)"
  fi
done
echo "Done. Restart daemon:"
echo "  systemctl --user restart hakod"
