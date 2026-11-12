//! Razer 90-byte feature report: pure packet builders and parsers (no I/O).
//!
//! Layout (OpenRazer `driver/razercommon.h`, `struct razer_report`):
//!   `[0]` status, `[1]` transaction id, `[2..4]` remaining packets (BE),
//!   `[4]` protocol type, `[5]` data size, `[6]` command class, `[7]` command id,
//!   `[8..88]` arguments, `[88]` XOR of bytes `2..88`, `[89]` reserved.
//! Response status: 0x01 busy, 0x02 ok, 0x03 failure, 0x04 timeout (wireless
//! device asleep), 0x05 not supported.
//!
//! Command ids and argument layouts follow OpenRazer `driver/razerchromacommon.c`
//! (byte layouts only, reimplemented) and the uchroma protocol reference
//! (<https://hyperb1iss.github.io/uchroma/reference/razer-protocol-reference.html>).

use anyhow::{bail, Result};

/// Storage byte: 0x00 applies without saving, 0x01 (VARSTORE) is kept by the
/// firmware across power cycles (OpenRazer issue #1290). On models with
/// onboard profiles it is the profile slot, so VARSTORE is profile 1
/// (`razer_get_active_varstore` in OpenRazer `razerkbd_driver.c`; uchroma:
/// "0x00 = no-store, 0x01 = default/white"). See docs/onboard-writes/razer.md.
pub const NOSTORE: u8 = 0x00;
pub const VARSTORE: u8 = 0x01;

/// LED ids (OpenRazer `razercommon.h`). ZERO addresses a mouse's whole matrix.
pub const ZERO_LED: u8 = 0x00;
pub const SCROLL_LED: u8 = 0x01;
pub const LOGO_LED: u8 = 0x04;
pub const BACKLIGHT_LED: u8 = 0x05;
pub const RIGHT_SIDE_LED: u8 = 0x10;
pub const LEFT_SIDE_LED: u8 = 0x11;
const GAME_LED: u8 = 0x08;

/// `(command class, command id, arguments)`; the data size is `arguments.len()`.
pub type Cmd = (u8, u8, Vec<u8>);

pub fn make_report(transaction: u8, class: u8, cmd: u8, args: &[u8]) -> [u8; 90] {
    let mut report = [0u8; 90];
    report[0] = 0x00;
    report[1] = transaction;
    report[2] = 0x00;
    report[3] = 0x00;
    report[4] = 0x00;
    let data_size = args.len().min(80) as u8;
    report[5] = data_size;
    report[6] = class;
    report[7] = cmd;
    report[8..8 + data_size as usize].copy_from_slice(&args[..data_size as usize]);
    let mut crc = 0u8;
    for b in &report[2..88] {
        crc ^= *b;
    }
    report[88] = crc;
    report[89] = 0;
    report
}

/// The answer's XOR checksum (byte 88 over bytes 2..88) holds.
pub fn crc_ok(resp: &[u8; 90]) -> bool {
    resp[2..88].iter().fold(0, |crc, b| crc ^ b) == resp[88]
}

/// OpenRazer razer_send_payload acceptance: same class/cmd, status busy or ok.
pub fn check_response(req: &[u8; 90], resp: &[u8; 90]) -> Result<()> {
    let (class, cmd) = (req[6], req[7]);
    if resp[6] != class || resp[7] != cmd {
        bail!(
            "razer response mismatch class={:#04x} cmd={:#04x} (sent {class:#04x}/{cmd:#04x})",
            resp[6],
            resp[7]
        );
    }
    if !matches!(resp[0], 0x01 | 0x02) {
        bail!(
            "razer command failed status={:#04x} class={class:#04x} cmd={cmd:#04x}",
            resp[0]
        );
    }
    Ok(())
}

/// Firmware effects, in the order the UI lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Off,
    Static,
    Spectrum,
    Wave,
    Reactive,
    Breathing,
    Starlight,
}

impl Effect {
    pub const ALL: [Effect; 7] = [
        Self::Off,
        Self::Static,
        Self::Spectrum,
        Self::Wave,
        Self::Reactive,
        Self::Breathing,
        Self::Starlight,
    ];

    pub fn from_str(s: &str) -> Self {
        match s {
            "off" => Self::Off,
            "spectrum" => Self::Spectrum,
            "wave" => Self::Wave,
            "reactive" => Self::Reactive,
            "breathing" => Self::Breathing,
            "starlight" => Self::Starlight,
            _ => Self::Static,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Static => "static",
            Self::Spectrum => "spectrum",
            Self::Wave => "wave",
            Self::Reactive => "reactive",
            Self::Breathing => "breathing",
            Self::Starlight => "starlight",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Static => "Static",
            Self::Spectrum => "Spectrum",
            Self::Wave => "Wave",
            Self::Reactive => "Reactive",
            Self::Breathing => "Breathing",
            Self::Starlight => "Starlight",
        }
    }

    /// Bit in a model's supported-effects mask.
    pub const fn bit(self) -> u8 {
        1 << self as u8
    }

    pub fn has_color(self) -> bool {
        matches!(
            self,
            Self::Static | Self::Breathing | Self::Reactive | Self::Starlight
        )
    }

    /// Effects whose argument bytes carry a speed (wave speed is fixed 0x28).
    pub fn has_speed(self) -> bool {
        matches!(self, Self::Reactive | Self::Starlight)
    }
}

/// Wave direction byte: OpenRazer passes userspace 1 (left) / 2 (right)
/// through; Ornata V3 firmware has them swapped (`direction ^= 3`).
pub fn wave_dir(direction: &str, flipped: bool) -> u8 {
    let d = match direction {
        "right" => 0x02,
        _ => 0x01,
    };
    if flipped {
        d ^ 0x03
    } else {
        d
    }
}

/// Wave direction for firmware taking 0x00/0x01 (OpenRazer daemon
/// `WAVE_DIRS = (0, 1)`): the daemon passes 1 through and turns 2 into 0.
pub fn wave_dir_zero_based(direction: &str) -> u8 {
    match direction {
        "right" => 0x00,
        _ => 0x01,
    }
}

/// Map the normalized speed to this effect's device speed byte.
/// Reactive range 0x01..=0x04, Starlight range 0x01..=0x03; lower = faster.
pub fn speed_byte(effect: Effect, speed: &str) -> u8 {
    match effect {
        Effect::Reactive => match speed {
            "slow" => 0x04,
            "fast" => 0x01,
            _ => 0x02,
        },
        Effect::Starlight => match speed {
            "slow" => 0x03,
            "fast" => 0x01,
            _ => 0x02,
        },
        _ => 0x02,
    }
}

/// Argument bytes for the extended-matrix effect command (class 0x0f, cmd 0x02):
/// `[store, led, effect id, ...]`, single-colour variants.
pub fn effect_args(
    store: u8,
    led: u8,
    effect: Effect,
    [r, g, b]: [u8; 3],
    speed: &str,
    wave_dir: u8,
) -> Vec<u8> {
    let spd = speed_byte(effect, speed);
    match effect {
        Effect::Off => vec![store, led, 0x00, 0x00, 0x00, 0x00],
        Effect::Static => vec![store, led, 0x01, 0x00, 0x00, 0x01, r, g, b],
        Effect::Breathing => vec![store, led, 0x02, 0x01, 0x00, 0x01, r, g, b],
        Effect::Spectrum => vec![store, led, 0x03, 0x00, 0x00, 0x00],
        Effect::Wave => vec![store, led, 0x04, wave_dir, 0x28, 0x00],
        Effect::Reactive => vec![store, led, 0x05, 0x00, spd, 0x01, r, g, b],
        Effect::Starlight => vec![store, led, 0x07, 0x00, spd, 0x01, r, g, b],
    }
}

pub fn set_effect(
    store: u8,
    led: u8,
    effect: Effect,
    rgb: [u8; 3],
    speed: &str,
    wave_dir: u8,
) -> Cmd {
    (
        0x0f,
        0x02,
        effect_args(store, led, effect, rgb, speed, wave_dir),
    )
}

/// Percent (0..=100) to the device's 0..=255 brightness byte.
pub fn brightness_wire(pct: u8) -> u8 {
    ((pct.min(100) as u16) * 255 / 100) as u8
}

/// Extended-matrix brightness (class 0x0f, cmd 0x04).
pub fn set_brightness(store: u8, led: u8, pct: u8) -> Cmd {
    (0x0f, 0x04, vec![store, led, brightness_wire(pct)])
}

/// Extended-matrix brightness read (class 0x0f, cmd 0x84); the answer's
/// argument `[2]` is the 0..=255 value.
pub fn get_brightness(store: u8, led: u8) -> Cmd {
    (0x0f, 0x84, vec![store, led, 0])
}

/// Extended-matrix effect read (class 0x0f, cmd 0x82, size 0x0c); the answer
/// carries the `set_effect` arguments (freenapse `src/lighting.rs`).
pub fn get_effect(store: u8, led: u8) -> Cmd {
    let mut args = vec![0; 0x0c];
    args[..2].copy_from_slice(&[store, led]);
    (0x0f, 0x82, args)
}

/// Keyboard game mode = state of the game LED (class 0x03, cmd 0x00).
pub fn set_game_mode(store: u8, on: bool) -> Cmd {
    (0x03, 0x00, vec![store, GAME_LED, u8::from(on)])
}

/// Game LED state read (class 0x03, cmd 0x80); the answer's argument `[2]`
/// is the state (OpenRazer `razer_attr_read_game_led_state`).
pub fn get_game_mode(store: u8) -> Cmd {
    (0x03, 0x80, vec![store, GAME_LED, 0])
}

/// Mouse DPI, same value on both axes (class 0x04, cmd 0x05, size 7).
pub fn set_dpi(store: u8, dpi: u16) -> Cmd {
    let [hi, lo] = dpi.to_be_bytes();
    (0x04, 0x05, vec![store, hi, lo, hi, lo, 0x00, 0x00])
}

pub fn get_dpi(store: u8) -> Cmd {
    (0x04, 0x85, vec![store, 0, 0, 0, 0, 0, 0])
}

/// X DPI from a get-DPI response (arguments `[1..3]`, big-endian).
pub fn parse_dpi(resp: &[u8; 90]) -> u16 {
    u16::from_be_bytes([resp[9], resp[10]])
}

/// Most DPI stages a mouse holds (OpenRazer `RAZER_MOUSE_MAX_DPI_STAGES`).
pub const MAX_DPI_STAGES: usize = 5;

/// DPI stages (class 0x04, cmd 0x06, size 0x26): `[store, active stage
/// (1-based), count]`, then 7 bytes per stage: index (0-based), X DPI BE,
/// Y DPI BE, 2 reserved. Same DPI on both axes.
pub fn set_dpi_stages(store: u8, active: u8, dpis: &[u16]) -> Cmd {
    let dpis = &dpis[..dpis.len().min(MAX_DPI_STAGES)];
    let mut args = vec![0u8; 0x26];
    args[..3].copy_from_slice(&[store, active, dpis.len() as u8]);
    for (i, &dpi) in dpis.iter().enumerate() {
        let [hi, lo] = dpi.to_be_bytes();
        args[3 + i * 7..8 + i * 7].copy_from_slice(&[i as u8, hi, lo, hi, lo]);
    }
    (0x04, 0x06, args)
}

pub fn get_dpi_stages(store: u8) -> Cmd {
    let mut args = vec![0u8; 0x26];
    args[0] = store;
    (0x04, 0x86, args)
}

/// (active stage, X DPI of each stage) from a get-stages response; stages
/// past the response's data size are dropped, as OpenRazer does.
pub fn parse_dpi_stages(resp: &[u8; 90]) -> (u8, Vec<u16>) {
    let (args, size) = (&resp[8..88], usize::from(resp[5]).min(80));
    let count = usize::from(args[2]).min(MAX_DPI_STAGES);
    let dpis = (0..count)
        .map(|i| 4 + i * 7)
        .take_while(|&off| off + 4 <= size)
        .map(|off| u16::from_be_bytes([args[off], args[off + 1]]))
        .collect();
    (args[1], dpis)
}

/// Classic poll-rate byte (class 0x00, cmd 0x05 / 0x85).
const POLL_CLASSIC: [(u32, u8); 3] = [(1000, 0x01), (500, 0x02), (125, 0x08)];
/// HyperPolling byte (class 0x00, cmd 0x40 / 0xc0).
const POLL_V2: [(u32, u8); 7] = [
    (8000, 0x01),
    (4000, 0x02),
    (2000, 0x04),
    (1000, 0x08),
    (500, 0x10),
    (250, 0x20),
    (125, 0x40),
];

fn poll_byte(table: &[(u32, u8)], hz: u32) -> Option<u8> {
    table.iter().find(|(h, _)| *h == hz).map(|&(_, b)| b)
}

fn poll_hz(table: &[(u32, u8)], byte: u8) -> Option<u32> {
    table.iter().find(|(_, b)| *b == byte).map(|&(h, _)| h)
}

pub fn set_poll(hz: u32) -> Option<Cmd> {
    poll_byte(&POLL_CLASSIC, hz).map(|b| (0x00, 0x05, vec![b]))
}

pub fn get_poll() -> Cmd {
    (0x00, 0x85, vec![0])
}

pub fn parse_poll(resp: &[u8; 90]) -> Option<u32> {
    poll_hz(&POLL_CLASSIC, resp[8])
}

/// HyperPolling set; Razer sends it once with `arg` 0x00 and once with 0x01.
pub fn set_poll2(arg: u8, hz: u32) -> Option<Cmd> {
    poll_byte(&POLL_V2, hz).map(|b| (0x00, 0x40, vec![arg, b]))
}

pub fn get_poll2() -> Cmd {
    (0x00, 0xc0, vec![0])
}

pub fn parse_poll2(resp: &[u8; 90]) -> Option<u32> {
    poll_hz(&POLL_V2, resp[9])
}

/// Battery level (class 0x07, cmd 0x80): argument `[1]` is 0..=255.
pub fn get_battery() -> Cmd {
    (0x07, 0x80, vec![0, 0])
}

pub fn parse_battery(resp: &[u8; 90]) -> u8 {
    ((resp[9] as u16) * 100 / 255) as u8
}

/// Charging status (class 0x07, cmd 0x84): argument `[1]` is 1 while charging.
pub fn get_charging() -> Cmd {
    (0x07, 0x84, vec![0, 0])
}

pub fn parse_charging(resp: &[u8; 90]) -> bool {
    resp[9] == 0x01
}

/// Idle time before power save, seconds BE, firmware range 60..=900
/// (class 0x07, cmd 0x03 / 0x83).
pub fn set_idle(secs: u16) -> Cmd {
    let [hi, lo] = secs.clamp(60, 900).to_be_bytes();
    (0x07, 0x03, vec![hi, lo])
}

pub fn get_idle() -> Cmd {
    (0x07, 0x83, vec![0, 0])
}

pub fn parse_idle(resp: &[u8; 90]) -> u16 {
    u16::from_be_bytes([resp[8], resp[9]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(txn: u8, (class, cmd, args): Cmd) -> [u8; 90] {
        make_report(txn, class, cmd, &args)
    }

    fn resp_with(args: &[u8]) -> [u8; 90] {
        let mut r = [0u8; 90];
        r[0] = 0x02;
        r[8..8 + args.len()].copy_from_slice(args);
        r
    }

    #[test]
    fn report_header_and_crc() {
        // razerchromacommon.c comment vector: 00 3f 0000 00 03 0f 04 0104b7
        let r = report(0x3f, (0x0f, 0x04, vec![0x01, 0x04, 0xb7]));
        assert_eq!(
            &r[..11],
            &[0, 0x3f, 0, 0, 0, 0x03, 0x0f, 0x04, 0x01, 0x04, 0xb7]
        );
        assert!(r[11..88].iter().all(|&b| b == 0));
        assert_eq!(r[88], 0x03 ^ 0x0f ^ 0x04 ^ 0x01 ^ 0x04 ^ 0xb7);
        assert_eq!(r[89], 0);
    }

    #[test]
    fn effect_args_match_openrazer_vectors() {
        // Vectors from razerchromacommon.c comments (VARSTORE, BACKLIGHT_LED).
        let e = |fx, rgb, speed| effect_args(VARSTORE, BACKLIGHT_LED, fx, rgb, speed, 0x01);
        assert_eq!(
            e(Effect::Off, [0; 3], "medium"),
            [0x01, 0x05, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            e(Effect::Static, [0xff, 0, 0], "medium"),
            [0x01, 0x05, 0x01, 0x00, 0x00, 0x01, 0xff, 0x00, 0x00]
        );
        assert_eq!(
            e(Effect::Breathing, [0, 0xff, 0], "medium"),
            [0x01, 0x05, 0x02, 0x01, 0x00, 0x01, 0x00, 0xff, 0x00]
        );
        assert_eq!(
            e(Effect::Spectrum, [0; 3], "medium"),
            [0x01, 0x05, 0x03, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            e(Effect::Wave, [0; 3], "medium"),
            [0x01, 0x05, 0x04, 0x01, 0x28, 0x00]
        );
        assert_eq!(
            e(Effect::Reactive, [0xff, 0, 0], "slow"),
            [0x01, 0x05, 0x05, 0x00, 0x04, 0x01, 0xff, 0x00, 0x00]
        );
        assert_eq!(
            e(Effect::Starlight, [0xff, 0, 0], "slow"),
            [0x01, 0x05, 0x07, 0x00, 0x03, 0x01, 0xff, 0x00, 0x00]
        );
        // Volatile store byte + mouse logo LED.
        assert_eq!(
            set_effect(NOSTORE, LOGO_LED, Effect::Static, [1, 2, 3], "medium", 1),
            (
                0x0f,
                0x02,
                vec![0x00, 0x04, 0x01, 0x00, 0x00, 0x01, 1, 2, 3]
            )
        );
    }

    #[test]
    fn wave_direction_and_flip() {
        assert_eq!(wave_dir("left", false), 0x01);
        assert_eq!(wave_dir("right", false), 0x02);
        assert_eq!(wave_dir("left", true), 0x02);
        assert_eq!(wave_dir("right", true), 0x01);
    }

    #[test]
    fn effect_mask_and_names_roundtrip() {
        let mut seen = 0u8;
        for fx in Effect::ALL {
            assert_eq!(Effect::from_str(fx.as_str()), fx);
            assert_eq!(seen & fx.bit(), 0);
            seen |= fx.bit();
        }
        assert_eq!(seen, 0x7f);
    }

    #[test]
    fn misc_commands_bytes() {
        assert_eq!(
            set_brightness(NOSTORE, ZERO_LED, 100),
            (0x0f, 0x04, vec![0, 0, 0xff])
        );
        assert_eq!(
            set_brightness(VARSTORE, BACKLIGHT_LED, 50),
            (0x0f, 0x04, vec![1, 5, 127])
        );
        assert_eq!(
            set_game_mode(NOSTORE, true),
            (0x03, 0x00, vec![0x00, 0x08, 0x01])
        );
        assert_eq!(
            get_game_mode(VARSTORE),
            (0x03, 0x80, vec![0x01, 0x08, 0x00])
        );
        let (class, cmd, args) = get_effect(VARSTORE, LOGO_LED);
        assert_eq!(
            (class, cmd, args.len(), &args[..3]),
            (0x0f, 0x82, 12, &[1, 4, 0][..])
        );
        // 1600 dpi = 0x0640 on both axes, 7 argument bytes.
        assert_eq!(
            set_dpi(NOSTORE, 1600),
            (0x04, 0x05, vec![0x00, 0x06, 0x40, 0x06, 0x40, 0x00, 0x00])
        );
        assert_eq!(get_dpi(NOSTORE), (0x04, 0x85, vec![0; 7]));
        assert_eq!(set_poll(1000), Some((0x00, 0x05, vec![0x01])));
        assert_eq!(set_poll(125), Some((0x00, 0x05, vec![0x08])));
        assert_eq!(set_poll(2000), None);
        assert_eq!(set_poll2(0x00, 8000), Some((0x00, 0x40, vec![0x00, 0x01])));
        assert_eq!(set_poll2(0x01, 250), Some((0x00, 0x40, vec![0x01, 0x20])));
        assert_eq!(get_poll2(), (0x00, 0xc0, vec![0]));
        assert_eq!(get_battery(), (0x07, 0x80, vec![0, 0]));
        assert_eq!(get_charging(), (0x07, 0x84, vec![0, 0]));
        // 300 s = 0x012c; out-of-range values clamp to the firmware's 60..=900.
        assert_eq!(set_idle(300), (0x07, 0x03, vec![0x01, 0x2c]));
        assert_eq!(set_idle(10), (0x07, 0x03, vec![0x00, 0x3c]));
        assert_eq!(set_idle(5000), (0x07, 0x03, vec![0x03, 0x84]));
    }

    #[test]
    fn response_parsers() {
        assert_eq!(parse_dpi(&resp_with(&[0x00, 0x06, 0x40, 0x06, 0x40])), 1600);
        assert_eq!(parse_poll(&resp_with(&[0x02])), Some(500));
        assert_eq!(parse_poll(&resp_with(&[0x07])), None);
        assert_eq!(parse_poll2(&resp_with(&[0x00, 0x04])), Some(2000));
        assert_eq!(parse_battery(&resp_with(&[0x00, 0xff])), 100);
        assert_eq!(parse_battery(&resp_with(&[0x00, 0x80])), 50);
        assert!(parse_charging(&resp_with(&[0x00, 0x01])));
        assert!(!parse_charging(&resp_with(&[0x00, 0x00])));
        assert_eq!(parse_idle(&resp_with(&[0x03, 0x84])), 900);
    }

    #[test]
    fn dpi_stage_bytes_match_openrazer_layout() {
        // razer_attr_write_dpi_stages example: stages 800 / 1800 / 3600, second
        // active; razer_chroma_misc_set_dpi_stages numbers stages from 0.
        let (class, cmd, args) = set_dpi_stages(NOSTORE, 2, &[800, 1800, 3600]);
        assert_eq!((class, cmd, args.len()), (0x04, 0x06, 0x26));
        assert_eq!(
            &args[..24],
            &[
                0x00, 0x02, 0x03, // store, active, count
                0x00, 0x03, 0x20, 0x03, 0x20, 0x00, 0x00, // stage 0: 800
                0x01, 0x07, 0x08, 0x07, 0x08, 0x00, 0x00, // stage 1: 1800
                0x02, 0x0e, 0x10, 0x0e, 0x10, 0x00, 0x00, // stage 2: 3600
            ]
        );
        assert!(args[24..].iter().all(|&b| b == 0));
        // At most five stages are sent.
        assert_eq!(set_dpi_stages(NOSTORE, 1, &[400; 7]).2[2], 5);
        assert_eq!(get_dpi_stages(NOSTORE), (0x04, 0x86, vec![0; 0x26]));

        // razer_attr_read_dpi_stages response comment: varstore, active 2,
        // 4 stages, then (stage no., X, Y, reserved) per stage.
        let mut resp = resp_with(&[
            0x01, 0x02, 0x04, 0x01, 0x03, 0x20, 0x03, 0x20, 0x00, 0x00, 0x02, 0x07, 0x08, 0x07,
            0x08, 0x00, 0x00, 0x03, 0x0e, 0x10, 0x0c, 0x80, 0x00, 0x00, 0x04, 0x19, 0x00, 0x19,
            0x00, 0x00, 0x00,
        ]);
        resp[5] = 0x26;
        assert_eq!(parse_dpi_stages(&resp), (2, vec![800, 1800, 3600, 6400]));
        // A short data size cuts the list.
        resp[5] = 15;
        assert_eq!(parse_dpi_stages(&resp), (2, vec![800, 1800]));
    }

    #[test]
    fn zero_based_wave_direction() {
        assert_eq!(wave_dir_zero_based("left"), 0x01);
        assert_eq!(wave_dir_zero_based("right"), 0x00);
    }

    #[test]
    fn response_check_accepts_busy_and_ok_only() {
        let r = report(0x1f, get_battery());
        let mut resp = r;
        for (status, ok) in [
            (0x01, true),
            (0x02, true),
            (0x03, false),
            (0x04, false),
            (0x05, false),
        ] {
            resp[0] = status;
            assert_eq!(check_response(&r, &resp).is_ok(), ok, "status {status:#x}");
        }
        resp[0] = 0x02;
        resp[7] = 0x84;
        assert!(check_response(&r, &resp).is_err());
    }
}
