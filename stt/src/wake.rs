//! Wake words: sherpa-onnx open-vocabulary keyword spotting. The phrase the
//! user typed is split into the model's word pieces here (sentencepiece
//! unigram, Viterbi over the piece scores), so any word works untrained.

use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig, OnlineStream};
use std::collections::HashMap;
use std::path::Path;

/// The pieces of a sentencepiece `ModelProto` (field 1, repeated
/// `SentencePiece { piece = 1, score = 2, type = 3 }`) usable in text:
/// normal (type 1, the default) and user-defined (4 is control, 2 unknown).
pub fn parse_pieces(b: &[u8]) -> Result<HashMap<String, f32>, String> {
    let mut out = HashMap::new();
    let mut r = Reader { b, i: 0 };
    while !r.done() {
        let (field, wire) = r.key()?;
        if field == 1 && wire == 2 {
            let sub = r.bytes()?;
            let mut s = Reader { b: sub, i: 0 };
            let (mut piece, mut score, mut kind) = (String::new(), 0f32, 1u64);
            while !s.done() {
                match s.key()? {
                    (1, 2) => piece = String::from_utf8_lossy(s.bytes()?).into_owned(),
                    (2, 5) => score = f32::from_le_bytes(s.fixed::<4>()?),
                    (3, 0) => kind = s.varint()?,
                    (_, w) => s.skip(w)?,
                }
            }
            if kind == 1 || kind == 4 {
                out.insert(piece, score);
            }
        } else {
            r.skip(wire)?;
        }
    }
    if out.is_empty() {
        return Err("no word pieces in the model".into());
    }
    Ok(out)
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let x = *self.b.get(self.i).ok_or("truncated varint")?;
            self.i += 1;
            v |= u64::from(x & 0x7f) << shift;
            if x < 0x80 {
                return Ok(v);
            }
        }
        Err("varint too long".into())
    }

    fn key(&mut self) -> Result<(u64, u64), String> {
        let k = self.varint()?;
        Ok((k >> 3, k & 7))
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = self.varint()? as usize;
        let end = self
            .i
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or("truncated field")?;
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let end = self.i + N;
        let s = self.b.get(self.i..end).ok_or("truncated field")?;
        self.i = end;
        Ok(s.try_into().expect("N bytes"))
    }

    fn skip(&mut self, wire: u64) -> Result<(), String> {
        match wire {
            0 => self.varint().map(drop),
            1 => self.fixed::<8>().map(drop),
            2 => self.bytes().map(drop),
            5 => self.fixed::<4>().map(drop),
            w => Err(format!("unsupported wire type {w}")),
        }
    }
}

/// `text` as word pieces: per word, the split with the highest total score
/// (`▁` marks a word start). A character with no piece stays on its own.
pub fn encode(text: &str, pieces: &HashMap<String, f32>) -> Vec<String> {
    const LONGEST: usize = 16;
    let mut out = Vec::new();
    for word in text.split_whitespace() {
        let chars: Vec<char> = std::iter::once('▁').chain(word.chars()).collect();
        let n = chars.len();
        let mut best = vec![(f32::NEG_INFINITY, 0usize); n + 1];
        best[0].0 = 0.0;
        for end in 1..=n {
            for start in end.saturating_sub(LONGEST)..end {
                if best[start].0 == f32::NEG_INFINITY {
                    continue;
                }
                let p: String = chars[start..end].iter().collect();
                let score = match pieces.get(&p) {
                    Some(s) => *s,
                    None if end - start == 1 => -100.0,
                    None => continue,
                };
                if best[start].0 + score > best[end].0 {
                    best[end] = (best[start].0 + score, start);
                }
            }
        }
        let mut split = Vec::new();
        let mut end = n;
        while end > 0 {
            let start = best[end].1;
            split.push(chars[start..end].iter().collect::<String>());
            end = start;
        }
        out.extend(split.into_iter().rev());
    }
    out
}

/// The spotter's keyword lines: pieces, boost, threshold, `@tag`.
pub fn keyword_lines(
    phrases: &[String],
    pieces: &HashMap<String, f32>,
    threshold: f32,
    boost: f32,
) -> String {
    phrases
        .iter()
        .map(|p| {
            let upper = p.trim().to_uppercase();
            format!(
                "{} :{boost:.2} #{threshold:.2} @{}\n",
                encode(&upper, pieces).join(" "),
                upper.replace(' ', "_")
            )
        })
        .collect()
}

pub struct Spotter {
    kws: KeywordSpotter,
    stream: OnlineStream,
}

impl Spotter {
    pub fn new(dir: &Path, phrases: &[String], threshold: f32, boost: f32) -> Result<Self, String> {
        let k = &hako_core::voice::CATALOG.kws;
        let file = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
        let pieces = std::fs::read(dir.join(&k.pieces))
            .map_err(|e| format!("wake-word model: {e}"))
            .and_then(|b| parse_pieces(&b))?;
        let mut cfg = KeywordSpotterConfig::default();
        cfg.model_config.transducer.encoder = file(&k.encoder);
        cfg.model_config.transducer.decoder = file(&k.decoder);
        cfg.model_config.transducer.joiner = file(&k.joiner);
        cfg.model_config.tokens = file(&k.tokens);
        cfg.model_config.num_threads = 1;
        cfg.keywords_buf = Some(keyword_lines(phrases, &pieces, threshold, boost));
        let kws = KeywordSpotter::create(&cfg).ok_or("the wake-word model could not be loaded")?;
        let stream = kws.create_stream();
        Ok(Self { kws, stream })
    }

    /// Feed 16 kHz audio; the phrase (lowercase) when a wake word ends here.
    pub fn feed(&mut self, pcm: &[f32]) -> Option<String> {
        self.stream.accept_waveform(16_000, pcm);
        let mut heard = None;
        while self.kws.is_ready(&self.stream) {
            self.kws.decode(&self.stream);
            if let Some(r) = self.kws.get_result(&self.stream) {
                if !r.keyword.is_empty() {
                    heard = Some(r.keyword.replace('_', " ").to_lowercase());
                    self.kws.reset(&self.stream);
                }
            }
        }
        heard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto(pieces: &[(&str, f32, Option<u64>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (p, s, t) in pieces {
            let mut sub = vec![0x0a, p.len() as u8];
            sub.extend_from_slice(p.as_bytes());
            sub.push(0x15);
            sub.extend_from_slice(&s.to_le_bytes());
            if let Some(t) = t {
                sub.extend_from_slice(&[0x18, *t as u8]);
            }
            out.extend_from_slice(&[0x0a, sub.len() as u8]);
            out.extend(sub);
        }
        // An unrelated field (trainer spec) is skipped.
        out.extend_from_slice(&[0x12, 0x02, 0x08, 0x01]);
        out
    }

    #[test]
    fn parses_pieces_and_skips_control_ones() {
        let p = parse_pieces(&proto(&[
            ("<unk>", 0.0, Some(2)),
            ("▁HE", -3.0, None),
            ("LL", -4.0, None),
        ]))
        .unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p["▁HE"], -3.0);
        assert!(parse_pieces(&[0x0a, 0x09]).is_err(), "truncated");
    }

    #[test]
    fn viterbi_picks_the_best_total_score() {
        let pieces: HashMap<String, f32> = [
            ("▁HE", -3.0),
            ("LL", -4.0),
            ("O", -2.0),
            ("▁H", -5.0),
            ("E", -2.0),
            ("L", -3.0),
            ("▁WORLD", -7.7),
            ("▁W", -5.7),
            ("OR", -5.3),
            ("▁", -2.0),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b))
        .collect();
        assert_eq!(encode("HELLO WORLD", &pieces).join(" "), "▁HE LL O ▁WORLD");
        // A letter with no piece stays on its own.
        assert_eq!(encode("HEX", &pieces).join(" "), "▁HE X");
        let lines = keyword_lines(&["hello world".into()], &pieces, 0.25, 1.5);
        assert_eq!(lines, "▁HE LL O ▁WORLD :1.50 #0.25 @HELLO_WORLD\n");
    }

    /// Needs the downloaded model: HAKO_KWS_DIR=<extracted dir>.
    /// `cargo test -p hako-stt live_wake -- --ignored`
    #[test]
    #[ignore]
    fn live_wake_tokenizes_like_the_reference() {
        let dir = std::env::var("HAKO_KWS_DIR").expect("HAKO_KWS_DIR");
        let dir = Path::new(&dir);
        let pieces = parse_pieces(&std::fs::read(dir.join("bpe.model")).unwrap()).unwrap();
        let raw = std::fs::read_to_string(dir.join("keywords_raw.txt")).unwrap();
        let want = std::fs::read_to_string(dir.join("keywords.txt")).unwrap();
        for (r, w) in raw.lines().zip(want.lines()) {
            assert_eq!(encode(r, &pieces).join(" "), w.trim(), "{r}");
        }
        Spotter::new(dir, &["jarvis".into()], 0.25, 1.5).unwrap();
    }
}
