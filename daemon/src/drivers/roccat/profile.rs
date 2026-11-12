//! Older Kone mice that keep every setting in onboard profiles.
//!
//! Layouts from libratbag `driver-roccat-kone-pure.c` (Kone Pure),
//! `driver-roccat.c` (Kone XTD) and `driver-roccat-kone-emp.c` (Kone EMP), IDs
//! from `data/devices/roccat-kone-{pure,xtd,emp}.device`; cross-checked with
//! the Linux kernel `hid-roccat-koneplus.c` / `hid-roccat-common.c` (Kone XTD
//! `0x2e22`: report 6 is 0x2b bytes, selected by control report
//! `04 <profile> 80`, writes acknowledged on report 4: 1 ok, 3 busy, 0/2/4
//! error) and roccat-tools `koneplus.h` / `koneplus_profile_settings.c`
//! (byte layout, checksum = 16-bit sum of every byte before it, little-endian).
//! Reading: report 5 names the active profile, `04 <profile> 80` selects its
//! settings, then report 6 holds them.
//!
//! Only the Kone XTD writes (DPI stages, active stage, report rate of the
//! active profile): every source agrees on its layout and checksum, and two
//! captures of the factory profile (libratbag issue #1466, Eruption
//! `roccat_kone_xtd.rs`) check the sum. A write reads the block, validates it,
//! changes one field, writes it back and reads it back to compare. Kone Pure
//! (libratbag sends no checksum, roccat-tools does) and Kone EMP (libratbag
//! only) stay read-only with blocked placeholders.

use super::{nodes, open_info, Spec, ROCCAT};
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{report_ids, HidrawDevice};
use anyhow::{bail, ensure, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::merge_setting;
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// 31-byte settings: dpi mask 6, x res 7..12 (× 50), current 12, rate 19.
    Pure,
    /// 43-byte settings, same leading layout as the Pure; y res 13..18,
    /// checksum 41..43.
    Xtd,
    /// 41-byte settings: dpi mask 6, x res 7..12 (× 100 + 100), current 17,
    /// rate 18.
    Emp,
}

pub(super) const KONE_PURE: Spec<Family> = Spec {
    name: "ROCCAT Kone Pure",
    vid: ROCCAT,
    pids: &[0x2dc2, 0x2dbe, 0x2dcb],
    family: Family::Pure,
};
pub(super) const KONE_XTD: Spec<Family> = Spec {
    name: "ROCCAT Kone XTD",
    vid: ROCCAT,
    pids: &[0x2e22],
    family: Family::Xtd,
};
pub(super) const KONE_EMP: Spec<Family> = Spec {
    name: "ROCCAT Kone EMP",
    vid: ROCCAT,
    pids: &[0x2e24],
    family: Family::Emp,
};

#[cfg(test)]
pub(super) const ALL: &[&Spec<Family>] = &[&KONE_PURE, &KONE_XTD, &KONE_EMP];

const REPORT_RATES: [i64; 4] = [125, 250, 500, 1000];
const REFRESH: Duration = Duration::from_secs(10);
const STAGES: [SettingKey; 5] = [
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
    SettingKey::DpiStage5,
];
/// roccat-tools `KONEXTD_PROFILE_SETTING_CPI_MIN/MAX` (1..41 × 200 cpi);
/// libratbag accepts the same range.
const XTD_DPI: (i64, i64, i64) = (200, 8200, 200);

/// Stage range shown per family: XTD as above; Pure from libratbag's write
/// check (200..8200 step 50); EMP from its encoding (0..0x77 × 100 + 100).
fn dpi_range(f: Family) -> (i64, i64, i64) {
    match f {
        Family::Xtd => XTD_DPI,
        Family::Pure => (200, 8200, 50),
        Family::Emp => (100, 12000, 100),
    }
}
const XTD_LEN: usize = 0x2b;

const PURE_BLOCKED: &str = "sources disagree on the checksum (libratbag sends none, \
roccat-tools a byte sum)";
const EMP_BLOCKED: &str = "single source (libratbag); not in the kernel or roccat-tools";
const LIGHT_BLOCKED: &str = "lighting layout from one source only (roccat-tools)";

impl Family {
    fn settings_len(self) -> usize {
        match self {
            Family::Pure => 31,
            Family::Xtd => XTD_LEN,
            Family::Emp => 41,
        }
    }
}

/// (current DPI, report rate in Hz) from a settings report; `None` when the
/// active slot is out of range or disabled.
fn parse(f: Family, b: &[u8]) -> Option<(i64, Option<i64>)> {
    if b.len() < f.settings_len() || b[0] != 0x06 {
        return None;
    }
    let (current, rate) = match f {
        Family::Pure | Family::Xtd => (b[12], b[19]),
        Family::Emp => (b[17], b[18]),
    };
    if current >= 5 || b[6] & (1 << current) == 0 {
        return None;
    }
    let x = i64::from(b[7 + usize::from(current)]);
    let dpi = match f {
        Family::Pure | Family::Xtd => x * 50,
        Family::Emp => x * 100 + 100,
    };
    Some((dpi, REPORT_RATES.get(usize::from(rate)).copied()))
}

/// Little-endian 16-bit sum of every byte before the last two.
fn byte_sum(b: &[u8]) -> [u8; 2] {
    b[..b.len() - 2]
        .iter()
        .fold(0u16, |a, &x| a.wrapping_add(u16::from(x)))
        .to_le_bytes()
}

/// Checks a Kone XTD settings block read for `profile`; `Err` names what is
/// wrong, and nothing is written.
fn validate_xtd(b: &[u8], profile: u8) -> Result<()> {
    ensure!(b.len() == XTD_LEN, "settings block is {} bytes", b.len());
    ensure!(
        b[0] == 0x06 && b[1] == 0x2b,
        "unexpected settings header {:02x} {:02x}",
        b[0],
        b[1]
    );
    ensure!(
        b[2] == profile,
        "settings are for profile {}, not {profile}",
        b[2]
    );
    ensure!(b[41..] == byte_sum(b), "settings checksum mismatch");
    let mask = b[6];
    ensure!(
        mask != 0 && mask < 0x20,
        "invalid DPI stage mask {mask:#04x}"
    );
    ensure!(
        b[12] < 5 && mask & (1 << b[12]) != 0,
        "invalid active stage {}",
        b[12]
    );
    for i in 0..5 {
        if mask & (1 << i) != 0 {
            ensure!(
                (1..=164).contains(&b[7 + i]),
                "invalid DPI value {:#04x} in stage {}",
                b[7 + i],
                i + 1
            );
        }
    }
    ensure!(b[19] < 4, "invalid report rate code {}", b[19]);
    Ok(())
}

/// One change to a Kone XTD settings block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// Stage 0..5 to DPI (x and y, as libratbag and roccat-tools write).
    Stage(usize, i64),
    /// Active (startup) stage 0..5; must be enabled.
    Active(u8),
    /// Report rate in Hz.
    Rate(i64),
}

/// `b` with `change` applied and the checksum renewed.
fn apply_xtd(b: &[u8], change: Change) -> Result<Vec<u8>> {
    let mut out = b.to_vec();
    match change {
        Change::Stage(i, dpi) => {
            let (min, max, step) = XTD_DPI;
            ensure!(
                i < 5 && (min..=max).contains(&dpi) && dpi % step == 0,
                "DPI {dpi} is not a Kone XTD value"
            );
            let raw = (dpi / 50) as u8;
            out[7 + i] = raw;
            out[13 + i] = raw;
        }
        Change::Active(i) => {
            ensure!(
                i < 5 && out[6] & (1 << i) != 0,
                "stage {} is disabled in this profile",
                i + 1
            );
            out[12] = i;
        }
        Change::Rate(hz) => {
            let code = REPORT_RATES
                .iter()
                .position(|&r| r == hz)
                .with_context(|| format!("{hz} Hz is not a Kone XTD report rate"))?;
            out[19] = code as u8;
        }
    }
    let sum = byte_sum(&out);
    out[41..].copy_from_slice(&sum);
    Ok(out)
}

/// Feature-report access, so the write sequence can run against a fake.
trait Feature {
    fn get(&self, buf: &mut [u8]) -> Result<()>;
    fn set(&self, buf: &mut [u8]) -> Result<()>;
}

impl Feature for HidrawDevice {
    fn get(&self, buf: &mut [u8]) -> Result<()> {
        self.get_feature(buf)
    }
    fn set(&self, buf: &mut [u8]) -> Result<()> {
        self.set_feature(buf)
    }
}

/// Polls control report 4 after a write: 1 = done, 3 = busy, anything else
/// is the device refusing it (kernel `roccat_common2_receive_control_status`).
fn status(dev: &impl Feature, pause: Duration) -> Result<()> {
    for _ in 0..40 {
        std::thread::sleep(pause);
        let mut buf = [0x04, 0, 0];
        dev.get(&mut buf)?;
        match buf[1] {
            1 => return Ok(()),
            3 => std::thread::sleep(pause * 5),
            v => bail!("device rejected the request (status {v})"),
        }
    }
    bail!("device stayed busy")
}

/// The active profile 0..=4 (report 5).
fn active_profile(dev: &impl Feature) -> Result<u8> {
    let mut buf = [0x05, 0, 0];
    dev.get(&mut buf)?;
    ensure!(
        buf[0] == 0x05 && buf[2] <= 4,
        "unexpected active profile report {buf:02x?}"
    );
    Ok(buf[2])
}

/// Reads profile `profile`'s settings block (`len` bytes).
fn read_settings(dev: &impl Feature, profile: u8, len: usize, pause: Duration) -> Result<Vec<u8>> {
    dev.set(&mut [0x04, profile, 0x80])?;
    status(dev, pause)?;
    let mut buf = vec![0u8; len];
    buf[0] = 0x06;
    dev.get(&mut buf)?;
    Ok(buf)
}

/// Read, validate, change, write and read back the active profile of a
/// Kone XTD. Returns the block now on the device.
fn write_xtd(dev: &impl Feature, change: Change, pause: Duration) -> Result<Vec<u8>> {
    let profile = active_profile(dev)?;
    let old = read_settings(dev, profile, XTD_LEN, pause)?;
    validate_xtd(&old, profile).context("not writing: the stored profile looks invalid")?;
    let mut new = apply_xtd(&old, change)?;
    if new == old {
        return Ok(old);
    }
    let sent = new.clone();
    dev.set(&mut new)?;
    status(dev, pause).context("profile write")?;
    let back = read_settings(dev, profile, XTD_LEN, pause)?;
    ensure!(
        back == sent,
        "read-back differs from the written profile; check the mouse in ROCCAT Swarm"
    );
    Ok(back)
}

pub(super) fn open(spec: &'static Spec<Family>, pid: u16, _ctx: &DriverCtx) -> Opened {
    let list = nodes(spec.vid, pid)?;
    let info = list
        .iter()
        .find(|d| report_ids(&d.report_descriptor()).contains(&0x06))
        .or(list.first())
        .with_context(|| format!("no hidraw node for {:04x}:{pid:04x}", spec.vid))?;
    let (dev, instance) = open_info(info)?;
    let mut m = Mouse {
        spec,
        id: DeviceId {
            vendor_id: spec.vid,
            product_id: pid,
            instance,
        },
        dev: Some(dev),
        state: SettingsMap::new(),
        last_read: None,
        last_error: None,
    };
    m.refresh();
    Ok(vec![Box::new(m)])
}

struct Mouse {
    spec: &'static Spec<Family>,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    state: SettingsMap,
    last_read: Option<Instant>,
    last_error: Option<String>,
}

const PAUSE: Duration = Duration::from_millis(20);

impl Mouse {
    fn read(&self) -> Result<Vec<u8>> {
        let dev = self.dev.as_ref().context("not connected")?;
        let profile = active_profile(dev)?;
        read_settings(dev, profile, self.spec.family.settings_len(), PAUSE)
    }

    /// Show `b`'s current DPI, report rate and (on the XTD) every stage.
    fn show(&mut self, b: &[u8]) -> Result<()> {
        let (dpi, rate) = parse(self.spec.family, b).context("unexpected settings report")?;
        self.state.clear();
        merge_setting(&mut self.state, SettingKey::Dpi, SettingValue::Int(dpi));
        if let Some(r) = rate {
            merge_setting(
                &mut self.state,
                SettingKey::ReportRate,
                SettingValue::Int(r),
            );
        }
        if self.spec.family == Family::Xtd {
            for (i, key) in STAGES.into_iter().enumerate() {
                let v = i64::from(b[7 + i]) * 50;
                merge_setting(&mut self.state, key, SettingValue::Int(v));
            }
            let active = i64::from(b[12]) + 1;
            merge_setting(
                &mut self.state,
                SettingKey::DpiStageActive,
                SettingValue::Int(active),
            );
        }
        Ok(())
    }

    fn refresh(&mut self) {
        self.last_read = Some(Instant::now());
        match self.read().and_then(|b| self.show(&b)) {
            Ok(()) => self.last_error = None,
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    fn controls(&self) -> Vec<SettingMeta> {
        use SettingKey::*;
        if self.state.is_empty() {
            return Vec::new();
        }
        let blocked = match self.spec.family {
            Family::Xtd => None,
            Family::Pure => Some(PURE_BLOCKED),
            Family::Emp => Some(EMP_BLOCKED),
        };
        let gate = |m: SettingMeta| match blocked {
            Some(reason) => m.persistent().blocked(reason),
            None => m.persistent(),
        };
        let (min, max, step) = dpi_range(self.spec.family);
        let mut c = vec![SettingMeta::readonly(Dpi, "Current DPI", 0.0, 12750.0).unit("dpi")];
        for (i, key) in STAGES.into_iter().enumerate() {
            c.push(gate(
                SettingMeta::slider(
                    key,
                    &format!("DPI stage {}", i + 1),
                    min as f64,
                    max as f64,
                    step as f64,
                )
                .unit("dpi"),
            ));
        }
        c.push(gate(SettingMeta::dropdown(
            DpiStageActive,
            "Active DPI stage",
            (1..=5)
                .map(|i| SettingOption::int(i, &i.to_string()))
                .collect(),
        )));
        c.push(gate(SettingMeta::dropdown(
            ReportRate,
            "Report rate",
            REPORT_RATES
                .iter()
                .map(|&r| SettingOption::int(r, &format!("{r} Hz")))
                .collect(),
        )));
        let (effects, reason): (&[(&str, &str)], _) = match self.spec.family {
            // roccat-tools `KoneplusProfileSettingsLightEffectType`.
            Family::Xtd => (
                &[
                    ("off", "Off"),
                    ("static", "Fully lighted"),
                    ("blinking", "Blinking"),
                    ("breathing", "Breathing"),
                    ("heartbeat", "Heartbeat"),
                ],
                LIGHT_BLOCKED,
            ),
            // roccat-tools `KonepureProfileSettingsLightEffectType`.
            Family::Pure => (
                &[
                    ("off", "Off"),
                    ("static", "Fully lighted"),
                    ("breathing", "Breathing"),
                ],
                PURE_BLOCKED,
            ),
            // libratbag `roccat_settings_report.lighting_effect`.
            Family::Emp => (
                &[
                    ("static", "Fixed"),
                    ("blinking", "Blinking"),
                    ("breathing", "Breathing"),
                    ("heartbeat", "Beating"),
                ],
                EMP_BLOCKED,
            ),
        };
        c.push(
            SettingMeta::dropdown(
                Effect,
                "Onboard lighting effect",
                effects
                    .iter()
                    .map(|(v, l)| SettingOption::text(v, l))
                    .collect(),
            )
            .persistent()
            .blocked(reason),
        );
        c
    }
}

/// The change a user write asks for.
fn change_for(key: SettingKey, value: &SettingValue) -> Result<Change> {
    let v = value.as_int().context("expected a number")?;
    if let Some(i) = STAGES.iter().position(|&k| k == key) {
        return Ok(Change::Stage(i, v));
    }
    match key {
        SettingKey::DpiStageActive if (1..=5).contains(&v) => Ok(Change::Active(v as u8 - 1)),
        SettingKey::ReportRate => Ok(Change::Rate(v)),
        _ => bail!("{} is not supported", key.as_str()),
    }
}

impl DeviceAdapter for Mouse {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Mouse,
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![],
            settings: self.state.clone(),
            controls: self.controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if self.spec.family != Family::Xtd {
            bail!(
                "{} is stored in the onboard profile; not written",
                key.as_str()
            );
        }
        let change = change_for(key, &value)?;
        let dev = self.dev.as_ref().context("not connected")?;
        let result = write_xtd(dev, change, PAUSE);
        self.last_read = Some(Instant::now());
        match result {
            Ok(b) => {
                self.last_error = None;
                self.show(&b)
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_some() && self.last_read.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
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
    use std::cell::RefCell;

    /// Factory profile 0 of a Kone XTD, as read by libratbag (issue #1466)
    /// and replayed by Eruption `roccat_kone_xtd.rs`.
    const XTD_FACTORY: [u8; 43] = [
        0x06, 0x2b, 0x00, 0x00, 0x06, 0x06, 0x1f, 0x10, 0x20, 0x40, 0x80, 0xa4, 0x00, 0x10, 0x20,
        0x40, 0x80, 0xa4, 0x01, 0x03, 0x0f, 0x01, 0x01, 0x01, 0x02, 0x00, 0x64, 0x00, 0x00, 0x06,
        0xfa, 0x50, 0x00, 0x00, 0x64, 0x00, 0x00, 0x06, 0xfa, 0x50, 0x00, 0x04, 0x07,
    ];

    #[test]
    fn parses_pure_and_xtd_settings() {
        let mut b = [0u8; 31];
        b[0] = 0x06;
        b[6] = 0b00101; // slots 0 and 2 enabled
        b[7..12].copy_from_slice(&[16, 32, 64, 96, 128]);
        b[12] = 2;
        b[19] = 3;
        assert_eq!(parse(Family::Pure, &b), Some((3200, Some(1000))));
        b[12] = 1; // disabled slot
        assert_eq!(parse(Family::Pure, &b), None);
        let mut x = [0u8; 43];
        x[..31].copy_from_slice(&b);
        x[12] = 0;
        x[19] = 9; // unknown rate code
        assert_eq!(parse(Family::Xtd, &x), Some((800, None)));
        assert_eq!(parse(Family::Xtd, &b), None, "short report");
        assert_eq!(parse(Family::Xtd, &XTD_FACTORY), Some((800, Some(1000))));
    }

    #[test]
    fn parses_emp_settings() {
        let mut b = [0u8; 41];
        b[0] = 0x06;
        b[1] = 0x29;
        b[6] = 0b11111;
        b[7..12].copy_from_slice(&[0, 7, 15, 31, 0x77]);
        b[17] = 4;
        b[18] = 1;
        assert_eq!(parse(Family::Emp, &b), Some((12000, Some(250))));
        b[17] = 5;
        assert_eq!(parse(Family::Emp, &b), None);
    }

    #[test]
    fn xtd_checksum_matches_captures() {
        assert_eq!(byte_sum(&XTD_FACTORY), [0x04, 0x07]);
        // Eruption's other capture of the same block (lighting differs).
        let other: [u8; 43] = [
            0x06, 0x2b, 0x00, 0x00, 0x06, 0x06, 0x1f, 0x10, 0x20, 0x40, 0x80, 0xa4, 0x00, 0x10,
            0x20, 0x40, 0x80, 0xa4, 0x01, 0x03, 0x0f, 0x01, 0x01, 0x03, 0x02, 0x13, 0x00, 0x64,
            0xfa, 0x13, 0x00, 0x64, 0xfa, 0x13, 0x00, 0x64, 0xfa, 0x13, 0x00, 0x64, 0xfa, 0x62,
            0x09,
        ];
        assert_eq!(byte_sum(&other), [0x62, 0x09]);
        validate_xtd(&XTD_FACTORY, 0).unwrap();
        validate_xtd(&other, 0).unwrap();
    }

    #[test]
    fn xtd_changes_build_exact_bytes() {
        // Stage 2 to 2000 dpi: 2000 / 50 = 0x28 in x (8) and y (14);
        // sum 0x0704 - 0x20 * 2 + 0x28 * 2 = 0x0714.
        let b = apply_xtd(&XTD_FACTORY, Change::Stage(1, 2000)).unwrap();
        let mut want = XTD_FACTORY;
        want[8] = 0x28;
        want[14] = 0x28;
        want[41..].copy_from_slice(&[0x14, 0x07]);
        assert_eq!(b, want);
        // 500 Hz = code 2; sum 0x0704 - 1.
        let b = apply_xtd(&XTD_FACTORY, Change::Rate(500)).unwrap();
        assert_eq!((b[19], &b[41..]), (2, &[0x03, 0x07][..]));
        // Active stage 5 (index 4); sum 0x0704 + 4.
        let b = apply_xtd(&XTD_FACTORY, Change::Active(4)).unwrap();
        assert_eq!((b[12], &b[41..]), (4, &[0x08, 0x07][..]));
        // Out of range or disabled.
        assert!(apply_xtd(&XTD_FACTORY, Change::Stage(0, 8250)).is_err());
        assert!(apply_xtd(&XTD_FACTORY, Change::Stage(0, 850)).is_err());
        assert!(apply_xtd(&XTD_FACTORY, Change::Rate(2000)).is_err());
        let mut two = XTD_FACTORY;
        two[6] = 0b00011;
        let s = byte_sum(&two);
        two[41..].copy_from_slice(&s);
        assert!(apply_xtd(&two, Change::Active(2)).is_err());
    }

    #[test]
    fn validation_rejects_bad_blocks() {
        let fix = |mut b: [u8; 43]| {
            let s = byte_sum(&b);
            b[41..].copy_from_slice(&s);
            b
        };
        let mut b = XTD_FACTORY;
        b[41] ^= 1;
        assert!(validate_xtd(&b, 0).is_err(), "bad checksum");
        assert!(validate_xtd(&XTD_FACTORY, 1).is_err(), "wrong profile");
        // Old firmware returns 0xff for other profiles (libratbag #1466),
        // with a valid sum.
        assert!(validate_xtd(&fix([0xff; 43]), 1).is_err());
        let mut b = XTD_FACTORY;
        b[19] = 4;
        assert!(validate_xtd(&fix(b), 0).is_err(), "rate code");
        let mut b = XTD_FACTORY;
        b[9] = 0;
        assert!(
            validate_xtd(&fix(b), 0).is_err(),
            "zero DPI in an enabled stage"
        );
    }

    /// A Kone XTD that stores profile blocks, optionally corrupting writes.
    struct Fake {
        profile: u8,
        stored: RefCell<Vec<u8>>,
        selected: RefCell<u8>,
        writes: RefCell<Vec<Vec<u8>>>,
        drop_writes: bool,
        status: u8,
    }

    impl Fake {
        fn new(block: &[u8]) -> Self {
            Fake {
                profile: 0,
                stored: RefCell::new(block.to_vec()),
                selected: RefCell::new(0),
                writes: RefCell::new(vec![]),
                drop_writes: false,
                status: 1,
            }
        }
    }

    impl Feature for Fake {
        fn get(&self, buf: &mut [u8]) -> Result<()> {
            match buf[0] {
                0x05 => buf.copy_from_slice(&[0x05, 0x03, self.profile]),
                0x04 => buf.copy_from_slice(&[0x04, self.status, 0x00]),
                0x06 => {
                    assert_eq!(*self.selected.borrow(), self.profile);
                    buf.copy_from_slice(&self.stored.borrow());
                }
                id => bail!("unexpected get {id:#x}"),
            }
            Ok(())
        }
        fn set(&self, buf: &mut [u8]) -> Result<()> {
            match buf {
                [0x04, p, 0x80] => *self.selected.borrow_mut() = *p,
                [0x06, ..] => {
                    self.writes.borrow_mut().push(buf.to_vec());
                    if !self.drop_writes {
                        *self.stored.borrow_mut() = buf.to_vec();
                    }
                }
                _ => bail!("unexpected set {buf:02x?}"),
            }
            Ok(())
        }
    }

    #[test]
    fn write_reads_validates_writes_and_verifies() {
        let dev = Fake::new(&XTD_FACTORY);
        let back = write_xtd(&dev, Change::Rate(500), Duration::ZERO).unwrap();
        assert_eq!(back[19], 2);
        assert_eq!(dev.writes.borrow().len(), 1);
        assert_eq!(dev.writes.borrow()[0], back);
        // No change, no write.
        write_xtd(&dev, Change::Rate(500), Duration::ZERO).unwrap();
        assert_eq!(dev.writes.borrow().len(), 1);
    }

    #[test]
    fn write_aborts_on_a_bad_block() {
        let mut bad = XTD_FACTORY;
        bad[41] ^= 1;
        let dev = Fake::new(&bad);
        let e = write_xtd(&dev, Change::Rate(500), Duration::ZERO).unwrap_err();
        assert!(format!("{e:#}").contains("checksum"), "{e:#}");
        assert!(dev.writes.borrow().is_empty());
    }

    #[test]
    fn write_reports_read_back_mismatch_and_rejection() {
        let mut dev = Fake::new(&XTD_FACTORY);
        dev.drop_writes = true;
        let e = write_xtd(&dev, Change::Stage(0, 400), Duration::ZERO).unwrap_err();
        assert!(e.to_string().contains("read-back"), "{e}");
        let mut dev = Fake::new(&XTD_FACTORY);
        dev.status = 2;
        assert!(write_xtd(&dev, Change::Stage(0, 400), Duration::ZERO).is_err());
        assert!(dev.writes.borrow().is_empty(), "select failed first");
    }

    #[test]
    fn controls_offer_xtd_writes_and_block_the_rest() {
        let make = |spec: &'static Spec<Family>| {
            let mut m = Mouse {
                spec,
                id: DeviceId {
                    vendor_id: ROCCAT,
                    product_id: spec.pids[0],
                    instance: "t".into(),
                },
                dev: None,
                state: SettingsMap::new(),
                last_read: None,
                last_error: None,
            };
            if spec.family == Family::Xtd {
                m.show(&XTD_FACTORY).unwrap();
            } else {
                merge_setting(&mut m.state, SettingKey::Dpi, SettingValue::Int(800));
            }
            m.controls()
        };
        let xtd = make(&KONE_XTD);
        for c in &xtd {
            match c.key {
                SettingKey::Dpi => assert!(c.read_only),
                SettingKey::Effect => assert!(c.persistent && c.blocked.is_some()),
                _ => assert!(c.persistent && c.blocked.is_none(), "{:?}", c.key),
            }
        }
        let stage = xtd.iter().find(|c| c.key == SettingKey::DpiStage1).unwrap();
        assert!(stage.validate(&SettingValue::Int(8200)).is_ok());
        assert!(stage.validate(&SettingValue::Int(8400)).is_err());
        for spec in [&KONE_PURE, &KONE_EMP] {
            assert!(make(spec)
                .iter()
                .filter(|c| c.key != SettingKey::Dpi)
                .all(|c| c.persistent && c.blocked.is_some()));
        }
        assert_eq!(
            change_for(SettingKey::DpiStageActive, &SettingValue::Int(3)).unwrap(),
            Change::Active(2)
        );
    }
}
