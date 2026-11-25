//! Where to cut a recording into utterances: speech / silence per frame
//! (Silero VAD probabilities, or signal energy without a VAD model) and the
//! pause rule. Pure, so it is tested without a model.

/// Samples per decision frame (32 ms at 16 kHz, Silero's frame).
pub const FRAME: usize = 512;
const RATE_MS: usize = 16;

/// Speech after a pause this long is a new utterance.
pub struct Splitter {
    pause_frames: usize,
    /// Frames seen so far (absolute, from the session start).
    frames: usize,
    speaking: bool,
    heard_speech: bool,
    silent_run: usize,
    /// Energy fallback: running noise floor (RMS).
    floor: f32,
}

/// What one frame decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Speech started at this frame.
    SpeechStart,
    /// Speech stopped; `cut` is the sample to cut at (the middle of the
    /// pause) when the pause was long enough to end an utterance.
    SpeechEnd { cut: Option<usize> },
}

impl Splitter {
    pub fn new(pause_ms: u32) -> Self {
        Self {
            pause_frames: (pause_ms as usize).div_ceil(FRAME / RATE_MS),
            frames: 0,
            speaking: false,
            heard_speech: false,
            silent_run: 0,
            floor: 0.003,
        }
    }

    /// One frame's speech probability (0..=1).
    pub fn push_prob(&mut self, prob: f32) -> Option<Event> {
        self.step(prob >= 0.5)
    }

    /// One frame of audio, judged by energy against an adaptive floor.
    pub fn push_energy(&mut self, frame: &[f32]) -> Option<Event> {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt();
        // The floor follows quiet frames quickly and loud ones slowly.
        let rate = if rms < self.floor { 0.2 } else { 0.005 };
        self.floor += (rms - self.floor) * rate;
        let speech = rms > (self.floor * 3.0).max(0.01);
        self.step(speech)
    }

    fn step(&mut self, speech: bool) -> Option<Event> {
        self.frames += 1;
        if speech {
            self.silent_run = 0;
            self.heard_speech = true;
            if !self.speaking {
                self.speaking = true;
                return Some(Event::SpeechStart);
            }
            return None;
        }
        self.silent_run += 1;
        // Three silent frames end speech; the pause rule may also cut.
        let mut event = None;
        if self.speaking && self.silent_run >= 3 {
            self.speaking = false;
            event = Some(Event::SpeechEnd { cut: None });
        }
        if self.heard_speech && self.silent_run == self.pause_frames.max(3) {
            self.heard_speech = false;
            // Both at once: the cut implies the stop.
            let cut = (self.frames - self.pause_frames / 2) * FRAME;
            return Some(Event::SpeechEnd { cut: Some(cut) });
        }
        event
    }
}

/// The quietest 100 ms window start in `pcm[from..to]` (a forced cut when an
/// utterance outgrows the model's longest input).
pub fn quietest(pcm: &[f32], from: usize, to: usize) -> usize {
    const WIN: usize = 1600;
    let to = to.min(pcm.len());
    if to <= from + WIN {
        return to;
    }
    (from..to - WIN)
        .step_by(WIN / 4)
        .min_by(|&a, &b| {
            let e = |i: usize| pcm[i..i + WIN].iter().map(|s| s * s).sum::<f32>();
            e(a).total_cmp(&e(b))
        })
        .map_or(to, |i| i + WIN / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize, amp: f32) -> Vec<Vec<f32>> {
        (0..n)
            .map(|i| {
                (0..FRAME)
                    .map(|j| amp * (((i * FRAME + j) as f32) * 0.3).sin())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_long_pause_after_speech_cuts_once() {
        let mut s = Splitter::new(500);
        let mut events = Vec::new();
        for f in frames(20, 0.0005)
            .into_iter()
            .chain(frames(30, 0.3))
            .chain(frames(40, 0.0005))
        {
            events.extend(s.push_energy(&f));
        }
        assert_eq!(events[0], Event::SpeechStart);
        let cuts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::SpeechEnd { cut: Some(c) } => Some(*c),
                _ => None,
            })
            .collect();
        assert_eq!(cuts.len(), 1, "{events:?}");
        // In the pause, after the speech (frames 20..50).
        assert!(cuts[0] > 50 * FRAME && cuts[0] < 70 * FRAME, "{}", cuts[0]);
    }

    #[test]
    fn silence_alone_never_cuts() {
        let mut s = Splitter::new(300);
        assert!((0..100).all(|_| s.push_prob(0.1).is_none()));
    }

    #[test]
    fn probabilities_drive_the_same_rule() {
        let mut s = Splitter::new(320);
        assert_eq!(s.push_prob(0.9), Some(Event::SpeechStart));
        let mut cut = None;
        for _ in 0..20 {
            if let Some(Event::SpeechEnd { cut: Some(c) }) = s.push_prob(0.1) {
                cut = Some(c);
            }
        }
        assert!(cut.is_some());
    }

    #[test]
    fn quietest_finds_the_gap() {
        let mut pcm = vec![0.5f32; 16000 * 3];
        pcm[16000..17600].fill(0.0);
        let c = quietest(&pcm, 0, pcm.len());
        assert!((16000..17600 + 800).contains(&c), "{c}");
    }
}
