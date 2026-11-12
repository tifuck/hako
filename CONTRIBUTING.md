# Contributing

Hako is a Linux-only Rust workspace: a daemon (`hakod`) that owns all
hardware and audio I/O, and a thin egui client (`hako`). The client never
opens HID or PipeWire; shared types and the IPC schema live in `libs/core`, so
change them there first, then the daemon and client.

## Checks

Every PR must pass what CI runs:

```bash
scripts/check-public.sh   # no local paths, user names or secrets
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Hardware tests are `#[ignore]`d; run them locally with
`cargo test -- --ignored` with the device plugged in.

## Adding a device

Drivers live in one folder per manufacturer under `daemon/src/drivers/<vendor>/`.

1. **Existing vendor, same protocol** — add a `Model` entry (VID, PIDs, kind,
   name, `usbfs`) to that vendor's `MODELS` table. Usually that is the whole
   change. List every PID the device enumerates as (receiver, wired, ...); the
   first is canonical and names its remap file.
2. **New vendor** — create `daemon/src/drivers/<vendor>/mod.rs` that exports
   `pub const MODELS: &[Model]` and an adapter implementing `DeviceAdapter`,
   then add `<vendor>::MODELS` to `drivers::models()` in
   `daemon/src/drivers/mod.rs`. That is the only shared file you touch.
   `open` returns a list: a wireless receiver (Unifying, Bolt, HyperSpeed,
   Slipstream...) returns one adapter per paired device. Such a device's
   `DeviceId` should carry its own wireless PID (the one its evdev node
   reports) so remaps attach to the right input node. Set `multi: true` on a
   receiver whose `open` skips the devices in `ctx.open`: the hub then
   reopens it every 10 s so devices paired later show up.
   A protocol that can recognise devices no table lists (HID++, QMK raw HID)
   may add a sysfs-only probe to `FALLBACKS` in `drivers/mod.rs`.
   Lighting the firmware only shows while it is re-sent: return a short
   `tick_interval()` (e.g. 50 ms) from the adapter.
   Run `hakod --diagnose` to see every HID node, its report IDs and usage
   pages, and which driver (if any) handles it.
3. **Generated files** — regenerate both from the registry (tests fail if
   either is stale):
   ```bash
   cargo run -q -p hakod -- --udev-rules > udev/70-hako.rules
   cargo run -q -p hakod -- --list-devices > docs/DEVICES.md
   ```
   Set `usbfs: true` only if the driver needs raw USB control transfers.
   Tested a device on real hardware? Add it to `HARDWARE_VERIFIED` in
   `daemon/src/drivers/mod.rs` and regenerate `docs/DEVICES.md`.
4. **Settings** — setting keys are device-agnostic (`brightness`, `effect`,
   `color_r/g/b`, `dpi`, `report_rate`, `mic_volume`, ...). Reuse them. A new
   key is one line in the `setting_keys!` table in `libs/core/src/catalog.rs`,
   with the UI group (tab) it belongs to. Declare each control's range or
   choices in its `SettingMeta`. The daemon validates every write against it
   before your `set` runs, so `set` only sees values you advertised. Mark a
   control `.hidden(true)` when it is settable but irrelevant right now (e.g.
   color while the effect is `spectrum`). The UI builds tabs from the groups
   your controls use, so most devices need no UI changes.
5. **Tests** — unit-test packet building and report parsing against byte
   layouts from your source. The registry test fails if two models claim the
   same VID:PID.

Each adapter runs on its own thread (`daemon/src/worker.rs`), so blocking
HID calls in `set`/`tick` are fine; keep reads bounded with timeouts.

Keep a vendor's protocol code inside its folder. Split it into files there
(e.g. `hidpp.rs`, `<model>.rs`) when it grows. Transport helpers useful to
several vendors (hidraw, feature reports, usbfs) belong in `daemon/src/hid/`.

## Protocol sources and licensing

Hako is MIT. Reimplement protocols from public documentation, packet
captures, and your own reverse engineering. **Do not copy code** from GPL
projects (OpenRazer, Solaar, libratbag, HeadsetControl, OpenRGB); citing them
as references for byte layouts is fine. Name your sources in the driver's
module doc comment and in `NOTICE`.

## Onboard memory (flash / EEPROM / onboard profiles)

Normal controls must be volatile: the daemon re-applies them on reconnect.
A write that changes the device's own memory is opt-in per device:

- Mark the control `.persistent()`. The daemon refuses it until the owner
  turns on "Allow saving to device memory" for that device (a warning dialog
  in the UI; stored in `config.toml`, never by the driver).
- Protocols with a store flag or save command expose a
  `SettingKey::SaveToDevice` toggle (`.persistent()`) instead; while it is on,
  later user changes are also stored. The daemon switches it off when the
  opt-in is off.
- Write onboard memory only inside `set()` for a user change: never on open,
  reconnect, tick, or profile apply (profiles never contain these controls).
- Read-validate-write-verify: read the block, check length / checksum /
  magic and abort if wrong, write, read back where the protocol allows.
- Only offer a write at **high confidence**: the exact layout (report, bytes,
  address / slot, encoding, checksum, commit) agreed by two independent
  sources, or by one widely used implementation for that exact PID plus a
  second confirming source; unambiguous target; no known brick reports.
  Anything less is shown as `.persistent().blocked("reason")`.
- Record the research per vendor in `docs/onboard-writes/<vendor>.md`.

Never write firmware or bootloaders, factory-reset, change pairing, or write
onboard key maps / macros.

## Releases

Pushing a tag `vX.Y.Z` that matches `version` in the workspace `Cargo.toml`
runs `.github/workflows/release.yml`: it checks, tests, builds on Ubuntu 22.04
(glibc 2.35+), builds `hako_X.Y.Z_amd64.deb` (`packaging/deb/`) and test-installs
it, and publishes the `.deb`, `hako-X.Y.Z-x86_64-linux.tar.gz`, `SHA256SUMS`
and provenance attestations. Installed release builds find it through the
GitHub API and offer the update. Keep the asset names and the tarball layout
(`bin/`, `scripts/install-user.sh`, `udev/`) stable: the updater relies on them.
