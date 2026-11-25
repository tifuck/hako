//! Recent transcripts: `~/.local/share/hako/voice/history.jsonl` (one
//! `HistoryEntry` per line, oldest first), with an optional 16 kHz WAV per
//! entry next to it.

use super::sounds::write_wav;
use super::write_bytes_atomic;
use anyhow::{Context, Result};
use hako_core::voice::{HistoryEntry, SAMPLE_RATE};
use std::path::{Path, PathBuf};
use tracing::warn;

const FILE: &str = "history.jsonl";

pub struct History {
    dir: PathBuf,
}

impl History {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Every entry, oldest first; unreadable lines are skipped.
    pub fn load(&self) -> Vec<HistoryEntry> {
        let text = match std::fs::read_to_string(self.dir.join(FILE)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                warn!("voice history: {e}");
                return Vec::new();
            }
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Append a transcript (with its audio when `audio` is given), keeping
    /// the newest `keep`; 0 keeps nothing and deletes the history.
    pub fn add(
        &self,
        mut entry: HistoryEntry,
        audio: Option<(&[f32], u64)>,
        keep: u32,
    ) -> Result<Vec<HistoryEntry>> {
        if keep == 0 {
            self.clear()?;
            return Ok(Vec::new());
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        if let Some((pcm, session)) = audio {
            let name = format!("{}-{session}.wav", entry.at);
            write_wav(&self.dir.join(&name), pcm, SAMPLE_RATE)?;
            entry.audio = Some(name);
        }
        let mut entries = self.load();
        entries.push(entry);
        self.save(entries, keep)
    }

    /// Keep the newest `keep` entries (0: delete the history).
    pub fn trim(&self, keep: u32) -> Result<Vec<HistoryEntry>> {
        if keep == 0 {
            self.clear()?;
            return Ok(Vec::new());
        }
        let entries = self.load();
        if entries.len() <= keep as usize {
            return Ok(entries);
        }
        self.save(entries, keep)
    }

    fn save(&self, mut entries: Vec<HistoryEntry>, keep: u32) -> Result<Vec<HistoryEntry>> {
        let drop = entries.len().saturating_sub(keep as usize);
        for old in entries.drain(..drop) {
            self.remove_audio(&old);
        }
        let mut text = String::new();
        for e in &entries {
            text.push_str(&serde_json::to_string(e)?);
            text.push('\n');
        }
        let path = self.dir.join(FILE);
        write_bytes_atomic(&path, text.as_bytes())?;
        Ok(entries)
    }

    /// Delete every entry and its audio.
    pub fn clear(&self) -> Result<()> {
        for e in self.load() {
            self.remove_audio(&e);
        }
        match std::fs::remove_file(self.dir.join(FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("delete {}", self.dir.join(FILE).display()))
            }
            _ => Ok(()),
        }
    }

    fn remove_audio(&self, e: &HistoryEntry) {
        // A plain file name only: the file may have been edited by hand.
        if let Some(name) = e.audio.as_deref().filter(|n| plain_name(n)) {
            let _ = std::fs::remove_file(self.dir.join(name));
        }
    }
}

fn plain_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && Path::new(name).file_name() == Some(name.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: u64, text: &str) -> HistoryEntry {
        HistoryEntry {
            at,
            text: text.into(),
            model: "m".into(),
            audio: None,
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hako-voice-hist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn keeps_the_newest_entries_and_their_audio() {
        let d = dir("keep");
        let h = History::new(d.clone());
        h.add(entry(1, "one"), Some((&[0.0; 160], 1)), 2).unwrap();
        h.add(entry(2, "two"), None, 2).unwrap();
        let got = h.add(entry(3, "three"), Some((&[0.5; 160], 3)), 2).unwrap();
        assert_eq!(
            got.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["two", "three"]
        );
        assert_eq!(h.load(), got);
        // The dropped entry's WAV went with it; the kept one is there.
        assert!(!d.join("1-1.wav").exists());
        assert!(d.join("3-3.wav").exists());
        assert_eq!(&std::fs::read(d.join("3-3.wav")).unwrap()[0..4], b"RIFF");

        assert_eq!(h.trim(1).unwrap().len(), 1);
        assert_eq!(h.load()[0].text, "three");
        // 0 = off: everything goes.
        assert!(h.trim(0).unwrap().is_empty());
        assert!(!d.join(FILE).exists());
        assert!(!d.join("3-3.wav").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn bad_lines_are_skipped_and_audio_paths_stay_inside() {
        let d = dir("bad");
        std::fs::create_dir_all(&d).unwrap();
        let outside = d.with_extension("keep");
        std::fs::write(&outside, "x").unwrap();
        let mut evil = entry(1, "evil");
        evil.audio = Some(format!(
            "../{}",
            outside.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(
            d.join(FILE),
            format!("not json\n{}\n", serde_json::to_string(&evil).unwrap()),
        )
        .unwrap();
        let h = History::new(d.clone());
        assert_eq!(h.load(), [evil]);
        h.clear().unwrap();
        assert!(outside.exists());
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(d);
    }
}
