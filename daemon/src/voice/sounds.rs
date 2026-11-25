//! Pings: short WAVs synthesised at startup into `$XDG_RUNTIME_DIR`, played
//! with `pw-play`. Also the default sink's mute while recording
//! (`advanced.mute_while_recording`).

use super::write_bytes_atomic;
use anyhow::{Context, Result};
use hako_core::voice::{SoundSettings, SoundTheme};
use std::f32::consts::TAU;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tracing::warn;

const RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Start,
    Stop,
    Cancel,
    Error,
}

impl Sound {
    const ALL: [Self; 4] = [Self::Start, Self::Stop, Self::Cancel, Self::Error];

    fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Cancel => "cancel",
            Self::Error => "error",
        }
    }

    pub fn enabled(self, s: &SoundSettings) -> bool {
        match self {
            Self::Start => s.start,
            Self::Stop => s.stop,
            Self::Cancel => s.cancel,
            Self::Error => s.error,
        }
    }
}

/// A tone: frequency glide `from` -> `to` Hz over `ms`, at `gain`.
struct Tone {
    from: f32,
    to: f32,
    ms: u32,
    gain: f32,
    /// Odd harmonics on top (a buzz instead of a sine).
    buzz: bool,
    /// Decay instead of a soft fade in and out (a click).
    click: bool,
}

const fn tone(from: f32, to: f32, ms: u32, gain: f32) -> Tone {
    Tone {
        from,
        to,
        ms,
        gain,
        buzz: false,
        click: false,
    }
}

const fn click(freq: f32, ms: u32, gain: f32) -> Tone {
    Tone {
        from: freq,
        to: freq,
        ms,
        gain,
        buzz: false,
        click: true,
    }
}

const fn buzz(freq: f32, ms: u32, gain: f32) -> Tone {
    Tone {
        from: freq,
        to: freq,
        ms,
        gain,
        buzz: true,
        click: false,
    }
}

/// Silence between tones.
const fn gap(ms: u32) -> Tone {
    tone(0.0, 0.0, ms, 0.0)
}

fn recipe(theme: SoundTheme, sound: Sound) -> Vec<Tone> {
    match (theme, sound) {
        (SoundTheme::Soft, Sound::Start) => {
            vec![tone(660.0, 660.0, 60, 0.35), tone(880.0, 880.0, 60, 0.35)]
        }
        (SoundTheme::Soft, Sound::Stop) => {
            vec![tone(880.0, 880.0, 60, 0.35), tone(660.0, 660.0, 60, 0.35)]
        }
        (SoundTheme::Soft, Sound::Cancel) => {
            vec![
                tone(392.0, 392.0, 50, 0.35),
                gap(40),
                tone(392.0, 392.0, 50, 0.35),
            ]
        }
        (SoundTheme::Soft, Sound::Error) => vec![buzz(165.0, 220, 0.25)],
        (SoundTheme::Click, Sound::Start) => {
            vec![click(2200.0, 18, 0.5), gap(30), click(2800.0, 18, 0.5)]
        }
        (SoundTheme::Click, Sound::Stop) => {
            vec![click(2800.0, 18, 0.5), gap(30), click(2200.0, 18, 0.5)]
        }
        (SoundTheme::Click, Sound::Cancel) => {
            vec![click(900.0, 20, 0.5), gap(40), click(900.0, 20, 0.5)]
        }
        (SoundTheme::Click, Sound::Error) => vec![buzz(140.0, 160, 0.25)],
    }
}

/// Mono 16-bit samples of `tones` at [`RATE`].
fn synthesize(tones: &[Tone]) -> Vec<i16> {
    let mut out = Vec::new();
    for t in tones {
        let n = (RATE * t.ms / 1000) as usize;
        let fade = (RATE as usize / 200).min(n / 2).max(1); // 5 ms
        let mut phase = 0f32;
        for i in 0..n {
            let at = i as f32 / n as f32;
            phase += TAU * (t.from + (t.to - t.from) * at) / RATE as f32;
            let mut v = phase.sin();
            if t.buzz {
                v += (3.0 * phase).sin() / 3.0 + (5.0 * phase).sin() / 5.0;
                v *= 0.7;
            }
            let envelope = if t.click {
                // 1 ms attack, then a fast decay.
                (i as f32 / (RATE / 1000) as f32).min(1.0) * (-(i as f32) / (n as f32 / 5.0)).exp()
            } else {
                // Soft edges: no click at the start or the end.
                (i.min(n - 1 - i) as f32 / fade as f32).min(1.0)
            };
            out.push((v * envelope * t.gain * i16::MAX as f32) as i16);
        }
    }
    out
}

/// A complete RIFF/WAVE file: PCM 16-bit mono at `rate`.
pub fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// f32 samples (-1..=1) as 16-bit PCM.
pub fn to_i16(pcm: &[f32]) -> Vec<i16> {
    pcm.iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

pub struct Sounds {
    dir: PathBuf,
    written: bool,
    /// pw-play children not reaped yet; the start ping is cancellable.
    playing: Vec<(Sound, Child)>,
}

impl Sounds {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            written: false,
            playing: Vec::new(),
        }
    }

    fn path(&self, theme: SoundTheme, sound: Sound) -> PathBuf {
        let theme = match theme {
            SoundTheme::Soft => "soft",
            SoundTheme::Click => "click",
        };
        self.dir
            .join(format!("hako-voice-{theme}-{}.wav", sound.name()))
    }

    /// Write every theme's WAVs (once per daemon run).
    pub fn prepare(&mut self) -> Result<()> {
        if self.written {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        for theme in [SoundTheme::Soft, SoundTheme::Click] {
            for sound in Sound::ALL {
                let path = self.path(theme, sound);
                write_bytes_atomic(&path, &wav(&synthesize(&recipe(theme, sound)), RATE))
                    .with_context(|| format!("write {}", path.display()))?;
            }
        }
        self.written = true;
        Ok(())
    }

    /// Play `sound` unless it is off; never waits for it.
    pub fn play(&mut self, sound: Sound, settings: &SoundSettings) {
        self.reap();
        if !sound.enabled(settings) || settings.volume <= 0.0 {
            return;
        }
        if let Err(e) = self.prepare() {
            warn!("voice sounds: {e:#}");
            return;
        }
        let mut cmd = Command::new("pw-play");
        cmd.arg("--volume").arg(format!("{:.3}", settings.volume));
        if let Some(sink) = &settings.sink {
            cmd.arg("--target").arg(sink);
        }
        cmd.arg(self.path(settings.theme, sound))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.spawn() {
            Ok(child) => self.playing.push((sound, child)),
            Err(e) => warn!("voice sounds: cannot run pw-play: {e}"),
        }
    }

    /// Stop a start ping still playing (a cancel right after the start).
    pub fn cancel_start(&mut self) {
        for (sound, child) in &mut self.playing {
            if *sound == Sound::Start {
                let _ = child.kill();
            }
        }
        self.reap();
    }

    fn reap(&mut self) {
        self.playing
            .retain_mut(|(_, child)| matches!(child.try_wait(), Ok(None)));
    }
}

impl Drop for Sounds {
    fn drop(&mut self) {
        for (_, child) in &mut self.playing {
            let _ = child.wait();
        }
    }
}

/// Mutes the default sink while recording and restores what it was.
#[derive(Default)]
pub struct SinkMute {
    /// We muted it (it was not muted before).
    muted: bool,
}

impl SinkMute {
    /// Mute now (no-op when it is muted already, by us or the user).
    pub fn mute(&mut self) {
        if self.muted {
            return;
        }
        match sink_muted() {
            Ok(true) => {}
            Ok(false) => {
                if let Err(e) = set_sink_mute(true) {
                    warn!("voice: mute while recording: {e:#}");
                } else {
                    self.muted = true;
                }
            }
            Err(e) => warn!("voice: mute while recording: {e:#}"),
        }
    }

    /// Unmute if we muted it.
    pub fn restore(&mut self) {
        if !std::mem::take(&mut self.muted) {
            return;
        }
        if let Err(e) = set_sink_mute(false) {
            warn!("voice: unmuting the default sink: {e:#}");
        }
    }
}

const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";

fn sink_muted() -> Result<bool> {
    let out = Command::new("wpctl")
        .args(["get-volume", DEFAULT_SINK])
        .stdin(Stdio::null())
        .output()
        .context("cannot run wpctl")?;
    anyhow::ensure!(
        out.status.success(),
        "wpctl: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(parse_muted(&String::from_utf8_lossy(&out.stdout)))
}

/// `Volume: 0.40 [MUTED]` -> true.
fn parse_muted(out: &str) -> bool {
    out.contains("[MUTED]")
}

fn set_sink_mute(on: bool) -> Result<()> {
    let out = Command::new("wpctl")
        .args(["set-mute", DEFAULT_SINK, if on { "1" } else { "0" }])
        .stdin(Stdio::null())
        .output()
        .context("cannot run wpctl")?;
    anyhow::ensure!(
        out.status.success(),
        "wpctl: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Write `pcm` (16 kHz mono) as a WAV at `path`.
pub fn write_wav(path: &Path, pcm: &[f32], rate: u32) -> Result<()> {
    write_bytes_atomic(path, &wav(&to_i16(pcm), rate))
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
    }

    #[test]
    fn every_sound_is_a_valid_short_wav() {
        for theme in [SoundTheme::Soft, SoundTheme::Click] {
            for sound in Sound::ALL {
                let samples = synthesize(&recipe(theme, sound));
                let ms = samples.len() as u32 * 1000 / RATE;
                assert!((30..=300).contains(&ms), "{theme:?} {sound:?}: {ms} ms");
                assert!(samples.iter().any(|s| s.unsigned_abs() > 1000));
                // Soft edges / decays: no hard click at either end.
                assert!(samples[0].unsigned_abs() < 2000);
                let b = wav(&samples, RATE);
                assert_eq!(&b[0..4], b"RIFF");
                assert_eq!(&b[8..16], b"WAVEfmt ");
                assert_eq!(u32_at(&b, 4) as usize, b.len() - 8);
                assert_eq!(u32_at(&b, 24), RATE);
                assert_eq!(&b[36..40], b"data");
                assert_eq!(u32_at(&b, 40) as usize, samples.len() * 2);
            }
        }
    }

    #[test]
    fn start_rises_and_stop_falls() {
        let first = |s: Sound| recipe(SoundTheme::Soft, s)[0].from;
        let last = |s: Sound| recipe(SoundTheme::Soft, s).last().unwrap().from;
        assert!(first(Sound::Start) < last(Sound::Start));
        assert!(first(Sound::Stop) > last(Sound::Stop));
    }

    #[test]
    fn prepare_writes_the_files() {
        let dir = std::env::temp_dir().join(format!("hako-voice-sounds-{}", std::process::id()));
        let mut s = Sounds::new(dir.clone());
        s.prepare().unwrap();
        let b = std::fs::read(s.path(SoundTheme::Click, Sound::Error)).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mute_state_is_read_from_wpctl() {
        assert!(parse_muted("Volume: 0.40 [MUTED]\n"));
        assert!(!parse_muted("Volume: 0.40\n"));
    }
}
