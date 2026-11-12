//! Per-model data for the generic Razer adapter (`device.rs`): data, not code.
//!
//! Sources (IDs and per-model parameters only; no code copied):
//! - OpenRazer kernel driver, `driver/razerkbd_driver.{c,h}` and
//!   `driver/razermouse_driver.{c,h}` (<https://github.com/openrazer/openrazer>):
//!   USB PIDs, the transaction id each command uses per PID (`Link::txns`
//!   lists the commands whose id differs from the PID's default), the report
//!   interface (`razer_get_report_params` / `razer_get_report` index), the
//!   SET→GET wait (`*_WAIT_US`), the LED ids each model drives (the device
//!   files `razer_*_probe` creates), classic vs. HyperPolling poll-rate
//!   commands, which models answer battery / charging / idle-time / DPI-stage
//!   queries.
//! - OpenRazer daemon, `daemon/openrazer_daemon/hardware/{keyboards,mouse}.py`:
//!   supported effects per model (`METHODS`), `DPI_MAX`, `POLL_RATES`,
//!   `WAVE_DIRS`.
//!
//! Every row below is taken from those two sources; the comment above a row
//! notes where a model deviates from the defaults of its section.

use super::protocol::{
    Effect, BACKLIGHT_LED, LEFT_SIDE_LED, LOGO_LED, RIGHT_SIDE_LED, SCROLL_LED, ZERO_LED,
};
use hako_core::device::DeviceKind;

/// Poll-rate command family for one PID.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Poll {
    None,
    /// class 0x00 cmd 0x05/0x85: 125 / 500 / 1000 Hz.
    Classic,
    /// HyperPolling, class 0x00 cmd 0x40/0xc0, with the rates the PID offers.
    /// `true`: sent twice, with argument 0x00 then 0x01 (mice); keyboards
    /// send it once.
    V2(&'static [u32], bool),
}

pub const CLASSIC_RATES: &[u32] = &[125, 500, 1000];
const HYPER_8K: Poll = Poll::V2(&[125, 500, 1000, 2000, 4000, 8000], true);
const HYPER_1K: Poll = Poll::V2(&[125, 500, 1000], true);
const KBD_8K: Poll = Poll::V2(&[125, 250, 500, 1000, 2000, 4000, 8000], false);
const KBD_1K: Poll = Poll::V2(&[125, 250, 500, 1000], false);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Battery {
    None,
    /// Rechargeable: level + charging status.
    Rechargeable,
    /// AA/AAA cell: level only (OpenRazer answers "not charging" itself).
    Cell,
}

/// Command families whose transaction id may differ from the link default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Family {
    Dpi,
    DpiStages,
    /// Setting the poll rate.
    Poll,
    /// Reading the poll rate (defaults to `Poll`).
    PollGet,
    /// The second HyperPolling set (argument 0x01).
    PollRepeat,
    /// Battery level and charging status.
    Battery,
    Idle,
    Brightness,
    Fx(Effect),
}

/// How one USB PID of a model is addressed.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    /// Default transaction id for DPI / poll rate / battery / idle commands.
    pub txn: u8,
    /// Default transaction id for lighting (brightness and effects).
    pub fx_txn: u8,
    /// USB interface (wIndex) the feature reports go to = hidraw node used.
    pub iface: u8,
    pub poll: Poll,
    /// Command families that use another transaction id than the defaults.
    pub txns: &'static [(Family, u8)],
}

impl Link {
    pub fn txn(&self, family: Family) -> u8 {
        if let Some(&(_, txn)) = self.txns.iter().find(|(f, _)| *f == family) {
            return txn;
        }
        match family {
            Family::Brightness | Family::Fx(_) => self.fx_txn,
            Family::PollGet | Family::PollRepeat => self.txn(Family::Poll),
            _ => self.txn,
        }
    }
}

/// Wave direction bytes the firmware takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wave {
    /// 1 = left, 2 = right.
    Std,
    /// 1 and 2 swapped (Ornata V3: kernel `direction ^= 3`).
    Flipped,
    /// 0x01 / 0x00 (daemon `WAVE_DIRS = (0, 1)`).
    ZeroBased,
}

#[derive(Debug)]
pub struct Spec {
    pub name: &'static str,
    pub kind: DeviceKind,
    /// Every PID the device enumerates as (wired first: it names the config).
    pub pids: &'static [u16],
    pub link: Link,
    /// A PID (wireless dongle) addressed differently from `link`.
    pub alt: Option<(u16, Link)>,
    /// LEDs driven together with the same effect; empty = no lighting.
    pub leds: &'static [u8],
    /// Supported effects (`Effect::bit` mask).
    pub effects: u8,
    pub wave: Wave,
    /// 0 = no DPI control.
    pub dpi_max: u16,
    /// DPI stages (class 0x04 cmd 0x06/0x86).
    pub stages: bool,
    pub battery: Battery,
    pub idle: bool,
    /// Transaction id of the game-mode command, if the keyboard has one.
    pub game_txn: Option<u8>,
    /// Delay between SET_REPORT and GET_REPORT (wireless links need longer).
    pub wait_ms: u64,
    /// Why "save to device" mode keeps this model's lighting volatile; `None`
    /// = it is stored with VARSTORE, as OpenRazer does for the same PIDs.
    pub volatile_lighting: Option<&'static str>,
}

impl Spec {
    pub fn link_for(&self, pid: u16) -> Link {
        match self.alt {
            Some((alt, link)) if alt == pid => link,
            _ => self.link,
        }
    }

    pub fn has_effect(&self, fx: Effect) -> bool {
        self.effects & fx.bit() != 0
    }

    const fn base(name: &'static str, kind: DeviceKind, pids: &'static [u16], link: Link) -> Self {
        Self {
            name,
            kind,
            pids,
            link,
            alt: None,
            leds: &[],
            effects: 0,
            wave: Wave::Std,
            dpi_max: 0,
            stages: false,
            battery: Battery::None,
            idle: false,
            game_txn: None,
            wait_ms: 1,
            volatile_lighting: None,
        }
    }

    const fn volatile_lighting(mut self, why: &'static str) -> Self {
        self.volatile_lighting = Some(why);
        self
    }

    const fn alt(mut self, pid: u16, link: Link) -> Self {
        self.alt = Some((pid, link));
        self
    }

    const fn lit(mut self, leds: &'static [u8], effects: u8) -> Self {
        self.leds = leds;
        self.effects = effects;
        self
    }

    const fn battery(mut self, battery: Battery) -> Self {
        self.battery = battery;
        self
    }

    const fn idle(mut self) -> Self {
        self.idle = true;
        self
    }

    const fn stages(mut self) -> Self {
        self.stages = true;
        self
    }

    const fn wait(mut self, ms: u64) -> Self {
        self.wait_ms = ms;
        self
    }

    const fn game(mut self, txn: u8) -> Self {
        self.game_txn = Some(txn);
        self
    }

    const fn wave(mut self, wave: Wave) -> Self {
        self.wave = wave;
        self
    }

    const fn fx_txn(mut self, txn: u8) -> Self {
        self.link.fx_txn = txn;
        self
    }

    const fn iface(mut self, iface: u8) -> Self {
        self.link.iface = iface;
        self
    }

    const fn poll(mut self, poll: Poll) -> Self {
        self.link.poll = poll;
        self
    }

    /// Per-family transaction ids of the main link.
    const fn txns(mut self, txns: &'static [(Family, u8)]) -> Self {
        self.link.txns = txns;
        self
    }
}

const OFF: u8 = Effect::Off.bit();
const STATIC: u8 = Effect::Static.bit();
const SPECTRUM: u8 = Effect::Spectrum.bit();
const WAVE: u8 = Effect::Wave.bit();
const REACTIVE: u8 = Effect::Reactive.bit();
const BREATH: u8 = Effect::Breathing.bit();
const STARLIGHT: u8 = Effect::Starlight.bit();
const ALL_FX: u8 = OFF | STATIC | SPECTRUM | WAVE | REACTIVE | BREATH | STARLIGHT;
/// Per-zone mouse LEDs (logo / scroll wheel / side).
const ZONE_FX: u8 = OFF | STATIC | SPECTRUM | REACTIVE | BREATH;
/// Whole-matrix mice (Basilisk V3 / Cobra Pro family).
const MATRIX_FX: u8 = OFF | STATIC | SPECTRUM | WAVE;
/// DeathAdder Essential family: static / breathing only.
const BASIC_FX: u8 = OFF | STATIC | BREATH;
const SIDES: &[u8] = &[LOGO_LED, SCROLL_LED, LEFT_SIDE_LED, RIGHT_SIDE_LED];

/// Lighting the save mode does not store (docs/onboard-writes/razer.md).
const ACTIVE_PROFILE: &str =
    "VARSTORE writes profile 1, not the active onboard profile (OpenRazer PR #2935)";
const BASILISK_V3_BOOT: &str =
    "a Basilisk V3 stopped enumerating at boot with a stored effect (OpenRazer #1767)";
const OTHER_FX_COMMANDS: &str =
    "OpenRazer drives part of this model's lighting with other commands; not confirmed";

const fn link(txn: u8, iface: u8) -> Link {
    Link {
        txn,
        fx_txn: txn,
        iface,
        poll: Poll::None,
        txns: &[],
    }
}

/// Keyboard: extended-matrix backlight, game mode via the standard LED command
/// with transaction 0xff (OpenRazer `razer_attr_write_game_led_state`).
const fn kbd(name: &'static str, pids: &'static [u16], txn: u8, iface: u8, fx: u8) -> Spec {
    Spec::base(name, DeviceKind::Keyboard, pids, link(txn, iface))
        .lit(&[BACKLIGHT_LED], fx)
        .game(0xff)
}

/// Mouse on interface 0; `txn` is the default for every command.
const fn mouse(
    name: &'static str,
    pids: &'static [u16],
    txn: u8,
    dpi_max: u16,
    poll: Poll,
) -> Spec {
    let mut s = Spec::base(name, DeviceKind::Mouse, pids, link(txn, 0)).poll(poll);
    s.dpi_max = dpi_max;
    s
}

use Battery::{Cell, Rechargeable};
use Family::{Battery as Bat, DpiStages, Fx, Idle, PollGet, PollRepeat};
use Poll::Classic;

/// Idle time and DPI stages on 0xff (Viper / DeathAdder V2 / Lancehead era).
const IDLE_STAGES_FF: &[(Family, u8)] = &[(Idle, 0xff), (DpiStages, 0xff)];
const STAGES_FF: &[(Family, u8)] = &[(DpiStages, 0xff)];
const IDLE_FF: &[(Family, u8)] = &[(Idle, 0xff)];

pub const SPECS: &[Spec] = &[
    // ── Keyboards (extended matrix, BACKLIGHT_LED) ──────────────────────────
    // Transaction 0x3f generation.
    kbd("Razer Huntsman Elite", &[0x0226], 0x3f, 1, ALL_FX).wave(Wave::ZeroBased),
    kbd("Razer Huntsman", &[0x0227], 0x3f, 1, ALL_FX).wave(Wave::ZeroBased),
    kbd(
        "Razer Huntsman Tournament Edition",
        &[0x0243],
        0x3f,
        2,
        ALL_FX,
    )
    .wave(Wave::ZeroBased),
    kbd("Razer Huntsman Mini", &[0x0257], 0x3f, 2, ALL_FX).wave(Wave::ZeroBased),
    kbd("Razer Huntsman Mini (JP)", &[0x0269], 0x3f, 2, ALL_FX).wave(Wave::ZeroBased),
    kbd("Razer BlackWidow V3", &[0x024e], 0x3f, 3, ALL_FX),
    kbd("Razer BlackWidow V3 Tenkeyless", &[0x0a24], 0x3f, 2, ALL_FX),
    kbd(
        "Razer BlackWidow V3 Pro",
        &[0x025a, 0x025c],
        0x3f,
        2,
        ALL_FX,
    )
    .alt(0x025c, link(0x9f, 2))
    .battery(Rechargeable)
    .wait(5),
    kbd("Razer BlackWidow 2019", &[0x0241], 0x3f, 1, ALL_FX),
    // Spectrum / reactive / starlight use the standard matrix protocol here.
    kbd(
        "Razer BlackWidow Essential",
        &[0x0237],
        0x3f,
        1,
        OFF | STATIC | WAVE | BREATH,
    ),
    kbd("Razer BlackWidow Lite", &[0x0235], 0x3f, 1, BASIC_FX),
    kbd("Razer Ornata Chroma", &[0x021e], 0x3f, 1, ALL_FX).wave(Wave::ZeroBased),
    kbd("Razer Ornata", &[0x021f], 0x3f, 1, ALL_FX & !SPECTRUM).wave(Wave::ZeroBased),
    kbd("Razer Cynosa Chroma", &[0x022a], 0x3f, 1, ALL_FX).wave(Wave::ZeroBased),
    kbd("Razer Cynosa Chroma Pro", &[0x022c], 0x3f, 1, ALL_FX).wave(Wave::ZeroBased),
    kbd(
        "Razer Cynosa Lite",
        &[0x023f],
        0x3f,
        1,
        OFF | STATIC | SPECTRUM | BREATH,
    ),
    kbd("Razer DeathStalker V2", &[0x0295], 0x3f, 3, ALL_FX),
    // Wired: lighting on 0x3f but battery on 0x1f; dongle: everything on 0x9f.
    kbd(
        "Razer DeathStalker V2 Pro",
        &[0x0292, 0x0290],
        0x3f,
        3,
        ALL_FX,
    )
    .txns(&[(Bat, 0x1f)])
    .alt(0x0290, link(0x9f, 2))
    .battery(Rechargeable)
    .wait(5),
    kbd(
        "Razer DeathStalker V2 Pro Tenkeyless",
        &[0x0298, 0x0296],
        0x3f,
        3,
        ALL_FX,
    )
    .txns(&[(Bat, 0x1f)])
    .alt(0x0296, link(0x9f, 2))
    .battery(Rechargeable)
    .wait(5),
    // Transaction 0x1f generation (wireless dongles use 0x9f).
    kbd("Razer BlackWidow Elite", &[0x0228], 0x1f, 1, ALL_FX).wave(Wave::ZeroBased),
    kbd(
        "Razer BlackWidow V3 Mini HyperSpeed",
        &[0x0258, 0x0271],
        0x1f,
        3,
        ALL_FX,
    )
    .alt(0x0271, link(0x9f, 3))
    .battery(Rechargeable)
    .wait(5),
    kbd("Razer BlackWidow V4", &[0x0287], 0x1f, 3, ALL_FX).poll(KBD_8K),
    kbd("Razer BlackWidow V4 Pro", &[0x028d], 0x1f, 3, ALL_FX).poll(KBD_8K),
    kbd("Razer BlackWidow V4 75%", &[0x02a5], 0x1f, 3, ALL_FX).poll(KBD_8K),
    // Static and reactive go out on 0x3f, the other effects on 0x1f.
    kbd("Razer BlackWidow V4 X", &[0x0293], 0x1f, 2, ALL_FX)
        .txns(&[(Fx(Effect::Static), 0x3f), (Fx(Effect::Reactive), 0x3f)]),
    kbd(
        "Razer BlackWidow V4 Mini HyperSpeed",
        &[0x02b9, 0x02ba],
        0x1f,
        3,
        ALL_FX,
    )
    .alt(0x02ba, link(0x9f, 3))
    .battery(Rechargeable)
    .wait(5),
    kbd(
        "Razer BlackWidow V4 Tenkeyless HyperSpeed",
        &[0x02d7, 0x02d5],
        0x1f,
        3,
        ALL_FX,
    )
    .poll(KBD_1K)
    .alt(
        0x02d5,
        Link {
            poll: KBD_1K,
            ..link(0x9f, 2)
        },
    )
    .battery(Rechargeable)
    .wait(5),
    // Huntsman V2 (full size) sends game mode with its own 0x1f transaction.
    kbd("Razer Huntsman V2", &[0x026c], 0x1f, 3, ALL_FX)
        .game(0x1f)
        .poll(KBD_8K),
    kbd(
        "Razer Huntsman V2 Analog",
        &[0x0266],
        0x1f,
        3,
        ALL_FX & !STARLIGHT,
    ),
    kbd(
        "Razer Huntsman Mini Analog",
        &[0x0282],
        0x1f,
        3,
        ALL_FX & !STARLIGHT,
    ),
    kbd("Razer Huntsman V3 Pro", &[0x02a6], 0x1f, 3, ALL_FX).volatile_lighting(ACTIVE_PROFILE),
    kbd(
        "Razer Huntsman V3 Pro Tenkeyless",
        &[0x02a7],
        0x1f,
        3,
        ALL_FX,
    )
    .volatile_lighting(ACTIVE_PROFILE),
    kbd("Razer Huntsman V3 Pro Mini", &[0x02b0], 0x1f, 3, ALL_FX)
        .wave(Wave::ZeroBased)
        .volatile_lighting(ACTIVE_PROFILE),
    kbd("Razer Ornata V2", &[0x025d], 0x1f, 2, ALL_FX),
    kbd(
        "Razer Ornata V3",
        &[0x02a1, 0x028f],
        0x1f,
        2,
        OFF | STATIC | SPECTRUM | WAVE | BREATH,
    )
    .wave(Wave::Flipped),
    kbd(
        "Razer Ornata V3 Tenkeyless",
        &[0x02a3],
        0x1f,
        2,
        OFF | STATIC | SPECTRUM | WAVE | REACTIVE | BREATH,
    ),
    kbd(
        "Razer Ornata V3 X",
        &[0x0294, 0x02a2],
        0x1f,
        1,
        OFF | STATIC | SPECTRUM | BREATH,
    ),
    kbd("Razer Cynosa V2", &[0x025e], 0x1f, 1, ALL_FX).wave(Wave::ZeroBased),
    kbd("Razer Joro", &[0x02cd], 0x1f, 3, ALL_FX).battery(Rechargeable),
    // ── Mice (interface 0 unless noted) ─────────────────────────────────────
    // DeathAdder.
    mouse("Razer DeathAdder V2", &[0x0084], 0x3f, 20000, Classic)
        .lit(&[LOGO_LED, SCROLL_LED], ZONE_FX),
    mouse("Razer DeathAdder V2 Mini", &[0x008c], 0x3f, 8500, Classic)
        .lit(&[LOGO_LED], ZONE_FX)
        .txns(STAGES_FF)
        .stages(),
    mouse(
        "Razer DeathAdder V2 Pro",
        &[0x007c, 0x007d],
        0x3f,
        20000,
        Classic,
    )
    .lit(&[LOGO_LED], ZONE_FX)
    .txns(IDLE_STAGES_FF)
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(60),
    mouse("Razer DeathAdder V2 Lite", &[0x00a1], 0x1f, 8500, Classic)
        .lit(&[LOGO_LED], ZONE_FX)
        .txns(STAGES_FF)
        .stages(),
    mouse(
        "Razer DeathAdder V2 X HyperSpeed",
        &[0x009c],
        0x1f,
        14000,
        Classic,
    )
    .stages()
    .battery(Cell)
    .idle()
    .wait(31),
    mouse("Razer DeathAdder V3", &[0x00b2], 0x1f, 30000, HYPER_8K).stages(),
    // 0x00c2 / 0x00c3: later DeathAdder V3 Pro revision.
    mouse(
        "Razer DeathAdder V3 Pro",
        &[0x00b6, 0x00b7, 0x00c2, 0x00c3],
        0x1f,
        35000,
        Classic,
    )
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31),
    mouse(
        "Razer DeathAdder V3 HyperSpeed",
        &[0x00c4, 0x00c5],
        0x1f,
        26000,
        Classic,
    )
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31),
    mouse(
        "Razer DeathAdder V4 Pro",
        &[0x00be, 0x00bf],
        0x1f,
        45000,
        HYPER_8K,
    )
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31),
    mouse("Razer DeathAdder Elite", &[0x005c], 0x3f, 16000, Classic)
        .lit(&[LOGO_LED, SCROLL_LED], ZONE_FX),
    // DeathAdder Essential family: settings on 0xff, lighting on 0x3f.
    mouse("Razer DeathAdder Essential", &[0x006e], 0xff, 6400, Classic)
        .fx_txn(0x3f)
        .lit(&[LOGO_LED, SCROLL_LED], BASIC_FX),
    mouse(
        "Razer DeathAdder Essential (White Edition)",
        &[0x0071],
        0xff,
        6400,
        Classic,
    )
    .fx_txn(0x3f)
    .lit(&[LOGO_LED, SCROLL_LED], BASIC_FX),
    mouse(
        "Razer DeathAdder Essential (2021)",
        &[0x0098],
        0xff,
        6400,
        Classic,
    )
    .fx_txn(0x3f)
    .lit(&[LOGO_LED], BASIC_FX),
    // DeathAdder Chroma: lighting uses the standard protocol (not driven).
    mouse("Razer DeathAdder Chroma", &[0x0043], 0xff, 10000, Classic),
    mouse("Razer DeathAdder 2000", &[0x004f], 0xff, 2000, Classic),
    mouse("Razer DeathAdder 3500", &[0x0054], 0xff, 3500, Classic),
    // Viper (2019) family: settings on 0xff, lighting on 0x3f.
    mouse("Razer Viper", &[0x0078], 0xff, 16000, Classic)
        .fx_txn(0x3f)
        .lit(&[LOGO_LED], ZONE_FX)
        .stages(),
    mouse("Razer Viper Mini", &[0x008a], 0xff, 8500, Classic)
        .fx_txn(0x3f)
        .lit(&[LOGO_LED], ZONE_FX)
        .stages(),
    mouse(
        "Razer Viper Ultimate",
        &[0x007a, 0x007b],
        0xff,
        20000,
        Classic,
    )
    .fx_txn(0x3f)
    .lit(&[LOGO_LED], ZONE_FX)
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(60),
    // Viper 8K: DPI on 0xff, lighting and the first HyperPolling set on 0x1f,
    // the second HyperPolling set back on 0xff.
    mouse("Razer Viper 8KHz", &[0x0091], 0xff, 20000, HYPER_8K)
        .fx_txn(0x1f)
        .txns(&[(Family::Poll, 0x1f), (PollRepeat, 0xff)])
        .lit(&[LOGO_LED], ZONE_FX)
        .stages(),
    mouse(
        "Razer Viper Mini Signature Edition",
        &[0x009e, 0x009f],
        0x1f,
        30000,
        HYPER_1K,
    )
    .alt(
        0x009f,
        Link {
            poll: HYPER_8K,
            ..link(0x1f, 0)
        },
    )
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(60),
    mouse(
        "Razer Viper V2 Pro",
        &[0x00a5, 0x00a6],
        0x1f,
        30000,
        Classic,
    )
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31),
    mouse("Razer Viper V3 HyperSpeed", &[0x00b8], 0x1f, 30000, Classic)
        .stages()
        .battery(Cell)
        .idle()
        .wait(60),
    mouse(
        "Razer Viper V3 Pro",
        &[0x00c0, 0x00c1],
        0x1f,
        35000,
        Classic,
    )
    .alt(
        0x00c1,
        Link {
            poll: HYPER_8K,
            ..link(0x1f, 0)
        },
    )
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(60),
    // Both PIDs use the classic poll-rate command.
    mouse(
        "Razer Viper V3 Pro SE",
        &[0x00de, 0x00df],
        0x1f,
        35000,
        Classic,
    )
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(60),
    // HyperPolling dongle on its own (its paired mouse is not reported):
    // second HyperPolling set on 0xff.
    mouse(
        "Razer HyperPolling Wireless Dongle",
        &[0x00b3],
        0x1f,
        30000,
        HYPER_8K,
    )
    .txns(&[(PollRepeat, 0xff)])
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(60),
    // Basilisk.
    mouse("Razer Basilisk", &[0x0064], 0x3f, 16000, Classic).lit(&[LOGO_LED, SCROLL_LED], ZONE_FX),
    mouse("Razer Basilisk Essential", &[0x0065], 0x3f, 6400, Classic)
        .lit(&[LOGO_LED], ZONE_FX)
        .txns(STAGES_FF)
        .stages(),
    mouse("Razer Basilisk V2", &[0x0085], 0x1f, 20000, Classic)
        .lit(&[LOGO_LED, SCROLL_LED], ZONE_FX),
    // Basilisk V3 (wired) has no "none" effect; both wired V3s use interface 3.
    mouse("Razer Basilisk V3", &[0x0099], 0x1f, 26000, Classic)
        .iface(3)
        .lit(&[ZERO_LED], STATIC | SPECTRUM | WAVE)
        .txns(STAGES_FF)
        .stages()
        .volatile_lighting(BASILISK_V3_BOOT),
    mouse("Razer Basilisk V3 35K", &[0x00cb], 0x1f, 35000, Classic)
        .iface(3)
        .lit(&[ZERO_LED], MATRIX_FX)
        .stages()
        .volatile_lighting(BASILISK_V3_BOOT),
    mouse(
        "Razer Basilisk V3 Pro",
        &[0x00aa, 0x00ab],
        0x1f,
        30000,
        Classic,
    )
    .lit(&[ZERO_LED], MATRIX_FX)
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31)
    .volatile_lighting(BASILISK_V3_BOOT),
    mouse(
        "Razer Basilisk V3 Pro 35K",
        &[0x00cc, 0x00cd],
        0x1f,
        35000,
        Classic,
    )
    .lit(&[ZERO_LED], MATRIX_FX)
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31)
    .volatile_lighting(BASILISK_V3_BOOT),
    mouse(
        "Razer Basilisk V3 Pro 35K Phantom Green Edition",
        &[0x00d6, 0x00d7],
        0x1f,
        35000,
        Classic,
    )
    .lit(&[ZERO_LED], MATRIX_FX)
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31)
    .volatile_lighting(BASILISK_V3_BOOT),
    mouse(
        "Razer Basilisk V3 X HyperSpeed",
        &[0x00b9],
        0x1f,
        18000,
        Classic,
    )
    .lit(&[SCROLL_LED], ZONE_FX | WAVE)
    .stages()
    .battery(Cell)
    .idle()
    .wait(31)
    .volatile_lighting(BASILISK_V3_BOOT),
    // Idle time and poll-rate reads on 0xff; the logo / scroll LEDs have no
    // wave, so all four zones share the per-zone effects.
    mouse(
        "Razer Basilisk Ultimate",
        &[0x0086, 0x0088],
        0x1f,
        20000,
        Classic,
    )
    .lit(SIDES, ZONE_FX)
    .txns(&[(Idle, 0xff), (PollGet, 0xff)])
    .battery(Rechargeable)
    .idle()
    .wait(31),
    mouse(
        "Razer Basilisk X HyperSpeed",
        &[0x0083],
        0xff,
        16000,
        Classic,
    )
    .stages()
    .battery(Cell)
    .idle()
    .wait(31),
    // Lighting uses a model-specific static command (not driven).
    mouse(
        "Razer Basilisk Mobile",
        &[0x00d3, 0x00d4],
        0x1f,
        18000,
        Classic,
    )
    .stages()
    .battery(Cell)
    .idle()
    .wait(31),
    // Naga.
    mouse("Razer Naga X", &[0x0096], 0x1f, 18000, Classic)
        .iface(3)
        .lit(&[SCROLL_LED, LEFT_SIDE_LED], ZONE_FX | WAVE)
        .txns(STAGES_FF)
        .stages(),
    // Logo LED only: the side-panel matrix uses Naga Trinity commands.
    mouse("Razer Naga V2 Pro", &[0x00a7, 0x00a8], 0x1f, 30000, Classic)
        .lit(&[LOGO_LED], ZONE_FX | WAVE)
        .stages()
        .battery(Rechargeable)
        .idle()
        .wait(31)
        .volatile_lighting(OTHER_FX_COMMANDS),
    mouse("Razer Naga V2 HyperSpeed", &[0x00b4], 0x1f, 30000, Classic)
        .stages()
        .battery(Cell)
        .idle()
        .wait(31),
    // Logo and scroll LEDs; the side-panel matrix uses other commands.
    mouse("Razer Naga Pro", &[0x008f, 0x0090], 0x1f, 20000, Classic)
        .lit(&[LOGO_LED, SCROLL_LED], ZONE_FX | WAVE)
        .stages()
        .battery(Rechargeable)
        .idle()
        .wait(31)
        .volatile_lighting(OTHER_FX_COMMANDS),
    mouse(
        "Razer Naga Left-Handed Edition 2020",
        &[0x008d],
        0x1f,
        20000,
        Classic,
    )
    .lit(&[LOGO_LED, SCROLL_LED, RIGHT_SIDE_LED], ZONE_FX | WAVE),
    // Lighting uses Naga Trinity / mouse-extended commands (not driven).
    mouse("Razer Naga Trinity", &[0x0067], 0xff, 16000, Classic),
    mouse("Razer Naga Hex V2", &[0x0050], 0x3f, 16000, Classic),
    mouse("Razer Naga Chroma", &[0x0053], 0xff, 16000, Classic),
    mouse("Razer Naga 2014", &[0x0040], 0xff, 8200, Classic),
    mouse(
        "Razer Naga Epic Chroma",
        &[0x003e, 0x003f],
        0xff,
        8200,
        Classic,
    )
    .battery(Rechargeable)
    .idle(),
    // Receiver only (the Bluetooth PID has no USB interface to address).
    mouse("Razer Orochi V2", &[0x0094], 0x1f, 18000, Classic)
        .txns(IDLE_FF)
        .stages()
        .battery(Cell)
        .idle()
        .wait(400),
    mouse("Razer Orochi (2015)", &[0x0048], 0xff, 8200, Classic).idle(),
    // Cobra: breathing goes out on 0x3f, the other effects on 0x1f.
    mouse("Razer Cobra", &[0x00a3], 0xff, 8500, Classic)
        .fx_txn(0x1f)
        .txns(&[(Fx(Effect::Breathing), 0x3f)])
        .lit(&[LOGO_LED], ZONE_FX)
        .stages(),
    mouse("Razer Cobra Pro", &[0x00af, 0x00b0], 0x1f, 30000, Classic)
        .lit(&[ZERO_LED], MATRIX_FX)
        .stages()
        .battery(Rechargeable)
        .idle()
        .wait(31),
    mouse(
        "Razer Cobra HyperSpeed",
        &[0x00da, 0x00db],
        0x1f,
        26000,
        Classic,
    )
    .lit(&[ZERO_LED], MATRIX_FX)
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31),
    // Pro Click.
    mouse("Razer Pro Click", &[0x0080, 0x0077], 0x1f, 16000, Classic)
        .txns(IDLE_FF)
        .stages()
        .battery(Rechargeable)
        .idle()
        .wait(31),
    mouse("Razer Pro Click Mini", &[0x009a], 0x1f, 12000, Classic)
        .stages()
        .battery(Rechargeable)
        .idle()
        .wait(31),
    // The daemon also lists 250 Hz, which the classic command cannot encode.
    mouse(
        "Razer Pro Click V2",
        &[0x00d0, 0x00d1],
        0x1f,
        30000,
        Classic,
    )
    .lit(&[ZERO_LED], MATRIX_FX)
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31)
    .volatile_lighting(OTHER_FX_COMMANDS),
    mouse(
        "Razer Pro Click V2 Vertical Edition",
        &[0x00c7, 0x00c8],
        0x1f,
        30000,
        Classic,
    )
    .lit(&[ZERO_LED], MATRIX_FX)
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31)
    .volatile_lighting(OTHER_FX_COMMANDS),
    // Mamba.
    mouse("Razer Mamba Elite", &[0x006c], 0x1f, 16000, Classic)
        .txns(&[(Family::Poll, 0xff)])
        .lit(SIDES, ZONE_FX | WAVE),
    mouse(
        "Razer Mamba Wireless",
        &[0x0073, 0x0072],
        0x3f,
        16000,
        Classic,
    )
    .lit(&[LOGO_LED, SCROLL_LED], ZONE_FX)
    .txns(IDLE_FF)
    .battery(Rechargeable)
    .idle()
    .wait(31),
    // Mamba (2015) family: lighting uses the standard protocol (not driven).
    // Poll rate is set on 0x3f but read on 0xff.
    mouse(
        "Razer Mamba Chroma",
        &[0x0044, 0x0045],
        0xff,
        16000,
        Classic,
    )
    .txns(&[(Family::Poll, 0x3f), (PollGet, 0xff)])
    .battery(Rechargeable)
    .idle(),
    mouse(
        "Razer Mamba Tournament Edition",
        &[0x0046],
        0xff,
        16000,
        Poll::None,
    ),
    mouse("Razer Mamba (2012)", &[0x0024, 0x0025], 0xff, 6400, Classic)
        .battery(Rechargeable)
        .idle(),
    // Lancehead (all 0x3f; idle time and DPI stages on 0xff).
    mouse("Razer Lancehead", &[0x0059, 0x005a], 0x3f, 16000, Classic)
        .lit(SIDES, ZONE_FX | WAVE)
        .txns(IDLE_STAGES_FF)
        .stages()
        .battery(Rechargeable)
        .idle(),
    mouse(
        "Razer Lancehead Tournament Edition",
        &[0x0060],
        0x3f,
        16000,
        Classic,
    )
    .lit(SIDES, ZONE_FX | WAVE)
    .txns(STAGES_FF)
    .stages(),
    // 2019 model: battery on 0x1f.
    mouse(
        "Razer Lancehead Wireless",
        &[0x0070, 0x006f],
        0x3f,
        16000,
        Classic,
    )
    .lit(SIDES, ZONE_FX | WAVE)
    .txns(&[(Idle, 0xff), (DpiStages, 0xff), (Bat, 0x1f)])
    .stages()
    .battery(Rechargeable)
    .idle()
    .wait(31),
    // Atheris: idle time and DPI stages on 0xff.
    mouse("Razer Atheris", &[0x0062], 0x1f, 7200, Classic)
        .txns(IDLE_STAGES_FF)
        .stages()
        .battery(Cell)
        .idle()
        .wait(400),
    // Abyssus: settings on 0xff, lighting on 0x3f.
    mouse(
        "Razer Abyssus Elite (D.Va Edition)",
        &[0x006a],
        0xff,
        7200,
        Classic,
    )
    .fx_txn(0x3f)
    .lit(&[LOGO_LED], ZONE_FX),
    mouse("Razer Abyssus Essential", &[0x006b], 0xff, 7200, Classic)
        .fx_txn(0x3f)
        .lit(&[LOGO_LED], ZONE_FX),
    mouse("Razer Abyssus V2", &[0x005b], 0xff, 5000, Classic),
    mouse("Razer Abyssus 2000", &[0x005e], 0xff, 2000, Classic),
    // Older wired / wireless mice: DPI and poll rate only.
    mouse(
        "Razer Diamondback Chroma",
        &[0x004c],
        0xff,
        16000,
        Poll::None,
    ),
    mouse("Razer Taipan", &[0x0034], 0xff, 8200, Classic),
    mouse("Razer Ouroboros", &[0x0032], 0xff, 8200, Classic)
        .battery(Rechargeable)
        .idle(),
];

pub fn spec_for(pid: u16) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.pids.contains(&pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent() {
        let mut seen = std::collections::HashSet::new();
        for s in SPECS {
            assert!(!s.pids.is_empty(), "{}", s.name);
            for &pid in s.pids {
                assert!(seen.insert(pid), "{pid:04x} listed twice ({})", s.name);
                // Huntsman V2 TKL has its own adapter in mod.rs.
                assert_ne!(pid, 0x026b, "{}", s.name);
            }
            if let Some((alt, _)) = s.alt {
                assert!(
                    s.pids[1..].contains(&alt),
                    "{}: alt PID not a secondary PID",
                    s.name
                );
            }
            let has_control = !s.leds.is_empty()
                || s.dpi_max > 0
                || s.link.poll != Poll::None
                || s.game_txn.is_some()
                || s.idle;
            assert!(
                has_control || s.battery != Battery::None,
                "{} exposes nothing",
                s.name
            );
            assert_eq!(
                s.leds.is_empty(),
                s.effects == 0,
                "{}: LEDs without effects",
                s.name
            );
            assert!(s.effects & !ALL_FX == 0, "{}", s.name);
            assert!(s.dpi_max == 0 || s.dpi_max >= 100, "{}", s.name);
            assert!(!s.stages || s.dpi_max > 0, "{}: stages without DPI", s.name);
            assert!(s.wait_ms > 0 && s.wait_ms <= 400, "{}", s.name);
            assert_eq!(
                s.kind == DeviceKind::Keyboard,
                s.game_txn.is_some(),
                "{}",
                s.name
            );
            assert!(s.name.starts_with("Razer "), "{}", s.name);
            // Overrides only for families the model uses, each listed once.
            for link in std::iter::once(s.link).chain(s.alt.map(|(_, l)| l)) {
                for (i, (f, _)) in link.txns.iter().enumerate() {
                    assert!(
                        !link.txns[..i].iter().any(|(g, _)| g == f),
                        "{}: {f:?} twice",
                        s.name
                    );
                    let used = match f {
                        Family::Dpi => s.dpi_max > 0,
                        Family::DpiStages => s.stages,
                        Family::Poll | Family::PollGet => link.poll != Poll::None,
                        Family::PollRepeat => matches!(link.poll, Poll::V2(_, true)),
                        Family::Battery => s.battery != Battery::None,
                        Family::Idle => s.idle,
                        Family::Brightness => !s.leds.is_empty(),
                        Family::Fx(fx) => s.has_effect(*fx),
                    };
                    assert!(used, "{}: override for unused {f:?}", s.name);
                }
            }
        }
    }

    #[test]
    fn links_per_pid() {
        let bw = spec_for(0x02d5).unwrap();
        assert_eq!(bw.name, "Razer BlackWidow V4 Tenkeyless HyperSpeed");
        assert_eq!(
            (bw.link_for(0x02d7).txn, bw.link_for(0x02d7).iface),
            (0x1f, 3)
        );
        assert_eq!(
            (bw.link_for(0x02d5).txn, bw.link_for(0x02d5).iface),
            (0x9f, 2)
        );
        let viper = spec_for(0x00c1).unwrap();
        assert_eq!(viper.link_for(0x00c0).poll, Classic);
        assert_eq!(viper.link_for(0x00c1).poll, HYPER_8K);
        let v = spec_for(0x0078).unwrap();
        assert_eq!((v.link.txn, v.link.fx_txn), (0xff, 0x3f));
        assert!(spec_for(0x026b).is_none());
    }

    /// Transaction ids per command, as OpenRazer assigns them for these PIDs.
    #[test]
    fn per_family_transaction_ids() {
        use Family::*;
        let t = |pid: u16, f: Family| spec_for(pid).unwrap().link_for(pid).txn(f);
        // Cobra: razer_attr_write_matrix_effect_breath_common puts it on 0x3f.
        assert_eq!(t(0x00a3, Fx(Effect::Breathing)), 0x3f);
        assert_eq!(t(0x00a3, Fx(Effect::Static)), 0x1f);
        assert_eq!(t(0x00a3, Brightness), 0x1f);
        assert_eq!((t(0x00a3, Dpi), t(0x00a3, DpiStages)), (0xff, 0xff));
        // Viper 8K: poll rate 2 first on 0x1f, repeated on 0xff.
        assert_eq!((t(0x0091, Poll), t(0x0091, PollRepeat)), (0x1f, 0xff));
        assert_eq!((t(0x0091, Dpi), t(0x0091, Brightness)), (0xff, 0x1f));
        // Other HyperPolling mice repeat on the same id.
        assert_eq!(t(0x00b2, PollRepeat), 0x1f);
        assert_eq!(t(0x00b3, PollRepeat), 0xff);
        // DeathAdder V2 Pro: battery / DPI 0x3f, idle and stages 0xff.
        assert_eq!((t(0x007d, Battery), t(0x007d, Idle)), (0x3f, 0xff));
        assert_eq!(t(0x007d, DpiStages), 0xff);
        // Lancehead Wireless (2019): battery 0x1f, lighting 0x3f, idle 0xff.
        assert_eq!(t(0x006f, Battery), 0x1f);
        assert_eq!((t(0x006f, Fx(Effect::Wave)), t(0x006f, Idle)), (0x3f, 0xff));
        // BlackWidow V4 X: static / reactive 0x3f, the rest 0x1f.
        assert_eq!(t(0x0293, Fx(Effect::Static)), 0x3f);
        assert_eq!(t(0x0293, Fx(Effect::Reactive)), 0x3f);
        assert_eq!(t(0x0293, Fx(Effect::Wave)), 0x1f);
        assert_eq!(t(0x0293, Brightness), 0x1f);
        // DeathStalker V2 Pro: wired battery 0x1f / lighting 0x3f; dongle 0x9f.
        assert_eq!((t(0x0292, Battery), t(0x0292, Brightness)), (0x1f, 0x3f));
        assert_eq!((t(0x0290, Battery), t(0x0290, Brightness)), (0x9f, 0x9f));
        // Mamba Elite: poll rate 0xff, DPI 0x1f.
        assert_eq!((t(0x006c, Poll), t(0x006c, Dpi)), (0xff, 0x1f));
        // Get and set of the poll rate differ on these.
        assert_eq!((t(0x0088, Poll), t(0x0088, PollGet)), (0x1f, 0xff));
        assert_eq!((t(0x0045, Poll), t(0x0045, PollGet)), (0x3f, 0xff));
        assert_eq!(t(0x0091, PollGet), 0x1f);
    }

    /// PIDs OpenRazer (master 477f4d6) sends extended-matrix effects and
    /// brightness to with VARSTORE (`razerkbd_driver.c`, `razermouse_driver.c`).
    const OPENRAZER_VARSTORE_LIGHTING: &[u16] = &[
        // Mice.
        0x0059, 0x005a, 0x005c, 0x0060, 0x0064, 0x0065, 0x0067, 0x006a, 0x006b, 0x006c, 0x006e,
        0x006f, 0x0070, 0x0071, 0x0072, 0x0073, 0x0078, 0x007a, 0x007b, 0x007c, 0x007d, 0x0084,
        0x0085, 0x0086, 0x0088, 0x008a, 0x008c, 0x008d, 0x008f, 0x0090, 0x0091, 0x0096, 0x0098,
        0x0099, 0x00a1, 0x00a3, 0x00a7, 0x00a8, 0x00aa, 0x00ab, 0x00af, 0x00b0, 0x00b9, 0x00c7,
        0x00c8, 0x00cb, 0x00cc, 0x00cd, 0x00d0, 0x00d1, 0x00d3, 0x00d4, 0x00d6, 0x00d7, 0x00da,
        0x00db, // Keyboards.
        0x021e, 0x021f, 0x0226, 0x0227, 0x0228, 0x022a, 0x022b, 0x022c, 0x0235, 0x0237, 0x023f,
        0x0241, 0x0243, 0x0244, 0x024e, 0x0257, 0x0258, 0x025a, 0x025c, 0x025d, 0x025e, 0x0266,
        0x0269, 0x026b, 0x026c, 0x0271, 0x027a, 0x0282, 0x0287, 0x028d, 0x028f, 0x0290, 0x0292,
        0x0293, 0x0294, 0x0295, 0x0296, 0x0298, 0x02a1, 0x02a2, 0x02a3, 0x02a5, 0x02a6, 0x02a7,
        0x02b0, 0x02b9, 0x02ba, 0x02cd, 0x02d5, 0x02d7, 0x0a24,
    ];
    /// PIDs `razer_attr_write_dpi_stages` handles; it always sends VARSTORE.
    const OPENRAZER_VARSTORE_STAGES: &[u16] = &[
        0x0059, 0x005a, 0x0060, 0x0062, 0x0065, 0x006f, 0x0070, 0x0077, 0x0078, 0x007a, 0x007b,
        0x007c, 0x007d, 0x0080, 0x0083, 0x008a, 0x008c, 0x008f, 0x0090, 0x0091, 0x0094, 0x0095,
        0x0096, 0x0099, 0x009a, 0x009c, 0x009e, 0x009f, 0x00a1, 0x00a3, 0x00a5, 0x00a6, 0x00a7,
        0x00a8, 0x00aa, 0x00ab, 0x00af, 0x00b0, 0x00b2, 0x00b3, 0x00b4, 0x00b6, 0x00b7, 0x00b8,
        0x00b9, 0x00be, 0x00bf, 0x00c0, 0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00c7, 0x00c8,
        0x00cb, 0x00cc, 0x00cd, 0x00d0, 0x00d1, 0x00d3, 0x00d4, 0x00d6, 0x00d7, 0x00da, 0x00db,
        0x00de, 0x00df,
    ];

    /// Save mode only stores what OpenRazer stores, with the same command,
    /// for that exact PID.
    #[test]
    fn stored_settings_match_openrazer_varstore_pids() {
        for s in SPECS {
            assert!(
                s.volatile_lighting.is_none() || !s.leds.is_empty(),
                "{}: reason without lighting",
                s.name
            );
            for pid in s.pids {
                if !s.leds.is_empty() && s.volatile_lighting.is_none() {
                    assert!(OPENRAZER_VARSTORE_LIGHTING.contains(pid), "{pid:04x}");
                }
                if s.stages {
                    assert!(OPENRAZER_VARSTORE_STAGES.contains(pid), "{pid:04x}");
                }
            }
        }
        let held = |pid| spec_for(pid).unwrap().volatile_lighting.is_some();
        for pid in [
            0x0099, 0x00cb, 0x00aa, 0x02a6, 0x02a7, 0x02b0, 0x00a7, 0x00d0,
        ] {
            assert!(held(pid), "{pid:04x}");
        }
        assert!(!held(0x0084) && !held(0x0287));
    }

    #[test]
    fn keyboard_poll_rates_and_wave_bytes() {
        let bw = spec_for(0x0287).unwrap();
        assert_eq!(bw.link.poll, KBD_8K);
        let tkl = spec_for(0x02d5).unwrap();
        assert_eq!(tkl.link_for(0x02d5).poll, KBD_1K);
        assert_eq!(tkl.link_for(0x02d7).poll, KBD_1K);
        assert_eq!(spec_for(0x0257).unwrap().wave, Wave::ZeroBased);
        assert_eq!(spec_for(0x02a1).unwrap().wave, Wave::Flipped);
        assert_eq!(spec_for(0x024e).unwrap().wave, Wave::Std);
    }
}
