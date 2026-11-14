//! "Bragi" protocol (K70 RGB PRO, K100, K65 Mini, Dark Core RGB PRO, M65 RGB
//! Ultra, Slipstream receivers, Virtuoso/HS80 headsets).
//!
//! Frame: `[addr, cmd, ...]`, addr 0x08 = the device itself, 0x08|n = paired
//! device n behind a receiver. Replies echo the command at \[1\], carry a
//! status at \[2\] (0 = ok) and a little-endian value from \[3\]; their \[0\] is
//! the addr or just its low bits (0x00 self, 0x01 first paired device).
//! Keyboards and mice use unnumbered reports on usage page 0xFF42; headsets
//! put a 0x02 byte in front of requests and get 0x01 in front of replies.
//!
//! Sources: ckb-next `src/daemon/bragi_proto.h`, `bragi_common.c`,
//! `device_bragi.c`, `led_bragi.c`, `dpi_bragi.c` (frames, properties,
//! handles, LED counts); OpenRGB `Controllers/CorsairPeripheralV2Controller`
//! (lighting resource 0x22 with `12 00` RGB triplets, pinned for K100/K65
//! Mini; resource 0x01 planar RRR..GGG..BBB fallback); OpenLinkHub
//! `src/devices/*` (PIDs, LED counts, DPI ranges, poll-rate values);
//! HeadsetControl `lib/devices/corsair_virtuoso_xt.hpp`,
//! `corsair_void_v2w.hpp` and github.com/joanjgm/hs80-battery (headset
//! framing, battery in tenths of a percent, writes persist on headsets).
//!
//! Property writes are kept in the device's onboard memory: only `store`
//! (read, validate, write, read back) writes the poll rate, angle snapping
//! and headset brightness, and only for a user change; `onboard.rs` lists
//! the models and the sources.

use super::{hid, Backend};
use crate::hid::{HidDeviceInfo, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::device::BatteryState;
use std::time::{Duration, Instant};

pub const USAGE_PAGE: u16 = 0xff42;
pub const SELF: u8 = 0x08;

const SET: u8 = 0x01;
const GET: u8 = 0x02;
const CLOSE_HANDLE: u8 = 0x05;
const WRITE_DATA: u8 = 0x06;
const CONTINUE_WRITE: u8 = 0x07;
const OPEN_HANDLE: u8 = 0x0d;
const PING: u8 = 0x12;

pub const PROP_POLLRATE: u8 = 0x01;
pub const PROP_BRIGHTNESS: u8 = 0x02;
pub const PROP_MODE: u8 = 0x03;
pub const PROP_ANGLE_SNAP: u8 = 0x07;
/// Headset sleep timer on (1) / off (0), and its timeout in ms.
pub const PROP_SLEEP_ENABLED: u8 = 0x0d;
pub const PROP_SLEEP_TIMEOUT: u8 = 0x0e;
pub const PROP_BATTERY_LEVEL: u8 = 0x0f;
pub const PROP_BATTERY_STATUS: u8 = 0x10;
pub const PROP_PID: u8 = 0x12;
pub const PROP_APP_VER: u8 = 0x13;
pub const PROP_DPI_X: u8 = 0x21;
pub const PROP_DPI_Y: u8 = 0x22;
pub const PROP_SUBDEVICES: u8 = 0x36;
/// V2 headsets: 0 = sidetone on, 1 = off (HeadsetControl
/// `corsair_void_v2w.hpp`, OpenLinkHub `voidV2W.go`); the XT reads it the
/// other way round.
pub const PROP_SIDETONE_OFF: u8 = 0x46;
/// Sidetone level, 0..=1000 in steps of 10.
pub const PROP_SIDETONE_LEVEL: u8 = 0x47;
/// Highest poll-rate value the device accepts (read only; ckb-next).
pub const PROP_MAX_POLLRATE: u8 = 0x96;
/// Active noise cancellation (0 off; Virtuoso MAX 0..=2), which has to be
/// off for sidetone.
pub const PROP_ANC: u8 = 0xd1;

pub const MODE_HARDWARE: u32 = 1;
pub const MODE_SOFTWARE: u32 = 2;
/// Brightness property scale (ckb-next, HeadsetControl).
pub const BRIGHTNESS_MAX: u32 = 1000;
pub const SIDETONE_LEVEL_MAX: u32 = 1000;
/// Longest headset sleep timer (HeadsetControl caps it at 90 minutes).
pub const SLEEP_MAX_MS: u32 = 90 * MINUTE_MS;
pub const MINUTE_MS: u32 = 60_000;

const RES_LIGHTING: u8 = 0x01;
const RES_ALT_LIGHTING: u8 = 0x22;
const LIGHTING_HANDLE: u8 = 0x00;
/// Open-handle status when the handle is still open from an earlier session.
const ERR_HANDLE_BUSY: u8 = 0x03;

const TIMEOUT: Duration = Duration::from_millis(500);

pub fn get(addr: u8, prop: u8) -> Vec<u8> {
    vec![addr, GET, prop, 0]
}

pub fn set(addr: u8, prop: u8, value: u32) -> Vec<u8> {
    let mut f = vec![addr, SET, prop, 0];
    f.extend_from_slice(&value.to_le_bytes());
    f
}

pub fn open_handle(addr: u8, handle: u8, resource: u8) -> Vec<u8> {
    vec![addr, OPEN_HANDLE, handle, resource]
}

pub fn close_handle(addr: u8, handle: u8) -> Vec<u8> {
    vec![addr, CLOSE_HANDLE, 1, handle]
}

pub fn ping(addr: u8) -> Vec<u8> {
    vec![addr, PING]
}

/// `data` for `handle` split into `pkt_len`-byte frames: the first carries
/// a 7-byte header with the total length, the rest a 3-byte header.
pub fn write_data(addr: u8, handle: u8, data: &[u8], pkt_len: usize) -> Vec<Vec<u8>> {
    let mut first = vec![addr, WRITE_DATA, handle];
    first.extend_from_slice(&(data.len() as u32).to_le_bytes());
    let n = data.len().min(pkt_len.saturating_sub(first.len()));
    first.extend_from_slice(&data[..n]);
    let mut out = vec![first];
    for chunk in data[n..].chunks(pkt_len.saturating_sub(3).max(1)) {
        let mut f = vec![addr, CONTINUE_WRITE, handle];
        f.extend_from_slice(chunk);
        out.push(f);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u8,
    pub value: u32,
}

/// The reply to a request sent as `[addr, cmd, ..]`, or None for traffic.
pub fn parse_reply(frame: &[u8], addr: u8, cmd: u8) -> Option<Reply> {
    if frame.len() < 3 || frame[1] != cmd || (frame[0] != addr && frame[0] != addr & 0x07) {
        return None;
    }
    let mut v = [0u8; 4];
    let tail = &frame[3..frame.len().min(7)];
    v[..tail.len()].copy_from_slice(tail);
    Some(Reply {
        status: frame[2],
        value: u32::from_le_bytes(v),
    })
}

/// Which lighting resource the device accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Resource 0x22: `12 00` then R G B per LED.
    Triplets,
    /// Resource 0x01: all reds, then greens, then blues.
    Planar,
}

pub fn lighting_payload(format: Format, leds: usize, rgb: [u8; 3]) -> Vec<u8> {
    match format {
        Format::Triplets => {
            let mut d = vec![0x12, 0x00];
            for _ in 0..leds {
                d.extend_from_slice(&rgb);
            }
            d
        }
        Format::Planar => rgb
            .iter()
            .flat_map(|&c| std::iter::repeat_n(c, leds))
            .collect(),
    }
}

/// Poll-rate property value (1..=7) to Hz.
pub fn pollrate_hz(v: u32) -> Option<u32> {
    (1..=7).contains(&v).then(|| 125 << (v - 1))
}

/// Hz to the poll-rate property value.
pub fn pollrate_value(hz: u32) -> Option<u32> {
    (1..=7).find(|&v| pollrate_hz(v) == Some(hz))
}

/// Values an onboard property may hold; other properties are never stored.
pub fn storable(prop: u8, value: u32) -> bool {
    match prop {
        PROP_POLLRATE => pollrate_hz(value).is_some(),
        PROP_ANGLE_SNAP => value <= 1,
        PROP_BRIGHTNESS => value <= BRIGHTNESS_MAX,
        PROP_SLEEP_ENABLED | PROP_SIDETONE_OFF => value <= 1,
        PROP_SLEEP_TIMEOUT => value <= SLEEP_MAX_MS && value.is_multiple_of(MINUTE_MS),
        PROP_SIDETONE_LEVEL => value <= SIDETONE_LEVEL_MAX && value.is_multiple_of(10),
        PROP_ANC => value <= 2,
        _ => false,
    }
}

/// Property access; `Link` on a device, a fake in tests.
pub trait Props {
    fn get(&mut self, addr: u8, prop: u8) -> Result<u32>;
    fn set(&mut self, addr: u8, prop: u8, value: u32) -> Result<()>;
}

/// Write a property the device keeps in onboard memory: read the stored
/// value and refuse to touch a property holding something unexpected, skip
/// the write when it already holds `value`, write, then read it back.
pub fn store(io: &mut impl Props, addr: u8, prop: u8, value: u32) -> Result<()> {
    if !storable(prop, value) {
        bail!("refusing to store {value} in property {prop:#04x}");
    }
    let old = io
        .get(addr, prop)
        .with_context(|| format!("read property {prop:#04x} before writing"))?;
    if !storable(prop, old) {
        bail!("property {prop:#04x} holds unexpected value {old}; not writing");
    }
    if old == value {
        return Ok(());
    }
    io.set(addr, prop, value)?;
    let now = io
        .get(addr, prop)
        .with_context(|| format!("read back property {prop:#04x} (the write may have applied)"))?;
    if now != value {
        bail!("property {prop:#04x} reads back {now} after writing {value}");
    }
    Ok(())
}

/// `store` each of `writes` on the last of `path`, in software mode, which
/// headsets require for writes (HeadsetControl). Unless the host already
/// drives the device (`software`), every address on `path` (a V2 receiver,
/// then its headset) is switched to software mode first and back to
/// hardware mode afterwards, also when a write fails.
pub fn store_in_software(
    io: &mut impl Props,
    path: &[u8],
    writes: &[(u8, u32)],
    software: bool,
) -> Result<()> {
    let &addr = path.last().context("no address to write")?;
    let store_all = |io: &mut _| {
        writes
            .iter()
            .try_for_each(|&(prop, value)| store(io, addr, prop, value))
    };
    if software {
        return store_all(io);
    }
    let mut switched = 0;
    let mut result = Ok(());
    for &a in path {
        result = io.set(a, PROP_MODE, MODE_SOFTWARE);
        if result.is_err() {
            break;
        }
        switched += 1;
    }
    if result.is_ok() {
        result = store_all(io);
    }
    for &a in path[..switched].iter().rev() {
        let back = io.set(a, PROP_MODE, MODE_HARDWARE);
        result = result.and(back);
    }
    result
}

/// One report as written to hidraw: report number 0 for an unnumbered
/// descriptor, the headset's 0x02 prefix, then `frame` zero-padded.
pub fn wire(frame: &[u8], lead_zero: bool, headset: bool, wire_len: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(wire_len + 1);
    if lead_zero {
        buf.push(0);
    }
    if headset {
        buf.push(0x02);
    }
    buf.extend_from_slice(frame);
    buf.resize(wire_len + usize::from(lead_zero), 0);
    buf
}

/// Property 0x10 (1 charging, 2 discharging, 3 full) + level in tenths.
pub fn battery(level: u32, status: Option<u32>, label: &str) -> BatteryState {
    BatteryState {
        label: label.into(),
        percent: Some((level / 10).min(100) as u8),
        charging: status == Some(1),
        present: true,
    }
}

/// One hidraw node speaking Bragi.
pub struct Link {
    dev: HidrawDevice,
    /// Unnumbered descriptor: hidraw wants a 0x00 report number first.
    lead_zero: bool,
    /// Bytes per report on the wire (request prefix included).
    wire_len: usize,
    /// Headset framing: 0x02 before requests, 0x01 before replies.
    headset: bool,
}

impl Link {
    pub fn open(info: &HidDeviceInfo, headset: bool) -> Result<Self> {
        let desc = hid::report_descriptor(info).unwrap_or_default();
        let (lead_zero, wire_len) = match (
            hid::output_report_len(&desc, 0),
            hid::output_report_len(&desc, 2),
        ) {
            (Some(n), _) => (true, n),
            (None, Some(n)) if headset => (false, n + 1),
            _ => (true, 64),
        };
        Ok(Self {
            dev: HidrawDevice::open(&info.path)?,
            lead_zero,
            wire_len,
            headset,
        })
    }

    /// Frame bytes that fit one report after the headset prefix.
    pub fn pkt_len(&self) -> usize {
        self.wire_len - usize::from(self.headset)
    }

    pub fn send(&mut self, frame: &[u8]) -> Result<()> {
        self.dev
            .write(&wire(frame, self.lead_zero, self.headset, self.wire_len))
    }

    /// Send `frame` and wait for its reply, skipping unrelated reports.
    pub fn transact(&mut self, frame: &[u8], timeout: Duration) -> Result<Reply> {
        self.dev.drain(Duration::ZERO);
        self.send(frame)?;
        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; self.wire_len.max(64) + 1];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(n) = self.dev.read_timeout(&mut buf, left) else {
                break;
            };
            let r = match (self.headset, buf.first()) {
                (true, Some(0x01)) => &buf[1..n],
                (true, _) => continue,
                (false, _) => &buf[..n],
            };
            if let Some(reply) = parse_reply(r, frame[0], frame[1]) {
                return Ok(reply);
            }
        }
        bail!("no reply to {:02x?}", &frame[..frame.len().min(4)])
    }

    pub fn get(&mut self, addr: u8, prop: u8) -> Result<u32> {
        let r = self.transact(&get(addr, prop), TIMEOUT)?;
        if r.status != 0 {
            bail!("get property {prop:#04x}: status {:#04x}", r.status);
        }
        Ok(r.value)
    }

    pub fn set(&mut self, addr: u8, prop: u8, value: u32) -> Result<()> {
        let r = self.transact(&set(addr, prop, value), TIMEOUT)?;
        if r.status != 0 {
            bail!("set property {prop:#04x}: status {:#04x}", r.status);
        }
        Ok(())
    }
}

impl Props for Link {
    fn get(&mut self, addr: u8, prop: u8) -> Result<u32> {
        Link::get(self, addr, prop)
    }
    fn set(&mut self, addr: u8, prop: u8, value: u32) -> Result<()> {
        Link::set(self, addr, prop, value)
    }
}

/// Addresses of the paired devices a subdevice bitmask (property 0x36) lists.
pub fn subdevice_addrs(bits: u32) -> impl Iterator<Item = u8> {
    (1..8u8)
        .filter(move |i| bits >> i & 1 == 1)
        .map(|i| SELF | i)
}

/// Paired devices behind a receiver: (addr, product id). Addresses `skip`
/// accepts (already open) are not queried, so their adapters see no
/// foreign replies.
pub fn paired(link: &mut Link, skip: impl Fn(u8) -> bool) -> Result<Vec<(u8, u16)>> {
    let bits = link.get(SELF, PROP_SUBDEVICES)?;
    Ok(subdevice_addrs(bits)
        .filter(|&addr| !skip(addr))
        .filter_map(|addr| {
            let pid = link.get(addr, PROP_PID).ok()? as u16;
            (pid != 0 && pid != 0xffff).then_some((addr, pid))
        })
        .collect())
}

/// A keyboard or mouse, wired or behind a receiver.
pub struct Bragi {
    link: Link,
    addr: u8,
    leds: usize,
    format: Option<Format>,
    software: Option<bool>,
    wireless: bool,
}

impl Bragi {
    pub fn new(link: Link, addr: u8, leds: u16, wireless: bool) -> Self {
        Self {
            link,
            addr,
            leds: usize::from(leds),
            format: None,
            software: None,
            wireless,
        }
    }

    /// Fails when nothing answers on `addr` (asleep, unpaired, wrong node).
    pub fn probe(&mut self) -> Result<()> {
        self.link.get(self.addr, PROP_MODE).map(drop)
    }

    fn open_lighting(&mut self) -> Result<Format> {
        for (res, format) in [
            (RES_ALT_LIGHTING, Format::Triplets),
            (RES_LIGHTING, Format::Planar),
        ] {
            let open = open_handle(self.addr, LIGHTING_HANDLE, res);
            let mut r = self.link.transact(&open, TIMEOUT)?;
            if r.status == ERR_HANDLE_BUSY {
                self.link
                    .transact(&close_handle(self.addr, LIGHTING_HANDLE), TIMEOUT)?;
                r = self.link.transact(&open, TIMEOUT)?;
            }
            if r.status == 0 {
                return Ok(format);
            }
        }
        bail!("device refused both lighting resources")
    }
}

impl Backend for Bragi {
    fn software(&mut self, on: bool) -> Result<()> {
        if self.software == Some(on) {
            return Ok(());
        }
        // Unknown until the switch completes: one that fails midway (mode
        // set, handle not open; handle closed, mode not set) is redone in
        // full next time.
        self.software = None;
        if on {
            self.link.set(self.addr, PROP_MODE, MODE_SOFTWARE)?;
            if self.leds > 0 {
                self.format = Some(self.open_lighting()?);
            }
        } else {
            if self.format.take().is_some() {
                let _ = self
                    .link
                    .transact(&close_handle(self.addr, LIGHTING_HANDLE), TIMEOUT);
            }
            self.link.set(self.addr, PROP_MODE, MODE_HARDWARE)?;
        }
        self.software = Some(on);
        Ok(())
    }

    fn color(&mut self, rgb: [u8; 3]) -> Result<()> {
        let format = self.format.context("lighting handle not open")?;
        let data = lighting_payload(format, self.leds, rgb);
        let frames = write_data(self.addr, LIGHTING_HANDLE, &data, self.link.pkt_len());
        let first = self.link.transact(&frames[0], TIMEOUT)?;
        if first.status != 0 {
            bail!("lighting write: status {:#04x}", first.status);
        }
        // Continuation frames are not reliably acknowledged (OpenRGB paces them).
        for f in &frames[1..] {
            let _ = self.link.transact(f, Duration::from_millis(20));
        }
        Ok(())
    }

    fn dpi(&mut self, dpi: u16) -> Result<()> {
        // ckb-next ignores the status here: the first write after a mode
        // switch is reported failed although it applies.
        for prop in [PROP_DPI_X, PROP_DPI_Y] {
            self.link
                .transact(&set(self.addr, prop, u32::from(dpi)), TIMEOUT)?;
        }
        Ok(())
    }

    fn battery(&mut self) -> Result<Option<BatteryState>> {
        if !self.wireless {
            return Ok(None);
        }
        let level = self.link.get(self.addr, PROP_BATTERY_LEVEL)?;
        let status = self.link.get(self.addr, PROP_BATTERY_STATUS).ok();
        Ok(Some(battery(level, status, "Battery")))
    }

    fn keepalive(&mut self) -> Result<()> {
        // iCUE pings about every 50 s; some boards (K55 RGB PRO) fall back to
        // their onboard lighting without it.
        self.link.transact(&ping(self.addr), TIMEOUT).map(drop)
    }

    fn read_dpi(&mut self) -> Option<u16> {
        self.link.get(self.addr, PROP_DPI_X).ok().map(|v| v as u16)
    }

    fn report_rate(&mut self) -> Option<u32> {
        pollrate_hz(self.link.get(self.addr, PROP_POLLRATE).ok()?)
    }

    fn store(&mut self, writes: &[(u8, u32)]) -> Result<()> {
        let software = self.software == Some(true);
        store_in_software(&mut self.link, &[self.addr], writes, software)
    }

    fn stored(&mut self, prop: u8) -> Option<u32> {
        self.link.get(self.addr, prop).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_frames_match_ckb_next() {
        assert_eq!(get(0x08, PROP_MODE), [0x08, 0x02, 0x03, 0x00]);
        assert_eq!(
            set(0x08, PROP_MODE, MODE_SOFTWARE),
            [0x08, 0x01, 0x03, 0x00, 0x02, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            set(0x09, PROP_DPI_X, 1600),
            [0x09, 0x01, 0x21, 0x00, 0x40, 0x06, 0x00, 0x00]
        );
        assert_eq!(
            open_handle(0x08, 0, RES_ALT_LIGHTING),
            [0x08, 0x0d, 0x00, 0x22]
        );
        assert_eq!(close_handle(0x08, 0), [0x08, 0x05, 0x01, 0x00]);
        assert_eq!(ping(0x09), [0x09, 0x12]);
    }

    #[test]
    fn onboard_frames_match_sources() {
        // ckb-next cmd_pollrate_bragi / OpenLinkHub cmdSetPollingRate (1 ms).
        assert_eq!(
            set(SELF, PROP_POLLRATE, pollrate_value(1000).unwrap()),
            [0x08, 0x01, 0x01, 0x00, 0x04, 0x00, 0x00, 0x00]
        );
        // ckb-next cmd_snap_bragi / OpenLinkHub cmdAngleSnapping (on).
        assert_eq!(
            set(SELF, PROP_ANGLE_SNAP, 1),
            [0x08, 0x01, 0x07, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
        // OpenLinkHub transfer(): report number 0, then 0x08 01 01 00 <rate>.
        let w = wire(&set(SELF, PROP_POLLRATE, 7), true, false, 64);
        assert_eq!(w.len(), 65);
        assert_eq!(w[..7], [0x00, 0x08, 0x01, 0x01, 0x00, 0x07, 0x00]);
        assert!(w[7..].iter().all(|&b| b == 0));
        // HeadsetControl writeProperty: report 0x02, headset 0x09, SET,
        // brightness, 0x00, 1000 LE; and software mode before it.
        let w = wire(
            &set(SELF | 1, PROP_BRIGHTNESS, BRIGHTNESS_MAX),
            false,
            true,
            64,
        );
        assert_eq!(w.len(), 64);
        assert_eq!(
            w[..10],
            [0x02, 0x09, 0x01, 0x02, 0x00, 0xe8, 0x03, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            wire(&set(SELF | 1, PROP_MODE, MODE_SOFTWARE), false, true, 64)[..6],
            [0x02, 0x09, 0x01, 0x03, 0x00, 0x02]
        );
    }

    /// A device's property table that logs every write.
    #[derive(Default)]
    struct Fake {
        props: std::collections::HashMap<u8, u32>,
        writes: Vec<(u8, u32)>,
        /// Value the firmware keeps instead of the one written.
        sticky: Option<u32>,
    }

    impl Props for Fake {
        fn get(&mut self, _addr: u8, prop: u8) -> Result<u32> {
            self.props.get(&prop).copied().context("status 0x05")
        }
        fn set(&mut self, _addr: u8, prop: u8, value: u32) -> Result<()> {
            self.writes.push((prop, value));
            if prop != PROP_MODE {
                self.props.insert(prop, self.sticky.unwrap_or(value));
            }
            Ok(())
        }
    }

    fn fake(prop: u8, value: u32) -> Fake {
        Fake {
            props: [(prop, value)].into(),
            ..Fake::default()
        }
    }

    #[test]
    fn store_reads_validates_writes_and_verifies() {
        let mut d = fake(PROP_POLLRATE, 4);
        store(&mut d, SELF, PROP_POLLRATE, 7).unwrap();
        assert_eq!(d.writes, [(PROP_POLLRATE, 7)]);
        // Already stored: no write, no wear.
        store(&mut d, SELF, PROP_POLLRATE, 7).unwrap();
        assert_eq!(d.writes.len(), 1);

        // An unexpected stored value aborts before writing.
        let mut d = fake(PROP_POLLRATE, 9);
        assert!(store(&mut d, SELF, PROP_POLLRATE, 4).is_err());
        assert!(d.writes.is_empty());
        // So does a property the device does not answer.
        let mut d = Fake::default();
        assert!(store(&mut d, SELF, PROP_ANGLE_SNAP, 1).is_err());
        assert!(d.writes.is_empty());
        // Values and properties outside the allow-list are never sent.
        let mut d = fake(PROP_BRIGHTNESS, 0);
        assert!(store(&mut d, SELF, PROP_BRIGHTNESS, 1001).is_err());
        assert!(store(&mut d, SELF, PROP_MODE, 2).is_err());
        assert!(d.writes.is_empty());

        // A read-back that differs is an error.
        let mut d = fake(PROP_ANGLE_SNAP, 0);
        d.sticky = Some(0);
        let e = store(&mut d, SELF, PROP_ANGLE_SNAP, 1).unwrap_err();
        assert!(e.to_string().contains("reads back 0"), "{e}");
    }

    #[test]
    fn store_brackets_software_mode_when_needed() {
        let mut d = fake(PROP_BRIGHTNESS, 1000);
        store_in_software(&mut d, &[SELF | 1], &[(PROP_BRIGHTNESS, 0)], false).unwrap();
        assert_eq!(
            d.writes,
            [
                (PROP_MODE, MODE_SOFTWARE),
                (PROP_BRIGHTNESS, 0),
                (PROP_MODE, MODE_HARDWARE)
            ]
        );
        // A failed store still hands the device back to hardware mode.
        let mut d = fake(PROP_BRIGHTNESS, 5000);
        assert!(store_in_software(&mut d, &[SELF], &[(PROP_BRIGHTNESS, 0)], false).is_err());
        assert_eq!(
            d.writes,
            [(PROP_MODE, MODE_SOFTWARE), (PROP_MODE, MODE_HARDWARE)]
        );
        // Already in software mode (host lighting): no mode switch.
        let mut d = fake(PROP_POLLRATE, 4);
        store_in_software(&mut d, &[SELF], &[(PROP_POLLRATE, 3)], true).unwrap();
        assert_eq!(d.writes, [(PROP_POLLRATE, 3)]);
    }

    #[test]
    fn v2_writes_bracket_receiver_and_headset() {
        let mut d = Fake {
            props: [(PROP_SLEEP_ENABLED, 0), (PROP_SLEEP_TIMEOUT, 0)].into(),
            ..Fake::default()
        };
        let writes = [
            (PROP_SLEEP_ENABLED, 1),
            (PROP_SLEEP_TIMEOUT, 15 * MINUTE_MS),
        ];
        store_in_software(&mut d, &[SELF, SELF | 1], &writes, false).unwrap();
        assert_eq!(
            d.writes,
            [
                (PROP_MODE, MODE_SOFTWARE),
                (PROP_MODE, MODE_SOFTWARE),
                (PROP_SLEEP_ENABLED, 1),
                (PROP_SLEEP_TIMEOUT, 900_000),
                (PROP_MODE, MODE_HARDWARE),
                (PROP_MODE, MODE_HARDWARE)
            ]
        );
        // A timeout that is not whole minutes (or over 90) is never sent, and
        // nor is anything after it.
        let mut d = fake(PROP_SLEEP_TIMEOUT, 0);
        let bad = [(PROP_SLEEP_TIMEOUT, 61_000), (PROP_SIDETONE_OFF, 0)];
        assert!(store_in_software(&mut d, &[SELF], &bad, true).is_err());
        assert!(!storable(PROP_SLEEP_TIMEOUT, 91 * MINUTE_MS));
        assert!(!storable(PROP_SIDETONE_LEVEL, 1005));
        assert!(!storable(PROP_ANC, 3));
        assert!(d.writes.is_empty());
    }

    #[test]
    fn write_data_splits_with_headers() {
        let data: Vec<u8> = (0..130).collect();
        let f = write_data(0x08, 0, &data, 64);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0][..9], [0x08, 0x06, 0x00, 130, 0, 0, 0, 0, 1]);
        assert_eq!(f[0].len(), 64);
        assert_eq!(f[1][..4], [0x08, 0x07, 0x00, 57]);
        assert_eq!(f[1].len(), 64);
        // 57 + 61 = 118 sent, 12 left.
        assert_eq!(f[2][..4], [0x08, 0x07, 0x00, 118]);
        assert_eq!(f[2].len(), 15);
        // Short payload fits the first frame.
        assert_eq!(
            write_data(0x09, 0, &[7, 8], 64),
            vec![vec![0x09, 6, 0, 2, 0, 0, 0, 7, 8]]
        );
        // A tiny packet size cannot panic (descriptors are checked too).
        for pkt_len in 0..8 {
            let f = write_data(0x08, 0, &data, pkt_len);
            assert_eq!(f.iter().map(|f| f.len() - 3).sum::<usize>(), 4 + 130);
        }
    }

    #[test]
    fn lighting_payloads() {
        assert_eq!(
            lighting_payload(Format::Triplets, 2, [1, 2, 3]),
            [0x12, 0x00, 1, 2, 3, 1, 2, 3]
        );
        assert_eq!(
            lighting_payload(Format::Planar, 2, [1, 2, 3]),
            [1, 1, 2, 2, 3, 3]
        );
    }

    #[test]
    fn reply_parsing() {
        // Wired keyboard answering a GET of the mode: hardware (1).
        let r = parse_reply(&[0x08, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00], 0x08, GET);
        assert_eq!(
            r,
            Some(Reply {
                status: 0,
                value: 1
            })
        );
        // Dark Core RGB PRO SE behind its dongle, battery 0x0226 = 550 (HaloBattery capture).
        let r = parse_reply(&[0x01, 0x02, 0x00, 0x26, 0x02, 0x00, 0x00], 0x09, GET);
        assert_eq!(r.map(|r| r.value), Some(550));
        // HS80 receiver reply with its 0x01 prefix stripped: 0x0276 = 630.
        let r = parse_reply(&[0x01, 0x02, 0x00, 0x76, 0x02], 0x09, GET);
        assert_eq!(
            r,
            Some(Reply {
                status: 0,
                value: 630
            })
        );
        // Unsupported property.
        assert_eq!(
            parse_reply(&[0x08, 0x02, 0x05], 0x08, GET).map(|r| r.status),
            Some(5)
        );
        // Another command or another paired device is not ours.
        assert_eq!(parse_reply(&[0x08, 0x01, 0x00], 0x08, GET), None);
        assert_eq!(parse_reply(&[0x02, 0x02, 0x00, 1], 0x09, GET), None);
    }

    #[test]
    fn subdevice_bits_to_addrs() {
        // Bit 0 is the receiver itself; bits 1..=7 are paired slots.
        assert_eq!(
            subdevice_addrs(0b0000_0110).collect::<Vec<_>>(),
            [0x09, 0x0a]
        );
        assert_eq!(subdevice_addrs(0x01).count(), 0);
        assert_eq!(subdevice_addrs(0x1fe).last(), Some(0x0f));
    }

    #[test]
    fn pollrate_and_battery() {
        assert_eq!(pollrate_hz(1), Some(125));
        assert_eq!(pollrate_hz(4), Some(1000));
        assert_eq!(pollrate_hz(7), Some(8000));
        assert_eq!(pollrate_hz(0), None);
        assert_eq!(pollrate_value(125), Some(1));
        assert_eq!(pollrate_value(8000), Some(7));
        assert_eq!(pollrate_value(300), None);
        let b = battery(630, Some(2), "Battery");
        assert_eq!((b.percent, b.charging), (Some(63), false));
        assert!(battery(1000, Some(1), "Battery").charging);
    }

    /// Read-only: mode + battery of the first Bragi device found.
    #[test]
    #[ignore]
    fn hw_read_mode() {
        let node = crate::hid::enumerate_hidraw()
            .unwrap()
            .into_iter()
            .find(|d| {
                d.vendor_id == super::super::VID
                    && hid::report_descriptor(d)
                        .is_some_and(|desc| hid::usage_pages(&desc).contains(&USAGE_PAGE))
            })
            .expect("bragi node");
        let mut link = Link::open(&node, false).unwrap();
        eprintln!("mode {:?}", link.get(SELF, PROP_MODE));
        eprintln!("battery {:?}", link.get(SELF, PROP_BATTERY_LEVEL));
    }
}
