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
//! Read-only: the only other write is the 700's CoAP-client enable, which
//! Swarm II sends before every read. tb-heal reports that on its Linux
//! kernel the `2251` dongle ignored hidraw and needed libusb control
//! transfers with usbhid detached; tb-stealth600 uses hidraw on `224c`. This
//! driver uses hidraw for both and is untested on hardware. Settings (noise
//! gate, EQ, ...) are not offered: see `docs/onboard-writes/turtlebeach.md`.

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{enumerate_hidraw, hidioc, usage_pages, HidDeviceInfo, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingValue};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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
}

/// Transmitter first; `2253` is the headset itself on its USB-C cable.
const STEALTH_700_GEN3: Spec = Spec {
    name: "Turtle Beach Stealth 700X Gen 3",
    pids: &[0x2251, 0x2253],
    coap_client: true,
    charging: false,
};
const STEALTH_600_GEN3: Spec = Spec {
    name: "Turtle Beach Stealth 600 Gen 3 (PC)",
    pids: &[0x224c],
    coap_client: false,
    charging: true,
};

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

pub const MODELS: &[Model] = &[model!(STEALTH_700_GEN3), model!(STEALTH_600_GEN3)];

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

/// Status from a GSI document. Without key 230 (not in the 600's sources) the
/// headset counts as linked while it reports a battery level.
fn parse_gsi(spec: &Spec, body: &[u8]) -> Option<Status> {
    let doc: serde_json::Value = serde_json::from_slice(body).ok()?;
    if doc.get("OR")?.as_str()? != "GSI" {
        return None;
    }
    let kvp = doc.get("KVP")?.as_object()?;
    let key = |k: &str| kvp.get(k).and_then(|v| v.as_str());
    let battery = key("240")
        .and_then(|v| v.parse::<u8>().ok())
        .filter(|&p| p <= 100);
    let linked = key("230").map_or(battery.is_some(), |v| v == "1");
    let charging = spec.charging.then(|| key("250") == Some("1"));
    Some(Status {
        linked,
        battery,
        charging,
    })
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

/// One GSI read; None = no answer in time.
fn read_gsi(spec: &Spec, link: &mut dyn Link) -> Result<Option<Status>> {
    drain(link)?;
    if spec.coap_client {
        link.send(&request_report(&race(COAP_CLIENT_ON, &[])))?;
        std::thread::sleep(ENABLE_PAUSE);
        drain(link)?;
    }
    link.send(&request_report(&race(COAP, &coap_get("GSI"))))?;
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
            // The document may come in several 0x9902 segments.
            if payload.first() == Some(&b'{') {
                body.clear();
            }
            body.extend_from_slice(payload);
            if let Some(s) = parse_gsi(spec, &body) {
                return Ok(Some(s));
            }
        }
    }
    Ok(None)
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

struct StealthAdapter {
    spec: &'static Spec,
    id: DeviceId,
    link: Option<Box<dyn Link>>,
    battery: BatteryState,
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
}

impl StealthAdapter {
    fn poll(&mut self) -> Result<()> {
        self.last_poll = Some(Instant::now());
        let link = self.link.as_deref_mut().context("not connected")?;
        let status = read_gsi(self.spec, link)?;
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
        if status.is_none() {
            bail!("transmitter did not answer the status read");
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
            settings: Default::default(),
            controls: vec![],
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("unsupported setting {}", key.as_str())
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

    /// Transmitter double: queues a stale report, then answers GET /GSI
    /// with `json` split over two 0x9902 segments.
    struct FakeTransmitter {
        sent: Sent,
        queue: VecDeque<[u8; SIZE]>,
        json: Option<&'static str>,
    }

    impl Link for FakeTransmitter {
        fn send(&mut self, r: &[u8]) -> Result<()> {
            assert_eq!(r.len(), SIZE);
            self.sent.lock().unwrap().push(fragment_out(r).to_vec());
            if fragment_out(r)[4..6] == COAP.to_le_bytes() {
                if let Some(json) = self.json {
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

    fn adapter(spec: &'static Spec, json: Option<&'static str>) -> (StealthAdapter, Sent) {
        let sent = Sent::default();
        let stale = reports(&gsi_response(r#"{"OR":"GSI","KVP":{"240":"1"}}"#, &[]), 0);
        let fake = FakeTransmitter {
            sent: sent.clone(),
            queue: stale.into_iter().collect(),
            json,
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
            last_poll: None,
            failures: 0,
            last_error: None,
        };
        (a, sent)
    }

    #[test]
    fn poll_sends_only_the_status_read() {
        let (mut a, sent) = adapter(&STEALTH_700_GEN3, Some(GSI_700));
        a.poll().unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            [race(COAP_CLIENT_ON, &[]), race(COAP, &coap_get("GSI"))]
        );
        // The stale report was drained, not taken as the answer.
        assert_eq!(a.battery.percent, Some(100));
        assert!(a.battery.present && !a.battery.charging);

        let (mut a, sent) = adapter(
            &STEALTH_600_GEN3,
            Some(r#"{"OR":"GSI","KVP":{"220":"My Headset","240":"64","250":"1"}}"#),
        );
        a.poll().unwrap();
        assert_eq!(*sent.lock().unwrap(), [race(COAP, &coap_get("GSI"))]);
        assert_eq!(a.battery.percent, Some(64));
        assert!(a.battery.present && a.battery.charging);
    }

    #[test]
    fn silent_transmitter_is_an_error_and_shows_no_battery() {
        let (mut a, _) = adapter(&STEALTH_700_GEN3, None);
        assert!(a.poll().is_err());
        assert!(!a.battery.present && a.battery.percent.is_none());
        assert!(a.snapshot().controls.is_empty());
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
