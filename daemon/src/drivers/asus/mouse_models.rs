//! ASUS ROG / TUF mice: one capability row per model (data only).
//!
//! Row sources, cited per entry: \[L\] libratbag `data/devices/asus-*.device`
//! (PIDs, DPI ranges, DOUBLE_DPI = step 100), \[G\] G-Helper
//! `app/Peripherals/Mouse/Models/*.cs` (PIDs, interface, stages, rates,
//! snapping, lift-off, battery, auto-off, zones, brightness scale), \[O\]
//! OpenRGB `AsusAuraMouseDevices.h` / `AsusAuraMouseControllerDetect.cpp`
//! (PIDs, vendor interface, zones, brightness scale).
//!
//! DPI is left out where L and G disagree on the wire step (Gladius II,
//! Gladius II Core, Chakram X, Strix Impact II family, TUF M4 Air): a wrong
//! step would write twice or half the requested DPI.
//!
//! Debounce (`debounce`) follows G `HasDebounceSetting`; L and G agree on its
//! field (stages + 1) and values. Active-stage selection (`stage_select`)
//! follows G `CanChangeDPIProfile`. `omni` lists the PIDs G-Helper's Omni
//! pairing list reports for the mouse (`PeripheralsProvider.MouseFromOmniPid`).

use super::protocol::{Battery, Lod};

#[derive(Clone, Copy, Debug)]
pub struct Dpi {
    pub min: u16,
    pub max: u16,
    /// Wire unit: value = dpi / step - 1.
    pub step: u16,
    /// Per-axis stages: read through `12 04 02`.
    pub xy: bool,
    /// Each stage has an LED colour that rides along with DPI writes.
    pub colors: bool,
    /// `12 00` reports the stage from 0 (TUF M3 Gen II).
    pub zero_based: bool,
    /// `12 00` byte 11 is the active stage (L and G agree), so `dpi` can
    /// follow it; otherwise only the per-stage values are offered.
    pub active: bool,
}

const fn dpi(min: u16, max: u16, step: u16) -> Option<Dpi> {
    Some(Dpi {
        min,
        max,
        step,
        xy: false,
        colors: false,
        zero_based: false,
        active: true,
    })
}

/// Newer AimPoint-generation mice: per-axis DPI with stage colours.
const fn dpi_xy(max: u16) -> Option<Dpi> {
    Some(Dpi {
        min: 100,
        max,
        step: 50,
        xy: true,
        colors: true,
        zero_based: false,
        active: true,
    })
}

#[derive(Debug)]
pub struct MouseSpec {
    pub name: &'static str,
    /// Cable PID first (canonical: names the settings and remap files), then dongle.
    pub pids: &'static [u16],
    /// USB interface of the vendor collection (usage page 0xFF01).
    pub iface: u32,
    /// PIDs of a dedicated 2.4 GHz receiver driven like the Omni receiver
    /// (interface 2, report 0x03).
    pub rx: &'static [u16],
    /// PIDs the Omni receiver's pairing list reports for this mouse.
    pub omni: &'static [u16],
    /// HID name fragment → model sharing these PIDs (G-Helper matches the
    /// USB product string).
    pub variants: &'static [(&'static str, &'static MouseSpec)],
    /// Onboard DPI stages; rate and snapping fields follow them.
    pub stages: u8,
    pub dpi: Option<Dpi>,
    pub rates: &'static [u16],
    pub snapping: bool,
    pub debounce: bool,
    /// The active stage can be switched (`51 31 09`).
    pub stage_select: bool,
    pub lod: Lod,
    pub battery: Battery,
    pub auto_off: bool,
    /// LED zones written one by one (0 logo, 1 wheel, 2 underglow, 3 all).
    pub zones: &'static [u8],
    /// Device brightness scale: 4 on older mice, else 100.
    pub max_brightness: u8,
}

const R1000: &[u16] = &[125, 250, 500, 1000];
const R250_1000: &[u16] = &[250, 500, 1000];
const R8000: &[u16] = &[125, 250, 500, 1000, 2000, 4000, 8000];
const LOGO: &[u8] = &[0];
const WHEEL: &[u8] = &[1];
const LOGO_WHEEL: &[u8] = &[0, 1];
const THREE: &[u8] = &[0, 1, 2];

const BASE: MouseSpec = MouseSpec {
    name: "",
    pids: &[],
    iface: 0,
    rx: &[],
    omni: &[],
    variants: &[],
    stages: 4,
    dpi: None,
    rates: R1000,
    snapping: true,
    debounce: false,
    stage_select: true,
    lod: Lod::None,
    battery: Battery::None,
    auto_off: false,
    zones: &[],
    max_brightness: 100,
};

pub const MICE: &[MouseSpec] = &[
    // [G GladiusIII, O]
    MouseSpec {
        name: "ASUS ROG Gladius III",
        pids: &[0x197b],
        debounce: true,
        dpi: dpi(100, 26_000, 50),
        lod: Lod::Calibrated,
        zones: THREE,
        ..BASE
    },
    // [G GladiusIIIWireless, O]; battery percent at byte 4 also confirmed by
    // github.com/BillyNeverDies/rog-mouse-battery on 197f.
    MouseSpec {
        name: "ASUS ROG Gladius III Wireless",
        pids: &[0x197d, 0x197f],
        debounce: true,
        dpi: dpi(100, 26_000, 50),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: THREE,
        ..BASE
    },
    // [G GladiusIIIAimpoint, O] (O lists the logo zone only).
    MouseSpec {
        name: "ASUS ROG Gladius III Wireless AimPoint",
        pids: &[0x1a70, 0x1a72],
        omni: &[0x1a72],
        debounce: true,
        dpi: dpi_xy(36_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: LOGO,
        ..BASE
    },
    // [G GladiusIIIAimpointEva2]
    MouseSpec {
        name: "ASUS ROG Gladius III Wireless AimPoint EVA-02",
        pids: &[0x1b0a, 0x1b0c],
        debounce: true,
        dpi: dpi_xy(36_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: LOGO,
        ..BASE
    },
    // [L asus-rog-gladius2-origin (DOUBLE_DPI), G GladiusIIOrigin, O iface 2]
    MouseSpec {
        name: "ASUS ROG Gladius II Origin",
        pids: &[0x1877],
        debounce: true,
        iface: 2,
        stages: 2,
        dpi: dpi(100, 12_000, 100),
        lod: Lod::Calibrated,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [G GladiusIIOriginCOD, O iface 2]
    MouseSpec {
        name: "ASUS ROG Gladius II Origin COD",
        pids: &[0x18b1],
        debounce: true,
        iface: 2,
        stages: 2,
        dpi: dpi(100, 12_000, 100),
        lod: Lod::Calibrated,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-gladius2 (no DOUBLE_DPI) vs G GladiusII (step 100): no DPI; O iface 2]
    MouseSpec {
        name: "ASUS ROG Gladius II",
        pids: &[0x1845],
        debounce: true,
        iface: 2,
        stages: 2,
        lod: Lod::Calibrated,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [G GladiusIIWireless, O iface 2]
    MouseSpec {
        name: "ASUS ROG Gladius II Wireless",
        pids: &[0x18a0],
        debounce: true,
        iface: 2,
        stages: 2,
        dpi: dpi(100, 16_000, 100),
        battery: Battery::Quarters,
        auto_off: true,
        zones: LOGO_WHEEL,
        max_brightness: 4,
        ..BASE
    },
    // [G GladiusIICore (Strix Impact II class), O]
    MouseSpec {
        name: "ASUS ROG Gladius II Core",
        pids: &[0x18dd],
        debounce: true,
        zones: LOGO_WHEEL,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-chakram (DOUBLE_DPI), G Chakram, O]
    MouseSpec {
        name: "ASUS ROG Chakram",
        pids: &[0x18e3, 0x18e5],
        debounce: true,
        stage_select: false,
        dpi: dpi(100, 16_000, 100),
        lod: Lod::Calibrated,
        battery: Battery::Quarters,
        auto_off: true,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-chakram-core (DOUBLE_DPI), G ChakramCore, O]
    MouseSpec {
        name: "ASUS ROG Chakram Core",
        pids: &[0x1958],
        stage_select: false,
        dpi: dpi(100, 16_000, 100),
        lod: Lod::Plain,
        zones: LOGO_WHEEL,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-chakram-x (DOUBLE_DPI) vs G ChakramX (step 50): no DPI; O]
    // Cable mode also offers 2-8 kHz (G ChakramXWired); the common set is listed.
    MouseSpec {
        name: "ASUS ROG Chakram X",
        pids: &[0x1a18, 0x1a1a],
        debounce: true,
        rates: R250_1000,
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: THREE,
        ..BASE
    },
    // [L asus-rog-spatha-x, G SpathaX, O]
    MouseSpec {
        name: "ASUS ROG Spatha X",
        pids: &[0x1977, 0x1979],
        dpi: Some(Dpi {
            min: 100,
            max: 19_000,
            step: 50,
            xy: false,
            colors: true,
            zero_based: false,
            active: true,
        }),
        rates: R250_1000,
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: THREE,
        ..BASE
    },
    // [L asus-rog-keris (DOUBLE_DPI), G Keris, O]
    MouseSpec {
        name: "ASUS ROG Keris",
        pids: &[0x195c],
        debounce: true,
        stage_select: false,
        dpi: dpi(100, 16_000, 100),
        lod: Lod::Plain,
        zones: LOGO_WHEEL,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-keris-wireless (DOUBLE_DPI), G KerisWireless, O]
    MouseSpec {
        name: "ASUS ROG Keris Wireless",
        pids: &[0x195e, 0x1960],
        debounce: true,
        stage_select: false,
        dpi: dpi(100, 16_000, 100),
        lod: Lod::Plain,
        battery: Battery::Quarters,
        auto_off: true,
        zones: LOGO_WHEEL,
        max_brightness: 4,
        ..BASE
    },
    // [G KerisWirelessEvaEdition]
    MouseSpec {
        name: "ASUS ROG Keris Wireless EVA Edition",
        pids: &[0x1a57, 0x1a59],
        debounce: true,
        stage_select: false,
        dpi: dpi(100, 16_000, 100),
        lod: Lod::Plain,
        battery: Battery::Quarters,
        auto_off: true,
        zones: LOGO_WHEEL,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-keris-wireless-aimpoint, G KerisWirelssAimpoint, O]
    MouseSpec {
        name: "ASUS ROG Keris Wireless AimPoint",
        pids: &[0x1a66, 0x1a68],
        omni: &[0x1a68, 0x1a6a],
        debounce: true,
        dpi: dpi_xy(36_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: LOGO,
        ..BASE
    },
    // [G KerisIIAceWired]; its 2.4 GHz link is the Omni receiver.
    MouseSpec {
        name: "ASUS ROG Keris II Ace",
        pids: &[0x1b16],
        omni: &[0x1b1a, 0x1b18],
        dpi: dpi_xy(42_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: LOGO,
        ..BASE
    },
    // [G KerisIIOriginWired]
    MouseSpec {
        name: "ASUS ROG Keris II Origin",
        pids: &[0x1c0c],
        omni: &[0x1c0e],
        dpi: dpi_xy(42_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: THREE,
        ..BASE
    },
    // [G KerisIIOriginKJPWired]
    MouseSpec {
        name: "ASUS ROG Keris II Origin KJP",
        pids: &[0x1d4c],
        omni: &[0x1d4e],
        dpi: dpi_xy(42_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: THREE,
        ..BASE
    },
    // [L asus-rog-harpe-wireless (1a92), G HarpeAceAimLabEdition (1a92 cable, 1a94 dongle)]
    MouseSpec {
        name: "ASUS ROG Harpe Ace Aim Lab Edition",
        pids: &[0x1a92, 0x1a94],
        omni: &[0x1a94],
        debounce: true,
        dpi: dpi_xy(36_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: WHEEL,
        ..BASE
    },
    // [G HarpeAceExtremeWeird]
    MouseSpec {
        name: "ASUS ROG Harpe Ace Extreme",
        pids: &[0x1b67],
        omni: &[0x1b68, 0x1b69],
        debounce: true,
        dpi: dpi_xy(42_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: WHEEL,
        ..BASE
    },
    // [G HarpeAceMiniWired]
    MouseSpec {
        name: "ASUS ROG Harpe Ace Mini",
        pids: &[0x1b63],
        omni: &[0x1b65],
        dpi: dpi_xy(42_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: WHEEL,
        ..BASE
    },
    // [G HarpeMiniCore]
    MouseSpec {
        name: "ASUS ROG Harpe Mini Core",
        pids: &[0x1c8f],
        dpi: Some(Dpi {
            min: 100,
            max: 12_000,
            step: 50,
            xy: false,
            colors: true,
            zero_based: false,
            active: true,
        }),
        lod: Lod::Calibrated,
        zones: WHEEL,
        ..BASE
    },
    // [G HarpeIIAceWired; HarpeIIAceWireless: own receiver 1ad0, mi_02 col03,
    // report 3; DetectHarpeIIWireless: product name with EXTREME = Extreme Edition 2.0]
    MouseSpec {
        name: "ASUS ROG Harpe II Ace",
        pids: &[0x1c69, 0x1ad0],
        rx: &[0x1ad0],
        variants: &[("EXTREME", &HARPE_II_EXTREME)],
        dpi: dpi_xy(42_000),
        rates: R8000,
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: WHEEL,
        ..BASE
    },
    // [L asus-rog-pugio, G Pugio, O iface 2]. Rate field 2 = stages + 0 as
    // libratbag; G-Helper's Pugio writes field 4, which on a 2-stage mouse is
    // snapping, so that line was not followed.
    MouseSpec {
        name: "ASUS ROG Pugio",
        pids: &[0x1846],
        iface: 2,
        stages: 2,
        dpi: dpi(100, 7_200, 50),
        lod: Lod::Calibrated,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [G PugioII, O]
    MouseSpec {
        name: "ASUS ROG Pugio II",
        pids: &[0x1906, 0x1908],
        debounce: true,
        stage_select: false,
        dpi: dpi(100, 16_000, 100),
        lod: Lod::Calibrated,
        battery: Battery::Quarters,
        auto_off: true,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-strix-impact, G StrixImpact, O iface 2]; O: rejects zone 3.
    MouseSpec {
        name: "ASUS ROG Strix Impact",
        pids: &[0x1847],
        debounce: true,
        iface: 2,
        stages: 2,
        dpi: dpi(100, 5_000, 50),
        snapping: false,
        zones: LOGO,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-strix-impact2 vs G StrixImpactII DPI step: no DPI; O]
    MouseSpec {
        name: "ASUS ROG Strix Impact II",
        pids: &[0x18e1],
        debounce: true,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [G StrixImpactIIElectroPunk, O]
    MouseSpec {
        name: "ASUS ROG Strix Impact II Electro Punk",
        pids: &[0x1956],
        debounce: true,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [G StrixImpactIIMoonlightWhite, O]
    MouseSpec {
        name: "ASUS ROG Strix Impact II Moonlight White",
        pids: &[0x19d2],
        debounce: true,
        zones: THREE,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-strix-impact2-wireless vs G DPI step: no DPI; G StrixImpactIIWireless, O]
    MouseSpec {
        name: "ASUS ROG Strix Impact II Wireless",
        pids: &[0x1947, 0x1949],
        debounce: true,
        stage_select: false,
        lod: Lod::Plain,
        battery: Battery::Quarters,
        auto_off: true,
        zones: LOGO_WHEEL,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-strix-impact3, G StrixImpactIII, O]
    MouseSpec {
        name: "ASUS ROG Strix Impact III",
        pids: &[0x1a88],
        dpi: dpi(100, 12_000, 50),
        zones: LOGO_WHEEL,
        ..BASE
    },
    // [G TUFM3, O iface 1]
    MouseSpec {
        name: "ASUS TUF Gaming M3",
        pids: &[0x1910],
        debounce: true,
        iface: 1,
        dpi: dpi(100, 7_000, 100),
        zones: LOGO,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-tuf-gaming-m3-gen-ii, G TUFM3GenII (0-based stage), O iface 2]
    MouseSpec {
        name: "ASUS TUF Gaming M3 Gen II",
        pids: &[0x1a9b],
        debounce: true,
        iface: 2,
        dpi: Some(Dpi {
            min: 100,
            max: 8_000,
            step: 50,
            xy: false,
            colors: true,
            zero_based: true,
            active: true,
        }),
        zones: LOGO,
        ..BASE
    },
    // [L asus-tuf-gaming-m4-air vs G TUFM4Air DPI step: no DPI]
    MouseSpec {
        name: "ASUS TUF Gaming M4 Air",
        pids: &[0x1a03],
        lod: Lod::Calibrated,
        ..BASE
    },
    // [G TUFM4Wirelss, TUFM4WirelssCN]
    MouseSpec {
        name: "ASUS TUF Gaming M4 Wireless",
        pids: &[0x19f4, 0x1a8d],
        debounce: true,
        dpi: dpi(100, 12_000, 100),
        battery: Battery::Percent,
        auto_off: true,
        ..BASE
    },
    // [G TXGamingMini(Wired)]
    MouseSpec {
        name: "ASUS TX Gaming Mini",
        pids: &[0x1af3, 0x1af5],
        debounce: true,
        dpi: Some(Dpi {
            min: 100,
            max: 12_000,
            step: 50,
            xy: true,
            colors: false,
            zero_based: false,
            active: true,
        }),
        battery: Battery::Percent,
        auto_off: true,
        ..BASE
    },
    // [L asus-tuf-gaming-mini-wl-miku (SEPARATE_XY_DPI), G TUFGamingMiniMiku].
    // Lighting left out: L reads raw brightness, G a 0-100 scale.
    MouseSpec {
        name: "ASUS TUF Gaming Mini Wireless Miku",
        pids: &[0x1c56, 0x1c57],
        debounce: true,
        dpi: Some(Dpi {
            min: 100,
            max: 12_000,
            step: 50,
            xy: true,
            colors: false,
            zero_based: false,
            active: true,
        }),
        battery: Battery::Percent,
        auto_off: true,
        ..BASE
    },
    // [G TUFM5, O iface 2]
    MouseSpec {
        name: "ASUS TUF Gaming M5",
        pids: &[0x1898],
        debounce: true,
        iface: 2,
        stages: 2,
        dpi: dpi(100, 6_200, 100),
        zones: LOGO,
        max_brightness: 4,
        ..BASE
    },
    // [L asus-rog-gladius2-origin-pink (DOUBLE_DPI), G GladiusIIOriginPink].
    // Lighting left out: G sends an extra dual-colour byte before RGB, L does not.
    MouseSpec {
        name: "ASUS ROG Gladius II Origin PNK LTD",
        pids: &[0x18cd],
        debounce: true,
        iface: 2,
        stages: 2,
        dpi: dpi(100, 12_000, 100),
        lod: Lod::Calibrated,
        ..BASE
    },
    // [L asus-rog-strix-carry (STRIX_PROFILE, 100:7200, no DOUBLE_DPI), G StrixCarry
    // (mi_01, step 50, battery quarters at byte 6)]. The active stage is left
    // out: L reads it at byte 11, G at byte 10. No LEDs.
    MouseSpec {
        name: "ASUS ROG Strix Carry",
        pids: &[0x18b4],
        debounce: true,
        stage_select: false,
        iface: 1,
        stages: 2,
        dpi: Some(Dpi {
            min: 100,
            max: 7_200,
            step: 50,
            xy: false,
            colors: false,
            zero_based: false,
            active: false,
        }),
        battery: Battery::QuartersAt6,
        ..BASE
    },
];

/// Models without a PID of their own: reached through a receiver's pairing
/// list or a product name (see `omni` and `variants`).
pub const UNLISTED: &[MouseSpec] = &[
    // [L asus-rog-strix-impact3-wireless (OMNI_RECEIVER, SEPARATE_XY_DPI,
    // LED_V2), G StrixImpactIIIWirelessOmni (paired PID 1ad7)]
    MouseSpec {
        name: "ASUS ROG Strix Impact III Wireless",
        omni: &[0x1ad7],
        dpi: dpi_xy(36_000),
        lod: Lod::Calibrated,
        battery: Battery::Percent,
        auto_off: true,
        zones: WHEEL,
        ..BASE
    },
    HARPE_II_EXTREME,
];

/// [G HarpeIIExtremeEdition20: Harpe II Ace with DPI in 1-dpi units up to 65000]
const HARPE_II_EXTREME: MouseSpec = MouseSpec {
    name: "ASUS ROG Harpe II Extreme Edition 2.0",
    pids: &[0x1ad0],
    rx: &[0x1ad0],
    dpi: Some(Dpi {
        min: 100,
        max: 65_000,
        step: 1,
        xy: true,
        colors: true,
        zero_based: false,
        active: true,
    }),
    rates: R8000,
    lod: Lod::Calibrated,
    battery: Battery::Percent,
    auto_off: true,
    zones: WHEEL,
    ..BASE
};

pub fn find(pid: u16) -> Option<&'static MouseSpec> {
    MICE.iter().find(|m| m.pids.contains(&pid))
}

/// The mouse the Omni receiver lists as `paired`.
pub fn find_omni(paired: u16) -> Option<&'static MouseSpec> {
    MICE.iter()
        .chain(UNLISTED)
        .find(|m| m.omni.contains(&paired))
}
