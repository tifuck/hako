//! Razer keyboards and mice over the 90-byte Razer feature report
//! (`protocol.rs`). Sources: OpenRazer (<https://github.com/openrazer/openrazer>,
//! GPL; cited for IDs and byte layouts only, no code copied) and the uchroma
//! protocol reference; per-file notes name the exact files.
//!
//! - Huntsman V2 TKL (below): its own adapter, hidraw with usbfs fallback.
//! - Everything else: one generic adapter (`device.rs`) driven by the per-model
//!   table in `models.rs`.
//! - Headsets: `kraken.rs`.
//!
//! Huntsman V2 TKL lighting via OpenRazer protocol.
//!
//! OpenRazer kernel driver uses USB control transfers:
//!   SET_REPORT / GET_REPORT Feature (wValue 0x300) with **wIndex = 0x03**
//!   (interface 3, a vendor-only collection with no key input).
//!
//! Preferred path: hidraw on interface 3. usbhid turns HIDIOC\[SG\]FEATURE with
//! report id 0 into exactly that control transfer, so no kernel driver has to
//! be detached. Fallback: usbfs control transfers (claims interface 3), for
//! when interface 3 has no hidraw node (e.g. left unbound by a crashed daemon).
//!
//! Onboard memory: brightness (0x0f/0x04) and effect (0x0f/0x02) go out with
//! NOSTORE, also when re-applied on open, so the keyboard keeps nothing and
//! the daemon restores the user's choice on every open. "Save changes to
//! device memory" (`save_to_device`, behind the owner's onboard-memory
//! opt-in) sends them with VARSTORE (profile 1), as OpenRazer does for this
//! PID, only inside `set`: turning it on stores the current lighting once,
//! later user changes are stored too. Brightness is read back (VARSTORE, the
//! stored value) only in that mode; docs/onboard-writes/razer.md has the
//! details.
//!
//! Report rate (HyperPolling `00/40 [00 rate]`, read `00/c0`) and game mode
//! (game LED `03/00 [NOSTORE 08 on]`, read `03/80`), both on transaction
//! 0x1f, as OpenRazer `razerkbd_driver.c` sends them for this PID
//! (`razer_attr_{read,write}_{poll_rate,game_led_state}`; rates from the
//! daemon's `POLL_RATES`). Only sent on a user change; `tick` reads them back
//! with the brightness. Open is unchanged, so they are not re-applied there.

mod device;
mod kraken;
mod models;
mod protocol;

use crate::drivers::{DeviceAdapter, Model};
use crate::hid::{find_hidraw, find_usb_device_path, HidrawDevice, UsbDevice};
use anyhow::{anyhow, bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use protocol::{
    check_response, make_report, Effect as KbEffectKind, BACKLIGHT_LED, NOSTORE, VARSTORE,
};
use tracing::{info, warn};

const VID: u16 = 0x1532;
const PID: u16 = 0x026b;

/// OpenRazer report_index for Huntsman V2 TKL (USB interface number / wIndex).
const REPORT_IFACE: u16 = 0x03;

/// Transaction id OpenRazer uses for lighting on HUNTSMAN_V2_TENKEYLESS.
const TXN_LED: u8 = 0x1f;
/// Transaction id for device-mode / firmware (standard) commands.
const TXN_STD: u8 = 0xff;
/// Transaction id OpenRazer uses for poll rate and game mode on this PID.
const TXN_MISC: u8 = 0x1f;
/// OpenRazer daemon `POLL_RATES` for HUNTSMAN_V2_TENKEYLESS.
const POLL_RATES: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

/// HID feature buffer = report id 0 + 90-byte Razer report.
const FEATURE_LEN: usize = 91;
/// Waits before each re-read of a read answered busy.
const BUSY_REREAD_MS: [u64; 3] = [5, 10, 20];

const HUNTSMAN_V2_TKL: Model = Model {
    vendor_id: VID,
    product_ids: &[PID],
    kind: DeviceKind::Keyboard,
    name: "Razer Huntsman V2 TKL",
    // usbfs control-transfer fallback when interface 3 has no hidraw node.
    usbfs: true,
    multi: false,
    open: |_, ctx| {
        Ok(vec![
            Box::new(RazerKeyboardAdapter::try_open(ctx.paths.clone())?) as Box<dyn DeviceAdapter>,
        ])
    },
};

/// Huntsman V2 TKL first, then one entry per row of `models::SPECS`, then the
/// headsets of `kraken::MODELS`.
pub const MODELS: &[Model] = &ALL_MODELS;

const MODEL_COUNT: usize = 1 + models::SPECS.len() + kraken::MODELS.len();

const ALL_MODELS: [Model; MODEL_COUNT] = {
    let mut out = [HUNTSMAN_V2_TKL; MODEL_COUNT];
    let mut i = 0;
    while i < models::SPECS.len() {
        let s = &models::SPECS[i];
        out[i + 1] = Model {
            vendor_id: VID,
            product_ids: s.pids,
            kind: s.kind,
            name: s.name,
            usbfs: false,
            multi: false,
            open: device::open,
        };
        i += 1;
    }
    let mut j = 0;
    while j < kraken::MODELS.len() {
        let m = &kraken::MODELS[j];
        out[i + 1 + j] = Model {
            vendor_id: m.vendor_id,
            product_ids: m.product_ids,
            kind: m.kind,
            name: m.name,
            usbfs: m.usbfs,
            multi: m.multi,
            open: m.open,
        };
        j += 1;
    }
    out
};

enum Transport {
    Usb { dev: UsbDevice, iface: u16 },
    Hidraw(HidrawDevice),
}

impl Transport {
    fn set_feature(&mut self, report: &[u8; 90]) -> Result<()> {
        match self {
            Transport::Usb { dev, iface } => {
                let mut buf = *report;
                dev.hid_set_feature(*iface, &mut buf)
            }
            Transport::Hidraw(dev) => {
                let mut buf = [0u8; FEATURE_LEN];
                buf[1..].copy_from_slice(report);
                dev.set_feature(&mut buf)
            }
        }
    }

    fn get_feature(&mut self) -> Result<[u8; 90]> {
        let mut resp = [0u8; 90];
        match self {
            Transport::Usb { dev, iface } => dev.hid_get_feature(*iface, &mut resp)?,
            Transport::Hidraw(dev) => {
                // buf[0] = report id in, report data comes back at buf[1..].
                let mut buf = [0u8; FEATURE_LEN];
                dev.get_feature(&mut buf)?;
                resp.copy_from_slice(&buf[1..]);
            }
        }
        Ok(resp)
    }

    /// OpenRazer razer_send_payload: SET request, short wait, GET response,
    /// then require matching class/cmd and status busy (0x01) or ok (0x02);
    /// a read answered busy is read again a few times (`check_response`).
    fn transact(&mut self, report: &[u8; 90]) -> Result<[u8; 90]> {
        self.set_feature(report)?;
        std::thread::sleep(std::time::Duration::from_millis(1));
        let mut resp = self.get_feature()?;
        for ms in BUSY_REREAD_MS {
            if !protocol::busy_read(report, &resp) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(ms));
            resp = self.get_feature()?;
        }
        check_response(report, &resp)?;
        Ok(resp)
    }
}

/// How often the hardware brightness (changed with Fn keys) is read back.
const BRIGHTNESS_POLL: std::time::Duration = std::time::Duration::from_secs(10);

pub struct RazerKeyboardAdapter {
    id: DeviceId,
    transport: Option<Transport>,
    settings: SettingsMap,
    last_error: Option<String>,
    paths: ConfigPaths,
    last_poll: std::time::Instant,
}

impl RazerKeyboardAdapter {
    pub fn try_open(paths: ConfigPaths) -> Result<Self> {
        let (transport, instance) = open_transport()?;

        let id = DeviceId {
            vendor_id: VID,
            product_id: PID,
            instance,
        };
        let mut settings = hako_core::config::load_device_settings(&paths, VID, PID)?;
        let effect = KbEffectKind::from_str(
            get_setting(&settings, SettingKey::Effect)
                .and_then(|v| v.as_str())
                .unwrap_or("static"),
        );
        crate::drivers::sanitize_saved(&mut settings, &controls(effect));
        defaults(&mut settings);
        let mut a = Self {
            id,
            transport: Some(transport),
            settings,
            last_error: None,
            paths,
            last_poll: std::time::Instant::now(),
        };

        // Normal (firmware) mode, as razer_kbd_probe does. Driver mode (0x03)
        // hands Fn/media keys to the OpenRazer daemon, which we don't run; this
        // also recovers a keyboard left in driver mode by earlier builds.
        if let Err(e) = a.send(TXN_STD, 0x00, 0x04, &[0x00, 0x00]) {
            warn!("razer normal mode: {e}");
            a.last_error = Some(format!("device mode: {e}"));
        }

        // In save mode the stored brightness (changed with Fn keys) is current.
        if a.saving() {
            if let Ok(Some(br)) = a.read_brightness_pct() {
                merge_setting(
                    &mut a.settings,
                    SettingKey::Brightness,
                    SettingValue::Int(br as i64),
                );
            }
        }
        // Re-apply the chosen lighting, never stored on open.
        if let Err(e) = a.apply_all(NOSTORE) {
            warn!("razer restore on open: {e}");
            a.last_error = Some(e.to_string());
        }

        let via = match &a.transport {
            Some(Transport::Usb { dev, iface }) => {
                format!("usb {} iface={iface}", dev.path.display())
            }
            Some(Transport::Hidraw(h)) => format!("hidraw {}", h.path.display()),
            None => "none".into(),
        };
        info!("Razer keyboard open via {via}");
        Ok(a)
    }

    fn send(&mut self, txn: u8, class: u8, cmd: u8, args: &[u8]) -> Result<()> {
        self.send_get(txn, class, cmd, args)?;
        std::thread::sleep(std::time::Duration::from_millis(15));
        Ok(())
    }

    fn send_get(&mut self, txn: u8, class: u8, cmd: u8, args: &[u8]) -> Result<[u8; 90]> {
        let report = make_report(txn, class, cmd, args);
        self.transport
            .as_mut()
            .context("no device")?
            .transact(&report)
    }

    fn saving(&self) -> bool {
        get_setting(&self.settings, SettingKey::SaveToDevice)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn set_brightness_pct(&mut self, pct: u8, store: u8) -> Result<()> {
        let wire = ((pct.min(100) as u16) * 255 / 100) as u8;
        self.send(TXN_LED, 0x0f, 0x04, &[store, BACKLIGHT_LED, wire])
    }

    fn read_brightness_pct(&mut self) -> Result<Option<u8>> {
        let resp = self.send_get(TXN_LED, 0x0f, 0x84, &[VARSTORE, BACKLIGHT_LED, 0x00])?;
        // arguments[2] = brightness (status already checked by transact)
        if resp[5] >= 3 {
            return Ok(Some(pct_from_wire(resp[10]))); // arg[2]
        }
        Ok(None)
    }

    fn apply_all(&mut self, store: u8) -> Result<()> {
        let br = get_setting(&self.settings, SettingKey::Brightness)
            .and_then(|v| v.as_int())
            .unwrap_or(80) as u8;
        self.set_brightness_pct(br, store)?;
        std::thread::sleep(std::time::Duration::from_millis(20));

        let effect = KbEffectKind::from_str(
            get_setting(&self.settings, SettingKey::Effect)
                .and_then(|v| v.as_str())
                .unwrap_or("static"),
        );
        let r = get_setting(&self.settings, SettingKey::ColorR)
            .and_then(|v| v.as_int())
            .unwrap_or(0) as u8;
        let g = get_setting(&self.settings, SettingKey::ColorG)
            .and_then(|v| v.as_int())
            .unwrap_or(120) as u8;
        let b = get_setting(&self.settings, SettingKey::ColorB)
            .and_then(|v| v.as_int())
            .unwrap_or(255) as u8;
        let speed = get_setting(&self.settings, SettingKey::Speed)
            .and_then(|v| v.as_str())
            .unwrap_or("medium")
            .to_string();
        let direction = get_setting(&self.settings, SettingKey::Direction)
            .and_then(|v| v.as_str())
            .unwrap_or("left")
            .to_string();
        let args = kb_effect_args(store, effect, r, g, b, &speed, &direction);
        self.send(TXN_LED, 0x0f, 0x02, &args)?;
        let wire = (u16::from(br.min(100)) * 255 / 100) as u8;
        if store == VARSTORE && self.read_brightness_pct()? != Some(pct_from_wire(wire)) {
            bail!("stored brightness reads back differently");
        }
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        hako_core::config::save_device_settings(&self.paths, VID, PID, &self.settings)
    }

    /// Report rate or game mode: one command each, nothing else is resent.
    fn write_misc(&mut self, key: SettingKey, value: &SettingValue) -> Result<()> {
        let (class, cmd, args) = match key {
            SettingKey::ReportRate => value
                .as_int()
                .and_then(|hz| protocol::set_poll2(0x00, hz as u32))
                .context("unsupported report rate")?,
            _ => protocol::set_game_mode(NOSTORE, value.as_bool().context("expected true/false")?),
        };
        self.send(TXN_MISC, class, cmd, &args)
    }

    /// Current report rate (Hz) and game mode, as the keyboard reports them.
    fn read_misc(&mut self) -> Result<(Option<u32>, bool)> {
        let (class, cmd, args) = protocol::get_poll2();
        let hz = protocol::parse_poll2(&self.send_get(TXN_MISC, class, cmd, &args)?);
        let (class, cmd, args) = protocol::get_game_mode(NOSTORE);
        let game = self.send_get(TXN_MISC, class, cmd, &args)?[10] == 0x01;
        Ok((hz, game))
    }
}

fn open_transport() -> Result<(Transport, String)> {
    // Probe = GET firmware version (read-only, works in any device mode).
    let probe = make_report(TXN_STD, 0x00, 0x81, &[0x00, 0x00]);
    let mut errs = Vec::new();

    // 1) Preferred: hidraw on interface 3 (usbhid stays bound).
    match find_hidraw(VID, PID, Some(u32::from(REPORT_IFACE))) {
        Ok(info) => match HidrawDevice::open(&info.path) {
            Ok(dev) => {
                let mut t = Transport::Hidraw(dev);
                match t.transact(&probe) {
                    Ok(_) => return Ok((t, info.path.display().to_string())),
                    Err(e) => errs.push(format!("{}: {e}", info.path.display())),
                }
            }
            Err(e) => errs.push(e.to_string()),
        },
        Err(e) => errs.push(format!("hidraw iface {REPORT_IFACE}: {e}")),
    }

    // 2) Fallback: usbfs control transfers; claim interface 3 (detaches usbhid
    //    if bound, else SET_REPORT is EBUSY).
    match find_usb_device_path(VID, PID).and_then(|p| UsbDevice::open(&p).map(|d| (p, d))) {
        Ok((path, mut dev)) => {
            if let Err(e) = dev.claim_interface(REPORT_IFACE) {
                warn!("USB claim iface {REPORT_IFACE} failed: {e}");
            }
            let mut t = Transport::Usb {
                dev,
                iface: REPORT_IFACE,
            };
            match t.transact(&probe) {
                Ok(_) => return Ok((t, path.display().to_string())),
                Err(e) => errs.push(format!("{}: {e}", path.display())),
            }
        }
        Err(e) => errs.push(e.to_string()),
    }

    bail!("no Razer control path ({})", errs.join("; "))
}

/// Rounded, so it exactly inverts the truncating `pct * 255 / 100`.
fn pct_from_wire(wire: u8) -> u8 {
    ((u16::from(wire) * 100 + 127) / 255).min(100) as u8
}

fn defaults(s: &mut SettingsMap) {
    for (k, v) in [
        (SettingKey::Brightness, SettingValue::Int(80)),
        (SettingKey::ColorR, SettingValue::Int(0)),
        (SettingKey::ColorG, SettingValue::Int(120)),
        (SettingKey::ColorB, SettingValue::Int(255)),
        (SettingKey::Effect, SettingValue::Text("static".into())),
        (SettingKey::Speed, SettingValue::Text("medium".into())),
        (SettingKey::Direction, SettingValue::Text("left".into())),
    ] {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

/// Wave direction byte. OpenRazer passes the userspace value (WAVE_DIRS 1/2)
/// through unchanged for HUNTSMAN_V2_TENKEYLESS: 1 = left, 2 = right.
fn kb_wave_dir(direction: &str) -> u8 {
    protocol::wave_dir(direction, false)
}

/// Argument bytes for the extended-matrix effect command (class 0x0f, cmd 0x02).
fn kb_effect_args(
    store: u8,
    effect: KbEffectKind,
    r: u8,
    g: u8,
    b: u8,
    speed: &str,
    direction: &str,
) -> Vec<u8> {
    protocol::effect_args(
        store,
        BACKLIGHT_LED,
        effect,
        [r, g, b],
        speed,
        kb_wave_dir(direction),
    )
}

fn controls(effect: KbEffectKind) -> Vec<SettingMeta> {
    use SettingKey::*;
    let effect_opts = vec![
        SettingOption::text("off", "Off"),
        SettingOption::text("static", "Static"),
        SettingOption::text("spectrum", "Spectrum"),
        SettingOption::text("wave", "Wave"),
        SettingOption::text("reactive", "Reactive"),
        SettingOption::text("breathing", "Breathing"),
        SettingOption::text("starlight", "Starlight"),
    ];
    let speed_opts = vec![
        SettingOption::text("slow", "Slow"),
        SettingOption::text("medium", "Medium"),
        SettingOption::text("fast", "Fast"),
    ];
    let dir_opts = vec![
        SettingOption::text("left", "Left"),
        SettingOption::text("right", "Right"),
    ];
    let rate_opts = POLL_RATES
        .iter()
        .map(|&hz| SettingOption::int(i64::from(hz), &format!("{hz} Hz")))
        .collect();
    // Canonical order: Effect -> Brightness -> Direction -> Speed -> Color.
    // Everything is always listed (profiles may set it); irrelevant ones hidden.
    vec![
        SettingMeta::dropdown(ReportRate, "Report rate", rate_opts),
        SettingMeta::toggle(GameMode, "Game mode"),
        SettingMeta::dropdown(Effect, "Effect", effect_opts),
        SettingMeta::slider(Brightness, "Brightness", 0.0, 100.0, 1.0).unit("%"),
        SettingMeta::dropdown(Direction, "Direction", dir_opts)
            .hidden(effect != KbEffectKind::Wave),
        SettingMeta::dropdown(Speed, "Speed", speed_opts).hidden(!effect.has_speed()),
        SettingMeta::color(ColorR, "Red").hidden(!effect.has_color()),
        SettingMeta::color(ColorG, "Green").hidden(!effect.has_color()),
        SettingMeta::color(ColorB, "Blue").hidden(!effect.has_color()),
        SettingMeta::toggle(SaveToDevice, "Save changes to device memory").persistent(),
    ]
}

impl DeviceAdapter for RazerKeyboardAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Keyboard,
            name: "Razer Huntsman V2 TKL".into(),
            connected: self.transport.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls: controls(KbEffectKind::from_str(
                get_setting(&self.settings, SettingKey::Effect)
                    .and_then(|v| v.as_str())
                    .unwrap_or("static"),
            )),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value.clone());
        let store = if self.saving() { VARSTORE } else { NOSTORE };
        let result = match key {
            SettingKey::ReportRate | SettingKey::GameMode => self.write_misc(key, &value),
            // Turning it on stores the current lighting once; off sends nothing.
            SettingKey::SaveToDevice if !self.saving() => Ok(()),
            _ => self.apply_all(store),
        };
        if let Err(mut e) = result {
            // Not applied: keep the UI and the file on the old value.
            self.settings = before;
            // Brightness may already be written (or stored) before the effect
            // or the read-back failed: send the old lighting back.
            let lighting = !matches!(
                key,
                SettingKey::ReportRate | SettingKey::GameMode | SettingKey::SaveToDevice
            );
            if lighting {
                if let Err(re) = self.apply_all(store) {
                    e = anyhow!(
                        "{e:#}; restoring the previous lighting also failed ({re:#}), the keyboard may show part of the change"
                    );
                }
            }
            self.last_error = Some(format!("{e:#}"));
            return Err(e);
        }
        self.last_error = None;
        self.persist()
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        if self.last_poll.elapsed() < BRIGHTNESS_POLL {
            return Ok(());
        }
        self.last_poll = std::time::Instant::now();
        // VARSTORE reads the stored brightness: current only in save mode.
        if let Some(Ok(Some(br))) = self.saving().then(|| self.read_brightness_pct()) {
            let cur = get_setting(&self.settings, SettingKey::Brightness)
                .and_then(|v| v.as_int())
                .unwrap_or(-1);
            if cur != br as i64 {
                merge_setting(
                    &mut self.settings,
                    SettingKey::Brightness,
                    SettingValue::Int(br as i64),
                );
            }
        }
        // Game mode also follows the keyboard's own key.
        if let Ok((hz, game)) = self.read_misc() {
            if let Some(hz) = hz {
                merge_setting(
                    &mut self.settings,
                    SettingKey::ReportRate,
                    SettingValue::Int(i64::from(hz)),
                );
            }
            merge_setting(
                &mut self.settings,
                SettingKey::GameMode,
                SettingValue::Bool(game),
            );
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.transport = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kb_effect_args_match_wire_tables() {
        // Static red: 01 05 01 00 00 01 FF 00 00
        assert_eq!(
            kb_effect_args(
                VARSTORE,
                KbEffectKind::Static,
                0xFF,
                0x00,
                0x00,
                "medium",
                "left"
            ),
            vec![0x01, 0x05, 0x01, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x00]
        );
        // Off: 01 05 00 00 00 00
        assert_eq!(
            kb_effect_args(VARSTORE, KbEffectKind::Off, 0, 0, 0, "medium", "left"),
            vec![0x01, 0x05, 0x00, 0x00, 0x00, 0x00]
        );
        // Spectrum: 01 05 03 00 00 00
        assert_eq!(
            kb_effect_args(VARSTORE, KbEffectKind::Spectrum, 0, 0, 0, "medium", "left"),
            vec![0x01, 0x05, 0x03, 0x00, 0x00, 0x00]
        );
        // Wave left: 01 05 04 01 28 00 ; right dir byte = 0x02
        assert_eq!(
            kb_effect_args(VARSTORE, KbEffectKind::Wave, 0, 0, 0, "medium", "left"),
            vec![0x01, 0x05, 0x04, 0x01, 0x28, 0x00]
        );
        assert_eq!(kb_wave_dir("right"), 0x02);
        // Reactive fast green: 01 05 05 00 01 01 00 FF 00
        assert_eq!(
            kb_effect_args(
                VARSTORE,
                KbEffectKind::Reactive,
                0x00,
                0xFF,
                0x00,
                "fast",
                "left"
            ),
            vec![0x01, 0x05, 0x05, 0x00, 0x01, 0x01, 0x00, 0xFF, 0x00]
        );
        // Breathing single blue: 01 05 02 01 00 01 00 00 FF
        assert_eq!(
            kb_effect_args(
                VARSTORE,
                KbEffectKind::Breathing,
                0x00,
                0x00,
                0xFF,
                "medium",
                "left"
            ),
            vec![0x01, 0x05, 0x02, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF]
        );
        // Starlight slow white: 01 05 07 00 03 01 FF FF FF
        assert_eq!(
            kb_effect_args(
                VARSTORE,
                KbEffectKind::Starlight,
                0xFF,
                0xFF,
                0xFF,
                "slow",
                "left"
            ),
            vec![0x01, 0x05, 0x07, 0x00, 0x03, 0x01, 0xFF, 0xFF, 0xFF]
        );
    }

    /// Lighting is volatile unless save mode is on; the toggle needs the opt-in.
    #[test]
    fn huntsman_stores_only_in_save_mode() {
        assert_eq!(
            kb_effect_args(NOSTORE, KbEffectKind::Static, 1, 2, 3, "medium", "left"),
            vec![0x00, 0x05, 0x01, 0x00, 0x00, 0x01, 1, 2, 3]
        );
        let c = controls(KbEffectKind::Static);
        let save = c
            .iter()
            .find(|m| m.key == SettingKey::SaveToDevice)
            .unwrap();
        assert!(save.persistent && save.blocked.is_none());
        assert!(c.iter().filter(|m| m.persistent).count() == 1);
        for pct in 0..=100u8 {
            assert_eq!(pct_from_wire((u16::from(pct) * 255 / 100) as u8), pct);
        }
    }

    #[test]
    fn wave_speed_is_fixed_so_hidden() {
        let speed_hidden = |e| {
            controls(e)
                .iter()
                .find(|m| m.key == SettingKey::Speed)
                .unwrap()
                .hidden
        };
        assert!(speed_hidden(KbEffectKind::Wave));
        assert!(!speed_hidden(KbEffectKind::Reactive));
    }

    #[test]
    fn report_layout_and_response_check() {
        // Get brightness: 00 1f 0000 00 03 0f 84 010500 ... crc
        let r = make_report(TXN_LED, 0x0f, 0x84, &[VARSTORE, BACKLIGHT_LED, 0x00]);
        assert_eq!(
            &r[..11],
            &[0x00, 0x1f, 0, 0, 0, 0x03, 0x0f, 0x84, 0x01, 0x05, 0x00]
        );
        assert_eq!(r[88], 0x03 ^ 0x0f ^ 0x84 ^ 0x01 ^ 0x05);

        let mut resp = r;
        resp[0] = 0x02;
        assert!(check_response(&r, &resp).is_ok());
        resp[0] = 0x01; // a busy read has no data yet; a busy write is done
        assert!(check_response(&r, &resp).is_err());
        let w = make_report(TXN_LED, 0x0f, 0x04, &[VARSTORE, BACKLIGHT_LED, 0xcc]);
        let mut wresp = w;
        wresp[0] = 0x01;
        assert!(check_response(&w, &wresp).is_ok());
        resp[0] = 0x05;
        assert!(check_response(&r, &resp).is_err());
        resp[0] = 0x02;
        resp[7] = 0x81;
        assert!(check_response(&r, &resp).is_err());
    }

    /// Full frames this keyboard sends, as literal bytes (hand-computed XOR),
    /// so moving the builders into protocol.rs cannot change the wire format.
    #[test]
    fn huntsman_frames_are_byte_identical() {
        fn literal(head: &[u8], crc: u8) -> [u8; 90] {
            let mut f = [0u8; 90];
            f[..head.len()].copy_from_slice(head);
            f[88] = crc;
            f
        }
        // Normal device mode (open): 00 ff 0000 00 02 00 04 0000
        assert_eq!(
            make_report(TXN_STD, 0x00, 0x04, &[0x00, 0x00]),
            literal(&[0, 0xff, 0, 0, 0, 0x02, 0x00, 0x04], 0x06)
        );
        // Firmware probe: 00 ff 0000 00 02 00 81 0000
        assert_eq!(
            make_report(TXN_STD, 0x00, 0x81, &[0x00, 0x00]),
            literal(&[0, 0xff, 0, 0, 0, 0x02, 0x00, 0x81], 0x83)
        );
        // Brightness 80 % -> 0xcc: 00 1f 0000 00 03 0f 04 01 05 cc
        assert_eq!(
            make_report(TXN_LED, 0x0f, 0x04, &[VARSTORE, BACKLIGHT_LED, 0xcc]),
            literal(
                &[0, 0x1f, 0, 0, 0, 0x03, 0x0f, 0x04, 0x01, 0x05, 0xcc],
                0xc0
            )
        );
        // Default static colour (0,120,255): 00 1f 0000 00 09 0f 02 010501000001 0078ff
        let args = kb_effect_args(
            VARSTORE,
            KbEffectKind::Static,
            0,
            120,
            255,
            "medium",
            "left",
        );
        assert_eq!(
            make_report(TXN_LED, 0x0f, 0x02, &args),
            literal(
                &[
                    0, 0x1f, 0, 0, 0, 0x09, 0x0f, 0x02, 0x01, 0x05, 0x01, 0x00, 0x00, 0x01, 0x00,
                    0x78, 0xff
                ],
                0x87
            )
        );
        // Wave right and Starlight medium keep their bytes too.
        assert_eq!(
            kb_effect_args(VARSTORE, KbEffectKind::Wave, 0, 0, 0, "medium", "right"),
            vec![0x01, 0x05, 0x04, 0x02, 0x28, 0x00]
        );
        assert_eq!(
            kb_effect_args(VARSTORE, KbEffectKind::Starlight, 1, 2, 3, "medium", "left"),
            vec![0x01, 0x05, 0x07, 0x00, 0x02, 0x01, 1, 2, 3]
        );
        assert_eq!(
            kb_effect_args(VARSTORE, KbEffectKind::Reactive, 1, 2, 3, "slow", "left"),
            vec![0x01, 0x05, 0x05, 0x00, 0x04, 0x01, 1, 2, 3]
        );
    }

    /// Report rate and game mode frames as OpenRazer sends them for 026b.
    #[test]
    fn huntsman_report_rate_and_game_mode_frames() {
        let frame = |(class, cmd, args): protocol::Cmd| make_report(TXN_MISC, class, cmd, &args);
        // 1000 Hz: 00 1f 0000 00 02 00 40 00 08
        let r = frame(protocol::set_poll2(0x00, 1000).unwrap());
        assert_eq!(&r[..10], &[0, 0x1f, 0, 0, 0, 0x02, 0x00, 0x40, 0x00, 0x08]);
        assert_eq!(r[88], 0x02 ^ 0x40 ^ 0x08);
        assert_eq!(frame(protocol::set_poll2(0x00, 250).unwrap())[9], 0x20);
        assert_eq!(
            frame(protocol::get_poll2())[..9],
            [0, 0x1f, 0, 0, 0, 0x01, 0, 0xc0, 0]
        );
        // Game mode on: 00 1f 0000 00 03 03 00 00 08 01
        let g = frame(protocol::set_game_mode(NOSTORE, true));
        assert_eq!(
            &g[..11],
            &[0, 0x1f, 0, 0, 0, 0x03, 0x03, 0x00, 0x00, 0x08, 0x01]
        );
        assert_eq!(g[88], 0x09);
        assert_eq!(
            &frame(protocol::get_game_mode(NOSTORE))[6..11],
            &[0x03, 0x80, 0, 0x08, 0]
        );

        let c = controls(KbEffectKind::Static);
        let rate = c.iter().find(|m| m.key == SettingKey::ReportRate).unwrap();
        for hz in POLL_RATES {
            assert!(rate.validate(&SettingValue::Int(i64::from(hz))).is_ok());
            assert!(protocol::set_poll2(0x00, hz).is_some(), "{hz}");
        }
        assert!(rate.validate(&SettingValue::Int(750)).is_err());
        let game = c.iter().find(|m| m.key == SettingKey::GameMode).unwrap();
        assert!(!game.persistent && game.blocked.is_none());
    }

    #[test]
    fn huntsman_v2_tkl_stays_first_and_unchanged() {
        let m = &MODELS[0];
        assert_eq!((m.vendor_id, m.product_ids), (0x1532, &[0x026b][..]));
        assert_eq!(m.name, "Razer Huntsman V2 TKL");
        assert!(m.usbfs);
        assert_eq!(MODELS.len(), 1 + models::SPECS.len() + kraken::MODELS.len());
        assert!(MODELS[1..].iter().all(|m| !m.usbfs && m.vendor_id == VID));
    }

    /// Read-only: GET brightness through hidraw on interface 3.
    /// Needs usbhid bound to iface 3 (daemon not holding it via usbfs).
    #[test]
    #[ignore]
    fn hw_hidraw_iface3_get_brightness() {
        let info = find_hidraw(VID, PID, Some(3)).expect("hidraw iface 3");
        let mut t = Transport::Hidraw(HidrawDevice::open(&info.path).unwrap());
        let req = make_report(TXN_LED, 0x0f, 0x84, &[VARSTORE, BACKLIGHT_LED, 0x00]);
        let resp = t.transact(&req).expect("transact");
        eprintln!("resp {:02x?}", &resp[..12]);
    }
}
