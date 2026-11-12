//! ASUS ROG HID wire format, shared by mice, keyboards and headsets.
//!
//! Every command is one 64-byte report `[group, cmd, args...]`, zero padded.
//! Group `0x12` reads, `0x50`/`0x51` write. The device answers with an input
//! report that echoes the first two bytes, or `FF AA` when it cannot (command
//! unsupported, device asleep or out of range). Every `51 xx` write is
//! volatile and is re-applied on (re)connect; `50 03` (mice) and `50 55`
//! (keyboards) store the active onboard profile and are sent only for a user
//! change while the owner's save-to-device mode is on.
//!
//! Byte layouts: libratbag `src/asus.c`, `src/asus.h` (asus_query,
//! ASUS_STATUS_ERROR, `_asus_dpi2_data`/`_asus_dpi4_data`, ASUS_CMD_SAVE);
//! G-Helper `app/Peripherals/Mouse/AsusMouse.cs` (WriteForResponse, battery,
//! energy, lift-off, DPI colours, GetSaveProfilePacket) and
//! `Keyboard/AsusKeyboard.cs` (SaveLighting); OpenRGB
//! `AsusAuraMouseController.cpp` (SendUpdate, SaveMode) and
//! `AsusAuraKeyboardController.cpp` (UpdateDevice, SaveMode); pelta-control
//! `pelta/device.py` (report 0xCC framing); G-Helper
//! `app/Peripherals/PeripheralsProvider.cs` (Omni pairing list),
//! `Keyboard/AsusKeyboard.cs` and `Headset/AsusHeadset.cs`.

use crate::hid::{enumerate_hidraw, find_hidraw, usage_pages, HidrawDevice};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

pub const VID: u16 = 0x0b05;
pub const PACKET: usize = 64;
pub type Packet = [u8; PACKET];

/// Wireless mice answer through the dongle; G-Helper allows a few hundred ms.
const REPLY_TIMEOUT: Duration = Duration::from_millis(300);

/// Command bytes, zero padded to one report.
pub fn packet(bytes: &[u8]) -> Packet {
    let mut p = [0u8; PACKET];
    p[..bytes.len()].copy_from_slice(bytes);
    p
}

/// hidraw write buffer. Unnumbered reports go out as `00` + 64 bytes (the
/// kernel drops the 0). Numbered ones (Omni receiver 0x03, Pelta 0xCC) are 64
/// bytes including the id, so the payload's last byte is cut.
pub fn frame(report_id: u8, p: &Packet) -> Vec<u8> {
    let body = if report_id == 0 {
        &p[..]
    } else {
        &p[..PACKET - 1]
    };
    let mut v = Vec::with_capacity(PACKET + 1);
    v.push(report_id);
    v.extend_from_slice(body);
    v
}

/// Payload of an input report, or `None` if it carries another report id.
pub fn unframe(report_id: u8, raw: &[u8]) -> Option<Packet> {
    let data = if report_id == 0 {
        raw
    } else {
        raw.strip_prefix(&[report_id])?
    };
    let mut p = [0u8; PACKET];
    let n = data.len().min(PACKET);
    p[..n].copy_from_slice(&data[..n]);
    Some(p)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Ok,
    Rejected,
    Other,
}

/// Classify a report read after sending `req`. Reads `12 00` (profile) and
/// `12 04 xx` (settings / XY DPI / DPI colours) also echo the sub-command.
pub fn classify(req: &Packet, resp: &Packet) -> Reply {
    if resp[0] == 0xff && resp[1] == 0xaa {
        return Reply::Rejected;
    }
    let n = if req[0] == 0x12 && matches!(req[1], 0x00 | 0x04) {
        3
    } else {
        2
    };
    if resp[..n] == req[..n] {
        Reply::Ok
    } else {
        Reply::Other
    }
}

/// A command/echo exchange with the device (a [`Transport`], or a fake in tests).
pub trait Exchange {
    /// Send a command and wait for its echo.
    fn transact(&mut self, cmd: &[u8]) -> Result<Packet>;
}

impl Exchange for Transport {
    fn transact(&mut self, cmd: &[u8]) -> Result<Packet> {
        Transport::transact(self, cmd)
    }
}

pub struct Transport {
    dev: HidrawDevice,
    report_id: u8,
}

impl Transport {
    /// Open the vendor interface of `pid`; returns the transport and its path.
    pub fn open(pid: u16, iface: u32, report_id: u8) -> Result<(Self, String)> {
        let info = find_hidraw(VID, pid, Some(iface))?;
        let dev = HidrawDevice::open(&info.path)?;
        Ok((Self { dev, report_id }, info.path.display().to_string()))
    }

    /// Open the node of `pid` whose report descriptor declares usage `page`
    /// (G-Helper picks the vendor collection this way where it names no
    /// interface).
    pub fn open_page(pid: u16, page: u16, report_id: u8) -> Result<(Self, String)> {
        let info = enumerate_hidraw()?
            .into_iter()
            .find(|d| {
                d.vendor_id == VID
                    && d.product_id == pid
                    && usage_pages(&d.report_descriptor()).contains(&page)
            })
            .with_context(|| format!("no {VID:04x}:{pid:04x} node with usage page {page:04x}"))?;
        let dev = HidrawDevice::open(&info.path)?;
        Ok((Self { dev, report_id }, info.path.display().to_string()))
    }

    /// Send a command and return the next report on this transport's id,
    /// for replies that do not echo the command (Omni pairing list).
    pub fn request(&mut self, cmd: &[u8]) -> Result<Packet> {
        self.dev.drain(Duration::from_millis(1));
        self.dev.write(&frame(self.report_id, &packet(cmd)))?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut buf = [0u8; PACKET + 1];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(n) = self.dev.read_timeout(&mut buf, left) else {
                break;
            };
            match unframe(self.report_id, &buf[..n]) {
                Some(resp) if resp[0] == 0xff && resp[1] == 0xaa => {
                    bail!("device rejected {:02x}", cmd[0])
                }
                Some(resp) => return Ok(resp),
                None => {}
            }
        }
        bail!("no reply to {:02x}", cmd[0])
    }

    /// Send a command and wait for its echo.
    pub fn transact(&mut self, cmd: &[u8]) -> Result<Packet> {
        let req = packet(cmd);
        self.dev.drain(Duration::from_millis(1));
        self.dev.write(&frame(self.report_id, &req))?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut buf = [0u8; PACKET + 1];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(n) = self.dev.read_timeout(&mut buf, left) else {
                break;
            };
            let Some(resp) = unframe(self.report_id, &buf[..n]) else {
                continue;
            };
            match classify(&req, &resp) {
                Reply::Ok => return Ok(resp),
                Reply::Rejected => bail!(
                    "device rejected {:02x} {:02x} (unsupported, asleep or out of range)",
                    req[0],
                    req[1]
                ),
                Reply::Other => {}
            }
        }
        bail!("no reply to {:02x} {:02x}", req[0], req[1])
    }

    /// Write and give the device 20 ms to answer, as OpenRGB does for
    /// keyboards; their replies carry nothing we use.
    pub fn send(&mut self, cmd: &[u8]) -> Result<()> {
        self.dev.drain(Duration::from_millis(1));
        self.dev.write(&frame(self.report_id, &packet(cmd)))?;
        let mut buf = [0u8; PACKET + 1];
        let _ = self.dev.read_timeout(&mut buf, Duration::from_millis(20));
        Ok(())
    }
}

// ── Mice ────────────────────────────────────────────────────────────────────

/// `12 00`: current profile and DPI stage.
pub const GET_PROFILE: &[u8] = &[0x12, 0x00];
/// `50 03`: store the active profile's working values in onboard memory
/// (libratbag `ASUS_CMD_SAVE` 0x0350 LE, G-Helper `GetSaveProfilePacket`,
/// OpenRGB `AuraMouseController::SaveMode`). No address: the mouse picks the
/// active profile slot itself.
pub const MOUSE_SAVE: &[u8] = &[0x50, 0x03];
/// `12 07`: battery, auto-off and low-battery threshold.
pub const GET_BATTERY: &[u8] = &[0x12, 0x07];
/// `12 06`: lift-off distance.
pub const GET_LOD: &[u8] = &[0x12, 0x06];
/// `12 04 03`: DPI stage colours, 3 bytes per stage from byte 4.
pub const GET_DPI_COLORS: &[u8] = &[0x12, 0x04, 0x03];

/// Report rates by wire index (G-Helper `PollingRate`; libratbag
/// `ASUS_POLLING_RATES` is the first four).
pub const RATES_HZ: [u16; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

/// `12 04 00` settings, or `12 04 02` for per-axis DPI.
pub fn get_settings(xy: bool) -> [u8; 3] {
    [0x12, 0x04, if xy { 0x02 } else { 0x00 }]
}

/// Active DPI stage (1-based) from `12 00`: byte 11, as libratbag
/// `results[9]` and G-Helper `ParseDPIProfile`. TUF M3 Gen II counts from 0.
pub fn parse_stage(resp: &Packet, stages: u8, zero_based: bool) -> u8 {
    let s = resp[11].saturating_add(u8::from(zero_based));
    if (1..=stages).contains(&s) {
        s
    } else {
        1
    }
}

/// DPI of `stage` (1-based). Stages are u16 LE from byte 4 (2 bytes each, or
/// 4 with per-axis DPI where X comes first); wire = (dpi - step) / step.
pub fn parse_dpi(resp: &Packet, stage: u8, xy: bool, step: u16) -> u16 {
    let at = 4 + (usize::from(stage) - 1) * if xy { 4 } else { 2 };
    let raw = u16::from_le_bytes([resp[at], resp[at + 1]]);
    raw.saturating_mul(step).saturating_add(step)
}

/// Report rate in Hz. Rate follows the DPI stages (byte 4 + 2 * stages);
/// newer firmware keeps flags in the high bits (libratbag SETTINGS_V2 masks 0x07).
pub fn parse_rate(resp: &Packet, stages: u8) -> Option<u16> {
    RATES_HZ
        .get(usize::from(resp[4 + 2 * usize::from(stages)] & 0x07))
        .copied()
}

/// Angle snapping sits two fields after the rate (rate, debounce, snapping).
pub fn parse_snapping(resp: &Packet, stages: u8) -> bool {
    resp[8 + 2 * usize::from(stages)] == 0x01
}

pub fn parse_dpi_color(resp: &Packet, stage: u8) -> [u8; 3] {
    let at = 4 + (usize::from(stage) - 1) * 3;
    [resp[at], resp[at + 1], resp[at + 2]]
}

/// `51 31 <stage-1> 00 <lo> <hi>`; mice with DPI colours take the stage's
/// RGB after it (G-Helper `GetUpdateDPIPacket`), so callers re-send the
/// colour read from `12 04 03`.
pub fn set_dpi(stage: u8, dpi: u16, step: u16, color: Option<[u8; 3]>) -> Vec<u8> {
    let [lo, hi] = (dpi.max(step) / step - 1).to_le_bytes();
    let mut v = vec![0x51, 0x31, stage - 1, 0x00, lo, hi];
    if let Some(rgb) = color {
        v.extend_from_slice(&rgb);
    }
    v
}

/// `51 31 <field> 00 <value>`. Fields after the DPI stages: rate (+0),
/// debounce (+1), angle snapping (+2) (libratbag `ASUS_FIELD_*`).
pub fn set_field(field: u8, value: u8) -> [u8; 5] {
    [0x51, 0x31, field, 0x00, value]
}

/// `51 31 09 00 <stage>`: make `stage` (1-based) the active DPI stage
/// (G-Helper `GetChangeDPIProfilePacket2`; TUF M3 Gen II counts from 0).
pub fn set_active_stage(stage: u8, zero_based: bool) -> [u8; 5] {
    set_field(0x09, stage - u8::from(zero_based))
}

/// Debounce times in ms both sources accept: wire index `i` is `4 + 4i` ms
/// (libratbag `ASUS_DEBOUNCE_TIMES`, G-Helper `DebounceTime`); G-Helper
/// limits the range to 12..=32.
pub const DEBOUNCE_MS: [u8; 6] = [12, 16, 20, 24, 28, 32];

/// Debounce follows the rate (byte 6 + 2 * stages, libratbag `response`,
/// G-Helper `ParseDebounce`).
pub fn parse_debounce(resp: &Packet, stages: u8) -> u8 {
    4 + 4 * resp[6 + 2 * usize::from(stages)].min(7)
}

pub fn debounce_wire(ms: u8) -> u8 {
    ms.saturating_sub(4) / 4
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lod {
    None,
    /// `51 35 FF 00 FF <v>`, read at byte 7. G-Helper: this also resets the
    /// surface calibration; there is no separate command.
    Calibrated,
    /// `51 35 00 00 <v>`, read at byte 4 (Chakram Core, Keris, Impact II Wireless).
    Plain,
}

/// 0 = low, 1 = high.
pub fn set_lod(lod: Lod, high: bool) -> Vec<u8> {
    let v = u8::from(high);
    match lod {
        Lod::Calibrated => vec![0x51, 0x35, 0xff, 0x00, 0xff, v],
        Lod::Plain | Lod::None => vec![0x51, 0x35, 0x00, 0x00, v],
    }
}

pub fn parse_lod(lod: Lod, resp: &Packet) -> bool {
    resp[if lod == Lod::Calibrated { 7 } else { 4 }] == 0x01
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Battery {
    None,
    /// Byte 4 is the charge in percent.
    Percent,
    /// Byte 4 counts quarters (older Chakram/Keris/Pugio II/Impact II firmware).
    Quarters,
    /// Byte 6 counts quarters (G-Helper `StrixCarry.ParseBattery`).
    QuartersAt6,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BatteryReport {
    pub percent: u8,
    pub charging: bool,
    /// Auto-off wire value (byte 5) and low-battery threshold (byte 6), raw.
    pub auto_off: u8,
    pub low_battery: u8,
}

pub fn parse_battery(kind: Battery, resp: &Packet) -> BatteryReport {
    BatteryReport {
        percent: match kind {
            Battery::Quarters => resp[4].saturating_mul(25).min(100),
            Battery::QuartersAt6 => resp[6].saturating_mul(25).min(100),
            _ => resp[4].min(100),
        },
        charging: resp[9] > 0,
        auto_off: resp[5],
        low_battery: resp[6],
    }
}

/// `51 37 00 00 <auto-off> 00 <low battery>`; both are written together.
pub fn set_energy(auto_off: u8, low_battery: u8) -> [u8; 7] {
    [0x51, 0x37, 0x00, 0x00, auto_off, 0x00, low_battery]
}

/// Auto-off wire value ↔ minutes (0 = never): 0..=4 are 1/2/3/5/10 min, 0xFF never.
const AUTO_OFF_MIN: [u8; 5] = [1, 2, 3, 5, 10];

pub fn auto_off_minutes(wire: u8) -> Option<u8> {
    match wire {
        0xff => Some(0),
        w => AUTO_OFF_MIN.get(usize::from(w)).copied(),
    }
}

pub fn auto_off_wire(minutes: u8) -> Option<u8> {
    match minutes {
        0 => Some(0xff),
        m => AUTO_OFF_MIN.iter().position(|&x| x == m).map(|i| i as u8),
    }
}

/// Shared lighting modes (wire value is the same on every documented mouse
/// and keyboard).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Static,
    Breathing,
    Cycle,
}

impl Effect {
    pub fn parse(s: &str) -> Self {
        match s {
            "breathing" => Self::Breathing,
            "cycle" => Self::Cycle,
            _ => Self::Static,
        }
    }

    pub fn wire(self) -> u8 {
        match self {
            Self::Static => 0x00,
            Self::Breathing => 0x01,
            Self::Cycle => 0x02,
        }
    }

    pub fn has_color(self) -> bool {
        self != Self::Cycle
    }
}

/// Mouse LED: `51 28 <zone> 00 <mode> <brightness> R G B <dir> <random> <speed>`
/// (OpenRGB SendUpdate). Zones: 0 logo, 1 wheel, 2 underglow, 3 all.
pub fn set_led(zone: u8, effect: Effect, brightness: u8, rgb: [u8; 3]) -> [u8; 9] {
    let [r, g, b] = if effect.has_color() { rgb } else { [0; 3] };
    [0x51, 0x28, zone, 0x00, effect.wire(), brightness, r, g, b]
}

/// UI percent → device brightness scale (0..=4 on older mice, else 0..=100).
pub fn scale_brightness(pct: u8, max: u8) -> u8 {
    ((u16::from(pct.min(100)) * u16::from(max) + 50) / 100) as u8
}

// ── Omni receiver ───────────────────────────────────────────────────────────

/// Pairing list request, sent on report 1 of the receiver's interface 2;
/// mice are then driven on report 3, keyboards on report 2 (G-Helper
/// `DedectOmniMouse`, `KeyboardFromOmniPid`).
pub const OMNI_LIST: &[u8] = &[0xa0, 0x00, 0x00];
pub const OMNI_LIST_REPORT: u8 = 0x01;
pub const OMNI_MOUSE_REPORT: u8 = 0x03;
pub const OMNI_KEYBOARD_REPORT: u8 = 0x02;
pub const OMNI_IFACE: u32 = 2;

/// Paired device PIDs: u16 LE every 4 bytes from byte 4, up to the first 0.
pub fn parse_paired(resp: &Packet) -> Vec<u16> {
    (4..PACKET - 4)
        .step_by(4)
        .map(|at| u16::from_le_bytes([resp[at], resp[at + 1]]))
        .take_while(|&pid| pid != 0)
        .collect()
}

// ── Keyboards ───────────────────────────────────────────────────────────────

/// `51 2C <mode> 00 <speed> <brightness 0..100> <colour mode> <dir> 02 R G B`
/// (OpenRGB UpdateDevice for per-key boards; brightness = level * 25).
pub fn kb_effect(effect: Effect, speed: u8, brightness: u8, rgb: [u8; 3]) -> [u8; 12] {
    let [r, g, b] = if effect.has_color() { rgb } else { [0; 3] };
    let speed = if effect == Effect::Static { 0 } else { speed };
    [
        0x51,
        0x2c,
        effect.wire(),
        0x00,
        speed,
        brightness,
        0x00,
        0x00,
        0x02,
        r,
        g,
        b,
    ]
}

/// TUF K1 / K5 (not per-key): no `02` marker, RGB from byte 8 (OpenRGB
/// UpdateDevice `is_per_led_keyboard == false`, G-Helper `TUFK1`).
pub fn kb_effect_flat(effect: Effect, speed: u8, brightness: u8, rgb: [u8; 3]) -> [u8; 11] {
    let [r, g, b] = if effect.has_color() { rgb } else { [0; 3] };
    let speed = if effect == Effect::Static { 0 } else { speed };
    [
        0x51,
        0x2c,
        effect.wire(),
        0x00,
        speed,
        brightness,
        0x00,
        0x00,
        r,
        g,
        b,
    ]
}

/// `50 55`: store the active profile's lighting in onboard memory (OpenRGB
/// `AuraKeyboardController::SaveMode`, G-Helper `AsusKeyboard.SaveLighting`).
pub const KB_SAVE: &[u8] = &[0x50, 0x55];

/// `12 01`: battery percent at byte 5 (byte 10 on Falchion boards),
/// charging when byte 8 is 1 (G-Helper `AsusKeyboard.ReadBattery`).
pub const KB_POWER: &[u8] = &[0x12, 0x01];

pub fn kb_battery(resp: &Packet, at: usize) -> (u8, bool) {
    (resp[at].min(100), resp[8] == 1)
}

// ── ROG headsets (report 0xCC) ──────────────────────────────────────────────

/// `12 00 00 01`: headset linked to the dongle (byte 4; pelta-control).
pub const HS_LINK: &[u8] = &[0x12, 0x00, 0x00, 0x01];
/// `12 08`: charging (byte 4 == 1).
pub const HS_CHARGING: &[u8] = &[0x12, 0x08];
/// `12 03`: effect, brightness, R, G, B from byte 4.
pub const HS_LIGHTING: &[u8] = &[0x12, 0x03];
/// `12 24`: sidetone on (byte 4); `12 19`: sidetone level (byte 4).
pub const HS_SIDETONE_ON: &[u8] = &[0x12, 0x24];
pub const HS_SIDETONE_LEVEL: &[u8] = &[0x12, 0x19];
/// Sidetone level range both sources accept (pelta-control: 0-20, G-Helper
/// default `MaxSidetone` 20; 100 on Pelta).
pub const HS_SIDETONE_MAX: u8 = 20;
/// Sleep timer choices in minutes, 0 = never (wire value = minutes).
pub const HS_SLEEP_MIN: [u8; 6] = [0, 2, 3, 5, 10, 15];

#[derive(Debug, PartialEq, Eq)]
pub struct HsPower {
    pub sleep: u8,
    /// `None` while the headset is off or out of range (0xFE).
    pub battery: Option<u8>,
    pub low_battery: u8,
    pub low_battery_voice: u8,
}

/// `12 07` reply: sleep minutes, battery %, low-battery %, low-battery voice
/// from byte 4 (pelta-control `power_info`, G-Helper `ParseBattery`).
pub fn hs_power(resp: &Packet) -> HsPower {
    HsPower {
        sleep: resp[4],
        battery: (resp[5] <= 100).then_some(resp[5]),
        low_battery: resp[6],
        low_battery_voice: resp[7],
    }
}

/// `51 37 00 00 <sleep> <low %> <voice>`.
pub fn hs_set_power(sleep: u8, low_battery: u8, voice: u8) -> [u8; 7] {
    [0x51, 0x37, 0x00, 0x00, sleep, low_battery, voice]
}

/// Headset effects by wire value (pelta-control `EFFECTS`, G-Helper
/// `HeadsetLightingMode`).
pub const HS_EFFECTS: [(u8, &str); 4] = [
    (1, "static"),
    (2, "breathing"),
    (3, "strobing"),
    (4, "cycle"),
];
/// Strobing: both sources offer it on the Pelta; only G-Helper on the Delta II.
pub const HS_STROBE: u8 = 3;

/// `51 28 00 00 <effect> <brightness 0-100> R G B`.
pub fn hs_led(effect: u8, brightness: u8, rgb: [u8; 3]) -> [u8; 9] {
    let [r, g, b] = rgb;
    [0x51, 0x28, 0x00, 0x00, effect, brightness.min(100), r, g, b]
}

/// `12 03` reply: effect wire value, brightness, colour; `None` for values
/// outside [`HS_EFFECTS`].
pub fn hs_parse_led(resp: &Packet) -> Option<(u8, u8, [u8; 3])> {
    HS_EFFECTS
        .iter()
        .any(|&(w, _)| w == resp[4])
        .then(|| (resp[4], resp[5].min(100), [resp[6], resp[7], resp[8]]))
}

/// `41 20`: AI noise reduction on (byte 4 == 1); `41 02 00 00 <on>` sets it
/// (pelta-control `status` / `set_noise_reduction`, G-Helper
/// `ReadNoiseReduction` / `SetNoiseReduction`).
pub const HS_NOISE_REDUCTION: &[u8] = &[0x41, 0x20];

pub fn hs_set_noise_reduction(on: bool) -> [u8; 5] {
    [0x41, 0x02, 0x00, 0x00, u8::from(on)]
}

/// `12 28`: voice prompt (byte 4); `41 0A 00 00 <v>` sets it (pelta-control
/// `VOICE_PROMPT` / `set_voice_prompt`, G-Helper `SetVoicePrompt` and
/// `AsusHeadsetSettings.voicePrompts`).
pub const HS_VOICE_PROMPT: &[u8] = &[0x12, 0x28];
pub const HS_VOICE_PROMPTS: [(u8, &str); 3] = [(1, "English"), (2, "Chinese"), (0, "Prompt sound")];

pub fn hs_set_voice_prompt(v: u8) -> [u8; 5] {
    [0x41, 0x0a, 0x00, 0x00, v]
}

/// `41 11 00 00 <on>` then `61 11 00 00 <level>`.
pub fn hs_sidetone(on: bool) -> [u8; 5] {
    [0x41, 0x11, 0x00, 0x00, u8::from(on)]
}

pub fn hs_sidetone_level(level: u8) -> [u8; 5] {
    [0x61, 0x11, 0x00, 0x00, level.min(HS_SIDETONE_MAX)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(bytes: &[u8]) -> Packet {
        packet(bytes)
    }

    #[test]
    fn framing_matches_hidraw_conventions() {
        // Unnumbered: 00 + 64 payload bytes.
        let f = frame(0, &packet(GET_BATTERY));
        assert_eq!(f.len(), 65);
        assert_eq!(&f[..3], &[0x00, 0x12, 0x07]);
        // Omni receiver signature request, libratbag asus_omni_identify: 03 12 12 02.
        let f = frame(0x03, &packet(&[0x12, 0x12, 0x02]));
        assert_eq!(f.len(), 64);
        assert_eq!(&f[..4], &[0x03, 0x12, 0x12, 0x02]);
        // Pelta: CC 12 07, 64 bytes total.
        assert_eq!(&frame(0xcc, &packet(GET_BATTERY))[..3], &[0xcc, 0x12, 0x07]);

        assert_eq!(unframe(0x03, &[0x03, 0x12, 0x07, 0, 0, 80]).unwrap()[4], 80);
        assert_eq!(unframe(0x03, &[0x01, 0x12, 0x07]), None);
        assert_eq!(
            unframe(0, &[0x12, 0x07, 0, 0, 80]).unwrap()[..2],
            [0x12, 0x07]
        );
    }

    #[test]
    fn replies_match_echo_or_error() {
        let req = packet(&[0x12, 0x04, 0x02]);
        assert_eq!(classify(&req, &reply(&[0x12, 0x04, 0x02, 0, 9])), Reply::Ok);
        assert_eq!(classify(&req, &reply(&[0x12, 0x04, 0x00])), Reply::Other);
        assert_eq!(classify(&req, &reply(&[0xff, 0xaa])), Reply::Rejected);
        let set = packet(&set_field(4, 3));
        assert_eq!(classify(&set, &reply(&[0x51, 0x31, 0x00])), Reply::Ok);
        assert_eq!(classify(&set, &reply(&[0x12, 0x07])), Reply::Other);
    }

    #[test]
    fn settings_layout_follows_stage_count() {
        // libratbag _asus_dpi4_data: pad[4], dpi[4] u16, rate, response, snapping.
        let mut r = [0u8; PACKET];
        r[..3].copy_from_slice(&[0x12, 0x04, 0x00]);
        r[4..12].copy_from_slice(&[0x07, 0x00, 0x0f, 0x00, 0x1f, 0x00, 0x3f, 0x00]);
        r[12] = 0x03;
        r[16] = 0x01;
        assert_eq!(parse_dpi(&r, 1, false, 100), 800);
        assert_eq!(parse_dpi(&r, 3, false, 50), 1600);
        assert_eq!(parse_rate(&r, 4), Some(1000));
        assert!(parse_snapping(&r, 4));
        // V2 firmware sets high bits in the rate byte.
        r[12] = 0x12;
        assert_eq!(parse_rate(&r, 4), Some(500));

        // _asus_dpi2_data: dpi[2], rate at 8, snapping at 12.
        let mut r = [0u8; PACKET];
        r[4..6].copy_from_slice(&[0x0f, 0x00]);
        r[8] = 0x02;
        r[12] = 0x01;
        assert_eq!(parse_dpi(&r, 1, false, 100), 1600);
        assert_eq!(parse_rate(&r, 2), Some(500));
        assert!(parse_snapping(&r, 2));

        // Per-axis: stage 2 X at byte 8 (G-Helper ParseDPI, 5 + 4i with report id).
        let mut r = [0u8; PACKET];
        r[8..10].copy_from_slice(&[0x1f, 0x00]);
        assert_eq!(parse_dpi(&r, 2, true, 50), 1600);

        let mut p = [0u8; PACKET];
        p[11] = 2;
        assert_eq!(parse_stage(&p, 4, false), 2);
        assert_eq!(parse_stage(&p, 4, true), 3);
        p[11] = 0;
        assert_eq!(parse_stage(&p, 4, false), 1);

        let mut c = [0u8; PACKET];
        c[7..10].copy_from_slice(&[1, 2, 3]);
        assert_eq!(parse_dpi_color(&c, 2), [1, 2, 3]);
    }

    #[test]
    fn mouse_write_commands() {
        // 1600 dpi, step 50 → 31; step 100 (double-DPI mice) → 15.
        assert_eq!(
            set_dpi(1, 1600, 50, None),
            vec![0x51, 0x31, 0x00, 0x00, 0x1f, 0x00]
        );
        assert_eq!(
            set_dpi(4, 1600, 100, None),
            vec![0x51, 0x31, 0x03, 0x00, 0x0f, 0x00]
        );
        assert_eq!(
            set_dpi(2, 26000, 50, Some([0xff, 0x00, 0x80])),
            vec![0x51, 0x31, 0x01, 0x00, 0x07, 0x02, 0xff, 0x00, 0x80]
        );
        // G-Helper: rate 51 31 04 00 <idx>, snapping 51 31 06 00 01 on 4-stage mice.
        assert_eq!(set_field(4, 3), [0x51, 0x31, 0x04, 0x00, 0x03]);
        assert_eq!(set_field(6, 1), [0x51, 0x31, 0x06, 0x00, 0x01]);
        assert_eq!(
            set_lod(Lod::Calibrated, true),
            vec![0x51, 0x35, 0xff, 0x00, 0xff, 0x01]
        );
        assert_eq!(
            set_lod(Lod::Plain, false),
            vec![0x51, 0x35, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            set_energy(0x03, 0x02),
            [0x51, 0x37, 0x00, 0x00, 0x03, 0x00, 0x02]
        );
        let mut lod = [0u8; PACKET];
        lod[7] = 1;
        assert!(parse_lod(Lod::Calibrated, &lod));
        assert!(!parse_lod(Lod::Plain, &lod));
    }

    #[test]
    fn led_packets_match_captures() {
        // G-Helper Pugio capture: 51 28 03 00 00 04 00 ff 40.
        assert_eq!(
            set_led(3, Effect::Static, 4, [0x00, 0xff, 0x40]),
            [0x51, 0x28, 0x03, 0x00, 0x00, 0x04, 0x00, 0xff, 0x40]
        );
        // G-Helper Strix Impact capture: 51 28 00 00 00 04 35 04 ff.
        assert_eq!(
            set_led(0, Effect::Static, 4, [0x35, 0x04, 0xff]),
            [0x51, 0x28, 0x00, 0x00, 0x00, 0x04, 0x35, 0x04, 0xff]
        );
        assert_eq!(
            set_led(1, Effect::Cycle, 100, [1, 2, 3]),
            [0x51, 0x28, 0x01, 0x00, 0x02, 0x64, 0, 0, 0]
        );
        assert_eq!(scale_brightness(100, 4), 4);
        assert_eq!(scale_brightness(50, 4), 2);
        assert_eq!(scale_brightness(0, 4), 0);
        assert_eq!(scale_brightness(37, 100), 37);
    }

    #[test]
    fn battery_and_auto_off() {
        // Gladius III Wireless: 12 07 echo, byte 4 = percent (rog-mouse-battery).
        let mut r = reply(&[0x12, 0x07, 0x00, 0x00, 85, 0x03, 0x14]);
        assert_eq!(
            parse_battery(Battery::Percent, &r),
            BatteryReport {
                percent: 85,
                charging: false,
                auto_off: 0x03,
                low_battery: 0x14
            }
        );
        r[4] = 3;
        r[9] = 1;
        let q = parse_battery(Battery::Quarters, &r);
        assert_eq!((q.percent, q.charging), (75, true));

        assert_eq!(auto_off_minutes(0xff), Some(0));
        assert_eq!(auto_off_minutes(3), Some(5));
        assert_eq!(auto_off_minutes(9), None);
        for m in [0, 1, 2, 3, 5, 10] {
            assert_eq!(auto_off_minutes(auto_off_wire(m).unwrap()), Some(m));
        }
        assert_eq!(auto_off_wire(4), None);
    }

    #[test]
    fn keyboard_effect_packets() {
        // Static red, full brightness (level 4 * 25).
        assert_eq!(
            kb_effect(Effect::Static, 30, 100, [0xff, 0x00, 0x00]),
            [0x51, 0x2c, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x02, 0xff, 0x00, 0x00]
        );
        assert_eq!(
            kb_effect(Effect::Breathing, 30, 50, [0x00, 0x00, 0xff]),
            [0x51, 0x2c, 0x01, 0x00, 0x1e, 0x32, 0x00, 0x00, 0x02, 0x00, 0x00, 0xff]
        );
        assert_eq!(
            kb_effect(Effect::Cycle, 0, 75, [1, 2, 3]),
            [0x51, 0x2c, 0x02, 0x00, 0x00, 0x4b, 0x00, 0x00, 0x02, 0, 0, 0]
        );
    }

    #[test]
    fn headset_power_and_writes() {
        // pelta-control: reply data starts at report byte 5 (payload byte 4);
        // power_info = sleep, battery, low-battery level, low-battery voice.
        let r = unframe(0xcc, &[0xcc, 0x12, 0x07, 0x00, 0x00, 10, 64, 20, 1]).unwrap();
        assert_eq!(
            hs_power(&r),
            HsPower {
                sleep: 10,
                battery: Some(64),
                low_battery: 20,
                low_battery_voice: 1
            }
        );
        // 0xFE = unknown (headset off).
        let r = unframe(0xcc, &[0xcc, 0x12, 0x07, 0x00, 0x00, 5, 0xfe]).unwrap();
        assert_eq!(hs_power(&r).battery, None);
        assert_eq!(
            &frame(0xcc, &packet(&hs_set_power(15, 20, 1)))[..8],
            &[0xcc, 0x51, 0x37, 0x00, 0x00, 15, 20, 1]
        );
        // pelta-control set_lighting: 51 28 00 00 eff bri R G B (G-Helper LightingPacket).
        assert_eq!(
            hs_led(2, 50, [0x80, 0x00, 0xff]),
            [0x51, 0x28, 0x00, 0x00, 0x02, 50, 0x80, 0x00, 0xff]
        );
        let r = unframe(0xcc, &[0xcc, 0x12, 0x03, 0, 0, 4, 50, 128, 0, 255]).unwrap();
        assert_eq!(hs_parse_led(&r), Some((4, 50, [128, 0, 255])));
        // pelta-control EFFECTS / G-Helper HeadsetLightingMode: 3 = strobing.
        let r = unframe(0xcc, &[0xcc, 0x12, 0x03, 0, 0, 3, 50]).unwrap();
        assert_eq!(hs_parse_led(&r), Some((HS_STROBE, 50, [0; 3])));
        let r = unframe(0xcc, &[0xcc, 0x12, 0x03, 0, 0, 5, 50]).unwrap();
        assert_eq!(hs_parse_led(&r), None);
        assert_eq!(hs_sidetone(true), [0x41, 0x11, 0x00, 0x00, 0x01]);
        assert_eq!(hs_sidetone_level(30), [0x61, 0x11, 0x00, 0x00, 20]);
        // pelta-control put(0x41, 0x02, on) / put(0x41, 0x0A, v); G-Helper the same.
        assert_eq!(
            &frame(0xcc, &packet(&hs_set_noise_reduction(true)))[..6],
            &[0xcc, 0x41, 0x02, 0x00, 0x00, 0x01]
        );
        assert_eq!(
            &frame(0xcc, &packet(&hs_set_voice_prompt(2)))[..6],
            &[0xcc, 0x41, 0x0a, 0x00, 0x00, 0x02]
        );
        // Both read noise reduction from `41 20` at raw byte 5.
        assert_eq!(
            &frame(0xcc, &packet(HS_NOISE_REDUCTION))[..3],
            &[0xcc, 0x41, 0x20]
        );
        let r = unframe(0xcc, &[0xcc, 0x41, 0x20, 0, 0, 1]).unwrap();
        assert_eq!(classify(&packet(HS_NOISE_REDUCTION), &r), Reply::Ok);
        assert_eq!(r[4], 1);
    }

    #[test]
    fn save_commands_match_sources() {
        // libratbag ASUS_CMD_SAVE 0x0350 (u16 LE); G-Helper {rid, 0x50, 0x03};
        // OpenRGB usb_save_buf {0x00, 0x50, 0x03}.
        assert_eq!(
            &frame(0, &packet(MOUSE_SAVE))[..4],
            &[0x00, 0x50, 0x03, 0x00]
        );
        assert_eq!(frame(0, &packet(MOUSE_SAVE)).len(), 65);
        // Omni receiver: libratbag shifts the payload behind report id 3.
        assert_eq!(
            &frame(OMNI_MOUSE_REPORT, &packet(MOUSE_SAVE))[..3],
            &[0x03, 0x50, 0x03]
        );
        // OpenRGB usb_save_buf[1..3] = 50 55; G-Helper {rid, 0x50, 0x55}.
        assert_eq!(&frame(0, &packet(KB_SAVE))[..4], &[0x00, 0x50, 0x55, 0x00]);
        // The device echoes the command; FF AA is a refusal.
        let req = packet(MOUSE_SAVE);
        assert_eq!(classify(&req, &reply(&[0x50, 0x03])), Reply::Ok);
        assert_eq!(classify(&req, &reply(&[0xff, 0xaa])), Reply::Rejected);
    }

    #[test]
    fn omni_pairing_list() {
        // G-Helper ResolveOmniMouse: report 01, PIDs at raw byte 5, 9, ... (LE).
        assert_eq!(
            &frame(OMNI_LIST_REPORT, &packet(OMNI_LIST))[..4],
            &[0x01, 0xa0, 0x00, 0x00]
        );
        let mut raw = vec![
            0x01, 0xa0, 0x00, 0x00, 0x00, 0x94, 0x1a, 0x01, 0x00, 0x85, 0x1a,
        ];
        raw.resize(64, 0);
        let r = unframe(OMNI_LIST_REPORT, &raw).unwrap();
        assert_eq!(parse_paired(&r), vec![0x1a94, 0x1a85]);
        assert!(parse_paired(&packet(&[])).is_empty());
        // A full list stops at the end of the 64-byte report.
        let full = [0x11u8; PACKET];
        assert_eq!(parse_paired(&full).len(), 14);
    }

    #[test]
    fn stages_and_debounce() {
        // G-Helper GetChangeDPIProfilePacket2: 51 31 09 00 <profile>; TUF M3 Gen II: profile - 1.
        assert_eq!(set_active_stage(3, false), [0x51, 0x31, 0x09, 0x00, 0x03]);
        assert_eq!(set_active_stage(3, true), [0x51, 0x31, 0x09, 0x00, 0x02]);
        // G-Helper GetUpdateDebouncePacket: 51 31 05 00 <i> on 4-stage mice
        // (libratbag dpi_count + ASUS_FIELD_RESPONSE), 51 31 03 on 2-stage ones.
        assert_eq!(
            set_field(4 + 1, debounce_wire(12)),
            [0x51, 0x31, 0x05, 0x00, 0x02]
        );
        assert_eq!(debounce_wire(32), 7);
        let mut r = [0u8; PACKET];
        r[14] = 0x03; // G-Helper ParseDebounce packet[15] (report id at 0)
        assert_eq!(parse_debounce(&r, 4), 16);
        r[10] = 0x07; // 2-stage: packet[11]
        assert_eq!(parse_debounce(&r, 2), 32);
        for ms in DEBOUNCE_MS {
            let mut r = [0u8; PACKET];
            r[14] = debounce_wire(ms);
            assert_eq!(parse_debounce(&r, 4), ms);
        }
        // Strix Carry: battery quarters at byte 6.
        let b = parse_battery(Battery::QuartersAt6, &packet(&[0x12, 0x07, 0, 0, 0, 0, 3]));
        assert_eq!(b.percent, 75);
    }

    #[test]
    fn keyboard_flat_and_battery() {
        // OpenRGB non-per-LED keyboards / G-Helper TUFK1: RGB at raw bytes 9..11.
        let f = frame(
            0,
            &packet(&kb_effect_flat(Effect::Breathing, 2, 100, [1, 2, 3])),
        );
        assert_eq!(
            &f[..12],
            &[0x00, 0x51, 0x2c, 0x01, 0x00, 0x02, 0x64, 0x00, 0x00, 1, 2, 3]
        );
        // G-Helper: 12 01 reply [6] = battery, [9] = charging (report id at 0).
        let r = unframe(0, &[0x12, 0x01, 0, 0, 0, 77, 0, 0, 1, 0, 55]).unwrap();
        assert_eq!(kb_battery(&r, 5), (77, true));
        assert_eq!(kb_battery(&r, 10), (55, true));
    }
}
