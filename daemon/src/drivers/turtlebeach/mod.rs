//! Turtle Beach (VID 0x10f5) Gen 3 wireless headsets: headset battery and
//! link state read from the transmitter's Airoha chip. The Vulcan keyboards
//! sold under this VID are ROCCAT designs and live in `drivers/roccat/`.
//!
//! Sources (layouts only, no code copied):
//! - halobattery-stealth700 (MIT) `docs/protocol.md`
//!   <https://github.com/PaulRichez/halobattery-stealth700> and HaloBattery
//!   PR #211 <https://github.com/HeyOkay/HaloBattery/pull/211>: Stealth 700X
//!   Gen 3 transmitter `2251`, headset on its own cable `2253` (same GSI
//!   read), framing, RACE `0x9942` / `0x9902`, GSI keys; key 250 does not
//!   follow the cable on this model.
//! - tb-heal (MIT) `tb-heal` <https://github.com/richard523/tb-heal>: the same
//!   exchange for `2251` / `2253` on Linux.
//! - tb-stealth600 `tb600.py` <https://github.com/Jcbeleno/tb-stealth600> (no
//!   licence: read for layouts only): Stealth 600 Gen 3 PC transmitter `224c`,
//!   hidraw write + HIDIOCGINPUT, CoAP GET in RACE `0x9902`, GSI 240 battery
//!   (255 = unknown) and 250 charging. Device name: linux-hardware.org
//!   `usb:10f5-224c` ("Stealth 600PC Gen 3").
//!
//! 62-byte reports on the 0xFF13 collection: output `06 <len> <flag>
//! <race>`, input `07 <len> <flag> <race>` fetched with HIDIOCGINPUT
//! (len 0 = nothing queued). RACE packets `05 <type> <len16 LE> <id16 LE>
//! <payload>` (5A request, 5B response, 5D indication; len counts the id)
//! may span reports. Status is CoAP `GET /GSI` tunnelled in `0x9902`; the
//! reply payload is `{"OR":"GSI","KVP":{"230":"1","240":"86",...}}`.
//!
//! Reads are the GSI status plus, per model, the Mic / NG (Stealth Pro II:
//! Mic / SAF) resources for the settings in `settings.rs`; the only other
//! write besides those is the 700's CoAP-client enable, which Swarm II sends
//! before every read. tb-heal reports that on its Linux
//! kernel the `2251` dongle ignored hidraw and needed libusb control
//! transfers with usbhid detached; tb-stealth600 uses hidraw on `224c`. This
//! driver uses hidraw for both and is untested on hardware. Settings other
//! than those in `settings.rs` (EQ, buttons, ...) are not offered: see
//! `docs/onboard-writes/turtlebeach.md`.
//!
//! Swarm II Mobile 6.27 (official Android app, layouts only; PIDs and key
//! maps in `docs/vendor-re/turtlebeach.md`): a write is `PUT set_kvp` in the
//! same tunnel and counts as acknowledged when a CoAP 2.xx answers it or the
//! headset pushes an update for the key within 1.5 s; one attempt.

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{enumerate_hidraw, hidioc, usage_pages, HidDeviceInfo, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingValue, SettingsMap};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use settings::Keys;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

mod halo;
mod settings;

const TURTLE_BEACH: u16 = 0x10f5;

const SIZE: usize = 62;
const REPORT_OUT: u8 = 0x06;
const REPORT_IN: u8 = 0x07;
/// RACE bytes per report (after id, length and flag).
const FRAGMENT_MAX: usize = SIZE - 3;

const MARKER: u8 = 0x05;
const REQUEST: u8 = 0x5a;
const RESPONSE: u8 = 0x5b;
const INDICATION: u8 = 0x5d;
const PACKET_MAX: usize = 2048;
const COAP_CLIENT_ON: u16 = 0x9942;
const COAP: u16 = 0x9902;

struct Spec {
    name: &'static str,
    pids: &'static [u16],
    /// Enable the chip's CoAP client before each read (the 700's sources).
    coap_client: bool,
    /// GSI key 250 is the charging flag.
    charging: bool,
    /// Key map of the settings controls.
    keys: Keys,
}

macro_rules! spec {
    ($id:ident, $name:expr, [$($pid:expr),+], $coap_client:expr, $charging:expr, $keys:expr) => {
        const $id: Spec = Spec {
            name: $name,
            pids: &[$($pid),+],
            coap_client: $coap_client,
            charging: $charging,
            keys: $keys,
        };
    };
}

// Product names and PIDs: Swarm II Mobile's PID table. Each row lists the
// transmitter (dongle) first and the headset on its own USB cable after it.

// Thunderbird = Stealth 700 Gen 3: the 700's sources (CoAP client enable, key
// 250 not the charging flag). `2253` is the headset itself on its USB-C cable.
spec!(
    STEALTH_700_GEN3,
    "Turtle Beach Stealth 700X Gen 3",
    [0x2251, 0x2250, 0x2253],
    true,
    false,
    Keys::V1
);
spec!(
    STEALTH_700_GEN3_BLUE,
    "Turtle Beach Stealth 700X Gen 3 (blue)",
    [0x2267, 0x2266, 0x2269],
    true,
    false,
    Keys::V1
);
spec!(
    STEALTH_700_GEN3_PC,
    "Turtle Beach Stealth 700 Gen 3 (PC)",
    [0x2262, 0x2264],
    true,
    false,
    Keys::V1
);
spec!(
    STEALTH_700_GEN3_PS,
    "Turtle Beach Stealth 700 Gen 3 (PlayStation)",
    [0x2256, 0x226b, 0x2258, 0x226d],
    true,
    false,
    Keys::V1
);

// Sparrow = Stealth 600 Gen 3: the 600's sources (no client enable).
spec!(
    STEALTH_600_GEN3,
    "Turtle Beach Stealth 600 Gen 3 (PC)",
    [0x224c, 0x225a, 0x224e, 0x225c],
    false,
    true,
    Keys::V1
);
spec!(
    STEALTH_600_GEN3_PS,
    "Turtle Beach Stealth 600 Gen 3 (PlayStation)",
    [0x2238, 0x2242, 0x223a, 0x2244],
    false,
    true,
    Keys::V1
);
spec!(
    STEALTH_600_GEN3_XBOX,
    "Turtle Beach Stealth 600 Gen 3 (Xbox)",
    [0x2233, 0x2232, 0x223d, 0x223c, 0x2235, 0x223f, 0x2278],
    false,
    true,
    Keys::V1
);

// Merlin = Stealth 500, Iceman = Atlas Air: the same Mosit stack as the 600;
// no hardware report for these, the 600's flags are assumed.
spec!(
    STEALTH_500_PC,
    "Turtle Beach Stealth 500 (PC)",
    [0x2271, 0x2273],
    false,
    true,
    Keys::V1
);
spec!(
    STEALTH_500_PS,
    "Turtle Beach Stealth 500 (PlayStation)",
    [0x222e, 0x2230, 0x2282],
    false,
    true,
    Keys::V1
);
spec!(
    STEALTH_500_XBOX,
    "Turtle Beach Stealth 500 (Xbox)",
    [0x222a, 0x2229, 0x222c, 0x227e],
    false,
    true,
    Keys::V1
);
spec!(
    STEALTH_500_SWITCH2,
    "Turtle Beach Stealth 500 (Switch 2)",
    [0x22a4, 0x22a6, 0x22a8, 0x22a5, 0x22a7, 0x22a9],
    false,
    true,
    Keys::V1
);
spec!(
    ATLAS_AIR,
    "Turtle Beach Atlas Air (PC)",
    [0x225e, 0x2260],
    false,
    true,
    Keys::V1
);

// Corona = Stealth Pro II: CoAP v2 key map, same transport.
spec!(
    STEALTH_PRO_II_PC,
    "Turtle Beach Stealth Pro II (PC)",
    [0x2288, 0x2287, 0x22a1, 0x2289],
    false,
    true,
    Keys::V2
);
spec!(
    STEALTH_PRO_II_XBOX,
    "Turtle Beach Stealth Pro II (Xbox)",
    [
        0x2285, 0x2284, 0x2283, 0x229f, 0x229d, 0x229c, 0x229b, 0x22a0, 0x22b1, 0x22b0, 0x22af,
        0x2286, 0x229e, 0x22b2
    ],
    false,
    true,
    Keys::V2
);

macro_rules! model {
    ($spec:ident) => {
        Model {
            vendor_id: TURTLE_BEACH,
            product_ids: $spec.pids,
            kind: DeviceKind::Headset,
            name: $spec.name,
            usbfs: false,
            multi: false,
            open: |pid, ctx| open(&$spec, pid, ctx),
        }
    };
}

pub const MODELS: &[Model] = &[
    model!(STEALTH_700_GEN3),
    model!(STEALTH_700_GEN3_BLUE),
    model!(STEALTH_700_GEN3_PC),
    model!(STEALTH_700_GEN3_PS),
    model!(STEALTH_600_GEN3),
    model!(STEALTH_600_GEN3_PS),
    model!(STEALTH_600_GEN3_XBOX),
    model!(STEALTH_500_PC),
    model!(STEALTH_500_PS),
    model!(STEALTH_500_XBOX),
    model!(STEALTH_500_SWITCH2),
    model!(ATLAS_AIR),
    model!(STEALTH_PRO_II_PC),
    model!(STEALTH_PRO_II_XBOX),
    halo::MODEL,
];

// ── Messages ────────────────────────────────────────────────────────────────

fn race(id: u16, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![MARKER, REQUEST];
    p.extend((payload.len() as u16 + 2).to_le_bytes());
    p.extend(id.to_le_bytes());
    p.extend_from_slice(payload);
    p
}

/// One output report; every request here fits in one.
fn request_report(race: &[u8]) -> [u8; SIZE] {
    let mut r = [0u8; SIZE];
    r[0] = REPORT_OUT;
    r[1] = race.len() as u8;
    r[3..3 + race.len()].copy_from_slice(race);
    r
}

/// CoAP (RFC 7252) confirmable GET, message id 1, no token, one Uri-Path.
fn coap_get(path: &str) -> Vec<u8> {
    let mut m = vec![0x40, 0x01, 0x00, 0x01, 0xb0 | path.len() as u8];
    m.extend_from_slice(path.as_bytes());
    m
}

/// CoAP confirmable PUT with message id `mid`, no token, one Uri-Path.
fn coap_put(mid: u16, path: &str, body: &[u8]) -> Vec<u8> {
    let mut m = vec![0x40, 0x03];
    m.extend(mid.to_be_bytes());
    m.push(0xb0 | path.len() as u8);
    m.extend_from_slice(path.as_bytes());
    m.push(0xff);
    m.extend_from_slice(body);
    m
}

/// The valid RACE bytes of an input report; empty if it carries nothing.
fn fragment(r: &[u8]) -> &[u8] {
    let Some(&len) = r.get(1) else { return &[] };
    let len = usize::from(len);
    if r.first() != Some(&REPORT_IN) || len > FRAGMENT_MAX || 3 + len > r.len() {
        return &[];
    }
    &r[3..3 + len]
}

/// Take the next complete RACE packet off the front of `stream`, dropping
/// bytes that cannot start one.
fn take_packet(stream: &mut Vec<u8>) -> Option<Vec<u8>> {
    while let Some(&first) = stream.first() {
        if first != MARKER {
            stream.remove(0);
            continue;
        }
        let len = u16::from_le_bytes([*stream.get(2)?, *stream.get(3)?]);
        let size = 4 + usize::from(len);
        if size > PACKET_MAX || len < 2 {
            stream.remove(0);
            continue;
        }
        if stream.len() < size {
            return None;
        }
        return Some(stream.drain(..size).collect());
    }
    None
}

/// The CoAP message carried by a `0x9902` response or indication.
fn coap_message(p: &[u8]) -> Option<&[u8]> {
    let id = u16::from_le_bytes([*p.get(4)?, *p.get(5)?]);
    (matches!(p[1], RESPONSE | INDICATION) && id == COAP).then(|| &p[6..])
}

/// Payload of a successful (2.xx) CoAP response; empty if it has none.
fn coap_payload(m: &[u8]) -> Option<&[u8]> {
    let (&head, &code) = (m.first()?, m.get(1)?);
    if head >> 6 != 1 || code >> 5 != 2 {
        return None;
    }
    let mut i = 4 + usize::from(head & 0x0f);
    // Extended option delta / length (RFC 7252 3.1): 13 = one more byte, 14 = two.
    let ext = |nibble: u8, i: &mut usize| -> Option<usize> {
        Some(match nibble {
            13 => {
                *i += 1;
                13 + usize::from(*m.get(*i - 1)?)
            }
            14 => {
                *i += 2;
                269 + usize::from(u16::from_be_bytes([*m.get(*i - 2)?, *m.get(*i - 1)?]))
            }
            15 => return None,
            n => usize::from(n),
        })
    };
    while i < m.len() {
        let b = m[i];
        i += 1;
        if b == 0xff {
            return Some(&m[i..]);
        }
        ext(b >> 4, &mut i)?;
        i += ext(b & 0x0f, &mut i)?;
    }
    (i == m.len()).then_some(&[][..])
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Status {
    linked: bool,
    battery: Option<u8>,
    charging: Option<bool>,
}

type Kvp = BTreeMap<String, String>;

/// The key/value pairs of resource `name` from its `{"OR":..,"KVP":{..}}`
/// document.
fn parse_resource(body: &[u8], name: &str) -> Option<Kvp> {
    let doc: serde_json::Value = serde_json::from_slice(body).ok()?;
    if !doc.get("OR")?.as_str()?.eq_ignore_ascii_case(name) {
        return None;
    }
    Some(
        doc.get("KVP")?
            .as_object()?
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect(),
    )
}

/// Status from the GSI pairs. Key 230 is the active dongle host (0 none,
/// 1 Xbox, 2 PC, 3 PlayStation; Swarm II `ActiveDongleHostSetting`): linked
/// when it is not 0. Without it (not in the 600's sources) the headset counts
/// as linked while it reports a battery level.
fn gsi_status(spec: &Spec, kvp: &Kvp) -> Status {
    let key = |k: &str| kvp.get(k).map(String::as_str);
    let battery = key("240")
        .and_then(|v| v.parse::<u8>().ok())
        .filter(|&p| p <= 100);
    let linked = key("230").map_or(battery.is_some(), |v| v != "0");
    let charging = spec.charging.then(|| key("250") == Some("1"));
    Status {
        linked,
        battery,
        charging,
    }
}

#[cfg(test)]
fn parse_gsi(spec: &Spec, body: &[u8]) -> Option<Status> {
    parse_resource(body, "GSI").map(|k| gsi_status(spec, &k))
}

// ── Transport ───────────────────────────────────────────────────────────────

/// Report transport, so the exchange can be tested without hardware.
trait Link: Send {
    fn send(&mut self, report: &[u8]) -> Result<()>;
    /// One input report (`buf[0]` = report id); returns the length read.
    fn input(&mut self, buf: &mut [u8]) -> Result<usize>;
}

struct Dongle {
    dev: HidrawDevice,
    /// Second handle for HIDIOCGINPUT (replies are fetched, not pushed).
    input: File,
}

impl Dongle {
    fn open(info: &HidDeviceInfo) -> Result<Self> {
        Ok(Self {
            dev: HidrawDevice::open(&info.path)?,
            input: OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&info.path)?,
        })
    }
}

impl Link for Dongle {
    fn send(&mut self, report: &[u8]) -> Result<()> {
        self.dev.write(report)
    }

    fn input(&mut self, buf: &mut [u8]) -> Result<usize> {
        let req = hidioc(0x0a, buf.len());
        let ret = unsafe { libc::ioctl(self.input.as_raw_fd(), req, buf.as_mut_ptr()) };
        if ret < 0 {
            bail!("HIDIOCGINPUT: {}", std::io::Error::last_os_error());
        }
        Ok((ret as usize).min(buf.len()))
    }
}

/// The 0xFF13 (RACE) collection's node.
fn find_node(pid: u16) -> Result<HidDeviceInfo> {
    enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == TURTLE_BEACH && d.product_id == pid)
        .find(|d| usage_pages(&d.report_descriptor()).contains(&0xff13))
        .with_context(|| format!("no RACE node for {TURTLE_BEACH:04x}:{pid:04x}"))
}

const POLL: Duration = Duration::from_secs(15);
/// tb-heal waits 100 ms after the enable, HaloBattery 300 ms.
const ENABLE_PAUSE: Duration = Duration::from_millis(300);
const READ_GAP: Duration = Duration::from_millis(30);
const REPLY_WAIT: Duration = Duration::from_millis(1500);
const DRAIN_MAX: usize = 16;

/// Pop queued input reports until the chip reports nothing left.
fn drain(link: &mut dyn Link) -> Result<()> {
    for _ in 0..DRAIN_MAX {
        let mut buf = [0u8; SIZE];
        buf[0] = REPORT_IN;
        let n = link.input(&mut buf)?;
        if fragment(&buf[..n]).is_empty() {
            break;
        }
    }
    Ok(())
}

/// One GET of resource `name`; None = no answer in time. The document may
/// come in several 0x9902 segments.
fn read_resource(link: &mut dyn Link, name: &str) -> Result<Option<Kvp>> {
    drain(link)?;
    link.send(&request_report(&race(COAP, &coap_get(name))))?;
    let deadline = Instant::now() + REPLY_WAIT;
    let (mut stream, mut body) = (Vec::new(), Vec::new());
    while Instant::now() < deadline {
        std::thread::sleep(READ_GAP);
        let mut buf = [0u8; SIZE];
        buf[0] = REPORT_IN;
        let n = link.input(&mut buf)?;
        stream.extend_from_slice(fragment(&buf[..n]));
        while let Some(p) = take_packet(&mut stream) {
            let Some(payload) = coap_message(&p).and_then(coap_payload) else {
                continue;
            };
            if payload.first() == Some(&b'{') {
                body.clear();
            }
            body.extend_from_slice(payload);
            if let Some(k) = parse_resource(&body, name) {
                return Ok(Some(k));
            }
        }
    }
    Ok(None)
}

/// Send the CoAP-client enable the 700's sources want before an exchange.
fn enable_client(spec: &Spec, link: &mut dyn Link) -> Result<()> {
    drain(link)?;
    if spec.coap_client {
        link.send(&request_report(&race(COAP_CLIENT_ON, &[])))?;
        std::thread::sleep(ENABLE_PAUSE);
        drain(link)?;
    }
    Ok(())
}

/// One GSI read: status and the raw pairs; None = no answer in time.
fn read_gsi(spec: &Spec, link: &mut dyn Link) -> Result<Option<(Status, Kvp)>> {
    enable_client(spec, link)?;
    Ok(read_resource(link, "GSI")?.map(|k| (gsi_status(spec, &k), k)))
}

/// Settings from the model's other resources; a resource that does not
/// answer is skipped, and the first silent one ends the pass.
fn read_settings(spec: &Spec, link: &mut dyn Link, out: &mut SettingsMap) -> Result<()> {
    for name in settings::resources(spec.keys) {
        match read_resource(link, name)? {
            Some(kvp) => settings::decode(spec.keys, &kvp, out),
            None => break,
        }
    }
    Ok(())
}

/// How long Swarm II waits for a write to be confirmed.
const WRITE_WAIT: Duration = Duration::from_millis(1500);

/// Whether a CoAP message answers the PUT `mid` (Some(true): 2.xx, Some(false):
/// an error code) or, as an update push, mentions `key`.
fn write_ack(m: &[u8], mid: u16, key: &str) -> Option<bool> {
    if m.len() >= 4 && m[2..4] == mid.to_be_bytes() {
        match m[1] >> 5 {
            2 => return Some(true),
            4 | 5 => return Some(false),
            _ => {}
        }
    }
    let docs =
        serde_json::Deserializer::from_slice(coap_payload(m)?).into_iter::<serde_json::Value>();
    docs.flatten()
        .any(|d| d.get("UP").is_some() && d.get("KVP").and_then(|k| k.get(key)).is_some())
        .then_some(true)
}

/// `PUT set_kvp` of one key, one attempt; an error if the headset answers
/// with a CoAP error or stays silent.
fn put_kvp(spec: &Spec, link: &mut dyn Link, mid: u16, key: &str, value: &str) -> Result<()> {
    enable_client(spec, link)?;
    let body = format!(r#"{{"0x{key}":"{value}"}}"#);
    link.send(&request_report(&race(
        COAP,
        &coap_put(mid, "set_kvp", body.as_bytes()),
    )))?;
    let deadline = Instant::now() + WRITE_WAIT;
    let mut stream = Vec::new();
    while Instant::now() < deadline {
        std::thread::sleep(READ_GAP);
        let mut buf = [0u8; SIZE];
        buf[0] = REPORT_IN;
        let n = link.input(&mut buf)?;
        stream.extend_from_slice(fragment(&buf[..n]));
        while let Some(p) = take_packet(&mut stream) {
            match coap_message(&p).and_then(|m| write_ack(m, mid, key)) {
                Some(true) => return Ok(()),
                Some(false) => bail!("the headset rejected the setting"),
                None => {}
            }
        }
    }
    bail!("the headset did not confirm the setting")
}

// ── Adapter ─────────────────────────────────────────────────────────────────

fn open(spec: &'static Spec, pid: u16, _ctx: &DriverCtx) -> Opened {
    let info = find_node(pid)?;
    let dongle = Dongle::open(&info)?;
    info!("{} open via {}", spec.name, info.path.display());
    let mut a = StealthAdapter {
        spec,
        id: DeviceId {
            vendor_id: TURTLE_BEACH,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
        link: Some(Box::new(dongle)),
        battery: offline(),
        settings: SettingsMap::new(),
        mid: FIRST_PUT_ID,
        last_poll: None,
        failures: 0,
        last_error: None,
    };
    if let Err(e) = a.poll() {
        warn!("{} status: {e}", spec.name);
        a.last_error = Some(e.to_string());
    }
    Ok(vec![Box::new(a)])
}

fn offline() -> BatteryState {
    BatteryState {
        label: "Headset".into(),
        percent: None,
        charging: false,
        present: false,
    }
}

/// CoAP message id of the first write (the GET uses 1).
const FIRST_PUT_ID: u16 = 2;

struct StealthAdapter {
    spec: &'static Spec,
    id: DeviceId,
    link: Option<Box<dyn Link>>,
    battery: BatteryState,
    settings: SettingsMap,
    mid: u16,
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
}

impl StealthAdapter {
    fn poll(&mut self) -> Result<()> {
        self.last_poll = Some(Instant::now());
        let link = self.link.as_deref_mut().context("not connected")?;
        let read = read_gsi(self.spec, link)?;
        let status = read.as_ref().map(|r| r.0);
        if let Some((_, kvp)) = &read {
            settings::decode(self.spec.keys, kvp, &mut self.settings);
        }
        self.battery = match status {
            // The reply still carries the last level while the headset is off.
            Some(s) if s.linked => BatteryState {
                percent: s.battery,
                charging: s.charging == Some(true),
                present: true,
                ..offline()
            },
            _ => offline(),
        };
        debug!("{} {:?}", self.spec.name, status);
        match status {
            None => bail!("transmitter did not answer the status read"),
            Some(s) if s.linked => read_settings(self.spec, link, &mut self.settings)?,
            Some(_) => {}
        }
        Ok(())
    }
}

impl DeviceAdapter for StealthAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.spec.name.into(),
            connected: self.link.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: settings::controls(self.spec.keys),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let (coap, text) = settings::encode(self.spec.keys, key, &value)?;
        let link = self.link.as_deref_mut().context("not connected")?;
        let mid = self.mid;
        self.mid = mid.wrapping_add(1).max(FIRST_PUT_ID);
        put_kvp(self.spec, link, mid, coap, &text)?;
        self.settings.insert(key.as_str().to_string(), value);
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        if self.link.is_none() || self.last_poll.is_some_and(|t| t.elapsed() < POLL) {
            return Ok(());
        }
        match self.poll() {
            Ok(()) => {
                self.failures = 0;
                self.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(e.to_string());
                if self.failures >= 3 {
                    self.link = None;
                }
                Err(e)
            }
        }
    }

    fn shutdown(&mut self) {
        self.link = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const GSI_700: &str = r#"{"OR":"GSI","KVP":{"200":"0","210":"0","220":"My Headset","230":"1","240":"100","250":"0","260":"86","270":"0","280":"4","290":"0","2a0":"64","2b0":"22","2c0":"50"}}"#;

    #[test]
    fn requests_match_the_sources() {
        // CoAP client enable and GET /GSI, byte for byte (HaloBattery PR #211).
        assert_eq!(
            race(COAP_CLIENT_ON, &[]),
            [0x05, 0x5a, 0x02, 0x00, 0x42, 0x99]
        );
        assert_eq!(
            race(COAP, &coap_get("GSI")),
            [0x05, 0x5a, 0x0a, 0x00, 0x02, 0x99, 0x40, 0x01, 0x00, 0x01, 0xb3, b'G', b'S', b'I']
        );
        let r = request_report(&race(COAP_CLIENT_ON, &[]));
        assert_eq!(r.len(), 62);
        assert_eq!(r[..9], [0x06, 6, 0, 0x05, 0x5a, 0x02, 0x00, 0x42, 0x99]);
        assert!(r[9..].iter().all(|&b| b == 0));
    }

    /// Input reports carrying `race` split into report-sized fragments.
    fn reports(race: &[u8], flag: u8) -> Vec<[u8; SIZE]> {
        race.chunks(FRAGMENT_MAX)
            .map(|c| {
                let mut r = [0u8; SIZE];
                r[..3].copy_from_slice(&[REPORT_IN, c.len() as u8, flag]);
                r[3..3 + c.len()].copy_from_slice(c);
                r
            })
            .collect()
    }

    /// A RACE response carrying a CoAP 2.05 piggybacked ACK with `json`.
    fn gsi_response(json: &str, token: &[u8]) -> Vec<u8> {
        let mut coap = vec![0x60 | token.len() as u8, 0x45, 0x00, 0x01];
        coap.extend_from_slice(token);
        coap.extend_from_slice(&[0xc1, 50, 0xff]); // Content-Format: JSON
        coap.extend_from_slice(json.as_bytes());
        let mut p = race(COAP, &coap);
        p[1] = RESPONSE;
        p
    }

    #[test]
    fn replies_are_reassembled_and_parsed() {
        let resp = gsi_response(GSI_700, &[1, 2, 3, 4]);
        let parts = reports(&resp, 0);
        assert!(parts.len() > 1);
        let mut stream = vec![0xaa]; // junk before the marker is dropped
        stream.extend_from_slice(fragment(&parts[0]));
        assert_eq!(take_packet(&mut stream), None);
        for r in &parts[1..] {
            stream.extend_from_slice(fragment(r));
        }
        let p = take_packet(&mut stream).unwrap();
        assert_eq!(p, resp);
        let body = coap_message(&p).and_then(coap_payload).unwrap();
        assert_eq!(body, GSI_700.as_bytes());
        assert_eq!(
            parse_gsi(&STEALTH_700_GEN3, body),
            Some(Status {
                linked: true,
                battery: Some(100),
                charging: None,
            })
        );
        // Length 0 = nothing queued; oversized lengths and other ids are ignored.
        assert!(fragment(&[REPORT_IN, 0, 0, 0x05]).is_empty());
        assert!(fragment(&[REPORT_IN, 60, 0]).is_empty());
        assert!(fragment(&[REPORT_OUT, 1, 0, 0x05]).is_empty());
        // A CoAP error (4.04) carries no status.
        assert_eq!(coap_payload(&[0x60, 0x84, 0, 1]), None);
        assert_eq!(coap_payload(&[0x60, 0x44, 0, 1]), Some(&[][..]));
    }

    #[test]
    fn gsi_keys_per_model() {
        let off = r#"{"OR":"GSI","KVP":{"230":"0","240":"71","250":"0"}}"#;
        assert_eq!(
            parse_gsi(&STEALTH_700_GEN3, off.as_bytes()),
            Some(Status {
                linked: false,
                battery: Some(71),
                charging: None,
            })
        );
        // 230 is the active dongle host (2 = PC), not a 0/1 flag.
        let pc = r#"{"OR":"GSI","KVP":{"230":"2","240":"50"}}"#;
        assert!(parse_gsi(&STEALTH_700_GEN3, pc.as_bytes()).unwrap().linked);
        // 600 Gen 3: 250 is charging, 255 is an unknown level.
        let charging = r#"{"OR":"GSI","KVP":{"240":"42","250":"1"}}"#;
        assert_eq!(
            parse_gsi(&STEALTH_600_GEN3, charging.as_bytes()),
            Some(Status {
                linked: true,
                battery: Some(42),
                charging: Some(true),
            })
        );
        let unknown = r#"{"OR":"GSI","KVP":{"240":"255","250":"0"}}"#;
        assert_eq!(
            parse_gsi(&STEALTH_600_GEN3, unknown.as_bytes()),
            Some(Status {
                linked: false,
                battery: None,
                charging: Some(false),
            })
        );
        // Pushes and other resources are not the status record.
        assert_eq!(
            parse_gsi(&STEALTH_700_GEN3, br#"{"UP":"GSI","KVP":{"240":"5"}}"#),
            None
        );
        assert_eq!(parse_gsi(&STEALTH_700_GEN3, b"{\"OR\":\"GSI\""), None);
    }

    type Sent = Arc<Mutex<Vec<Vec<u8>>>>;

    const MIC_DOC: &str = r#"{"OR":"Mic","KVP":{"600":"0","610":"60","620":"1","630":"35"}}"#;
    const NG_DOC: &str = r#"{"OR":"NG","KVP":{"700":"1","710":"20"}}"#;

    /// How the transmitter answers a `PUT set_kvp`.
    #[derive(Clone, Copy)]
    enum Put {
        Silent,
        /// CoAP 2.04 with the PUT's message id.
        Changed,
        /// An empty ACK, then an update push for the key.
        Push,
        /// CoAP 4.00.
        Rejected,
    }

    /// Transmitter double: queues a stale report, then answers GET of a
    /// known resource with its document split over two 0x9902 segments.
    struct FakeTransmitter {
        sent: Sent,
        queue: VecDeque<[u8; SIZE]>,
        docs: Vec<(&'static str, &'static str)>,
        put: Put,
    }

    impl Link for FakeTransmitter {
        fn send(&mut self, r: &[u8]) -> Result<()> {
            assert_eq!(r.len(), SIZE);
            self.sent.lock().unwrap().push(fragment_out(r).to_vec());
            let out = fragment_out(r);
            if out[4..6] != COAP.to_le_bytes() {
                return Ok(());
            }
            let m = &out[6..];
            if m[1] == 0x01 {
                let path = std::str::from_utf8(&m[5..5 + usize::from(m[4] & 0x0f)]).unwrap();
                if let Some((_, json)) = self.docs.iter().find(|d| d.0 == path) {
                    let (a, b) = json.split_at(json.len() / 2);
                    let first = gsi_response(a, &[]);
                    let mut second = race(COAP, &[0x60, 0x45, 0x00, 0x02, 0xff]);
                    second.extend_from_slice(b.as_bytes());
                    let n = (second.len() - 4) as u16;
                    second[2..4].copy_from_slice(&n.to_le_bytes());
                    second[1] = INDICATION;
                    for p in [first, second] {
                        self.queue.extend(reports(&p, 0));
                    }
                }
            } else if m[1] == 0x03 {
                let reply = |code: u8, mid: &[u8], body: &[u8]| {
                    let mut c = vec![0x60, code, mid[0], mid[1]];
                    if !body.is_empty() {
                        c.push(0xff);
                        c.extend_from_slice(body);
                    }
                    let mut p = race(COAP, &c);
                    p[1] = if body.is_empty() {
                        RESPONSE
                    } else {
                        INDICATION
                    };
                    reports(&p, 0)
                };
                let push = br#"{"UP":"Mic","KVP":{"620":"0"}}"#;
                match self.put {
                    Put::Silent => {}
                    Put::Changed => self.queue.extend(reply(0x44, &m[2..4], &[])),
                    Put::Push => {
                        self.queue.extend(reply(0x00, &m[2..4], &[]));
                        self.queue.extend(reply(0x45, &[0, 9], push));
                    }
                    Put::Rejected => self.queue.extend(reply(0x80, &m[2..4], &[])),
                }
            }
            Ok(())
        }

        fn input(&mut self, buf: &mut [u8]) -> Result<usize> {
            match self.queue.pop_front() {
                Some(r) => buf.copy_from_slice(&r),
                None => buf[1..].fill(0),
            }
            Ok(buf.len())
        }
    }

    fn fragment_out(r: &[u8]) -> &[u8] {
        &r[3..3 + usize::from(r[1])]
    }

    fn adapter(spec: &'static Spec, gsi: Option<&'static str>) -> (StealthAdapter, Sent) {
        adapter_put(spec, gsi, Put::Changed)
    }

    fn adapter_put(
        spec: &'static Spec,
        gsi: Option<&'static str>,
        put: Put,
    ) -> (StealthAdapter, Sent) {
        let sent = Sent::default();
        let stale = reports(&gsi_response(r#"{"OR":"GSI","KVP":{"240":"1"}}"#, &[]), 0);
        let fake = FakeTransmitter {
            sent: sent.clone(),
            queue: stale.into_iter().collect(),
            docs: gsi
                .map(|g| vec![("GSI", g), ("Mic", MIC_DOC), ("NG", NG_DOC)])
                .unwrap_or_default(),
            put,
        };
        let a = StealthAdapter {
            spec,
            id: DeviceId {
                vendor_id: TURTLE_BEACH,
                product_id: spec.pids[0],
                instance: "test".into(),
            },
            link: Some(Box::new(fake)),
            battery: offline(),
            settings: SettingsMap::new(),
            mid: FIRST_PUT_ID,
            last_poll: None,
            failures: 0,
            last_error: None,
        };
        (a, sent)
    }

    #[test]
    fn poll_reads_status_then_settings() {
        let (mut a, sent) = adapter(&STEALTH_700_GEN3, Some(GSI_700));
        a.poll().unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            [
                race(COAP_CLIENT_ON, &[]),
                race(COAP, &coap_get("GSI")),
                race(COAP, &coap_get("Mic")),
                race(COAP, &coap_get("NG")),
            ]
        );
        // The stale report was drained, not taken as the answer.
        assert_eq!(a.battery.percent, Some(100));
        assert!(a.battery.present && !a.battery.charging);
        // 280 = "4" is the 30 minute auto-off; Mic and NG fill in the rest.
        assert_eq!(a.settings["pm_shutdown"], SettingValue::Int(30));
        assert_eq!(a.settings["mic_side_tone_volume"], SettingValue::Int(35));
        assert_eq!(a.settings["mic_noise_filter"], SettingValue::Bool(true));
        assert_eq!(a.snapshot().controls.len(), 6);

        let (mut a, sent) = adapter(
            &STEALTH_600_GEN3,
            Some(r#"{"OR":"GSI","KVP":{"220":"My Headset","240":"64","250":"1"}}"#),
        );
        a.poll().unwrap();
        assert_eq!(sent.lock().unwrap()[0], race(COAP, &coap_get("GSI")));
        assert_eq!(a.battery.percent, Some(64));
        assert!(a.battery.present && a.battery.charging);
    }

    #[test]
    fn unlinked_headset_reads_no_settings() {
        let (mut a, sent) = adapter(
            &STEALTH_700_GEN3,
            Some(r#"{"OR":"GSI","KVP":{"230":"0","240":"71"}}"#),
        );
        a.poll().unwrap();
        assert_eq!(sent.lock().unwrap().len(), 2);
        assert!(!a.battery.present);
    }

    #[test]
    fn silent_transmitter_is_an_error_and_shows_no_battery() {
        let (mut a, _) = adapter(&STEALTH_700_GEN3, None);
        assert!(a.poll().is_err());
        assert!(!a.battery.present && a.battery.percent.is_none());
    }

    #[test]
    fn writes_are_put_set_kvp_and_wait_for_an_answer() {
        // Byte for byte: confirmable PUT 0.03, id 2, Uri-Path "set_kvp", JSON.
        let body = br#"{"0x620":"1"}"#;
        let mut want = vec![0x40, 0x03, 0x00, 0x02, 0xb7];
        want.extend_from_slice(b"set_kvp");
        want.push(0xff);
        want.extend_from_slice(body);
        assert_eq!(coap_put(2, "set_kvp", body), want);

        let (mut a, sent) = adapter(&STEALTH_700_GEN3, Some(GSI_700));
        a.set(SettingKey::MicSideTone, SettingValue::Bool(true))
            .unwrap();
        let put = sent.lock().unwrap().last().unwrap().clone();
        assert_eq!(put, race(COAP, &want));
        assert_eq!(a.settings["mic_side_tone"], SettingValue::Bool(true));
        // The next write takes the next message id.
        a.set(SettingKey::PmShutdown, SettingValue::Int(10))
            .unwrap();
        let put = sent.lock().unwrap().last().unwrap().clone();
        assert_eq!(put[8..10], [0x00, 0x03]);
        assert!(String::from_utf8_lossy(&put).contains(r#"{"0x280":"2"}"#));

        // An update push for the key confirms a write whose answer was empty.
        let (mut a, _) = adapter_put(&STEALTH_700_GEN3, Some(GSI_700), Put::Push);
        a.set(SettingKey::MicSideTone, SettingValue::Bool(false))
            .unwrap();
        assert_eq!(a.settings["mic_side_tone"], SettingValue::Bool(false));
    }

    #[test]
    fn failed_writes_leave_the_setting_alone() {
        let (mut a, _) = adapter_put(&STEALTH_700_GEN3, Some(GSI_700), Put::Rejected);
        let e = a
            .set(SettingKey::MicMute, SettingValue::Bool(true))
            .unwrap_err();
        assert!(e.to_string().contains("rejected"));
        assert!(!a.settings.contains_key("mic_mute"));
        let (mut a, _) = adapter_put(&STEALTH_700_GEN3, Some(GSI_700), Put::Silent);
        let e = a
            .set(SettingKey::MicMute, SettingValue::Bool(true))
            .unwrap_err();
        assert!(e.to_string().contains("did not confirm"));
        // Unsupported keys never reach the headset.
        assert!(a.set(SettingKey::Brightness, SettingValue::Int(1)).is_err());
    }

    /// Read-only status of a plugged-in transmitter.
    #[test]
    #[ignore]
    fn hw_read_status() {
        for spec in [&STEALTH_700_GEN3, &STEALTH_600_GEN3] {
            for &pid in spec.pids {
                let Ok(info) = find_node(pid) else { continue };
                let mut d = Dongle::open(&info).unwrap();
                eprintln!("{} {:?}", spec.name, read_gsi(spec, &mut d));
            }
        }
    }
}
