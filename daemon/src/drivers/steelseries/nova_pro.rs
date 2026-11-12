//! Arctis Nova Pro Wireless (0x12e0 / 0x12e5) — port of known HID protocol.
//! Base (`12e0`) and X (`12e5`) GameDACs speak the same protocol.

use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{find_hidraw, pad64, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_audio::AudioService;
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, AuxConfig, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const VID: u16 = 0x1038;
/// ChatMix target: the analog output, else any Nova Pro sink.
const CHATMIX_SINKS: &[&str] = &[
    r"alsa_output\.usb-SteelSeries_Arctis_Nova_Pro_Wireless.*analog",
    r"SteelSeries_Arctis_Nova_Pro_Wireless",
];

pub(super) fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let cfg = hako_core::config::load_app_config(&ctx.paths).unwrap_or_default();
    Ok(vec![Box::new(SteelSeriesAdapter::try_open(
        pid,
        ctx.audio.clone(),
        cfg.aux,
        ctx.paths.clone(),
    )?)])
}

pub struct SteelSeriesAdapter {
    id: DeviceId,
    name: String,
    dev: Option<HidrawDevice>,
    settings: SettingsMap,
    last_error: Option<String>,
    game_mix: f64,
    chat_mix: f64,
    batteries: Vec<BatteryState>,
    last_poll: Instant,
    last_aux_check: Instant,
    poll_failures: u8,
    audio: Arc<AudioService>,
    /// A dial report arrived since the ChatMix volumes were last pushed.
    mix_dirty: bool,
    paths: ConfigPaths,
}

impl SteelSeriesAdapter {
    pub fn try_open(
        pid: u16,
        audio: Arc<AudioService>,
        aux_config: AuxConfig,
        paths: ConfigPaths,
    ) -> Result<Self> {
        // The utility HID collection that answers the 0x06 opcode set (status /
        // battery / ChatMix / setters) enumerates as bInterfaceNumber 4 on the
        // 12e5 GameDAC (the reference's libusb "interface 7" maps here); 7 is
        // kept as a fallback for other revisions. We deliberately do NOT fall
        // back to an arbitrary interface — opening the wrong collection would
        // silently swallow every init/status/setter write with no visible error.
        let info = find_hidraw(VID, pid, Some(4)).or_else(|_| find_hidraw(VID, pid, Some(7)))?;
        let dev = HidrawDevice::open(&info.path)?;
        let instance = if info.uniq.is_empty() {
            info.path.display().to_string()
        } else {
            info.uniq.clone()
        };
        let id = DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance,
        };
        let mut settings = hako_core::config::load_device_settings(&paths, VID, pid)?;
        defaults_into(&mut settings);
        let mut adapter = Self {
            id,
            name: "Arctis Nova Pro Wireless".into(),
            dev: Some(dev),
            settings,
            last_error: None,
            game_mix: 1.0,
            chat_mix: 1.0,
            batteries: vec![
                BatteryState {
                    label: "Headset".into(),
                    percent: None,
                    charging: false,
                    present: true,
                },
                // Spare battery charging in the base station's slot.
                BatteryState {
                    label: "Spare".into(),
                    percent: None,
                    charging: false,
                    present: true,
                },
            ],
            last_poll: Instant::now() - Duration::from_secs(10),
            last_aux_check: Instant::now(),
            poll_failures: 0,
            audio,
            mix_dirty: false,
            paths,
        };
        if let Err(e) = adapter.init_device() {
            warn!("init_device: {e}");
            adapter.last_error = Some(format!("init: {e}"));
        }
        // A (re)connected headset ends a kill-switch reset.
        adapter.audio.resume_chatmix();
        if let Err(error) = adapter.audio.ensure_chatmix(CHATMIX_SINKS) {
            warn!("chatmix: {error}");
        }
        let aux_enabled = get_setting(&adapter.settings, SettingKey::AuxEnabled)
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if aux_config.auto_enable && aux_enabled {
            let mut aux_config = aux_config;
            if let Some(percent) = get_setting(&adapter.settings, SettingKey::AuxCapturePercent)
                .and_then(|value| value.as_int())
            {
                aux_config.capture_percent = percent.clamp(0, 100) as u8;
            }
            if let Err(error) = adapter.audio.aux_enable(&aux_config, &aux_config.effects) {
                warn!("auto-enable AUX: {error}");
                adapter
                    .audio
                    .aux_keep_pending(&aux_config, &aux_config.effects);
            }
        }
        let _ = adapter.poll_status();
        info!("SteelSeries open {}", adapter.id.key());
        Ok(adapter)
    }

    fn write_cmd(&mut self, cmd: &[u8]) -> Result<()> {
        let dev = self.dev.as_mut().context("no device")?;
        dev.write(&pad64(cmd))?;
        Ok(())
    }

    /// `06 09`: HeadsetControl's "save state" and Linux-Arctis-Manager's
    /// `save_to_flash` for this PID, i.e. a flash write after every setter.
    /// Kept byte-identical (hardware-verified driver); not behind the
    /// onboard-memory opt-in yet, see `docs/onboard-writes/steelseries.md`.
    fn commit(&mut self) -> Result<()> {
        self.write_cmd(&[0x06, 0x09, 0x00])
    }

    fn init_device(&mut self) -> Result<()> {
        let wireless = get_setting(&self.settings, SettingKey::WirelessModeRange)
            .and_then(|v| v.as_bool())
            .map(|b| if b { 0x01 } else { 0x00 })
            .unwrap_or(0x00);
        let mic_vol = get_setting(&self.settings, SettingKey::MicVolume)
            .and_then(|v| v.as_int())
            .unwrap_or(0x0a) as u8;
        let side = get_setting(&self.settings, SettingKey::MicSideTone)
            .and_then(|v| v.as_int())
            .unwrap_or(0) as u8;
        let led = get_setting(&self.settings, SettingKey::MicLedBrightness)
            .and_then(|v| v.as_int())
            .unwrap_or(0x0a) as u8;
        let pm = get_setting(&self.settings, SettingKey::PmShutdown)
            .and_then(|v| v.as_int())
            .unwrap_or(0x05) as u8;
        let commands: Vec<Vec<u8>> = vec![
            vec![0x06, 0x20],
            vec![0x06, 0x20],
            vec![0x06, 0x10],
            vec![0x06, 0x10],
            vec![0x06, 0x10],
            vec![0x06, 0x3b],
            vec![0x06, 0x8d, 0x01],
            vec![0x06, 0x20],
            vec![0x06, 0x20],
            vec![0x06, 0x20],
            vec![0x06, 0x80],
            vec![0x06, 0x3b],
            vec![0x06, 0x8d, 0x01],
            vec![0x06, 0xc3, wireless],
            vec![0x06, 0xc1, pm],
            vec![0x06, 0x85, 0x0a],
            vec![0x06, 0x37, mic_vol],
            vec![0x06, 0xb2],
            vec![0x06, 0x47, 0x64, 0x00, 0x64],
            vec![0x06, 0x83, 0x01],
            vec![0x06, 0x89, 0x00],
            // 0x27 (mic "gain") intentionally not sent — unverified for this
            // device; on the 12e5 it alters output level. See controls().
            vec![0x06, 0xb3, 0x00],
            vec![0x06, 0x39, side],
            vec![0x06, 0xbf, led],
            vec![0x06, 0x43, 0x01],
            vec![0x06, 0x69, 0x00],
            vec![0x06, 0x3b, 0x00],
            vec![0x06, 0x8d, 0x01],
            vec![0x06, 0x49, 0x01],
            vec![0x06, 0xb7, 0x00],
            vec![0x06, 0xb0],
        ];
        for c in commands {
            self.write_cmd(&c)?;
            std::thread::sleep(Duration::from_millis(5));
        }
        // NOTE: no hardware EQ. The Nova Pro Wireless `0x33`/`0x2e` EQ registers
        // are not wired to the playback path on this device (that EQ is host-side
        // DSP in SteelSeries GG/Sonar), so we don't expose or send them.
        Ok(())
    }

    fn poll_status(&mut self) -> Result<()> {
        self.write_cmd(&[0x06, 0xb0])?;
        self.drain_reports(Duration::from_millis(80))?;
        self.last_poll = Instant::now();
        Ok(())
    }

    /// Handle every pending input report (ChatMix dial, status replies) without
    /// blocking. With a non-zero `status_wait`, keep reading up to that long
    /// until the 0x06 0xb0 status reply has been seen.
    fn drain_reports(&mut self, status_wait: Duration) -> Result<()> {
        let deadline = Instant::now() + status_wait;
        let mut got_status = status_wait.is_zero();
        let mut buf = [0u8; 64];
        for _ in 0..64 {
            let timeout = if got_status {
                Duration::ZERO
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            let dev = self.dev.as_mut().context("no device")?;
            let Ok(n) = dev.read_timeout(&mut buf, timeout) else {
                break;
            };
            got_status |= self.handle_report(&buf[..n]);
        }
        // One volume update for however many dial reports were queued.
        if std::mem::take(&mut self.mix_dirty) {
            if let Err(e) = self
                .audio
                .set_chatmix_volumes(self.game_mix as f32, self.chat_mix as f32)
            {
                warn!("chatmix volumes: {e}");
            }
        }
        Ok(())
    }

    /// Returns true for a status (0x06 0xb0) reply.
    fn handle_report(&mut self, b: &[u8]) -> bool {
        match b {
            [0x07, 0x45, game, chat, ..] => {
                self.game_mix = (*game as f64) / 100.0;
                self.chat_mix = (*chat as f64) / 100.0;
                merge_setting(
                    &mut self.settings,
                    SettingKey::ChatMixGame,
                    SettingValue::Float(self.game_mix),
                );
                merge_setting(
                    &mut self.settings,
                    SettingKey::ChatMixChat,
                    SettingValue::Float(self.chat_mix),
                );
                self.mix_dirty = true;
                false
            }
            [0x06, 0xb0, ..] if b.len() >= 16 => {
                self.decode_status(b);
                true
            }
            _ => false,
        }
    }

    fn decode_status(&mut self, data: &[u8]) {
        if data.len() < 16 {
            return;
        }
        let (hs, charging, online) = headset_battery(data);
        let spare = battery_pct(data[7]);
        self.batteries[0].percent = hs;
        self.batteries[0].charging = charging;
        self.batteries[0].present = online;
        self.batteries[1].percent = spare;
        // Live-pull hardware fields into settings so the UI mirrors the device.
        // 0x06 0xb0 reply layout (Linux-Arctis-Manager wireless_settings):
        //  [6] headset batt 0-8, [7] spare (charger slot) batt 0-8,
        //  [11] muted-mic LED 1–10, [12] auto-off level 0-6, [13] wireless
        //  mode, [15] headset status
        if (1..=10).contains(&data[11]) {
            merge_setting(
                &mut self.settings,
                SettingKey::MicLedBrightness,
                SettingValue::Int(data[11] as i64),
            );
        }
        if data[12] <= 6 {
            merge_setting(
                &mut self.settings,
                SettingKey::PmShutdown,
                SettingValue::Int(data[12] as i64),
            );
        }
        merge_setting(
            &mut self.settings,
            SettingKey::WirelessModeRange,
            SettingValue::Bool(data[13] != 0x00),
        );
        debug!(
            "battery headset={hs:?} charging={charging} online={online} spare={spare:?} mic_led={} pm={}",
            data[11], data[12]
        );
    }

    fn persist(&self, settings: &SettingsMap) -> Result<()> {
        hako_core::config::save_device_settings(
            &self.paths,
            self.id.vendor_id,
            self.id.product_id,
            settings,
        )
    }
}

/// Headset (percent, charging, online) from status byte \[15\]:
/// 0x01 offline, 0x02 cable charging, 0x04 charged, 0x08 discharging.
fn headset_battery(data: &[u8]) -> (Option<u8>, bool, bool) {
    let status = data[15];
    let online = status != 0x01;
    let pct = if online { battery_pct(data[6]) } else { None };
    (pct, status == 0x02, online)
}

fn battery_pct(raw: u8) -> Option<u8> {
    // Hardware reports 0..=8 eighths; anything higher (0xff) means offline.
    if raw > 8 {
        return None;
    }
    // round((raw/8)*100), not floor — floor read every odd raw ~1% low
    // (raw=1 → 12 vs 13, raw=7 → 87 vs 88).
    Some(((raw as u16 * 100 + 4) / 8).min(100) as u8)
}

fn defaults_into(s: &mut SettingsMap) {
    for key in [
        "aux_gate_enabled",
        "aux_gate_threshold",
        "aux_comp_enabled",
        "aux_comp_threshold",
        "aux_comp_ratio",
    ] {
        s.remove(key);
    }
    let d = [
        (SettingKey::MicVolume, SettingValue::Int(10)),
        (SettingKey::MicSideTone, SettingValue::Int(0)),
        (SettingKey::MicLedBrightness, SettingValue::Int(10)),
        (SettingKey::PmShutdown, SettingValue::Int(5)),
        (SettingKey::WirelessModeRange, SettingValue::Bool(false)),
        (SettingKey::ChatMixGame, SettingValue::Float(1.0)),
        (SettingKey::ChatMixChat, SettingValue::Float(1.0)),
        (SettingKey::AuxEnabled, SettingValue::Bool(false)),
        (SettingKey::AuxCapturePercent, SettingValue::Int(66)),
    ];
    for (k, v) in d {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

fn controls() -> Vec<SettingMeta> {
    use SettingKey::*;
    // Labels are section-relative (the UI card header supplies context), so they
    // stay short and the control column lines up. Order = non-mic first.
    vec![
        SettingMeta::readonly(ChatMixGame, "Game", 0.0, 1.0),
        SettingMeta::readonly(ChatMixChat, "Chat", 0.0, 1.0),
        SettingMeta::dropdown(
            PmShutdown,
            "Auto-off",
            vec![
                SettingOption::int(0, "Never"),
                SettingOption::int(1, "After 1 min"),
                SettingOption::int(2, "After 5 min"),
                SettingOption::int(3, "After 10 min"),
                SettingOption::int(4, "After 15 min"),
                SettingOption::int(5, "After 30 min"),
                SettingOption::int(6, "After 60 min"),
            ],
        ),
        SettingMeta::toggle(WirelessModeRange, "Range mode"),
        SettingMeta::toggle(AuxEnabled, "Monitor line-in"),
        SettingMeta::slider(AuxCapturePercent, "Capture level", 0.0, 100.0, 1.0).unit("%"),
        SettingMeta::slider(MicVolume, "Mic level", 1.0, 10.0, 1.0),
        // Four discrete named levels read better as a dropdown than a 0–3 slider.
        SettingMeta::dropdown(
            MicSideTone,
            "Sidetone",
            vec![
                SettingOption::int(0, "Off"),
                SettingOption::int(1, "Low"),
                SettingOption::int(2, "Medium"),
                SettingOption::int(3, "High"),
            ],
        ),
        // NOTE: "high gain" (opcode 0x27) is intentionally omitted. It is
        // unverified for this device family — HeadsetControl does not implement
        // it, and on the 12e5 variant it drives output level, not mic gain.
        SettingMeta::slider(MicLedBrightness, "Mute LED", 1.0, 10.0, 1.0),
    ]
}

impl DeviceAdapter for SteelSeriesAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.name.clone(),
            connected: self.dev.is_some(),
            batteries: self.batteries.clone(),
            // The toggle shows whether AUX is actually on (or pending a retry);
            // the persisted value only feeds `aux.auto_enable` on connect.
            settings: {
                let mut s = self.settings.clone();
                merge_setting(
                    &mut s,
                    SettingKey::AuxEnabled,
                    SettingValue::Bool(self.audio.aux_active()),
                );
                s
            },
            controls: controls(),
            // HID errors first; audio errors live in the audio service.
            last_error: self.last_error.clone().or_else(|| self.audio.last_error()),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let mut proposed = self.settings.clone();
        merge_setting(&mut proposed, key, value.clone());
        match key {
            SettingKey::MicVolume => {
                let v = value.as_int().unwrap_or(10) as u8;
                self.write_cmd(&[0x06, 0x37, v])?;
                self.commit()?;
            }
            SettingKey::MicSideTone => {
                let v = value.as_int().unwrap_or(0) as u8;
                self.write_cmd(&[0x06, 0x39, v])?;
                self.commit()?;
            }
            SettingKey::MicLedBrightness => {
                let v = value.as_int().unwrap_or(10) as u8;
                self.write_cmd(&[0x06, 0xbf, v])?;
                self.commit()?;
            }
            SettingKey::PmShutdown => {
                let v = value.as_int().unwrap_or(5) as u8;
                self.write_cmd(&[0x06, 0xc1, v])?;
                self.commit()?;
            }
            SettingKey::WirelessModeRange => {
                let v = if value.as_bool().unwrap_or(false) {
                    0x01
                } else {
                    0x00
                };
                self.write_cmd(&[0x06, 0xc3, v])?;
                self.commit()?;
            }
            SettingKey::AuxEnabled => {
                let on = value.as_bool().unwrap_or(false);
                let mut cfg = hako_core::config::load_app_config(&self.paths)
                    .map(|c| c.aux)
                    .unwrap_or_default();
                if let Some(p) =
                    get_setting(&proposed, SettingKey::AuxCapturePercent).and_then(|v| v.as_int())
                {
                    cfg.capture_percent = p.clamp(0, 100) as u8;
                }
                if on {
                    self.audio.aux_enable(&cfg, &cfg.effects)?;
                } else {
                    self.audio.aux_disable()?;
                }
            }
            SettingKey::AuxCapturePercent => {
                let p = value.as_int().unwrap_or(66).clamp(0, 100) as u8;
                if self.audio.aux_active() {
                    self.audio.set_aux_capture_percent(p)?;
                }
            }
            _ => bail!("unsupported setting {}", key.as_str()),
        }
        self.persist(&proposed)?;
        self.settings = proposed;
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        if !self.audio.chatmix_active() {
            let _ = self.audio.ensure_chatmix(CHATMIX_SINKS);
        }
        // ChatMix dial events arrive unsolicited; pick them up every tick.
        if self.dev.is_some() {
            let _ = self.drain_reports(Duration::ZERO);
        }
        if self.last_aux_check.elapsed() >= Duration::from_secs(5) {
            self.last_aux_check = Instant::now();
            self.audio.heal_aux();
        }
        if self.last_poll.elapsed() >= Duration::from_secs(5) {
            if let Err(e) = self.poll_status() {
                self.poll_failures = self.poll_failures.saturating_add(1);
                self.last_error = Some(e.to_string());
                if self.poll_failures >= 3 {
                    self.dev = None;
                }
                return Err(e);
            } else {
                self.poll_failures = 0;
                self.last_error = None;
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        let _ = self.audio.aux_disable();
        // ChatMix destroyed by daemon-level audio reset on stop
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_values_are_rounded_eighths() {
        assert_eq!(battery_pct(0), Some(0));
        assert_eq!(battery_pct(1), Some(13));
        assert_eq!(battery_pct(7), Some(88));
        assert_eq!(battery_pct(8), Some(100));
        assert_eq!(battery_pct(0xff), None);
    }

    #[test]
    fn headset_status_byte_15() {
        let mut d = [0u8; 16];
        d[6] = 4;
        d[15] = 0x08; // discharging
        assert_eq!(headset_battery(&d), (Some(50), false, true));
        d[15] = 0x02; // cable charging
        assert_eq!(headset_battery(&d), (Some(50), true, true));
        d[15] = 0x01; // offline: level byte is stale
        assert_eq!(headset_battery(&d), (None, false, false));
    }

    #[test]
    fn controls_only_list_supported_keys() {
        let keys: Vec<_> = controls().into_iter().map(|control| control.key).collect();
        assert_eq!(
            keys,
            vec![
                SettingKey::ChatMixGame,
                SettingKey::ChatMixChat,
                SettingKey::PmShutdown,
                SettingKey::WirelessModeRange,
                SettingKey::AuxEnabled,
                SettingKey::AuxCapturePercent,
                SettingKey::MicVolume,
                SettingKey::MicSideTone,
                SettingKey::MicLedBrightness,
            ]
        );
        assert!(controls()
            .into_iter()
            .filter(|control| control.read_only)
            .all(|control| matches!(
                control.key,
                SettingKey::ChatMixGame | SettingKey::ChatMixChat
            )));
    }
}
