//! hakod — always-on device daemon.

mod drivers;
mod hid;
mod hub;
mod ipc_server;
mod mixer;
mod net;
mod remap;
mod worker;

use anyhow::Result;
use clap::Parser;
use hako_audio::latency::{run_latency_test, LatencyPath, LatencyTestOptions};
use hako_audio::AudioService;
use hako_core::config::{load_app_config, ConfigPaths};
use hako_core::default_socket_path;
use hako_core::ipc::{audio_latency_samples, IpcRequest};
use hako_core::{read_json_frame_sync, write_json_frame_sync};
use hub::{DaemonHub, DISCOVERY_PERIOD, LOG_LEVELS};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "hakod", version, about = "hako device daemon")]
struct Cli {
    /// Reset PipeWire ChatMix/AUX graph and exit (uses running daemon if available)
    #[arg(long)]
    reset_audio: bool,

    /// Log level; default: `log_level` from config.toml, else info
    #[arg(long)]
    log_level: Option<String>,

    /// Override socket path
    #[arg(long)]
    socket: Option<std::path::PathBuf>,

    /// Print udev rules for every supported device and exit
    #[arg(long)]
    udev_rules: bool,

    /// Print the supported-device list (Markdown) and exit
    #[arg(long)]
    list_devices: bool,

    /// Report every connected HID device and what handles it, then exit
    #[arg(long)]
    diagnose: bool,

    /// Measure the latency the default processing chain (or a mixer path)
    /// adds over a direct link (software only, no device plays anything);
    /// exit 1 if over one quantum
    #[arg(long)]
    latency_test: bool,

    /// With --latency-test: `chain` (gate, EQ, compressor in one
    /// filter-chain) or `mixer` (a virtual input with that chain routed to a
    /// virtual output)
    #[arg(long, default_value = "chain", value_parser = ["chain", "mixer"], requires = "latency_test")]
    path: String,

    /// With --latency-test: the `node.latency` preset
    #[arg(long, default_value = "balanced", value_parser = ["low", "balanced", "safe"], requires = "latency_test")]
    latency: String,

    /// With --latency-test: seconds to measure
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=60), requires = "latency_test")]
    seconds: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.udev_rules {
        print!("{}", drivers::udev_rules());
        return Ok(());
    }
    if cli.list_devices {
        print!("{}", drivers::device_list());
        return Ok(());
    }
    if cli.diagnose {
        print!("{}", drivers::diagnose()?);
        return Ok(());
    }
    let paths = ConfigPaths::xdg()?;
    // RUST_LOG, then --log-level, then the Settings page value.
    let configured = load_app_config(&paths)
        .map(|c| c.log_level)
        .unwrap_or_default();
    let level = cli.log_level.clone().unwrap_or_else(|| {
        if LOG_LEVELS.contains(&configured.as_str()) {
            configured
        } else {
            "info".into()
        }
    });
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&level));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let socket = cli.socket.unwrap_or_else(default_socket_path);

    if cli.reset_audio {
        return reset_audio_cli(&socket, &paths);
    }
    if cli.latency_test {
        let path = match cli.path.as_str() {
            "mixer" => LatencyPath::Mixer,
            _ => LatencyPath::Chain,
        };
        return latency_test_cli(&cli.latency, cli.seconds, path).await;
    }

    paths.ensure()?;
    info!("config root {}", paths.root.display());

    // Bind IPC first so the UI can always connect even if HID discovery is
    // slow/stuck, and so a second daemon exits before touching any device.
    let (listener, _lock) = ipc_server::bind(&socket)?;

    let hub = Arc::new(DaemonHub::new(paths));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let hub_disc = hub.clone();
    let discovery = tokio::spawn(async move {
        // Each pass is its own task: a panic in one (driver code) is logged
        // and the next pass still runs, instead of hotplug silently ending.
        // A JoinSet, so aborting discovery at shutdown aborts the pass too.
        let pass = |hub: Arc<DaemonHub>| async move {
            let mut set = tokio::task::JoinSet::new();
            set.spawn(async move { hub.discover_once().await });
            if let Some(Err(e)) = set.join_next().await {
                warn!("discovery pass crashed: {e}");
            }
        };
        // Discovery runs in the background so the socket is already accepting.
        pass(hub_disc.clone()).await;
        hub_disc.maybe_apply_last_profile().await;

        // Devices tick on their own worker threads; this only finds new ones.
        let mut interval = tokio::time::interval(DISCOVERY_PERIOD);
        loop {
            interval.tick().await;
            pass(hub_disc.clone()).await;
        }
    });

    let server = ipc_server::serve(listener, hub.clone());
    tokio::pin!(server);
    let result = tokio::select! {
        signal = tokio::signal::ctrl_c() => signal.map_err(Into::into),
        _ = terminate.recv() => Ok(()),
        result = &mut server => result,
    };

    info!("shutting down");
    discovery.abort();
    let _ = discovery.await;
    hub.shutdown().await;
    let _ = std::fs::remove_file(&socket);
    result
}

/// Ctrl-C / SIGTERM stop the run; it cleans up before returning.
async fn latency_test_cli(preset: &str, seconds: u32, path: LatencyPath) -> Result<()> {
    let latency = audio_latency_samples(preset)
        .ok_or_else(|| anyhow::anyhow!("unknown latency preset {preset}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let flag = stop.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        flag.store(true, Ordering::Relaxed);
    });
    let opts = LatencyTestOptions {
        latency,
        seconds,
        path,
    };
    let report = tokio::task::spawn_blocking(move || run_latency_test(&opts, &stop)).await??;
    print!("{report}");
    if report.verdict().is_err() {
        std::process::exit(1);
    }
    Ok(())
}

fn reset_audio_cli(socket: &std::path::Path, paths: &ConfigPaths) -> Result<()> {
    if socket.exists() {
        if let Ok(mut stream) = StdUnixStream::connect(socket) {
            let req = IpcRequest {
                id: serde_json::json!(1),
                method: hako_core::ipc::Method::ResetAudio,
                params: serde_json::Value::Null,
            };
            write_json_frame_sync(&mut stream, &req)?;
            let v = read_json_frame_sync(&mut stream)?;
            println!("{v}");
            return Ok(());
        }
    }
    // No daemon: also remove line-in links an older version left behind.
    let audio = AudioService::new();
    let aux = load_app_config(paths).map(|c| c.aux).unwrap_or_default();
    if let Err(e) = audio.remove_orphan_aux_links(&aux) {
        warn!("orphan AUX links: {e:#}");
    }
    audio.reset_all()?;
    println!("{{\"ok\":true,\"local\":true}}");
    Ok(())
}
