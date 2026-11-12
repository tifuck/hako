//! SteelSeries model registry: PIDs plus the protocol data each model needs.
//!
//! PIDs: headsets from HeadsetControl `lib/devices/steelseries_*.hpp`
//! (Arctis 1 Cyberpunk, Nova 5X White, Nova 7 WoW Gen 2 firmware and
//! GameBuds X from Linux-Arctis-Manager `daemon/device-configs/*.yaml`, same
//! protocol as their siblings there), mice from rivalcfg
//! `rivalcfg/devices/*.py`, keyboards from OpenRGB `SteelSeriesDevices.h` /
//! `SteelSeriesControllerDetect.cpp`. The USB interface of each model is the
//! one those projects open. Arctis Nova Pro Wireless X (white) `225d` and the
//! wired Arctis Nova Pro `12cb` / `12fa` / `12cd` from Linux-Arctis-Manager
//! `nova_pro_wireless.yaml` / `nova_pro_wired.yaml` (`225d`, `12cb`, `12cd`
//! also listed by linux-hardware.org).

use super::headset::{self, ChatMix, Family, HeadsetSpec, Save};
use super::keyboard::{self, Gen, KbProto, KeyboardSpec, APEX9_LEN, APEX_LEN};
use super::mouse::{
    self, Battery, Brightness, Codes, Dpi, Gradient, Light, MouseSave, MouseSpec, Rate, Sensor,
    Sleep, Zones, RANGE_12000,
};
use super::{nova_pro, VID};
use crate::drivers::{DriverCtx, Model, Opened};
use anyhow::Context;
use hako_core::device::DeviceKind;

pub(super) enum Spec {
    Headset(HeadsetSpec),
    Mouse(MouseSpec),
    Keyboard(KeyboardSpec),
}
use Spec::{Headset as H, Keyboard as K, Mouse as M};

macro_rules! models {
    ($($kind:ident $name:literal [$($pid:literal),+] $spec:expr;)*) => {
        pub const MODELS: &[Model] = &[
            Model {
                vendor_id: VID,
                product_ids: &[0x12e5],
                kind: DeviceKind::Headset,
                name: "Arctis Nova Pro Wireless X",
                usbfs: false,
                multi: false,
                open: nova_pro::open,
            },
            Model {
                vendor_id: VID,
                product_ids: &[0x12e0],
                kind: DeviceKind::Headset,
                name: "Arctis Nova Pro Wireless",
                usbfs: false,
                multi: false,
                open: nova_pro::open,
            },
            Model {
                vendor_id: VID,
                product_ids: &[0x225d],
                kind: DeviceKind::Headset,
                name: "Arctis Nova Pro Wireless X (White)",
                usbfs: false,
                multi: false,
                open: nova_pro::open,
            },
            $(Model {
                vendor_id: VID,
                product_ids: &[$($pid),+],
                kind: DeviceKind::$kind,
                name: $name,
                usbfs: false,
                multi: false,
                open,
            },)*
        ];

        /// Protocol data for every model after the Nova Pro Wireless, in order.
        const SPECS: &[(&[u16], Spec)] = &[$((&[$($pid),+], $spec),)*];
    };
}

// ── Headsets (HeadsetControl) ───────────────────────────────────────────────

/// Auto-off choices (minutes) where any value up to 90 is accepted.
const OFF_90: &[u8] = &[0, 5, 10, 15, 30, 45, 60, 90];

/// Host-mixed ChatMix targets. PipeWire names USB sinks after the product
/// string (`Arctis 7+` -> `alsa_output.usb-SteelSeries_Arctis_7_-00...`).
const SINKS_ARCTIS7_PLUS: &[&str] = &[
    r"alsa_output\.usb-SteelSeries_Arctis_7_-.*analog",
    r"SteelSeries_Arctis_7_-",
];
const SINKS_NOVA7: &[&str] = &[
    r"alsa_output\.usb-SteelSeries_Arctis_Nova_7.*analog",
    r"SteelSeries_Arctis_Nova_7",
];
const SINKS_NOVA5: &[&str] = &[
    r"alsa_output\.usb-SteelSeries_Arctis_Nova_5.*analog",
    r"SteelSeries_Arctis_Nova_5",
];

/// Arctis 7 / Pro: separate Game and Chat outputs, mixed by the headset.
const ARCTIS7: HeadsetSpec = HeadsetSpec {
    interface: 5,
    family: Family::Arctis7,
    battery: true,
    chatmix: ChatMix::Dial,
    sidetone: Some(0x12),
    mic_volume: None,
    mic_led: false,
    auto_off: OFF_90,
    eq: None,
    volume_limiter: false,
    bt_call: false,
    save: Save::Legacy,
};
/// Wired Arctis Pro: same protocol, no battery.
const ARCTIS_PRO: HeadsetSpec = HeadsetSpec {
    battery: false,
    save: Save::Blocked(
        "Linux-Arctis-Manager drives this model with the Arctis 5 protocol, not the Arctis 7 one",
    ),
    ..ARCTIS7
};
const ARCTIS1: HeadsetSpec = HeadsetSpec {
    interface: 3,
    family: Family::Arctis1,
    chatmix: ChatMix::None,
    ..ARCTIS7
};
/// Only Linux-Arctis-Manager lists these PIDs.
const ARCTIS1_CYBERPUNK: HeadsetSpec = HeadsetSpec {
    save: Save::Blocked("only Linux-Arctis-Manager sends 06 09 to this PID"),
    ..ARCTIS1
};
const ARCTIS9: HeadsetSpec = HeadsetSpec {
    interface: 0,
    family: Family::Arctis9,
    sidetone: Some(0xfd - 0xc0),
    save: Save::Blocked(
        "sources disagree: HeadsetControl sends 90 00, Linux-Arctis-Manager 00 09 00",
    ),
    ..ARCTIS7
};
const PRO_WIRELESS: HeadsetSpec = HeadsetSpec {
    interface: 0,
    family: Family::ProWireless,
    chatmix: ChatMix::None,
    sidetone: Some(9),
    auto_off: &[0, 10, 20, 30, 60, 90],
    save: Save::Blocked(
        "sources disagree: HeadsetControl sends 90 aa, Linux-Arctis-Manager 00 05 aa 00",
    ),
    ..ARCTIS7
};
const ARCTIS7_PLUS: HeadsetSpec = HeadsetSpec {
    interface: 3,
    family: Family::Arctis7Plus,
    chatmix: ChatMix::Host(SINKS_ARCTIS7_PLUS),
    sidetone: Some(3),
    mic_volume: Some(7),
    mic_led: true,
    eq: Some(&headset::EQ_ARCTIS7_PLUS),
    save: Save::Nova,
    ..ARCTIS7
};
/// Xbox SKU: HeadsetControl lists a dial, Linux-Arctis-Manager says it has
/// none, so ChatMix is left out.
const ARCTIS7X_PLUS: HeadsetSpec = HeadsetSpec {
    chatmix: ChatMix::None,
    ..ARCTIS7_PLUS
};
/// PlayStation SKU: as the Xbox one, and sidetone is disputed the same way.
const ARCTIS7P_PLUS: HeadsetSpec = HeadsetSpec {
    sidetone: None,
    ..ARCTIS7X_PLUS
};
/// No EQ: HeadsetControl writes 1 dB per step, Linux-Arctis-Manager 0.5 dB
/// (and a parametric frame on Gen 2 firmware).
const NOVA7: HeadsetSpec = HeadsetSpec {
    family: Family::Nova7,
    chatmix: ChatMix::Host(SINKS_NOVA7),
    mic_volume: Some(7),
    mic_led: true,
    eq: None,
    volume_limiter: true,
    bt_call: true,
    save: Save::Blocked(
        "HeadsetControl saves only the Gen 2 (227e); 00 09 here rests on Linux-Arctis-Manager and an unmerged kernel patch",
    ),
    ..ARCTIS7_PLUS
};
/// The 7P dial drives sidetone in hardware; neither sidetone nor ChatMix work.
const NOVA7P: HeadsetSpec = HeadsetSpec {
    chatmix: ChatMix::None,
    sidetone: None,
    ..NOVA7
};
const NOVA5: HeadsetSpec = HeadsetSpec {
    family: Family::Nova5,
    chatmix: ChatMix::Host(SINKS_NOVA5),
    sidetone: Some(10),
    mic_volume: Some(15),
    eq: Some(&headset::EQ_NOVA5),
    bt_call: false,
    save: Save::Blocked(
        "sources disagree: HeadsetControl adds 00 35 01 after 00 09, Linux-Arctis-Manager has no save",
    ),
    ..NOVA7
};
/// HeadsetControl reads no dial here; Linux-Arctis-Manager lists one.
const NOVA3P: HeadsetSpec = HeadsetSpec {
    chatmix: ChatMix::None,
    mic_led: false,
    auto_off: &[0, 1, 5, 10, 15, 30, 45, 60, 75, 90],
    eq: Some(&headset::EQ_NOVA3P),
    volume_limiter: false,
    save: Save::Blocked(
        "sources disagree on the report type (output vs feature) and the output path is untested on Linux",
    ),
    ..NOVA5
};
const NOVA3: HeadsetSpec = HeadsetSpec {
    interface: 4,
    family: Family::Nova3,
    battery: false,
    chatmix: ChatMix::None,
    sidetone: Some(3),
    mic_volume: Some(10),
    mic_led: true,
    auto_off: &[],
    eq: Some(&headset::EQ_NOVA3),
    volume_limiter: false,
    bt_call: false,
    save: Save::Blocked(
        "sources disagree: HeadsetControl sends 06 09 as a feature report, Linux-Arctis-Manager as an output report",
    ),
};
/// Wired GameDAC: volatile mic level and sidetone only.
const NOVA_PRO_WIRED: HeadsetSpec = HeadsetSpec {
    interface: 4,
    family: Family::NovaProWired,
    battery: false,
    chatmix: ChatMix::None,
    sidetone: Some(3),
    mic_volume: Some(10),
    mic_led: false,
    auto_off: &[],
    eq: None,
    volume_limiter: false,
    bt_call: false,
    save: Save::Blocked("only Linux-Arctis-Manager sends 06 09 to the wired GameDAC"),
};
const GAMEBUDS: HeadsetSpec = HeadsetSpec {
    interface: 3,
    family: Family::GameBuds,
    battery: true,
    chatmix: ChatMix::None,
    sidetone: None,
    mic_volume: None,
    mic_led: false,
    auto_off: &[],
    eq: None,
    volume_limiter: false,
    bt_call: false,
    save: Save::None,
};

// ── Mice (rivalcfg) ─────────────────────────────────────────────────────────

const RATE_1: Rate = Rate {
    cmd: &[0x2b],
    zero_based: false,
};
const RATE_0: Rate = Rate {
    cmd: &[0x2b],
    zero_based: true,
};

/// Save commands only rivalcfg sends for that model (see the dossier).
const ONLY_RIVALCFG: MouseSave =
    MouseSave::Blocked("only rivalcfg sends the save command for this model");
/// Aerox / Prime Wireless / Prime Mini / Rival 3 Gen 2 `11 00`.
const SAVE_11: MouseSave =
    MouseSave::Blocked("only rivalcfg sends 11 00; OpenRGB copies it and notes it is unconfirmed");
/// OpenRGB carries the bytes but marks saving unsupported on these.
const ONLY_RIVALCFG_OPENRGB_X: MouseSave = MouseSave::Blocked(
    "only rivalcfg confirms the save for this model; OpenRGB's copy is marked unsupported",
);

const RIVAL3: MouseSpec = MouseSpec {
    interface: 3,
    wireless: &[],
    dpi: Dpi::Table {
        cmd: &[0x0b, 0x00],
        sensor: Sensor::TrueMoveCore,
        first: 1,
        len: 1,
    },
    rate: Some(Rate {
        cmd: &[0x04, 0x00],
        zero_based: false,
    }),
    light: Light {
        zones: Zones::Rival3,
        brightness: Brightness::Inline,
        // rainbow-shift / steady effect ids.
        rainbow: Some(&[0x06, 0x00, 0x00]),
        steady: &[&[0x06, 0x00, 0x04]],
    },
    battery: None,
    sleep: None,
    packet_len: 0,
    save: MouseSave::Command(&[0x09, 0x00]),
};
const RIVAL3_WIRELESS: MouseSpec = MouseSpec {
    dpi: Dpi::Table {
        cmd: &[0x20],
        sensor: Sensor::TrueMoveAir,
        first: 1,
        len: 2,
    },
    rate: Some(Rate {
        cmd: &[0x17],
        zero_based: true,
    }),
    light: Light::NONE,
    battery: Some(Battery::Rival),
    save: MouseSave::Blocked("OpenRGB deliberately disables saving on the Rival 3 Wireless"),
    ..RIVAL3
};
const AEROX3: MouseSpec = MouseSpec {
    dpi: Dpi::Table {
        cmd: &[0x2d],
        sensor: Sensor::TrueMoveCore,
        first: 1,
        len: 1,
    },
    rate: Some(RATE_1),
    light: Light {
        zones: Zones::Mask {
            count: 3,
            wide: false,
        },
        brightness: Brightness::Pct,
        rainbow: Some(&[0x22, 0b111]),
        steady: &[],
    },
    save: SAVE_11,
    ..RIVAL3
};
const AEROX5: MouseSpec = MouseSpec {
    dpi: Dpi::Table {
        cmd: &[0x2d],
        sensor: Sensor::TrueMoveAir,
        first: 0,
        len: 1,
    },
    ..AEROX3
};
/// Aerox 3/5/9 Wireless and Prime Wireless share one command set; set
/// `wireless` to the model's dongle PIDs.
const AEROX_WIRELESS: MouseSpec = MouseSpec {
    rate: Some(RATE_0),
    light: Light {
        zones: Zones::Indexed(3),
        brightness: Brightness::None,
        rainbow: Some(&[0x22, 0xff]),
        steady: &[],
    },
    battery: Some(Battery::Aerox),
    // `29` + milliseconds, 0 = never.
    sleep: Some(Sleep {
        cmd: &[0x29],
        min: 0,
        unit: 60_000,
        len: 3,
    }),
    ..AEROX5
};
const PRIME_WIRELESS: MouseSpec = MouseSpec {
    light: Light {
        zones: Zones::Fixed {
            prefix: &[0x21, 0x01, 0x00],
            suffix: &[],
        },
        brightness: Brightness::None,
        rainbow: None,
        steady: &[],
    },
    ..AEROX_WIRELESS
};
const RIVAL5: MouseSpec = MouseSpec {
    interface: 0,
    light: Light {
        zones: Zones::Mask {
            count: 10,
            wide: true,
        },
        brightness: Brightness::Steps,
        rainbow: Some(&[0x22, 0xff, 0x03]),
        steady: &[],
    },
    save: MouseSave::Blocked(
        "sources disagree: rivalcfg sends 11 00, OpenRGB says the Rival 5 has no save",
    ),
    ..AEROX5
};
const PRIME: MouseSpec = MouseSpec {
    interface: 0,
    wireless: &[],
    dpi: Dpi::Step50 {
        cmd: &[0x61],
        first: 0,
        flags: false,
    },
    rate: Some(Rate {
        cmd: &[0x5d],
        zero_based: false,
    }),
    light: Light {
        zones: Zones::Fixed {
            prefix: &[0x62, 0x01],
            suffix: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff],
        },
        brightness: Brightness::Prime,
        rainbow: None,
        steady: &[],
    },
    battery: None,
    sleep: None,
    packet_len: 0,
    save: ONLY_RIVALCFG,
};
const PRIME_MINI: MouseSpec = MouseSpec {
    interface: 3,
    dpi: Dpi::Step50 {
        cmd: &[0x2d],
        first: 0,
        flags: false,
    },
    rate: Some(RATE_1),
    light: Light {
        zones: Zones::Fixed {
            prefix: &[0x21, 0x00],
            suffix: &[],
        },
        brightness: Brightness::None,
        rainbow: None,
        steady: &[],
    },
    save: SAVE_11,
    ..PRIME
};
/// Static color on every LED of a rivalcfg `rgbgradient` / `rgbgradientv2` mouse.
const fn gradient(cmd: &'static [u8], leds: u8, header: &'static Gradient) -> Light {
    Light {
        zones: Zones::Gradient { cmd, leds, header },
        brightness: Brightness::None,
        rainbow: None,
        steady: &[],
    }
}
/// Rival 310 / Sensei 310 / Sensei TEN: `5b 00`, logo and wheel.
const LIGHT_310: Light = gradient(
    &[0x5b, 0x00],
    2,
    &Gradient::V1 {
        len: 26,
        led_at: &[0],
        duration_at: 1,
        repeat_at: 17,
        count_at: 25,
    },
);
/// Rival 600: `05 00`, wheel, logo and six strip LEDs.
const LIGHT_600: Light = gradient(
    &[0x05, 0x00],
    8,
    &Gradient::V1 {
        len: 28,
        led_at: &[0, 5],
        duration_at: 6,
        repeat_at: 22,
        count_at: 27,
    },
);
/// Rival 500 / 700 / 710: `05 00`, logo and wheel.
const LIGHT_700: Light = gradient(&[0x05, 0x00], 2, &Gradient::V2);

const SENSEI_TEN: MouseSpec = MouseSpec {
    dpi: Dpi::Step50 {
        cmd: &[0x55, 0x00],
        first: 1,
        flags: true,
    },
    rate: Some(Rate {
        cmd: &[0x54, 0x00],
        zero_based: false,
    }),
    light: LIGHT_310,
    save: ONLY_RIVALCFG_OPENRGB_X,
    ..PRIME
};
const RIVAL310: MouseSpec = MouseSpec {
    dpi: Dpi::Preset {
        head: &[0x53, 0x00],
        tail: &[0x00, 0x42],
        codes: RANGE_12000,
    },
    save: MouseSave::Command(&[0x59, 0x00]),
    ..SENSEI_TEN
};
const RIVAL600: MouseSpec = MouseSpec {
    dpi: Dpi::Preset {
        head: &[0x03, 0x00],
        tail: &[0x00, 0x42],
        codes: RANGE_12000,
    },
    rate: Some(Rate {
        cmd: &[0x04, 0x00],
        zero_based: false,
    }),
    light: LIGHT_600,
    save: MouseSave::Command(&[0x09, 0x00]),
    ..SENSEI_TEN
};
const RIVAL700: MouseSpec = MouseSpec {
    light: LIGHT_700,
    ..RIVAL600
};
const RIVAL650: MouseSpec = MouseSpec {
    dpi: Dpi::Preset {
        head: &[0x15],
        tail: &[],
        codes: RANGE_12000,
    },
    rate: Some(Rate {
        cmd: &[0x17],
        zero_based: false,
    }),
    light: Light::NONE,
    battery: Some(Battery::Rival),
    // Seconds, 1-20 minutes.
    sleep: Some(Sleep {
        cmd: &[0x2b, 0x01, 0x01, 0x00, 0x00, 0x00],
        min: 1,
        unit: 60,
        len: 2,
    }),
    save: MouseSave::Command(&[0x09]),
    ..SENSEI_TEN
};

/// Rival 3 Gen 2: Aerox 3 commands, X/Y DPI.
const RIVAL3_GEN2: MouseSpec = MouseSpec {
    dpi: Dpi::Xy {
        cmd: &[0x34],
        sensor: Sensor::TrueMoveCore,
        first: 0,
        pad_to: 0,
    },
    ..AEROX3
};
const RIVAL3_WIRELESS_GEN2: MouseSpec = MouseSpec {
    dpi: Dpi::Xy {
        cmd: &[0x2c],
        sensor: Sensor::TrueMoveAir,
        first: 1,
        pad_to: 5,
    },
    ..RIVAL3_WIRELESS
};

/// Rival 95 / 100 / 105 sensitivity presets.
const CHOICE_RIVAL100: &[(u32, u8)] = &[
    (250, 0x08),
    (500, 0x07),
    (1000, 0x06),
    (1250, 0x05),
    (1500, 0x04),
    (1750, 0x03),
    (2000, 0x02),
    (4000, 0x01),
];
const CHOICE_KANA2: &[(u32, u8)] = &[
    (400, 0x08),
    (800, 0x07),
    (1200, 0x06),
    (1600, 0x05),
    (2000, 0x04),
    (2400, 0x03),
    (3200, 0x02),
    (4000, 0x01),
];
const CHOICE_KINZU2: &[(u32, u8)] = &[(400, 0x04), (800, 0x03), (1600, 0x02), (3200, 0x01)];

/// Older mice: `03 <preset> <code>` sensitivity, `04 00 <code>` polling rate.
const RIVAL95: MouseSpec = MouseSpec {
    interface: 0,
    wireless: &[],
    dpi: Dpi::Preset {
        head: &[0x03],
        tail: &[],
        codes: Codes::Choice(CHOICE_RIVAL100),
    },
    rate: Some(Rate {
        cmd: &[0x04, 0x00],
        zero_based: false,
    }),
    light: Light::NONE,
    battery: None,
    sleep: None,
    packet_len: 0,
    save: ONLY_RIVALCFG,
};
/// `05 00 rgb` color; `07 00 01` = steady (not breathing).
const RIVAL100: MouseSpec = MouseSpec {
    light: Light {
        zones: Zones::Fixed {
            prefix: &[0x05, 0x00],
            suffix: &[],
        },
        brightness: Brightness::None,
        rainbow: None,
        steady: &[&[0x07, 0x00, 0x01]],
    },
    save: MouseSave::Command(&[0x09, 0x00]),
    ..RIVAL95
};
/// Rival 106 / 110 / 300S: PMW3326 codes, 32-byte reports.
const RIVAL110: MouseSpec = MouseSpec {
    dpi: Dpi::Preset {
        head: &[0x03],
        tail: &[],
        codes: Codes::Sensor(Sensor::Pmw3326),
    },
    packet_len: 32,
    save: ONLY_RIVALCFG_OPENRGB_X,
    ..RIVAL100
};
/// Logo (`08 01`) and wheel (`08 02`) LEDs.
const RIVAL300: MouseSpec = MouseSpec {
    dpi: Dpi::Preset {
        head: &[0x03],
        tail: &[],
        codes: Codes::Linear {
            min: 50,
            max: 6500,
            step: 50,
            first: 1,
        },
    },
    light: Light {
        zones: Zones::List(&[&[0x08, 0x01], &[0x08, 0x02]]),
        brightness: Brightness::None,
        rainbow: None,
        steady: &[&[0x07, 0x01, 0x01], &[0x07, 0x02, 0x01]],
    },
    save: MouseSave::Command(&[0x09, 0x00]),
    ..RIVAL95
};
const SENSEI_RAW: MouseSpec = MouseSpec {
    dpi: Dpi::Preset {
        head: &[0x03],
        tail: &[],
        codes: Codes::Linear {
            min: 90,
            max: 5670,
            step: 90,
            first: 1,
        },
    },
    light: Light {
        zones: Zones::None,
        brightness: Brightness::Level4(&[&[0x05, 0x01]]),
        rainbow: None,
        steady: &[],
    },
    save: MouseSave::Command(&[0x09, 0x00]),
    ..RIVAL95
};
const KANA2: MouseSpec = MouseSpec {
    dpi: Dpi::Preset {
        head: &[0x03],
        tail: &[],
        codes: Codes::Choice(CHOICE_KANA2),
    },
    light: Light {
        zones: Zones::None,
        brightness: Brightness::Level4(&[&[0x05, 0x01], &[0x05, 0x02]]),
        rainbow: None,
        steady: &[],
    },
    ..RIVAL95
};
const KINZU2: MouseSpec = MouseSpec {
    dpi: Dpi::Preset {
        head: &[0x03],
        tail: &[],
        codes: Codes::Choice(CHOICE_KINZU2),
    },
    save: MouseSave::Command(&[0x09, 0x00]),
    ..RIVAL95
};

// ── Keyboards (OpenRGB) ─────────────────────────────────────────────────────

const APEX: KeyboardSpec = KeyboardSpec {
    interface: 1,
    wireless: &[],
    proto: KbProto::PerKey {
        len: APEX_LEN,
        gen: Gen::One,
        fw_gen3: false,
    },
    actuation: false,
};
const APEX_FW: KeyboardSpec = KeyboardSpec {
    proto: KbProto::PerKey {
        len: APEX_LEN,
        gen: Gen::One,
        fw_gen3: true,
    },
    ..APEX
};
const APEX9: KeyboardSpec = KeyboardSpec {
    proto: KbProto::PerKey {
        len: APEX9_LEN,
        gen: Gen::Two,
        fw_gen3: true,
    },
    ..APEX
};

models! {
    // Headsets: PIDs of one model differ by firmware (0-4 vs percent battery).
    Headset "Arctis Nova 7" [0x2202, 0x22a1] H(NOVA7);
    Headset "Arctis Nova 7 Gen 2" [0x227e] H(HeadsetSpec { save: Save::NovaGen2, ..NOVA7 });
    Headset "Arctis Nova 7X" [0x2206, 0x22a4, 0x22a5] H(NOVA7);
    Headset "Arctis Nova 7X White" [0x2258, 0x22ad] H(NOVA7);
    Headset "Arctis Nova 7X Gen 2" [0x229e] H(NOVA7);
    Headset "Arctis Nova 7P" [0x220a, 0x22a7] H(NOVA7P);
    Headset "Arctis Nova 7P Gen 2" [0x2298] H(NOVA7P);
    Headset "Arctis Nova 7 Diablo IV" [0x223a, 0x22a9] H(NOVA7);
    Headset "Arctis Nova 7 WoW Edition" [0x227a, 0x22ab] H(NOVA7);
    Headset "Arctis Nova 5" [0x2232] H(NOVA5);
    Headset "Arctis Nova 5X" [0x2253] H(NOVA5);
    Headset "Arctis Nova 5X White" [0x2264] H(NOVA5);
    Headset "Arctis Nova 3P Wireless" [0x2269] H(NOVA3P);
    Headset "Arctis Nova 3X Wireless" [0x226d] H(NOVA3P);
    Headset "Arctis Nova 3" [0x12ec] H(NOVA3);
    Headset "Arctis Nova Pro" [0x12cb, 0x12fa] H(NOVA_PRO_WIRED);
    Headset "Arctis Nova Pro for Xbox" [0x12cd] H(NOVA_PRO_WIRED);
    Headset "Arctis GameBuds" [0x230a] H(GAMEBUDS);
    Headset "Arctis GameBuds X" [0x2317] H(GAMEBUDS);
    Headset "Arctis 7+" [0x220e] H(ARCTIS7_PLUS);
    Headset "Arctis 7+ Destiny 2" [0x2236] H(ARCTIS7_PLUS);
    Headset "Arctis 7P+" [0x2212] H(ARCTIS7P_PLUS);
    Headset "Arctis 7X+" [0x2216] H(ARCTIS7X_PLUS);
    Headset "Arctis 7" [0x1260] H(ARCTIS7);
    Headset "Arctis 7 (2019)" [0x12ad] H(ARCTIS7);
    Headset "Arctis Pro" [0x1252] H(ARCTIS_PRO);
    Headset "Arctis Pro + GameDAC" [0x1280] H(HeadsetSpec {
        save: Save::Blocked("only HeadsetControl sends 06 09 to the GameDAC"),
        ..ARCTIS_PRO
    });
    Headset "Arctis 1 Wireless" [0x12b3] H(ARCTIS1);
    Headset "Arctis 1 Wireless for Xbox" [0x12b6] H(ARCTIS1);
    Headset "Arctis 1 Wireless Cyberpunk 2077" [0x12d0] H(ARCTIS1_CYBERPUNK);
    Headset "Arctis 1 Wireless for Xbox Cyberpunk 2077" [0x12d2] H(ARCTIS1_CYBERPUNK);
    Headset "Arctis 7X" [0x12d7] H(ARCTIS1);
    Headset "Arctis 7P" [0x12d5] H(ARCTIS1);
    Headset "Arctis 9" [0x12c2, 0x12c4] H(ARCTIS9);
    Headset "Arctis Pro Wireless" [0x1290] H(PRO_WIRELESS);
    // Mice: wired PID first, then the 2.4 GHz dongle.
    Mouse "Rival 3" [0x1824, 0x184c] M(RIVAL3);
    Mouse "Rival 3 Wireless" [0x1830] M(RIVAL3_WIRELESS);
    Mouse "Rival 3 Gen 2" [0x1870] M(RIVAL3_GEN2);
    Mouse "Rival 3 Wireless Gen 2" [0x1872]
        M(MouseSpec { save: ONLY_RIVALCFG, ..RIVAL3_WIRELESS_GEN2 });
    Mouse "Rival 5" [0x183c, 0x183e] M(RIVAL5);
    Mouse "Rival 310" [0x1720, 0x171e, 0x1736] M(RIVAL310);
    Mouse "Sensei 310" [0x1722] M(RIVAL310);
    Mouse "Rival 600" [0x1724, 0x172e] M(RIVAL600);
    Mouse "Rival 650 Wireless" [0x172b, 0x1726] M(RIVAL650);
    Mouse "Rival 700" [0x1700] M(MouseSpec { save: ONLY_RIVALCFG_OPENRGB_X, ..RIVAL700 });
    Mouse "Rival 710" [0x1730] M(MouseSpec { save: ONLY_RIVALCFG_OPENRGB_X, ..RIVAL700 });
    Mouse "Rival 500" [0x170e] M(MouseSpec { save: ONLY_RIVALCFG, ..RIVAL700 });
    Mouse "Rival 110" [0x1729] M(RIVAL110);
    Mouse "Rival 106" [0x1816] M(RIVAL110);
    Mouse "Rival 300S" [0x1810] M(RIVAL110);
    Mouse "Rival 100" [0x1702, 0x170a, 0x170b, 0x170c] M(RIVAL100);
    Mouse "Rival 105" [0x1814] M(RIVAL100);
    Mouse "Rival 95" [0x1706, 0x1707, 0x1704] M(RIVAL95);
    Mouse "Rival 100 PC Bang" [0x1708] M(RIVAL95);
    Mouse "Rival 300" [0x1710, 0x1712, 0x171c, 0x1394, 0x171a, 0x1716, 0x1714, 0x1718]
        M(RIVAL300);
    Mouse "Rival" [0x1384, 0x1392] M(RIVAL300);
    Mouse "Sensei [RAW]" [0x1369, 0x1362, 0x136d, 0x136f, 0x1380, 0x1390] M(SENSEI_RAW);
    Mouse "Kana v2" [0x137a] M(KANA2);
    Mouse "Kinzu v2" [0x1366, 0x1378] M(KINZU2);
    Mouse "Sensei TEN" [0x1832, 0x1834] M(SENSEI_TEN);
    Mouse "Prime" [0x182e, 0x182a, 0x1856] M(PRIME);
    Mouse "Prime+" [0x182c] M(PRIME);
    Mouse "Prime Mini" [0x184d] M(PRIME_MINI);
    Mouse "Prime Wireless" [0x1842, 0x1840]
        M(MouseSpec { wireless: &[0x1840], ..PRIME_WIRELESS });
    Mouse "Prime Mini Wireless" [0x184a, 0x1848]
        M(MouseSpec { wireless: &[0x1848], ..PRIME_WIRELESS });
    Mouse "Aerox 3" [0x1836] M(AEROX3);
    Mouse "Aerox 3 Wireless" [0x183a, 0x1838]
        M(MouseSpec { wireless: &[0x1838], ..AEROX_WIRELESS });
    Mouse "Aerox 3 Wireless CS2 Dragon Lore" [0x187a, 0x1878]
        M(MouseSpec { wireless: &[0x1878], ..AEROX_WIRELESS });
    Mouse "Aerox 5" [0x1850] M(AEROX5);
    Mouse "Aerox 5 Wireless" [0x1854, 0x1852]
        M(MouseSpec { wireless: &[0x1852], ..AEROX_WIRELESS });
    Mouse "Aerox 5 Wireless Destiny 2" [0x185e, 0x185c]
        M(MouseSpec { wireless: &[0x185c], ..AEROX_WIRELESS });
    Mouse "Aerox 5 Wireless Diablo IV" [0x1862, 0x1860]
        M(MouseSpec { wireless: &[0x1860], ..AEROX_WIRELESS });
    Mouse "Aerox 9 Wireless" [0x185a, 0x1858]
        M(MouseSpec { wireless: &[0x1858], ..AEROX_WIRELESS });
    Mouse "Aerox 9 Wireless WoW Edition" [0x1876, 0x1874]
        M(MouseSpec { wireless: &[0x1874], ..AEROX_WIRELESS });
    // Keyboards: wired PID first, then the 2.4 GHz dongle.
    Keyboard "Apex 3" [0x161a] K(KeyboardSpec { interface: 3, proto: KbProto::TenZone, ..APEX });
    Keyboard "Apex 3 TKL" [0x1622] K(KeyboardSpec { proto: KbProto::EightZone, ..APEX });
    Keyboard "Apex 5" [0x161c] K(APEX);
    Keyboard "Apex 7" [0x1612] K(APEX);
    Keyboard "Apex 7 TKL" [0x1618] K(APEX);
    Keyboard "Apex Pro" [0x1610] K(APEX);
    Keyboard "Apex Pro 3" [0x1640] K(KeyboardSpec { actuation: true, ..APEX });
    Keyboard "Apex Pro TKL" [0x1614] K(APEX_FW);
    Keyboard "Apex Pro TKL (2023)" [0x1628, 0x1630, 0x1632] K(KeyboardSpec {
        wireless: &[0x1630, 0x1632],
        proto: KbProto::PerKey { len: APEX_LEN, gen: Gen::Two, fw_gen3: false },
        ..APEX
    });
    Keyboard "Apex Pro TKL Gen 3" [0x1642, 0x1644, 0x1646] K(KeyboardSpec {
        wireless: &[0x1644, 0x1646],
        proto: KbProto::PerKey { len: APEX_LEN, gen: Gen::Three, fw_gen3: false },
        ..APEX
    });
    Keyboard "Apex 9 TKL" [0x1634] K(APEX9);
    Keyboard "Apex 9 Mini" [0x1620] K(APEX9);
    Keyboard "Apex M750" [0x0616] K(KeyboardSpec { interface: 2, proto: KbProto::ApexM, ..APEX });
    Keyboard "Apex (OG) / Apex Fnatic" [0x1202]
        K(KeyboardSpec { interface: 0, proto: KbProto::OldApex, ..APEX });
    Keyboard "Apex 350" [0x1206] K(KeyboardSpec { interface: 0, proto: KbProto::OldApex, ..APEX });
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let (_, spec) = SPECS
        .iter()
        .find(|(pids, _)| pids.contains(&pid))
        .with_context(|| format!("no SteelSeries spec for {pid:04x}"))?;
    let name = MODELS
        .iter()
        .find(|m| m.product_ids.contains(&pid))
        .map_or("SteelSeries", |m| m.name);
    match spec {
        Spec::Headset(s) => headset::open(pid, name, s, ctx),
        Spec::Mouse(s) => mouse::open(pid, name, s, ctx),
        Spec::Keyboard(s) => keyboard::open(pid, name, s, ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::catalog::SettingValue;

    /// The Nova Pro Wireless entries (`nova_pro.rs`) that open `MODELS`.
    const NOVA_PRO: usize = 3;

    #[test]
    fn nova_pro_wireless_entries_are_unchanged() {
        let names = [
            (0x12e5, "Arctis Nova Pro Wireless X"),
            (0x12e0, "Arctis Nova Pro Wireless"),
            (0x225d, "Arctis Nova Pro Wireless X (White)"),
        ];
        for (m, (pid, name)) in MODELS[..NOVA_PRO].iter().zip(names) {
            assert_eq!(m.vendor_id, 0x1038);
            assert_eq!(m.product_ids, &[pid]);
            assert_eq!(m.kind, DeviceKind::Headset);
            assert_eq!(m.name, name);
            assert!(!m.usbfs);
        }
    }

    /// A value each control accepts: its first choice, its minimum, or on.
    fn sample(meta: &hako_core::catalog::SettingMeta) -> SettingValue {
        use hako_core::catalog::ControlKind;
        match meta.kind {
            ControlKind::Dropdown => meta.options[0].value.clone(),
            ControlKind::Toggle => SettingValue::Bool(true),
            _ => SettingValue::Int(meta.min.unwrap_or(0.0) as i64),
        }
    }

    #[test]
    fn every_control_accepts_its_own_values() {
        for (m, (_, spec)) in MODELS[NOVA_PRO..].iter().zip(SPECS) {
            let controls = match spec {
                Spec::Headset(s) => headset::controls(s),
                Spec::Mouse(s) => mouse::controls(s, &Default::default()),
                Spec::Keyboard(s) => keyboard::controls(s.actuation, keyboard::save_blocked(s)),
            };
            let mut keys = std::collections::HashSet::new();
            for meta in controls {
                assert!(keys.insert(meta.key), "{}: {:?} twice", m.name, meta.key);
                // The only offered memory write is the save toggle; the
                // rest are listed blocked. Neither is a settings packet.
                assert!(
                    !meta.persistent
                        || meta.blocked.is_some()
                        || meta.key == hako_core::catalog::SettingKey::SaveToDevice,
                    "{} {:?}",
                    m.name,
                    meta.key
                );
                if meta.read_only || meta.persistent {
                    continue;
                }
                let value = sample(&meta);
                assert!(meta.validate(&value).is_ok(), "{} {:?}", m.name, meta.key);
                if let (Some(min), Some(max)) = (meta.min, meta.max) {
                    assert!(min < max, "{} {:?}", m.name, meta.key);
                }
                if let Spec::Headset(s) = spec {
                    let mut settings = hako_core::catalog::SettingsMap::new();
                    hako_core::config::merge_setting(&mut settings, meta.key, value);
                    assert!(
                        headset::packets(s, &mut settings, meta.key).is_ok(),
                        "{} {:?}",
                        m.name,
                        meta.key
                    );
                }
            }
        }
    }

    /// Save-to-device is offered exactly where the dossier
    /// (`docs/onboard-writes/steelseries.md`) rates it HIGH; elsewhere it is
    /// listed blocked or absent.
    #[test]
    fn save_to_device_only_where_researched() {
        use hako_core::catalog::SettingKey;
        let offered: Vec<_> = MODELS[NOVA_PRO..]
            .iter()
            .zip(SPECS)
            .filter(|(_, (_, spec))| {
                let c = match spec {
                    Spec::Headset(s) => headset::controls(s),
                    Spec::Mouse(s) => mouse::controls(s, &Default::default()),
                    Spec::Keyboard(s) => keyboard::controls(s.actuation, keyboard::save_blocked(s)),
                };
                c.iter()
                    .any(|m| m.key == SettingKey::SaveToDevice && m.blocked.is_none())
            })
            .map(|(m, _)| m.name)
            .collect();
        assert_eq!(
            offered,
            [
                "Arctis Nova 7 Gen 2",
                "Arctis 7+",
                "Arctis 7+ Destiny 2",
                "Arctis 7P+",
                "Arctis 7X+",
                "Arctis 7",
                "Arctis 7 (2019)",
                "Arctis 1 Wireless",
                "Arctis 1 Wireless for Xbox",
                "Arctis 7X",
                "Arctis 7P",
                "Rival 3",
                "Rival 310",
                "Sensei 310",
                "Rival 600",
                "Rival 650 Wireless",
                "Rival 100",
                "Rival 105",
                "Rival 300",
                "Rival",
                "Sensei [RAW]",
                "Kinzu v2",
            ]
        );
        // The Nova Pro Wireless entries are untouched.
        assert!(MODELS[..NOVA_PRO]
            .iter()
            .all(|m| m.name.starts_with("Arctis Nova Pro Wireless")));
    }

    #[test]
    fn host_chatmix_targets_the_headsets_own_sink() {
        for (m, (_, spec)) in MODELS[NOVA_PRO..].iter().zip(SPECS) {
            let Spec::Headset(s) = spec else { continue };
            if let ChatMix::Host(sinks) = s.chatmix {
                assert_eq!(sinks.len(), 2, "{}", m.name);
                assert!(sinks[0].starts_with(r"alsa_output\.usb-SteelSeries_"));
                assert!(sinks.iter().all(|p| p.contains("SteelSeries_Arctis")));
            }
        }
        // The classic Arctis 7 mixes in hardware.
        assert_eq!(ARCTIS7.chatmix, ChatMix::Dial);
    }

    #[test]
    fn every_model_has_a_matching_spec() {
        assert_eq!(MODELS.len(), SPECS.len() + NOVA_PRO);
        let mut seen = std::collections::HashSet::new();
        for (m, (pids, spec)) in MODELS[NOVA_PRO..].iter().zip(SPECS) {
            assert_eq!(m.product_ids, *pids, "{}", m.name);
            for pid in m.product_ids {
                assert!(seen.insert(pid), "{pid:04x} listed twice");
            }
            let (kind, wireless, useful) = match spec {
                Spec::Headset(s) => (
                    DeviceKind::Headset,
                    &[][..],
                    s.battery || !headset::controls(s).is_empty(),
                ),
                // Every mouse has DPI, every keyboard color.
                Spec::Mouse(s) => (DeviceKind::Mouse, s.wireless, true),
                Spec::Keyboard(s) => (DeviceKind::Keyboard, s.wireless, true),
            };
            assert_eq!(m.kind, kind, "{}", m.name);
            assert!(useful, "{} exposes nothing", m.name);
            // The dongle is never the canonical PID (it names the remap file).
            assert!(
                wireless.iter().all(|w| m.product_ids[1..].contains(w)),
                "{}",
                m.name
            );
        }
    }
}
