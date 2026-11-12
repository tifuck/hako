use super::*;

#[test]
fn request_framing_matches_sources() {
    assert_eq!(packet(Flight, 0x05, 0), pad(&[0x21, 0xff, 0x05], 20));
    assert_eq!(packet(Alpha, 0x11, 0x40)[..5], [0x21, 0xbb, 0x11, 0x40, 0]);
    assert_eq!(packet(Alpha, 0x11, 0x40).len(), 31);
    let k = packet(Cloud2Kingston, 0x18, 30);
    assert_eq!(k.len(), 62);
    assert_eq!(
        k[..18],
        [
            0x06, 0x00, 0x02, 0x00, 0x9a, 0x00, 0x00, 0x68, 0x4a, 0x8e, 0x0a, 0x00, 0x00, 0x00,
            0xbb, 0x18, 30, 0
        ]
    );
    assert_eq!(
        packet(Cloud2Hp, 0x22, 10),
        pad(&[0x06, 0xff, 0xbb, 0x22, 10], 52)
    );
    assert_eq!(packet(Cloud3, 0x89, 0), pad(&[0x66, 0x89], 62));
    assert_eq!(packet(Cloud2Core, 0x89, 0).len(), 64);
    assert_eq!(
        cloud3s_get(0x06)[..7],
        [0x0c, 0x02, 0x03, 0x01, 0x00, 0x06, 0]
    );
    assert_eq!(
        packet(Cloud3S, 0x01, 1)[..7],
        [0x0c, 0x02, 0x03, 0x00, 0x00, 0x01, 1]
    );
    assert_eq!(packet(Alpha2, 0x50, 0x02), pad(&[0x50, 0x02], 64));
    assert_eq!(packet(Cloud3Wired, 0x86, 1), pad(&[0x20, 0x86, 1], 62));
}

#[test]
fn setters_emit_documented_sequences() {
    let int = SettingValue::Int;
    let on = SettingValue::Bool(true);
    assert_eq!(
        set_packets(Alpha, SettingKey::MicSideTone, &int(0)).unwrap(),
        vec![packet(Alpha, 0x10, 0)]
    );
    assert_eq!(
        set_packets(Alpha, SettingKey::MicSideTone, &int(64)).unwrap(),
        vec![packet(Alpha, 0x10, 1), packet(Alpha, 0x11, 64)]
    );
    assert_eq!(
        set_packets(Cloud2Hp, SettingKey::MicMute, &on).unwrap(),
        vec![packet(Cloud2Hp, 0x20, 1)]
    );
    // Core sidetone code 1 = level -5 = 0xfb on the wire.
    assert_eq!(
        set_packets(Cloud2Core, SettingKey::MicSideTone, &int(1)).unwrap(),
        vec![packet(Cloud2Core, 0x01, 1), packet(Cloud2Core, 0x05, 0xfb)]
    );
    assert_eq!(
        set_packets(Cloud3, SettingKey::PmShutdown, &int(20)).unwrap(),
        vec![packet(Cloud3, 0x02, 20)]
    );
    assert!(set_packets(Flight, SettingKey::MicMute, &on).is_err());
}

#[test]
fn flight_voltage_to_percent() {
    assert_eq!(flight_percent(3000), 5);
    assert_eq!(flight_percent(3328), 5);
    assert_eq!(flight_percent(3800), 55);
    assert_eq!(flight_percent(3970), 100);
    assert_eq!(
        parse(Flight, &[0x21, 0xff, 0x05, 0x0e, 0xd8]),
        vec![
            Event::Online(true),
            Event::Battery(55),
            Event::Charging(false)
        ]
    );
    // 0x1010 mV > 0x100b: charger voltage, no level.
    assert_eq!(
        parse(Flight, &[0x21, 0xff, 0x05, 0x10, 0x10]),
        vec![Event::Online(true), Event::Charging(true)]
    );
    assert_eq!(parse(Flight, &[0x65, 0x04]), vec![Event::Muted(true)]);
}

#[test]
fn parsers_read_documented_offsets() {
    assert_eq!(
        parse(Alpha, &[0x21, 0xbb, 0x25, 77]),
        vec![Event::Battery(77)]
    );
    assert_eq!(
        parse(Alpha, &[0x21, 0xbb, 0x03, 0x01]),
        vec![Event::Online(false)]
    );
    assert_eq!(
        parse(Cloud2Kingston, &[0x0b, 0, 0xbb, 0x02, 0, 0, 0, 64]),
        vec![Event::Battery(64)]
    );
    assert_eq!(
        parse(Cloud2Kingston, &[0x0b, 0, 0xbb, 0x08, 1, 0, 0, 0]),
        vec![Event::Muted(true)]
    );
    assert_eq!(
        parse(Cloud2Hp, &[0x06, 0xff, 0xbb, 0x02, 0, 0x0f, 0xa0, 90]),
        vec![Event::Battery(90)]
    );
    assert_eq!(
        parse(Cloud3, &[0x66, 0x0d, 1, 0, 42]),
        vec![Event::Battery(42)]
    );
    assert!(parse(Cloud3, &[0x66, 0x89, 0, 0, 42]).is_empty());
    assert_eq!(
        parse(Cloud3, &[0x66, 0x0a, 1, 0, 0]),
        vec![Event::Muted(true)]
    );
    // 0x03 is a mute echo only on the Core.
    assert!(parse(Cloud3, &[0x66, 0x03, 1, 0, 0]).is_empty());
    assert_eq!(
        parse(Cloud2Core, &[0x66, 0x88, 0xfc, 0, 0]),
        vec![Event::SideToneLevel(-4)]
    );
    assert_eq!(
        parse(Cloud3S, &[0x0c, 2, 3, 1, 0, 0x06, 80]),
        vec![Event::Battery(80)]
    );
    assert!(parse(Cloud3S, &[0x0c, 2, 3, 1, 0, 0x06, 0xff]).is_empty());
    assert_eq!(
        parse(Cloud3S, &[0x0d, 0, 0, 0, 0x03, 1]),
        vec![Event::Muted(true)]
    );
    assert_eq!(
        parse(Alpha2, &[0x51, 0x02, 66, 0x01, 0x1a, 0, 0x00, 0x0f]),
        vec![
            Event::Online(true),
            Event::Battery(66),
            Event::Charging(true)
        ]
    );
    assert_eq!(parse(Alpha2, &[0x53, 0x01, 0]), vec![Event::Online(false)]);
}

#[test]
fn alpha2_block_roundtrip() {
    let mut reply = pad(&[0x61, 0x02], 64);
    let block = [0, 0, 0, 0, 1, 10, 0, 0, 0x83, 0, 0, 2];
    reply[4..16].copy_from_slice(&block);
    assert_eq!(alpha2_block(&reply), Some(block));
    assert_eq!(
        alpha2_events(&block),
        vec![
            Event::SideToneOn(true),
            Event::SideToneLevel(10),
            Event::EqPreset(2),
            // 0x83 = 3 steps toward chat: game turned down to 40 %.
            Event::ChatMix(0.4, 1.0)
        ]
    );
    let mut b = block;
    alpha2_apply(&mut b, SettingKey::MicSideTone, &SettingValue::Int(0)).unwrap();
    assert_eq!((b[4], b[5]), (0, 10));
    alpha2_apply(&mut b, SettingKey::EqPreset, &SettingValue::Int(3)).unwrap();
    let w = alpha2_write(&b);
    assert_eq!(w[..2], [0x60, 0x01]);
    assert_eq!(w[2..14], b);
    let mut bad = reply.clone();
    bad[15] = 0; // preset 0 is invalid
    assert_eq!(alpha2_block(&bad), None);

    let mut ack = pad(&[0xff, 0x01], 64);
    ack[14..16].copy_from_slice(&[0x60, 0x01]);
    assert_eq!(alpha2_ack(&ack, 0x60, 0x01), Some(true));
    ack[1] = 0x02;
    assert_eq!(alpha2_ack(&ack, 0x60, 0x01), Some(false));
    assert_eq!(alpha2_ack(&ack, 0x52, 0x01), None);
}

#[test]
fn alpha_s_command_and_status() {
    let status = packet(AlphaS, ALPHA_S_STATUS, 0);
    assert_eq!(status.len(), 62);
    assert_eq!(
        status[..12],
        [0x06, 0, 0x01, 0, 0x9b, 0, 0, 0x68, 0x4a, 0x8e, 0x09, 0]
    );
    assert_eq!(
        set_packets(AlphaS, SettingKey::MicSideTone, &SettingValue::Int(1)).unwrap()[0][10],
        0x07
    );
    let q88 = |db: f32| ((db * 256.0) as i16).to_be_bytes();
    // Sidetone on + muted; game at -6.5 dB (step -3 of 8), chat at 0 dB.
    let mut r = status.clone();
    r[5] = 0x80;
    r[14] = 0x03;
    r[16..18].copy_from_slice(&q88(-6.5));
    assert_eq!(
        parse(AlphaS, &r),
        vec![
            Event::SideToneOn(true),
            Event::Muted(true),
            Event::ChatMix(0.625, 1.0)
        ]
    );
    // -45 dB = silent; no ack bit = no reply.
    r[20..22].copy_from_slice(&q88(-45.0));
    assert_eq!(parse(AlphaS, &r)[2], Event::ChatMix(0.625, 0.0));
    r[5] = 0;
    assert!(parse(AlphaS, &r).is_empty());
}

#[test]
fn voice_prompts_auto_off_and_eq() {
    let on = SettingValue::Bool(true);
    assert_eq!(
        set_packets(Alpha, SettingKey::VoicePrompts, &on).unwrap(),
        vec![packet(Alpha, 0x13, 1)]
    );
    assert_eq!(
        parse(Alpha, &[0x21, 0xbb, 0x09, 0x01]),
        vec![Event::VoicePrompts(true)]
    );
    assert_eq!(
        set_packets(Cloud3S, SettingKey::VoicePrompts, &on).unwrap()[0][..7],
        [0x0c, 0x02, 0x03, 0x00, 0x00, 0x0b, 0x01]
    );
    assert_eq!(
        parse(Cloud3S, &[0x0c, 2, 3, 1, 0, 0x14, 0]),
        vec![Event::VoicePrompts(false)]
    );
    // 20 min = 1200 s = 04 b0.
    assert_eq!(
        set_packets(Cloud3S, SettingKey::PmShutdown, &SettingValue::Int(20)).unwrap()[0][..9],
        [0x0c, 0x02, 0x03, 0x00, 0x00, 0x4a, 0x04, 0xb0, 0]
    );
    assert_eq!(
        parse(Cloud3S, &[0x0c, 2, 3, 1, 0, 0x4b, 0x07, 0x08]),
        vec![Event::AutoOff(30)]
    );
    // +6 dB = 600 = 02 58; -3.5 dB = -350 = fe a2 (HyperHeadset's examples).
    assert_eq!(
        set_packets(Cloud3S, SettingKey::EqBand1, &SettingValue::Float(6.0)).unwrap()[0][..9],
        [0x0c, 0x02, 0x03, 0x00, 0x00, 0x5f, 0x00, 0x02, 0x58]
    );
    assert_eq!(
        set_packets(Cloud3S, SettingKey::EqBand10, &SettingValue::Float(-3.5)).unwrap()[0][6..9],
        [0x09, 0xfe, 0xa2]
    );
    assert!(set_packets(Cloud2Hp, SettingKey::VoicePrompts, &on).is_err());
}

#[test]
fn sidetone_volume_and_mic_attached() {
    // hsctl COMMANDS.md: `06 ff bb 23 VOLUME` set, `06 ff bb 0b 0a` = volume 10.
    assert_eq!(
        set_packets(
            Cloud2Hp,
            SettingKey::MicSideToneVolume,
            &SettingValue::Int(10)
        )
        .unwrap(),
        vec![pad(&[0x06, 0xff, 0xbb, 0x23, 10], 52)]
    );
    assert_eq!(
        parse(Cloud2Hp, &[0x06, 0xff, 0xbb, 0x0b, 0x0a, 0, 0, 0]),
        vec![Event::SideToneVolume(10)]
    );
    assert!(queries(Cloud2Hp).contains(&packet(Cloud2Hp, 0x0b, 0)));
    // HyperHeadset get_mic_connected: status byte 1 = attached.
    assert_eq!(
        parse(Cloud2Hp, &[0x06, 0xff, 0xbb, 0x08, 1, 0, 0, 0]),
        vec![Event::MicAttached(true)]
    );
    assert_eq!(
        parse(Alpha, &[0x21, 0xbb, 0x08, 0]),
        vec![Event::MicAttached(false)]
    );
    assert_eq!(
        parse(Cloud2Core, &[0x66, 0x07, 1, 0, 0]),
        vec![Event::MicAttached(true)]
    );
    assert!(parse(Cloud3, &[0x66, 0x8c, 1, 0, 0]).is_empty());
    assert!(set_packets(Cloud3, SettingKey::MicSideToneVolume, &SettingValue::Int(1)).is_err());
}

#[test]
fn stinger_2_is_its_own_model_and_keeps_old_settings() {
    assert!(!CLOUD_II_HP.pids.contains(&0x0d93));
    assert_eq!(CLOUD_STINGER_2.pids, &[0x0d93]);
    assert_eq!(CLOUD_STINGER_2.family, Cloud2Hp);
    let root = std::env::temp_dir().join(format!("hako-stinger-{}", std::process::id()));
    let paths = ConfigPaths {
        root: root.clone(),
        config_toml: root.join("config.toml"),
        devices_dir: root.join("devices"),
        profiles_dir: root.join("profiles"),
        remaps_dir: root.join("remaps"),
    };
    let mut old = SettingsMap::new();
    merge_setting(&mut old, SettingKey::PmShutdown, SettingValue::Int(20));
    hako_core::config::save_device_settings(&paths, HP, 0x0696, &old).unwrap();
    assert_eq!(load_settings(&CLOUD_STINGER_2, &paths).unwrap(), old);
    // Its own file wins once written.
    let mut own = SettingsMap::new();
    merge_setting(&mut own, SettingKey::PmShutdown, SettingValue::Int(5));
    hako_core::config::save_device_settings(&paths, HP, 0x0d93, &own).unwrap();
    assert_eq!(load_settings(&CLOUD_STINGER_2, &paths).unwrap(), own);
    assert_eq!(load_settings(&CLOUD_II_HP, &paths).unwrap(), old);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn sidetone_value_per_family() {
    assert_eq!(sidetone_value(Alpha, Some(true), Some(64)), Some(64));
    assert_eq!(sidetone_value(Alpha, Some(false), Some(64)), Some(0));
    assert_eq!(sidetone_value(Cloud2Core, Some(true), Some(-5)), Some(1));
    assert_eq!(sidetone_value(Cloud3, Some(true), None), Some(1));
    assert_eq!(sidetone_value(Cloud3, None, None), None);
}

#[test]
fn every_model_has_a_control_or_battery_and_valid_values() {
    for spec in ALL {
        let f = spec.family;
        let c = controls(f);
        assert!(!c.is_empty() || has_battery(f), "{}", spec.name);
        // Every writable control maps to packets.
        for meta in c.iter().filter(|m| !m.read_only) {
            let v = meta
                .options
                .last()
                .map(|o| o.value.clone())
                .unwrap_or(match meta.kind {
                    hako_core::catalog::ControlKind::Toggle => SettingValue::Bool(true),
                    _ => SettingValue::Int(meta.max.unwrap_or(1.0) as i64),
                });
            assert!(meta.validate(&v).is_ok(), "{} {:?}", spec.name, meta.key);
            if f == Alpha2 {
                let mut b = [0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0, 0, 1];
                assert!(alpha2_apply(&mut b, meta.key, &v).is_ok());
            } else {
                assert!(
                    set_packets(f, meta.key, &v).is_ok(),
                    "{} {:?}",
                    spec.name,
                    meta.key
                );
            }
        }
    }
}

/// Read-only probe of a plugged-in headset (sends status queries only).
#[test]
#[ignore]
fn hw_poll_status() {
    let paths = ConfigPaths::xdg().unwrap();
    for spec in ALL {
        for &pid in spec.pids {
            if let Ok(mut a) = HeadsetAdapter::open(spec, pid, paths.clone()) {
                a.poll().unwrap();
                eprintln!("{} {:?} {:?}", spec.name, a.battery, a.settings);
            }
        }
    }
}
