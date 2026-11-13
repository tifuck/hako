//! The LAN audio service: this machine's key, discovery beacons, pairing
//! with approval, Noise XX sessions to paired machines and their UDP audio
//! (`hako_audio::net` ports). `GetNet` reads the state in memory; only
//! `PatchNet` waits (a `Pair` for its handshake).
//!
//! While Network is on: an accept thread (plus one per incoming connection,
//! which then reads that session), a reader per dialled session, the UDP
//! audio receiver, the beacon listener and a 1 s tick (pings, timeouts,
//! beacons, reconnects, link stats). The audio send path runs on
//! PipeWire's data thread (`send_fn`).
//! Design: `docs/superpowers/specs/2026-11-13-lan-audio-design.md`.

mod wire;

use crate::drivers::EventTx;
use crate::mixer::PatchError;
use anyhow::{anyhow, bail, Context, Result};
use hako_audio::net::{NetAudio, NetPort, SendFn};
use hako_core::config::ConfigPaths;
use hako_core::net::{
    identity_path, load_net, peer_id, recv_node, save_identity, send_node, update_net, LinkStats,
    NearbyPeer, NetFile, NetOp, NetState, PairRequest, Peer, PeerState, DEFAULT_JITTER_MS,
    DEFAULT_PORT, MAX_NET_OPS,
};
use hako_core::protocol::AdapterEvent;
use hako_core::VERSION;
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, warn};
use wire::{
    clean_name, expire, handshake, read_control, Beacon, Cipher, Control, Handshake, Identity,
    Seen, MAX_AUDIO_PACKET, MAX_AUDIO_PLAIN, MAX_SAMPLES,
};

const TICK: Duration = Duration::from_secs(1);
/// Connect, handshake and Hello time limit; also every control write's.
const HANDSHAKE: Duration = Duration::from_secs(5);
/// A session with nothing heard for this long is dead.
const SILENCE: Duration = Duration::from_secs(5);
/// A pairing request nobody answered is dropped after this.
const PENDING: Duration = Duration::from_secs(60);
/// A denied key is refused for this long.
const DENIED: Duration = Duration::from_secs(60);
const MAX_PENDING: usize = 4;
/// Ticks between beacons, between reconnect attempts.
const BEACON_TICKS: u64 = 2;
const DIAL_TICKS: u64 = 3;
/// Blocking receives wake this often to see whether to stop.
const RECV_WAIT: Duration = Duration::from_millis(200);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// One authenticated connection to a peer.
struct Session {
    key: String,
    id: String,
    name: String,
    /// Its control and audio address: peer IP, the port its Hello named.
    addr: SocketAddr,
    code: String,
    cipher: Arc<Cipher>,
    /// Frames go out in nonce order: sealed and written under this lock.
    writer: Mutex<TcpStream>,
    /// For `shutdown` while a write is stuck.
    sock: TcpStream,
    heard: Mutex<Instant>,
    opened: Instant,
    rtt_ms: Mutex<f32>,
}

impl Session {
    fn new(stream: &TcpStream, hs: Handshake, name: &str, port: u16) -> Result<Self> {
        let id = peer_id(&hs.key);
        Ok(Self {
            name: clean_name(name, &id),
            addr: SocketAddr::new(stream.peer_addr()?.ip(), port),
            writer: Mutex::new(stream.try_clone()?),
            sock: stream.try_clone()?,
            key: hs.key,
            id,
            code: hs.code,
            cipher: Arc::new(hs.cipher),
            heard: Mutex::new(Instant::now()),
            opened: Instant::now(),
            rtt_ms: Mutex::new(0.0),
        })
    }

    /// A failed write ends the session (its reader sees the shutdown).
    fn send(&self, msg: &Control) -> Result<()> {
        let mut writer = lock(&self.writer);
        let sent = self
            .cipher
            .seal_control(msg)
            .and_then(|frame| Ok(writer.write_all(&frame)?));
        if sent.is_err() {
            self.shutdown();
        }
        sent
    }

    fn shutdown(&self) {
        let _ = self.sock.shutdown(Shutdown::Both);
    }

    /// The session clock (µs) that `Ping` carries.
    fn clock(&self) -> u64 {
        self.opened.elapsed().as_micros() as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    /// They asked to pair: Accept / Deny.
    Incoming,
    /// We dialled and wait for `Accepted` (`shown`: the user asked to pair;
    /// reconnects are not listed).
    Outgoing {
        shown: bool,
    },
    Live,
}

struct Entry {
    session: Arc<Session>,
    stage: Stage,
    since: Instant,
}

/// A live session's audio: its keys and its nodes (`None`: no audio, tests).
struct Link {
    key: String,
    cipher: Arc<Cipher>,
    port: Option<NetPort>,
}

/// The sockets and threads while Network is on.
struct Live {
    stop: Arc<AtomicBool>,
    port: u16,
    udp: Arc<UdpSocket>,
    beacons: Option<Arc<UdpSocket>>,
    /// The accept thread's listener (shut down to wake it).
    listener: RawFd,
    /// Dropped: the tick thread ends.
    tick: mpsc::Sender<()>,
    threads: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct Inner {
    file: NetFile,
    identity: Option<Identity>,
    /// Why Network is not working.
    problem: Option<String>,
    live: Option<Live>,
    /// Pending and live sessions by peer key.
    sessions: HashMap<String, Entry>,
    /// Beacons heard, by key.
    nearby: HashMap<String, Seen>,
    /// Where each peer was last connected this run (reconnects).
    seen_at: HashMap<String, SocketAddr>,
    denied: HashMap<String, Instant>,
    /// Reconnects in flight.
    dialing: HashSet<String>,
    /// Refreshed by the tick (1 Hz).
    stats: HashMap<String, LinkStats>,
}

struct Shared {
    paths: ConfigPaths,
    events: EventTx,
    /// Beacon port; `None`: no discovery (tests).
    discovery: Option<u16>,
    audio: Option<Mutex<NetAudio>>,
    inner: Mutex<Inner>,
    /// Audio routes by source address. Lock order: `inner`, then `links`.
    links: Mutex<HashMap<SocketAddr, Arc<Link>>>,
    /// One `PatchNet` at a time.
    patching: Mutex<()>,
    /// The last `NetUpdated` sent: events go out on change only.
    sent: Mutex<Option<NetState>>,
}

pub struct NetService {
    shared: Arc<Shared>,
}

impl NetService {
    /// Load `net.toml` and this machine's key; Network comes up if it is
    /// on. `discovery`: the beacon port (`None`: none); `audio`: `None`
    /// opens no nodes (tests).
    pub fn start(
        paths: ConfigPaths,
        events: EventTx,
        discovery: Option<u16>,
        audio: Option<NetAudio>,
    ) -> Self {
        let mut inner = Inner::default();
        match load_net(&paths) {
            Ok(file) => inner.file = file,
            Err(e) => {
                warn!("net.toml: {e:#}");
                inner.problem = Some(format!("{e:#}"));
            }
        }
        match load_identity(&paths) {
            Ok(identity) => inner.identity = identity,
            Err(e) => {
                warn!("{e:#}");
                inner.problem = Some(format!("{e:#}"));
            }
        }
        let on = inner.file.enabled;
        let shared = Arc::new(Shared {
            paths,
            events,
            discovery,
            audio: audio.map(Mutex::new),
            inner: Mutex::new(inner),
            links: Mutex::default(),
            patching: Mutex::default(),
            sent: Mutex::default(),
        });
        if on {
            shared.up();
        }
        Self { shared }
    }

    /// Never waits on the network.
    pub fn state(&self) -> NetState {
        self.shared.state()
    }

    /// Apply `ops` in order, stopping at the first failure. A `Pair` waits
    /// for the handshake (its request is in the returned state; `Accepted`
    /// arrives later as `NetUpdated`). Blocking.
    pub fn patch(&self, ops: &[NetOp]) -> Result<NetState, PatchError> {
        if ops.len() > MAX_NET_OPS {
            return Err(PatchError::Invalid(format!(
                "at most {MAX_NET_OPS} ops per patch"
            )));
        }
        let _one = lock(&self.shared.patching);
        let done = ops.iter().try_for_each(|op| self.shared.apply(op));
        self.shared.publish();
        done.map(|()| self.shared.state())
    }

    /// Daemon stop: Bye to peers, nodes closed, threads stopped. Blocking.
    pub fn shutdown(&self) {
        self.shared.down();
    }
}

impl Shared {
    fn apply(self: &Arc<Self>, op: &NetOp) -> Result<(), PatchError> {
        match op {
            NetOp::Pair { address } => {
                let addr = resolve(address).map_err(PatchError::Invalid)?;
                let (want, there) = {
                    let inner = lock(&self.inner);
                    let want = inner
                        .nearby
                        .values()
                        .find(|s| s.addr == addr)
                        .map(|s| s.beacon.key.clone());
                    let there = inner
                        .sessions
                        .values()
                        .find(|e| e.session.addr == addr)
                        .map(|e| e.session.name.clone());
                    (want, there)
                };
                if let Some(name) = there {
                    return Err(PatchError::Invalid(format!("already connected to {name}")));
                }
                self.dial(addr, true, want.as_deref())
                    .map_err(PatchError::Invalid)
            }
            NetOp::Respond { id, accept } => self.respond(id, *accept),
            _ => {
                self.edit(|f| f.apply(op))?;
                match op {
                    NetOp::SetEnabled { on: true } => self.up(),
                    NetOp::SetEnabled { on: false } => {
                        self.down();
                        lock(&self.inner).problem = None;
                    }
                    NetOp::Forget { id } => {
                        let found = lock(&self.inner)
                            .sessions
                            .values()
                            .find(|e| &e.session.id == id)
                            .map(|e| e.session.clone());
                        if let Some(s) = found {
                            self.close(&s, Some(&Control::Bye));
                        }
                    }
                    NetOp::SetJitter { id, jitter_ms } => {
                        for link in lock(&self.links).values() {
                            if &peer_id(&link.key) == id {
                                if let Some(port) = &link.port {
                                    port.set_jitter(*jitter_ms);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
        }
    }

    /// Read-modify-write `net.toml` and the copy in memory.
    fn edit(
        &self,
        change: impl FnOnce(&mut NetFile) -> Result<(), String>,
    ) -> Result<(), PatchError> {
        let mut inner = lock(&self.inner);
        let mut invalid = None;
        let saved = update_net(&self.paths, |f| {
            change(f).map_err(|e| {
                invalid = Some(e.clone());
                anyhow!(e)
            })
        });
        match saved {
            Ok((file, ())) => {
                inner.file = file;
                Ok(())
            }
            Err(e) => Err(invalid.map_or(PatchError::Io(e), PatchError::Invalid)),
        }
    }

    /// Bind and start the threads (once); failures become `problem`.
    fn up(self: &Arc<Self>) {
        let mut inner = lock(&self.inner);
        if inner.live.is_some() {
            return;
        }
        inner.problem = None;
        if inner.identity.is_none() {
            let made = load_identity(&self.paths).and_then(|found| match found {
                Some(identity) => Ok(identity),
                None => {
                    let identity = Identity::generate()?;
                    save_identity(&self.paths, &identity.text())?;
                    Ok(identity)
                }
            });
            match made {
                Ok(identity) => inner.identity = Some(identity),
                Err(e) => {
                    inner.problem = Some(format!("identity key: {e:#}"));
                    return;
                }
            }
        }
        let port = inner.file.port;
        let bound = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).and_then(|listener| {
            let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
            udp.set_read_timeout(Some(RECV_WAIT))?;
            Ok((listener, Arc::new(udp)))
        });
        let (listener, udp) = match bound {
            Ok(bound) => bound,
            Err(e) => {
                warn!("net: port {port}: {e}");
                inner.problem = Some(format!("port {port}: {e}"));
                return;
            }
        };
        let beacons = self.discovery.and_then(|port| {
            let bound = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).and_then(|s| {
                s.set_broadcast(true)?;
                s.set_read_timeout(Some(RECV_WAIT))?;
                Ok(Arc::new(s))
            });
            bound
                .inspect_err(|e| {
                    warn!("net: discovery port {port}: {e}");
                    inner.problem = Some(format!(
                        "discovery port {port}: {e} (Connect to address still works)"
                    ));
                })
                .ok()
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (tick, ticks) = mpsc::channel();
        let fd = listener.as_raw_fd();
        let mut threads = Vec::new();
        let (me, flag) = (self.clone(), stop.clone());
        threads.extend(spawn("hako-net-accept", move || me.accept(listener, &flag)));
        let (me, flag, sock) = (self.clone(), stop.clone(), udp.clone());
        threads.extend(spawn("hako-net-audio", move || me.audio_rx(&sock, &flag)));
        if let Some(sock) = beacons.clone() {
            let (me, flag) = (self.clone(), stop.clone());
            threads.extend(spawn("hako-net-beacons", move || {
                me.discovery_rx(&sock, &flag)
            }));
        }
        let me = self.clone();
        threads.extend(spawn("hako-net-tick", move || me.ticks(&ticks)));
        inner.live = Some(Live {
            stop,
            port,
            udp,
            beacons,
            listener: fd,
            tick,
            threads,
        });
    }

    /// Bye to every peer, close their nodes, stop the threads.
    fn down(&self) {
        let Some(live) = lock(&self.inner).live.take() else {
            return;
        };
        live.stop.store(true, Ordering::Relaxed);
        drop(live.tick);
        // SAFETY: the accept thread owns the listener until it is joined
        // below; shutdown only wakes its `accept`.
        unsafe { libc::shutdown(live.listener, libc::SHUT_RDWR) };
        let sessions: Vec<_> = lock(&self.inner)
            .sessions
            .values()
            .map(|e| e.session.clone())
            .collect();
        for s in sessions {
            self.close(&s, Some(&Control::Bye));
        }
        for thread in live.threads {
            let _ = thread.join();
        }
        let mut inner = lock(&self.inner);
        inner.nearby.clear();
        inner.stats.clear();
        drop(inner);
        self.publish();
    }

    fn accept(self: &Arc<Self>, listener: TcpListener, stop: &Arc<AtomicBool>) {
        for conn in listener.incoming() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let stream = match conn {
                Ok(stream) => stream,
                Err(e) => {
                    debug!("net: accept: {e}");
                    std::thread::sleep(RECV_WAIT);
                    continue;
                }
            };
            let (me, stop) = (self.clone(), stop.clone());
            // ponytail: one thread per incoming handshake, unbounded (LAN
            // flooding is out of scope); a cap if that ever matters.
            spawn("hako-net-in", move || {
                let from = stream.peer_addr();
                if let Err(e) = me.answer(stream, &stop) {
                    debug!("net: incoming {from:?}: {e:#}");
                }
            });
        }
    }

    /// The responder: handshake, Hello, then known keys go live, unknown
    /// ones that ask to pair wait for Accept / Deny. Reads the session
    /// until it ends.
    fn answer(self: &Arc<Self>, mut stream: TcpStream, stop: &Arc<AtomicBool>) -> Result<()> {
        let (private, own, hello) = {
            let inner = lock(&self.inner);
            let (Some(live), Some(identity)) = (&inner.live, &inner.identity) else {
                bail!("Network is off");
            };
            (
                identity.private.clone(),
                identity.public.clone(),
                hello(&inner.file, live.port, false),
            )
        };
        prepare(&stream)?;
        let hs = handshake(&mut stream, &private, false)?;
        let mut last = 0;
        let Control::Hello {
            name, port, pair, ..
        } = read_control(&mut stream, &hs.cipher, &mut last)?
        else {
            bail!("no Hello");
        };
        let s = Arc::new(Session::new(&stream, hs, &name, port)?);
        let admitted = {
            let mut inner = lock(&self.inner);
            let stage = if s.key == own {
                Err("this machine's own key".to_string())
            } else if inner.denied.contains_key(&s.key) {
                Err("denied a moment ago".into())
            } else if inner.file.peers.iter().any(|p| p.key == s.key) {
                Ok(Stage::Live)
            } else if !pair {
                Err("not paired".into())
            } else {
                Ok(Stage::Incoming)
            };
            stage.and_then(|stage| register(&mut inner, &s, stage, stop).map(|()| stage))
        };
        let stage = match admitted {
            Ok(stage) => stage,
            Err(e) => {
                let _ = s.send(&Control::Denied);
                bail!("{}: {e}", s.name);
            }
        };
        let setup = s.send(&hello).and_then(|()| {
            if stage == Stage::Live {
                self.go_live(&s).map_err(|e| anyhow!(message(e)))?;
                s.send(&Control::Accepted)?;
            }
            Ok(())
        });
        if let Err(e) = setup {
            self.close(&s, Some(&Control::Bye));
            return Err(e);
        }
        self.publish();
        self.read_loop(&s, stream, last);
        Ok(())
    }

    /// The initiator: connect, handshake, Hello, then the session waits for
    /// `Accepted` (`pair`: listed as a request). `want`: the key that must
    /// answer (a beacon's or a paired peer's). Blocking.
    fn dial(
        self: &Arc<Self>,
        addr: SocketAddr,
        pair: bool,
        want: Option<&str>,
    ) -> Result<(), String> {
        let (private, own, stop, hello) = {
            let inner = lock(&self.inner);
            let (Some(live), Some(identity)) = (&inner.live, &inner.identity) else {
                return Err("Network is off".into());
            };
            (
                identity.private.clone(),
                identity.public.clone(),
                live.stop.clone(),
                hello(&inner.file, live.port, pair),
            )
        };
        let failed = |e: anyhow::Error| format!("{addr}: {e:#}");
        let mut stream =
            TcpStream::connect_timeout(&addr, HANDSHAKE).map_err(|e| format!("{addr}: {e}"))?;
        prepare(&stream).map_err(|e| format!("{addr}: {e}"))?;
        let hs = handshake(&mut stream, &private, true).map_err(failed)?;
        if hs.key == own {
            return Err(format!("{addr} is this machine"));
        }
        if want.is_some_and(|key| key != hs.key) {
            return Err(format!(
                "{addr} is not the machine that announced itself there"
            ));
        }
        let mut last = 0;
        let reply = hs
            .cipher
            .seal_control(&hello)
            .and_then(|frame| Ok(stream.write_all(&frame)?))
            .and_then(|()| read_control(&mut stream, &hs.cipher, &mut last))
            .map_err(failed)?;
        let (name, port) = match reply {
            Control::Hello { name, port, .. } => (name, port),
            Control::Denied => return Err(format!("{addr} declined")),
            _ => return Err(format!("{addr} closed the connection")),
        };
        let s = Arc::new(Session::new(&stream, hs, &name, port).map_err(failed)?);
        let registered = register(
            &mut lock(&self.inner),
            &s,
            Stage::Outgoing { shown: pair },
            &stop,
        );
        if let Err(e) = registered {
            let _ = s.send(&Control::Bye);
            return Err(e);
        }
        self.publish();
        let me = self.clone();
        let reader = s.clone();
        spawn("hako-net-session", move || {
            me.read_loop(&reader, stream, last)
        });
        Ok(())
    }

    /// Answer a pending request by peer id.
    fn respond(self: &Arc<Self>, id: &str, accept: bool) -> Result<(), PatchError> {
        let found = lock(&self.inner)
            .sessions
            .values()
            .find(|e| e.session.id == id && e.stage != Stage::Live)
            .map(|e| (e.session.clone(), e.stage));
        let Some((s, stage)) = found else {
            return Err(PatchError::Invalid(format!(
                "no pairing request from {id:?}"
            )));
        };
        match (stage, accept) {
            (Stage::Incoming, true) => {
                self.go_live(&s)?;
                let _ = s.send(&Control::Accepted);
            }
            (Stage::Incoming, false) => {
                lock(&self.inner)
                    .denied
                    .insert(s.key.clone(), Instant::now());
                self.close(&s, Some(&Control::Denied));
            }
            (_, false) => self.close(&s, Some(&Control::Bye)),
            (_, true) => {
                return Err(PatchError::Invalid(format!(
                    "waiting for {} to accept",
                    s.name
                )))
            }
        }
        Ok(())
    }

    /// Pin `s`'s key (new, or renamed), then open its nodes and audio route.
    fn go_live(self: &Arc<Self>, s: &Arc<Session>) -> Result<(), PatchError> {
        let gone = || PatchError::Invalid(format!("{} disconnected", s.name));
        let pinned = lock(&self.inner)
            .file
            .peers
            .iter()
            .any(|p| p.key == s.key && p.name == s.name);
        if !pinned {
            self.edit(|f| {
                f.trust(Peer {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    key: s.key.clone(),
                    jitter_ms: DEFAULT_JITTER_MS,
                })
            })?;
        }
        let (jitter, udp) = {
            let inner = lock(&self.inner);
            let live = inner.live.as_ref().ok_or_else(gone)?;
            let jitter = inner
                .file
                .peers
                .iter()
                .find(|p| p.key == s.key)
                .map_or(DEFAULT_JITTER_MS, |p| p.jitter_ms);
            (jitter, live.udp.clone())
        };
        let port = self.audio.as_ref().map(|audio| {
            lock(audio).open(
                &send_node(&s.id),
                &recv_node(&s.id),
                &s.name,
                jitter,
                send_fn(s.cipher.clone(), udp, s.addr),
            )
        });
        let link = Arc::new(Link {
            key: s.key.clone(),
            cipher: s.cipher.clone(),
            port,
        });
        let mut inner = lock(&self.inner);
        let Some(entry) = inner
            .sessions
            .get_mut(&s.key)
            .filter(|e| Arc::ptr_eq(&e.session, s))
        else {
            drop(inner);
            return Err(gone());
        };
        entry.stage = Stage::Live;
        inner.seen_at.insert(s.key.clone(), s.addr);
        let old = lock(&self.links).insert(s.addr, link);
        drop(inner);
        drop(old);
        self.publish();
        Ok(())
    }

    /// Read `s`'s control frames until it ends, then close it.
    fn read_loop(self: &Arc<Self>, s: &Arc<Session>, mut stream: TcpStream, mut last: u64) {
        let _ = stream.set_read_timeout(None);
        while let Ok(msg) = read_control(&mut stream, &s.cipher, &mut last) {
            *lock(&s.heard) = Instant::now();
            match msg {
                Control::Ping { t } => {
                    let _ = s.send(&Control::Pong { t });
                }
                Control::Pong { t } => {
                    *lock(&s.rtt_ms) = s.clock().saturating_sub(t) as f32 / 1000.0;
                }
                Control::Accepted => {
                    let outgoing = lock(&self.inner).sessions.get(&s.key).is_some_and(|e| {
                        Arc::ptr_eq(&e.session, s) && matches!(e.stage, Stage::Outgoing { .. })
                    });
                    if outgoing {
                        if let Err(e) = self.go_live(s) {
                            warn!("net: {}: {}", s.name, message(e));
                            break;
                        }
                    }
                }
                Control::Denied | Control::Bye => break,
                Control::Hello { .. } => {}
            }
        }
        self.close(s, Some(&Control::Bye));
    }

    /// Tell the peer (`msg`), disconnect and drop the session.
    fn close(&self, s: &Arc<Session>, msg: Option<&Control>) {
        if let Some(msg) = msg {
            let _ = s.send(msg);
        }
        s.shutdown();
        let link = {
            let mut inner = lock(&self.inner);
            if inner
                .sessions
                .get(&s.key)
                .is_some_and(|e| Arc::ptr_eq(&e.session, s))
            {
                inner.sessions.remove(&s.key);
                inner.stats.remove(&s.key);
            }
            let mut links = lock(&self.links);
            match links.get(&s.addr) {
                Some(link) if Arc::ptr_eq(&link.cipher, &s.cipher) => links.remove(&s.addr),
                _ => None,
            }
        };
        // Its nodes close outside the locks.
        drop(link);
        self.publish();
    }

    fn audio_rx(&self, udp: &UdpSocket, stop: &AtomicBool) {
        let mut buf = [0u8; 2048];
        let mut plain = [0u8; MAX_AUDIO_PLAIN];
        let mut samples = [0i16; MAX_SAMPLES];
        while !stop.load(Ordering::Relaxed) {
            match udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    deliver(&self.links, from, &buf[..n], &mut plain, &mut samples);
                }
                Err(e) => idle(&e),
            }
        }
    }

    fn discovery_rx(&self, sock: &UdpSocket, stop: &AtomicBool) {
        let mut buf = [0u8; 1024];
        while !stop.load(Ordering::Relaxed) {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => self.heard(&buf[..n], from),
                Err(e) => idle(&e),
            }
        }
    }

    fn heard(&self, bytes: &[u8], from: SocketAddr) {
        let Some(beacon) = Beacon::parse(bytes) else {
            return;
        };
        let addr = SocketAddr::new(from.ip(), beacon.port);
        let mut inner = lock(&self.inner);
        if inner
            .identity
            .as_ref()
            .is_some_and(|i| i.public == beacon.key)
        {
            return;
        }
        let changed = inner
            .nearby
            .get(&beacon.key)
            .is_none_or(|s| s.beacon != beacon || s.addr != addr);
        inner.nearby.insert(
            beacon.key.clone(),
            Seen {
                beacon,
                addr,
                at: Instant::now(),
            },
        );
        drop(inner);
        if changed {
            self.publish();
        }
    }

    fn ticks(self: &Arc<Self>, stop: &mpsc::Receiver<()>) {
        for n in 0.. {
            self.tick(n);
            if !matches!(stop.recv_timeout(TICK), Err(RecvTimeoutError::Timeout)) {
                return;
            }
        }
    }

    /// Pings, dead and expired sessions, beacons, reconnects, link stats.
    fn tick(self: &Arc<Self>, n: u64) {
        let now = Instant::now();
        let (stale, sessions, dials, beacon) = {
            let mut guard = lock(&self.inner);
            let inner = &mut *guard;
            let sessions: Vec<_> = inner.sessions.values().map(|e| e.session.clone()).collect();
            let stale: Vec<_> = inner
                .sessions
                .values()
                .filter(|e| {
                    now.duration_since(*lock(&e.session.heard)) > SILENCE
                        || (e.stage != Stage::Live && now.duration_since(e.since) > PENDING)
                })
                .map(|e| e.session.clone())
                .collect();
            inner
                .denied
                .retain(|_, at| now.duration_since(*at) < DENIED);
            expire(&mut inner.nearby, now);
            inner.stats = lock(&self.links)
                .values()
                .map(|link| {
                    let port = link.port.as_ref().map(NetPort::stats).unwrap_or_default();
                    let rtt_ms = inner
                        .sessions
                        .get(&link.key)
                        .map_or(0.0, |e| *lock(&e.session.rtt_ms));
                    let stats = LinkStats {
                        buffered_ms: port.buffered_ms,
                        dropouts: port.dropouts,
                        receiving: port.receiving,
                        sending: port.sending,
                        rtt_ms,
                    };
                    (link.key.clone(), stats)
                })
                .collect();
            let dials = if n.is_multiple_of(DIAL_TICKS) {
                reconnects(inner)
            } else {
                Vec::new()
            };
            let beacon = match (&inner.live, &inner.identity, self.discovery) {
                (Some(live), Some(identity), Some(port)) if n.is_multiple_of(BEACON_TICKS) => {
                    live.beacons.clone().map(|sock| {
                        let beacon = Beacon {
                            id: peer_id(&identity.public),
                            name: own_name(&inner.file),
                            key: identity.public.clone(),
                            port: live.port,
                        };
                        (sock, beacon.encode(), (Ipv4Addr::BROADCAST, port))
                    })
                }
                _ => None,
            };
            (stale, sessions, dials, beacon)
        };
        for s in &stale {
            self.close(s, Some(&Control::Bye));
        }
        for s in sessions {
            let _ = s.send(&Control::Ping { t: s.clock() });
        }
        for (key, addr) in dials {
            let me = self.clone();
            spawn("hako-net-dial", move || {
                if let Err(e) = me.dial(addr, false, Some(&key)) {
                    debug!("net: reconnect: {e}");
                }
                lock(&me.inner).dialing.remove(&key);
            });
        }
        if let Some((sock, bytes, to)) = beacon {
            if let Err(e) = sock.send_to(&bytes, to) {
                debug!("net: beacon: {e}");
            }
        }
        self.publish();
    }

    fn state(&self) -> NetState {
        let inner = lock(&self.inner);
        let live = |key: &str| {
            inner
                .sessions
                .get(key)
                .filter(|e| e.stage == Stage::Live)
                .map(|e| &e.session)
        };
        let peers = inner
            .file
            .peers
            .iter()
            .map(|p| {
                let s = live(&p.key);
                PeerState {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    jitter_ms: p.jitter_ms,
                    connected: s.is_some(),
                    address: s
                        .map(|s| s.addr)
                        .or_else(|| inner.seen_at.get(&p.key).copied())
                        .map(|a| a.to_string()),
                    link: s.map(|_| inner.stats.get(&p.key).copied().unwrap_or_default()),
                }
            })
            .collect();
        let mut nearby: Vec<_> = inner
            .nearby
            .values()
            .filter(|s| !inner.file.peers.iter().any(|p| p.key == s.beacon.key))
            .map(|s| NearbyPeer {
                id: s.beacon.id.clone(),
                name: s.beacon.name.clone(),
                address: s.addr.to_string(),
            })
            .collect();
        nearby.sort_by(|a, b| a.id.cmp(&b.id));
        let mut requests: Vec<_> = inner
            .sessions
            .values()
            .filter_map(|e| {
                let incoming = match e.stage {
                    Stage::Incoming => true,
                    Stage::Outgoing { shown: true } => false,
                    _ => return None,
                };
                Some(PairRequest {
                    id: e.session.id.clone(),
                    name: e.session.name.clone(),
                    address: e.session.addr.to_string(),
                    code: e.session.code.clone(),
                    incoming,
                })
            })
            .collect();
        requests.sort_by(|a, b| a.id.cmp(&b.id));
        NetState {
            enabled: inner.file.enabled,
            name: own_name(&inner.file),
            id: inner
                .identity
                .as_ref()
                .map(|i| peer_id(&i.public))
                .unwrap_or_default(),
            port: inner.file.port,
            problem: inner.problem.clone(),
            peers,
            nearby,
            requests,
        }
    }

    /// Send `NetUpdated` if the state changed.
    fn publish(&self) {
        // Computed under `sent`: a slower thread cannot send an older state last.
        let mut sent = lock(&self.sent);
        let state = self.state();
        if sent.as_ref() != Some(&state) {
            *sent = Some(state.clone());
            let _ = self.events.send(AdapterEvent::NetUpdated(Box::new(state)));
        }
    }
}

/// Add `s` unless Network went off meanwhile (`stop` is not the live one),
/// its key has a session already, or too many requests wait.
fn register(
    inner: &mut Inner,
    s: &Arc<Session>,
    stage: Stage,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    if !inner
        .live
        .as_ref()
        .is_some_and(|live| Arc::ptr_eq(&live.stop, stop))
    {
        return Err("Network was turned off".into());
    }
    if inner.sessions.contains_key(&s.key) {
        return Err(format!("already connected to {}", s.name));
    }
    let waiting = inner
        .sessions
        .values()
        .filter(|e| e.stage == Stage::Incoming)
        .count();
    if stage == Stage::Incoming && waiting >= MAX_PENDING {
        return Err("too many pairing requests".into());
    }
    inner.sessions.insert(
        s.key.clone(),
        Entry {
            session: s.clone(),
            stage,
            since: Instant::now(),
        },
    );
    Ok(())
}

/// Paired machines to dial now: not connected or being dialled, with a
/// known address, and a higher id than this machine (the lower id dials).
fn reconnects(inner: &mut Inner) -> Vec<(String, SocketAddr)> {
    let Some(own) = inner.identity.as_ref().map(|i| peer_id(&i.public)) else {
        return Vec::new();
    };
    let dials: Vec<_> = inner
        .file
        .peers
        .iter()
        .filter(|p| {
            own < p.id && !inner.sessions.contains_key(&p.key) && !inner.dialing.contains(&p.key)
        })
        .filter_map(|p| {
            let addr = inner
                .nearby
                .get(&p.key)
                .map(|s| s.addr)
                .or_else(|| inner.seen_at.get(&p.key).copied())?;
            Some((p.key.clone(), addr))
        })
        .collect();
    inner
        .dialing
        .extend(dials.iter().map(|(key, _)| key.clone()));
    dials
}

/// One UDP packet: dropped before decryption unless its source address
/// belongs to a live session; else decrypted into its port. Accepted?
fn deliver(
    links: &Mutex<HashMap<SocketAddr, Arc<Link>>>,
    from: SocketAddr,
    packet: &[u8],
    plain: &mut [u8],
    samples: &mut [i16],
) -> bool {
    let Some(link) = lock(links).get(&from).cloned() else {
        return false;
    };
    let Some((pos, n)) = link.cipher.open_audio(packet, plain, samples) else {
        return false;
    };
    if let Some(port) = &link.port {
        port.push(pos, &samples[..n]);
    }
    true
}

/// A session's audio send path: seal each block and `sendto` it without
/// blocking. Runs on PipeWire's data thread: its buffers are made here,
/// once, and it takes no lock.
fn send_fn(cipher: Arc<Cipher>, udp: Arc<UdpSocket>, to: SocketAddr) -> SendFn {
    // The socket is IPv4 (bound to 0.0.0.0): so are its peers.
    let SocketAddr::V4(to) = to else {
        return Box::new(|_, _| {});
    };
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: to.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(*to.ip()).to_be(),
        },
        sin_zero: [0; 8],
    };
    let mut plain = vec![0u8; MAX_AUDIO_PLAIN];
    let mut packet = vec![0u8; MAX_AUDIO_PACKET];
    Box::new(move |pos, samples| {
        let n = cipher.seal_audio(pos, samples, &mut plain, &mut packet);
        if n == 0 {
            return;
        }
        // SAFETY: `packet` holds `n` bytes, `addr` is a valid sockaddr_in,
        // and `udp` keeps the socket open.
        unsafe {
            libc::sendto(
                udp.as_raw_fd(),
                packet.as_ptr().cast(),
                n,
                libc::MSG_DONTWAIT,
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );
        }
    })
}

/// The key file; absent = `None`. Unreadable is an error, never replaced.
fn load_identity(paths: &ConfigPaths) -> Result<Option<Identity>> {
    let path = identity_path(paths);
    match std::fs::read_to_string(&path) {
        Ok(text) => Identity::parse(&text)
            .map(Some)
            .with_context(|| format!("{}", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// This machine as peers see it: `name`, else the hostname.
fn own_name(file: &NetFile) -> String {
    if !file.name.is_empty() {
        return file.name.clone();
    }
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    clean_name(&host, "hako")
}

fn hello(file: &NetFile, port: u16, pair: bool) -> Control {
    Control::Hello {
        name: own_name(file),
        version: VERSION.into(),
        port,
        pair,
    }
}

/// `host` or `host:port` (default port), IPv4.
fn resolve(address: &str) -> Result<SocketAddr, String> {
    let address = address.trim();
    address
        .to_socket_addrs()
        .or_else(|_| (address, DEFAULT_PORT).to_socket_addrs())
        .ok()
        .and_then(|mut found| found.find(SocketAddr::is_ipv4))
        .ok_or_else(|| format!("cannot find {address:?} (host or host:port)"))
}

/// Control connections: no Nagle, handshake time limits.
fn prepare(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE))?;
    stream.set_write_timeout(Some(HANDSHAKE))
}

/// After a failed receive: timeouts just loop; anything else waits a
/// moment instead of spinning.
fn idle(e: &std::io::Error) {
    if !matches!(
        e.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    ) {
        debug!("net: receive: {e}");
        std::thread::sleep(RECV_WAIT);
    }
}

fn message(e: PatchError) -> String {
    match e {
        PatchError::Invalid(e) => e,
        PatchError::Io(e) => format!("{e:#}"),
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .inspect_err(|e| warn!("spawn {name}: {e}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::net::DISCOVERY_PORT;
    use tokio::sync::broadcast;

    fn paths(name: &str) -> ConfigPaths {
        let root = std::env::temp_dir().join(format!("hako-net-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root,
        };
        paths.ensure().unwrap();
        paths
    }

    /// A port free for TCP and UDP, never the real ones.
    fn free_port() -> u16 {
        loop {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            if ![DEFAULT_PORT, DISCOVERY_PORT].contains(&port)
                && UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok()
            {
                return port;
            }
        }
    }

    /// A machine on its own port: no discovery, no audio nodes.
    fn machine(name: &str) -> (NetService, ConfigPaths, u16) {
        machine_with(name, None)
    }

    fn machine_with(name: &str, audio: Option<NetAudio>) -> (NetService, ConfigPaths, u16) {
        let paths = paths(name);
        let port = free_port();
        update_net(&paths, |f| {
            f.port = port;
            f.name = name.into();
            Ok(())
        })
        .unwrap();
        let (events, _) = broadcast::channel(256);
        let net = NetService::start(paths.clone(), events, None, audio);
        (net, paths, port)
    }

    /// Two machines on this computer's PipeWire, paired over loopback: a
    /// tone played into alpha's network sink comes out of bravo's network
    /// source. Needs `pw-cat`; adds four short-lived nodes to the session.
    #[test]
    #[ignore]
    fn live_audio_crosses_the_link() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let (a, a_paths, _) = machine_with("live-a", Some(NetAudio::new()));
        let (b, b_paths, b_port) = machine_with("live-b", Some(NetAudio::new()));
        let (a_id, b_id) = (turn_on(&a), turn_on(&b));
        a.patch(&[NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
        }])
        .unwrap();
        b.patch(&[NetOp::Respond {
            id: a_id.clone(),
            accept: true,
        }])
        .unwrap();
        wait("connected", || connected(&a, &b_id) && connected(&b, &a_id));
        std::thread::sleep(Duration::from_secs(1));

        let pw_cat = |mode: &str, target: &str| {
            let mut c = Command::new("pw-cat");
            c.args([mode, "--raw", "--format", "s16", "--rate", "48000"])
                .args(["--channels", "2", "--target", target, "-"]);
            c
        };
        let mut recorder = pw_cat("--record", &hako_core::net::recv_node(&a_id))
            .stdout(Stdio::piped())
            .spawn()
            .expect("pw-cat");
        let mut out = recorder.stdout.take().unwrap();
        let recorded = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut bytes);
            bytes
        });
        let mut player = pw_cat("--playback", &hako_core::net::send_node(&b_id))
            .stdin(Stdio::piped())
            .spawn()
            .expect("pw-cat");
        let tone: Vec<u8> = (0..48_000 * 2)
            .flat_map(|i| {
                let s =
                    ((i as f32 * 440.0 / 48_000.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                [s, s]
            })
            .flat_map(i16::to_le_bytes)
            .collect();
        player.stdin.take().unwrap().write_all(&tone).unwrap();
        let link = || {
            b.state()
                .peers
                .iter()
                .find(|p| p.id == a_id)
                .and_then(|p| p.link)
        };
        wait("receiving", || link().is_some_and(|l| l.receiving));
        let _ = player.wait();
        std::thread::sleep(Duration::from_millis(500));
        let _ = recorder.kill();
        let _ = recorder.wait();
        let bytes = recorded.join().unwrap();
        let peak = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b).unsigned_abs())
            .max()
            .unwrap_or(0);
        eprintln!(
            "recorded {} bytes, peak {peak}, link {:?}",
            bytes.len(),
            link()
        );
        // The session may put a stream volume on pw-cat's playback (seen:
        // 0.28), so the tone (8000) only has to come through clearly.
        assert!(peak > 1500, "peak {peak}");
        done([(a, a_paths), (b, b_paths)]);
    }

    fn wait(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn connected(net: &NetService, id: &str) -> bool {
        net.state().peers.iter().any(|p| p.id == id && p.connected)
    }

    fn trusts(paths: &ConfigPaths, id: &str) -> bool {
        load_net(paths).unwrap().peers.iter().any(|p| p.id == id)
    }

    fn turn_on(net: &NetService) -> String {
        let state = net.patch(&[NetOp::SetEnabled { on: true }]).unwrap();
        assert_eq!(state.problem, None);
        state.id
    }

    fn done(machines: [(NetService, ConfigPaths); 2]) {
        for (net, paths) in machines {
            net.shutdown();
            let _ = std::fs::remove_dir_all(&paths.root);
        }
    }

    #[test]
    fn two_machines_pair_reconnect_and_forget() {
        let (a, a_paths, _) = machine("alpha");
        let (b, b_paths, b_port) = machine("bravo");
        let (a_id, b_id) = (turn_on(&a), turn_on(&b));
        assert!(!a_id.is_empty() && a_id != b_id);

        let state = a
            .patch(&[NetOp::Pair {
                address: format!("127.0.0.1:{b_port}"),
            }])
            .unwrap();
        let out = state.requests.first().expect("outgoing request").clone();
        assert!(!out.incoming);
        assert_eq!(
            (out.id.as_str(), out.name.as_str()),
            (b_id.as_str(), "bravo")
        );
        let theirs = b.state().requests;
        assert_eq!(theirs.len(), 1, "{theirs:?}");
        assert!(theirs[0].incoming);
        assert_eq!(
            (theirs[0].id.as_str(), theirs[0].name.as_str()),
            (a_id.as_str(), "alpha")
        );
        assert_eq!(theirs[0].code, out.code);

        b.patch(&[NetOp::Respond {
            id: a_id.clone(),
            accept: true,
        }])
        .unwrap();
        wait("connected", || connected(&a, &b_id) && connected(&b, &a_id));
        assert!(a.state().requests.is_empty() && b.state().requests.is_empty());
        assert!(trusts(&a_paths, &b_id) && trusts(&b_paths, &a_id));

        // A drop (bravo off and on again): back without a request.
        b.patch(&[NetOp::SetEnabled { on: false }]).unwrap();
        wait("dropped", || !connected(&a, &b_id));
        turn_on(&b);
        wait("reconnected", || {
            connected(&a, &b_id) && connected(&b, &a_id)
        });
        assert!(a.state().requests.is_empty() && b.state().requests.is_empty());

        let state = a.patch(&[NetOp::Forget { id: b_id.clone() }]).unwrap();
        assert!(state.peers.is_empty());
        assert!(!trusts(&a_paths, &b_id));
        wait("disconnected", || !connected(&b, &a_id));
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn a_denied_request_pins_nothing() {
        let (a, a_paths, _) = machine("deny-a");
        let (b, b_paths, b_port) = machine("deny-b");
        let a_id = turn_on(&a);
        turn_on(&b);
        let pair = [NetOp::Pair {
            address: format!("localhost:{b_port}"),
        }];
        a.patch(&pair).unwrap();
        b.patch(&[NetOp::Respond {
            id: a_id,
            accept: false,
        }])
        .unwrap();
        wait("request gone", || a.state().requests.is_empty());
        assert!(b.state().requests.is_empty());
        assert!(load_net(&a_paths).unwrap().peers.is_empty());
        assert!(load_net(&b_paths).unwrap().peers.is_empty());
        // Asking again at once is refused.
        match a.patch(&pair) {
            Err(PatchError::Invalid(e)) => assert!(e.contains("declined"), "{e}"),
            other => panic!("{other:?}"),
        }
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn pairing_with_an_announced_machine_needs_its_announced_key() {
        let (a, a_paths, _) = machine("beacon-a");
        let (b, b_paths, b_port) = machine("beacon-b");
        turn_on(&a);
        let b_key = {
            turn_on(&b);
            lock(&b.shared.inner)
                .identity
                .as_ref()
                .unwrap()
                .public
                .clone()
        };
        let announce = |key: &str| {
            let beacon = Beacon {
                id: peer_id(key),
                name: "Bravo".into(),
                key: key.into(),
                port: b_port,
            };
            a.shared
                .heard(&beacon.encode(), "127.0.0.1:47820".parse().unwrap());
        };
        let pair = [NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
        }];
        let impostor = "00".repeat(32);
        announce(&impostor);
        assert_eq!(a.state().nearby[0].address, format!("127.0.0.1:{b_port}"));
        match a.patch(&pair) {
            Err(PatchError::Invalid(e)) => assert!(e.contains("announced"), "{e}"),
            other => panic!("{other:?}"),
        }
        lock(&a.shared.inner).nearby.clear();
        announce(&b_key);
        let state = a.patch(&pair).unwrap();
        assert_eq!(state.requests.len(), 1);
        assert!(state.nearby.len() == 1, "not paired yet: still nearby");
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn patches_check_ops_and_network_off_listens_nowhere() {
        let (net, paths, port) = machine("ops");
        let state = net.state();
        assert!(
            !state.enabled && state.id.is_empty(),
            "no key before the first enable"
        );
        // Off: nothing bound, Pair refused.
        assert!(TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok());
        let pair = NetOp::Pair {
            address: "127.0.0.1".into(),
        };
        assert!(matches!(net.patch(&[pair]), Err(PatchError::Invalid(_))));
        let ops = vec![NetOp::SetEnabled { on: false }; MAX_NET_OPS + 1];
        assert!(matches!(net.patch(&ops), Err(PatchError::Invalid(_))));
        // In order, stopping at the first failure.
        let ops = [
            NetOp::SetName {
                name: "Desk".into(),
            },
            NetOp::Forget { id: "nope".into() },
            NetOp::SetName {
                name: "Tower".into(),
            },
        ];
        assert!(matches!(net.patch(&ops), Err(PatchError::Invalid(_))));
        assert_eq!(net.state().name, "Desk");
        assert_eq!(load_net(&paths).unwrap().name, "Desk");

        // The key is made once and kept.
        let id = turn_on(&net);
        assert!(
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_err(),
            "listening"
        );
        net.patch(&[NetOp::SetEnabled { on: false }]).unwrap();
        assert!(
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok(),
            "closed"
        );
        net.shutdown();
        let (events, _) = broadcast::channel(8);
        let again = NetService::start(paths.clone(), events, None, None);
        assert_eq!(again.state().id, id);
        again.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_taken_port_is_a_problem_not_a_failure() {
        let (net, paths, port) = machine("taken");
        let _taken = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap();
        let state = net.patch(&[NetOp::SetEnabled { on: true }]).unwrap();
        assert!(state.enabled);
        assert!(state.problem.is_some_and(|p| p.contains(&port.to_string())));
        let state = net.patch(&[NetOp::SetEnabled { on: false }]).unwrap();
        assert_eq!(state.problem, None);
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn audio_goes_out_through_send_fn_and_only_known_addresses_are_opened() {
        let (a, b, _, _) = wire::tests::pair();
        let tx = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut send = send_fn(Arc::new(a.cipher), tx.clone(), rx.local_addr().unwrap());
        let block = [1i16, -2, 3, -4];
        send(960, &block);
        let mut buf = [0u8; 2048];
        let (n, from) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(from, tx.local_addr().unwrap());
        let packet = &buf[..n];

        let (mut plain, mut samples) = ([0u8; MAX_AUDIO_PLAIN], [0i16; MAX_SAMPLES]);
        let links = Mutex::new(HashMap::new());
        let mut got = |from| deliver(&links, from, packet, &mut plain, &mut samples);
        assert!(!got(from), "no session");
        lock(&links).insert(
            from,
            Arc::new(Link {
                key: "k".into(),
                cipher: Arc::new(b.cipher),
                port: None,
            }),
        );
        assert!(!got("127.0.0.1:9".parse().unwrap()), "another address");
        assert!(got(from));
        assert!(!got(from), "replayed");
        assert_eq!(&samples[..4], &block);
    }

    #[test]
    fn addresses_take_the_default_port() {
        assert_eq!(
            resolve("127.0.0.1"),
            Ok(SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)))
        );
        assert_eq!(
            resolve(" 10.0.0.2:5000 "),
            Ok(SocketAddr::from(([10, 0, 0, 2], 5000)))
        );
        assert!(resolve("").is_err());
    }
}
