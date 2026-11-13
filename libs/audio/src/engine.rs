//! The PipeWire control plane: one connection on its own thread, a registry
//! cache callers read without a round trip, and the few requests hako makes
//! (links, node `Props`, the default sink).
//!
//! The thread reconnects with backoff when the server goes away; meanwhile the
//! cache is empty and `connected` is false, and `AudioService`'s healing
//! rebuilds ChatMix / AUX once the nodes are back.
//!
//! Links are created without `object.linger`, so the server destroys them with
//! their proxy. Observed on PipeWire 1.6.2 with two `hako-test-` loopbacks
//! (`pw-link -l` while a test client held / dropped the proxy): present while
//! the proxy lives; gone after dropping it, after the client exits, and after
//! the client is SIGKILLed. A daemon crash therefore leaves no orphan links.

use anyhow::{anyhow, bail, Result};
use pipewire as pw;
use pw::{
    link::{Link as PwLink, LinkListener},
    metadata::{Metadata, MetadataListener},
    node::{Node as PwNode, NodeListener},
    properties::properties,
    proxy::{ProxyListener, ProxyT},
    spa,
    types::ObjectType,
};
use spa::pod::{serialize::PodSerializer, Object, Pod, Property, Value, ValueArray};
use spa::utils::dict::DictRef;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Longest a request waits for the engine thread (a hung server must not
/// hold the audio lock forever).
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// A fresh engine waits this long for its first registry snapshot.
const CONNECT_WAIT: Duration = Duration::from_secs(2);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// Metadata key `wpctl set-default` writes; WirePlumber follows it.
const CONFIGURED_SINK: &str = "default.configured.audio.sink";
/// Metadata key pavucontrol's "move" writes; WirePlumber links the stream
/// to that node.
/// ponytail: `target.object` only; session managers that read just
/// `target.node` (pipewire-media-session) need that too.
const TARGET: &str = "target.object";
/// Linux `EPIPE`: the core error a lost connection reports.
const EPIPE: i32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    pub media_class: String,
    /// Global props, then the bound node's full info props.
    pub props: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Port {
    pub node: u32,
    pub name: String,
    pub direction: Direction,
    /// `audio.channel` (`FL`, `MONO`, `AUX0`, ...); the mixer links by it.
    pub channel: String,
}

#[derive(Debug, Clone)]
pub struct Link {
    pub output_port: u32,
    pub input_port: u32,
    /// Global props; links hako made also get their info props (`link.async`).
    pub props: HashMap<String, String>,
}

/// Registry cache, keyed by global id (id order is roughly creation order).
#[derive(Debug, Default)]
pub struct Graph {
    /// Holds a complete snapshot of a live server.
    pub connected: bool,
    /// The first connection attempt has finished (either way).
    settled: bool,
    pub nodes: BTreeMap<u32, Node>,
    pub ports: BTreeMap<u32, Port>,
    pub links: BTreeMap<u32, Link>,
    /// Node name in the `default` metadata's `default.audio.sink`.
    pub default_sink: Option<String>,
}

impl Graph {
    pub fn node_id(&self, name: &str) -> Option<u32> {
        self.nodes
            .iter()
            .find(|(_, node)| node.name == name)
            .map(|(&id, _)| id)
    }

    /// `(node name, port name)` of every port in `direction`, in id order.
    pub fn port_names(&self, direction: Direction) -> Vec<(&str, &str)> {
        self.ports
            .values()
            .filter(|port| port.direction == direction)
            .filter_map(|port| {
                Some((
                    self.nodes.get(&port.node)?.name.as_str(),
                    port.name.as_str(),
                ))
            })
            .collect()
    }

    /// `(node id, port id)` of `node:port`.
    pub fn port(&self, node: &str, port: &str, direction: Direction) -> Option<(u32, u32)> {
        self.ports
            .iter()
            .find(|(_, p)| {
                p.direction == direction
                    && p.name == port
                    && self.nodes.get(&p.node).is_some_and(|n| n.name == node)
            })
            .map(|(&id, p)| (p.node, id))
    }

    pub fn has_ports(&self, node: &str, direction: Direction, ports: &[&str]) -> bool {
        ports
            .iter()
            .all(|port| self.port(node, port, direction).is_some())
    }

    /// Audio ports of `node` in `direction` (a sink's monitor ports are not),
    /// as `(port id, channel)` in id order.
    pub fn channels(&self, node: &str, direction: Direction) -> Vec<(u32, &str)> {
        let Some(id) = self.node_id(node) else {
            return Vec::new();
        };
        self.ports
            .iter()
            .filter(|(_, p)| {
                p.node == id && p.direction == direction && !p.name.starts_with("monitor_")
            })
            .map(|(&port, p)| (port, p.channel.as_str()))
            .collect()
    }

    /// `(node id, port id)` of `node`'s `channel` port in `direction`.
    pub fn channel_port(
        &self,
        node: &str,
        channel: &str,
        direction: Direction,
    ) -> Option<(u32, u32)> {
        let id = self.node_id(node)?;
        self.channels(node, direction)
            .into_iter()
            .find(|(_, c)| *c == channel)
            .map(|(port, _)| (id, port))
    }

    fn port_label(&self, id: u32) -> Option<String> {
        let port = self.ports.get(&id)?;
        Some(format!(
            "{}:{}",
            self.nodes.get(&port.node)?.name,
            port.name
        ))
    }

    /// Every link as `("node:port", "node:port")`, like `pw-link -l`.
    pub fn link_names(&self) -> HashSet<(String, String)> {
        self.links
            .values()
            .filter_map(|link| {
                Some((
                    self.port_label(link.output_port)?,
                    self.port_label(link.input_port)?,
                ))
            })
            .collect()
    }

    pub fn links_between(&self, output_port: u32, input_port: u32) -> Vec<u32> {
        self.links
            .iter()
            .filter(|(_, l)| l.output_port == output_port && l.input_port == input_port)
            .map(|(&id, _)| id)
            .collect()
    }

    fn merge_node(&mut self, id: u32, props: HashMap<String, String>) {
        let node = self.nodes.entry(id).or_default();
        if let Some(name) = props.get("node.name") {
            node.name = name.clone();
        }
        if let Some(class) = props.get("media.class") {
            node.media_class = class.clone();
        }
        node.props.extend(props);
    }

    fn merge_link(&mut self, id: u32, ports: (u32, u32), props: HashMap<String, String>) {
        self.links
            .entry(id)
            .or_insert_with(|| Link {
                output_port: ports.0,
                input_port: ports.1,
                props: HashMap::new(),
            })
            .props
            .extend(props);
    }
}

/// The node name in a `default.audio.sink` value (`{ "name": "..." }`).
pub fn parse_default_name(json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .get("name")?
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// The `Spa:String:JSON` value `wpctl set-default` writes.
pub fn format_default_name(name: &str) -> String {
    serde_json::json!({ "name": name }).to_string()
}

/// `Props` with `channelVolumes` set, every channel to `volume` (mute untouched).
pub fn volume_pod(volume: f32, channels: usize) -> Result<Vec<u8>> {
    props_pod(vec![Property::new(
        spa::sys::SPA_PROP_channelVolumes,
        Value::ValueArray(ValueArray::Float(vec![volume; channels])),
    )])
}

/// `Props` with `channelVolumes` (every channel `volume`, linear) and `mute`.
pub fn fader_pod(volume: f32, mute: bool, channels: usize) -> Result<Vec<u8>> {
    props_pod(vec![
        Property::new(
            spa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(vec![volume; channels])),
        ),
        Property::new(spa::sys::SPA_PROP_mute, Value::Bool(mute)),
    ])
}

/// `Props` with a filter-chain `params` struct: `"node:port" value` pairs.
pub fn params_pod(params: &[(String, f32)]) -> Result<Vec<u8>> {
    let values = params
        .iter()
        .flat_map(|(key, value)| [Value::String(key.clone()), Value::Float(*value)])
        .collect();
    props_pod(vec![Property::new(
        spa::sys::SPA_PROP_params,
        Value::Struct(values),
    )])
}

fn props_pod(properties: Vec<Property>) -> Result<Vec<u8>> {
    let object = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Props,
        id: spa::sys::SPA_PARAM_Props,
        properties,
    });
    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &object)
        .map_err(|error| anyhow!("serialize Props: {error:?}"))?;
    Ok(cursor.into_inner())
}

type Reply = mpsc::Sender<Result<u32, String>>;

enum Request {
    Link {
        token: u64,
        output: (u32, u32),
        input: (u32, u32),
        reply: Reply,
    },
    Unlink(u64),
    Destroy(u32, Reply),
    SetDefaultSink(String, Reply),
    SetTarget(u32, Option<String>, Reply),
    SetProps(u32, Vec<u8>, Reply),
}

impl Request {
    fn fail(self, message: &str) {
        let reply = match self {
            Request::Unlink(_) => return,
            Request::Link { reply, .. }
            | Request::Destroy(_, reply)
            | Request::SetDefaultSink(_, reply)
            | Request::SetTarget(_, _, reply)
            | Request::SetProps(_, _, reply) => reply,
        };
        let _ = reply.send(Err(message.to_string()));
    }
}

#[derive(Default)]
struct Shared {
    graph: Mutex<Graph>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Graph> {
        self.graph.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut Graph)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    requests: pw::channel::Sender<Request>,
    started: Instant,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// The process-wide engine, started on first use. Fails while PipeWire is
/// unreachable (right after start: once the first snapshot had its chance).
pub fn engine() -> Result<&'static Engine> {
    let engine = ENGINE.get_or_init(Engine::start);
    let wait = (engine.started + CONNECT_WAIT).saturating_duration_since(Instant::now());
    let (graph, _) = engine
        .shared
        .changed
        .wait_timeout_while(engine.shared.lock(), wait, |graph| !graph.settled)
        .unwrap_or_else(PoisonError::into_inner);
    if !graph.connected {
        bail!("PipeWire is not reachable");
    }
    Ok(engine)
}

impl Engine {
    fn start() -> Self {
        let shared = Arc::new(Shared::default());
        let (requests, receiver) = pw::channel::channel();
        let thread_shared = shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("hako-pipewire".into())
            .spawn(move || run(thread_shared, receiver))
        {
            warn!("spawn PipeWire engine thread: {error}");
        }
        Self {
            shared,
            requests,
            started: Instant::now(),
        }
    }

    /// Read the cache. `f` runs under its lock: no engine requests inside.
    pub fn with_graph<T>(&self, f: impl FnOnce(&Graph) -> T) -> T {
        f(&self.shared.lock())
    }

    /// Wait until `ready` holds for the cache (re-checked on every registry
    /// change, and at least every 100 ms so `ready` may poll a child process).
    pub fn wait_for(
        &self,
        timeout: Duration,
        mut ready: impl FnMut(&Graph) -> Result<bool>,
    ) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut graph = self.shared.lock();
        loop {
            if ready(&graph)? {
                return Ok(true);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            graph = self
                .shared
                .changed
                .wait_timeout(graph, left.min(Duration::from_millis(100)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn request(&self, request: impl FnOnce(Reply) -> Request) -> Result<u32> {
        let (reply, answer) = mpsc::channel();
        if self.requests.send(request(reply)).is_err() {
            bail!("PipeWire engine is not running");
        }
        match answer.recv_timeout(REPLY_TIMEOUT) {
            Ok(result) => result.map_err(anyhow::Error::msg),
            Err(_) => bail!("PipeWire did not answer within {REPLY_TIMEOUT:?}"),
        }
    }

    /// Link `(node id, port id)` output to input. The link lives as long as
    /// the handle (and this process).
    pub fn link(&self, output: (u32, u32), input: (u32, u32)) -> Result<LinkHandle> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut handle = LinkHandle {
            id: 0,
            token: NEXT.fetch_add(1, Ordering::Relaxed),
            requests: self.requests.clone(),
        };
        let token = handle.token;
        handle.id = self.request(|reply| Request::Link {
            token,
            output,
            input,
            reply,
        })?;
        Ok(handle)
    }

    /// Destroy a global hako did not create (e.g. a lingering `pw-link` link).
    pub fn destroy(&self, id: u32) -> Result<()> {
        self.request(|reply| Request::Destroy(id, reply)).map(drop)
    }

    /// What `wpctl set-default` does: the configured default WirePlumber follows.
    pub fn set_default_sink(&self, name: &str) -> Result<()> {
        let name = name.to_string();
        self.request(|reply| Request::SetDefaultSink(name, reply))
            .map(drop)
    }

    /// Move a playback stream to the node named `target`, or (`None`) give
    /// it back to WirePlumber's choice.
    pub fn set_target(&self, stream: u32, target: Option<&str>) -> Result<()> {
        let target = target.map(str::to_string);
        self.request(|reply| Request::SetTarget(stream, target, reply))
            .map(drop)
    }

    /// `set_param(Props)` on a node (pod from `volume_pod` / `params_pod`).
    pub fn set_props(&self, node: u32, pod: Vec<u8>) -> Result<()> {
        self.request(|reply| Request::SetProps(node, pod, reply))
            .map(drop)
    }
}

/// A link hako created; dropping it destroys the link.
pub struct LinkHandle {
    pub id: u32,
    token: u64,
    requests: pw::channel::Sender<Request>,
}

impl std::fmt::Debug for LinkHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LinkHandle({})", self.id)
    }
}

impl Drop for LinkHandle {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Unlink(self.token));
    }
}

fn dict(props: &DictRef) -> HashMap<String, String> {
    props
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn run(shared: Arc<Shared>, requests: pw::channel::Receiver<Request>) {
    pw::init();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(mainloop) => mainloop,
        Err(error) => return warn!("PipeWire main loop: {error}"),
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(context) => context,
        Err(error) => return warn!("PipeWire context: {error}"),
    };
    let conn: Rc<RefCell<Option<Conn>>> = Rc::default();
    let handler = conn.clone();
    let _requests = requests.attach(mainloop.loop_(), move |request| {
        match handler.borrow().as_ref().filter(|conn| !conn.dead.get()) {
            Some(conn) => conn.handle(request),
            None => request.fail("PipeWire is not connected"),
        }
    });

    let mut backoff = BACKOFF_MIN;
    let mut next_try = Instant::now();
    let mut warned = false;
    loop {
        if conn.borrow().as_ref().is_some_and(|conn| conn.dead.get()) {
            drop(conn.borrow_mut().take());
            shared.update(|graph| {
                *graph = Graph {
                    settled: true,
                    ..Graph::default()
                }
            });
            warn!("PipeWire connection lost; reconnecting");
            next_try = Instant::now() + backoff;
        }
        if conn.borrow().is_none() && Instant::now() >= next_try {
            match Conn::connect(&context, &shared) {
                Ok(connected) => *conn.borrow_mut() = Some(connected),
                Err(error) => {
                    shared.update(|graph| graph.settled = true);
                    if !warned {
                        warn!("connect to PipeWire: {error:#}; retrying");
                        warned = true;
                    }
                    next_try = Instant::now() + backoff;
                }
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        if shared.lock().connected {
            backoff = BACKOFF_MIN;
            warned = false;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }
}

/// One server connection. Field order is drop order: proxies and listeners
/// go before the registry and core they belong to.
struct Conn {
    bound: Rc<RefCell<Bound>>,
    _core_listener: pw::core::Listener,
    _registry_listener: pw::registry::Listener,
    registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
    shared: Arc<Shared>,
    dead: Rc<Cell<bool>>,
}

/// Proxies this connection holds (listener before its proxy: drop order).
#[derive(Default)]
struct Bound {
    nodes: HashMap<u32, (NodeListener, PwNode)>,
    metadata: Option<(u32, MetadataListener, Metadata)>,
    links: HashMap<u64, (ProxyListener, LinkListener, PwLink)>,
}

impl Conn {
    fn connect(context: &pw::context::ContextRc, shared: &Arc<Shared>) -> Result<Self> {
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;
        let bound: Rc<RefCell<Bound>> = Rc::default();
        let dead = Rc::new(Cell::new(false));

        let registry_listener = {
            let (weak, on_global, on_remove) = (registry.downgrade(), bound.clone(), bound.clone());
            let (shared, removed) = (shared.clone(), shared.clone());
            registry
                .add_listener_local()
                .global(move |global| {
                    let Some(props) = global.props else {
                        return;
                    };
                    let id = global.id;
                    match global.type_ {
                        ObjectType::Node => {
                            shared.update(|graph| graph.merge_node(id, dict(props)));
                            let Some(node) = weak
                                .upgrade()
                                .and_then(|registry| registry.bind::<PwNode, _>(global).ok())
                            else {
                                return;
                            };
                            let shared = shared.clone();
                            let listener = node
                                .add_listener_local()
                                .info(move |info| {
                                    if let Some(props) = info.props() {
                                        shared.update(|graph| graph.merge_node(id, dict(props)));
                                    }
                                })
                                .register();
                            on_global.borrow_mut().nodes.insert(id, (listener, node));
                        }
                        ObjectType::Port => {
                            let (Some(node), Some(name), Some(direction)) = (
                                props.get("node.id").and_then(|v| v.parse().ok()),
                                props.get("port.name"),
                                match props.get("port.direction") {
                                    Some("in") => Some(Direction::In),
                                    Some("out") => Some(Direction::Out),
                                    _ => None,
                                },
                            ) else {
                                return;
                            };
                            let port = Port {
                                node,
                                name: name.to_string(),
                                direction,
                                channel: props.get("audio.channel").unwrap_or_default().to_string(),
                            };
                            shared.update(|graph| {
                                graph.ports.insert(id, port);
                            });
                        }
                        ObjectType::Link => {
                            let port = |key| props.get(key).and_then(|v| v.parse().ok());
                            if let (Some(output), Some(input)) =
                                (port("link.output.port"), port("link.input.port"))
                            {
                                shared.update(|graph| {
                                    graph.merge_link(id, (output, input), dict(props))
                                });
                            }
                        }
                        ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                            let Some(metadata) = weak
                                .upgrade()
                                .and_then(|registry| registry.bind::<Metadata, _>(global).ok())
                            else {
                                return;
                            };
                            let shared = shared.clone();
                            let listener = metadata
                                .add_listener_local()
                                .property(move |subject, key, _type, value| {
                                    // `key: None` clears every property.
                                    if subject == 0 && key.is_none_or(|k| k == "default.audio.sink")
                                    {
                                        let name = value
                                            .filter(|_| key.is_some())
                                            .and_then(parse_default_name);
                                        shared.update(|graph| graph.default_sink = name);
                                    }
                                    0
                                })
                                .register();
                            on_global.borrow_mut().metadata = Some((id, listener, metadata));
                        }
                        _ => {}
                    }
                })
                .global_remove(move |id| {
                    removed.update(|graph| {
                        graph.nodes.remove(&id);
                        graph.ports.remove(&id);
                        graph.links.remove(&id);
                    });
                    let mut bound = on_remove.borrow_mut();
                    bound.nodes.remove(&id);
                    if bound
                        .metadata
                        .as_ref()
                        .is_some_and(|(meta, ..)| *meta == id)
                    {
                        bound.metadata = None;
                        removed.update(|graph| graph.default_sink = None);
                    }
                })
                .register()
        };

        // Two round trips: the first returns every global, the second the
        // info of the nodes bound while handling them.
        // Compared by sequence number: PipeWire 0.3.x reports `done` without
        // the async bit that `sync` returns, newer versions keep it.
        let pending = Rc::new(Cell::new(Some(core.sync(0)?.seq())));
        let rounds = Rc::new(Cell::new(0));
        let core_listener = {
            let (weak, shared, dead) = (core.downgrade(), shared.clone(), dead.clone());
            core.add_listener_local()
                .done(move |id, seq| {
                    if id != pw::core::PW_ID_CORE || pending.get() != Some(seq.seq()) {
                        return;
                    }
                    rounds.set(rounds.get() + 1);
                    if rounds.get() == 1 {
                        pending.set(
                            weak.upgrade()
                                .and_then(|core| core.sync(0).ok())
                                .map(|seq| seq.seq()),
                        );
                    } else {
                        pending.set(None);
                        shared.update(|graph| {
                            graph.connected = true;
                            graph.settled = true;
                        });
                        info!("PipeWire engine connected");
                    }
                })
                .error(move |id, _seq, res, message| {
                    // Errors of other objects reach their proxy's listener.
                    // The server also reports harmless ones on the core
                    // ("unknown resource" when a proxy is destroyed after
                    // its object went, e.g. links of a node that exited);
                    // only a broken connection is fatal.
                    if id == pw::core::PW_ID_CORE && res == -EPIPE {
                        warn!("PipeWire: {message} ({res})");
                        dead.set(true);
                    } else {
                        debug!("PipeWire: object {id}: {message} ({res})");
                    }
                })
                .register()
        };

        Ok(Self {
            bound,
            _core_listener: core_listener,
            _registry_listener: registry_listener,
            registry,
            core,
            shared: shared.clone(),
            dead,
        })
    }

    fn handle(&self, request: Request) {
        match request {
            Request::Link {
                token,
                output,
                input,
                reply,
            } => self.link(token, output, input, reply),
            Request::Unlink(token) => {
                self.bound.borrow_mut().links.remove(&token);
            }
            Request::Destroy(id, reply) => {
                let result = self.registry.destroy_global(id).into_result();
                let _ = reply.send(result.map(|_| 0).map_err(|e| format!("destroy {id}: {e}")));
            }
            Request::SetDefaultSink(name, reply) => {
                let result = match &self.bound.borrow().metadata {
                    Some((_, _, metadata)) => {
                        let value = format_default_name(&name);
                        metadata.set_property(
                            0,
                            CONFIGURED_SINK,
                            Some("Spa:String:JSON"),
                            Some(&value),
                        );
                        Ok(0)
                    }
                    None => Err("no default metadata (is WirePlumber running?)".to_string()),
                };
                let _ = reply.send(result);
            }
            Request::SetTarget(stream, target, reply) => {
                let result = match &self.bound.borrow().metadata {
                    Some((_, _, metadata)) => {
                        metadata.set_property(stream, TARGET, None, target.as_deref());
                        Ok(0)
                    }
                    None => Err("no default metadata (is WirePlumber running?)".to_string()),
                };
                let _ = reply.send(result);
            }
            Request::SetProps(node, pod, reply) => {
                let result = match (self.bound.borrow().nodes.get(&node), Pod::from_bytes(&pod)) {
                    (Some((_, proxy)), Some(pod)) => {
                        proxy.set_param(spa::param::ParamType::Props, 0, pod);
                        Ok(0)
                    }
                    (None, _) => Err(format!("PipeWire node {node} is gone")),
                    (_, None) => Err("invalid Props pod".to_string()),
                };
                let _ = reply.send(result);
            }
        }
    }

    fn link(&self, token: u64, output: (u32, u32), input: (u32, u32), reply: Reply) {
        // No `object.linger`: the link dies with its proxy (see the module doc).
        let props = properties! {
            "link.output.node" => output.0.to_string(),
            "link.output.port" => output.1.to_string(),
            "link.input.node" => input.0.to_string(),
            "link.input.port" => input.1.to_string(),
            "object.linger" => "false",
        };
        let link: PwLink = match self.core.create_object("link-factory", &props) {
            Ok(link) => link,
            Err(error) => {
                let _ = reply.send(Err(format!("create link: {error}")));
                return;
            }
        };
        let reply = Rc::new(Cell::new(Some(reply)));
        let on_error = reply.clone();
        let proxy_listener = link
            .upcast_ref()
            .add_listener_local()
            .bound(move |id| {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Ok(id));
                }
            })
            .error(move |_seq, res, message| match on_error.take() {
                Some(reply) => {
                    let _ = reply.send(Err(format!("create link: {message} ({res})")));
                }
                None => warn!("hako link: {message} ({res})"),
            })
            .register();
        let shared = self.shared.clone();
        let info_listener = link
            .add_listener_local()
            .info(move |info| {
                let props = info.props().map(dict).unwrap_or_default();
                if props.get("link.async").is_some_and(|value| value == "true") {
                    warn!(
                        "hako link {} is async (adds a quantum of latency)",
                        info.id()
                    );
                }
                let ports = (info.output_port_id(), info.input_port_id());
                shared.update(|graph| graph.merge_link(info.id(), ports, props));
            })
            .register();
        self.bound
            .borrow_mut()
            .links
            .insert(token, (proxy_listener, info_listener, link));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sink_json_round_trips() {
        assert_eq!(
            parse_default_name(r#"{ "name": "alsa_output.x" }"#).as_deref(),
            Some("alsa_output.x")
        );
        let odd = "a \"quoted\" \\ name";
        assert_eq!(
            parse_default_name(&format_default_name(odd)).as_deref(),
            Some(odd)
        );
        assert_eq!(format_default_name("Hako_Game"), r#"{"name":"Hako_Game"}"#);
        assert_eq!(parse_default_name(r#"{ "name": "" }"#), None);
        assert_eq!(parse_default_name("not json"), None);
        assert_eq!(parse_default_name(r#"{ "other": 1 }"#), None);
    }

    fn port(node: u32, name: &str, direction: Direction) -> Port {
        Port {
            node,
            name: name.into(),
            direction,
            channel: String::new(),
        }
    }

    #[test]
    fn graph_resolves_names_and_links() {
        let mut graph = Graph::default();
        for (id, name) in [(10, "line"), (20, "Hako_AUX_DSP")] {
            graph.merge_node(
                id,
                HashMap::from([("node.name".to_string(), name.to_string())]),
            );
        }
        graph
            .ports
            .insert(11, port(10, "capture_FL", Direction::Out));
        graph
            .ports
            .insert(12, port(10, "capture_FR", Direction::Out));
        graph
            .ports
            .insert(21, port(20, "playback_FL", Direction::In));
        graph.merge_link(30, (11, 21), HashMap::new());
        // A link to a port whose node is unknown is not listed.
        graph.merge_link(31, (12, 99), HashMap::new());

        assert_eq!(graph.node_id("Hako_AUX_DSP"), Some(20));
        assert_eq!(
            graph.port("line", "capture_FR", Direction::Out),
            Some((10, 12))
        );
        assert_eq!(graph.port("line", "capture_FR", Direction::In), None);
        assert!(graph.has_ports("line", Direction::Out, &["capture_FL", "capture_FR"]));
        assert!(!graph.has_ports("line", Direction::Out, &["capture_MONO"]));
        assert_eq!(
            graph.port_names(Direction::Out),
            [("line", "capture_FL"), ("line", "capture_FR")]
        );
        assert_eq!(
            graph.link_names(),
            HashSet::from([("line:capture_FL".into(), "Hako_AUX_DSP:playback_FL".into())])
        );
        assert_eq!(graph.links_between(11, 21), [30]);

        // Info props merge into a link's global props.
        graph.merge_link(
            30,
            (11, 21),
            HashMap::from([("link.async".into(), "false".into())]),
        );
        assert_eq!(graph.links[&30].props["link.async"], "false");
    }

    /// Kills and restarts a private server (its own `core.name`), never the
    /// session's. The engine is per process, so this needs its own run.
    #[test]
    #[ignore = "spawns a private PipeWire server; run alone with --exact"]
    fn live_engine_reconnects() {
        use std::process::{Command, Stdio};
        assert!(ENGINE.get().is_none(), "run this test alone");
        let name = format!("hako-test-pw-{}", std::process::id());
        let conf = std::env::temp_dir().join(format!("{name}.conf"));
        std::fs::write(
            &conf,
            format!(
                "context.properties = {{ core.daemon = true core.name = {name} support.dbus = false }}\n\
                 context.spa-libs = {{ support.* = support/libspa-support }}\n\
                 context.modules = [ {{ name = libpipewire-module-protocol-native }} {{ name = libpipewire-module-access }} ]\n"
            ),
        )
        .unwrap();
        let socket =
            std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(&name);
        let spawn = || {
            let child = Command::new("pipewire")
                .arg("-c")
                .arg(&conf)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..50 {
                if socket.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            child
        };
        let stop = |mut child: std::process::Child| {
            let _ = child.kill();
            let _ = child.wait();
        };

        let server = spawn();
        std::env::set_var("PIPEWIRE_REMOTE", &name);
        let engine = engine().expect("connect to the private server");
        let connected = |want: bool| {
            engine
                .wait_for(Duration::from_secs(10), |graph| Ok(graph.connected == want))
                .unwrap()
        };
        assert!(connected(true));
        stop(server);
        assert!(connected(false), "server death not noticed");
        assert!(
            engine.destroy(u32::MAX).is_err(),
            "requests fail while down"
        );
        let server = spawn();
        let back = connected(true);
        stop(server);
        let _ = std::fs::remove_file(&conf);
        for suffix in ["", ".lock", "-manager", "-manager.lock"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", socket.display()));
        }
        assert!(back, "no reconnect after the server came back");
    }

    #[test]
    fn props_pods_serialize() {
        let pod = volume_pod(0.125, 2).unwrap();
        assert!(Pod::from_bytes(&pod).is_some_and(|pod| pod.is_object()));
        let pod = params_pod(&[("fx0:Ratio".into(), 4.0)]).unwrap();
        assert!(Pod::from_bytes(&pod).is_some_and(|pod| pod.is_object()));
        let pod = fader_pod(0.5, true, 2).unwrap();
        assert!(Pod::from_bytes(&pod).is_some_and(|pod| pod.is_object()));
    }
}
