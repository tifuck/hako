//! ASTRO A50 base stations: Gen 5 (`046d:0b1c`) and Gen 4 (`9886:002c`, PC
//! mode; in console mode it enumerates as `9886:002b` with no HID). Neither
//! speaks HID++: both take 64-byte reports with id 0x02 on their usage page
//! 0xFF32 node (Gen 5 interface 8, Gen 4 interface 6).
//!
//! Gen 5, reimplemented from HeadsetControl `lib/devices/logitech_astro_a50.hpp`
//! (no code copied): request `02 0C <3 + n> 00 <cmd> <handle> <payload n>`;
//! a reply repeats the cmd with a payload from byte 6 (bare acks carry none,
//! and the base also pushes frames unsolicited, so replies match on cmd).
//! - Battery cmd 0x06: `[percent, _, docked/charging]`.
//! - ChatMix cmd 0x0A: `[0 (chat) ..= 12 (game)]`, read only.
//! - Volume cmd 0x08: read `[raw, 0, level 0..=21]`; write handle 0x1B
//!   `[FF, level]`.
//! - Sidetone cmd 0x09 handle 0x1C `[01, FF, level 0..=6]`; base LED cmd
//!   0x0F handle 0x1C `[brightness 0..=100]`; noise gate cmd 0x14 handle 0x2D
//!   `[0 off / 1 night / 4 tournament]`. These three cannot be read back.
//!
//! Gen 4, from eh-fifty (MIT, `eh_fifty.py` + README protocol notes) and
//! HeadsetControl `lib/devices/logitech_astro_a50_gen4.hpp`, which agree on
//! every layout used here: request `02 <cmd> [<n> <payload n>]`, one reply
//! `02 <status> <n> <payload>` (status 2 = data, 0 = accepted, 1 = error).
//! - Headset status 0x54: bit 0 docked, bit 1 on (battery is only read then).
//! - Battery 0x7C: bit 7 charging, bits 0-6 percent.
//! - Sliders: set 0x62 `[slider, 0..=100]`, get 0x68 `[slider]` →
//!   `[68, slider, active, saved]`; slider 4 = mic, 5 = sidetone.
//! - Noise gate: set 0x64 `[mode]` (echoed), get 0x6A → `[6A, active, saved]`;
//!   0 streaming, 1 night, 2 home, 3 tournament.
//! - EQ preset: set 0x67 `[1..=3]` → `[67, preset]`, get 0x6C → `[preset]`.
//!
//! Every write here changes the active value only. Gen 4's SAVE_VALUES
//! (0x61) would store them on the base: shown blocked (see
//! `docs/onboard-writes/logitech.md`). Not implemented: the EQ bands of
//! either generation (a write replaces all bands at once), Gen 4 balance
//! (eh-fifty and HeadsetControl disagree on its direction).

use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{enumerate_hidraw, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::warn;

pub const GEN4_VID: u16 = 0x9886;
pub const GEN4_PID: u16 = 0x002c;
pub const GEN5_PID: u16 = 0x0b1c;
const FRAME: usize = 64;
const REPORT_ID: u8 = 0x02;
const POLL_EVERY: Duration = Duration::from_secs(15);
const REPLY_WAIT: Duration = Duration::from_millis(1000);

const G5_MARKER: u8 = 0x0c;
const G5_BATTERY: u8 = 0x06;
const G5_VOLUME: u8 = 0x08;
const G5_SIDETONE: u8 = 0x09;
const G5_CHATMIX: u8 = 0x0a;
const G5_LED: u8 = 0x0f;
const G5_NOISE_GATE: u8 = 0x14;
const G5_CHATMIX_MAX: u8 = 12;
const G5_VOLUME_MAX: u8 = 21;
const G5_SIDETONE_MAX: u8 = 6;

const G4_STATUS: u8 = 0x54;
const G4_SET_SLIDER: u8 = 0x62;
const G4_SET_NOISE_GATE: u8 = 0x64;
const G4_SET_EQ_PRESET: u8 = 0x67;
const G4_GET_SLIDER: u8 = 0x68;
const G4_GET_NOISE_GATE: u8 = 0x6a;
const G4_GET_EQ_PRESET: u8 = 0x6c;
const G4_BATTERY: u8 = 0x7c;
const G4_SLIDER_MIC: u8 = 0x04;
const G4_SLIDER_SIDETONE: u8 = 0x05;

/// Why Gen 4's SAVE_VALUES (0x61) is not offered.
const BLOCK_SAVE: &str = "base-station save (0x61): only eh-fifty sends it; \
     HeadsetControl documents it but never does, and the stored layout is unverified";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gen {
    Four,
    Five,
}

// ── Gen 5 frames ─────────────────────────────────────────────────────────────

fn g5_request(cmd: u8, handle: u8, payload: &[u8]) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    f[..6].copy_from_slice(&[
        REPORT_ID,
        G5_MARKER,
        3 + payload.len() as u8,
        0x00,
        cmd,
        handle,
    ]);
    f[6..6 + payload.len()].copy_from_slice(payload);
    f
}

/// Payload of a frame answering `cmd` (None: other cmd, push of another
/// property, or a bare ack).
fn g5_reply(f: &[u8], cmd: u8) -> Option<&[u8]> {
    if f.len() < 7 || f[0] != REPORT_ID || f[1] != G5_MARKER || f[4] != cmd || f[2] <= 3 {
        return None;
    }
    let end = (3 + usize::from(f[2])).min(f.len());
    Some(&f[6..end])
}

/// ChatMix 0 (chat) ..= 12 (game) → (game, chat) levels, each full up to
/// the centre (HeadsetControl's split).
fn g5_chatmix(raw: u8) -> (f64, f64) {
    let r = f64::from(raw.min(G5_CHATMIX_MAX));
    let half = f64::from(G5_CHATMIX_MAX) / 2.0;
    (
        (r / half).min(1.0),
        ((f64::from(G5_CHATMIX_MAX) - r) / half).min(1.0),
    )
}

/// Gen 5 write for `key`: (cmd, handle, payload).
fn g5_write(key: SettingKey, v: u8) -> Option<(u8, u8, Vec<u8>)> {
    Some(match key {
        SettingKey::Volume => (G5_VOLUME, 0x1b, vec![0xff, v]),
        SettingKey::MicSideTone => (G5_SIDETONE, 0x1c, vec![0x01, 0xff, v]),
        SettingKey::Brightness => (G5_LED, 0x1c, vec![v]),
        SettingKey::MicNoiseFilter => (G5_NOISE_GATE, 0x2d, vec![v]),
        _ => return None,
    })
}

// ── Gen 4 frames ─────────────────────────────────────────────────────────────

fn g4_request(cmd: u8, payload: &[u8]) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    f[0] = REPORT_ID;
    f[1] = cmd;
    if !payload.is_empty() {
        f[2] = payload.len() as u8;
        f[3..3 + payload.len()].copy_from_slice(payload);
    }
    f
}

/// Payload of a reply; the length byte is capped (firmware misreports it).
fn g4_reply(f: &[u8]) -> Result<&[u8]> {
    if f.len() < 3 || f[0] != REPORT_ID {
        bail!("ASTRO A50: unexpected reply");
    }
    let end = (3 + usize::from(f[2])).min(f.len());
    match f[1] {
        0x02 => Ok(&f[3..end]),
        0x00 => Ok(&[]),
        0x01 => {
            let name: String = f
                .get(7..)
                .unwrap_or_default()
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| {
                    if (0x20..0x7f).contains(&c) {
                        c as char
                    } else {
                        '?'
                    }
                })
                .collect();
            bail!("ASTRO A50: {name}")
        }
        s => bail!("ASTRO A50: unknown reply status {s:#x}"),
    }
}

/// (percent, charging) from the 0x7C byte.
fn g4_battery(raw: u8) -> (u8, bool) {
    ((raw & 0x7f).min(100), raw & 0x80 != 0)
}

fn g4_slider(key: SettingKey) -> Option<u8> {
    match key {
        SettingKey::MicVolume => Some(G4_SLIDER_MIC),
        SettingKey::MicSideTone => Some(G4_SLIDER_SIDETONE),
        _ => None,
    }
}

fn controls(gen: Gen) -> Vec<SettingMeta> {
    use SettingKey::*;
    let opts = |o: &[(i64, &str)]| o.iter().map(|&(v, l)| SettingOption::int(v, l)).collect();
    match gen {
        Gen::Five => vec![
            SettingMeta::readonly(ChatMixGame, "Game", 0.0, 1.0),
            SettingMeta::readonly(ChatMixChat, "Chat", 0.0, 1.0),
            SettingMeta::slider(Volume, "Headset volume", 0.0, G5_VOLUME_MAX.into(), 1.0),
            SettingMeta::slider(MicSideTone, "Sidetone", 0.0, G5_SIDETONE_MAX.into(), 1.0),
            SettingMeta::dropdown(
                MicNoiseFilter,
                "Noise gate",
                opts(&[(0, "Off"), (1, "Night"), (4, "Tournament")]),
            ),
            SettingMeta::slider(Brightness, "Base station LED", 0.0, 100.0, 1.0).unit("%"),
        ],
        Gen::Four => [
            SettingMeta::slider(MicVolume, "Microphone", 0.0, 100.0, 1.0).unit("%"),
            SettingMeta::slider(MicSideTone, "Sidetone", 0.0, 100.0, 1.0).unit("%"),
            SettingMeta::dropdown(
                MicNoiseFilter,
                "Noise gate",
                opts(&[
                    (0, "Streaming"),
                    (1, "Night"),
                    (2, "Home"),
                    (3, "Tournament"),
                ]),
            ),
            SettingMeta::dropdown(
                EqPreset,
                "EQ preset",
                opts(&[(1, "Preset 1"), (2, "Preset 2"), (3, "Preset 3")]),
            ),
        ]
        .into_iter()
        .chain(super::device::memory_placeholders(Some(BLOCK_SAVE), None))
        .collect(),
    }
}

// ── Adapter ──────────────────────────────────────────────────────────────────

pub fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let (gen, vid) = if pid == GEN5_PID {
        (Gen::Five, super::device::VID)
    } else {
        (Gen::Four, GEN4_VID)
    };
    let mut nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .filter(|d| crate::hid::usage_pages(&d.report_descriptor()).contains(&0xff32))
        .collect();
    nodes.sort_by_key(|d| d.interface);
    let info = nodes.first().context("no ASTRO (0xFF32) interface")?;
    let dev = HidrawDevice::open(&info.path)?;
    let chosen = hako_core::config::load_device_settings(&ctx.paths, vid, pid)?;
    let name = super::MODELS
        .iter()
        .find(|m| m.vendor_id == vid && m.product_ids.contains(&pid))
        .map_or("ASTRO A50", |m| m.name);
    let mut a = Astro {
        id: DeviceId {
            vendor_id: vid,
            product_id: pid,
            instance: "astro".into(),
        },
        gen,
        name,
        dev: Some(dev),
        paths: ctx.paths.clone(),
        settings: chosen.clone(),
        chosen,
        battery: BatteryState {
            label: "Headset".into(),
            percent: None,
            charging: false,
            present: true,
        },
        stale: false,
        last_poll: Instant::now(),
        last_error: None,
    };
    for c in controls(gen) {
        if let Some(v) = get_setting(&a.chosen, c.key).and_then(|v| v.as_int()) {
            if let Err(e) = a.write(c.key, v as u8) {
                warn!("{}: restore {}: {e}", a.name, c.key.as_str());
            }
        }
    }
    a.poll();
    Ok(vec![Box::new(a)])
}

struct Astro {
    id: DeviceId,
    gen: Gen,
    name: &'static str,
    dev: Option<HidrawDevice>,
    paths: ConfigPaths,
    settings: SettingsMap,
    chosen: SettingsMap,
    battery: BatteryState,
    /// Gen 4: a reply may still be in flight after a timeout.
    stale: bool,
    last_poll: Instant,
    last_error: Option<String>,
}

impl Astro {
    /// Write `f` and return the first frame `accept` takes (bounded).
    fn transact(&mut self, f: &[u8; FRAME], accept: impl Fn(&[u8]) -> bool) -> Result<Vec<u8>> {
        let flush = if std::mem::take(&mut self.stale) {
            REPLY_WAIT
        } else {
            Duration::from_millis(1)
        };
        let dev = self.dev.as_mut().context("device closed")?;
        dev.drain(flush);
        dev.write(f)?;
        let deadline = Instant::now() + REPLY_WAIT;
        let mut buf = [0u8; FRAME];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.stale = true;
                bail!("ASTRO A50: no reply to {:#04x}", f[1]);
            }
            if let Ok(n) = dev.read_timeout(&mut buf, left) {
                if accept(&buf[..n]) {
                    return Ok(buf[..n].to_vec());
                }
            }
        }
    }

    /// Gen 5 request; `read` waits for the frame answering `cmd`.
    fn g5(&mut self, cmd: u8, handle: u8, payload: &[u8], read: bool) -> Result<Vec<u8>> {
        let f = g5_request(cmd, handle, payload);
        if !read {
            self.dev.as_mut().context("device closed")?.write(&f)?;
            return Ok(Vec::new());
        }
        let r = self.transact(&f, |r| g5_reply(r, cmd).is_some())?;
        Ok(g5_reply(&r, cmd).unwrap_or_default().to_vec())
    }

    /// Gen 4 request: exactly one reply per request.
    fn g4(&mut self, cmd: u8, payload: &[u8]) -> Result<Vec<u8>> {
        let r = self.transact(&g4_request(cmd, payload), |r| r.first() == Some(&REPORT_ID))?;
        g4_reply(&r).map(<[u8]>::to_vec)
    }

    fn write(&mut self, key: SettingKey, v: u8) -> Result<()> {
        match self.gen {
            Gen::Five => {
                let (cmd, handle, payload) = g5_write(key, v).context("unsupported setting")?;
                self.g5(cmd, handle, &payload, false).map(|_| ())
            }
            Gen::Four => {
                let (cmd, payload, echo) = match (key, g4_slider(key)) {
                    (_, Some(s)) => (G4_SET_SLIDER, vec![s, v], vec![G4_SET_SLIDER, s]),
                    (SettingKey::MicNoiseFilter, _) => (G4_SET_NOISE_GATE, vec![v], vec![v]),
                    (SettingKey::EqPreset, _) => {
                        (G4_SET_EQ_PRESET, vec![v], vec![G4_SET_EQ_PRESET, v])
                    }
                    _ => bail!("unsupported setting"),
                };
                let r = self.g4(cmd, &payload)?;
                if !r.is_empty() && !r.starts_with(&echo) {
                    bail!("ASTRO A50: write was not echoed back");
                }
                Ok(())
            }
        }
    }

    fn merge(&mut self, key: SettingKey, value: SettingValue) {
        merge_setting(&mut self.settings, key, value);
    }

    fn poll(&mut self) {
        self.last_poll = Instant::now();
        let ok = match self.gen {
            Gen::Five => self.poll_g5(),
            Gen::Four => self.poll_g4(),
        };
        if let Err(e) = ok {
            self.battery.percent = None;
            self.battery.charging = false;
            self.last_error = Some(e.to_string());
        }
    }

    fn poll_g5(&mut self) -> Result<()> {
        let b = self.g5(G5_BATTERY, 0x0c, &[], true)?;
        if let [pct, _, dock, ..] = b[..] {
            self.battery.percent = (pct <= 100).then_some(pct);
            self.battery.charging = dock != 0;
        }
        if let Some(&raw) = self.g5(G5_CHATMIX, 0x0c, &[], true)?.first() {
            let (game, chat) = g5_chatmix(raw);
            self.merge(SettingKey::ChatMixGame, SettingValue::Float(game));
            self.merge(SettingKey::ChatMixChat, SettingValue::Float(chat));
        }
        if let [_, _, level, ..] = self.g5(G5_VOLUME, 0x0c, &[], true)?[..] {
            if level <= G5_VOLUME_MAX {
                self.merge(SettingKey::Volume, SettingValue::Int(level.into()));
            }
        }
        Ok(())
    }

    fn poll_g4(&mut self) -> Result<()> {
        let on = self
            .g4(G4_STATUS, &[])?
            .first()
            .is_some_and(|s| s & 0x02 != 0);
        // The base keeps the last level after the headset powers off.
        (self.battery.percent, self.battery.charging) = if on {
            match self.g4(G4_BATTERY, &[])?.first() {
                Some(&raw) => {
                    let (p, c) = g4_battery(raw);
                    (Some(p), c)
                }
                None => (None, false),
            }
        } else {
            (None, false)
        };
        for key in [SettingKey::MicVolume, SettingKey::MicSideTone] {
            let s = g4_slider(key).unwrap_or_default();
            if let [G4_GET_SLIDER, id, active, ..] = self.g4(G4_GET_SLIDER, &[s])?[..] {
                if id == s && active <= 100 {
                    self.merge(key, SettingValue::Int(active.into()));
                }
            }
        }
        if let [G4_GET_NOISE_GATE, mode @ 0..=3, ..] = self.g4(G4_GET_NOISE_GATE, &[])?[..] {
            self.merge(SettingKey::MicNoiseFilter, SettingValue::Int(mode.into()));
        }
        if let Some(&p @ 1..=3) = self.g4(G4_GET_EQ_PRESET, &[])?.first() {
            self.merge(SettingKey::EqPreset, SettingValue::Int(p.into()));
        }
        Ok(())
    }
}

impl DeviceAdapter for Astro {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: controls(self.gen),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let v = value.as_int().context("expected a number")? as u8;
        if let Err(e) = self.write(key, v) {
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        self.last_error = None;
        self.merge(key, value.clone());
        merge_setting(&mut self.chosen, key, value);
        hako_core::config::save_device_settings(
            &self.paths,
            self.id.vendor_id,
            self.id.product_id,
            &self.chosen,
        )
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_some() && self.last_poll.elapsed() >= POLL_EVERY {
            self.poll();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen5_frames_match_headsetcontrol() {
        // Captured layouts quoted in logitech_astro_a50.hpp.
        assert_eq!(
            &g5_request(0x0a, 0x0c, &[])[..7],
            &[2, 0x0c, 3, 0, 0x0a, 0x0c, 0]
        );
        let (cmd, handle, p) = g5_write(SettingKey::MicSideTone, 6).unwrap();
        assert_eq!(
            &g5_request(cmd, handle, &p)[..10],
            &[2, 0x0c, 6, 0, 0x09, 0x1c, 0x01, 0xff, 6, 0]
        );
        let (cmd, handle, p) = g5_write(SettingKey::Volume, 21).unwrap();
        assert_eq!(
            &g5_request(cmd, handle, &p)[..8],
            &[2, 0x0c, 5, 0, 0x08, 0x1b, 0xff, 21]
        );
        let (cmd, handle, p) = g5_write(SettingKey::Brightness, 100).unwrap();
        assert_eq!(
            &g5_request(cmd, handle, &p)[..7],
            &[2, 0x0c, 4, 0, 0x0f, 0x1c, 100]
        );
        let (cmd, handle, p) = g5_write(SettingKey::MicNoiseFilter, 4).unwrap();
        assert_eq!(
            &g5_request(cmd, handle, &p)[..7],
            &[2, 0x0c, 4, 0, 0x14, 0x2d, 4]
        );
        assert!(g5_write(SettingKey::Dpi, 1).is_none());
        assert!(g5_request(0, 0, &[]).len() == FRAME);
    }

    #[test]
    fn gen5_replies() {
        // Battery: 02 0c 06 00 06 00 <pct> <pct2> <dock>.
        let r = [2, 0x0c, 6, 0, 0x06, 0, 87, 87, 1, 0, 0];
        assert_eq!(g5_reply(&r, G5_BATTERY), Some(&[87, 87, 1][..]));
        // A bare ack and another property's push are not answers.
        assert_eq!(
            g5_reply(&[2, 0x0c, 3, 0, 0x06, 0x0c, 0, 0], G5_BATTERY),
            None
        );
        assert_eq!(g5_reply(&r, G5_CHATMIX), None);
        // Volume: <raw> 00 <level>.
        let v = [2, 0x0c, 6, 0, 0x08, 0x0c, 0x0e, 0, 10, 0];
        assert_eq!(g5_reply(&v, G5_VOLUME), Some(&[0x0e, 0, 10][..]));
        assert_eq!(g5_chatmix(0), (0.0, 1.0));
        assert_eq!(g5_chatmix(6), (1.0, 1.0));
        assert_eq!(g5_chatmix(12), (1.0, 0.0));
        assert_eq!(g5_chatmix(3), (0.5, 1.0));
    }

    #[test]
    fn gen4_frames_match_eh_fifty() {
        assert_eq!(&g4_request(G4_BATTERY, &[])[..4], &[2, 0x7c, 0, 0]);
        assert_eq!(
            &g4_request(G4_SET_SLIDER, &[G4_SLIDER_SIDETONE, 40])[..6],
            &[2, 0x62, 2, 0x05, 40, 0]
        );
        assert_eq!(&g4_request(G4_GET_SLIDER, &[4])[..4], &[2, 0x68, 1, 4]);
        // OK with data, accepted without, error with its name.
        assert_eq!(
            g4_reply(&[2, 2, 4, 0x68, 5, 30, 50, 9]).unwrap(),
            &[0x68, 5, 30, 50]
        );
        // A length byte past the frame is capped.
        assert_eq!(g4_reply(&[2, 2, 200, 0x8a]).unwrap(), &[0x8a]);
        assert!(g4_reply(&[2, 0, 0, 0]).unwrap().is_empty());
        let mut err = [0u8; 32];
        err[..3].copy_from_slice(&[2, 1, 20]);
        err[7..7 + 8].copy_from_slice(b"HID_FAIL");
        assert_eq!(
            g4_reply(&err).unwrap_err().to_string(),
            "ASTRO A50: HID_FAIL"
        );
        assert!(g4_reply(&[3, 2, 1, 0]).is_err());
        assert_eq!(g4_battery(0x80 | 64), (64, true));
        assert_eq!(g4_battery(87), (87, false));
    }

    #[test]
    fn save_is_a_blocked_placeholder() {
        let four = controls(Gen::Four);
        let persistent: Vec<_> = four.iter().filter(|c| c.persistent).collect();
        assert_eq!(persistent.len(), 1);
        assert!(persistent[0].blocked.is_some());
        assert!(controls(Gen::Five).iter().all(|c| !c.persistent));
        // Every writable control has a wire mapping.
        for c in four.iter().filter(|c| !c.read_only && !c.persistent) {
            assert!(
                g4_slider(c.key).is_some()
                    || matches!(c.key, SettingKey::MicNoiseFilter | SettingKey::EqPreset),
                "{:?}",
                c.key
            );
        }
        for c in controls(Gen::Five).iter().filter(|c| !c.read_only) {
            assert!(g5_write(c.key, 0).is_some(), "{:?}", c.key);
        }
    }
}
