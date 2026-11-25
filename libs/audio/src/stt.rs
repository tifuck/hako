//! Voice capture: the `hako_stt` PipeWire stream (16 kHz mono f32, PipeWire
//! resamples) on its own thread and connection. It exists only while the
//! voice service holds an [`SttCapture`], so the desktop's mic-in-use
//! indicator is truthful.

use crate::engine::engine;
use anyhow::{anyhow, Result};
use hako_core::voice::{CAPTURE_NODE, SAMPLE_RATE};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use tracing::{debug, warn};

/// Description the stream shows in pavucontrol / the desktop.
pub const CAPTURE_DESCRIPTION: &str = "hako voice";

/// An open capture stream; dropping it closes the stream and its thread.
pub struct SttCapture {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    failed: Arc<AtomicBool>,
}

impl SttCapture {
    /// Capture from `target` (a source's `node.name`; `None` = the default
    /// source). `on_pcm` gets every block on the stream's thread and must
    /// not block it.
    pub fn open(target: Option<&str>, on_pcm: impl FnMut(&[f32]) + Send + 'static) -> Result<Self> {
        let (quit, rx) = pw::channel::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let target = target.map(str::to_string);
        let thread = std::thread::Builder::new()
            .name("hako-stt-capture".into())
            .spawn({
                let failed = failed.clone();
                move || {
                    if let Err(e) = run(rx, target, on_pcm, failed, &ready) {
                        let _ = ready.send(Err(e.to_string()));
                    }
                }
            })
            .map_err(|e| anyhow!("cannot start the capture thread: {e}"))?;
        match started.recv() {
            Ok(Ok(())) => Ok(Self {
                quit,
                thread: Some(thread),
                failed,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(anyhow!("voice capture: {e}"))
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("voice capture: the thread ended"))
            }
        }
    }

    /// The stream errored or PipeWire went away (no more audio comes).
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

impl Drop for SttCapture {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    rx: pw::channel::Receiver<()>,
    target: Option<String>,
    mut on_pcm: impl FnMut(&[f32]) + 'static,
    failed: Arc<AtomicBool>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let _core_listener = core
        .add_listener_local()
        .error({
            let failed = failed.clone();
            move |id, _seq, res, message| {
                if crate::engine::connection_lost(id, res) {
                    debug!("voice capture: core error {res}: {message}");
                    failed.store(true, Ordering::Relaxed);
                }
            }
        })
        .register();
    let _rx = rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    let stream =
        pw::stream::StreamRc::new(core.clone(), CAPTURE_NODE, stream_props(target.as_deref()))?;
    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let failed = failed.clone();
            move |_, _, _, state| {
                if let pw::stream::StreamState::Error(err) = state {
                    warn!("voice capture failed: {err}");
                    failed.store(true, Ordering::Relaxed);
                }
            }
        })
        .process({
            let mut block = Vec::new();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let (offset, size) = (data.chunk().offset(), data.chunk().size());
                let Some(bytes) = data.data() else {
                    return;
                };
                decode(crate::chunk_bytes(bytes, offset, size), &mut block);
                if !block.is_empty() {
                    on_pcm(&block);
                }
            }
        })
        .register()?;
    let pod = format_pod();
    let mut params = [spa::pod::Pod::from_bytes(&pod).expect("format pod parses")];
    // No RT_PROCESS: `process` runs on this thread's loop, so `on_pcm` may
    // allocate (it hands blocks to the voice service).
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    debug!(
        "voice capture: open on {}",
        target.as_deref().unwrap_or("the default source")
    );
    let _ = ready.send(Ok(()));
    mainloop.run();
    // The listener goes before the stream, both before the loop.
    drop(listener);
    drop(stream);
    Ok(())
}

fn stream_props(target: Option<&str>) -> pw::properties::PropertiesBox {
    let mut props = properties! {
        "media.type" => "Audio",
        "media.category" => "Capture",
        "node.name" => CAPTURE_NODE,
        "node.description" => CAPTURE_DESCRIPTION,
        "application.name" => "hako",
    };
    if let Some(target) = target {
        props.insert("target.object", target);
        // WirePlumber 0.4 (Ubuntu 22.04) reads the older key.
        props.insert("node.target", target);
    }
    props
}

/// F32 mono at [`SAMPLE_RATE`]; the adapter converts from the source.
fn format_pod() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(1);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("format pod serializes")
    .0
    .into_inner()
}

/// F32LE samples of `bytes` into `out` (a torn trailing sample is dropped).
fn decode(bytes: &[u8], out: &mut Vec<f32>) {
    out.clear();
    out.extend(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|s| f32::from_le_bytes(*s)),
    );
}

/// Capture sources for the mic picker: (`node.name`, description), sorted
/// by description. Mixer virtual mics (`Hako_Mic`, ...) are included: they
/// carry the mixer's effects (noise removal) into dictation.
pub fn sources() -> Result<Vec<(String, String)>> {
    Ok(engine()?.with_graph(|graph| {
        let mut list: Vec<(String, String)> = graph
            .nodes
            .values()
            .filter(|node| node.media_class == "Audio/Source")
            .map(|node| {
                let description = ["node.description", "node.nick"]
                    .iter()
                    .find_map(|key| node.props.get(*key))
                    .unwrap_or(&node.name)
                    .clone();
                (node.name.clone(), description)
            })
            .collect();
        list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        list
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_reads_f32le_and_drops_a_torn_sample() {
        let bytes: Vec<u8> = [0.5f32, -0.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut out = vec![9.0];
        decode(&bytes, &mut out);
        assert_eq!(out, [0.5, -0.25]);
        decode(&bytes[..6], &mut out);
        assert_eq!(out, [0.5]);
    }

    #[test]
    fn props_name_the_node_and_target_the_source() {
        let props = stream_props(Some("alsa_input.usb-mic"));
        assert_eq!(props.get("node.name"), Some(CAPTURE_NODE));
        assert_eq!(props.get("node.description"), Some(CAPTURE_DESCRIPTION));
        assert_eq!(props.get("target.object"), Some("alsa_input.usb-mic"));
        assert_eq!(stream_props(None).get("target.object"), None);
    }

    /// Opens the stream on the default source and reads some audio.
    #[test]
    #[ignore = "needs a live PipeWire session with a source"]
    fn live_capture_delivers_audio() {
        let (tx, rx) = std::sync::mpsc::channel();
        let capture = SttCapture::open(None, move |pcm| {
            let _ = tx.send(pcm.len());
        })
        .unwrap();
        let mut got = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while got < SAMPLE_RATE as usize / 2 && std::time::Instant::now() < deadline {
            got += rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap_or(0);
        }
        assert!(got >= SAMPLE_RATE as usize / 2, "only {got} samples");
        assert!(!capture.failed());
        assert!(!sources().unwrap().is_empty());
    }
}
