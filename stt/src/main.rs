//! hako-stt: the speech-to-text worker. hakod starts it, sends
//! `hako_core::voice::worker` frames on stdin (commands and 16 kHz mono PCM)
//! and reads results on stdout. One model at a time, one utterance session
//! at a time. Exits when stdin closes (hakod gone or unloading).

mod split;
mod wake;

use hako_core::voice::worker::{read_frame, write_msg, Frame, WorkerIn, WorkerOut};
use hako_core::voice::ModelCaps;
use split::{Event, Splitter, FRAME};
use std::io::{self, BufReader, BufWriter, Stdout};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use transcribe_cpp::{
    Backend, CancelToken, CommitPolicy, Feature, Model, ModelOptions, Pnc, RunOptions,
    StreamOptions, Task, VadOptions, VadSession,
};

/// Streaming: restart the stream at a pause this close to the model's limit.
const RESTART_MARGIN_MS: i64 = 15_000;
/// Speech kept around a VAD segment when trimming silence.
const PAD: usize = 3200;

type Out = Arc<Mutex<BufWriter<Stdout>>>;

fn send(out: &Out, msg: &WorkerOut) {
    let mut w = out.lock().unwrap_or_else(|p| p.into_inner());
    // hakod gone: the next stdin read ends the process.
    let _ = write_msg(&mut *w, msg);
}

/// The session a `Cancel` may abort mid-decode.
type Current = Arc<Mutex<Option<(u64, CancelToken)>>>;

fn main() {
    transcribe_cpp::disable_logging();
    let out: Out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let mut devices = vec!["CPU".to_string()];
    devices.extend(
        transcribe_cpp::devices()
            .into_iter()
            .filter(|d| d.kind != "cpu")
            .map(|d| d.name),
    );
    send(
        &out,
        &WorkerOut::Ready {
            version: env!("CARGO_PKG_VERSION").into(),
            devices,
        },
    );

    // Stdin is drained on its own thread so hakod never blocks writing
    // audio while a batch decode runs; Cancel aborts that decode.
    let current: Current = Arc::default();
    let (tx, rx) = mpsc::channel();
    {
        let current = current.clone();
        std::thread::spawn(move || {
            let mut stdin = BufReader::new(io::stdin().lock());
            while let Ok(Some(frame)) = read_frame::<WorkerIn>(&mut stdin) {
                if let Frame::Msg(WorkerIn::Cancel { session }) = &frame {
                    if let Some((s, token)) = &*current.lock().unwrap_or_else(|p| p.into_inner()) {
                        if s == session {
                            token.cancel();
                        }
                    }
                }
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });
    }

    let mut loaded: Option<Loaded> = None;
    while let Ok(frame) = rx.recv() {
        match frame {
            Frame::Msg(WorkerIn::Load {
                path,
                vad,
                device,
                cpu_only,
            }) => {
                loaded = None;
                match Loaded::load(&path, vad.as_deref(), device.as_deref(), cpu_only) {
                    Ok(l) => {
                        send(
                            &out,
                            &WorkerOut::Loaded {
                                caps: l.caps.clone(),
                            },
                        );
                        loaded = Some(l);
                    }
                    Err(message) => send(
                        &out,
                        &WorkerOut::Error {
                            session: None,
                            message,
                        },
                    ),
                }
            }
            Frame::Msg(WorkerIn::Unload) => loaded = None,
            Frame::Msg(WorkerIn::Begin {
                session,
                streaming,
                pause_ms,
                vad,
                language,
                translate,
                vocabulary,
                punctuation,
            }) => {
                let Some(l) = &loaded else {
                    send(
                        &out,
                        &WorkerOut::Error {
                            session: Some(session),
                            message: "no model loaded".into(),
                        },
                    );
                    continue;
                };
                let token = CancelToken::new();
                *current.lock().unwrap_or_else(|p| p.into_inner()) = Some((session, token.clone()));
                let run = l.run_options(language, translate, vocabulary, punctuation);
                let s = Session {
                    id: session,
                    out: &out,
                    rx: &rx,
                    token,
                };
                let result = if streaming && l.caps.streaming {
                    s.stream(l, &run, pause_ms)
                } else {
                    s.batch(l, &run, pause_ms, vad)
                };
                *current.lock().unwrap_or_else(|p| p.into_inner()) = None;
                if let Err(message) = result {
                    send(
                        &out,
                        &WorkerOut::Error {
                            session: Some(session),
                            message,
                        },
                    );
                }
            }
            Frame::Msg(WorkerIn::Wake {
                session,
                model_dir,
                keywords,
                threshold,
                boost,
            }) => {
                let s = Session {
                    id: session,
                    out: &out,
                    rx: &rx,
                    token: CancelToken::new(),
                };
                if let Err(message) = s.wake(&model_dir, &keywords, threshold, boost) {
                    send(
                        &out,
                        &WorkerOut::Error {
                            session: Some(session),
                            message,
                        },
                    );
                }
            }
            // Audio or an end for no session (it was cancelled): drop it.
            Frame::Msg(WorkerIn::End { .. } | WorkerIn::Cancel { .. }) | Frame::Pcm(_) => {}
        }
    }
}

struct Loaded {
    model: Model,
    vad: Option<Model>,
    caps: ModelCaps,
}

impl Loaded {
    fn load(
        path: &str,
        vad: Option<&str>,
        device: Option<&str>,
        cpu_only: bool,
    ) -> Result<Self, String> {
        let mut options = ModelOptions::default();
        if cpu_only {
            options.backend = Backend::Cpu;
        } else if let Some(name) = device {
            options.device = transcribe_cpp::devices()
                .into_iter()
                .find(|d| d.name == name);
            if options.device.is_none() && name != "CPU" {
                return Err(format!("no compute device {name:?}"));
            }
            if name == "CPU" {
                options.backend = Backend::Cpu;
            }
        }
        let model = Model::load_with(path, &options).map_err(|e| format!("load {path}: {e}"))?;
        let c = model
            .capabilities()
            .map_err(|e| format!("{path} is not a speech model: {e}"))?;
        let caps = ModelCaps {
            streaming: c.supports_streaming,
            translate: c.supports_translate,
            lang_detect: c.supports_language_detect,
            vocabulary: model.supports(Feature::Vocabulary),
            punctuation: model.supports(Feature::Pnc),
            max_audio_ms: c.max_audio_ms.max(0) as u64,
            languages: c.languages,
            arch: model.arch(),
            backend: model.backend(),
        };
        // VAD is optional: without it pauses are found by energy.
        let vad = vad.and_then(|p| {
            Model::load_with(
                p,
                &ModelOptions {
                    backend: Backend::Cpu,
                    ..Default::default()
                },
            )
            .ok()
        });
        Ok(Self { model, vad, caps })
    }

    fn run_options(
        &self,
        language: Option<String>,
        translate: bool,
        vocabulary: Vec<String>,
        punctuation: Option<bool>,
    ) -> RunOptions {
        let mut run = RunOptions {
            language,
            ..Default::default()
        };
        if translate && self.caps.translate {
            run.task = Task::Translate;
            run.target_language = Some("en".into());
        }
        if self.caps.vocabulary {
            run.vocabulary = vocabulary;
        }
        if self.caps.punctuation {
            run.pnc = match punctuation {
                Some(true) => Pnc::On,
                Some(false) => Pnc::Off,
                None => Pnc::Default,
            };
        }
        run
    }

    fn vad_session(&self) -> Option<VadSession> {
        self.vad.as_ref().and_then(|m| m.vad_session().ok())
    }
}

/// Speech / pause detection on the incoming audio, VAD or energy.
struct Detector {
    split: Splitter,
    vad: Option<VadSession>,
    /// Audio not yet judged (energy path: whole frames only).
    pending: Vec<f32>,
}

impl Detector {
    fn new(loaded: &Loaded, pause_ms: u32) -> Self {
        Self {
            split: Splitter::new(pause_ms),
            vad: loaded.vad_session(),
            pending: Vec::new(),
        }
    }

    fn push(&mut self, pcm: &[f32]) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(vad) = &mut self.vad {
            if let Ok(r) = vad.stream_feed(pcm) {
                events.extend(r.probs.iter().filter_map(|p| self.split.push_prob(*p)));
                return events;
            }
            // A VAD failure: fall back to energy for the rest of the session.
            self.vad = None;
        }
        self.pending.extend_from_slice(pcm);
        let whole = self.pending.len() / FRAME * FRAME;
        for f in self.pending[..whole].chunks(FRAME) {
            events.extend(self.split.push_energy(f));
        }
        self.pending.drain(..whole);
        events
    }
}

struct Session<'a> {
    id: u64,
    out: &'a Out,
    rx: &'a Receiver<Frame<WorkerIn>>,
    token: CancelToken,
}

/// What ended the input of a session.
enum Input {
    Pcm(Vec<f32>),
    End,
    Cancel,
}

impl Session<'_> {
    fn next(&self) -> Input {
        loop {
            match self.rx.recv() {
                Ok(Frame::Pcm(p)) => return Input::Pcm(p),
                Ok(Frame::Msg(WorkerIn::End { session })) if session == self.id => {
                    return Input::End
                }
                Ok(Frame::Msg(WorkerIn::Cancel { session })) if session == self.id => {
                    return Input::Cancel
                }
                // hakod is gone.
                Err(_) => return Input::Cancel,
                // Stale frames of an older session.
                Ok(_) => {}
            }
        }
    }

    fn speech(&self, events: &[Event]) {
        for e in events {
            let active = matches!(e, Event::SpeechStart);
            send(
                self.out,
                &WorkerOut::Speech {
                    session: self.id,
                    active,
                },
            );
        }
    }

    /// Listen for wake words until `End` / `Cancel`, with speech start / stop
    /// (energy) so hakod can tell where the command after it ends.
    fn wake(
        &self,
        dir: &str,
        keywords: &[String],
        threshold: f32,
        boost: f32,
    ) -> Result<(), String> {
        let mut spotter =
            wake::Spotter::new(std::path::Path::new(dir), keywords, threshold, boost)?;
        let mut split = Splitter::new(700);
        let mut pending: Vec<f32> = Vec::new();
        loop {
            match self.next() {
                Input::Cancel | Input::End => return Ok(()),
                Input::Pcm(pcm) => {
                    if let Some(keyword) = spotter.feed(&pcm) {
                        send(
                            self.out,
                            &WorkerOut::Wake {
                                session: self.id,
                                keyword,
                            },
                        );
                    }
                    pending.extend_from_slice(&pcm);
                    let whole = pending.len() / FRAME * FRAME;
                    let events: Vec<Event> = pending[..whole]
                        .chunks(FRAME)
                        .filter_map(|f| split.push_energy(f))
                        // Only pauses long enough to end a command count.
                        .filter(|e| !matches!(e, Event::SpeechEnd { cut: None }))
                        .collect();
                    pending.drain(..whole);
                    self.speech(&events);
                }
            }
        }
    }

    /// A streaming model: live partials; at a pause near the model's input
    /// limit the stream is finished and a new one continues the text.
    fn stream(&self, l: &Loaded, run: &RunOptions, pause_ms: Option<u32>) -> Result<(), String> {
        let mut session = l.model.session().map_err(|e| e.to_string())?;
        session.set_cancel_token(&self.token);
        let mut detect = Detector::new(l, pause_ms.unwrap_or(500));
        let limit = l.caps.max_audio_ms as i64;
        let opts = StreamOptions {
            commit_policy: CommitPolicy::Auto,
            ..Default::default()
        };
        // Text of finished streams of this session.
        let mut before = String::new();
        loop {
            let mut stream = session.stream(run, &opts).map_err(|e| e.to_string())?;
            let ended = loop {
                match self.next() {
                    Input::Cancel => return Ok(()),
                    Input::End => break true,
                    Input::Pcm(pcm) => {
                        let events = detect.push(&pcm);
                        self.speech(&events);
                        let u = stream.feed(&pcm).map_err(|e| e.to_string())?;
                        let received_ms = u.input_received_ms;
                        if u.committed_changed || u.tentative_changed {
                            let t = stream.text();
                            send(
                                self.out,
                                &WorkerOut::Partial {
                                    session: self.id,
                                    committed: join(&before, &t.committed),
                                    tentative: t.tentative,
                                },
                            );
                        }
                        let paused = events.iter().any(|e| matches!(e, Event::SpeechEnd { .. }));
                        if limit > 0 && received_ms > limit - RESTART_MARGIN_MS && paused {
                            break false;
                        }
                    }
                }
            };
            stream.finalize().map_err(|e| e.to_string())?;
            before = join(&before, stream.text().full.trim());
            if self.token.is_cancelled() {
                return Ok(());
            }
            if ended {
                send(
                    self.out,
                    &WorkerOut::Final {
                        session: self.id,
                        text: before,
                        language: None,
                    },
                );
                return Ok(());
            }
        }
    }

    /// A batch model: decode at pauses (when asked) and at the end; an
    /// utterance longer than the model's limit is cut at its quietest point.
    fn batch(
        &self,
        l: &Loaded,
        run: &RunOptions,
        pause_ms: Option<u32>,
        vad: bool,
    ) -> Result<(), String> {
        let mut session = l.model.session().map_err(|e| e.to_string())?;
        session.set_cancel_token(&self.token);
        let mut detect = Detector::new(l, pause_ms.unwrap_or(500));
        let mut trim = if vad { l.vad_session() } else { None };
        let max = match l.caps.max_audio_ms {
            0 => usize::MAX,
            ms => (ms as usize).saturating_sub(2000) * 16,
        };
        let mut pcm: Vec<f32> = Vec::new();
        let mut start = 0;
        let mut texts: Vec<String> = Vec::new();
        let mut language = None;
        loop {
            match self.next() {
                Input::Cancel => return Ok(()),
                Input::End => break,
                Input::Pcm(chunk) => {
                    let events = detect.push(&chunk);
                    self.speech(&events);
                    pcm.extend_from_slice(&chunk);
                    let mut cut = None;
                    if pause_ms.is_some() {
                        cut = events.iter().rev().find_map(|e| match e {
                            Event::SpeechEnd { cut: Some(c) } if *c > start => {
                                Some((*c).min(pcm.len()))
                            }
                            _ => None,
                        });
                    }
                    if cut.is_none() && pcm.len() - start > max {
                        cut = Some(split::quietest(&pcm, start + max / 2, pcm.len()));
                    }
                    if let Some(c) = cut {
                        let audio = speech_only(&pcm[start..c], trim.as_mut());
                        start = c;
                        if let Some(text) =
                            self.decode(&mut session, &audio, run, &mut texts, &mut language)?
                        {
                            send(
                                self.out,
                                &WorkerOut::Segment {
                                    session: self.id,
                                    text,
                                },
                            );
                        }
                    }
                }
            }
        }
        let audio = speech_only(&pcm[start..], trim.as_mut());
        self.decode(&mut session, &audio, run, &mut texts, &mut language)?;
        if self.token.is_cancelled() {
            return Ok(());
        }
        send(
            self.out,
            &WorkerOut::Final {
                session: self.id,
                text: texts.join(" "),
                language,
            },
        );
        Ok(())
    }

    fn decode(
        &self,
        session: &mut transcribe_cpp::Session,
        audio: &[f32],
        run: &RunOptions,
        texts: &mut Vec<String>,
        language: &mut Option<String>,
    ) -> Result<Option<String>, String> {
        // Shorter than 100 ms of speech: nothing to transcribe.
        if audio.len() < 1600 {
            return Ok(None);
        }
        match session.run(audio, run) {
            Ok(t) => {
                if t.language.is_some() {
                    *language = t.language;
                }
                let text = t.text.trim().to_string();
                if text.is_empty() {
                    return Ok(None);
                }
                texts.push(text.clone());
                Ok(Some(text))
            }
            Err(_) if self.token.is_cancelled() => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// `pcm` without its silence (VAD segments, padded); all of it without VAD
/// or when the VAD fails. Nothing when the VAD hears no speech.
fn speech_only(pcm: &[f32], vad: Option<&mut VadSession>) -> Vec<f32> {
    let Some(vad) = vad else {
        return pcm.to_vec();
    };
    let opts = VadOptions {
        speech_pad_ms: 200,
        ..Default::default()
    };
    let Ok(r) = vad.run(pcm, &opts) else {
        return pcm.to_vec();
    };
    let mut out = Vec::new();
    let mut last_end = 0;
    for s in r.segments {
        let a = (s.start_sample.max(0) as usize)
            .saturating_sub(PAD)
            .max(last_end);
        let b = (s.end_sample.max(0) as usize + PAD).min(pcm.len());
        if a < b {
            out.extend_from_slice(&pcm[a..b]);
            last_end = b;
        }
    }
    out
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a} {b}"),
    }
}
