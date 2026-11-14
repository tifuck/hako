//! Arctis Nova Pro Wireless (0x12e0 / 0x12e5) — port of known HID protocol.
//! Base (`12e0`) and X (`12e5`) GameDACs speak the same protocol. The X
//! (white) `225d` shares it per Linux-Arctis-Manager `nova_pro_wireless.yaml`
//! (same variant list and interface 4), the only source listing that PID, so
//! it never gets the `06 09` flash save.
//!
//! Setters are volatile and re-sent on open. "Save changes to device memory"
//! (`save_to_device`, behind the owner's onboard-memory opt-in) adds the
//! `06 09` flash save after each setter; turning it on saves once.

use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{find_hidraw, pad64, HidrawDevice};
use anyhow::{anyhow, bail, Context, Result};
use hako_audio::AudioService;
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const VID: u16 = 0x1038;
/// How often a missing ChatMix is retried.
const CHATMIX_RETRY: Duration = Duration::from_secs(5);
/// First wait before a failed init sequence is sent again; doubled per
/// failure up to `INIT_RETRY_MAX`.
const INIT_RETRY: Duration = Duration::from_secs(5);
const INIT_RETRY_MAX: Duration = Duration::from_secs(60);
/// ChatMix target: the analog output, else any Nova Pro sink.
const CHATMIX_SINKS: &[&str] = &[
    r"alsa_output\.usb-SteelSeries_Arctis_Nova_Pro_Wireless.*analog",
    r"SteelSeries_Arctis_Nova_Pro_Wireless",
];

pub(super) fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    Ok(vec![Box::new(SteelSeriesAdapter::try_open(
        pid,
        ctx.audio.clone(),
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
    last_chatmix_try: Instant,
    poll_failures: u8,
    /// Why the last init sequence failed; it is re-sent from `tick` until it
    /// goes through, and shown until then.
    init_error: Option<String>,
    init_failures: u32,
    last_init_try: Instant,
    audio: Arc<AudioService>,
    /// A dial report arrived since the ChatMix volumes were last pushed.
    mix_dirty: bool,
    paths: ConfigPaths,
}

impl SteelSeriesAdapter {
    pub fn try_open(pid: u16, audio: Arc<AudioService>, paths: ConfigPaths) -> Result<Self> {
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
        crate::drivers::sanitize_saved(&mut settings, &controls(pid));
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
            last_chatmix_try: Instant::now(),
            poll_failures: 0,
            init_error: None,
            init_failures: 0,
            last_init_try: Instant::now(),
            audio,
            mix_dirty: false,
            paths,
        };
        adapter.try_init();
        // A (re)connected headset ends a kill-switch reset.
        adapter.audio.resume_chatmix();
        if let Err(error) = adapter.audio.ensure_chatmix(CHATMIX_SINKS) {
            warn!("chatmix: {error}");
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
    /// `save_to_flash` for this PID, a flash write; only in save mode, see
    /// `docs/onboard-writes/steelseries.md`.
    fn commit(&mut self, save: bool) -> Result<()> {
        if !save {
            return Ok(());
        }
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

    /// Send the init sequence; a failure is kept in `init_error` (and
    /// `last_error`) for `tick` to retry.
    fn try_init(&mut self) {
        self.last_init_try = Instant::now();
        match self.init_device() {
            Ok(()) => {
                self.init_failures = 0;
                if self.init_error.take().is_some() {
                    self.last_error = None;
                }
            }
            Err(e) => {
                warn!("init_device: {e}");
                self.init_failures = self.init_failures.saturating_add(1);
                self.init_error = Some(format!("init: {e}"));
                self.last_error = self.init_error.clone();
            }
        }
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

/// Wait before re-sending an init sequence that failed `failures` times.
fn init_retry_after(failures: u32) -> Duration {
    INIT_RETRY
        .saturating_mul(1 << failures.saturating_sub(1).min(8))
        .min(INIT_RETRY_MAX)
}

/// `225d` is listed by one source only: its setters stay volatile (they are
/// re-sent on open).
fn saves_after_set(pid: u16) -> bool {
    pid != 0x225d
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
    ];
    for (k, v) in d {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

fn controls(pid: u16) -> Vec<SettingMeta> {
    use SettingKey::*;
    let save = SettingMeta::toggle(SaveToDevice, "Save changes to device memory").persistent();
    let save = if saves_after_set(pid) {
        save
    } else {
        save.blocked("only Linux-Arctis-Manager lists this PID's flash save")
    };
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
        save,
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
            settings: self.settings.clone(),
            controls: controls(self.id.product_id),
            // HID errors first; audio errors live in the audio service.
            last_error: self.last_error.clone().or_else(|| self.audio.last_error()),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let mut proposed = self.settings.clone();
        merge_setting(&mut proposed, key, value.clone());
        let save = saves_after_set(self.id.product_id)
            && get_setting(&proposed, SettingKey::SaveToDevice)
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        let cmd = match key {
            // Turning it on saves once; if that fails the mode stays off.
            SettingKey::SaveToDevice => {
                self.commit(save)?;
                None
            }
            SettingKey::MicVolume => Some([0x06, 0x37, value.as_int().unwrap_or(10) as u8]),
            SettingKey::MicSideTone => Some([0x06, 0x39, value.as_int().unwrap_or(0) as u8]),
            SettingKey::MicLedBrightness => Some([0x06, 0xbf, value.as_int().unwrap_or(10) as u8]),
            SettingKey::PmShutdown => Some([0x06, 0xc1, value.as_int().unwrap_or(5) as u8]),
            SettingKey::WirelessModeRange => {
                Some([0x06, 0xc3, u8::from(value.as_bool().unwrap_or(false))])
            }
            _ => bail!("unsupported setting {}", key.as_str()),
        };
        let flash = match cmd {
            Some(cmd) => {
                self.write_cmd(&cmd)?;
                self.commit(save)
            }
            None => Ok(()),
        };
        // The device (or audio graph) already changed: show it even if saving fails.
        self.settings = proposed;
        let file = self.persist(&self.settings);
        flash.map_err(|e| anyhow!("applied, but not saved to device memory: {e:#}"))?;
        file
    }

    fn tick(&mut self) -> Result<()> {
        // Each try spawns pw-* processes under the audio lock: not every tick.
        if self.last_chatmix_try.elapsed() >= CHATMIX_RETRY && !self.audio.chatmix_active() {
            self.last_chatmix_try = Instant::now();
            // A failure is kept in the audio service's ChatMix error.
            let _ = self.audio.ensure_chatmix(CHATMIX_SINKS);
        }
        // ChatMix dial events arrive unsolicited; pick them up every tick.
        if self.dev.is_some() {
            let _ = self.drain_reports(Duration::ZERO);
        }
        if self.init_error.is_some()
            && self.dev.is_some()
            && self.last_init_try.elapsed() >= init_retry_after(self.init_failures)
        {
            self.try_init();
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
                // A failed init stays visible until it is re-sent.
                self.last_error = self.init_error.clone();
            }
        }
        Ok(())
    }

    /// The ChatMix dial is read in `tick`: keep its latency low. The slow
    /// parts of `tick` (status poll, ChatMix retry) run on timers.
    fn tick_interval(&self) -> Duration {
        Duration::from_millis(100)
    }

    fn shutdown(&mut self) {
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
    fn failed_init_is_retried_with_backoff() {
        let waits: Vec<_> = (1..=6).map(|f| init_retry_after(f).as_secs()).collect();
        assert_eq!(waits, [5, 10, 20, 40, 60, 60]);
        assert_eq!(init_retry_after(u32::MAX), INIT_RETRY_MAX);
    }

    #[test]
    fn only_the_white_x_skips_the_flash_save() {
        assert!(saves_after_set(0x12e0) && saves_after_set(0x12e5));
        assert!(!saves_after_set(0x225d));
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
        let keys: Vec<_> = controls(0x12e5)
            .into_iter()
            .map(|control| control.key)
            .collect();
        assert_eq!(
            keys,
            vec![
                SettingKey::ChatMixGame,
                SettingKey::ChatMixChat,
                SettingKey::PmShutdown,
                SettingKey::WirelessModeRange,
                SettingKey::MicVolume,
                SettingKey::MicSideTone,
                SettingKey::MicLedBrightness,
                SettingKey::SaveToDevice,
            ]
        );
        let save = |pid| {
            controls(pid)
                .into_iter()
                .find(|c| c.key == SettingKey::SaveToDevice)
                .unwrap()
        };
        assert!(save(0x12e5).persistent && save(0x12e5).blocked.is_none());
        assert!(save(0x225d).blocked.is_some());
        assert!(controls(0x12e5)
            .into_iter()
            .filter(|control| control.read_only)
            .all(|control| matches!(
                control.key,
                SettingKey::ChatMixGame | SettingKey::ChatMixChat
            )));
    }
}
