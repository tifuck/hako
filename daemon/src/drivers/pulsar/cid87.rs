//! Pulsar Nordic family with `cid` 0x57 (CrazyLight, X2 / X2H / X2A / X2N,
//! Xlite, X3, X3 LHD, signature editions, Feinmann F01, ...): the onboard
//! settings, as Pulsar's own web configurator (Bibimbap `/cMouse`, bundle
//! `app.3c6e7884.js`, cfg 1.3.14) reads and writes them. Facts and evidence:
//! `docs/vendor-re/pulsar.md` (Q4a); the 10-byte memory chunks and the
//! `(v, 0x55 - v)` pair encoding also agree with python-pulsar-mouse-tool and
//! rustar for the older PIDs.
//!
//! Memory is the active onboard profile, addressed with `07` (write) and `08`
//! (read) in 17-byte report 8 frames: `[3..5)` big-endian address, `[5]`
//! length (at most 10), data from `[6]`. The mouse answers with the same
//! command, status `[2] = 0` and the address echoed; the web app waits up to
//! 200 ms per try, tries five times 10 ms apart. A wireless mouse has to be
//! online (`03`) and is held with `03` `[6] = 1` while the host talks to it.
//!
//! Every write here is a flash write (`WriteFlashData`), so each control is
//! `.persistent()`. A write reads the current bytes, checks the pair / record
//! checksum, writes only if the value differs and reads it back to compare.
//! Only encodings the vendor code leaves no doubt about are offered: report
//! rate, active and per-stage DPI (sensors with the 10-DPI table), click
//! debounce and angle snapping. Lift-off, lighting, sleep (units not in the
//! files obtained) and the other sensors' DPI tables stay blocked.

use super::{nordic_packet, nordic_reply, NORDIC_REPORT};
use anyhow::{bail, ensure, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::merge_setting;
use std::time::Duration;
use tracing::warn;

/// Identity reply `cid` of this family (`Config.ini` `CID=87`).
pub const CID: u8 = 0x57;

const CMD_ONLINE: u8 = 0x03;
const CMD_WRITE: u8 = 0x07;
const CMD_READ: u8 = 0x08;
const CHUNK: usize = 10;
const ATTEMPTS: usize = 5;
const TRY_GAP: Duration = Duration::from_millis(10);
const REPLY_WAIT: Duration = Duration::from_millis(200);
const BUSY_ROUNDS: usize = 20;

const ADDR_RATE: u16 = 0x00;
const ADDR_COUNT: u16 = 0x02;
const ADDR_ACTIVE: u16 = 0x04;
const ADDR_DPI: u16 = 0x0c;
const ADDR_DEBOUNCE: u16 = 0xa9;
const ADDR_ANGLE: u16 = 0xaf;
const STAGES: usize = 5;

/// Sensor families of the vendor's `sensor.json`; only the table with 10-DPI
/// steps ("pulsar x1") is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensor {
    X1,
    S3950,
    S3955,
    Unknown,
}

/// Report-rate ceiling from the identity `type` byte (cable: 2 = 1 kHz,
/// 3 = 8 kHz; wireless: 0 = 1, 1 = 4, 4 = 2, 5 = 8 kHz). An undocumented
/// type gets the lowest.
pub fn type_max_rate(t: u8) -> i64 {
    match t {
        1 => 4000,
        4 => 2000,
        3 | 5 => 8000,
        _ => 1000,
    }
}

/// Only the documented cable types count as wired: an undocumented one is
/// held like a wireless mouse (the hold is the same `03` with `[6] = 1`).
pub fn type_wireless(t: u8) -> bool {
    !matches!(t, 2 | 3)
}

/// `mid` -> `"Pulsar <name>"` for `cid` 0x57.
pub fn mid_name(mid: u8) -> Option<String> {
    let n = NAMES
        .get(usize::from(mid))
        .copied()
        .filter(|n| !n.is_empty())?;
    Some(if n.starts_with("Pulsar") {
        n.to_string()
    } else {
        format!("Pulsar {n}")
    })
}

/// One request / answer exchange on report 8, so the protocol can run against
/// a fake.
pub trait Link {
    fn exchange(&mut self, req: &[u8; 17]) -> Result<Option<[u8; 17]>>;
}

impl Link for crate::hid::HidrawDevice {
    fn exchange(&mut self, req: &[u8; 17]) -> Result<Option<[u8; 17]>> {
        self.drain(Duration::ZERO);
        self.write(req)?;
        let start = std::time::Instant::now();
        let mut buf = [0u8; 64];
        while let Some(left) = REPLY_WAIT.checked_sub(start.elapsed()) {
            let n = match self.read_timeout(&mut buf, left) {
                Ok(n) => n,
                Err(e) if e.downcast_ref::<std::io::Error>().is_some() => return Err(e),
                Err(_) => break,
            };
            if nordic_reply(&buf[..n], req[1]) {
                return Ok(buf[..17].try_into().ok());
            }
        }
        Ok(None)
    }
}

fn pair(v: u8) -> [u8; 2] {
    [v, 0x55u8.wrapping_sub(v)]
}

fn unpair(b: &[u8]) -> Option<u8> {
    (b.len() >= 2 && b[0].wrapping_add(b[1]) == 0x55).then_some(b[0])
}

fn mem_request(cmd: u8, addr: u16, data: &[u8], len: usize) -> [u8; 17] {
    let mut body = [0u8; 14];
    body[1..3].copy_from_slice(&addr.to_be_bytes());
    body[3] = len as u8;
    body[4..4 + data.len()].copy_from_slice(data);
    nordic_packet(cmd, body)
}

/// Sends `req` until a reply with status 0 and the right echo arrives
/// (`echo` bytes of `[1..]` are compared).
fn acked(link: &mut impl Link, req: &[u8; 17], echo: usize) -> Result<[u8; 17]> {
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(TRY_GAP);
        }
        if let Some(r) = link.exchange(req)? {
            if r[2] == 0 && r[1..=echo] == req[1..=echo] {
                return Ok(r);
            }
        }
    }
    bail!(
        "the mouse did not acknowledge command {:#04x} at {:#06x}",
        req[1],
        u16::from_be_bytes([req[3], req[4]])
    )
}

pub fn read_mem(link: &mut impl Link, addr: u16, len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let n = (len - out.len()).min(CHUNK);
        let a = addr + out.len() as u16;
        let r = acked(link, &mem_request(CMD_READ, a, &[], n), 5)?;
        out.extend_from_slice(&r[6..6 + n]);
    }
    Ok(out)
}

fn write_mem(link: &mut impl Link, addr: u16, data: &[u8]) -> Result<()> {
    for (i, chunk) in data.chunks(CHUNK).enumerate() {
        let a = addr + (i * CHUNK) as u16;
        acked(link, &mem_request(CMD_WRITE, a, chunk, chunk.len()), 3)?;
    }
    Ok(())
}

/// `03` online query (`hold` sets / clears the host hold); `true` when the
/// mouse is online. A "busy" reply (`[10] = 1`) is asked again.
fn online(link: &mut impl Link, hold: Option<bool>) -> Result<bool> {
    let mut body = [0u8; 14];
    body[4] = u8::from(hold == Some(true));
    let req = nordic_packet(CMD_ONLINE, body);
    for _ in 0..BUSY_ROUNDS {
        let Some(r) = link.exchange(&req)? else {
            return Ok(false);
        };
        if r[10] != 1 {
            return Ok(r[6] == 1);
        }
        std::thread::sleep(TRY_GAP);
    }
    Ok(false)
}

/// Runs `f` with the mouse known to be online, holding a wireless link.
pub fn with_mouse<L: Link, T>(
    link: &mut L,
    wireless: bool,
    f: impl FnOnce(&mut L) -> Result<T>,
) -> Result<T> {
    ensure!(
        online(link, wireless.then_some(true))?,
        "the mouse is offline (asleep or out of range)"
    );
    let r = f(link);
    if wireless {
        if let Err(e) = online(link, Some(false)) {
            warn!("could not release the mouse's host hold: {e:#}");
        }
    }
    r
}

/// Report-rate byte: `1000 / hz` up to 1 kHz, `hz / 2000 * 16` above.
fn rate_code(hz: i64) -> Option<u8> {
    match hz {
        125 | 250 | 500 | 1000 => Some((1000 / hz) as u8),
        2000 | 4000 | 8000 => Some((hz / 2000 * 16) as u8),
        _ => None,
    }
}

fn rate_of(code: u8) -> Option<i64> {
    [125, 250, 500, 1000, 2000, 4000, 8000]
        .into_iter()
        .find(|&hz| rate_code(hz) == Some(code))
}

/// DPI record of the 10-DPI table, X = Y: `[lo, lo, hi << 2 | hi << 6, chk]`
/// (`hi` = value bits 8-9, value = `dpi / 10 - 1`).
fn dpi_record(dpi: i64) -> Result<[u8; 4]> {
    ensure!(
        (10..=10000).contains(&dpi) && dpi % 10 == 0,
        "DPI must be 10..=10000 in steps of 10"
    );
    let v = (dpi / 10 - 1) as u16;
    let hi = (v >> 8) as u8;
    let mut r = [v as u8, v as u8, hi << 2 | hi << 6, 0];
    r[3] = 0x55u8.wrapping_sub(r[0].wrapping_add(r[1]).wrapping_add(r[2]));
    Ok(r)
}

/// X DPI of a record, including the two ranges above 10000 DPI; `None` when
/// the checksum is wrong.
fn dpi_of(r: &[u8]) -> Option<i64> {
    if r.len() < 4
        || r[0]
            .wrapping_add(r[1])
            .wrapping_add(r[2])
            .wrapping_add(r[3])
            != 0x55
    {
        return None;
    }
    let v = i64::from(r[0]) | i64::from((r[2] >> 2) & 3) << 8;
    let base = (v + 1) * 10;
    Some(match r[2] & 3 {
        0 => base,
        2 => (v + 1) * 50 + 10000,
        3 => ((v + 1) * 50 + 10000) * 2,
        _ => return None,
    })
}

/// What the family offers for one mouse.
#[derive(Debug, Clone, Copy)]
pub struct Cid87 {
    pub sensor: Sensor,
    pub max_rate: i64,
    pub wireless: bool,
}

impl Cid87 {
    pub fn new(mid: u8, kind: u8) -> Self {
        Self {
            sensor: sensor_of(mid),
            max_rate: type_max_rate(kind),
            wireless: type_wireless(kind),
        }
    }

    fn dpi_writable(&self) -> bool {
        self.sensor == Sensor::X1
    }

    /// Reads the settings that are shown (the pair / record checksum must
    /// hold; anything else is left out).
    pub fn read_settings(&self, link: &mut impl Link) -> Result<SettingsMap> {
        let mut s = SettingsMap::new();
        let head = read_mem(link, ADDR_RATE, 6)?;
        let rate = unpair(&head[0..2]).and_then(rate_of);
        if let Some(hz) = rate {
            merge_setting(&mut s, SettingKey::ReportRate, SettingValue::Int(hz));
        }
        let count = unpair(&head[2..4]).map(usize::from);
        let active = unpair(&head[4..6]).map(usize::from);
        if let (Some(c), Some(a)) = (count, active) {
            if a < c {
                merge_setting(
                    &mut s,
                    SettingKey::DpiStageActive,
                    SettingValue::Int(a as i64 + 1),
                );
            }
        }
        if self.dpi_writable() {
            let table = read_mem(link, ADDR_DPI, 4 * STAGES)?;
            let keys = stage_keys();
            for (i, key) in keys.into_iter().enumerate() {
                if count.is_some_and(|c| i < c) {
                    if let Some(dpi) = dpi_of(&table[4 * i..4 * i + 4]) {
                        merge_setting(&mut s, key, SettingValue::Int(dpi));
                    }
                }
            }
        }
        let tail = read_mem(link, ADDR_DEBOUNCE, 8)?;
        if let Some(ms) = unpair(&tail[0..2]) {
            merge_setting(&mut s, SettingKey::Debounce, SettingValue::Int(ms.into()));
        }
        if let Some(a) = unpair(&tail[6..8]) {
            merge_setting(
                &mut s,
                SettingKey::AngleSnapping,
                SettingValue::Bool(a == 1),
            );
        }
        Ok(s)
    }

    /// Whether `set` writes `key`.
    pub fn writes(&self, key: SettingKey) -> bool {
        use SettingKey::*;
        match key {
            ReportRate | DpiStageActive | Debounce | AngleSnapping => true,
            DpiStage1 | DpiStage2 | DpiStage3 | DpiStage4 | DpiStage5 => self.dpi_writable(),
            _ => false,
        }
    }

    /// Read, check, write, read back: one setting.
    pub fn write(&self, link: &mut impl Link, key: SettingKey, value: &SettingValue) -> Result<()> {
        use SettingKey::*;
        ensure!(self.writes(key), "{} is not written", key.as_str());
        let int = || value.as_int().context("expected a number");
        match key {
            ReportRate => {
                let hz = int()?;
                ensure!(
                    hz <= self.max_rate,
                    "this mouse tops out at {} Hz",
                    self.max_rate
                );
                let code = rate_code(hz).context("unsupported report rate")?;
                write_checked(link, ADDR_RATE, &pair(code))
            }
            DpiStageActive => {
                let n = int()?;
                let count = read_pair(link, ADDR_COUNT)?;
                ensure!(
                    (1..=i64::from(count)).contains(&n),
                    "the mouse has {count} DPI stages"
                );
                write_checked(link, ADDR_ACTIVE, &pair(n as u8 - 1))
            }
            Debounce => {
                let ms = int()?;
                ensure!((2..=15).contains(&ms), "debounce must be 2..=15 ms");
                write_checked(link, ADDR_DEBOUNCE, &pair(ms as u8))
            }
            AngleSnapping => {
                let on = value.as_bool().context("expected on or off")?;
                write_checked(link, ADDR_ANGLE, &pair(u8::from(on)))
            }
            _ => {
                let i = stage_keys()
                    .iter()
                    .position(|&k| k == key)
                    .context("not a DPI stage")?;
                let count = read_pair(link, ADDR_COUNT)?;
                ensure!(
                    i < usize::from(count),
                    "stage {} is not in use (the mouse has {count} stages)",
                    i + 1
                );
                write_checked(link, ADDR_DPI + 4 * i as u16, &dpi_record(int()?)?)
            }
        }
    }

    /// [`Self::write`], then [`Self::read_settings`] into `state`. When only
    /// the re-read fails, `state` gets the written value and `stale` is set.
    pub fn write_and_read(
        &self,
        link: &mut impl Link,
        key: SettingKey,
        value: &SettingValue,
        state: &mut SettingsMap,
        stale: &mut bool,
    ) -> Result<()> {
        self.write(link, key, value)?;
        match self.read_settings(link) {
            Ok(s) => {
                *state = s;
                Ok(())
            }
            Err(e) => {
                merge_setting(state, key, value.clone());
                *stale = true;
                Err(e.context("written, but the settings were not read back"))
            }
        }
    }

    /// The controls: the writable ones unblocked, the rest placeholders.
    pub fn controls(&self, mut base: Vec<SettingMeta>, blocked: &str) -> Vec<SettingMeta> {
        use SettingKey::*;
        for m in &mut base {
            if !self.writes(m.key) {
                m.blocked = Some(
                    match m.key {
                        PmShutdown => "the units of the sleep value are not in the vendor files obtained",
                        LiftOffDistance | Effect | Brightness => blocked,
                        _ if stage_keys().contains(&m.key) => {
                            "this sensor's DPI table is not verified (only the 10-DPI table is written)"
                        }
                        _ => blocked,
                    }
                    .into(),
                );
                continue;
            }
            m.blocked = None;
            match m.key {
                ReportRate => {
                    m.options = [125, 250, 500, 1000, 2000, 4000, 8000]
                        .into_iter()
                        .filter(|&r| r <= self.max_rate)
                        .map(|r| SettingOption::int(r, &format!("{r} Hz")))
                        .collect();
                }
                Debounce => {
                    *m = SettingMeta::slider(Debounce, "Click debounce", 2.0, 15.0, 1.0)
                        .unit("ms")
                        .persistent()
                }
                k if stage_keys().contains(&k) => {
                    let label = m.label.clone();
                    *m = SettingMeta::slider(k, &label, 10.0, 10000.0, 10.0)
                        .unit("dpi")
                        .persistent();
                }
                _ => {}
            }
        }
        base
    }
}

fn stage_keys() -> [SettingKey; STAGES] {
    [
        SettingKey::DpiStage1,
        SettingKey::DpiStage2,
        SettingKey::DpiStage3,
        SettingKey::DpiStage4,
        SettingKey::DpiStage5,
    ]
}

fn read_pair(link: &mut impl Link, addr: u16) -> Result<u8> {
    let b = read_mem(link, addr, 2)?;
    unpair(&b)
        .with_context(|| format!("not writing: the bytes at {addr:#04x} are not a valid pair"))
}

/// Validates the current bytes (pair or DPI record checksum), writes `new`
/// when it differs and reads it back.
fn write_checked(link: &mut impl Link, addr: u16, new: &[u8]) -> Result<()> {
    let old = read_mem(link, addr, new.len())?;
    let valid = if new.len() == 2 {
        unpair(&old).is_some()
    } else {
        dpi_of(&old).is_some()
    };
    ensure!(
        valid,
        "not writing: the bytes at {addr:#04x} fail their checksum ({old:02x?})"
    );
    if old == new {
        return Ok(());
    }
    write_mem(link, addr, new)?;
    let back = read_mem(link, addr, new.len())?;
    ensure!(
        back == new,
        "read-back at {addr:#04x} is {back:02x?}, not {new:02x?}; check the mouse in Pulsar's configurator"
    );
    Ok(())
}

/// A pushed `0a` StatusChanged report: (flags A, flags B). A set bit means the
/// mouse changed a setting itself (DPI button, profile switch, ...).
pub fn parse_status_changed(r: &[u8]) -> Option<(u8, u8)> {
    (r.len() == 17 && nordic_reply(r, 0x0a) && r[0] == NORDIC_REPORT).then(|| (r[6], r[7]))
}

/// Marketing names by `mid` (`devicename.json`, `cid` 87); `""` where the vendor
/// itself only says "Pulsar device".
const NAMES: [&str; 160] = [
    "X2 Crazylight Mini",
    "X2 Crazylight Mini",
    "X2 Crazylight Mini",
    "X2 Crazylight Mini",
    "X2 Crazylight Mini",
    "X2 Crazylight Mini",
    "TenZ Signature Edition",
    "TenZ",
    "X2 Crazylight Mini",
    "X2 Crazylight Mini",
    "Pulsar X VSPO! Aizawa Ema",
    "X2 Crazylight Mini T1 Edition",
    "X2 Crazylight Mini T1 Edition",
    "X2 Crazylight Mini PRX Edition",
    "X2 Crazylight Mini Boardzy Edition",
    "X2 Crazylight Mini Randomfrankp Edition",
    "Xlite Crazylight Mini",
    "Xlite Crazylight Mini",
    "X3 Crazylight Mini",
    "X3 Crazylight Mini",
    "X3 LHD Crazylight Mini",
    "X3 LHD Crazylight Mini",
    "X2H Crazylight Mini",
    "X2H Crazylight Mini",
    "X2N Crazylight Mini",
    "X2N Crazylight Mini",
    "X2 Crazylight Medium",
    "X2 Mini Pulsar By You XS-1",
    "X2H Crazylight Medium",
    "X2H Medium Pulsar By You XS-1",
    "X2A Medium Pulsar By You XS-1",
    "X2A Mini Pulsar By You XS-1",
    "Xlite Mini Pulsar By You XS-1",
    "Xlite Crazylight Medium",
    "Xlite Large Pulsar By You XS-1",
    "Pulsar Lab. X2F",
    "TenZ Signature RED",
    "JV-X Pro Series",
    "Susanto-X Pro Series",
    "TenZ Signature Edition",
    "X2 Crazylight 5th Anniversary Edition",
    "X2 Crazylight WildScape Desert Edition",
    "X2 Crazylight Mini Forest",
    "X2 Crazylight Mini WildScape Ocean Edition",
    "Pulsar X VSPO! Asumi Sena",
    "Pulsar X VSPO! Hanabusa Lisa",
    "Pulsar X VSPO! Ichinose Uruha",
    "Pulsar X VSPO! Kaga Sumire",
    "Pulsar X VSPO! Kisaragi Ren",
    "Pulsar X VSPO! Kogara Toto",
    "Pulsar X VSPO! Komori Met",
    "Pulsar X VSPO! Kurumi Noah",
    "Pulsar X VSPO! Kaga Nazuna",
    "Pulsar X VSPO! Nekota Tsuna",
    "Pulsar X VSPO! Kaminari Qpi",
    "Pulsar X VSPO! Sendo Yuuhi",
    "Pulsar X VSPO! Shinomiya Runa",
    "Pulsar X VSPO! Shiranami Ramune",
    "Pulsar X VSPO! Tachibana Hinano",
    "Pulsar X VSPO! Tosaki Mimi",
    "Pulsar X VSPO! Tsumugi Kokage",
    "Pulsar X VSPO! Yakumo Beni",
    "Pulsar X VSPO! Yano Kuromu",
    "Pulsar X VSPO! Yumeno Akari",
    "Pulsar X VSPO! Amayui Moka",
    "Pulsar X VSPO! Choya Hanabi",
    "Pulsar Lab. X2F 5th Anniversary Edition",
    "ZywOo The Chosen Mouse Gen.2",
    "ZywOo The Chosen Mouse Gen.2",
    "X2 Crazylight Medium",
    "Xlite Crazylight Medium",
    "X3 Crazylight Medium",
    "X3 Crazylight Medium",
    "X3 LHD Crazylight Medium",
    "X3 LHD Crazylight Medium",
    "X2H Crazylight Medium",
    "X2N Crazylight Medium",
    "X2N Crazylight Medium",
    "X2 Crazylight Medium The Finals Edition",
    "X2 Crazylight Medium THE GRAND MAJOR 2025 Edition",
    "X2 Crazylight Medium Bruce Lee 85th Edition",
    "X2 Crazylight Mini Bruce Lee 85th Edition",
    "X2 Crazylight Medium PRX Pacific Gold Edition",
    "X2 Crazylight Medium Pokemon Pikachu Edition",
    "X2 Crazylight Mini Pokemon Pikachu Edition",
    "X2 Crazylight Mini Pokemon Mew-Two FF Edition",
    "X2 Crazylight Medium BlueArchive Hoshino Edition",
    "X2 Crazylight Medium BlueArchive Shiroko Edition",
    "X2 Crazylight Medium BlueArchive Nonomi Edition",
    "X2 Crazylight Medium BlueArchive Hoshino FF Edition",
    "Pulsar eS FS-1 Medium. Wireless Gaming mouse",
    "BUZZ-X Pro Series - MOON GRAY",
    "STA-X Pro Series - STEALTH BLACK",
    "CARPE-X Pro Series - CARP ORANGE",
    "X2 Crazylight Mini Pokemon Mew-Two FF Edition",
    "X2 Crazylight Mini PRX Pacific Gold Edition",
    "X2H Crazylight Mini BadSeedTech Edition",
    "X2H Crazylight Medium BadSeedTech Edition",
    "Jinggg-X Pro Series",
    "ZywOo The Chosen Mouse Gen.2",
    "Feinmann F01 Noctua Edition",
    "X3 Crazylight Desert",
    "X3 Crazylight Medium Desert",
    "X3 LHD Crazylight Medium Desert",
    "X2N Crazylight Mini Ocean",
    "X2N Crazylight Medium Ocean",
    "TenZ 2.0 Mini Misty Blue",
    "",
    "X2 Crazylight Medium Forest",
    "TenZ Signature Edition 2.0 Mini",
    "",
    "Susanto-X 2.0 Large Black",
    "",
    "",
    "",
    "Xlite Crazylight Rock",
    "X2H Crazylight Volcano",
    "Xlite Crazylight Medium Rock",
    "X2H Crazylight Medium Volcano",
    "Susanto-X 2.0 Large Phantom Jungle",
    "",
    "TenZ Signature Edition 2.0 Medium",
    "TenZ 2.0 Medium Misty Blue",
    "Susanto-X 2.0 Medium Phantom Jungle",
    "Susanto-X 2.0 Medium Black",
    "X2F Crazylight Black",
    "Pulsar eS FS-1 GT Silver",
    "Pulsar eS FS-1 Tournament Red",
    "Pulsar eS FS-1 Tournament Blue",
    "X2 Crazylight Mini Shibuki",
    "X2 Crazylight Mini Yuni",
    "X2 Crazylight Mini Mashiro",
    "ZywOo The Chosen Mouse Gen.3 Mini",
    "ZywOo The Chosen Mouse Gen.3 Mini",
    "ZywOo The Chosen Mouse Gen.3 Medium",
    "ZywOo The Chosen Mouse Gen.3 Medium",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "X2 Crazylight 2 Mini 2026 PRX Edition",
    "X2 Crazylight 2 Medium 2026 PRX Edition",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
];

/// Sensor by `mid` (`cfg.json` groups).
fn sensor_of(mid: u8) -> Sensor {
    match mid {
        1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 | 19
        | 20 | 21 | 22 | 23 | 24 | 25 | 26 | 27 | 28 | 29 | 30 | 31 | 32 | 33 | 34 | 35 | 36
        | 37 | 38 | 39 | 40 | 41 | 42 | 43 | 44 | 45 | 46 | 47 | 48 | 49 | 50 | 51 | 52 | 53
        | 54 | 55 | 56 | 57 | 58 | 59 | 60 | 61 | 62 | 63 | 64 | 65 | 66 | 67 | 70 | 71 | 72
        | 73 | 74 | 75 | 76 | 77 | 78 | 79 | 80 | 81 | 82 | 83 | 84 | 85 | 86 | 87 | 88 | 89
        | 90 | 95 | 96 | 97 | 98 | 102 | 103 | 104 | 105 | 106 | 109 | 116 | 117 | 118 | 119
        | 130 | 131 | 132 => Sensor::X1,
        68 | 69 | 91 | 92 | 93 | 94 | 99 | 100 | 127 | 128 | 129 => Sensor::S3950,
        101 | 107 | 108 | 110 | 111 | 112 | 113 | 114 | 115 | 120 | 121 | 122 | 123 | 124 | 125
        | 126 | 133 | 134 | 135 | 136 | 137 | 138 | 139 | 140 | 141 | 142 | 143 | 144 | 145
        | 146 | 147 | 148 | 149 | 150 | 151 | 152 | 153 | 154 | 155 | 156 | 157 | 158 | 159
        | 160 => Sensor::S3955,
        _ => Sensor::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A mouse's memory behind the report-8 protocol.
    struct Fake {
        mem: [u8; 0x100],
        online: bool,
        /// Ignore writes (a mouse that refuses to store them).
        stuck: bool,
        writes: Vec<(u16, Vec<u8>)>,
        holds: Cell<u32>,
        /// Reads answered before the mouse goes silent (`None`: all).
        reads_left: Option<usize>,
    }

    impl Fake {
        fn new() -> Self {
            let mut mem = [0u8; 0x100];
            mem[0..2].copy_from_slice(&pair(1)); // 1000 Hz
            mem[2..4].copy_from_slice(&pair(4)); // 4 stages
            mem[4..6].copy_from_slice(&pair(1)); // stage 2 active
            for (i, dpi) in [400, 800, 1600, 3200, 6400].into_iter().enumerate() {
                mem[0x0c + 4 * i..0x10 + 4 * i].copy_from_slice(&dpi_record(dpi).unwrap());
            }
            mem[0xa9..0xab].copy_from_slice(&pair(5));
            mem[0xaf..0xb1].copy_from_slice(&pair(0));
            Self {
                mem,
                online: true,
                stuck: false,
                writes: vec![],
                holds: Cell::new(0),
                reads_left: None,
            }
        }
    }

    impl Link for Fake {
        fn exchange(&mut self, req: &[u8; 17]) -> Result<Option<[u8; 17]>> {
            assert_eq!(req[0], NORDIC_REPORT);
            assert!(nordic_reply(req, req[1]), "bad checksum in request");
            let addr = usize::from(u16::from_be_bytes([req[3], req[4]]));
            let len = usize::from(req[5]);
            let mut body = [0u8; 14];
            body[1..4].copy_from_slice(&req[3..6]);
            match req[1] {
                CMD_ONLINE => {
                    if req[6] == 1 {
                        self.holds.set(self.holds.get() + 1);
                    }
                    body[4] = u8::from(self.online);
                }
                CMD_READ => {
                    assert!(len <= CHUNK);
                    match &mut self.reads_left {
                        Some(0) => return Ok(None),
                        Some(n) => *n -= 1,
                        None => {}
                    }
                    body[4..4 + len].copy_from_slice(&self.mem[addr..addr + len]);
                }
                CMD_WRITE => {
                    assert!(len <= CHUNK);
                    self.writes.push((addr as u16, req[6..6 + len].to_vec()));
                    if !self.stuck {
                        self.mem[addr..addr + len].copy_from_slice(&req[6..6 + len]);
                    }
                }
                c => panic!("unexpected command {c:#04x}"),
            }
            Ok(Some(nordic_packet(req[1], body)))
        }
    }

    const X1: Cid87 = Cid87 {
        sensor: Sensor::X1,
        max_rate: 1000,
        wireless: false,
    };

    fn set(m: &Cid87, dev: &mut Fake, key: SettingKey, v: SettingValue) -> Result<()> {
        m.write(dev, key, &v)
    }

    #[test]
    fn pairs_and_records_match_the_vendor_encoding() {
        assert_eq!(pair(0x2a), [0x2a, 0x2b]);
        assert_eq!(unpair(&[0x2a, 0x2b]), Some(0x2a));
        assert_eq!(unpair(&[0x2a, 0x2c]), None);
        // value = dpi / 10 - 1: 800 -> 79, 3200 -> 319 (page 1).
        assert_eq!(dpi_record(800).unwrap(), [79, 79, 0x00, 0xb7]);
        assert_eq!(dpi_record(3200).unwrap(), [0x3f, 0x3f, 0x44, 0x93]);
        assert!(dpi_record(805).is_err());
        assert!(dpi_record(10010).is_err());
        assert_eq!(dpi_of(&dpi_record(3200).unwrap()), Some(3200));
        assert_eq!(dpi_of(&dpi_record(10000).unwrap()), Some(10000));
        // The ranges above 10000 DPI (display only): 10050 and 20100 DPI.
        assert_eq!(dpi_of(&[0, 0, 0x22, 0x55 - 0x22]), Some(10050));
        assert_eq!(dpi_of(&[0, 0, 0x33, 0x55 - 0x33]), Some(20100));
        assert_eq!(dpi_of(&[79, 79, 0, 0]), None);
    }

    #[test]
    fn rate_codes() {
        let codes: Vec<_> = [125, 250, 500, 1000, 2000, 4000, 8000]
            .into_iter()
            .map(|hz| rate_code(hz).unwrap())
            .collect();
        assert_eq!(codes, [8, 4, 2, 1, 16, 32, 64]);
        assert_eq!(rate_code(750), None);
        assert_eq!(rate_of(32), Some(4000));
        assert_eq!(type_max_rate(2), 1000);
        assert_eq!(type_max_rate(5), 8000);
        assert!(type_wireless(1) && !type_wireless(3));
        // Undocumented types: the lowest ceiling, and the wireless hold.
        assert_eq!(type_max_rate(6), 1000);
        assert_eq!(type_max_rate(0xff), 1000);
        assert!(type_wireless(6) && !type_wireless(2));
    }

    #[test]
    fn memory_frames() {
        let r = mem_request(CMD_READ, 0x0c, &[], 10);
        assert_eq!(r[..6], [0x08, 0x08, 0x00, 0x00, 0x0c, 0x0a]);
        assert!(nordic_reply(&r, CMD_READ));
        let w = mem_request(CMD_WRITE, 0x0a, &pair(2), 2);
        assert_eq!(w[..8], [0x08, 0x07, 0x00, 0x00, 0x0a, 0x02, 0x02, 0x53]);
    }

    #[test]
    fn reads_shown_settings() {
        let mut dev = Fake::new();
        let s = X1.read_settings(&mut dev).unwrap();
        assert_eq!(s[SettingKey::ReportRate.as_str()], SettingValue::Int(1000));
        assert_eq!(s[SettingKey::DpiStageActive.as_str()], SettingValue::Int(2));
        assert_eq!(s[SettingKey::DpiStage2.as_str()], SettingValue::Int(800));
        assert_eq!(s[SettingKey::DpiStage4.as_str()], SettingValue::Int(3200));
        // Stage 5 is beyond the stage count.
        assert!(!s.contains_key(SettingKey::DpiStage5.as_str()));
        assert_eq!(s[SettingKey::Debounce.as_str()], SettingValue::Int(5));
        assert_eq!(
            s[SettingKey::AngleSnapping.as_str()],
            SettingValue::Bool(false)
        );
    }

    #[test]
    fn write_reads_checks_writes_and_verifies() {
        let mut dev = Fake::new();
        set(
            &X1,
            &mut dev,
            SettingKey::ReportRate,
            SettingValue::Int(500),
        )
        .unwrap();
        assert_eq!(dev.writes, [(0, vec![2, 0x53])]);
        set(
            &X1,
            &mut dev,
            SettingKey::DpiStage3,
            SettingValue::Int(2400),
        )
        .unwrap();
        assert_eq!(dev.writes[1], (0x14, dpi_record(2400).unwrap().to_vec()));
        set(
            &X1,
            &mut dev,
            SettingKey::DpiStageActive,
            SettingValue::Int(4),
        )
        .unwrap();
        assert_eq!(dev.writes[2], (4, vec![3, 0x52]));
        set(
            &X1,
            &mut dev,
            SettingKey::AngleSnapping,
            SettingValue::Bool(true),
        )
        .unwrap();
        assert_eq!(dev.writes[3], (0xaf, vec![1, 0x54]));
        // The same value again writes nothing.
        let n = dev.writes.len();
        set(&X1, &mut dev, SettingKey::Debounce, SettingValue::Int(5)).unwrap();
        assert_eq!(dev.writes.len(), n);
    }

    #[test]
    fn write_refusals() {
        let mut dev = Fake::new();
        let int = SettingValue::Int;
        // Above this mouse's rate, off the grid, out of range, stage unused.
        assert!(set(&X1, &mut dev, SettingKey::ReportRate, int(2000)).is_err());
        assert!(set(&X1, &mut dev, SettingKey::DpiStage1, int(405)).is_err());
        assert!(set(&X1, &mut dev, SettingKey::Debounce, int(1)).is_err());
        assert!(set(&X1, &mut dev, SettingKey::DpiStage5, int(800)).is_err());
        assert!(set(&X1, &mut dev, SettingKey::DpiStageActive, int(5)).is_err());
        assert!(set(&X1, &mut dev, SettingKey::LiftOffDistance, int(1)).is_err());
        // Other sensors' DPI tables are not written.
        let s3950 = Cid87 {
            sensor: Sensor::S3950,
            ..X1
        };
        assert!(set(&s3950, &mut dev, SettingKey::DpiStage1, int(800)).is_err());
        // A corrupt pair stops the write before anything is sent.
        dev.mem[0xa9] = 0x33;
        assert!(set(&X1, &mut dev, SettingKey::Debounce, int(8)).is_err());
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn a_mouse_that_does_not_store_the_write_is_reported() {
        let mut dev = Fake::new();
        dev.stuck = true;
        let e = set(
            &X1,
            &mut dev,
            SettingKey::ReportRate,
            SettingValue::Int(500),
        )
        .unwrap_err();
        assert!(e.to_string().contains("read-back"), "{e}");
    }

    #[test]
    fn a_write_whose_re_read_fails_keeps_the_written_value() {
        let mut dev = Fake::new();
        let mut state = X1.read_settings(&mut dev).unwrap();
        let mut stale = false;
        // The write's own check and read-back are answered, then nothing.
        dev.reads_left = Some(2);
        let e = X1
            .write_and_read(
                &mut dev,
                SettingKey::ReportRate,
                &SettingValue::Int(500),
                &mut state,
                &mut stale,
            )
            .unwrap_err();
        assert!(e.to_string().contains("written"), "{e}");
        assert_eq!(dev.writes, [(0, vec![2, 0x53])]);
        assert!(stale);
        assert_eq!(
            state[SettingKey::ReportRate.as_str()],
            SettingValue::Int(500)
        );
        assert_eq!(state[SettingKey::Debounce.as_str()], SettingValue::Int(5));
    }

    #[test]
    fn offline_mice_are_not_written_and_wireless_ones_are_held() {
        let mut dev = Fake::new();
        dev.online = false;
        assert!(with_mouse(&mut dev, true, |_| Ok(())).is_err());
        dev.online = true;
        dev.holds.set(0);
        with_mouse(&mut dev, true, |_| Ok(())).unwrap();
        assert_eq!(dev.holds.get(), 1);
        with_mouse(&mut dev, false, |_| Ok(())).unwrap();
        assert_eq!(dev.holds.get(), 1);
    }

    #[test]
    fn controls_unblock_only_what_is_written() {
        let base = super::super::controls(&super::super::NORDIC);
        let c = X1.controls(base.clone(), "x");
        let open: Vec<_> = c
            .iter()
            .filter(|m| m.blocked.is_none())
            .map(|m| m.key)
            .collect();
        for k in [
            SettingKey::ReportRate,
            SettingKey::DpiStageActive,
            SettingKey::DpiStage1,
            SettingKey::DpiStage5,
            SettingKey::Debounce,
            SettingKey::AngleSnapping,
        ] {
            assert!(open.contains(&k), "{k:?}");
        }
        assert!(c.iter().all(|m| m.persistent));
        for k in [
            SettingKey::LiftOffDistance,
            SettingKey::Effect,
            SettingKey::PmShutdown,
        ] {
            assert!(!open.contains(&k), "{k:?}");
        }
        let s3955 = Cid87 {
            sensor: Sensor::S3955,
            ..X1
        };
        let c = s3955.controls(base, "x");
        let dpi = c.iter().find(|m| m.key == SettingKey::DpiStage1).unwrap();
        assert!(dpi.blocked.is_some());
    }

    #[test]
    fn status_changed_push() {
        let mut body = [0u8; 14];
        body[4] = 0x01;
        body[5] = 0x02;
        let r = nordic_packet(0x0a, body);
        assert_eq!(parse_status_changed(&r), Some((1, 2)));
        assert_eq!(parse_status_changed(&nordic_packet(0x04, body)), None);
    }

    #[test]
    fn names_and_sensors_from_the_vendor_db() {
        assert_eq!(mid_name(1).as_deref(), Some("Pulsar X2 Crazylight Mini"));
        assert_eq!(
            mid_name(100).as_deref(),
            Some("Pulsar Feinmann F01 Noctua Edition")
        );
        assert_eq!(mid_name(107), None);
        assert_eq!(mid_name(255), None);
        assert_eq!(sensor_of(1), Sensor::X1);
        assert_eq!(sensor_of(68), Sensor::S3950);
        assert_eq!(sensor_of(133), Sensor::S3955);
        assert_eq!(sensor_of(250), Sensor::Unknown);
    }
}
