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
    DEFAULT_PORT, MAX_NET_OPS, MAX_PEERS,
};
use hako_core::protocol::AdapterEvent;
use hako_core::VERSION;
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, warn};
use wire::{
    clean_name, expire, handshake, read_control, Beacon, Cipher, Control, Handshake, Identity,
    OtherProtocol, Seen, FIRST_MESSAGE, MAX_AUDIO_PACKET, MAX_AUDIO_PLAIN, MAX_SAMPLES,
};

const TICK: Duration = Duration::from_secs(1);
/// Connect time limit, then the handshake and Hello's together; also every
/// control write's.
const HANDSHAKE: Duration = Duration::from_secs(5);
/// An incoming connection's first handshake message must arrive this fast
/// (an idle connection gives its slot back soon).
const FIRST_MESSAGE_WAIT: Duration = Duration::from_secs(1);
/// A session with nothing heard for this long is dead.
const SILENCE: Duration = Duration::from_secs(5);
/// A session's reads give up after this (the tick ends silent sessions
/// sooner; this holds without it).
const READ_LIMIT: Duration = SILENCE.saturating_mul(3);
/// A pairing request nobody answered is dropped after this.
const PENDING: Duration = Duration::from_secs(60);
/// A denied key is refused for this long, and so are pairing requests
/// from an address whose last one ended unanswered.
const DENIED: Duration = Duration::from_secs(60);
/// Addresses cooling down after an unanswered request (the oldest goes).
const MAX_COOLING: usize = 64;
const MAX_PENDING: usize = 4;
/// Pairing requests waiting from one IP address.
const MAX_PENDING_PER_IP: usize = 1;
/// Incoming connections still in their handshake, in all and from one IP
/// address.
const MAX_HANDSHAKES: usize = 8;
const MAX_HANDSHAKES_PER_IP: usize = 2;
/// Machines kept from beacons (the one heard longest ago goes).
const MAX_NEARBY: usize = 64;
/// Ticks between beacons.
const BEACON_TICKS: u64 = 2;
/// Reconnect attempts to a paired machine are this far apart, doubling
/// after each one up to `MAX_REDIAL` until it is connected again.
const REDIAL: Duration = Duration::from_secs(3);
const MAX_REDIAL: Duration = Duration::from_secs(300);
/// The higher id dials too once a paired machine has been disconnected this
/// long (the lower id may be unable to reach it), with the same backoff.
const REDIAL_GRACE: Duration = Duration::from_secs(15);
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
    /// This machine dialled it (else it answered).
    dialled: bool,
}

impl Session {
    fn new(
        stream: &TcpStream,
        hs: Handshake,
        name: &str,
        port: u16,
        dialled: bool,
    ) -> Result<Self> {
        let id = peer_id(&hs.key);
        Ok(Self {
            dialled,
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

    /// Without waiting (pings, goodbyes): skipped while another write is in
    /// progress, and a frame the socket cannot take at once ends the
    /// session (its peer stopped reading). Sent?
    fn try_send(&self, msg: &Control) -> bool {
        let writer = match self.writer.try_lock() {
            Ok(writer) => writer,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        let Ok(frame) = self.cipher.seal_control(msg) else {
            return false;
        };
        // SAFETY: `frame` holds `frame.len()` bytes and `writer` keeps the
        // socket open.
        let n = unsafe {
            libc::send(
                writer.as_raw_fd(),
                frame.as_ptr().cast(),
                frame.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        let sent = usize::try_from(n).is_ok_and(|n| n == frame.len());
        if !sent {
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
    /// The user's pairing was accepted there: our user confirms the code
    /// (Confirm / Cancel) before anything is pinned here.
    Confirm,
    Live,
}

struct Entry {
    session: Arc<Session>,
    stage: Stage,
    since: Instant,
    /// Live, and its key was pinned by this session's pairing: unpinned
    /// again if the other side answers `Denied` (it could not keep it).
    fresh: bool,
}

/// Reconnects to a paired machine since it was last connected.
struct Redial {
    attempts: u32,
    next: Instant,
    /// Shown on its row: it declined, another machine answered, or it
    /// speaks another protocol.
    problem: Option<String>,
}

/// Why a dial failed.
enum DialError {
    /// The machine answered `Denied`.
    Declined(String),
    /// Another key answered than the one wanted.
    WrongKey(String),
    /// It speaks another network protocol (`OtherProtocol`).
    Protocol(String),
    Failed(String),
    /// The connection or handshake failed.
    Io(String),
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Declined(e)
        | Self::WrongKey(e)
        | Self::Protocol(e)
        | Self::Failed(e)
        | Self::Io(e)) = self;
        f.write_str(e)
    }
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
    listener: Arc<TcpListener>,
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
    /// Why a change to `net.toml` was not saved (until the next save).
    save_problem: Option<String>,
    /// Why the last beacon was not sent (until the next one is).
    beacon_problem: Option<String>,
    live: Option<Live>,
    /// Pending and live sessions by peer key.
    sessions: HashMap<String, Entry>,
    /// Beacons heard, by key.
    nearby: HashMap<String, Seen>,
    /// Where each peer was last connected this run (reconnects).
    seen_at: HashMap<String, SocketAddr>,
    denied: HashMap<String, Instant>,
    /// Addresses whose pairing request ended unanswered, and when: new
    /// requests from them are `Busy` for `DENIED` (keys cost nothing).
    cooling: HashMap<IpAddr, Instant>,
    /// Reconnects in flight.
    dialing: HashSet<String>,
    /// Paired machines being reconnected, by key.
    redial: HashMap<String, Redial>,
    /// Refreshed by the tick (1 Hz).
    stats: HashMap<String, LinkStats>,
    /// The daemon is stopping: Network stays down.
    stopped: bool,
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
    /// One `PatchNet` op at a time (a `Pair`'s dial runs outside it).
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
        let mut problems = Vec::new();
        match load_net(&paths) {
            Ok(file) => inner.file = file,
            Err(e) => {
                warn!("net.toml: {e:#}");
                problems.push(format!("{e:#}"));
            }
        }
        match load_identity(&paths) {
            Ok(identity) => inner.identity = identity,
            Err(e) => {
                warn!("{e:#}");
                problems.push(format!("{e:#}"));
            }
        }
        inner.problem = (!problems.is_empty()).then(|| problems.join("; "));
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
    /// arrives later as `NetUpdated`), without holding up other patches.
    /// Blocking.
    pub fn patch(&self, ops: &[NetOp]) -> Result<NetState, PatchError> {
        if ops.len() > MAX_NET_OPS {
            return Err(PatchError::Invalid(format!(
                "at most {MAX_NET_OPS} ops per patch"
            )));
        }
        let done = ops.iter().try_for_each(|op| {
            let _one = (!matches!(op, NetOp::Pair { .. })).then(|| lock(&self.shared.patching));
            self.shared.apply(op)
        });
        self.shared.publish();
        done.map(|()| self.shared.state())
    }

    /// Daemon stop: Bye to peers, nodes closed, threads stopped, and no
    /// `PatchNet` brings Network up again. Blocking.
    pub fn shutdown(&self) {
        lock(&self.shared.inner).stopped = true;
        self.shared.down();
    }
}

impl Shared {
    fn apply(self: &Arc<Self>, op: &NetOp) -> Result<(), PatchError> {
        match op {
            NetOp::Pair { address, key } => {
                let addr = resolve(address).map_err(PatchError::Invalid)?;
                let (want, there) = {
                    let inner = lock(&self.inner);
                    let want = match key {
                        Some(key) => Some(key.clone()),
                        None => {
                            // Beacons are unauthenticated: with two claiming
                            // one address, neither key can be expected.
                            let mut claims = inner.nearby.values().filter(|s| s.addr == addr);
                            let want = claims.next().map(|s| s.beacon.key.clone());
                            if claims.next().is_some() {
                                return Err(PatchError::Invalid(format!(
                                    "several machines announce {addr}: pair from the Nearby list"
                                )));
                            }
                            want
                        }
                    };
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
                self.dial(addr, true, want.as_deref()).map_err(|e| match e {
                    DialError::Io(e) | DialError::Protocol(e) => PatchError::Io(anyhow!(e)),
                    e => PatchError::Invalid(e.to_string()),
                })
            }
            NetOp::Respond { id, accept } => self.respond(id, *accept),
            _ => {
                self.edit(|f| f.apply(op))?;
                match op {
                    NetOp::SetEnabled { on: true } => self.up(),
                    NetOp::SetEnabled { on: false } => {
                        self.down();
                        // A key file that still cannot be read stays a problem.
                        let mut inner = lock(&self.inner);
                        inner.problem = inner
                            .identity
                            .is_none()
                            .then(|| load_identity(&self.paths).err())
                            .flatten()
                            .map(|e| format!("identity key: {e:#}"));
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
        self.save(&mut lock(&self.inner), change)
    }

    /// `edit` under a lock already held.
    fn save(
        &self,
        inner: &mut Inner,
        change: impl FnOnce(&mut NetFile) -> Result<(), String>,
    ) -> Result<(), PatchError> {
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
                inner.save_problem = None;
                Ok(())
            }
            Err(e) => Err(invalid.map_or(PatchError::Io(e), PatchError::Invalid)),
        }
    }

    /// Bind and start the threads (once); failures become `problem`.
    fn up(self: &Arc<Self>) {
        let mut inner = lock(&self.inner);
        if inner.live.is_some() || inner.stopped {
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
        let listener = Arc::new(listener);
        let mut threads = Vec::new();
        let (me, flag, sock) = (self.clone(), stop.clone(), listener.clone());
        threads.extend(spawn("hako-net-accept", move || me.accept(&sock, &flag)));
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
        if threads.len() < 3 + usize::from(beacons.is_some()) {
            inner.problem = Some("could not start every network thread (see the log)".into());
        }
        inner.live = Some(Live {
            stop,
            port,
            udp,
            beacons,
            listener,
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
        // SAFETY: `live.listener` keeps the socket open; shutdown only wakes
        // the accept thread's `accept`.
        unsafe { libc::shutdown(live.listener.as_raw_fd(), libc::SHUT_RDWR) };
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
        inner.redial.clear();
        inner.beacon_problem = None;
        drop(inner);
        self.publish();
    }

    fn accept(self: &Arc<Self>, listener: &TcpListener, stop: &Arc<AtomicBool>) {
        let handshakes = Slots::default();
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
            // One thread per connection; past `MAX_HANDSHAKES` still in
            // their handshake (`MAX_HANDSHAKES_PER_IP` from its address),
            // new ones are dropped.
            let Ok(from) = stream.peer_addr() else {
                continue;
            };
            let Some(slot) = Slot::take(&handshakes, from.ip()) else {
                debug!("net: incoming {from}: too many handshakes");
                continue;
            };
            let (me, stop) = (self.clone(), stop.clone());
            spawn("hako-net-in", move || {
                let from = stream.peer_addr();
                if let Err(e) = me.answer(stream, &stop, slot) {
                    debug!("net: incoming {from:?}: {e:#}");
                }
            });
        }
    }

    /// The responder: handshake, Hello, then known keys go live, unknown
    /// ones that ask to pair wait for Accept / Deny. Only those get the
    /// pairing nonce (`wire::code_digest`). Reads the session until it
    /// ends (`slot` is given back once it is set up).
    fn answer(
        self: &Arc<Self>,
        stream: TcpStream,
        stop: &Arc<AtomicBool>,
        slot: Slot,
    ) -> Result<()> {
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
        let mut timed = Deadline::new(&stream).first(FIRST_MESSAGE, FIRST_MESSAGE_WAIT);
        let keyed = handshake(&mut timed, &private, false)?;
        let mut last = 0;
        let Control::Hello {
            name, port, pair, ..
        } = read_control(&mut timed, &keyed.cipher, &mut last)?
        else {
            bail!("no Hello");
        };
        let (hs, reveal) = keyed.reveal()?;
        let s = Arc::new(Session::new(&stream, hs, &name, port, false)?);
        // Held until the reveal is written: it must be the first frame, and
        // `s` is registered (pings, Accept) before.
        let mut writer = lock(&s.writer);
        let (admitted, old) = {
            let mut inner = lock(&self.inner);
            let mut stage = admission(&inner, &own, &s.key, s.addr.ip(), pair);
            let replacing = inner
                .sessions
                .get(&s.key)
                .map(|e| replaces(&own, &s, &e.session));
            let old = match (&stage, replacing) {
                (Ok(Stage::Live), Some(true)) => inner.sessions.remove(&s.key),
                (Ok(Stage::Live), Some(false)) => {
                    stage = Err((Control::Bye, "its other connection is kept"));
                    None
                }
                _ => None,
            };
            let admitted = stage
                .map_err(|(reply, e)| (reply, e.to_string()))
                .and_then(|stage| {
                    register(&mut inner, &s, stage, stop)
                        .map(|()| stage)
                        .map_err(|e| (Control::Bye, e))
                });
            (admitted, old)
        };
        if let Some(old) = old {
            self.close(&old.session, None);
        }
        let stage = match admitted {
            Ok(stage) => stage,
            Err((reply, e)) => {
                drop(writer);
                let _ = s.send(&reply);
                bail!("{}: {e}", s.name);
            }
        };
        let revealed = writer.write_all(&reveal);
        drop(writer);
        let setup = revealed.map_err(anyhow::Error::from).and_then(|()| {
            s.send(&hello)?;
            if stage == Stage::Live {
                self.go_live(&s, false).map_err(|e| anyhow!(message(e)))?;
                s.send(&Control::Accepted)?;
            }
            Ok(())
        });
        if let Err(e) = setup {
            self.close(&s, Some(&Control::Bye));
            // A paired machine's reconnect: the log is all that says why.
            if stage == Stage::Live {
                warn!("net: reconnect from {}: {e:#}", s.name);
            }
            return Err(e);
        }
        drop(slot);
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
    ) -> Result<(), DialError> {
        use DialError::{Declined, Failed, Io, Protocol, WrongKey};
        let (private, own, stop, hello) = {
            let inner = lock(&self.inner);
            let (Some(live), Some(identity)) = (&inner.live, &inner.identity) else {
                return Err(Failed("Network is off".into()));
            };
            (
                identity.private.clone(),
                identity.public.clone(),
                live.stop.clone(),
                hello(&inner.file, live.port, pair),
            )
        };
        let failed = |e: anyhow::Error| {
            let text = format!("{addr}: {e:#}");
            if e.is::<OtherProtocol>() {
                Protocol(text)
            } else {
                Io(text)
            }
        };
        let stream =
            TcpStream::connect_timeout(&addr, HANDSHAKE).map_err(|e| Io(format!("{addr}: {e}")))?;
        prepare(&stream).map_err(|e| Io(format!("{addr}: {e}")))?;
        let mut timed = Deadline::new(&stream);
        let keyed = handshake(&mut timed, &private, true).map_err(failed)?;
        if keyed.key == own {
            return Err(Failed(format!("{addr} is this machine")));
        }
        if want.is_some_and(|key| key != keyed.key) {
            return Err(WrongKey(format!(
                "{addr} is not the machine that announced itself there"
            )));
        }
        if pair {
            let inner = lock(&self.inner);
            let peers = &inner.file.peers;
            if peers.len() >= MAX_PEERS && !peers.iter().any(|p| p.key == keyed.key) {
                return Err(Failed(format!(
                    "at most {MAX_PEERS} paired peers: forget one first"
                )));
            }
        }
        let mut last = 0;
        let revealed = keyed
            .cipher
            .seal_control(&hello)
            .and_then(|frame| Ok(timed.write_all(&frame)?))
            .and_then(|()| keyed.read_reveal(&mut timed, &mut last))
            .map_err(failed)?;
        // A refusal comes instead of the reveal, an admission's Hello after it.
        let (hs, reply) = match revealed {
            Ok(hs) => {
                let reply = read_control(&mut timed, &hs.cipher, &mut last).map_err(failed)?;
                (Some(hs), reply)
            }
            Err(refused) => (None, refused),
        };
        let (hs, name, port) = match (hs, reply) {
            (Some(hs), Control::Hello { name, port, .. }) => (hs, name, port),
            (_, Control::Denied) => return Err(Declined(format!("{addr} declined"))),
            (_, Control::Busy) => {
                return Err(Failed(format!(
                    "{addr} is busy with other pairing requests: try again in a minute"
                )))
            }
            _ => return Err(Failed(format!("{addr} closed the connection"))),
        };
        let s = Arc::new(Session::new(&stream, hs, &name, port, true).map_err(failed)?);
        let (registered, old) = {
            let mut inner = lock(&self.inner);
            let old = inner
                .sessions
                .get(&s.key)
                .is_some_and(|e| replaces(&own, &s, &e.session))
                .then(|| inner.sessions.remove(&s.key))
                .flatten();
            let stage = Stage::Outgoing { shown: pair };
            let registered = register(&mut inner, &s, stage, &stop);
            if let (Ok(()), Some(r)) = (&registered, inner.redial.get_mut(&s.key)) {
                r.problem = None;
            }
            (registered, old)
        };
        if let Some(old) = old {
            self.close(&old.session, None);
        }
        // Dropped without Bye: the other side may hold this connection as
        // its session for now, and the one kept is not this one.
        registered.map_err(Failed)?;
        self.publish();
        let me = self.clone();
        let reader = s.clone();
        spawn("hako-net-session", move || {
            me.read_loop(&reader, stream, last)
        });
        Ok(())
    }

    /// Answer a pending request by peer id: an incoming one, or our own
    /// once the other side accepted it (the user confirms the code).
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
                let fresh = self.go_live(&s, true)?;
                if let Err(e) = s.send(&Control::Accepted) {
                    // It never learns it was accepted: not paired here either.
                    self.close(&s, None);
                    if fresh {
                        self.unpin(&s);
                    }
                    return Err(PatchError::Io(e.context(s.name.clone())));
                }
            }
            (Stage::Incoming, false) => {
                lock(&self.inner)
                    .denied
                    .insert(s.key.clone(), Instant::now());
                self.close(&s, Some(&Control::Denied));
            }
            (Stage::Confirm, true) => {
                if let Err(e) = self.go_live(&s, true) {
                    self.undo(&s);
                    return Err(e);
                }
            }
            (Stage::Confirm, false) => self.undo(&s),
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

    /// Pin `s`'s key (renamed: saved), then open its nodes and audio route.
    /// `new`: the user paired, so a key not pinned yet is pinned (`true`
    /// returned: fresh); otherwise only a key still pinned goes live (a
    /// reconnect in flight never undoes a Forget). Only a session still
    /// registered is pinned, and on failure no fresh pin is left behind.
    fn go_live(self: &Arc<Self>, s: &Arc<Session>, new: bool) -> Result<bool, PatchError> {
        let gone = || PatchError::Invalid(format!("{} disconnected", s.name));
        let fresh = {
            let mut inner = lock(&self.inner);
            let current = inner
                .sessions
                .get(&s.key)
                .is_some_and(|e| Arc::ptr_eq(&e.session, s));
            if !current {
                return Err(gone());
            }
            let known = inner.file.peers.iter().find(|p| p.key == s.key);
            if known.is_none() && !new {
                return Err(PatchError::Invalid(format!(
                    "{} is no longer paired",
                    s.name
                )));
            }
            let fresh = known.is_none();
            let renamed = known.is_none_or(|p| p.name != s.name);
            if renamed || known.is_some_and(|p| p.address != Some(s.addr)) {
                let saved = self.save(&mut inner, |f| {
                    f.trust(Peer {
                        id: s.id.clone(),
                        name: s.name.clone(),
                        key: s.key.clone(),
                        jitter_ms: DEFAULT_JITTER_MS,
                        address: Some(s.addr),
                    })
                });
                match saved {
                    Err(e) if fresh => return Err(e),
                    // Pinned already: live under the saved name.
                    Err(e) if renamed => {
                        let e = format!("{}: new name not saved: {}", s.name, message(e));
                        warn!("net: {e}");
                        inner.save_problem = Some(e);
                    }
                    // Only the address: `seen_at` still has it this run.
                    Err(e) => warn!("net: {}: address not saved: {}", s.name, message(e)),
                    Ok(()) => {}
                }
            }
            fresh
        };
        let link = match self.open_link(s) {
            Ok(link) => link,
            Err(e) => {
                if fresh {
                    self.unpin(s);
                }
                return Err(e);
            }
        };
        let mut inner = lock(&self.inner);
        let mut links = lock(&self.links);
        // Another live session's audio route (two machines behind one
        // address, or a Hello naming another's port) is never replaced.
        let taken = links
            .get(&s.addr)
            .filter(|l| l.key != s.key)
            .and_then(|l| inner.sessions.get(&l.key))
            .map(|e| e.session.name.clone());
        let Some(entry) = inner
            .sessions
            .get_mut(&s.key)
            .filter(|e| Arc::ptr_eq(&e.session, s) && taken.is_none())
        else {
            drop(links);
            drop(inner);
            drop(link);
            if fresh {
                self.unpin(s);
            }
            return Err(match taken {
                Some(other) => PatchError::Invalid(format!(
                    "{} connected from {}, where {other} is connected",
                    s.name, s.addr
                )),
                None => gone(),
            });
        };
        let old = links.insert(s.addr, link);
        drop(links);
        entry.stage = Stage::Live;
        entry.fresh = fresh;
        inner.seen_at.insert(s.key.clone(), s.addr);
        inner.redial.remove(&s.key);
        drop(inner);
        drop(old);
        self.publish();
        Ok(fresh)
    }

    /// `s`'s nodes and audio route.
    fn open_link(&self, s: &Session) -> Result<Arc<Link>, PatchError> {
        let (jitter, udp) = {
            let inner = lock(&self.inner);
            let live = inner
                .live
                .as_ref()
                .ok_or_else(|| PatchError::Invalid("Network was turned off".into()))?;
            let jitter = inner
                .file
                .peers
                .iter()
                .find(|p| p.key == s.key)
                .map_or(DEFAULT_JITTER_MS, |p| p.jitter_ms);
            (jitter, live.udp.clone())
        };
        let port = match &self.audio {
            Some(audio) => Some(
                lock(audio)
                    .open(
                        &send_node(&s.id),
                        &recv_node(&s.id),
                        &s.name,
                        jitter,
                        send_fn(s.cipher.clone(), Arc::downgrade(&udp), s.addr),
                    )
                    .map_err(|e| PatchError::Invalid(format!("{}: {e}", s.name)))?,
            ),
            None => None,
        };
        Ok(Arc::new(Link {
            key: s.key.clone(),
            cipher: s.cipher.clone(),
            port,
        }))
    }

    /// A session the other side accepted does not go live here: `Denied`
    /// makes it unpin what it pinned for it, then the session closes.
    fn undo(&self, s: &Arc<Session>) {
        // `try_send` would skip it during a ping.
        let _ = s.send(&Control::Denied);
        self.close(s, None);
    }

    /// Undo the pin `s`'s pairing made: the other side did not complete it.
    fn unpin(&self, s: &Session) {
        let removed = self.edit(|f| {
            f.peers.retain(|p| p.key != s.key);
            Ok(())
        });
        if let Err(e) = removed {
            let e = message(e);
            warn!("net: {} stays paired here: {e}", s.name);
            lock(&self.inner).save_problem = Some(format!(
                "{} did not complete the pairing but stays paired here: {e}",
                s.name
            ));
        }
    }

    /// Read `s`'s control frames until it ends, then close it.
    fn read_loop(self: &Arc<Self>, s: &Arc<Session>, mut stream: TcpStream, mut last: u64) {
        let _ = stream.set_read_timeout(Some(READ_LIMIT));
        let ended = loop {
            let msg = match read_control(&mut stream, &s.cipher, &mut last) {
                Ok(msg) => msg,
                Err(e) => break Err(e),
            };
            *lock(&s.heard) = Instant::now();
            match msg {
                Control::Ping { t } => {
                    let _ = s.send(&Control::Pong { t });
                }
                Control::Pong { t } => {
                    *lock(&s.rtt_ms) = s.clock().saturating_sub(t) as f32 / 1000.0;
                }
                Control::Accepted => {
                    let mut inner = lock(&self.inner);
                    let entry = inner
                        .sessions
                        .get_mut(&s.key)
                        .filter(|e| Arc::ptr_eq(&e.session, s));
                    match entry {
                        // The user's pairing: pinned only once our user
                        // confirms the code too (`respond`).
                        Some(e) if e.stage == (Stage::Outgoing { shown: true }) => {
                            e.stage = Stage::Confirm;
                            e.since = Instant::now();
                            drop(inner);
                            self.publish();
                        }
                        // A reconnect: only a key still pinned goes live.
                        Some(e) if e.stage == (Stage::Outgoing { shown: false }) => {
                            drop(inner);
                            if let Err(e) = self.go_live(s, false) {
                                let e = format!("{}: {}", s.name, message(e));
                                warn!("net: {e}");
                                if let Some(r) = lock(&self.inner).redial.get_mut(&s.key) {
                                    r.problem = Some(e);
                                }
                                self.undo(s);
                                return;
                            }
                        }
                        _ => {}
                    }
                }
                Control::Denied => {
                    let fresh = lock(&self.inner)
                        .sessions
                        .get(&s.key)
                        .is_some_and(|e| Arc::ptr_eq(&e.session, s) && e.fresh);
                    if fresh {
                        warn!("net: {} could not complete the pairing", s.name);
                        self.unpin(s);
                    }
                    break Ok("denied");
                }
                Control::Bye => break Ok("said goodbye"),
                Control::Hello { .. } | Control::Busy => {}
            }
        };
        match ended {
            // Not a network error (this side's own close is one): a frame
            // that did not decrypt or parse.
            Err(e) if !e.is::<std::io::Error>() => warn!("net: {}: {e:#}, disconnected", s.name),
            Err(e) => debug!("net: {}: disconnected: {e:#}", s.name),
            Ok(why) => debug!("net: {}: {why}", s.name),
        }
        self.close(s, Some(&Control::Bye));
    }

    /// Tell the peer (`msg`), disconnect and drop the session. An incoming
    /// request that ends here cools its address down.
    fn close(&self, s: &Arc<Session>, msg: Option<&Control>) {
        if let Some(msg) = msg {
            s.try_send(msg);
        }
        s.shutdown();
        let link = {
            let mut inner = lock(&self.inner);
            if inner
                .sessions
                .get(&s.key)
                .is_some_and(|e| Arc::ptr_eq(&e.session, s))
            {
                let ended = inner.sessions.remove(&s.key);
                inner.stats.remove(&s.key);
                if ended.is_some_and(|e| e.stage == Stage::Incoming) {
                    cool_down(&mut inner.cooling, s.addr.ip(), Instant::now());
                }
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
        // A beacon is unauthenticated: it changes where a paired machine's
        // next reconnect goes first (`reconnects`), never when, so forged
        // ones cannot make this machine dial at their pace.
        if inner.nearby.len() >= MAX_NEARBY && !inner.nearby.contains_key(&beacon.key) {
            let oldest = inner
                .nearby
                .iter()
                .min_by_key(|(_, s)| s.at)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                inner.nearby.remove(&key);
            }
        }
        inner.nearby.insert(
            beacon.key.clone(),
            Seen {
                beacon,
                addr,
                at: Instant::now(),
            },
        );
        // The tick publishes it (1 Hz, however many beacons arrive).
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
                .filter_map(|e| {
                    let silent = now.duration_since(*lock(&e.session.heard)) > SILENCE;
                    let expired = e.stage != Stage::Live && now.duration_since(e.since) > PENDING;
                    if !silent && !expired {
                        return None;
                    }
                    // Not confirmed in time: the other side unpins.
                    let bye = match e.stage {
                        Stage::Confirm => Control::Denied,
                        _ => Control::Bye,
                    };
                    Some((e.session.clone(), bye, silent))
                })
                .collect();
            inner
                .denied
                .retain(|_, at| now.duration_since(*at) < DENIED);
            inner
                .cooling
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
            inner
                .redial
                .retain(|key, _| inner.file.peers.iter().any(|p| &p.key == key));
            inner
                .seen_at
                .retain(|key, _| inner.file.peers.iter().any(|p| &p.key == key));
            let dials = reconnects(inner, now);
            let beacon = match (&inner.live, &inner.identity, self.discovery) {
                (Some(live), Some(identity), Some(port)) if n.is_multiple_of(BEACON_TICKS) => {
                    live.beacons.clone().map(|sock| {
                        let beacon = Beacon {
                            id: peer_id(&identity.public),
                            name: own_name(&inner.file),
                            key: identity.public.clone(),
                            port: live.port,
                        };
                        let to = SocketAddr::from((Ipv4Addr::BROADCAST, port));
                        (sock, beacon.encode(), to)
                    })
                }
                _ => None,
            };
            (stale, sessions, dials, beacon)
        };
        for (s, bye, silent) in &stale {
            if *silent {
                warn!(
                    "net: {}: silent for {} s, disconnected",
                    s.name,
                    SILENCE.as_secs()
                );
            } else {
                debug!("net: {}: not answered in time", s.name);
            }
            // ponytail: never waits (the tick serves every session), so a
            // Pong being written that instant costs a Confirm's `Denied` and
            // the other side keeps its pin until its reconnect is declined;
            // hand `undo` to a thread if that shows up.
            self.close(s, Some(bye));
        }
        for s in sessions {
            s.try_send(&Control::Ping { t: s.clock() });
        }
        for (key, addrs) in dials {
            // Dropped with the closure, also if the thread is not started.
            let dialing = Dialing(self.clone(), key);
            spawn("hako-net-dial", move || {
                let Dialing(me, key) = &dialing;
                me.redial(key, &addrs);
            });
        }
        if let Some((sock, bytes, to)) = beacon {
            self.announce(&sock, &bytes, to);
        }
        self.publish();
    }

    /// Send a beacon; a failure is shown until one goes out again.
    fn announce(&self, sock: &UdpSocket, beacon: &[u8], to: SocketAddr) {
        let sent = sock.send_to(beacon, to);
        if let Err(e) = &sent {
            debug!("net: beacon: {e}");
        }
        lock(&self.inner).beacon_problem = sent.err().map(|e| {
            format!(
                "discovery: this computer is not announced: {e} (Connect to address still works)"
            )
        });
    }

    /// Reconnect to paired `key` at `addrs` in order (its beacon's address,
    /// then where it was last connected); the first that answers wins. A
    /// decline, another machine answering or another protocol is shown on
    /// its row; the first failure since it was connected is a warning,
    /// later ones are not.
    fn redial(self: &Arc<Self>, key: &str, addrs: &[SocketAddr]) {
        let mut failed: Option<DialError> = None;
        for &addr in addrs {
            match self.dial(addr, false, Some(key)) {
                Ok(()) => {
                    failed = None;
                    break;
                }
                // A decline, impostor or other protocol says more than a
                // later timeout.
                Err(e)
                    if !matches!(
                        failed,
                        Some(
                            DialError::Declined(_)
                                | DialError::WrongKey(_)
                                | DialError::Protocol(_)
                        )
                    ) =>
                {
                    failed = Some(e);
                }
                Err(_) => {}
            }
        }
        let mut inner = lock(&self.inner);
        // Connected: `dial` cleared the problem, a later failure sets it.
        let (Some(r), Some(e)) = (inner.redial.get_mut(key), failed) else {
            return;
        };
        r.problem = match &e {
            DialError::Declined(e) => Some(format!(
                "{e} the reconnect: it may have unpaired this computer (pair again)"
            )),
            DialError::WrongKey(e) | DialError::Protocol(e) => Some(e.clone()),
            DialError::Failed(_) | DialError::Io(_) => None,
        };
        if r.attempts == 1 {
            warn!("net: reconnect: {e} (retrying less and less often)");
        } else {
            debug!("net: reconnect: {e}");
        }
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
                        .or(p.address)
                        .map(|a| a.to_string()),
                    link: s.map(|_| inner.stats.get(&p.key).copied().unwrap_or_default()),
                    problem: match s {
                        Some(s) => lock(&self.links)
                            .get(&s.addr)
                            .and_then(|link| link.port.as_ref())
                            .and_then(NetPort::problem),
                        None => inner.redial.get(&p.key).and_then(|r| r.problem.clone()),
                    },
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
                key: s.beacon.key.clone(),
                address: s.addr.to_string(),
            })
            .collect();
        nearby.sort_by(|a, b| a.id.cmp(&b.id));
        let mut requests: Vec<_> = inner
            .sessions
            .values()
            .filter_map(|e| {
                let (incoming, accepted) = match e.stage {
                    Stage::Incoming => (true, false),
                    Stage::Outgoing { shown: true } => (false, false),
                    Stage::Confirm => (false, true),
                    _ => return None,
                };
                Some(PairRequest {
                    id: e.session.id.clone(),
                    name: e.session.name.clone(),
                    address: e.session.addr.to_string(),
                    code: e.session.code.clone(),
                    incoming,
                    accepted,
                })
            })
            .collect();
        requests.sort_by(|a, b| a.id.cmp(&b.id));
        let problems: Vec<_> = [&inner.problem, &inner.save_problem, &inner.beacon_problem]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        NetState {
            enabled: inner.file.enabled,
            name: own_name(&inner.file),
            id: inner
                .identity
                .as_ref()
                .map(|i| peer_id(&i.public))
                .unwrap_or_default(),
            port: inner.file.port,
            problem: (!problems.is_empty()).then(|| problems.join("; ")),
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

/// The stage an incoming connection from `key` at `ip` gets after its Hello
/// (`pair`: it asks to pair; `own`: this machine's key), or the reply that
/// refuses it and why.
fn admission(
    inner: &Inner,
    own: &str,
    key: &str,
    ip: IpAddr,
    pair: bool,
) -> Result<Stage, (Control, &'static str)> {
    let waiting = |from: Option<IpAddr>| {
        inner
            .sessions
            .values()
            .filter(|e| e.stage == Stage::Incoming)
            .filter(|e| from.is_none_or(|from| e.session.addr.ip() == from))
            .count()
    };
    if key == own {
        Err((Control::Denied, "this machine's own key"))
    } else if inner.denied.contains_key(key) {
        Err((Control::Denied, "denied a moment ago"))
    } else if inner.file.peers.iter().any(|p| p.key == key) {
        Ok(Stage::Live)
    } else if !pair {
        Err((Control::Denied, "not paired"))
    } else if inner.cooling.contains_key(&ip) {
        Err((Control::Busy, "its last pairing request ended unanswered"))
    } else if waiting(None) >= MAX_PENDING || waiting(Some(ip)) >= MAX_PENDING_PER_IP {
        Err((Control::Busy, "too many pairing requests"))
    } else {
        Ok(Stage::Incoming)
    }
}

/// Whether the new session `s` replaces `old`, its key's session (`own`:
/// this machine's key). Dialled the same way: an answered one does (the
/// machine dialling again lost the old one: it restarted without Bye), a
/// dialled one does not. Opposite ways (both machines dialled at once):
/// both sides keep the connection the lower key dialled.
fn replaces(own: &str, s: &Session, old: &Session) -> bool {
    if s.dialled == old.dialled {
        return !s.dialled;
    }
    s.dialled == (own < s.key.as_str())
}

/// Add `s` unless Network went off meanwhile (`stop` is not the live one)
/// or its key has a session already.
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
    inner.sessions.insert(
        s.key.clone(),
        Entry {
            session: s.clone(),
            stage,
            since: Instant::now(),
            fresh: false,
        },
    );
    Ok(())
}

/// Paired machines to dial now: not connected or being dialled, due (see
/// `REDIAL`) and with a known address. The lower id dials at once, the
/// higher one only after `REDIAL_GRACE`. Addresses in order: the beacon's
/// (it may have moved), then where it last connected this run, then the
/// one saved in `net.toml` (a beacon proves nothing).
fn reconnects(inner: &mut Inner, now: Instant) -> Vec<(String, Vec<SocketAddr>)> {
    let Some(own) = inner.identity.as_ref().map(|i| peer_id(&i.public)) else {
        return Vec::new();
    };
    for p in &inner.file.peers {
        if own > p.id && !inner.sessions.contains_key(&p.key) {
            inner.redial.entry(p.key.clone()).or_insert(Redial {
                attempts: 0,
                next: now + REDIAL_GRACE,
                problem: None,
            });
        }
    }
    let dials: Vec<_> = inner
        .file
        .peers
        .iter()
        .filter(|p| {
            !inner.sessions.contains_key(&p.key)
                && !inner.dialing.contains(&p.key)
                && inner.redial.get(&p.key).is_none_or(|r| now >= r.next)
        })
        .filter_map(|p| {
            let mut addrs: Vec<_> = inner
                .nearby
                .get(&p.key)
                .map(|s| s.addr)
                .into_iter()
                .collect();
            for at in [inner.seen_at.get(&p.key).copied(), p.address]
                .into_iter()
                .flatten()
            {
                if !addrs.contains(&at) {
                    addrs.push(at);
                }
            }
            (!addrs.is_empty()).then(|| (p.key.clone(), addrs))
        })
        .collect();
    for (key, _) in &dials {
        inner.dialing.insert(key.clone());
        let r = inner.redial.entry(key.clone()).or_insert(Redial {
            attempts: 0,
            next: now,
            problem: None,
        });
        r.attempts = r.attempts.saturating_add(1);
        r.next = now + redial_delay(r.attempts);
    }
    dials
}

/// A reconnect in flight: its key leaves `dialing` when this is dropped,
/// however the dial ends.
struct Dialing(Arc<Shared>, String);

impl Drop for Dialing {
    fn drop(&mut self) {
        lock(&self.0.inner).dialing.remove(&self.1);
    }
}

/// Note that `ip`'s pairing request ended unanswered (see `MAX_COOLING`).
fn cool_down(cooling: &mut HashMap<IpAddr, Instant>, ip: IpAddr, now: Instant) {
    if cooling.len() >= MAX_COOLING && !cooling.contains_key(&ip) {
        let oldest = cooling.iter().min_by_key(|(_, at)| **at).map(|(ip, _)| *ip);
        if let Some(ip) = oldest {
            cooling.remove(&ip);
        }
    }
    cooling.insert(ip, now);
}

/// The wait after reconnect attempt `n` (1-based): `REDIAL`, doubling, at
/// most `MAX_REDIAL`.
fn redial_delay(n: u32) -> Duration {
    REDIAL
        .saturating_mul(2u32.saturating_pow(n.saturating_sub(1)))
        .min(MAX_REDIAL)
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
/// blocking; true when the socket took it. Runs on PipeWire's data thread:
/// its buffers are made here, once, and it takes no lock.
/// `udp` is weak: the PipeWire thread may drop a closed port's callback
/// late, and a live socket would keep Network's port taken.
fn send_fn(cipher: Arc<Cipher>, udp: Weak<UdpSocket>, to: SocketAddr) -> SendFn {
    // The socket is IPv4 (bound to 0.0.0.0): so are its peers.
    let SocketAddr::V4(to) = to else {
        return Box::new(|_, _| false);
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
            return false;
        }
        let Some(udp) = udp.upgrade() else {
            return false;
        };
        // SAFETY: `packet` holds `n` bytes, `addr` is a valid sockaddr_in,
        // and `udp` keeps the socket open.
        let sent = unsafe {
            libc::sendto(
                udp.as_raw_fd(),
                packet.as_ptr().cast(),
                n,
                libc::MSG_DONTWAIT,
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        usize::try_from(sent).is_ok_and(|sent| sent == n)
    })
}

/// The key file; absent = `None`. Unreadable is an error, never replaced.
fn load_identity(paths: &ConfigPaths) -> Result<Option<Identity>> {
    use std::os::unix::fs::PermissionsExt;
    let path = identity_path(paths);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            // Others could read it (copied, restored from a backup): kept,
            // and made owner-only again.
            let mode = std::fs::metadata(&path)
                .with_context(|| format!("{}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                warn!(
                    "{}: mode {:o}, making it owner-only (600)",
                    path.display(),
                    mode & 0o777
                );
                let tightened = std::fs::Permissions::from_mode(0o600);
                if let Err(e) = std::fs::set_permissions(&path, tightened) {
                    warn!("{}: {e}", path.display());
                }
            }
            Identity::parse(&text)
                .map(Some)
                .with_context(|| format!("{}", path.display()))
        }
        // A link to a missing file (a volume not mounted) is no new key.
        Err(e) if e.kind() == ErrorKind::NotFound && std::fs::symlink_metadata(&path).is_ok() => {
            bail!("{}: links to a missing file", path.display())
        }
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
// ponytail: getaddrinfo has no time limit, so a slow DNS server holds the
// `Pair` that long.
fn resolve(address: &str) -> Result<SocketAddr, String> {
    let address = address.trim();
    let found = match portless(address) {
        Some(host) => (host, DEFAULT_PORT).to_socket_addrs(),
        None => address.to_socket_addrs(),
    };
    found
        .map_err(|e| format!("cannot find {address:?} (host or host:port): {e}"))?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| format!("{address:?} has no IPv4 address"))
}

/// `address`'s host when it names no port: `host`, `1.2.3.4`, `::1`, `[::1]`.
fn portless(address: &str) -> Option<&str> {
    if address.parse::<IpAddr>().is_ok() {
        return Some(address);
    }
    if let Some(v6) = address.strip_prefix('[').and_then(|a| a.strip_suffix(']')) {
        return Some(v6);
    }
    (!address.contains(':')).then_some(address)
}

/// Control connections: no Nagle, handshake time limits.
fn prepare(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE))?;
    stream.set_write_timeout(Some(HANDSHAKE))
}

/// Reads with one time limit for the whole handshake and Hello: a read
/// timeout alone limits each read, so a peer sending a byte now and then
/// could hold the connection (and its thread) forever.
struct Deadline<'a> {
    stream: &'a TcpStream,
    until: Instant,
    /// An earlier limit for the bytes still to come of the first ones.
    first: Option<(Instant, usize)>,
}

impl<'a> Deadline<'a> {
    fn new(stream: &'a TcpStream) -> Self {
        Self {
            stream,
            until: Instant::now() + HANDSHAKE,
            first: None,
        }
    }

    /// The first `bytes` must also arrive within `wait`.
    fn first(self, bytes: usize, wait: Duration) -> Self {
        Self {
            first: Some((Instant::now() + wait, bytes)),
            ..self
        }
    }
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let until = self.first.map_or(self.until, |(at, _)| at.min(self.until));
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "handshake took too long",
            ));
        }
        self.stream.set_read_timeout(Some(left))?;
        let mut stream = self.stream;
        let n = stream.read(buf)?;
        self.first = self
            .first
            .and_then(|(at, bytes)| (bytes > n).then(|| (at, bytes - n)));
        Ok(n)
    }
}

impl Write for Deadline<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut stream = self.stream;
        stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut stream = self.stream;
        stream.flush()
    }
}

/// Incoming connections in their handshake, by source address.
type Slots = Arc<Mutex<HashMap<IpAddr, usize>>>;

/// One of `MAX_HANDSHAKES` (`MAX_HANDSHAKES_PER_IP` from one address)
/// incoming connections in their handshake; given back when dropped.
struct Slot(Slots, IpAddr);

impl Slot {
    fn take(slots: &Slots, ip: IpAddr) -> Option<Self> {
        let mut taken = lock(slots);
        let from = taken.get(&ip).copied().unwrap_or(0);
        if from >= MAX_HANDSHAKES_PER_IP || taken.values().sum::<usize>() >= MAX_HANDSHAKES {
            return None;
        }
        taken.insert(ip, from + 1);
        Some(Self(slots.clone(), ip))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut taken = lock(&self.0);
        if let Some(n) = taken.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                taken.remove(&self.1);
            }
        }
    }
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
        PatchError::Io(e) | PatchError::Kept(e) => format!("{e:#}"),
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
            key: None,
        }])
        .unwrap();
        accept_both(&a, &b, &a_id, &b_id);
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

    /// Alpha's request to bravo was accepted there: Confirm / Cancel.
    fn accepted(a: &NetService, b_id: &str) -> bool {
        a.state()
            .requests
            .iter()
            .any(|r| r.id == b_id && !r.incoming && r.accepted)
    }

    /// Bravo accepts alpha's request, alpha's user confirms the code.
    fn accept_both(a: &NetService, b: &NetService, a_id: &str, b_id: &str) {
        b.patch(&[NetOp::Respond {
            id: a_id.into(),
            accept: true,
        }])
        .unwrap();
        wait("accepted", || accepted(a, b_id));
        a.patch(&[NetOp::Respond {
            id: b_id.into(),
            accept: true,
        }])
        .unwrap();
        wait("connected", || connected(a, b_id) && connected(b, a_id));
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
                key: None,
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
        // Confirming before bravo accepted is refused.
        let confirm = [NetOp::Respond {
            id: b_id.clone(),
            accept: true,
        }];
        assert!(a.patch(&confirm).is_err());

        b.patch(&[NetOp::Respond {
            id: a_id.clone(),
            accept: true,
        }])
        .unwrap();
        // Bravo's Accept pins nothing on alpha until its user confirms.
        wait("accepted", || accepted(&a, &b_id));
        assert!(!trusts(&a_paths, &b_id) && !connected(&a, &b_id));
        assert_eq!(a.state().requests[0].code, out.code);
        a.patch(&confirm).unwrap();
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
            key: None,
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
            key: None,
        }];
        let impostor = "00".repeat(32);
        announce(&impostor);
        assert_eq!(a.state().nearby[0].address, format!("127.0.0.1:{b_port}"));
        match a.patch(&pair) {
            Err(PatchError::Invalid(e)) => assert!(e.contains("announced"), "{e}"),
            other => panic!("{other:?}"),
        }
        // Two beacons claim the address: a typed address expects neither,
        // the Nearby row clicked names its key.
        announce(&b_key);
        match a.patch(&pair) {
            Err(PatchError::Invalid(e)) => assert!(e.contains("several"), "{e}"),
            other => panic!("{other:?}"),
        }
        let row = |key: &str| {
            [NetOp::Pair {
                address: format!("127.0.0.1:{b_port}"),
                key: Some(key.into()),
            }]
        };
        match a.patch(&row(&impostor)) {
            Err(PatchError::Invalid(e)) => assert!(e.contains("announced"), "{e}"),
            other => panic!("{other:?}"),
        }
        let state = a.patch(&row(&b_key)).unwrap();
        assert_eq!(state.requests.len(), 1);
        assert!(state.nearby.len() == 2, "not paired yet: still nearby");
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
            key: None,
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
        let mut send = send_fn(
            Arc::new(a.cipher),
            Arc::downgrade(&tx),
            rx.local_addr().unwrap(),
        );
        let block = [1i16, -2, 3, -4];
        assert!(send(960, &block));
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
        // Nothing went out: not sent.
        assert!(!send(0, &[1; MAX_SAMPLES + 2]), "too long");
        drop(tx);
        assert!(!send(1920, &block), "socket closed");
    }

    /// A session to a stand-in peer (keys from a real handshake), not
    /// registered; keep the listener while it is used.
    fn session(name: &str) -> (Arc<Session>, TcpListener) {
        let (hs, _, _, _) = wire::tests::pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let s = Session::new(&stream, hs, name, DEFAULT_PORT, false).unwrap();
        (Arc::new(s), listener)
    }

    #[test]
    fn go_live_pins_only_a_live_session_and_a_reconnect_never_pins() {
        let (net, paths, _) = machine("golive");
        turn_on(&net);
        let (s, _peer) = session("Peer");
        assert!(net.shared.go_live(&s, true).is_err(), "not registered");
        assert!(!trusts(&paths, &s.id));
        let stop = lock(&net.shared.inner).live.as_ref().unwrap().stop.clone();
        let reconnect = Stage::Outgoing { shown: false };
        register(&mut lock(&net.shared.inner), &s, reconnect, &stop).unwrap();
        // A reconnect of a forgotten peer goes nowhere and pins nothing.
        assert!(net.shared.go_live(&s, false).is_err());
        assert!(!trusts(&paths, &s.id) && !connected(&net, &s.id));
        // The user's pairing pins it.
        assert!(matches!(net.shared.go_live(&s, true), Ok(true)));
        assert!(trusts(&paths, &s.id) && connected(&net, &s.id));
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_paired_machine_goes_live_when_its_new_name_cannot_be_saved() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root writes into a read-only directory
        }
        let (net, paths, _) = machine("rename");
        turn_on(&net);
        let taken = "discovery port 47820: taken".to_string();
        lock(&net.shared.inner).problem = Some(taken.clone());
        let (s, _peer) = session("Tower");
        net.shared
            .edit(|f| {
                f.trust(Peer {
                    id: s.id.clone(),
                    name: "Desk".into(),
                    key: s.key.clone(),
                    jitter_ms: DEFAULT_JITTER_MS,
                    address: Some(s.addr),
                })
            })
            .unwrap();
        let stop = lock(&net.shared.inner).live.as_ref().unwrap().stop.clone();
        let reconnect = Stage::Outgoing { shown: false };
        register(&mut lock(&net.shared.inner), &s, reconnect, &stop).unwrap();
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(&paths.root, mode(0o500)).unwrap();
        let live = net.shared.go_live(&s, false);
        std::fs::set_permissions(&paths.root, mode(0o700)).unwrap();
        assert!(matches!(live, Ok(false)));
        let state = net.state();
        assert!(state.peers[0].connected && state.peers[0].name == "Desk");
        let problem = state.problem.unwrap();
        assert!(
            problem.contains("not saved") && problem.contains(&taken),
            "{problem}"
        );
        // Saved again: only the other problem is left.
        let state = net
            .patch(&[NetOp::SetName {
                name: "Desk".into(),
            }])
            .unwrap();
        assert_eq!(state.problem, Some(taken));
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_peer_on_another_protocol_says_so_on_its_row() {
        let (net, paths, _) = machine("protocol");
        turn_on(&net);
        let key = "ee".repeat(32);
        net.shared
            .edit(|f| {
                f.trust(Peer {
                    id: peer_id(&key),
                    name: "Newer".into(),
                    key: key.clone(),
                    jitter_ms: DEFAULT_JITTER_MS,
                    address: None,
                })
            })
            .unwrap();
        // Answers the first handshake message as a newer hako would.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let newer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let private = Identity::generate().unwrap().private;
            let mut hs = snow::Builder::new(wire::NOISE.parse().unwrap())
                .local_private_key(&private)
                .unwrap()
                .build_responder()
                .unwrap();
            let mut first = [0u8; FIRST_MESSAGE];
            stream.read_exact(&mut first).unwrap();
            hs.read_message(&first[2..], &mut [0u8; 64]).unwrap();
            let mut msg = [0u8; 1024];
            let n = hs
                .write_message(&[wire::PROTOCOL + 1; 33], &mut msg)
                .unwrap();
            stream.write_all(&(n as u16).to_be_bytes()).unwrap();
            stream.write_all(&msg[..n]).unwrap();
        });
        lock(&net.shared.inner).redial.insert(
            key.clone(),
            Redial {
                attempts: 1,
                next: Instant::now() + MAX_REDIAL,
                problem: None,
            },
        );
        net.shared.redial(&key, &[addr]);
        newer.join().unwrap();
        let problem = net.state().peers[0].problem.clone();
        assert!(
            problem
                .as_deref()
                .is_some_and(|p| p.contains("update both computers")),
            "{problem:?}"
        );
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_beacon_not_sent_is_a_problem_until_one_is() {
        let (net, paths, _) = machine("announce");
        let to = |port| SocketAddr::from((Ipv4Addr::BROADCAST, port));
        // Broadcasts need SO_BROADCAST.
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        net.shared.announce(&sock, b"beacon", to(9));
        let problem = net.state().problem;
        assert!(
            problem
                .as_deref()
                .is_some_and(|p| p.contains("Connect to address still works")),
            "{problem:?}"
        );
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        net.shared
            .announce(&sock, b"beacon", rx.local_addr().unwrap());
        assert_eq!(net.state().problem, None);
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_key_link_to_a_missing_file_is_no_new_key() {
        let (net, paths, _) = machine("key-link");
        let gone = paths.root.join("gone.key");
        std::os::unix::fs::symlink(&gone, identity_path(&paths)).unwrap();
        let state = net.patch(&[NetOp::SetEnabled { on: true }]).unwrap();
        let problem = state.problem.unwrap();
        assert!(problem.contains("missing file"), "{problem}");
        assert!(state.id.is_empty() && !gone.exists(), "a new key was made");
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_second_machine_at_a_live_address_is_refused() {
        let (net, paths, _) = machine("one-address");
        turn_on(&net);
        let stop = lock(&net.shared.inner).live.as_ref().unwrap().stop.clone();
        let ((first, _p1), (second, _p2)) = (session("Desk"), session("Tower"));
        assert_eq!(first.addr, second.addr);
        for s in [&first, &second] {
            let reconnect = Stage::Outgoing { shown: false };
            register(&mut lock(&net.shared.inner), s, reconnect, &stop).unwrap();
        }
        assert!(matches!(net.shared.go_live(&first, true), Ok(true)));
        assert!(net.shared.go_live(&second, true).is_err());
        assert_eq!(lock(&net.shared.links)[&first.addr].key, first.key);
        assert!(connected(&net, &first.id) && !connected(&net, &second.id));
        assert!(!trusts(&paths, &second.id));
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_crossed_pairing_request_is_not_a_decline() {
        let (a, a_paths, a_port) = machine("crossed-a");
        let (b, b_paths, b_port) = machine("crossed-b");
        turn_on(&a);
        turn_on(&b);
        a.patch(&[NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
            key: None,
        }])
        .unwrap();
        // Bravo asks alpha too while alpha's request waits there.
        let to = SocketAddr::from(([127, 0, 0, 1], a_port));
        let crossed = b.shared.dial(to, true, None);
        assert!(matches!(crossed, Err(DialError::Failed(_))));
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn network_stays_down_after_shutdown() {
        let (net, paths, port) = machine("stopped");
        net.shutdown();
        let state = net.patch(&[NetOp::SetEnabled { on: true }]).unwrap();
        assert!(state.enabled);
        assert!(TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok());
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_pairing_the_initiator_cannot_save_is_undone_on_both_sides() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root writes into a read-only directory
        }
        let (a, a_paths, _) = machine("half-a");
        let (b, b_paths, b_port) = machine("half-b");
        let (a_id, b_id) = (turn_on(&a), turn_on(&b));
        a.patch(&[NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
            key: None,
        }])
        .unwrap();
        b.patch(&[NetOp::Respond {
            id: a_id.clone(),
            accept: true,
        }])
        .unwrap();
        wait("accepted", || accepted(&a, &b_id));
        assert!(trusts(&b_paths, &a_id));
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(&a_paths.root, mode(0o500)).unwrap();
        // A ping holding alpha's writer must not cost the Denied.
        let s = lock(&a.shared.inner)
            .sessions
            .values()
            .next()
            .unwrap()
            .session
            .clone();
        let writing = lock(&s.writer);
        std::thread::scope(|scope| {
            let confirm = scope.spawn(|| {
                a.patch(&[NetOp::Respond {
                    id: b_id.clone(),
                    accept: true,
                }])
            });
            std::thread::sleep(Duration::from_millis(300));
            drop(writing);
            assert!(matches!(confirm.join().unwrap(), Err(PatchError::Io(_))));
        });
        wait("bravo unpinned alpha", || {
            !trusts(&b_paths, &a_id) && b.state().peers.is_empty()
        });
        std::fs::set_permissions(&a_paths.root, mode(0o700)).unwrap();
        assert!(load_net(&a_paths).unwrap().peers.is_empty());
        assert!(a.state().requests.is_empty());
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn cancelling_an_accepted_request_unpins_the_other_side() {
        let (a, a_paths, _) = machine("cancel-a");
        let (b, b_paths, b_port) = machine("cancel-b");
        let (a_id, b_id) = (turn_on(&a), turn_on(&b));
        a.patch(&[NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
            key: None,
        }])
        .unwrap();
        b.patch(&[NetOp::Respond {
            id: a_id.clone(),
            accept: true,
        }])
        .unwrap();
        wait("accepted", || accepted(&a, &b_id));
        a.patch(&[NetOp::Respond {
            id: b_id.clone(),
            accept: false,
        }])
        .unwrap();
        wait("bravo unpinned alpha", || {
            !trusts(&b_paths, &a_id) && b.state().peers.is_empty()
        });
        assert!(a.state().requests.is_empty() && a.state().peers.is_empty());
        assert!(!trusts(&a_paths, &b_id));
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn one_address_gets_one_pending_request_and_then_busy() {
        let (a, a_paths, _) = machine("busy-a");
        let (b, b_paths, b_port) = machine("busy-b");
        let (c, c_paths, _) = machine("busy-c");
        turn_on(&a);
        turn_on(&b);
        turn_on(&c);
        let pair = [NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
            key: None,
        }];
        a.patch(&pair).unwrap();
        // Charlie dials from the same address while alpha's request waits.
        match c.patch(&pair) {
            Err(PatchError::Invalid(e)) => assert!(e.contains("busy"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(b.state().requests.len(), 1);
        c.shutdown();
        let _ = std::fs::remove_dir_all(&c_paths.root);
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn an_unanswered_request_cools_its_address_down() {
        let (a, a_paths, _) = machine("cool-a");
        let (b, b_paths, b_port) = machine("cool-b");
        let (c, c_paths, _) = machine("cool-c");
        turn_on(&a);
        turn_on(&b);
        turn_on(&c);
        let pair = [NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
            key: None,
        }];
        a.patch(&pair).unwrap();
        // Nobody answers it in time.
        for e in lock(&b.shared.inner).sessions.values_mut() {
            e.since -= PENDING;
        }
        b.shared.tick(1);
        wait("expired", || {
            a.state().requests.is_empty() && b.state().requests.is_empty()
        });
        // Another key from that address is refused too (keys cost nothing).
        match c.patch(&pair) {
            Err(PatchError::Invalid(e)) => assert!(e.contains("busy"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(b.state().requests.is_empty());
        for at in lock(&b.shared.inner).cooling.values_mut() {
            *at -= DENIED;
        }
        b.shared.tick(1);
        c.patch(&pair).unwrap();
        assert_eq!(b.state().requests.len(), 1);
        c.shutdown();
        let _ = std::fs::remove_dir_all(&c_paths.root);
        done([(a, a_paths), (b, b_paths)]);

        let (mut cooling, now) = (HashMap::new(), Instant::now());
        for i in 0..=MAX_COOLING as u32 {
            let at = now + Duration::from_millis(i.into());
            cool_down(&mut cooling, Ipv4Addr::from(i).into(), at);
        }
        assert_eq!(cooling.len(), MAX_COOLING);
        assert!(!cooling.contains_key(&Ipv4Addr::from(0).into()), "oldest");
    }

    #[test]
    fn only_an_admitted_peer_gets_the_pairing_nonce() {
        let (b, b_paths, b_port) = machine("grind");
        turn_on(&b);
        let connect = || {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, b_port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let private = Identity::generate().unwrap().private;
            let keyed = handshake(&mut stream, &private, true).unwrap();
            (stream, keyed)
        };
        // The handshake alone (no Hello) gets nothing back.
        let (mut stream, _) = connect();
        let read = stream.read(&mut [0u8; 64]);
        assert!(!matches!(read, Ok(n) if n > 0), "revealed before Hello");
        drop(stream);
        // A new key asking to pair: listed, and only then revealed to.
        let ask = || {
            let (mut stream, keyed) = connect();
            let hello = Control::Hello {
                name: "Grinder".into(),
                version: "1".into(),
                port: 1,
                pair: true,
            };
            let frame = keyed.cipher.seal_control(&hello).unwrap();
            stream.write_all(&frame).unwrap();
            let got = keyed.read_reveal(&mut stream, &mut 0).unwrap();
            (stream, got.err())
        };
        let (stream, refused) = ask();
        assert_eq!(refused, None);
        assert_eq!(b.state().requests.len(), 1);
        // Given up at once: its address cools down, so retrying for another
        // code gets no reveal.
        drop(stream);
        wait("request gone", || b.state().requests.is_empty());
        for _ in 0..20 {
            assert_eq!(ask().1, Some(Control::Busy));
        }
        assert!(b.state().requests.is_empty());
        b.shutdown();
        let _ = std::fs::remove_dir_all(&b_paths.root);
    }

    #[test]
    fn pending_requests_are_capped_per_address_and_in_all() {
        let own = "ff".repeat(32);
        let new = "ee".repeat(32);
        let ip = |n: u8| IpAddr::from([10, 0, 0, n]);
        let mut inner = Inner::default();
        let mut peers = Vec::new();
        for n in 1..=MAX_PENDING as u8 {
            assert_eq!(
                admission(&inner, &own, &new, ip(n), true),
                Ok(Stage::Incoming)
            );
            let (s, peer) = session("Peer");
            let mut s = Arc::into_inner(s).unwrap();
            s.addr = SocketAddr::new(ip(n), DEFAULT_PORT);
            let entry = Entry {
                session: Arc::new(s),
                stage: Stage::Incoming,
                since: Instant::now(),
                fresh: false,
            };
            inner.sessions.insert(entry.session.key.clone(), entry);
            peers.push(peer);
        }
        let busy = Err((Control::Busy, "too many pairing requests"));
        assert_eq!(
            admission(&inner, &own, &new, ip(1), true),
            busy,
            "one address"
        );
        let next = ip(MAX_PENDING as u8 + 1);
        assert_eq!(admission(&inner, &own, &new, next, true), busy, "in all");
    }

    #[test]
    fn both_dialling_at_once_keep_one_connection() {
        let ([(a, a_paths), (b, b_paths)], _, _, b_port) = paired("both");
        let a_port = lock(&a.shared.inner).file.port;
        let key = |net: &NetService| lock(&net.shared.inner).file.peers[0].key.clone();
        let (a_key, b_key) = (key(&b), key(&a));
        let session = |net: &NetService| {
            lock(&net.shared.inner)
                .sessions
                .values()
                .find(|e| e.stage == Stage::Live)
                .map(|e| e.session.clone())
        };
        // Their own reconnects stay out of it: these dials are the only ones.
        lock(&a.shared.inner).dialing.insert(b_key.clone());
        lock(&b.shared.inner).dialing.insert(a_key.clone());
        for round in 0..3 {
            let s = session(&a).unwrap();
            a.shared.close(&s, None);
            wait("dropped", || session(&a).is_none() && session(&b).is_none());
            let start = std::sync::Barrier::new(2);
            let to = |port| SocketAddr::from(([127, 0, 0, 1], port));
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    start.wait();
                    let _ = a.shared.dial(to(b_port), false, Some(&b_key));
                });
                scope.spawn(|| {
                    start.wait();
                    let _ = b.shared.dial(to(a_port), false, Some(&a_key));
                });
            });
            wait("connected", || {
                session(&a).is_some() && session(&b).is_some()
            });
            let (sa, sb) = (session(&a).unwrap(), session(&b).unwrap());
            std::thread::sleep(Duration::from_millis(1500));
            let (now_a, now_b) = (session(&a), session(&b));
            assert!(
                now_a.is_some_and(|s| Arc::ptr_eq(&s, &sa))
                    && now_b.is_some_and(|s| Arc::ptr_eq(&s, &sb)),
                "round {round}: replaced or dropped"
            );
            let ends = |s: &Session| (s.sock.local_addr().unwrap(), s.sock.peer_addr().unwrap());
            let (ea, eb) = (ends(&sa), ends(&sb));
            assert_eq!((ea.0, ea.1), (eb.1, eb.0), "round {round}: one connection");
            assert_eq!(sa.dialled, a_key < b_key, "the lower key's");
        }
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn a_key_others_can_read_is_made_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let paths = paths("key-mode");
        let id = Identity::generate().unwrap();
        let path = identity_path(&paths);
        std::fs::write(&path, id.text()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let loaded = load_identity(&paths).unwrap().unwrap();
        assert_eq!(loaded.public, id.public);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn an_expired_confirm_never_waits_for_a_stuck_write() {
        let (net, paths, _) = machine("expired-confirm");
        let (s, _peer) = session("Peer");
        lock(&net.shared.inner).sessions.insert(
            s.key.clone(),
            Entry {
                session: s.clone(),
                stage: Stage::Confirm,
                since: Instant::now() - PENDING - TICK,
                fresh: false,
            },
        );
        let writing = lock(&s.writer);
        let (tx, rx) = mpsc::channel();
        let shared = net.shared.clone();
        std::thread::spawn(move || {
            shared.tick(1);
            let _ = tx.send(());
        });
        let ticked = rx.recv_timeout(Duration::from_secs(2));
        drop(writing);
        assert!(ticked.is_ok(), "the tick waited for the session's writer");
        assert!(lock(&net.shared.inner).sessions.is_empty());
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    /// Paired and connected over loopback: `(alpha, bravo, alpha's id,
    /// bravo's id)` with their paths.
    fn paired(prefix: &str) -> ([(NetService, ConfigPaths); 2], String, String, u16) {
        let (a, a_paths, _) = machine(&format!("{prefix}-a"));
        let (b, b_paths, b_port) = machine(&format!("{prefix}-b"));
        let (a_id, b_id) = (turn_on(&a), turn_on(&b));
        a.patch(&[NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
            key: None,
        }])
        .unwrap();
        accept_both(&a, &b, &a_id, &b_id);
        ([(a, a_paths), (b, b_paths)], a_id, b_id, b_port)
    }

    #[test]
    fn both_restarting_without_beacons_reconnect_to_the_saved_address() {
        let ([(a, a_paths), (b, b_paths)], a_id, b_id, b_port) = paired("restart");
        let saved = load_net(&a_paths).unwrap().peers[0].address;
        assert_eq!(saved, Some(SocketAddr::from(([127, 0, 0, 1], b_port))));
        a.shutdown();
        b.shutdown();
        let start = |paths: &ConfigPaths| {
            let (events, _) = broadcast::channel(256);
            NetService::start(paths.clone(), events, None, None)
        };
        let (a, b) = (start(&a_paths), start(&b_paths));
        wait("reconnected", || {
            connected(&a, &b_id) && connected(&b, &a_id)
        });
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn a_paired_machine_back_without_bye_replaces_its_old_session() {
        let ([(a, a_paths), (b, b_paths)], a_id, b_id, _) = paired("stale");
        let (low, high, low_id, high_id) = if a_id < b_id {
            (&a, &b, &a_id, &b_id)
        } else {
            (&b, &a, &b_id, &a_id)
        };
        let session = |net: &NetService| {
            lock(&net.shared.inner)
                .sessions
                .values()
                .next()
                .map(|e| e.session.clone())
        };
        let old = session(high).unwrap();
        // The lower id forgets its session as a crash would; the connection
        // stays up (it still answers pings), so the higher keeps the old one.
        lock(&low.shared.inner).sessions.clear();
        lock(&low.shared.links).clear();
        wait("replaced", || {
            session(high).is_some_and(|s| !Arc::ptr_eq(&s, &old))
                && connected(low, high_id)
                && connected(high, low_id)
        });
        assert!(low.state().peers.iter().all(|p| p.problem.is_none()));
        drop(old);
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn a_declined_reconnect_is_shown_on_the_peer() {
        let (a, a_paths, _) = machine("decline-a");
        let (b, b_paths, b_port) = machine("decline-b");
        let (a_id, b_id) = (turn_on(&a), turn_on(&b));
        a.patch(&[NetOp::Pair {
            address: format!("127.0.0.1:{b_port}"),
            key: None,
        }])
        .unwrap();
        accept_both(&a, &b, &a_id, &b_id);
        // The higher id forgets the lower one, which then redials.
        let (low, high, low_id, high_id) = if a_id < b_id {
            (&a, &b, &a_id, &b_id)
        } else {
            (&b, &a, &b_id, &a_id)
        };
        high.patch(&[NetOp::Forget { id: low_id.clone() }]).unwrap();
        wait("declined", || {
            low.state().peers.iter().any(|p| {
                &p.id == high_id
                    && !p.connected
                    && p.problem.as_deref().is_some_and(|e| e.contains("declined"))
            })
        });
        done([(a, a_paths), (b, b_paths)]);
    }

    #[test]
    fn reconnects_back_off_and_fall_back_from_a_beacon_to_the_verified_address() {
        let key = "ff".repeat(32);
        let mut inner = Inner {
            identity: Some(Identity {
                private: Vec::new(),
                public: "00".repeat(32),
            }),
            ..Inner::default()
        };
        inner.file.peers.push(Peer {
            id: peer_id(&key),
            name: "Peer".into(),
            key: key.clone(),
            jitter_ms: DEFAULT_JITTER_MS,
            address: Some("10.0.0.3:47821".parse().unwrap()),
        });
        let beacon: SocketAddr = "10.0.0.9:47821".parse().unwrap();
        let verified: SocketAddr = "10.0.0.2:47821".parse().unwrap();
        let saved: SocketAddr = "10.0.0.3:47821".parse().unwrap();
        inner.seen_at.insert(key.clone(), verified);
        inner.nearby.insert(
            key.clone(),
            Seen {
                beacon: Beacon {
                    id: peer_id(&key),
                    name: "Peer".into(),
                    key: key.clone(),
                    port: 47821,
                },
                addr: beacon,
                at: Instant::now(),
            },
        );
        let now = Instant::now();
        assert_eq!(
            reconnects(&mut inner, now),
            [(key.clone(), vec![beacon, verified, saved])]
        );
        assert!(reconnects(&mut inner, now).is_empty(), "in flight");
        let mut at = now;
        let mut gaps = Vec::new();
        for _ in 0..10 {
            inner.dialing.clear();
            let next = inner.redial[&key].next;
            gaps.push((next - at).as_secs());
            let early = next - Duration::from_millis(1);
            assert!(reconnects(&mut inner, early).is_empty());
            assert_eq!(reconnects(&mut inner, next).len(), 1);
            at = next;
        }
        assert_eq!(gaps, [3, 6, 12, 24, 48, 96, 192, 300, 300, 300]);

        // A beacon from a new address changes where the next attempt goes,
        // not when.
        let (net, paths, _) = machine("beacon-backoff");
        *lock(&net.shared.inner) = inner;
        let before = lock(&net.shared.inner).redial[&key].next;
        for port in 1..=5 {
            let beacon = Beacon {
                id: peer_id(&key),
                name: "Peer".into(),
                key: key.clone(),
                port,
            };
            net.shared
                .heard(&beacon.encode(), "10.0.0.66:47820".parse().unwrap());
        }
        let mut inner = lock(&net.shared.inner);
        assert_eq!(inner.redial[&key].next, before);
        assert_eq!(inner.redial[&key].attempts, 11);
        inner.dialing.clear();
        assert_eq!(
            reconnects(&mut inner, before),
            [(
                key.clone(),
                vec!["10.0.0.66:5".parse().unwrap(), verified, saved]
            )]
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn the_higher_id_redials_after_a_grace_period() {
        let key = "00".repeat(32);
        let mut inner = Inner {
            identity: Some(Identity {
                private: Vec::new(),
                public: "ff".repeat(32),
            }),
            ..Inner::default()
        };
        inner.file.peers.push(Peer {
            id: peer_id(&key),
            name: "Peer".into(),
            key: key.clone(),
            jitter_ms: DEFAULT_JITTER_MS,
            address: Some("10.0.0.3:47821".parse().unwrap()),
        });
        let now = Instant::now();
        assert!(reconnects(&mut inner, now).is_empty(), "the lower id dials");
        let due = now + REDIAL_GRACE;
        assert!(reconnects(&mut inner, due - Duration::from_millis(1)).is_empty());
        assert_eq!(reconnects(&mut inner, due).len(), 1);
        inner.dialing.clear();
        assert!(reconnects(&mut inner, due + Duration::from_secs(2)).is_empty());
        assert_eq!(reconnects(&mut inner, due + REDIAL).len(), 1, "backoff");
    }

    #[test]
    fn one_address_holds_at_most_two_handshake_slots() {
        let slots = Slots::default();
        let (a, b): (IpAddr, IpAddr) = ("10.0.0.2".parse().unwrap(), "10.0.0.3".parse().unwrap());
        let held: Vec<_> = (0..2).map(|_| Slot::take(&slots, a).unwrap()).collect();
        assert!(Slot::take(&slots, a).is_none());
        let others: Vec<_> = (0..2).map(|_| Slot::take(&slots, b).unwrap()).collect();
        drop(held);
        assert!(Slot::take(&slots, a).is_some());
        drop(others);
        assert!(lock(&slots).is_empty());

        // And `MAX_HANDSHAKES` from all addresses together.
        let held: Vec<_> = (0..MAX_HANDSHAKES as u8)
            .map(|i| Slot::take(&slots, IpAddr::from([10, 0, 1, i / 2])).unwrap())
            .collect();
        assert!(Slot::take(&slots, "10.0.2.1".parse().unwrap()).is_none());
        drop(held);
    }

    #[test]
    fn the_first_handshake_message_has_a_shorter_time_limit() {
        // The peer sends `sent` bytes, waits 600 ms, then one more.
        let read = |sent: usize| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let peer = std::thread::spawn(move || {
                let (mut peer, _) = listener.accept().unwrap();
                peer.write_all(&vec![0; sent]).unwrap();
                std::thread::sleep(Duration::from_millis(600));
                let _ = peer.write_all(&[0]);
            });
            let stream = TcpStream::connect(addr).unwrap();
            let start = Instant::now();
            let mut timed = Deadline::new(&stream).first(FIRST_MESSAGE, Duration::from_millis(300));
            let read = timed.read_exact(&mut [0u8; FIRST_MESSAGE + 1]);
            let took = start.elapsed();
            drop(stream);
            peer.join().unwrap();
            (read.is_ok(), took)
        };
        // Most of the first message, then a stall: dropped at 300 ms.
        let (ok, took) = read(10);
        assert!(!ok && took < Duration::from_millis(550), "{took:?}");
        // All of it at once: the rest only has the whole handshake's limit.
        let (ok, took) = read(FIRST_MESSAGE);
        assert!(ok && took >= Duration::from_millis(550), "{took:?}");
    }

    #[test]
    fn nearby_machines_are_capped() {
        let (net, paths, _) = machine("crowd");
        let from = "10.0.0.2:47820".parse().unwrap();
        for i in 0..100u32 {
            let key = format!("{i:064x}");
            let beacon = Beacon {
                id: peer_id(&key),
                name: "Crowd".into(),
                key,
                port: 47821,
            };
            net.shared.heard(&beacon.encode(), from);
        }
        let inner = lock(&net.shared.inner);
        assert_eq!(inner.nearby.len(), MAX_NEARBY);
        assert!(inner.nearby.contains_key(&format!("{:064x}", 99)));
        drop(inner);
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_handshake_has_one_time_limit_however_slowly_bytes_come() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let drip = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            for _ in 0..40 {
                if peer.write_all(&[0]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let stream = TcpStream::connect(addr).unwrap();
        let start = Instant::now();
        let mut timed = Deadline {
            stream: &stream,
            until: start + Duration::from_millis(300),
            first: None,
        };
        let read = timed.read_exact(&mut [0u8; 1000]);
        let kind = read.unwrap_err().kind();
        assert!(
            matches!(kind, ErrorKind::TimedOut | ErrorKind::WouldBlock),
            "{kind:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        drop(stream);
        drip.join().unwrap();
    }

    #[test]
    fn a_silent_pair_target_does_not_hold_up_other_patches() {
        let (net, paths, _) = machine("silent");
        turn_on(&net);
        // Connects, then never says anything.
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = silent.local_addr().unwrap().to_string();
        std::thread::scope(|scope| {
            let pair = scope.spawn(|| net.patch(&[NetOp::Pair { address, key: None }]));
            std::thread::sleep(Duration::from_millis(300));
            let start = Instant::now();
            net.patch(&[NetOp::SetName {
                name: "Desk".into(),
            }])
            .unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "{:?}",
                start.elapsed()
            );
            assert!(pair.join().unwrap().is_err());
        });
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn pings_and_goodbyes_never_wait_for_a_stuck_write() {
        let (s, _peer) = session("Peer");
        let stuck = lock(&s.writer);
        let start = Instant::now();
        assert!(!s.try_send(&Control::Ping { t: 1 }));
        assert!(start.elapsed() < Duration::from_millis(500));
        drop(stuck);
        assert!(s.try_send(&Control::Ping { t: 2 }));
    }

    #[test]
    fn load_problems_add_up_and_off_keeps_an_unreadable_key() {
        let (net, paths, _) = machine("unreadable");
        net.shutdown();
        std::fs::write(identity_path(&paths), "not a key\n").unwrap();
        let (events, _) = broadcast::channel(8);
        let net = NetService::start(paths.clone(), events, None, None);
        assert!(net.state().problem.is_some());
        let state = net.patch(&[NetOp::SetEnabled { on: false }]).unwrap();
        assert!(state
            .problem
            .is_some_and(|p| p.contains("net-identity.key")));
        net.shutdown();
        std::fs::write(hako_core::net::net_path(&paths), "version = [").unwrap();
        let (events, _) = broadcast::channel(8);
        let net = NetService::start(paths.clone(), events, None, None);
        let problem = net.state().problem.unwrap();
        assert!(
            problem.contains("net.toml") && problem.contains("net-identity.key"),
            "{problem}"
        );
        net.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
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
        assert_eq!(portless("desk.local"), Some("desk.local"));
        assert_eq!(portless("::1"), Some("::1"));
        assert_eq!(portless("[::1]"), Some("::1"));
        assert_eq!(portless("[::1]:47821"), None);
        assert_eq!(portless("desk.local:5000"), None);
        // A bad port is an error, not a host name to look up.
        let bad = resolve("1.2.3.4:99999").unwrap_err();
        assert!(bad.contains("invalid port"), "{bad}");
        let v6 = resolve("[::1]:5000").unwrap_err();
        assert!(v6.contains("no IPv4"), "{v6}");
    }
}
