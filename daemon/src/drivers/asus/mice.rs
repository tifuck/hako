//! ASUS ROG / TUF mouse adapter, driven by the rows in `mouse_models`.
//!
//! Settings are read from the mouse where unset, written without the `50 03`
//! save, and re-applied whenever a wireless mouse comes back into range.
//!
//! Save to device memory (owner opt-in): after each user change, and once
//! when the mode is turned on, the performance settings are read back from
//! the active profile and compared with what the user set; only if they match
//! is `50 03` sent, which stores that profile (libratbag commits every
//! profile this way, G-Helper flushes after every setter). Lighting and
//! auto-off are stored too but cannot be read back to check them first.
//! Re-applying settings on (re)connect never saves.
//!
//! DPI: `dpi_stage_1..N` are the onboard stages, `dpi_stage_active` the stage
//! the DPI button selected and `dpi` mirrors the active stage's value (the
//! active stage is re-read on connect and every few seconds, as the button
//! changes it; it is never pushed back on connect).

pub use super::mouse_models::{find, find_omni, MouseSpec};
use super::protocol::{self as p, Battery, Exchange, Lod, Transport, VID};
use super::{effect_of, int_of, lighting_controls, lighting_defaults, rgb_of};
use crate::drivers::DeviceAdapter;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

const BATTERY_POLL: Duration = Duration::from_secs(30);
/// While a wireless mouse is off or asleep, look for it more often.
const RELINK_POLL: Duration = Duration::from_secs(5);
/// How often the active DPI stage is re-read.
const STAGE_POLL: Duration = Duration::from_secs(5);

const STAGE_KEYS: [SettingKey; 4] = [
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
];

fn stage_key(stage: u8) -> SettingKey {
    STAGE_KEYS[usize::from(stage - 1)]
}

/// The model sharing `spec`'s PIDs whose HID name `name` matches, else `spec`.
pub fn variant(spec: &'static MouseSpec, name: &str) -> &'static MouseSpec {
    let name = name.to_ascii_uppercase();
    spec.variants
        .iter()
        .find(|(frag, _)| name.contains(frag))
        .map_or(spec, |&(_, v)| v)
}

pub struct MouseAdapter {
    spec: &'static MouseSpec,
    id: DeviceId,
    t: Option<Transport>,
    settings: SettingsMap,
    battery: Option<BatteryState>,
    /// Raw low-battery threshold from `12 07`; auto-off writes must resend it.
    low_battery: Option<u8>,
    online: bool,
    last_poll: Instant,
    last_stage_poll: Instant,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl MouseAdapter {
    /// A mouse on its own cable or receiver PID.
    pub fn open(spec: &'static MouseSpec, pid: u16, paths: ConfigPaths) -> Result<Self> {
        let (t, path) = if spec.rx.contains(&pid) {
            Transport::open(pid, p::OMNI_IFACE, p::OMNI_MOUSE_REPORT)?
        } else {
            Transport::open(pid, spec.iface, 0)?
        };
        let name = crate::hid::find_hidraw(VID, pid, None)
            .map(|i| i.name)
            .unwrap_or_default();
        let spec = variant(spec, &name);
        Self::with(spec, pid, t, path, paths)
    }

    /// A mouse the Omni receiver `receiver` lists as `paired`.
    pub fn open_paired(
        spec: &'static MouseSpec,
        receiver: u16,
        paired: u16,
        paths: ConfigPaths,
    ) -> Result<Self> {
        let (t, path) = Transport::open(receiver, p::OMNI_IFACE, p::OMNI_MOUSE_REPORT)?;
        Self::with(spec, paired, t, path, paths)
    }

    fn with(
        spec: &'static MouseSpec,
        pid: u16,
        t: Transport,
        path: String,
        paths: ConfigPaths,
    ) -> Result<Self> {
        let config_pid = spec.pids.first().copied().unwrap_or(pid);
        let mut settings = hako_core::config::load_device_settings(&paths, VID, config_pid)?;
        if !spec.zones.is_empty() {
            lighting_defaults(&mut settings, false);
        }
        let offered = controls(spec, &settings);
        crate::drivers::sanitize_saved(&mut settings, &offered);
        let mut a = Self {
            spec,
            id: DeviceId {
                vendor_id: VID,
                product_id: pid,
                instance: path.clone(),
            },
            t: Some(t),
            settings,
            battery: (spec.battery != Battery::None).then(|| BatteryState {
                label: "Mouse".into(),
                percent: None,
                charging: false,
                present: true,
            }),
            low_battery: None,
            online: true,
            last_poll: Instant::now(),
            last_stage_poll: Instant::now(),
            last_error: None,
            paths,
        };
        a.connect();
        info!(
            "{} open via {path} online={} error={:?}",
            spec.name, a.online, a.last_error
        );
        Ok(a)
    }

    fn t(&mut self) -> Result<&mut Transport> {
        self.t.as_mut().context("device closed")
    }

    /// Take unset performance values from the device, then push every setting.
    /// The active DPI stage is never pushed: the mouse's is adopted (see
    /// [`adopt`]); nor is a lift-off distance the mouse already has (see
    /// [`resend_lod`]). Runs on open and whenever a wireless mouse comes back.
    fn connect(&mut self) {
        if self.spec.battery != Battery::None {
            if let Err(e) = self.poll_battery() {
                self.last_error = Some(format!("mouse not reachable: {e}"));
                return;
            }
        }
        let mut errors = Vec::new();
        let spec = self.spec;
        let mut hw_lod = None;
        match self.t().and_then(|t| read_performance(spec, t)) {
            Ok(hw) => {
                hw_lod = get_setting(&hw, SettingKey::LiftOffDistance).cloned();
                adopt(spec, &mut self.settings, hw)
            }
            Err(e) => errors.push(format!("read settings: {e}")),
        }
        use SettingKey::*;
        let keys = STAGE_KEYS.into_iter().chain([
            ReportRate,
            Debounce,
            AngleSnapping,
            LiftOffDistance,
            PmShutdown,
            Effect,
        ]);
        for key in keys {
            if key == LiftOffDistance
                && !resend_lod(spec.lod, get_setting(&self.settings, key), hw_lod.as_ref())
            {
                continue;
            }
            let offered = controls(self.spec, &self.settings)
                .iter()
                .any(|m| m.key == key);
            if offered && get_setting(&self.settings, key).is_some() {
                if let Err(e) = self.apply(key) {
                    errors.push(format!("{}: {e}", key.as_str()));
                }
            }
        }
        self.last_error = (!errors.is_empty()).then(|| errors.join("; "));
    }

    fn saving(&self) -> bool {
        get_setting(&self.settings, SettingKey::SaveToDevice).and_then(|v| v.as_bool())
            == Some(true)
    }

    /// Check the active profile against the user's settings, then `50 03`.
    fn store(&mut self) -> Result<()> {
        let spec = self.spec;
        let expected = self.settings.clone();
        store_profile(spec, self.t()?, &expected)
    }

    fn poll_battery(&mut self) -> Result<()> {
        self.last_poll = Instant::now();
        let kind = self.spec.battery;
        let resp = self.t()?.transact(p::GET_BATTERY);
        let b = match resp {
            Ok(r) => p::parse_battery(kind, &r),
            Err(e) => {
                self.online = false;
                if let Some(s) = self.battery.as_mut() {
                    s.percent = None;
                    s.charging = false;
                }
                return Err(e);
            }
        };
        self.online = true;
        if let Some(s) = self.battery.as_mut() {
            s.percent = Some(b.percent);
            s.charging = b.charging;
        }
        self.low_battery = Some(b.low_battery);
        if self.spec.auto_off {
            if let Some(m) = p::auto_off_minutes(b.auto_off) {
                self.settings
                    .entry(SettingKey::PmShutdown.as_str().into())
                    .or_insert(SettingValue::Int(m.into()));
            }
        }
        Ok(())
    }

    /// Follow the DPI button: re-read the active stage and mirror its DPI.
    fn poll_stage(&mut self) -> Result<()> {
        self.last_stage_poll = Instant::now();
        let spec = self.spec;
        let Some(d) = spec.dpi.filter(|d| d.active) else {
            return Ok(());
        };
        let stage = p::parse_stage(
            &self.t()?.transact(p::GET_PROFILE)?,
            spec.stages,
            d.zero_based,
        );
        if int_of(&self.settings, SettingKey::DpiStageActive, 0) != i64::from(stage) {
            mirror_active(&mut self.settings, stage);
            self.persist()?;
        }
        Ok(())
    }

    fn apply(&mut self, key: SettingKey) -> Result<()> {
        let spec = self.spec;
        let value = get_setting(&self.settings, key).cloned();
        let int = value.as_ref().and_then(|v| v.as_int());
        match key {
            SettingKey::Dpi => match int {
                Some(dpi) => self.write_dpi(dpi as u16),
                None => Ok(()),
            },
            SettingKey::DpiStage1
            | SettingKey::DpiStage2
            | SettingKey::DpiStage3
            | SettingKey::DpiStage4 => {
                let stage = STAGE_KEYS.iter().position(|&k| k == key).unwrap_or(0) as u8 + 1;
                let Some(dpi) = int else {
                    return Ok(());
                };
                self.write_stage(stage, dpi as u16)?;
                if int_of(&self.settings, SettingKey::DpiStageActive, 0) == i64::from(stage) {
                    merge_setting(&mut self.settings, SettingKey::Dpi, SettingValue::Int(dpi));
                }
                Ok(())
            }
            SettingKey::DpiStageActive => {
                let (Some(d), Some(stage)) = (spec.dpi, int) else {
                    return Ok(());
                };
                let stage = u8::try_from(stage).context("no such DPI stage")?;
                let cmd = p::set_active_stage(stage, d.zero_based)?;
                self.t()?.transact(&cmd)?;
                mirror_active(&mut self.settings, stage);
                Ok(())
            }
            SettingKey::ReportRate => {
                let Some(idx) = p::RATES_HZ
                    .iter()
                    .position(|&hz| Some(i64::from(hz)) == int)
                else {
                    return Ok(());
                };
                self.t()?
                    .transact(&p::set_field(spec.stages, idx as u8))
                    .map(drop)
            }
            SettingKey::Debounce => {
                let ms = int.context("debounce not set")? as u8;
                self.t()?
                    .transact(&p::set_field(spec.stages + 1, p::debounce_wire(ms)))
                    .map(drop)
            }
            SettingKey::AngleSnapping => {
                let on = value.and_then(|v| v.as_bool()).unwrap_or(false);
                self.t()?
                    .transact(&p::set_field(spec.stages + 2, u8::from(on)))
                    .map(drop)
            }
            SettingKey::LiftOffDistance => {
                let high = value.as_ref().and_then(|v| v.as_str()) == Some("high");
                self.t()?.transact(&p::set_lod(spec.lod, high)).map(drop)
            }
            SettingKey::PmShutdown => {
                let wire = int
                    .and_then(|m| u8::try_from(m).ok())
                    .and_then(p::auto_off_wire)
                    .context("unsupported auto-off time")?;
                let low = self.low_battery.context("battery state not read yet")?;
                self.t()?.transact(&p::set_energy(wire, low)).map(drop)
            }
            k if is_lighting(k) => self.write_lighting(),
            _ => Ok(()),
        }
    }

    /// Writes the active onboard stage, so the DPI button keeps cycling.
    fn write_dpi(&mut self, dpi: u16) -> Result<()> {
        let spec = self.spec;
        let Some(d) = spec.dpi.filter(|d| d.active) else {
            return Ok(());
        };
        let stage = p::parse_stage(
            &self.t()?.transact(p::GET_PROFILE)?,
            spec.stages,
            d.zero_based,
        );
        self.write_stage(stage, dpi)?;
        merge_setting(
            &mut self.settings,
            stage_key(stage),
            SettingValue::Int(dpi.into()),
        );
        merge_setting(
            &mut self.settings,
            SettingKey::DpiStageActive,
            SettingValue::Int(stage.into()),
        );
        Ok(())
    }

    fn write_stage(&mut self, stage: u8, dpi: u16) -> Result<()> {
        let d = self.spec.dpi.context("no DPI control")?;
        let t = self.t()?;
        let color = if d.colors {
            Some(p::parse_dpi_color(&t.transact(p::GET_DPI_COLORS)?, stage))
        } else {
            None
        };
        t.transact(&p::set_dpi(stage, dpi, d.step, color))?;
        Ok(())
    }

    fn write_lighting(&mut self) -> Result<()> {
        let spec = self.spec;
        let effect = effect_of(&self.settings);
        let pct = int_of(&self.settings, SettingKey::Brightness, 100) as u8;
        let brightness = p::scale_brightness(pct, spec.max_brightness);
        let rgb = rgb_of(&self.settings);
        let t = self.t()?;
        for &zone in spec.zones {
            t.transact(&p::set_led(zone, effect, brightness, rgb))?;
        }
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        let config_pid = self
            .spec
            .pids
            .first()
            .copied()
            .unwrap_or(self.id.product_id);
        hako_core::config::save_device_settings(&self.paths, VID, config_pid, &self.settings)
    }
}

/// Record `stage` as active and copy its DPI into `dpi`.
fn mirror_active(settings: &mut SettingsMap, stage: u8) {
    merge_setting(
        settings,
        SettingKey::DpiStageActive,
        SettingValue::Int(stage.into()),
    );
    match get_setting(settings, stage_key(stage)).cloned() {
        Some(v) => merge_setting(settings, SettingKey::Dpi, v),
        None => {
            settings.remove(SettingKey::Dpi.as_str());
        }
    }
}

/// Take the values read from the mouse (`hw`) for unset keys. The active
/// stage is always the mouse's, as the DPI button may have moved it while
/// hakod was off or the mouse asleep (also on models with a stage dropdown:
/// the stage poll treats the button as the truth too), and `dpi` follows that
/// stage, so a stale saved `dpi` is never written into it.
fn adopt(spec: &MouseSpec, settings: &mut SettingsMap, hw: SettingsMap) {
    let stage = get_setting(&hw, SettingKey::DpiStageActive).and_then(|v| v.as_int());
    for (k, v) in hw {
        settings.entry(k).or_insert(v);
    }
    let active = spec.dpi.is_some_and(|d| d.active);
    if let Some(stage) = stage.and_then(|s| u8::try_from(s).ok()).filter(|_| active) {
        mirror_active(settings, stage);
    }
}

/// Whether `connect` re-sends the lift-off distance: on `Lod::Calibrated`
/// mice that write also resets the surface calibration, so only when the
/// mouse reported a different value.
fn resend_lod(lod: Lod, wanted: Option<&SettingValue>, hw: Option<&SettingValue>) -> bool {
    lod != Lod::Calibrated || hw.is_some_and(|hw| Some(hw) != wanted)
}

fn is_lighting(key: SettingKey) -> bool {
    matches!(
        key,
        SettingKey::Effect
            | SettingKey::Brightness
            | SettingKey::ColorR
            | SettingKey::ColorG
            | SettingKey::ColorB
    )
}

/// The active profile's performance settings, as far as `spec` reads them.
fn read_performance(spec: &MouseSpec, t: &mut impl Exchange) -> Result<SettingsMap> {
    let mut hw = SettingsMap::new();
    let s = t.transact(&p::get_settings(false))?;
    if let Some(hz) = p::parse_rate(&s, spec.stages).filter(|hz| spec.rates.contains(hz)) {
        merge_setting(
            &mut hw,
            SettingKey::ReportRate,
            SettingValue::Int(hz.into()),
        );
    }
    if spec.snapping {
        let on = p::parse_snapping(&s, spec.stages);
        merge_setting(&mut hw, SettingKey::AngleSnapping, SettingValue::Bool(on));
    }
    if spec.debounce {
        let ms = p::parse_debounce(&s, spec.stages);
        if p::DEBOUNCE_MS.contains(&ms) {
            merge_setting(&mut hw, SettingKey::Debounce, SettingValue::Int(ms.into()));
        }
    }
    if let Some(d) = spec.dpi {
        let s = if d.xy {
            t.transact(&p::get_settings(true))?
        } else {
            s
        };
        for stage in 1..=spec.stages {
            let dpi = p::parse_dpi(&s, stage, d.xy, d.step);
            if (d.min..=d.max).contains(&dpi) {
                merge_setting(&mut hw, stage_key(stage), SettingValue::Int(dpi.into()));
            }
        }
        if d.active {
            let stage = p::parse_stage(&t.transact(p::GET_PROFILE)?, spec.stages, d.zero_based);
            merge_setting(
                &mut hw,
                SettingKey::DpiStageActive,
                SettingValue::Int(stage.into()),
            );
            if let Some(v) = get_setting(&hw, stage_key(stage)).cloned() {
                merge_setting(&mut hw, SettingKey::Dpi, v);
            }
        }
    }
    if spec.lod != Lod::None {
        let high = p::parse_lod(spec.lod, &t.transact(p::GET_LOD)?);
        let v = if high { "high" } else { "low" };
        merge_setting(
            &mut hw,
            SettingKey::LiftOffDistance,
            SettingValue::Text(v.into()),
        );
    }
    Ok(hw)
}

/// Keys that follow the DPI button between stage polls: a difference there is
/// the user pressing it, not a failed write.
const UNCHECKED: [SettingKey; 2] = [SettingKey::Dpi, SettingKey::DpiStageActive];

/// Every value read back from the active profile that the user has set must
/// match it, so a dropped or refused write is never made permanent.
fn check_profile(expected: &SettingsMap, hw: &SettingsMap) -> Result<()> {
    let diff: Vec<String> = hw
        .iter()
        .filter_map(|(k, got)| {
            let key = SettingKey::parse(k).filter(|k| !UNCHECKED.contains(k))?;
            let want = get_setting(expected, key).filter(|want| *want != got)?;
            Some(format!("{k} reads {got:?}, expected {want:?}"))
        })
        .collect();
    if !diff.is_empty() {
        bail!(
            "not saved: the mouse's active profile does not hold your settings ({})",
            diff.join("; ")
        );
    }
    Ok(())
}

/// Read the active profile back, check it, then `50 03` (store it).
fn store_profile(spec: &MouseSpec, t: &mut impl Exchange, expected: &SettingsMap) -> Result<()> {
    let hw = read_performance(spec, t)?;
    check_profile(expected, &hw)?;
    t.transact(p::MOUSE_SAVE).context("save to device memory")?;
    Ok(())
}

pub fn controls(spec: &MouseSpec, settings: &SettingsMap) -> Vec<SettingMeta> {
    use SettingKey::*;
    let mut c = Vec::new();
    if let Some(d) = spec.dpi {
        let (min, max, step) = (d.min.into(), d.max.into(), d.step.into());
        if d.active {
            c.push(SettingMeta::slider(Dpi, "DPI", min, max, step).unit("dpi"));
        }
        for stage in 1..=spec.stages {
            let label = format!("DPI stage {stage}");
            c.push(SettingMeta::slider(stage_key(stage), &label, min, max, step).unit("dpi"));
        }
        if d.active && spec.stage_select {
            let opts = (1..=spec.stages)
                .map(|s| SettingOption::int(s.into(), &format!("Stage {s}")))
                .collect();
            c.push(SettingMeta::dropdown(
                DpiStageActive,
                "Active DPI stage",
                opts,
            ));
        }
    }
    if !spec.rates.is_empty() {
        let opts = spec
            .rates
            .iter()
            .map(|&hz| SettingOption::int(hz.into(), &format!("{hz} Hz")))
            .collect();
        c.push(SettingMeta::dropdown(ReportRate, "Report rate", opts));
    }
    if spec.debounce {
        let opts = p::DEBOUNCE_MS
            .iter()
            .map(|&ms| SettingOption::int(ms.into(), &format!("{ms} ms")))
            .collect();
        c.push(SettingMeta::dropdown(Debounce, "Button debounce", opts));
    }
    if spec.snapping {
        c.push(SettingMeta::toggle(AngleSnapping, "Angle snapping"));
    }
    if spec.lod != Lod::None {
        let lod = SettingMeta::dropdown(
            LiftOffDistance,
            "Lift-off distance",
            vec![
                SettingOption::text("low", "Low"),
                SettingOption::text("high", "High"),
            ],
        );
        c.push(if spec.lod == Lod::Calibrated {
            lod.warning("Changing the lift-off distance also resets the surface calibration.")
        } else {
            lod
        });
    }
    if !spec.zones.is_empty() {
        let step = if spec.max_brightness == 4 { 25.0 } else { 1.0 };
        c.extend(lighting_controls(effect_of(settings), step, false));
    }
    if spec.auto_off {
        let opts = [0, 1, 2, 3, 5, 10]
            .into_iter()
            .map(|m| match m {
                0 => SettingOption::int(0, "Never"),
                m => SettingOption::int(m, &format!("{m} min")),
            })
            .collect();
        c.push(SettingMeta::dropdown(PmShutdown, "Auto-off", opts));
    }
    // libratbag and G-Helper store the active profile with `50 03` on these
    // mice (see docs/onboard-writes/asus.md).
    c.push(SettingMeta::toggle(SaveToDevice, "Save changes to device memory").persistent());
    c
}

impl DeviceAdapter for MouseAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Mouse,
            name: self.spec.name.into(),
            connected: self.t.is_some(),
            batteries: self.battery.iter().cloned().collect(),
            settings: self.settings.clone(),
            controls: controls(self.spec, &self.settings),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value);
        // Turning save mode on stores the current profile once; off writes nothing.
        let result = if key == SettingKey::SaveToDevice {
            if self.saving() {
                self.store()
            } else {
                Ok(())
            }
        } else {
            self.apply(key)
        };
        if let Err(e) = result {
            // The device kept its old value; so do the UI and the file.
            self.settings = before;
            self.last_error = Some(e.to_string());
            if is_lighting(key) && self.spec.zones.len() > 1 {
                // Zones written before the failure get the old lighting back.
                let _ = self.write_lighting();
            }
            return Err(e);
        }
        // The change is live; storing it is a separate step that can fail.
        let stored = if key != SettingKey::SaveToDevice && self.saving() {
            self.store()
                .context("changed, but not saved to device memory")
        } else {
            Ok(())
        };
        self.last_error = stored.as_ref().err().map(|e| format!("{e:#}"));
        self.persist()
            .map_err(|e| anyhow::anyhow!("applied, but not saved: {e:#}"))?;
        stored
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        if self.spec.battery != Battery::None {
            let every = if self.online {
                BATTERY_POLL
            } else {
                RELINK_POLL
            };
            if self.last_poll.elapsed() >= every {
                let was_online = self.online;
                let _ = self.poll_battery();
                if self.online && !was_online {
                    info!("{} link up; re-applying settings", self.spec.name);
                    self.connect();
                }
            }
        }
        if self.online && self.last_stage_poll.elapsed() >= STAGE_POLL {
            let _ = self.poll_stage();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.t = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::asus::mouse_models::{MICE, UNLISTED};

    #[test]
    fn table_is_consistent() {
        for m in MICE.iter().chain(UNLISTED) {
            assert!(!m.pids.is_empty() || !m.omni.is_empty(), "{}", m.name);
            assert!(matches!(m.stages, 2 | 4), "{}", m.name);
            assert!(m.zones.is_empty() || matches!(m.max_brightness, 4 | 100));
            assert!(
                m.rates.iter().all(|hz| p::RATES_HZ.contains(hz)),
                "{}",
                m.name
            );
            assert!(m.rx.iter().all(|pid| m.pids.contains(pid)), "{}", m.name);
            if let Some(d) = m.dpi {
                assert!(d.min % d.step == 0 && d.max % d.step == 0, "{}", m.name);
            }
            let c = controls(m, &SettingsMap::new());
            assert!(
                !c.is_empty() || m.battery != Battery::None,
                "{} exposes nothing",
                m.name
            );
            for &pid in m.omni {
                assert_eq!(find_omni(pid).map(|s| s.name), Some(m.name));
            }
        }
        for m in MICE {
            assert_eq!(find(m.pids[0]).map(|s| s.name), Some(m.name));
        }
    }

    #[test]
    fn saved_out_of_range_stage_never_reaches_the_driver() {
        let m = MICE
            .iter()
            .find(|m| {
                controls(m, &SettingsMap::new())
                    .iter()
                    .any(|c| c.key == SettingKey::DpiStageActive)
            })
            .unwrap();
        for bad in [0, 5, 261] {
            let mut s = SettingsMap::new();
            merge_setting(&mut s, SettingKey::DpiStageActive, SettingValue::Int(bad));
            let c = controls(m, &s);
            crate::drivers::sanitize_saved(&mut s, &c);
            assert_eq!(get_setting(&s, SettingKey::DpiStageActive), None, "{bad}");
        }
    }

    #[test]
    fn controls_follow_capabilities() {
        let keris = find(0x1960).unwrap();
        let mut s = SettingsMap::new();
        lighting_defaults(&mut s, false);
        let c = controls(keris, &s);
        let keys: Vec<_> = c.iter().map(|m| m.key).collect();
        assert!(keys.contains(&SettingKey::Dpi));
        assert!(keys.contains(&SettingKey::DpiStage4));
        assert!(keys.contains(&SettingKey::Debounce));
        // G-Helper KerisWireless.CanChangeDPIProfile() == false.
        assert!(!keys.contains(&SettingKey::DpiStageActive));
        assert!(keys.contains(&SettingKey::PmShutdown));
        assert!(keys.contains(&SettingKey::LiftOffDistance));
        let dpi = c.iter().find(|m| m.key == SettingKey::Dpi).unwrap();
        assert!(dpi.validate(&SettingValue::Int(16_000)).is_ok());
        assert!(dpi.validate(&SettingValue::Int(16_100)).is_err());
        let rate = c.iter().find(|m| m.key == SettingKey::ReportRate).unwrap();
        assert!(rate.validate(&SettingValue::Int(1000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(2000)).is_err());
        let db = c.iter().find(|m| m.key == SettingKey::Debounce).unwrap();
        assert!(db.validate(&SettingValue::Int(12)).is_ok());
        assert!(db.validate(&SettingValue::Int(8)).is_err());

        let m4 = find(0x1a03).unwrap();
        let keys: Vec<_> = controls(m4, &s).iter().map(|m| m.key).collect();
        assert!(!keys.contains(&SettingKey::Dpi));
        assert!(!keys.contains(&SettingKey::DpiStage1));
        assert!(!keys.contains(&SettingKey::Effect));

        // 2-stage mouse with stage switching (G GladiusIIOrigin).
        let g2 = find(0x1877).unwrap();
        let c = controls(g2, &s);
        let keys: Vec<_> = c.iter().map(|m| m.key).collect();
        assert!(keys.contains(&SettingKey::DpiStage2));
        assert!(!keys.contains(&SettingKey::DpiStage3));
        let active = c
            .iter()
            .find(|m| m.key == SettingKey::DpiStageActive)
            .unwrap();
        assert!(active.validate(&SettingValue::Int(2)).is_ok());
        assert!(active.validate(&SettingValue::Int(3)).is_err());

        // Strix Carry: sources disagree on the active-stage byte, stages only.
        let carry = find(0x18b4).unwrap();
        let keys: Vec<_> = controls(carry, &s).iter().map(|m| m.key).collect();
        assert!(keys.contains(&SettingKey::DpiStage1));
        assert!(!keys.contains(&SettingKey::Dpi));
        assert!(!keys.contains(&SettingKey::DpiStageActive));
    }

    #[test]
    fn receivers_and_variants_resolve() {
        // G-Helper MouseFromOmniPid.
        assert_eq!(
            find_omni(0x1a94).unwrap().name,
            "ASUS ROG Harpe Ace Aim Lab Edition"
        );
        assert_eq!(find_omni(0x1b18).unwrap().name, "ASUS ROG Keris II Ace");
        assert_eq!(
            find_omni(0x1ad7).unwrap().name,
            "ASUS ROG Strix Impact III Wireless"
        );
        assert!(find_omni(0x1a85).is_none());
        // G-Helper DetectHarpeIIWireless: product name with EXTREME.
        let harpe = find(0x1ad0).unwrap();
        assert!(harpe.rx.contains(&0x1ad0));
        assert_eq!(variant(harpe, "ASUSTeK ROG HARPE II ACE").name, harpe.name);
        let ex = variant(harpe, "ASUSTeK ROG HARPE II EXTREME EDITION 20");
        assert_eq!(ex.name, "ASUS ROG Harpe II Extreme Edition 2.0");
        assert_eq!(ex.dpi.unwrap().step, 1);
        // Step 1: 26000 dpi → raw 25999 (G-Helper (dpi - 1) / 1).
        assert_eq!(
            p::set_dpi(1, 26_000, 1, None),
            vec![0x51, 0x31, 0x00, 0x00, 0x8f, 0x65]
        );
    }

    #[test]
    fn connect_adopts_the_mouses_active_stage() {
        // Keris Wireless: active-stage DPI, no stage dropdown.
        let keris = find(0x1960).unwrap();
        let int = |m: &SettingsMap, k| get_setting(m, k).and_then(|v| v.as_int());
        let mut saved = SettingsMap::new();
        for (k, v) in [
            (SettingKey::DpiStage1, 800),
            (SettingKey::DpiStage2, 1600),
            (SettingKey::DpiStageActive, 1),
            (SettingKey::Dpi, 800),
        ] {
            merge_setting(&mut saved, k, SettingValue::Int(v));
        }
        // The DPI button moved the mouse to stage 2 while hakod was off.
        let mut hw = SettingsMap::new();
        for (k, v) in [
            (SettingKey::DpiStage1, 400),
            (SettingKey::DpiStage2, 3200),
            (SettingKey::DpiStage3, 6400),
            (SettingKey::DpiStageActive, 2),
            (SettingKey::Dpi, 3200),
        ] {
            merge_setting(&mut hw, k, SettingValue::Int(v));
        }
        adopt(keris, &mut saved, hw);
        assert_eq!(int(&saved, SettingKey::DpiStageActive), Some(2));
        // The user's stage values win; `dpi` is the active one's.
        assert_eq!(int(&saved, SettingKey::DpiStage2), Some(1600));
        assert_eq!(int(&saved, SettingKey::Dpi), Some(1600));
        assert_eq!(int(&saved, SettingKey::DpiStage3), Some(6400));
    }

    #[test]
    fn reconnect_keeps_the_surface_calibration() {
        let (low, high) = (
            SettingValue::Text("low".into()),
            SettingValue::Text("high".into()),
        );
        // `51 35 FF 00 FF <v>` also recalibrates: only a real change is sent.
        assert!(!resend_lod(Lod::Calibrated, Some(&low), Some(&low)));
        assert!(resend_lod(Lod::Calibrated, Some(&high), Some(&low)));
        assert!(!resend_lod(Lod::Calibrated, Some(&high), None));
        assert!(resend_lod(Lod::Plain, Some(&low), Some(&low)));

        let calibrated = MICE.iter().find(|m| m.lod == Lod::Calibrated).unwrap();
        let c = controls(calibrated, &SettingsMap::new());
        let lod = c.iter().find(|m| m.key == SettingKey::LiftOffDistance);
        assert!(lod.unwrap().warning.is_some());
    }

    /// A mouse whose active profile answers `12 04 00` / `12 00` with fixed
    /// replies and records every command.
    struct FakeMouse {
        settings: p::Packet,
        profile: p::Packet,
        reject_save: bool,
        sent: Vec<Vec<u8>>,
    }

    impl Exchange for FakeMouse {
        fn transact(&mut self, cmd: &[u8]) -> Result<p::Packet> {
            self.sent.push(cmd.to_vec());
            match cmd {
                [0x12, 0x04, 0x00] => Ok(self.settings),
                [0x12, 0x00] => Ok(self.profile),
                [0x50, 0x03] if self.reject_save => bail!("device rejected 50 03"),
                [0x50, 0x03] => Ok(p::packet(cmd)),
                _ => bail!("unexpected {cmd:02x?}"),
            }
        }
    }

    #[test]
    fn save_checks_the_active_profile_first() {
        // Gladius II Wireless: 2 stages, step 100, no lift-off read.
        let spec = find(0x18a0).unwrap();
        assert_eq!(spec.lod, Lod::None);
        // _asus_dpi2_data: dpi 800 / 1600 (raw 7 / 15), rate idx 3, debounce
        // idx 2 (12 ms), snapping off; `12 00` byte 11 = stage 1.
        let settings = p::packet(&[
            0x12, 0x04, 0x00, 0x00, 0x07, 0x00, 0x0f, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00,
        ]);
        let mut profile = p::packet(&[0x12, 0x00]);
        profile[11] = 1;
        let mouse = |reject_save| FakeMouse {
            settings,
            profile,
            reject_save,
            sent: vec![],
        };
        let mut want = SettingsMap::new();
        for (k, v) in [
            (SettingKey::DpiStage1, SettingValue::Int(800)),
            (SettingKey::DpiStage2, SettingValue::Int(1600)),
            (SettingKey::ReportRate, SettingValue::Int(1000)),
            (SettingKey::Debounce, SettingValue::Int(12)),
            (SettingKey::AngleSnapping, SettingValue::Bool(false)),
            // Follows the DPI button: not compared.
            (SettingKey::Dpi, SettingValue::Int(1600)),
            (SettingKey::Effect, SettingValue::Text("static".into())),
        ] {
            merge_setting(&mut want, k, v);
        }

        let mut m = mouse(false);
        store_profile(spec, &mut m, &want).unwrap();
        assert_eq!(m.sent.last().unwrap(), &vec![0x50, 0x03]);
        assert_eq!(m.sent.iter().filter(|c| c[0] == 0x50).count(), 1);

        // A value the mouse does not hold aborts before the save.
        let mut stale = want.clone();
        merge_setting(&mut stale, SettingKey::DpiStage2, SettingValue::Int(3200));
        let mut m = mouse(false);
        let err = store_profile(spec, &mut m, &stale).unwrap_err().to_string();
        assert!(err.contains("dpi_stage_2"), "{err}");
        assert!(m.sent.iter().all(|c| c[0] != 0x50));

        // A refused save is an error.
        let mut m = mouse(true);
        assert!(store_profile(spec, &mut m, &want).is_err());

        // Every mouse offers the opt-in mode, as an onboard-memory control.
        let c = controls(spec, &SettingsMap::new());
        let save = c
            .iter()
            .find(|m| m.key == SettingKey::SaveToDevice)
            .unwrap();
        assert!(save.persistent && save.blocked.is_none());
    }

    /// Read-only: profile, settings and battery of the first ASUS mouse found.
    #[test]
    #[ignore]
    fn hw_read_first_mouse() {
        let list = crate::hid::enumerate_hidraw().unwrap();
        let (spec, pid) = list
            .iter()
            .filter(|d| d.vendor_id == VID)
            .find_map(|d| find(d.product_id).map(|s| (s, d.product_id)))
            .expect("no supported ASUS mouse");
        let (mut t, path) = if spec.rx.contains(&pid) {
            Transport::open(pid, p::OMNI_IFACE, p::OMNI_MOUSE_REPORT).unwrap()
        } else {
            Transport::open(pid, spec.iface, 0).unwrap()
        };
        eprintln!("{} at {path}", spec.name);
        for cmd in [p::GET_PROFILE, &p::get_settings(false), p::GET_BATTERY] {
            match t.transact(cmd) {
                Ok(r) => eprintln!("{cmd:02x?} -> {:02x?}", &r[..20]),
                Err(e) => eprintln!("{cmd:02x?} -> {e}"),
            }
        }
    }
}
