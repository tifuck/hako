//! Any HID LampArray device no vendor driver claims: the standard lighting
//! interface behind Windows Dynamic Lighting (HID usage page 0x59, Lighting
//! And Illumination).
//!
//! Sources: USB-IF HUTRR84 "Lighting And Illumination Page" (usages, report
//! semantics, the example descriptor the tests use) and HID Usage Tables 1.4
//! section 26; Microsoft "Dynamic lighting devices" (learn.microsoft.com) and
//! its MIT reference firmware ArduinoHidForWindows (`LampArrayReports.h`,
//! `Microsoft_HidLampArray.cpp`); hid-rgb-ctl (MIT) for the Linux hidraw flow.
//! Reimplemented from the spec; no code copied.
//!
//! Claimed at runtime (no model table): an unlisted VID:PID with a hidraw node
//! whose report descriptor opens a LampArray application collection (usage
//! 0x59:0x01) holding the attributes, lamp attribute request / response and a
//! range or multi update report. Report IDs and every field's bit position
//! come from that descriptor.
//!
//! On open, LampCount and MinUpdateInterval are read from the attributes
//! report and each lamp's colour / intensity level counts from the lamp
//! attributes reports. `static` turns AutonomousMode off and paints every lamp
//! with range updates (one per run of lamps with equal level counts; the last
//! sets LampUpdateComplete), or multi updates when the device has no range
//! report; values are scaled to each lamp's level counts, which the device
//! requires. Brightness drives the intensity channel when the lamp has more
//! than on/off intensity, otherwise it scales the colour. `onboard` turns
//! AutonomousMode back on (offered only when the device has the control
//! report). The spec defines no host timeout (and the reference firmware has
//! none), so a static colour is sent once, not streamed; lamp state is
//! volatile and resets to autonomous / off with the device, which re-enumerates
//! and is re-applied. Shutdown leaves the lighting as last written. LampArray
//! has no persistent writes. A USB / Bluetooth mouse or keyboard keeps what
//! the generic driver gives it (remaps, the kernel battery).

use super::generic::{classify, kernel_battery};
use super::{DeviceAdapter, DriverCtx, Fallback, Opened};
use crate::hid::{descriptor_items, enumerate_hidraw, HidDeviceInfo, HidrawDevice};
use anyhow::{bail, ensure, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::info;

/// Lighting And Illumination page and its usages (HUTRR84 table 1).
const PAGE: u32 = 0x59;
const LAMP_ARRAY: u32 = 0x01;
const ATTRIBUTES_REPORT: u32 = 0x02;
const LAMP_COUNT: u32 = 0x03;
const MIN_UPDATE_INTERVAL: u32 = 0x08;
const REQUEST_REPORT: u32 = 0x20;
const LAMP_ID: u32 = 0x21;
const RESPONSE_REPORT: u32 = 0x22;
/// Red / Green / Blue / Intensity level counts, then IsProgrammable.
const LEVELS: [u32; 4] = [0x28, 0x29, 0x2a, 0x2b];
const IS_PROGRAMMABLE: u32 = 0x2c;
const MULTI_UPDATE_REPORT: u32 = 0x50;
/// Red / Green / Blue / Intensity update channels.
const CHANNELS: [u32; 4] = [0x51, 0x52, 0x53, 0x54];
const UPDATE_FLAGS: u32 = 0x55;
const RANGE_UPDATE_REPORT: u32 = 0x60;
const LAMP_ID_START: u32 = 0x61;
const LAMP_ID_END: u32 = 0x62;
const CONTROL_REPORT: u32 = 0x70;
const AUTONOMOUS_MODE: u32 = 0x71;
/// LampUpdateFlags: LampUpdateComplete (HUTRR84 3.6.3).
const UPDATE_COMPLETE: u32 = 0x01;

/// ponytail: open reads every lamp (two reports each) while discovery waits;
/// raise if a real device has more lamps.
const MAX_LAMPS: u32 = 1024;
const MAX_INTERVAL: Duration = Duration::from_secs(1);
const BATTERY_POLL: Duration = Duration::from_secs(30);
const COLOR_KEYS: [SettingKey; 3] = [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB];

pub fn fallback(nodes: &[&HidDeviceInfo]) -> Option<Fallback> {
    nodes
        .iter()
        .any(|n| protocol(&n.report_descriptor()).is_some())
        .then(|| {
            let (kind, name) = identity(nodes);
            Fallback { kind, name, open }
        })
}

/// A USB / Bluetooth mouse or keyboard as the generic driver names it (so it
/// keeps its remaps), anything else as a lighting device.
fn identity(nodes: &[&HidDeviceInfo]) -> (DeviceKind, String) {
    classify(nodes).unwrap_or_else(|| {
        let named = nodes.iter().map(|n| n.name.trim()).find(|n| !n.is_empty());
        let name = match (named, nodes.first()) {
            (Some(n), _) => n.to_string(),
            (None, Some(f)) => format!("LampArray {:04x}:{:04x}", f.vendor_id, f.product_id),
            (None, None) => "LampArray".into(),
        };
        (DeviceKind::Other, name)
    })
}

/// One report field: a page-0x59 usage at a bit offset after the report ID.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Field {
    usage: u32,
    bit: u32,
    size: u32,
}

impl Field {
    fn max(self) -> u32 {
        if self.size >= 32 {
            u32::MAX
        } else {
            (1 << self.size) - 1
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Report {
    id: u8,
    /// Output report (written to the node) rather than a feature report.
    output: bool,
    /// Data bytes after the report ID.
    len: usize,
    fields: Vec<Field>,
}

impl Report {
    fn buf(&self) -> Vec<u8> {
        let mut b = vec![0; self.len + 1];
        b[0] = self.id;
        b
    }

    fn has(&self, usages: &[u32]) -> bool {
        usages
            .iter()
            .all(|&u| self.fields.iter().any(|f| f.usage == u))
    }

    fn count(&self, usage: u32) -> usize {
        self.fields.iter().filter(|f| f.usage == usage).count()
    }

    /// Bytes (report ID included) a reply needs to carry every `usages` field.
    fn bytes_for(&self, usages: &[u32]) -> usize {
        1 + self
            .fields
            .iter()
            .filter(|f| usages.contains(&f.usage))
            .map(|f| f.bit.saturating_add(f.size).div_ceil(8) as usize)
            .max()
            .unwrap_or(0)
    }

    fn get(&self, buf: &[u8], usage: u32) -> Option<u32> {
        let f = self.fields.iter().find(|f| f.usage == usage)?;
        Some((0..f.size.min(32)).fold(0, |v, i| {
            let b = (f.bit + i) as usize;
            let byte = buf.get(1 + b / 8).copied().unwrap_or(0);
            v | (u32::from(byte >> (b % 8) & 1) << i)
        }))
    }

    /// Write the `n`th field with `usage`, clamped to its width.
    fn put(&self, buf: &mut [u8], usage: u32, n: usize, value: u32) {
        let Some(f) = self.fields.iter().filter(|f| f.usage == usage).nth(n) else {
            return;
        };
        let value = value.min(f.max());
        for i in 0..f.size.min(32) {
            let b = (f.bit + i) as usize;
            if let Some(byte) = buf.get_mut(1 + b / 8) {
                let mask = 1 << (b % 8);
                if value >> i & 1 == 1 {
                    *byte |= mask;
                } else {
                    *byte &= !mask;
                }
            }
        }
    }
}

/// The LampArray reports a descriptor declares (first LampArray only).
#[derive(Debug, Default)]
struct Layout {
    attributes: Option<Report>,
    request: Option<Report>,
    response: Option<Report>,
    multi: Option<Report>,
    range: Option<Report>,
    control: Option<Report>,
}

impl Layout {
    fn slot(&mut self, report_usage: u32) -> Option<&mut Option<Report>> {
        Some(match report_usage {
            ATTRIBUTES_REPORT => &mut self.attributes,
            REQUEST_REPORT => &mut self.request,
            RESPONSE_REPORT => &mut self.response,
            MULTI_UPDATE_REPORT => &mut self.multi,
            RANGE_UPDATE_REPORT => &mut self.range,
            CONTROL_REPORT => &mut self.control,
            _ => return None,
        })
    }
}

/// Walk a report descriptor: the first LampArray application collection and
/// the fields of its report collections.
fn layout(desc: &[u8]) -> Option<Layout> {
    let mut out: Option<Layout> = None;
    let (mut page, mut size, mut count, mut id) = (0u32, 0u32, 0u32, 0u8);
    let mut usages: Vec<u32> = Vec::new();
    let mut depth = 0u32;
    let mut app: Option<u32> = None;
    // (depth, report usage) of the report collection being read.
    let mut sub: Option<(u32, u32)> = None;
    // Bits so far per (report ID, main item tag).
    let mut bits: Vec<((u8, u8), u32)> = Vec::new();
    for (tag, value) in descriptor_items(desc) {
        match tag {
            0x04 => page = value,
            0x74 => size = value,
            0x84 => id = value as u8,
            0x94 => count = value,
            // Usage (local); a 4-byte usage already carries its page.
            0x08 => usages.push(if value > 0xffff {
                value
            } else {
                (page << 16) | value
            }),
            0xa0 => {
                depth += 1;
                let usage = usages.first().copied();
                if out.is_none() && value == 1 && usage == Some((PAGE << 16) | LAMP_ARRAY) {
                    app = Some(depth);
                    out = Some(Layout::default());
                } else if app.is_some() && sub.is_none() {
                    sub = usage
                        .filter(|u| u >> 16 == PAGE)
                        .map(|u| (depth, u & 0xffff));
                }
                usages.clear();
            }
            0xc0 => {
                if sub.is_some_and(|(d, _)| d == depth) {
                    sub = None;
                }
                if app == Some(depth) {
                    app = None;
                }
                depth = depth.saturating_sub(1);
                usages.clear();
            }
            // Input / Output / Feature.
            0x80 | 0x90 | 0xb0 => {
                let start = match bits.iter_mut().find(|(k, _)| *k == (id, tag)) {
                    Some((_, b)) => std::mem::replace(b, *b + size * count),
                    None => {
                        bits.push(((id, tag), size * count));
                        0
                    }
                };
                let slot = match (sub, out.as_mut()) {
                    (Some((_, report)), Some(l)) if tag != 0x80 => l.slot(report),
                    _ => None,
                };
                if let Some(slot) = slot {
                    let r = slot.get_or_insert_with(|| Report {
                        id,
                        output: tag == 0x90,
                        ..Report::default()
                    });
                    if r.id == id && r.output == (tag == 0x90) {
                        for i in 0..count {
                            // Usages pair with fields in order; the last repeats.
                            let Some(&u) = usages.get(i as usize).or(usages.last()) else {
                                break;
                            };
                            if u >> 16 == PAGE {
                                r.fields.push(Field {
                                    usage: u & 0xffff,
                                    bit: start + i * size,
                                    size,
                                });
                            }
                        }
                    }
                }
                usages.clear();
            }
            _ => {}
        }
    }
    let mut l = out?;
    for r in [
        &mut l.attributes,
        &mut l.request,
        &mut l.response,
        &mut l.multi,
        &mut l.range,
        &mut l.control,
    ]
    .into_iter()
    .flatten()
    {
        let tag = if r.output { 0x90 } else { 0xb0 };
        let total = bits
            .iter()
            .find(|(k, _)| *k == (r.id, tag))
            .map_or(0, |b| b.1);
        r.len = total.div_ceil(8) as usize;
    }
    Some(l)
}

/// What the driver needs from a LampArray, with every field it reads or writes.
#[derive(Debug)]
struct Protocol {
    attributes: Report,
    request: Report,
    response: Report,
    /// The range update report, or the multi update report without one.
    update: Report,
    control: Option<Report>,
}

fn protocol(desc: &[u8]) -> Option<Protocol> {
    let l = layout(desc)?;
    let feature = |r: Option<Report>, usages: &[u32]| r.filter(|r| !r.output && r.has(usages));
    // Intensity is optional: Razer's firmware declares neither its level
    // count nor its update channel.
    // Without LampUpdateFlags no update would ever complete.
    let range = [
        &[UPDATE_FLAGS, LAMP_ID_START, LAMP_ID_END][..],
        &CHANNELS[..3],
    ]
    .concat();
    let multi = [&[UPDATE_FLAGS, LAMP_COUNT, LAMP_ID][..], &CHANNELS[..3]].concat();
    let response = [&[LAMP_ID, IS_PROGRAMMABLE][..], &LEVELS[..3]].concat();
    Some(Protocol {
        attributes: feature(l.attributes, &[LAMP_COUNT])?,
        request: feature(l.request, &[LAMP_ID])?,
        response: feature(l.response, &response)?,
        update: l
            .range
            .filter(|r| r.has(&range))
            .or(l.multi.filter(|r| r.has(&multi)))?,
        control: feature(l.control, &[AUTONOMOUS_MODE]),
    })
}

/// A lamp's level counts (Red, Green, Blue, Intensity), HUTRR84 3.2.4.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Lamp {
    programmable: bool,
    levels: [u32; 4],
}

fn parse_lamp(response: &Report, buf: &[u8]) -> Lamp {
    let v = |u| response.get(buf, u).unwrap_or(0);
    Lamp {
        programmable: v(IS_PROGRAMMABLE) != 0,
        levels: LEVELS.map(v),
    }
}

/// Channel values (R, G, B, I) that show `color` at `pct` brightness on
/// `lamp`. 0..=255 maps onto 0..=levels; a channel may not exceed its count.
fn lamp_rgbi(lamp: &Lamp, color: [u8; 3], pct: i64) -> [u32; 4] {
    let pct = pct.clamp(0, 100) as u32;
    // u64: level counts come from the device and may be up to u32::MAX.
    // Every result is at most the level count, so it fits a u32 again.
    let scale = |v: u32, levels: u32| ((u64::from(v) * u64::from(levels) + 127) / 255) as u32;
    let [r, g, b, i] = lamp.levels;
    if !lamp.programmable {
        // Fixed colour: RGB are ignored; intensity follows the brightest
        // channel, rounded up so any visible colour lights the lamp.
        let level = u64::from(color.into_iter().max().unwrap_or(0)) * u64::from(pct) / 100;
        return [0, 0, 0, (level * u64::from(i)).div_ceil(255) as u32];
    }
    let c = color.map(u32::from);
    if i > 1 {
        let intensity = ((u64::from(pct) * u64::from(i) + 50) / 100) as u32;
        [scale(c[0], r), scale(c[1], g), scale(c[2], b), intensity]
    } else {
        let c = c.map(|v| v * pct / 100);
        [scale(c[0], r), scale(c[1], g), scale(c[2], b), i]
    }
}

fn range_report(r: &Report, start: u32, end: u32, rgbi: [u32; 4], complete: bool) -> Vec<u8> {
    let mut b = r.buf();
    r.put(
        &mut b,
        UPDATE_FLAGS,
        0,
        if complete { UPDATE_COMPLETE } else { 0 },
    );
    r.put(&mut b, LAMP_ID_START, 0, start);
    r.put(&mut b, LAMP_ID_END, 0, end);
    for (u, v) in CHANNELS.into_iter().zip(rgbi) {
        r.put(&mut b, u, 0, v);
    }
    b
}

fn multi_report(r: &Report, lamps: &[(u32, [u32; 4])], complete: bool) -> Vec<u8> {
    let mut b = r.buf();
    r.put(&mut b, LAMP_COUNT, 0, lamps.len() as u32);
    r.put(
        &mut b,
        UPDATE_FLAGS,
        0,
        if complete { UPDATE_COMPLETE } else { 0 },
    );
    for (n, &(id, rgbi)) in lamps.iter().enumerate() {
        r.put(&mut b, LAMP_ID, n, id);
        for (u, v) in CHANNELS.into_iter().zip(rgbi) {
            r.put(&mut b, u, n, v);
        }
    }
    b
}

/// The update reports that paint every lamp; only the last one completes
/// the batch.
fn updates(update: &Report, lamps: &[Lamp], color: [u8; 3], pct: i64) -> Vec<Vec<u8>> {
    let values: Vec<[u32; 4]> = lamps.iter().map(|l| lamp_rgbi(l, color, pct)).collect();
    if update.has(&[LAMP_ID_START]) {
        // One range per run of lamps that take the same values.
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (i, v) in values.iter().enumerate() {
            match runs.last_mut() {
                Some(run) if values[run.1] == *v => run.1 = i,
                _ => runs.push((i, i)),
            }
        }
        let last = runs.len().saturating_sub(1);
        runs.iter()
            .enumerate()
            .map(|(n, &(s, e))| range_report(update, s as u32, e as u32, values[s], n == last))
            .collect()
    } else {
        let slots = update.count(LAMP_ID).max(1);
        let ids: Vec<(u32, [u32; 4])> = (0u32..).zip(values).collect();
        let last = ids.len().div_ceil(slots).saturating_sub(1);
        ids.chunks(slots)
            .enumerate()
            .map(|(n, chunk)| multi_report(update, chunk, n == last))
            .collect()
    }
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let vid = ctx.vendor_id;
    let nodes: Vec<HidDeviceInfo> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .collect();
    let refs: Vec<&HidDeviceInfo> = nodes.iter().collect();
    let (node, proto) = nodes
        .iter()
        .find_map(|n| protocol(&n.report_descriptor()).map(|p| (n, p)))
        .with_context(|| format!("{vid:04x}:{pid:04x} has no usable LampArray interface"))?;
    let dev = HidrawDevice::open(&node.path)?;
    let mut attrs = proto.attributes.buf();
    let got = dev.get_feature_len(&mut attrs)?;
    let need = proto
        .attributes
        .bytes_for(&[LAMP_COUNT, MIN_UPDATE_INTERVAL]);
    ensure!(
        got >= need,
        "short LampArray attributes reply ({got} of {need} bytes)"
    );
    let count = proto.attributes.get(&attrs, LAMP_COUNT).unwrap_or(0);
    ensure!(
        (1..=MAX_LAMPS).contains(&count),
        "LampArray reports {count} lamps"
    );
    let interval = proto
        .attributes
        .get(&attrs, MIN_UPDATE_INTERVAL)
        .map_or(Duration::ZERO, |us| {
            Duration::from_micros(us.into()).min(MAX_INTERVAL)
        });
    let need = proto
        .response
        .bytes_for(&[&[LAMP_ID, IS_PROGRAMMABLE][..], &LEVELS[..]].concat());
    let lamps = (0..count)
        .map(|id| {
            let mut req = proto.request.buf();
            proto.request.put(&mut req, LAMP_ID, 0, id);
            dev.set_feature(&mut req)?;
            let mut resp = proto.response.buf();
            let n = dev.get_feature_len(&mut resp)?;
            ensure!(n >= need, "short reply for lamp {id} ({n} of {need} bytes)");
            let got = proto.response.get(&resp, LAMP_ID);
            ensure!(got == Some(id), "asked for lamp {id}, got {got:?}");
            Ok(parse_lamp(&proto.response, &resp))
        })
        .collect::<Result<Vec<_>>>()?;
    let (kind, name) = identity(&refs);
    info!(
        "{name}: LampArray via {}, {count} lamps",
        dev.path.display()
    );
    let mut a = LampArrayAdapter {
        id: DeviceId {
            vendor_id: vid,
            product_id: pid,
            instance: if node.uniq.is_empty() {
                node.path.display().to_string()
            } else {
                node.uniq.clone()
            },
        },
        kind,
        name,
        settings: hako_core::config::load_device_settings(&ctx.paths, vid, pid)?,
        dev: Some(dev),
        proto,
        lamps,
        interval,
        last_update: None,
        paths: ctx.paths.clone(),
        last_error: None,
        hid_dirs: nodes.iter().map(|n| n.sys.clone()).collect(),
        battery: None,
        last_poll: Instant::now(),
    };
    a.battery = kernel_battery(&a.hid_dirs);
    // Device state is volatile; re-apply a host colour, leave onboard alone.
    if a.effect() == "static" {
        a.last_error = a.apply().err().map(|e| e.to_string());
    }
    Ok(vec![Box::new(a)])
}

pub struct LampArrayAdapter {
    id: DeviceId,
    kind: DeviceKind,
    name: String,
    dev: Option<HidrawDevice>,
    proto: Protocol,
    lamps: Vec<Lamp>,
    /// MinUpdateIntervalInMicroseconds between two updates of one lamp.
    interval: Duration,
    last_update: Option<Instant>,
    settings: SettingsMap,
    paths: ConfigPaths,
    last_error: Option<String>,
    hid_dirs: Vec<PathBuf>,
    battery: Option<BatteryState>,
    last_poll: Instant,
}

impl LampArrayAdapter {
    fn effect(&self) -> &str {
        let default = if self.proto.control.is_some() {
            "onboard"
        } else {
            "static"
        };
        get_setting(&self.settings, SettingKey::Effect)
            .and_then(SettingValue::as_str)
            .unwrap_or(default)
    }

    fn controls(&self) -> Vec<SettingMeta> {
        let mut effects = vec![SettingOption::text("static", "Static")];
        if self.proto.control.is_some() {
            effects.insert(
                0,
                SettingOption::text("onboard", "Onboard (device effects)"),
            );
        }
        let onboard = self.effect() == "onboard";
        let mut c = vec![
            SettingMeta::dropdown(SettingKey::Effect, "Effect", effects),
            SettingMeta::slider(SettingKey::Brightness, "Brightness", 0.0, 100.0, 1.0)
                .unit("%")
                .hidden(onboard),
        ];
        for (key, label) in COLOR_KEYS.into_iter().zip(["Red", "Green", "Blue"]) {
            c.push(SettingMeta::color(key, label).hidden(onboard));
        }
        c
    }

    fn set_autonomous(&self, on: bool) -> Result<()> {
        let (Some(c), Some(dev)) = (&self.proto.control, &self.dev) else {
            return Ok(());
        };
        let mut b = c.buf();
        c.put(&mut b, AUTONOMOUS_MODE, 0, on.into());
        dev.set_feature(&mut b)
    }

    fn apply(&mut self) -> Result<()> {
        if self.dev.is_none() {
            bail!("not connected");
        }
        if self.effect() == "onboard" {
            return self.set_autonomous(true);
        }
        // Disabling twice is a no-op; the host waits MinUpdateInterval after
        // it and between two updates of a lamp (HUTRR84 3.1, 3.3.1).
        self.set_autonomous(false)?;
        if let Some(t) = self.last_update {
            std::thread::sleep(self.interval.saturating_sub(t.elapsed()));
        } else {
            std::thread::sleep(self.interval);
        }
        let s = &self.settings;
        let int = |k, d: i64| {
            get_setting(s, k)
                .and_then(SettingValue::as_int)
                .unwrap_or(d)
        };
        let color = COLOR_KEYS.map(|k| int(k, 255).clamp(0, 255) as u8);
        let reports = updates(
            &self.proto.update,
            &self.lamps,
            color,
            int(SettingKey::Brightness, 100),
        );
        let output = self.proto.update.output;
        let dev = self.dev.as_mut().context("not connected")?;
        for mut r in reports {
            if output {
                dev.write(&r)?;
            } else {
                dev.set_feature(&mut r)?;
            }
        }
        self.last_update = Some(Instant::now());
        Ok(())
    }
}

impl DeviceAdapter for LampArrayAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.clone(),
            connected: self.dev.is_some(),
            batteries: self.battery.iter().cloned().collect(),
            settings: self.settings.clone(),
            controls: self.controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        merge_setting(&mut self.settings, key, value);
        let result = self.apply();
        self.last_error = result.as_ref().err().map(|e| e.to_string());
        result?;
        hako_core::config::save_device_settings(
            &self.paths,
            self.id.vendor_id,
            self.id.product_id,
            &self.settings,
        )
        .map_err(|e| anyhow::anyhow!("applied, but not saved: {e:#}"))
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        if self.last_poll.elapsed() >= BATTERY_POLL {
            self.last_poll = Instant::now();
            self.battery = kernel_battery(&self.hid_dirs);
        }
        Ok(())
    }

    /// Lighting stays as last written (no return to autonomous mode).
    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example descriptor of HUTRR84 section 4, byte for byte.
    #[rustfmt::skip]
    const SPEC: &[u8] = &[
        0x05, 0x59, 0x09, 0x01, 0xa1, 0x01,
        // Report 1: LampArrayAttributesReport.
        0x85, 0x01, 0x09, 0x02, 0xa1, 0x02,
        0x09, 0x03, 0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0xb1, 0x03,
        0x09, 0x04, 0x09, 0x05, 0x09, 0x06, 0x09, 0x07, 0x09, 0x08,
        0x15, 0x00, 0x27, 0xff, 0xff, 0xff, 0x7f, 0x75, 0x20, 0x95, 0x05, 0xb1, 0x03,
        0xc0,
        // Report 2: LampAttributesRequestReport.
        0x85, 0x02, 0x09, 0x20, 0xa1, 0x02,
        0x09, 0x21, 0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0xb1, 0x02,
        0xc0,
        // Report 3: LampAttributesResponseReport.
        0x85, 0x03, 0x09, 0x22, 0xa1, 0x02,
        0x09, 0x21, 0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0xb1, 0x02,
        0x09, 0x23, 0x09, 0x24, 0x09, 0x25, 0x09, 0x27, 0x09, 0x26,
        0x15, 0x00, 0x27, 0xff, 0xff, 0xff, 0x7f, 0x75, 0x20, 0x95, 0x05, 0xb1, 0x02,
        0x09, 0x28, 0x09, 0x29, 0x09, 0x2a, 0x09, 0x2b, 0x09, 0x2c, 0x09, 0x2d,
        0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x06, 0xb1, 0x02,
        0xc0,
        // Report 4: LampMultiUpdateReport, 8 slots.
        0x85, 0x04, 0x09, 0x50, 0xa1, 0x02,
        0x09, 0x03, 0x09, 0x55, 0x15, 0x00, 0x25, 0x08, 0x75, 0x08, 0x95, 0x02, 0xb1, 0x02,
        0x09, 0x21, 0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x08, 0xb1, 0x02,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x20, 0xb1, 0x02,
        0xc0,
        // Report 5: LampRangeUpdateReport.
        0x85, 0x05, 0x09, 0x60, 0xa1, 0x02,
        0x09, 0x55, 0x15, 0x00, 0x25, 0x08, 0x75, 0x08, 0x95, 0x01, 0xb1, 0x02,
        0x09, 0x61, 0x09, 0x62, 0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x02, 0xb1, 0x02,
        0x09, 0x51, 0x09, 0x52, 0x09, 0x53, 0x09, 0x54,
        0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x04, 0xb1, 0x02,
        0xc0,
        // Report 6: LampArrayControlReport.
        0x85, 0x06, 0x09, 0x70, 0xa1, 0x02,
        0x09, 0x71, 0x15, 0x00, 0x25, 0x01, 0x75, 0x08, 0x95, 0x01, 0xb1, 0x02,
        0xc0,
        0xc0,
    ];

    fn spec() -> Protocol {
        protocol(SPEC).expect("the spec example is a usable LampArray")
    }

    #[test]
    fn spec_descriptor_layout_matches_the_reference_structs() {
        // Sizes match ArduinoHidForWindows LampArrayReports.h (minus the ID byte).
        let p = spec();
        assert_eq!((p.attributes.id, p.attributes.len), (1, 22));
        assert_eq!((p.request.id, p.request.len), (2, 2));
        assert_eq!((p.response.id, p.response.len), (3, 28));
        assert_eq!((p.update.id, p.update.len, p.update.output), (5, 9, false));
        let control = p.control.as_ref().unwrap();
        assert_eq!((control.id, control.len), (6, 1));
        let l = layout(SPEC).unwrap();
        let multi = l.multi.unwrap();
        assert_eq!((multi.id, multi.len, multi.count(LAMP_ID)), (4, 50, 8));
        assert_eq!(multi.count(0x54), 8);
    }

    #[test]
    fn attributes_and_lamp_reports_parse_by_field() {
        let p = spec();
        // LampCount 0x0068, width .. kind 1 (keyboard), MinUpdateInterval 16 ms.
        let mut attrs = vec![0x01, 0x68, 0x00];
        for v in [450_000u32, 150_000, 30_000, 1, 16_000] {
            attrs.extend(v.to_le_bytes());
        }
        assert_eq!(p.attributes.get(&attrs, LAMP_COUNT), Some(0x68));
        assert_eq!(p.attributes.get(&attrs, 0x07), Some(1));
        assert_eq!(p.attributes.get(&attrs, MIN_UPDATE_INTERVAL), Some(16_000));
        // Lamp 7: positions, latency, purposes, then R G B I counts, programmable.
        let mut resp = vec![0x03, 0x07, 0x00];
        resp.extend([0u8; 20]);
        resp.extend([255, 255, 255, 1, 1, 0x04]);
        assert_eq!(p.response.get(&resp, LAMP_ID), Some(7));
        assert_eq!(
            parse_lamp(&p.response, &resp),
            Lamp {
                programmable: true,
                levels: [255, 255, 255, 1]
            }
        );
        let mut req = p.request.buf();
        p.request.put(&mut req, LAMP_ID, 0, 0x0123);
        assert_eq!(req, [0x02, 0x23, 0x01]);
    }

    #[test]
    fn range_and_multi_updates_match_the_spec_examples() {
        let p = spec();
        // HUTRR84 3.4.2: lamps 0x19..=0x31 to FF 00 00 FF.
        assert_eq!(
            range_report(&p.update, 0x19, 0x31, [0xff, 0, 0, 0xff], true),
            [0x05, 0x01, 0x19, 0x00, 0x31, 0x00, 0xff, 0x00, 0x00, 0xff]
        );
        // HUTRR84 3.4.1: 5 of 8 slots used, the rest zero.
        let multi = layout(SPEC).unwrap().multi.unwrap();
        let lamps = [
            (0x19, [0xff, 0x00, 0xff, 0x80]),
            (0x23, [0x80, 0x80, 0xff, 0xff]),
            (0x72, [0x00, 0x00, 0x80, 0xff]),
            (0x56, [0xff, 0x80, 0x00, 0x80]),
            (0x64, [0xff, 0xff, 0x00, 0xff]),
        ];
        let b = multi_report(&multi, &lamps, false);
        assert_eq!(b.len(), 51);
        assert_eq!(b[..3], [0x04, 0x05, 0x00]);
        assert_eq!(b[3..13], [0x19, 0, 0x23, 0, 0x72, 0, 0x56, 0, 0x64, 0]);
        assert_eq!(b[13..19], [0; 6]);
        assert_eq!(b[19..23], [0xff, 0x00, 0xff, 0x80]);
        assert_eq!(b[35..39], [0xff, 0xff, 0x00, 0xff]);
        assert!(b[39..].iter().all(|&v| v == 0));
    }

    #[test]
    fn colours_respect_each_lamps_level_counts() {
        let rgb = Lamp {
            programmable: true,
            levels: [255, 255, 255, 1],
        };
        // On/off intensity: brightness scales the colour.
        assert_eq!(lamp_rgbi(&rgb, [255, 128, 0], 100), [255, 128, 0, 1]);
        assert_eq!(lamp_rgbi(&rgb, [200, 100, 0], 50), [100, 50, 0, 1]);
        // Real intensity channel: colour kept, intensity carries brightness.
        let dimmable = Lamp {
            programmable: true,
            levels: [16, 16, 0, 32],
        };
        assert_eq!(lamp_rgbi(&dimmable, [255, 128, 255], 50), [16, 8, 0, 16]);
        // Fixed colour: RGB zero, intensity from the brightest channel.
        let fixed = Lamp {
            programmable: false,
            levels: [1, 0, 0, 1],
        };
        assert_eq!(lamp_rgbi(&fixed, [255, 0, 0], 100), [0, 0, 0, 1]);
        assert_eq!(lamp_rgbi(&fixed, [0, 0, 0], 100), [0, 0, 0, 0]);
    }

    #[test]
    fn updates_cover_every_lamp_and_complete_once() {
        let p = spec();
        let rgb = Lamp {
            programmable: true,
            levels: [255, 255, 255, 1],
        };
        let fixed = Lamp {
            programmable: false,
            levels: [0, 0, 0, 1],
        };
        let lamps = [rgb, rgb, fixed, rgb];
        let r = updates(&p.update, &lamps, [10, 20, 30], 100);
        assert_eq!(
            r,
            [
                vec![0x05, 0x00, 0, 0, 1, 0, 10, 20, 30, 1],
                vec![0x05, 0x00, 2, 0, 2, 0, 0, 0, 0, 1],
                vec![0x05, 0x01, 3, 0, 3, 0, 10, 20, 30, 1],
            ]
        );
        // Without a range report: 8-slot multi updates.
        let multi = layout(SPEC).unwrap().multi.unwrap();
        let r = updates(&multi, &[rgb; 10], [1, 2, 3], 100);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0][..3], [0x04, 8, 0x00]);
        assert_eq!(r[1][..3], [0x04, 2, 0x01]);
        assert_eq!(r[1][3..7], [8, 0, 9, 0]);
    }

    #[test]
    fn probe_needs_a_complete_lamp_array() {
        assert!(protocol(SPEC).is_some());
        // Same reports under a non-LampArray application collection.
        let mut other = SPEC.to_vec();
        other[3] = 0x02;
        assert!(protocol(&other).is_none());
        // Logical (not application) top collection.
        let mut logical = SPEC.to_vec();
        logical[5] = 0x02;
        assert!(protocol(&logical).is_none());
        // Without the control report there is no onboard mode.
        let ctrl = SPEC.len() - 20;
        let no_control = [&SPEC[..ctrl], &SPEC[SPEC.len() - 1..]].concat();
        let p = protocol(&no_control).unwrap();
        assert!(p.control.is_none());
        // A boot keyboard.
        assert!(protocol(&[0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0xc0]).is_none());
        assert!(protocol(&[]).is_none());
    }

    /// Razer Huntsman V2 TKL (1532:026b) interface 4, read from sysfs: no
    /// intensity usages, 8-slot multi update with RGB only, a vendor report.
    const RAZER: &str = "05590901a10185010902a1020903150027ffff000075109501b10309040905090609070908150027ffffff7f75209505b103c085020920a1020921150027ffff000075109501b102c085030922a1020921150027ffff000075109501b10209230924092509270926150027ffffff7f75209505b10209280929092a092c092d150026ff0075089505b102c085040950a102090309551500250875089502b1020921150027ffff000075109508b102095109520953095109520953095109520953095109520953095109520953095109520953095109520953095109520953150026ff0075089518b102c085050960a10209551500250875089501b10209610962150027ffff000075109502b102095109520953150026ff0075089503b102c085060970a10209711500250175089501b102c085070600ff150026ff0009027508953fb102c0050c0901a101850819d029d715002501750195088102c0";

    #[test]
    fn razer_descriptor_without_intensity_is_usable() {
        let desc: Vec<u8> = (0..RAZER.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&RAZER[i..i + 2], 16).unwrap())
            .collect();
        let p = protocol(&desc).expect("Razer LampArray is usable");
        assert_eq!((p.response.id, p.response.len), (3, 27));
        assert_eq!((p.update.id, p.update.len), (5, 8));
        assert!(p.control.is_some());
        // Response: LampId, 5 x u32, then R G B levels, programmable, binding.
        let mut resp = vec![0x03, 0x02, 0x00];
        resp.extend([0u8; 20]);
        resp.extend([255, 255, 255, 1, 0x29]);
        let lamp = parse_lamp(&p.response, &resp);
        assert_eq!(lamp.levels, [255, 255, 255, 0]);
        // Brightness scales RGB; there is no intensity byte to write.
        assert_eq!(
            updates(&p.update, &[lamp; 3], [200, 100, 0], 50),
            [vec![0x05, 0x01, 0, 0, 2, 0, 100, 50, 0]]
        );
    }

    #[test]
    fn replies_must_carry_the_fields_read() {
        let p = spec();
        assert_eq!(
            p.attributes.bytes_for(&[LAMP_COUNT, MIN_UPDATE_INTERVAL]),
            1 + 22
        );
        assert_eq!(p.attributes.bytes_for(&[LAMP_COUNT]), 1 + 2);
        let lamp = [&[LAMP_ID, IS_PROGRAMMABLE][..], &LEVELS[..]].concat();
        assert_eq!(p.response.bytes_for(&lamp), 1 + 27);
    }

    #[test]
    fn huge_level_counts_do_not_overflow() {
        let lamp = |programmable| Lamp {
            programmable,
            levels: [u32::MAX; 4],
        };
        let max = u32::MAX;
        assert_eq!(lamp_rgbi(&lamp(true), [255, 0, 0], 100), [max, 0, 0, max]);
        assert_eq!(lamp_rgbi(&lamp(false), [255, 0, 0], 100), [0, 0, 0, max]);
        let flat = Lamp {
            programmable: true,
            levels: [max, max, max, 1],
        };
        assert_eq!(lamp_rgbi(&flat, [255, 255, 255], 100), [max, max, max, 1]);
    }

    #[test]
    fn update_reports_need_update_flags() {
        // Rename LampUpdateFlags (0x55) in the range report: multi updates.
        let mut desc = SPEC.to_vec();
        let range = desc
            .windows(4)
            .position(|w| w == [0x09, 0x60, 0xa1, 0x02])
            .unwrap();
        assert_eq!(desc[range + 4..range + 6], [0x09, 0x55]);
        desc[range + 5] = 0x56;
        assert_eq!(protocol(&desc).unwrap().update.id, 4);
        // Neither update report has them: unusable.
        let multi = desc
            .windows(4)
            .position(|w| w == [0x09, 0x50, 0xa1, 0x02])
            .unwrap();
        assert_eq!(desc[multi + 6..multi + 8], [0x09, 0x55]);
        desc[multi + 7] = 0x56;
        assert!(protocol(&desc).is_none());
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hako-lamparray-{name}-{}", std::process::id()))
    }

    #[test]
    fn a_failed_write_is_not_saved() {
        let root = temp_root("save");
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root: root.clone(),
        };
        paths.ensure().unwrap();
        let mut a = LampArrayAdapter {
            id: DeviceId {
                vendor_id: 0x1234,
                product_id: 0x5678,
                instance: "x".into(),
            },
            kind: DeviceKind::Other,
            name: "Lamp".into(),
            dev: None,
            proto: spec(),
            lamps: Vec::new(),
            interval: Duration::ZERO,
            last_update: None,
            settings: SettingsMap::new(),
            paths: paths.clone(),
            last_error: None,
            hid_dirs: Vec::new(),
            battery: None,
            last_poll: Instant::now(),
        };
        let err = a.set(SettingKey::Effect, SettingValue::Text("static".into()));
        assert_eq!(err.unwrap_err().to_string(), "not connected");
        assert_eq!(a.last_error.as_deref(), Some("not connected"));
        assert_eq!(std::fs::read_dir(&paths.devices_dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_generic_probe_result_is_probed_again() {
        use crate::drivers::fallback_model;
        use crate::hid::BUS_USB;
        let root = temp_root("probe");
        let caps = root.join("hid/input/input0/capabilities");
        std::fs::create_dir_all(&caps).unwrap();
        std::fs::write(caps.join("key"), "ffff0000 0 0 0 0").unwrap();
        std::fs::write(caps.join("rel"), "143").unwrap();
        std::fs::write(caps.join("abs"), "0").unwrap();
        let node = HidDeviceInfo {
            path: root.join("hidraw0"),
            vendor_id: 0x1d50,
            product_id: 0x6a6b,
            interface: Some(0),
            uniq: String::new(),
            bus: BUS_USB,
            name: "Test Mouse".into(),
            sys: root.join("hid"),
        };
        // No report descriptor yet: only the generic probe accepts it.
        let generic = fallback_model(&[&node]).unwrap();
        assert!(std::ptr::eq(generic, fallback_model(&[&node]).unwrap()));
        // Its LampArray interface shows up: the specific model, from then on.
        std::fs::write(root.join("hid/report_descriptor"), SPEC).unwrap();
        let lamp = fallback_model(&[&node]).unwrap();
        assert!(!std::ptr::eq(generic, lamp));
        assert!(!generic.same(lamp) && lamp.same(lamp));
        std::fs::remove_file(root.join("hid/report_descriptor")).unwrap();
        assert!(std::ptr::eq(lamp, fallback_model(&[&node]).unwrap()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn output_update_reports_are_written_not_set() {
        // Range update declared as Output (91 02) instead of Feature.
        let mut desc = SPEC.to_vec();
        let range = desc
            .windows(4)
            .position(|w| w == [0x09, 0x60, 0xa1, 0x02])
            .unwrap();
        let ctrl = desc
            .windows(4)
            .position(|w| w == [0x09, 0x70, 0xa1, 0x02])
            .unwrap();
        for i in range..ctrl {
            if desc[i] == 0xb1 && desc[i + 1] == 0x02 {
                desc[i] = 0x91;
            }
        }
        let p = protocol(&desc).unwrap();
        assert!(p.update.output);
        assert_eq!((p.update.id, p.update.len), (5, 9));
    }
}
