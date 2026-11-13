//! LAN audio between hako machines: `net.toml` (paired peers), the IPC
//! types and the PipeWire node names a peer gets while connected.
//! Design: `docs/superpowers/specs/2026-11-13-lan-audio-design.md`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

pub const NET_VERSION: u32 = 1;
/// Control (TCP) and audio (UDP) port.
pub const DEFAULT_PORT: u16 = 47821;
/// Discovery beacons (UDP broadcast).
pub const DISCOVERY_PORT: u16 = 47820;
pub const MAX_PEERS: usize = 16;
pub const DEFAULT_JITTER_MS: u32 = 12;
pub const JITTER_MIN_MS: u32 = 2;
pub const JITTER_MAX_MS: u32 = 200;
pub const MAX_NAME: usize = 32;
/// Most ops one `PatchNet` may carry.
pub const MAX_NET_OPS: usize = 16;
/// Prefix of every peer node; the mixer offers these as devices.
pub const NODE_PREFIX: &str = "hako_net.";

/// The sink whose audio goes to peer `id` (`hako to <peer>`).
pub fn send_node(id: &str) -> String {
    format!("{NODE_PREFIX}{id}.send")
}

/// The source carrying what peer `id` sends (`hako from <peer>`).
pub fn recv_node(id: &str) -> String {
    format!("{NODE_PREFIX}{id}.recv")
}

/// The peer id of a `send_node` / `recv_node` name and whether it sends.
pub fn peer_of_node(node: &str) -> Option<(&str, bool)> {
    let rest = node.strip_prefix(NODE_PREFIX)?;
    if let Some(id) = rest.strip_suffix(".send") {
        return Some((id, true));
    }
    rest.strip_suffix(".recv").map(|id| (id, false))
}

/// Peer id: hex of the first 8 bytes of its public key (`key`: 64 hex chars).
pub fn peer_id(key: &str) -> String {
    key.chars().take(16).collect()
}

/// Serialises every read-modify-write of `net.toml` in this process.
static NET_LOCK: Mutex<()> = Mutex::new(());

/// `net.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetFile {
    pub version: u32,
    /// Off (the default): nothing listens, no beacons.
    #[serde(default)]
    pub enabled: bool,
    /// Shown to peers; empty = the hostname.
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for NetFile {
    fn default() -> Self {
        Self {
            version: NET_VERSION,
            enabled: false,
            name: String::new(),
            port: DEFAULT_PORT,
            peers: Vec::new(),
        }
    }
}

/// A paired machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Peer {
    /// `peer_id(key)`.
    pub id: String,
    /// Its name when last seen.
    pub name: String,
    /// X25519 public key, 64 lowercase hex chars: what is trusted.
    pub key: String,
    /// Receive buffer for its audio.
    #[serde(default = "default_jitter")]
    pub jitter_ms: u32,
}

fn default_jitter() -> u32 {
    DEFAULT_JITTER_MS
}

/// One edit, from the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum NetOp {
    SetEnabled {
        on: bool,
    },
    SetName {
        name: String,
    },
    /// Dial `address` (`host` or `host:port`) and ask to pair.
    Pair {
        address: String,
    },
    /// A pending request by peer id: an incoming one is accepted or denied;
    /// an outgoing one is cancelled (`accept: false`).
    Respond {
        id: String,
        accept: bool,
    },
    /// Unpair (and disconnect).
    Forget {
        id: String,
    },
    SetJitter {
        id: String,
        jitter_ms: u32,
    },
}

impl NetFile {
    /// The file-only ops (`SetEnabled`, `SetName`, `Forget`, `SetJitter`);
    /// on error nothing changed. `Pair` and `Respond` are the daemon's.
    pub fn apply(&mut self, op: &NetOp) -> Result<(), String> {
        let mut next = self.clone();
        match op {
            NetOp::SetEnabled { on } => next.enabled = *on,
            NetOp::SetName { name } => next.name = name.trim().to_string(),
            NetOp::Forget { id } => {
                next.peers.retain(|p| &p.id != id);
                if next.peers.len() == self.peers.len() {
                    return Err(format!("no paired peer {id:?}"));
                }
            }
            NetOp::SetJitter { id, jitter_ms } => {
                next.peers
                    .iter_mut()
                    .find(|p| &p.id == id)
                    .ok_or_else(|| format!("no paired peer {id:?}"))?
                    .jitter_ms = *jitter_ms;
            }
            NetOp::Pair { .. } | NetOp::Respond { .. } => {
                return Err("not a file op".into());
            }
        }
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Pin `peer` (new, or a known key under a new name).
    pub fn trust(&mut self, peer: Peer) -> Result<(), String> {
        let mut next = self.clone();
        match next.peers.iter_mut().find(|p| p.key == peer.key) {
            Some(slot) => slot.name = peer.name,
            None => next.peers.push(peer),
        }
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version > NET_VERSION {
            return Err(format!(
                "net.toml version {} is newer than this hako ({NET_VERSION})",
                self.version
            ));
        }
        if !self.name.is_empty() {
            validate_name(&self.name)?;
        }
        if self.port == 0 || self.port == DISCOVERY_PORT {
            return Err(format!("port must be 1..=65535 and not {DISCOVERY_PORT}"));
        }
        if self.peers.len() > MAX_PEERS {
            return Err(format!("at most {MAX_PEERS} paired peers"));
        }
        let mut keys = HashSet::new();
        for p in &self.peers {
            validate_key(&p.key)?;
            if p.id != peer_id(&p.key) {
                return Err(format!("peer id {:?} does not match its key", p.id));
            }
            if !keys.insert(p.id.as_str()) {
                return Err(format!("peer {:?} twice", p.id));
            }
            validate_name(&p.name)?;
            validate_jitter(p.jitter_ms)?;
        }
        Ok(())
    }
}

/// Machine names: 1..=32 printable characters.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty()
        || name.chars().count() > MAX_NAME
        || name.chars().any(char::is_control)
    {
        return Err(format!("name must be 1..={MAX_NAME} printable characters"));
    }
    Ok(())
}

/// A public key: 64 lowercase hex characters (32 bytes).
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.len() != 64 || !key.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("key must be 64 lowercase hex characters".into());
    }
    Ok(())
}

pub fn validate_jitter(ms: u32) -> Result<(), String> {
    if !(JITTER_MIN_MS..=JITTER_MAX_MS).contains(&ms) {
        return Err(format!(
            "buffer must be {JITTER_MIN_MS}..={JITTER_MAX_MS} ms"
        ));
    }
    Ok(())
}

/// `net.toml` next to `config.toml`.
pub fn net_path(paths: &crate::ConfigPaths) -> PathBuf {
    paths.root.join("net.toml")
}

/// This machine's private key (hex, owner-only). Never leaves the daemon.
pub fn identity_path(paths: &crate::ConfigPaths) -> PathBuf {
    paths.root.join("net-identity.key")
}

/// Absent = default (off, no peers). Unparsable or invalid is an error,
/// never replaced by defaults.
pub fn load_net(paths: &crate::ConfigPaths) -> Result<NetFile> {
    let path = net_path(paths);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(NetFile::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let file: NetFile =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if let Err(e) = file.validate() {
        bail!("{}: {e}", path.display());
    }
    Ok(file)
}

/// Read-modify-write `net.toml` atomically; nothing is written when `edit`
/// fails or leaves the file invalid.
pub fn update_net<T>(
    paths: &crate::ConfigPaths,
    edit: impl FnOnce(&mut NetFile) -> Result<T>,
) -> Result<(NetFile, T)> {
    let _guard = NET_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut file = load_net(paths)?;
    let out = edit(&mut file)?;
    file.version = NET_VERSION;
    if let Err(e) = file.validate() {
        bail!("{e}");
    }
    paths.ensure()?;
    crate::config::write_atomic(&net_path(paths), &toml::to_string_pretty(&file)?)?;
    Ok((file, out))
}

/// Write the identity key file (owner-only, atomic).
pub fn save_identity(paths: &crate::ConfigPaths, private_hex: &str) -> Result<()> {
    paths.ensure()?;
    crate::config::write_atomic(&identity_path(paths), private_hex)
}

// ── IPC ──────────────────────────────────────────────────────────

/// `PatchNet` params: applied in order; stops at the first failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchNetParams {
    pub ops: Vec<NetOp>,
}

/// `GetNet` / `PatchNet` result and the `NetUpdated` payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NetState {
    pub enabled: bool,
    /// This machine as peers see it.
    pub name: String,
    /// This machine's peer id (empty until the key exists).
    pub id: String,
    pub port: u16,
    /// Why networking is not working (port taken, file unreadable, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// Paired machines.
    #[serde(default)]
    pub peers: Vec<PeerState>,
    /// Discovered, not paired.
    #[serde(default)]
    pub nearby: Vec<NearbyPeer>,
    /// Pairings waiting for an answer.
    #[serde(default)]
    pub requests: Vec<PairRequest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerState {
    pub id: String,
    pub name: String,
    pub jitter_ms: u32,
    pub connected: bool,
    /// `ip:port` of the live session, or where it was last seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// While connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkStats>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkStats {
    /// Audio waiting in the receive buffer.
    pub buffered_ms: f32,
    /// Times the receive buffer ran dry since connecting.
    pub dropouts: u64,
    /// Its audio is arriving right now.
    pub receiving: bool,
    /// We are sending audio right now.
    pub sending: bool,
    /// Control round trip.
    pub rtt_ms: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NearbyPeer {
    pub id: String,
    pub name: String,
    /// `ip:port` to pass to `NetOp::Pair`.
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairRequest {
    pub id: String,
    pub name: String,
    pub address: String,
    /// Six digits from the handshake; equal on both machines.
    pub code: String,
    /// They asked us (Accept / Deny); else we are waiting for them.
    pub incoming: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn peer() -> Peer {
        Peer {
            id: peer_id(KEY),
            name: "Desktop".into(),
            key: KEY.into(),
            jitter_ms: DEFAULT_JITTER_MS,
        }
    }

    #[test]
    fn node_names_round_trip() {
        let id = peer_id(KEY);
        assert_eq!(id, "0011223344556677");
        assert_eq!(send_node(&id), "hako_net.0011223344556677.send");
        assert_eq!(peer_of_node(&send_node(&id)), Some((id.as_str(), true)));
        assert_eq!(peer_of_node(&recv_node(&id)), Some((id.as_str(), false)));
        assert_eq!(peer_of_node("hako_in_voice"), None);
    }

    #[test]
    fn ops_and_trust_keep_the_file_valid() {
        let mut f = NetFile::default();
        f.trust(peer()).unwrap();
        let mut renamed = peer();
        renamed.name = "Tower".into();
        f.trust(renamed).unwrap();
        assert_eq!(f.peers.len(), 1);
        assert_eq!(f.peers[0].name, "Tower");
        let id = f.peers[0].id.clone();
        f.apply(&NetOp::SetJitter {
            id: id.clone(),
            jitter_ms: 40,
        })
        .unwrap();
        let before = f.clone();
        for bad in [
            NetOp::SetJitter {
                id: id.clone(),
                jitter_ms: 1000,
            },
            NetOp::SetName {
                name: "x".repeat(40),
            },
            NetOp::Forget { id: "nope".into() },
            NetOp::Pair {
                address: "10.0.0.2".into(),
            },
        ] {
            assert!(f.apply(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(f, before);
        f.apply(&NetOp::Forget { id }).unwrap();
        assert!(f.peers.is_empty());

        let mut bad = peer();
        bad.key = KEY.to_uppercase();
        assert!(f.trust(bad).is_err(), "uppercase key");
        let mut bad = peer();
        bad.id = "ffffffffffffffff".into();
        assert!(f.trust(bad).is_err(), "id not from key");
    }

    #[test]
    fn the_file_and_ipc_round_trip() {
        let mut f = NetFile::default();
        f.trust(peer()).unwrap();
        let text = toml::to_string_pretty(&f).unwrap();
        assert_eq!(toml::from_str::<NetFile>(&text).unwrap(), f);
        let old: NetFile = toml::from_str("version = 1\n").unwrap();
        assert_eq!(old, NetFile::default());

        let patch = PatchNetParams {
            ops: vec![NetOp::Respond {
                id: "0011223344556677".into(),
                accept: true,
            }],
        };
        let json = serde_json::to_string(&patch).unwrap();
        assert!(json.contains(r#""op":"respond""#), "{json}");
        assert_eq!(
            serde_json::from_str::<PatchNetParams>(&json).unwrap(),
            patch
        );
        let state = NetState {
            enabled: true,
            name: "Laptop".into(),
            id: "aabbccddeeff0011".into(),
            port: DEFAULT_PORT,
            problem: None,
            peers: vec![PeerState {
                id: peer_id(KEY),
                name: "Desktop".into(),
                jitter_ms: 12,
                connected: true,
                address: Some("192.168.1.20:47821".into()),
                link: Some(LinkStats::default()),
            }],
            nearby: vec![],
            requests: vec![PairRequest {
                id: "1122334455667788".into(),
                name: "Tower".into(),
                address: "192.168.1.30:47821".into(),
                code: "123 456".into(),
                incoming: true,
            }],
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<NetState>(&json).unwrap(), state);
    }
}
