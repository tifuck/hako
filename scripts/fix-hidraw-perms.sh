#!/usr/bin/env bash
# Reinstalls the udev rules and re-applies them to plugged-in devices: the
# logged-in user gets a seat ACL (uaccess) on every supported node.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
sudo install -m 644 "$ROOT/udev/70-hako.rules" /etc/udev/rules.d/70-hako.rules
sudo udevadm control --reload-rules
sudo udevadm trigger
sudo udevadm settle
echo "Done. Restart daemon:"
echo "  systemctl --user restart hakod"
