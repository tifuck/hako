//! Compx frames (sources in `mod.rs`).
//!
//! v1: 17-byte output / input report `08 <cmd> <status> <addr hi> <addr lo>
//! <len> <data ×10> <check>`; the whole report, id included, sums to 0x55.
//! EEPROM values are stored as `[value, 0x55 - value]` pairs.
//!
//! v2 ("BITMOUSE", usage page 0xFF05): 64-byte report `08 <sum> 72 <param len>
//! <seq> <target> <cmd> <cmd len> <data..>`, `sum` = bytes 2.. added up;
//! replies are `08 72 <status> <seq> <target> <cmd> <len> <data..>`.

use anyhow::{bail, ensure, Result};
use std::time::{Duration, Instant};

pub const REPORT_ID: u8 = 0x08;
/// What a v1 report and every EEPROM pair add up to.
const SUM: u8 = 0x55;
pub const FRAME: usize = 17;
pub const V2_FRAME: usize = 64;

/// Link type, CID and MID (the vendor apps call it `EncryptionData`; the four
/// request bytes are never checked).
pub const CMD_LINK: u8 = 0x01;
/// Whether the mouse behind a receiver is linked (awake).
pub const CMD_ONLINE: u8 = 0x03;
pub const CMD_BATTERY: u8 = 0x04;
pub const CMD_WRITE: u8 = 0x07;
pub const CMD_READ: u8 = 0x08;
/// Unsolicited: something changed on the mouse.
pub const CMD_STATUS: u8 = 0x0a;
pub const CMD_CID_MID: u8 = 0x10;
/// EEPROM bytes one read / write frame carries.
const CHUNK: usize = 10;

/// The rate row: `[rate, ~, stage count, ~, active stage, ~]`.
pub const ADDR_RATE: u16 = 0x0000;
pub const ADDR_ACTIVE: u16 = 0x0004;
const ROW_LEN: u8 = 6;

/// (Hz, code) of the v1 rate byte.
pub const RATES: [(i64, u8); 7] = [
    (125, 0x08),
    (250, 0x04),
    (500, 0x02),
    (1000, 0x01),
    (2000, 0x10),
    (4000, 0x20),
    (8000, 0x40),
];

const V2_MARK: u8 = 0x72;
/// Byte 2 of a v2 device-initiated frame.
const V2_EVENT: u8 = 0x73;
/// The dongle sees a mouse (target 0), then the mouse's own state (target 1).
pub const V2_DONGLE_LINK: u8 = 0x81;
pub const V2_MOUSE_ONLINE: u8 = 0x23;
pub const V2_BATTERY: u8 = 0x07;
pub const V2_CHARGING: u8 = 0x0f;
/// v2 `target`: the dongle itself / a cabled mouse, or the mouse behind a dongle.
pub const V2_LOCAL: u8 = 0;
pub const V2_REMOTE: u8 = 1;

fn add(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b))
}

fn frame(cmd: u8, addr: u16, len: u8, data: &[u8]) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    f[0] = REPORT_ID;
    f[1] = cmd;
    f[3..5].copy_from_slice(&addr.to_be_bytes());
    f[5] = len;
    f[6..6 + data.len()].copy_from_slice(data);
    f[16] = SUM.wrapping_sub(add(&f[..16]));
    f
}

pub fn read_frame(addr: u16, len: u8) -> [u8; FRAME] {
    debug_assert!((1..=CHUNK as u8).contains(&len));
    frame(CMD_READ, addr, len, &[])
}

pub fn write_frame(addr: u16, data: &[u8]) -> [u8; FRAME] {
    debug_assert!((1..=CHUNK).contains(&data.len()));
    frame(CMD_WRITE, addr, data.len() as u8, data)
}

pub fn battery_frame() -> [u8; FRAME] {
    frame(CMD_BATTERY, 0, 0, &[])
}

pub fn pair(v: u8) -> [u8; 2] {
    [v, SUM.wrapping_sub(v)]
}

fn unpair(b: &[u8]) -> Option<u8> {
    (b[0].wrapping_add(b[1]) == SUM).then_some(b[0])
}

/// The data bytes of a v1 reply to `cmd` (at `addr` for EEPROM commands).
/// `None`: another report. An error: a corrupt or refused reply.
pub fn reply(raw: &[u8], cmd: u8, addr: u16) -> Option<Result<&[u8]>> {
    if raw.len() < FRAME || raw[0] != REPORT_ID || raw[1] != cmd {
        return None;
    }
    let eeprom = cmd == CMD_READ || cmd == CMD_WRITE;
    if eeprom && u16::from_be_bytes([raw[3], raw[4]]) != addr {
        return None;
    }
    Some(if add(&raw[..FRAME]) != SUM {
        Err(anyhow::anyhow!(
            "bad checksum in reply {:02x?}",
            &raw[..FRAME]
        ))
    } else if raw[2] != 0 {
        Err(anyhow::anyhow!(
            "mouse refused command {cmd:#04x} (status {:#04x})",
            raw[2]
        ))
    } else {
        Ok(&raw[6..16])
    })
}

/// (percent, charging) from a battery reply's data: level, state, mV.
pub fn battery(data: &[u8]) -> Option<(u8, bool)> {
    (data[0] <= 100).then_some((data[0], data[1] != 0))
}

/// `[rate code, stage count, active stage (from 0)]`, every pair checked.
pub type Row = [u8; 3];

pub fn parse_row(data: &[u8]) -> Result<Row> {
    let mut row = [0u8; 3];
    for (i, v) in row.iter_mut().enumerate() {
        *v = unpair(&data[2 * i..])
            .ok_or_else(|| anyhow::anyhow!("corrupt rate row {:02x?}", &data[..6]))?;
    }
    let [rate, count, active] = row;
    ensure!(
        RATES.iter().any(|&(_, c)| c == rate),
        "unknown report-rate code {rate:#04x}"
    );
    ensure!(
        (1..=8).contains(&count) && active < count,
        "stage {active} of {count} is out of range"
    );
    Ok(row)
}

pub fn rate_hz(code: u8) -> Option<i64> {
    RATES.iter().find(|&&(_, c)| c == code).map(|&(hz, _)| hz)
}

pub fn v2_frame(cmd: u8, param_len: u8, cmd_len: u8, target: u8) -> [u8; V2_FRAME] {
    let mut f = [0u8; V2_FRAME];
    f[0] = REPORT_ID;
    f[2] = V2_MARK;
    f[3] = param_len;
    f[5] = target;
    f[6] = cmd;
    f[7] = cmd_len;
    f[1] = add(&f[2..]);
    f
}

/// First data byte of a v2 reply to `cmd`.
pub fn v2_reply(raw: &[u8], cmd: u8) -> Option<Result<u8>> {
    if raw.len() < 8 || raw[0] != REPORT_ID || raw[1] != V2_MARK || raw[5] != cmd {
        return None;
    }
    Some(if raw[2] == 0xff {
        Err(anyhow::anyhow!("mouse refused command {cmd:#04x}"))
    } else if raw[6] == 0 {
        Err(anyhow::anyhow!("empty reply to {cmd:#04x}"))
    } else {
        Ok(raw[7])
    })
}

/// Output / input reports, so the exchanges can run against a fake mouse.
pub trait Link {
    fn send(&mut self, report: &[u8]) -> Result<()>;
    /// One input report, or an error once `timeout` passes (any error but
    /// an [`std::io::Error`] counts as that timeout).
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
}

impl Link for crate::hid::HidrawDevice {
    fn send(&mut self, report: &[u8]) -> Result<()> {
        // Stale replies (other apps, late answers) must not match ours.
        self.drain(Duration::ZERO);
        self.write(report)
    }
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.read_timeout(buf, timeout)
    }
}

pub const TIMEOUT: Duration = Duration::from_millis(400);

/// Sends `report`, returns what `accept` makes of the first matching reply,
/// `None` if nothing matched in time. A failed read is an error.
pub fn exchange<T>(
    link: &mut impl Link,
    report: &[u8],
    timeout: Duration,
    mut accept: impl FnMut(&[u8]) -> Option<Result<T>>,
) -> Result<Option<T>> {
    link.send(report)?;
    let end = Instant::now() + timeout;
    let mut buf = [0u8; 128];
    loop {
        let left = end.saturating_duration_since(Instant::now());
        let n = match link.recv(&mut buf, left) {
            Ok(n) => n,
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => return Err(e),
            Err(_) => return Ok(None),
        };
        if let Some(r) = accept(&buf[..n]) {
            return r.map(Some);
        }
        if left.is_zero() {
            return Ok(None);
        }
    }
}

const ASLEEP: &str = "the mouse did not answer (asleep or out of range: move it)";

/// A v1 command answered with data, tried twice (a waking mouse drops one).
fn query(link: &mut impl Link, report: &[u8; FRAME], min_len: u8) -> Result<[u8; 10]> {
    let (cmd, addr) = (report[1], u16::from_be_bytes([report[3], report[4]]));
    for _ in 0..2 {
        let got = exchange(link, report, TIMEOUT, |raw| {
            reply(raw, cmd, addr).map(|r| {
                r.and_then(|d| {
                    ensure!(
                        cmd != CMD_READ || raw[5] >= min_len,
                        "short reply ({} of {min_len} bytes)",
                        raw[5]
                    );
                    Ok(<[u8; 10]>::try_from(d)?)
                })
            })
        })?;
        if let Some(d) = got {
            return Ok(d);
        }
    }
    bail!(ASLEEP)
}

pub fn read_battery(link: &mut impl Link) -> Result<(u8, bool)> {
    let d = query(link, &battery_frame(), 0)?;
    battery(&d).ok_or_else(|| anyhow::anyhow!("battery level {} out of range", d[0]))
}

pub fn read_row(link: &mut impl Link) -> Result<Row> {
    parse_row(&query(link, &read_frame(ADDR_RATE, ROW_LEN), ROW_LEN)?)
}

/// What the mouse's link is: (CID, MID, link type). Types: 0 dongle 1K,
/// 1 dongle 4K, 2 wired 1K, 3 wired 8K, 4 dongle 2K, 5 dongle 8K, 6 wired
/// 2K, 7 wired 4K, 8 NearLink.
pub fn read_link(link: &mut impl Link) -> Result<(u8, u8, u8)> {
    // The mouse ignores the content; vary it like the vendor apps do.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x3c, |d| d.subsec_nanos() as u8);
    let data = [seed, seed ^ 0x5a, seed.wrapping_add(7), 0xa5];
    let d = query(link, &frame(CMD_LINK, 0, 8, &data), 0)?;
    Ok((d[4], d[5], d[6]))
}

/// Highest report rate a link type carries (ATK HUB's lists: wired 2K / 4K
/// links offer nothing above 1000 Hz there).
pub fn link_ceiling(link_type: u8) -> Option<i64> {
    match link_type {
        0 | 2 | 6 | 7 => Some(1000),
        1 => Some(4000),
        4 => Some(2000),
        3 | 5 | 8 => Some(8000),
        _ => None,
    }
}

/// CID and MID of the mouse (behind a receiver: the paired one), with the
/// four-byte unique id some firmwares report.
pub fn read_cid_mid(link: &mut impl Link) -> Result<(u8, u8)> {
    let d = query(link, &frame(CMD_CID_MID, 0, 0, &[]), 0)?;
    Ok((d[0], d[1]))
}

/// Whether a receiver currently has its mouse (awake and linked).
pub fn read_online(link: &mut impl Link) -> Result<bool> {
    let d = query(link, &frame(CMD_ONLINE, 0, 0, &[]), 0)?;
    Ok(d[0] == 1)
}

/// `n` bytes at `addr` (at most 10).
pub fn read_bytes(link: &mut impl Link, addr: u16, n: u8) -> Result<[u8; 10]> {
    query(link, &read_frame(addr, n), n)
}

/// The `[value, ~value]` pair at `addr`; `None` if the stored bytes are not a
/// valid pair.
pub fn read_pair(link: &mut impl Link, addr: u16) -> Result<Option<u8>> {
    Ok(unpair(&read_bytes(link, addr, 2)?))
}

fn send_write(link: &mut impl Link, addr: u16, data: &[u8]) -> Result<()> {
    // Some receivers acknowledge (echo) the write, some do not: the read-back
    // decides.
    exchange(link, &write_frame(addr, data), TIMEOUT, |raw| {
        reply(raw, CMD_WRITE, addr).map(|r| r.map(|_| ()))
    })?;
    Ok(())
}

/// Writes one `[value, ~value]` pair anywhere (debounce, lift-off, ...): read
/// and validate the stored pair, let `check` veto the old value, write only
/// the pair, read back. Returns the value as stored.
pub fn write_pair_at(
    link: &mut impl Link,
    addr: u16,
    value: u8,
    check: impl Fn(u8) -> Result<()>,
) -> Result<u8> {
    let stored = |link: &mut _| -> Result<u8> {
        let d = read_bytes(link, addr, 2)?;
        unpair(&d).ok_or_else(|| anyhow::anyhow!("corrupt stored pair {:02x?}", &d[..2]))
    };
    let old = stored(link).map_err(|e| e.context("not writing: stored value unreadable"))?;
    check(old)?;
    if old == value {
        return Ok(old);
    }
    send_write(link, addr, &pair(value))?;
    let back = stored(link).map_err(|e| e.context("written, but the read-back failed"))?;
    ensure!(
        back == value,
        "the mouse kept {back:#04x} instead of {value:#04x}"
    );
    Ok(back)
}

/// Replaces a 4-byte record (a DPI stage): read it, let `check` veto the
/// stored bytes (checksum / codec), write, read back.
pub fn write_record(
    link: &mut impl Link,
    addr: u16,
    rec: [u8; 4],
    check: impl Fn(&[u8]) -> Result<()>,
) -> Result<()> {
    let d = read_bytes(link, addr, 4)
        .map_err(|e| e.context("not writing: stored record unreadable"))?;
    check(&d[..4])?;
    if d[..4] == rec {
        return Ok(());
    }
    send_write(link, addr, &rec)?;
    let back =
        read_bytes(link, addr, 4).map_err(|e| e.context("written, but the read-back failed"))?;
    ensure!(
        back[..4] == rec,
        "the mouse kept {:02x?} instead of {rec:02x?}",
        &back[..4]
    );
    Ok(())
}

/// What changed, from an unsolicited v1 status report (bit 0 DPI, 1 rate,
/// 2 profile, 6 battery).
pub fn status_flags(raw: &[u8]) -> Option<u8> {
    (raw.len() >= FRAME
        && raw[0] == REPORT_ID
        && raw[1] == CMD_STATUS
        && raw[2] == 0
        && add(&raw[..FRAME]) == SUM)
        .then_some(raw[6])
}

/// A v2 device-initiated frame: `Some(true)` the mouse woke, `Some(false)` it
/// went to sleep.
pub fn v2_wake_event(raw: &[u8]) -> Option<bool> {
    if raw.len() < 4 || raw[0] != REPORT_ID || raw[2] != V2_EVENT {
        return None;
    }
    match raw[3] {
        4 => Some(false),
        5 => Some(true),
        _ => None,
    }
}

/// Writes one `[value, ~value]` pair of the rate row (`addr` 0 rate, 4 active
/// stage): read and check the row, let `check` veto, write only that pair,
/// read back. Returns the row as stored.
pub fn write_pair(
    link: &mut impl Link,
    addr: u16,
    value: u8,
    check: impl Fn(&Row) -> Result<()>,
) -> Result<Row> {
    debug_assert!(addr == ADDR_RATE || addr == ADDR_ACTIVE);
    let field = usize::from(addr / 2);
    let row = read_row(link).map_err(|e| e.context("not writing: stored settings unreadable"))?;
    check(&row)?;
    if row[field] == value {
        return Ok(row);
    }
    send_write(link, addr, &pair(value))?;
    let back = read_row(link).map_err(|e| e.context("written, but the read-back failed"))?;
    ensure!(
        back[field] == value,
        "the mouse kept {:#04x} instead of {value:#04x}",
        back[field]
    );
    Ok(back)
}

/// v2 receivers: (a mouse is linked, it is awake). Wired devices have no
/// receiver to ask.
pub fn read_v2_online(link: &mut impl Link) -> Result<bool> {
    let mut ask = |cmd, target| -> Result<u8> {
        for _ in 0..2 {
            let r = exchange(link, &v2_frame(cmd, 3, 1, target), TIMEOUT, |raw| {
                v2_reply(raw, cmd)
            })?;
            if let Some(v) = r {
                return Ok(v);
            }
        }
        bail!(ASLEEP)
    };
    Ok(ask(V2_DONGLE_LINK, V2_LOCAL)? != 0 && ask(V2_MOUSE_ONLINE, V2_REMOTE)? == 0)
}

pub fn read_v2_battery(link: &mut impl Link, target: u8) -> Result<(u8, bool)> {
    let mut ask = |cmd| -> Result<u8> {
        for _ in 0..2 {
            let r = exchange(link, &v2_frame(cmd, 2, 1, target), TIMEOUT, |raw| {
                v2_reply(raw, cmd)
            })?;
            if let Some(v) = r {
                return Ok(v);
            }
        }
        bail!(ASLEEP)
    };
    let level = ask(V2_BATTERY)?;
    ensure!(level <= 100, "battery level {level} out of range");
    Ok((level, ask(V2_CHARGING)? == 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::collections::VecDeque;

    #[test]
    fn v1_frames_match_sources() {
        // lamzu-ctl build_compx_read_packet(0, 2): 08 08 00 00 00 02 .. 43.
        let f = read_frame(0, 2);
        assert_eq!(f[..6], [0x08, 0x08, 0x00, 0x00, 0x00, 0x02]);
        assert_eq!(f[16], 0x43);
        // atk-control build_write(0, pack_pair(1)) = 1000 Hz: 01 54 data.
        let f = write_frame(ADDR_RATE, &pair(0x01));
        assert_eq!(f[..8], [0x08, 0x07, 0x00, 0x00, 0x00, 0x02, 0x01, 0x54]);
        assert_eq!(f[8..16], [0; 8]);
        assert_eq!(f[16], 0x55u8.wrapping_sub(0x08 + 0x07 + 0x02 + 0x55));
        // Battery query: 08 04 00 .. 49 (0x08 + 0x04 + 0x49 = 0x55).
        let f = battery_frame();
        assert_eq!(f[1..16], [0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(f[16], 0x49);
        for f in [
            read_frame(0x00a9, 10),
            write_frame(4, &pair(2)),
            battery_frame(),
        ] {
            assert_eq!(add(&f), 0x55);
        }
        // Address is big endian (OpenATK, lamzu-cfg).
        assert_eq!(read_frame(0x0102, 1)[3..5], [0x01, 0x02]);
    }

    #[test]
    fn rate_codes_and_pairs_match_sources() {
        assert_eq!(pair(0x20), [0x20, 0x35]);
        assert_eq!(rate_hz(0x01), Some(1000));
        assert_eq!(rate_hz(0x40), Some(8000));
        assert_eq!(rate_hz(0x03), None);
        // 1000 Hz, 4 stages, stage 2 active.
        let row = [0x01, 0x54, 0x04, 0x51, 0x01, 0x54];
        assert_eq!(parse_row(&row).unwrap(), [0x01, 4, 1]);
        let mut bad = row;
        bad[3] = 0x50;
        assert!(parse_row(&bad).is_err());
        let mut bad = row;
        bad[4..6].copy_from_slice(&pair(4));
        assert!(parse_row(&bad).is_err(), "active stage past the count");
        let mut bad = row;
        bad[..2].copy_from_slice(&pair(0x03));
        assert!(parse_row(&bad).is_err(), "unknown rate code");
    }

    fn v1_reply(cmd: u8, status: u8, addr: u16, data: &[u8]) -> Vec<u8> {
        let mut f = frame(cmd, addr, data.len() as u8, data);
        f[2] = status;
        f[16] = 0;
        f[16] = SUM.wrapping_sub(add(&f));
        f.to_vec()
    }

    #[test]
    fn parses_replies() {
        // DPIdeck / atk-control: level, charging, millivolts (BE).
        let r = v1_reply(CMD_BATTERY, 0, 0, &[87, 1, 0x0f, 0xa0]);
        let d = reply(&r, CMD_BATTERY, 0).unwrap().unwrap();
        assert_eq!(battery(d), Some((87, true)));
        assert_eq!(battery(&[101, 0]), None);
        assert!(reply(&r, CMD_READ, 0).is_none());
        let mut bad = r.clone();
        bad[7] ^= 1;
        assert!(reply(&bad, CMD_BATTERY, 0).unwrap().is_err());
        let r = v1_reply(CMD_READ, 1, 0, &[0; 6]);
        assert!(reply(&r, CMD_READ, 0).unwrap().is_err());
        assert!(reply(&r, CMD_READ, 4).is_none(), "other address");
    }

    #[test]
    fn v2_frames_match_sources() {
        // atk-control build_v2_frame(0x07, 2, 1, target=1): sum of the rest.
        let f = v2_frame(V2_BATTERY, 2, 1, V2_REMOTE);
        assert_eq!(f[..8], [0x08, 0x7d, 0x72, 0x02, 0x00, 0x01, 0x07, 0x01]);
        assert!(f[8..].iter().all(|&b| b == 0));
        assert_eq!(
            v2_frame(V2_CHARGING, 2, 1, V2_LOCAL)[1],
            0x72 + 2 + 0x0f + 1
        );
        let mut r = vec![0u8; 64];
        r[..8].copy_from_slice(&[0x08, 0x72, 0x00, 0x00, 0x01, 0x07, 0x01, 91]);
        assert_eq!(v2_reply(&r, V2_BATTERY).unwrap().unwrap(), 91);
        assert!(v2_reply(&r, V2_CHARGING).is_none());
        r[2] = 0xff;
        assert!(v2_reply(&r, V2_BATTERY).unwrap().is_err());
        r[1] = 0x73;
        assert!(
            v2_reply(&r, V2_BATTERY).is_none(),
            "device-initiated report"
        );
    }

    /// A v1 mouse with a 10-byte rate row; `stuck` ignores writes, `ack`
    /// echoes them.
    struct Fake {
        eeprom: [u8; 0x100],
        queue: VecDeque<Vec<u8>>,
        writes: Vec<Vec<u8>>,
        stuck: bool,
        ack: bool,
    }

    impl Fake {
        fn new(row: [u8; 6]) -> Self {
            let mut eeprom = [0u8; 0x100];
            eeprom[..6].copy_from_slice(&row);
            Fake {
                eeprom,
                queue: VecDeque::new(),
                writes: vec![],
                stuck: false,
                ack: true,
            }
        }
    }

    impl Link for Fake {
        fn send(&mut self, r: &[u8]) -> Result<()> {
            assert_eq!(r.len(), FRAME);
            assert_eq!(add(r), SUM, "bad checksum sent: {r:02x?}");
            let addr = usize::from(u16::from_be_bytes([r[3], r[4]]));
            let len = usize::from(r[5]);
            match r[1] {
                CMD_READ => {
                    let d = self.eeprom[addr..addr + len].to_vec();
                    self.queue.push_back(v1_reply(CMD_READ, 0, addr as u16, &d));
                }
                CMD_WRITE => {
                    self.writes.push(r.to_vec());
                    if !self.stuck {
                        self.eeprom[addr..addr + len].copy_from_slice(&r[6..6 + len]);
                    }
                    if self.ack {
                        self.queue.push_back(r.to_vec());
                    }
                }
                c => panic!("unexpected command {c:#04x}"),
            }
            Ok(())
        }
        fn recv(&mut self, buf: &mut [u8], _: Duration) -> Result<usize> {
            let r = self.queue.pop_front().context("timeout")?;
            buf[..r.len()].copy_from_slice(&r);
            Ok(r.len())
        }
    }

    const ROW: [u8; 6] = [0x01, 0x54, 0x04, 0x51, 0x00, 0x55];

    #[test]
    fn write_pair_reads_writes_only_the_pair_and_verifies() {
        let mut m = Fake::new(ROW);
        let row = write_pair(&mut m, ADDR_RATE, 0x20, |_| Ok(())).unwrap();
        assert_eq!(row, [0x20, 4, 0]);
        assert_eq!(m.writes.len(), 1);
        assert_eq!(m.writes[0][3..8], [0x00, 0x00, 0x02, 0x20, 0x35]);
        assert_eq!(m.eeprom[2..6], ROW[2..6], "neighbours untouched");
        // No acknowledgement: the read-back still confirms it.
        m.ack = false;
        let row = write_pair(&mut m, ADDR_ACTIVE, 3, |r| {
            ensure!(3 < r[1], "no such stage");
            Ok(())
        })
        .unwrap();
        assert_eq!(row, [0x20, 4, 3]);
        // Same value: nothing written.
        write_pair(&mut m, ADDR_ACTIVE, 3, |_| Ok(())).unwrap();
        assert_eq!(m.writes.len(), 2);
    }

    #[test]
    fn write_pair_aborts_on_bad_rows_vetoes_and_mismatch() {
        let mut bad = ROW;
        bad[1] = 0x00;
        let mut m = Fake::new(bad);
        assert!(write_pair(&mut m, ADDR_RATE, 0x02, |_| Ok(())).is_err());
        assert!(m.writes.is_empty());
        let mut m = Fake::new(ROW);
        assert!(write_pair(&mut m, ADDR_ACTIVE, 5, |r| {
            ensure!(5 < r[1], "no such stage");
            Ok(())
        })
        .is_err());
        assert!(m.writes.is_empty());
        let mut m = Fake::new(ROW);
        m.stuck = true;
        let e = write_pair(&mut m, ADDR_RATE, 0x02, |_| Ok(())).unwrap_err();
        assert!(e.to_string().contains("kept"), "{e}");
        // Asleep: nothing answers.
        struct Silent;
        impl Link for Silent {
            fn send(&mut self, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn recv(&mut self, _: &mut [u8], _: Duration) -> Result<usize> {
                bail!("timeout")
            }
        }
        let e = read_battery(&mut Silent).unwrap_err();
        assert!(e.to_string().contains("asleep"), "{e}");
        // Unplugged: the read fails, which is not "asleep".
        struct Gone;
        impl Link for Gone {
            fn send(&mut self, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn recv(&mut self, _: &mut [u8], _: Duration) -> Result<usize> {
                Err(std::io::Error::from_raw_os_error(libc::ENODEV).into())
            }
        }
        let e = read_battery(&mut Gone).unwrap_err();
        assert!(!e.to_string().contains("asleep"), "{e}");
        assert!(read_v2_battery(&mut Gone, V2_LOCAL).is_err());
    }

    /// Replies to the non-EEPROM commands, canned.
    struct Canned(Vec<Vec<u8>>, VecDeque<Vec<u8>>);

    impl Link for Canned {
        fn send(&mut self, r: &[u8]) -> Result<()> {
            let v2 = r.len() == V2_FRAME;
            let cmd = if v2 { r[6] } else { r[1] };
            if let Some(a) = self.0.iter().find(|a| a[if v2 { 5 } else { 1 }] == cmd) {
                self.1.push_back(a.clone());
            }
            Ok(())
        }
        fn recv(&mut self, buf: &mut [u8], _: Duration) -> Result<usize> {
            let r = self.1.pop_front().context("timeout")?;
            buf[..r.len()].copy_from_slice(&r);
            Ok(r.len())
        }
    }

    #[test]
    fn link_online_and_cid_mid_replies_are_parsed() {
        // ATK HUB / VGN HUB: cid, mid, type at data[4..7]; online = data[0].
        let link = v1_reply(CMD_LINK, 0, 0, &[1, 2, 3, 4, 2, 77, 5]);
        let mut m = Canned(vec![link], VecDeque::new());
        assert_eq!(read_link(&mut m).unwrap(), (2, 77, 5));
        let mut m = Canned(
            vec![v1_reply(CMD_CID_MID, 0, 0, &[1, 121, 0, 0])],
            VecDeque::new(),
        );
        assert_eq!(read_cid_mid(&mut m).unwrap(), (1, 121));
        let mut up = Canned(
            vec![v1_reply(CMD_ONLINE, 0, 0, &[1, 9, 8, 7])],
            VecDeque::new(),
        );
        assert!(read_online(&mut up).unwrap());
        let mut down = Canned(
            vec![v1_reply(CMD_ONLINE, 0, 0, &[0, 0, 0, 0])],
            VecDeque::new(),
        );
        assert!(!read_online(&mut down).unwrap());
        // A refusing mouse (status 1) is an error, not "offline".
        let mut no = Canned(vec![v1_reply(CMD_ONLINE, 1, 0, &[])], VecDeque::new());
        assert!(read_online(&mut no).is_err());
        assert_eq!(link_ceiling(5), Some(8000));
        assert_eq!(link_ceiling(1), Some(4000));
        assert_eq!(link_ceiling(0), Some(1000));
        assert_eq!(link_ceiling(9), None);
    }

    #[test]
    fn v2_online_needs_a_linked_and_awake_mouse() {
        let reply = |cmd: u8, v: u8| {
            let mut r = vec![0u8; 64];
            r[..8].copy_from_slice(&[0x08, 0x72, 0, 0, 0, cmd, 1, v]);
            r
        };
        let mut m = Canned(
            vec![reply(V2_DONGLE_LINK, 1), reply(V2_MOUSE_ONLINE, 0)],
            VecDeque::new(),
        );
        assert!(read_v2_online(&mut m).unwrap());
        let mut m = Canned(
            vec![reply(V2_DONGLE_LINK, 1), reply(V2_MOUSE_ONLINE, 1)],
            VecDeque::new(),
        );
        assert!(!read_v2_online(&mut m).unwrap(), "linked but asleep");
        let mut m = Canned(vec![reply(V2_DONGLE_LINK, 0)], VecDeque::new());
        assert!(!read_v2_online(&mut m).unwrap(), "no mouse");
    }

    #[test]
    fn unsolicited_reports_are_recognised() {
        let st = v1_reply(CMD_STATUS, 0, 0, &[0x41]);
        assert_eq!(status_flags(&st), Some(0x41));
        let mut bad = st.clone();
        bad[16] ^= 1;
        assert_eq!(status_flags(&bad), None);
        assert_eq!(status_flags(&v1_reply(CMD_BATTERY, 0, 0, &[1])), None);
        let ev = |t: u8| {
            let mut r = vec![0u8; 64];
            r[..4].copy_from_slice(&[0x08, 0x00, 0x73, t]);
            r
        };
        assert_eq!(v2_wake_event(&ev(4)), Some(false));
        assert_eq!(v2_wake_event(&ev(5)), Some(true));
        assert_eq!(v2_wake_event(&ev(1)), None);
        // A normal reply is not an event.
        assert_eq!(v2_wake_event(&[0x08, 0x72, 0, 0, 0, 7, 1, 90]), None);
    }

    #[test]
    fn write_pair_at_validates_writes_one_pair_and_verifies() {
        let mut m = Fake::new(ROW);
        m.eeprom[0xa9..0xab].copy_from_slice(&pair(4));
        m.eeprom[0xab..0xad].copy_from_slice(&pair(1));
        assert_eq!(write_pair_at(&mut m, 0xa9, 8, |_| Ok(())).unwrap(), 8);
        assert_eq!(m.writes.len(), 1);
        assert_eq!(m.writes[0][3..9], [0x00, 0xa9, 0x02, 0x08, 0x4d, 0x00]);
        assert_eq!(
            m.eeprom[0xa9..0xad],
            [8, 0x4d, 1, 0x54],
            "neighbour untouched"
        );
        // Same value: nothing written. A veto and a corrupt pair stop before it.
        write_pair_at(&mut m, 0xa9, 8, |_| Ok(())).unwrap();
        assert_eq!(m.writes.len(), 1);
        assert!(write_pair_at(&mut m, 0xa9, 2, |_| bail!("no")).is_err());
        m.eeprom[0xab] = 9;
        assert!(write_pair_at(&mut m, 0xab, 0, |_| Ok(())).is_err());
        assert_eq!(m.writes.len(), 1);
        let mut m = Fake::new(ROW);
        m.eeprom[0xa9..0xab].copy_from_slice(&pair(4));
        m.stuck = true;
        let e = write_pair_at(&mut m, 0xa9, 8, |_| Ok(())).unwrap_err();
        assert!(e.to_string().contains("kept"), "{e}");
    }

    #[test]
    fn write_record_checks_the_stored_bytes_and_reads_back() {
        let mut m = Fake::new(ROW);
        let old = [15, 15, 0, 0x55 - 30];
        m.eeprom[0x0c..0x10].copy_from_slice(&old);
        let new = [31, 31, 0, 0x55 - 62];
        write_record(&mut m, 0x0c, new, |d| {
            ensure!(d == old, "unexpected");
            Ok(())
        })
        .unwrap();
        assert_eq!(m.eeprom[0x0c..0x10], new);
        assert_eq!(m.writes.len(), 1);
        assert_eq!(m.writes[0][5], 4, "four data bytes");
        // A vetoed record is not written.
        assert!(write_record(&mut m, 0x0c, old, |_| bail!("not recognised")).is_err());
        assert_eq!(m.writes.len(), 1);
        m.stuck = true;
        let e = write_record(&mut m, 0x0c, old, |_| Ok(())).unwrap_err();
        assert!(e.to_string().contains("kept"), "{e}");
    }
}
