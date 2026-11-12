//! Wooting keyboards (VID 0x31e3; the original One / Two on legacy firmware
//! use Atmel's 0x03eb): whole-board colour and brightness through the RGB
//! SDK's direct mode, or the keyboard's own profile colours.
//!
//! Reimplemented from Wooting's official RGB SDK (MPL-2.0), commit 451f40d:
//! <https://github.com/WootingKb/wooting-rgb-sdk>. `src/wooting-usb.c`: PIDs
//! and their gamepad-mode alternates (`base | 1`, `base | 2`), the config
//! interface (usage page 0x1337, or 0xFF55 for multi-report firmware), the
//! 8-byte feature command and its reply, packet-size detection from the
//! report descriptor, the v1 / v2 / v3 colour reports and the v1 CRC.
//! `src/wooting-usb.h`: command ids, matrix size. `src/wooting-rgb-sdk.c`:
//! RGB565 encoding, the v1 LED-driver layout and gamma table (`gamma.rs`,
//! MPL-2.0), and `wooting_rgb_reset_rgb` (back to the profile colours).
//! Only that gamma table is copied; everything else is written here.
//!
//! The protocol has no brightness command, so brightness scales the colour.
//! Direct-mode colours are volatile and stay until reset or replug, so
//! `tick` does nothing; the daemon re-applies them on reconnect. Choosing
//! "Onboard profile", or stopping the daemon, sends the SDK's reset command.
//! Nothing is written to the keyboard's memory (see
//! `docs/onboard-writes/wooting.md`).

mod gamma;

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{enumerate_hidraw, usage_pages, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use std::time::Duration;
use tracing::{info, warn};

const VID: u16 = 0x31e3;
/// Atmel's VID, used by Wooting's first firmware (usb.ids: `03eb:ff01`
/// WootingOne, `03eb:ff02` WootingTwo). Only a node with Wooting's config
/// usage page is opened, so other Atmel devices are left alone.
const LEGACY_VID: u16 = 0x03eb;

const CFG_PAGE: u16 = 0x1337;
const CFG_V3_PAGE: u16 = 0xff55;

const RAW_COLORS: u8 = 11;
const RESET_ALL: u8 = 32;
const COLOR_INIT: u8 = 33;

/// The SDK's colour matrix.
const ROWS: usize = 6;
const COLS: usize = 21;

/// `WOOTING_READ_RESPONSE_TIMEOUT`.
const REPLY_WAIT: Duration = Duration::from_millis(1000);

const fn model(vendor_id: u16, name: &'static str, product_ids: &'static [u16]) -> Model {
    Model {
        vendor_id,
        product_ids,
        kind: DeviceKind::Keyboard,
        name,
        usbfs: false,
        multi: false,
        open,
    }
}

/// Every board may also enumerate as `base | 1` / `base | 2` (gamepad modes).
pub const MODELS: &[Model] = &[
    model(VID, "Wooting 60HE", &[0x1300, 0x1301, 0x1302]),
    model(VID, "Wooting 60HE (ARM)", &[0x1310, 0x1311, 0x1312]),
    model(VID, "Wooting 60HE+", &[0x1320, 0x1321, 0x1322]),
    model(VID, "Wooting 60HE V2", &[0x1340, 0x1341, 0x1342]),
    model(VID, "Wooting 80HE", &[0x1400, 0x1401, 0x1402]),
    model(VID, "Wooting Two HE", &[0x1220, 0x1221, 0x1222]),
    model(VID, "Wooting Two HE (ARM)", &[0x1230, 0x1231, 0x1232]),
    model(VID, "Wooting Two Lekker Edition", &[0x1210, 0x1211, 0x1212]),
    model(VID, "Wooting One", &[0x1100, 0x1101, 0x1102]),
    model(VID, "Wooting Two", &[0x1200, 0x1201, 0x1202]),
    model(VID, "Wooting UwU RGB", &[0x1510, 0x1511, 0x1512]),
    model(LEGACY_VID, "Wooting One (legacy firmware)", &[0xff01]),
    model(LEGACY_VID, "Wooting Two (legacy firmware)", &[0xff02]),
];

/// Matrix columns the SDK fills for a model (`max_columns`), and whether it
/// uses 64-byte packets when the report descriptor cannot be read
/// (`uses_small_packets` in its `set_meta_wooting_*`).
fn layout(canonical: u16) -> Option<(usize, bool)> {
    Some(match canonical {
        0x1100 | 0xff01 => (17, false),
        0x1200 | 0x1210 | 0x1220 | 0xff02 => (21, false),
        0x1230 => (21, true),
        0x1300 | 0x1340 => (14, false),
        0x1310 | 0x1320 => (14, true),
        0x1400 => (17, false),
        0x1510 => (7, true),
        _ => return None,
    })
}

/// Colour report format (`wooting_rgb_array_update_keyboard`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proto {
    /// Legacy firmware: one 129-byte report per LED-driver half, with CRC.
    /// `two` = full size (five parts; the One has four).
    V1 { two: bool },
    /// One 257-byte report, or four 65-byte ones (`small`).
    V2 { small: bool },
    /// Multi-report firmware (usage page 0xFF55): report 5, 2047 bytes.
    V3,
}

impl Proto {
    /// Reply length the SDK reads after each command.
    fn reply_len(self) -> usize {
        match self {
            Proto::V1 { .. } => 128,
            Proto::V2 { .. } => 256,
            Proto::V3 => 2046,
        }
    }
}

/// The SDK's packet-size check: the first Report Count prefix in the
/// descriptor is `95` (one-byte count, 64-byte packets) or `96` (256).
fn small_packets(desc: &[u8]) -> Option<bool> {
    desc.iter().find_map(|&b| match b {
        0x95 => Some(true),
        0x96 => Some(false),
        _ => None,
    })
}

/// Feature report for a parameterless command (`wooting_usb_send_feature_buff`).
fn command(proto: Proto, cmd: u8) -> [u8; 8] {
    if proto == Proto::V3 {
        [1, 0xd1, 0xda, cmd, 0, 0, 0, 0]
    } else {
        [0, 0xd0, 0xda, cmd, 0, 0, 0, 0]
    }
}

/// `encodeColor`: RGB565.
fn rgb565([r, g, b]: [u8; 3]) -> u16 {
    (u16::from(r & 0xf8) << 8) | (u16::from(g & 0xfc) << 3) | (u16::from(b) >> 3)
}

/// CRC-16/CCITT, init 0 (`getCrc16ccitt`).
fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |mut crc, &b| {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
        crc
    })
}

/// LED indices below the last one that no matrix key maps to (derived from
/// the SDK's `rgb_led_index` table, with the ISO enter / left shift copies).
const NO_LED: [usize; 5] = [50, 56, 60, 64, 69];

/// Every write that shows `rgb` on the whole board.
fn frames(proto: Proto, cols: usize, rgb: [u8; 3]) -> Vec<Vec<u8>> {
    let matrix = |buf: &mut [u8]| {
        let word = rgb565(rgb).to_le_bytes();
        for row in 0..ROWS {
            for col in 0..cols {
                let at = 2 * (row * COLS + col);
                buf[at..at + 2].copy_from_slice(&word);
            }
        }
    };
    match proto {
        Proto::V1 { two } => v1_reports(two, rgb),
        Proto::V2 { small } => {
            let mut r = vec![0u8; 257];
            r[1..4].copy_from_slice(&[0xd0, 0xda, RAW_COLORS]);
            matrix(&mut r[4..]);
            if small {
                r[1..].chunks(64).map(|c| [&[0], c].concat()).collect()
            } else {
                vec![r]
            }
        }
        Proto::V3 => {
            let mut r = vec![0u8; 2047];
            r[..4].copy_from_slice(&[5, 0xd1, 0xda, RAW_COLORS]);
            matrix(&mut r[4..]);
            vec![r]
        }
    }
}

/// `wooting_rgb_build_v1_buffers` + `wooting_usb_send_buffer_v1`: 24 LEDs per
/// 96-byte part; LED `i` sits at `[0x00, 0x08, 0x30, 0x38][i / 6] + i % 6` of
/// its part, with green +0x10 and blue +0x20. Part `p` goes to LED driver
/// `p / 2` at register `p % 2 * 96`.
fn v1_reports(two: bool, rgb: [u8; 3]) -> Vec<Vec<u8>> {
    let (last_led, parts) = if two { (116, 5) } else { (95, 4) };
    // The SDK stores RGB565 and decodes it before the gamma lookup.
    let [r, g, b] = [rgb[0] & 0xf8, rgb[1] & 0xfc, rgb[2] & 0xf8].map(|c| gamma::GAMMA[c as usize]);
    let mut data = vec![[0u8; 96]; parts];
    for led in (0..=last_led).filter(|l| !NO_LED.contains(l)) {
        let i = led % 24;
        let at = [0x00, 0x08, 0x30, 0x38][i / 6] + i % 6;
        let part = &mut data[led / 24];
        part[at] = r;
        part[at + 0x10] = g;
        part[at + 0x20] = b;
    }
    data.iter()
        .enumerate()
        .map(|(p, d)| {
            let mut rep = vec![0u8; 129];
            rep[1..6].copy_from_slice(&[0xd0, 0xda, RAW_COLORS, (p / 2) as u8, (p % 2 * 96) as u8]);
            rep[6..102].copy_from_slice(d);
            let crc = crc16(&rep[..127]);
            rep[127..].copy_from_slice(&crc.to_le_bytes());
            rep
        })
        .collect()
}

const EFFECTS: [(&str, &str); 2] = [("onboard", "Onboard profile"), ("static", "Static")];

fn controls(effect: &str) -> Vec<SettingMeta> {
    use SettingKey as K;
    let direct = effect == "static";
    let mut c = vec![
        SettingMeta::dropdown(
            K::Effect,
            "Effect",
            EFFECTS
                .iter()
                .map(|(v, l)| SettingOption::text(v, l))
                .collect(),
        ),
        SettingMeta::slider(K::Brightness, "Brightness", 0.0, 100.0, 1.0)
            .unit("%")
            .hidden(!direct),
    ];
    for (key, label) in [
        (K::ColorR, "Red"),
        (K::ColorG, "Green"),
        (K::ColorB, "Blue"),
    ] {
        c.push(SettingMeta::color(key, label).hidden(!direct));
    }
    c
}

fn defaults(s: &mut SettingsMap) {
    for (k, v) in [
        (SettingKey::Effect, SettingValue::Text("onboard".into())),
        (SettingKey::Brightness, SettingValue::Int(100)),
        (SettingKey::ColorR, SettingValue::Int(255)),
        (SettingKey::ColorG, SettingValue::Int(255)),
        (SettingKey::ColorB, SettingValue::Int(255)),
    ] {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

/// The colour to send: the chosen colour scaled by brightness.
fn color(s: &SettingsMap) -> [u8; 3] {
    let int = |k| {
        get_setting(s, k)
            .and_then(|v| v.as_int())
            .unwrap_or(255)
            .clamp(0, 255)
    };
    let pct = int(SettingKey::Brightness).min(100);
    [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB].map(|k| (int(k) * pct / 100) as u8)
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let vid = ctx.vendor_id;
    let model = MODELS
        .iter()
        .find(|m| m.vendor_id == vid && m.product_ids.contains(&pid))
        .with_context(|| format!("no Wooting model for {vid:04x}:{pid:04x}"))?;
    let canonical = model.product_ids[0];
    let (cols, small) = layout(canonical).context("no Wooting layout")?;
    let found = enumerate_hidraw()?
        .into_iter()
        .filter(|n| n.vendor_id == vid && n.product_id == pid)
        .find_map(|n| {
            let desc = n.report_descriptor();
            let pages = usage_pages(&desc);
            let proto = if pages.contains(&CFG_V3_PAGE) {
                Proto::V3
            } else if pages.contains(&CFG_PAGE) {
                if vid == LEGACY_VID {
                    Proto::V1 { two: cols == COLS }
                } else {
                    Proto::V2 {
                        small: small_packets(&desc).unwrap_or(small),
                    }
                }
            } else {
                return None;
            };
            Some((n, proto))
        });
    let Some((info, proto)) = found else {
        if vid == LEGACY_VID {
            // Another Atmel-based device on the same IDs.
            return Ok(vec![]);
        }
        bail!("no config interface (usage page 0x1337 / 0xFF55)");
    };
    let dev = HidrawDevice::open(&info.path)?;
    let mut settings = hako_core::config::load_device_settings(&ctx.paths, vid, canonical)?;
    defaults(&mut settings);
    let mut a = Wooting {
        name: model.name,
        vid,
        canonical,
        proto,
        cols,
        id: DeviceId {
            vendor_id: vid,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
        dev: Some(dev),
        direct: false,
        settings,
        last_error: None,
        paths: ctx.paths.clone(),
    };
    if let Err(e) = a.apply() {
        warn!("{}: restore lighting: {e}", model.name);
        a.last_error = Some(e.to_string());
    }
    info!(
        "{} open via {} ({proto:?})",
        model.name,
        info.path.display()
    );
    Ok(vec![Box::new(a)])
}

struct Wooting {
    name: &'static str,
    vid: u16,
    /// Names the settings file.
    canonical: u16,
    proto: Proto,
    cols: usize,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    /// Direct mode is on (init sent, no reset since).
    direct: bool,
    settings: SettingsMap,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl Wooting {
    fn effect(&self) -> &str {
        get_setting(&self.settings, SettingKey::Effect)
            .and_then(|v| v.as_str())
            .unwrap_or("onboard")
    }

    /// Send a command and wait for its reply (contents unused), as
    /// `wooting_usb_send_feature` does.
    fn command(&mut self, cmd: u8) -> Result<()> {
        let dev = self.dev.as_mut().context("not connected")?;
        dev.drain(Duration::ZERO);
        dev.set_feature(&mut command(self.proto, cmd))?;
        let mut reply = vec![0u8; self.proto.reply_len()];
        let mut got = 0;
        // Multi-report replies are one report; older ones may span several.
        while got < reply.len() {
            got += dev.read_timeout(&mut reply[got..], REPLY_WAIT)?;
            if self.proto == Proto::V3 {
                break;
            }
        }
        Ok(())
    }

    fn apply(&mut self) -> Result<()> {
        if self.effect() == "static" {
            if !self.direct {
                self.command(COLOR_INIT)?;
                self.direct = true;
            }
            let frames = frames(self.proto, self.cols, color(&self.settings));
            let dev = self.dev.as_mut().context("not connected")?;
            for f in frames {
                dev.write(&f)?;
            }
        } else if self.direct {
            self.command(RESET_ALL)?;
            self.direct = false;
        }
        Ok(())
    }
}

impl DeviceAdapter for Wooting {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Keyboard,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls: controls(self.effect()),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value);
        if let Err(e) = self.apply() {
            // The keyboard kept its old look; so do the UI and the file.
            self.settings = before;
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        self.last_error = None;
        hako_core::config::save_device_settings(
            &self.paths,
            self.vid,
            self.canonical,
            &self.settings,
        )
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    /// Lighting stays as last written (AGENTS.md hard rule 3); choosing
    /// "Onboard profile" is the way back to the profile colours.
    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_model_has_a_layout_and_gamepad_alternates() {
        let mut seen = HashSet::new();
        for m in MODELS {
            assert!(layout(m.product_ids[0]).is_some(), "{}", m.name);
            for &pid in m.product_ids {
                assert!(seen.insert((m.vendor_id, pid)), "{pid:04x} listed twice");
            }
            if m.vendor_id == VID {
                let base = m.product_ids[0];
                assert_eq!(m.product_ids, [base, base | 1, base | 2], "{}", m.name);
            }
        }
    }

    #[test]
    fn crc_is_ccitt_init_zero() {
        // CRC-16/XMODEM check value.
        assert_eq!(crc16(b"123456789"), 0x31c3);
        assert_eq!(crc16(&[]), 0);
    }

    #[test]
    fn colors_encode_as_rgb565() {
        assert_eq!(rgb565([255, 0, 0]), 0xf800);
        assert_eq!(rgb565([0, 255, 0]), 0x07e0);
        assert_eq!(rgb565([0, 0, 255]), 0x001f);
        assert_eq!(rgb565([0x12, 0x34, 0x56]), 0x11aa);
    }

    #[test]
    fn commands() {
        assert_eq!(
            command(Proto::V2 { small: false }, COLOR_INIT),
            [0, 0xd0, 0xda, 33, 0, 0, 0, 0]
        );
        assert_eq!(
            command(Proto::V1 { two: true }, RESET_ALL),
            [0, 0xd0, 0xda, 32, 0, 0, 0, 0]
        );
        assert_eq!(
            command(Proto::V3, RESET_ALL),
            [1, 0xd1, 0xda, 32, 0, 0, 0, 0]
        );
        assert_eq!(Proto::V3.reply_len(), 2046);
    }

    #[test]
    fn packet_size_from_descriptor() {
        // Vendor page 0x1337, 8-bit size, count 64 / 256.
        assert_eq!(
            small_packets(&[0x06, 0x37, 0x13, 0x75, 0x08, 0x95, 0x40]),
            Some(true)
        );
        assert_eq!(
            small_packets(&[0x06, 0x37, 0x13, 0x75, 0x08, 0x96, 0x00, 0x01]),
            Some(false)
        );
        assert_eq!(small_packets(&[]), None);
    }

    #[test]
    fn v2_report_holds_the_matrix() {
        let f = frames(Proto::V2 { small: false }, 14, [255, 0, 0]);
        assert_eq!(f.len(), 1);
        let r = &f[0];
        assert_eq!(r.len(), 257);
        assert_eq!(r[..6], [0, 0xd0, 0xda, 11, 0x00, 0xf8]);
        let word = |row: usize, col: usize| {
            let at = 4 + 2 * (row * COLS + col);
            u16::from_le_bytes([r[at], r[at + 1]])
        };
        assert_eq!(word(5, 13), 0xf800);
        assert_eq!(word(0, 14), 0, "columns past the model stay dark");
        assert!(r[4 + 2 * ROWS * COLS..].iter().all(|&b| b == 0));

        let small = frames(Proto::V2 { small: true }, 14, [255, 0, 0]);
        assert_eq!(small.len(), 4);
        assert!(small.iter().all(|c| c.len() == 65 && c[0] == 0));
        let joined: Vec<u8> = small.iter().flat_map(|c| c[1..].to_vec()).collect();
        assert_eq!(joined, r[1..]);
    }

    #[test]
    fn v3_report_is_report_5() {
        let f = frames(Proto::V3, 17, [0, 0, 255]);
        assert_eq!(f.len(), 1);
        let r = &f[0];
        assert_eq!(r.len(), 2047);
        assert_eq!(r[..6], [5, 0xd1, 0xda, 11, 0x1f, 0x00]);
        assert_eq!(
            r[4..256],
            frames(Proto::V2 { small: false }, 17, [0, 0, 255])[0][4..256]
        );
        assert!(r[256..].iter().all(|&b| b == 0));
    }

    #[test]
    fn v1_reports_carry_parts_and_crc() {
        let one = frames(Proto::V1 { two: false }, 17, [255, 255, 255]);
        assert_eq!(one.len(), 4, "the One has no fifth part");
        let two = frames(Proto::V1 { two: true }, 21, [255, 255, 255]);
        assert_eq!(two.len(), 5);
        for (p, r) in two.iter().enumerate() {
            assert_eq!(r.len(), 129);
            let head = [0, 0xd0, 0xda, 11, (p / 2) as u8, (p % 2 * 96) as u8];
            assert_eq!(r[..6], head);
            assert_eq!(u16::from_le_bytes([r[127], r[128]]), crc16(&r[..127]));
        }
        // White: 0xf8 / 0xfc after RGB565, gamma 236 / 247.
        assert_eq!(
            (two[0][6], two[0][6 + 0x10], two[0][6 + 0x20]),
            (236, 247, 236)
        );
        // LED 50 (part 2, slot 2) is unmapped; LED 51 is lit.
        assert_eq!(two[2][6 + 2], 0);
        assert_eq!(two[2][6 + 3], 236);
    }

    #[test]
    fn v1_crc_matches_the_sdk() {
        // CRC bytes the SDK's own `wooting_rgb_array_update_keyboard` writes
        // for a white Wooting Two (built against stubbed hidapi).
        let crcs: Vec<[u8; 2]> = frames(Proto::V1 { two: true }, 21, [255, 255, 255])
            .iter()
            .map(|r| [r[127], r[128]])
            .collect();
        assert_eq!(
            crcs,
            [
                [0x09, 0xda],
                [0xd0, 0xf6],
                [0x23, 0x99],
                [0x77, 0xdb],
                [0x60, 0xf8]
            ]
        );
    }

    #[test]
    fn controls_follow_effect() {
        let mut s = SettingsMap::new();
        defaults(&mut s);
        for (e, _) in EFFECTS {
            merge_setting(&mut s, SettingKey::Effect, SettingValue::Text(e.into()));
            for c in controls(e) {
                assert!(!c.persistent);
                if let Some(v) = get_setting(&s, c.key) {
                    assert!(c.validate(v).is_ok(), "{e} {:?}", c.key);
                }
            }
        }
        assert!(controls("onboard")
            .iter()
            .filter(|c| c.key != SettingKey::Effect)
            .all(|c| c.hidden));
        assert!(controls("static").iter().all(|c| !c.hidden));
        merge_setting(&mut s, SettingKey::Brightness, SettingValue::Int(50));
        merge_setting(&mut s, SettingKey::ColorG, SettingValue::Int(100));
        assert_eq!(color(&s), [127, 50, 127]);
    }
}
