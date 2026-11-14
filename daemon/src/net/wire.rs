//! What goes over the LAN: the Noise XX handshake and control frames (TCP),
//! audio packets (UDP) and discovery beacons (UDP broadcast). Plain
//! functions over byte buffers, so they are tested without the service.

use anyhow::{bail, Context, Result};
use hako_audio::net::{NET_CHANNELS, NET_MAX_FRAMES};
use hako_core::net::{peer_id, validate_key, MAX_NAME};
use serde_json::{json, Value};
use snow::params::HashChoice;
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::{HandshakeState, StatelessTransportState};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const NOISE: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// The network protocol: beacons carry it and both handshake payloads
/// start with it. 2: commit / reveal pairing codes.
pub const PROTOCOL: u8 = 2;
/// A pairing nonce (and its commitment).
const NONCE: usize = 32;
/// Handshake message 1 on the wire: length + the initiator's ephemeral
/// key, no payload.
pub const FIRST_MESSAGE: usize = 2 + 32;
/// A Poly1305 tag.
const TAG: usize = 16;
/// First byte of an audio packet.
const AUDIO: u8 = 1;
/// Set in audio nonces, clear in control ones: one key seals both, so a
/// control frame can never be replayed as audio (or back).
const AUDIO_NONCE: u64 = 1 << 63;
/// Samples in the largest audio block.
pub const MAX_SAMPLES: usize = NET_MAX_FRAMES * NET_CHANNELS;
/// Plaintext of the largest audio block: frame position + samples.
pub const MAX_AUDIO_PLAIN: usize = 8 + MAX_SAMPLES * 2;
/// The largest audio packet: kind, nonce, ciphertext.
pub const MAX_AUDIO_PACKET: usize = 1 + 8 + MAX_AUDIO_PLAIN + TAG;
/// A nearby machine is forgotten when its beacons stop for this long.
pub const BEACON_TTL: Duration = Duration::from_secs(6);

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex (as `hex` writes it) back to bytes.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2)
        || !text.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// This machine's static key pair.
pub struct Identity {
    pub private: Vec<u8>,
    /// Hex, as peers pin it.
    pub public: String,
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let pair = snow::Builder::new(NOISE.parse()?).generate_keypair()?;
        Ok(Self {
            private: pair.private,
            public: hex(&pair.public),
        })
    }

    /// The key file: private key hex, then public key hex, one per line.
    /// A public key that is not the private key's is refused.
    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines().map(str::trim);
        let (Some(private), Some(public)) = (lines.next(), lines.next()) else {
            bail!("expected two lines (private key, public key)");
        };
        validate_key(private).map_err(anyhow::Error::msg)?;
        validate_key(public).map_err(anyhow::Error::msg)?;
        let private = unhex(private).context("private key")?;
        let params: snow::params::NoiseParams = NOISE.parse()?;
        let mut dh = DefaultResolver
            .resolve_dh(&params.dh)
            .context("no X25519")?;
        dh.set(&private);
        if hex(dh.pubkey()) != public {
            bail!("the public key is not the private key's");
        }
        Ok(Self {
            private,
            public: public.to_string(),
        })
    }

    pub fn text(&self) -> String {
        format!("{}\n{}\n", hex(&self.private), self.public)
    }
}

/// BLAKE2s of `parts` in order.
fn blake2s(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = DefaultResolver
        .resolve_hash(&HashChoice::Blake2s)
        .expect("BLAKE2s is built in");
    for part in parts {
        hash.input(part);
    }
    let mut out = [0u8; 32];
    hash.result(&mut out);
    out
}

/// What both screens' code comes from: the handshake hash and both
/// pairing nonces. The responder committed to its nonce before seeing the
/// initiator's, and revealed it only after, so neither side (nor a man in
/// the middle, who is one side of each handshake) can steer the code.
pub fn code_digest(hash: &[u8], initiator: &[u8], responder: &[u8]) -> [u8; 32] {
    blake2s(&[hash, initiator, responder])
}

/// Six digits from `code_digest`, as both screens show them.
pub fn pairing_code(digest: &[u8]) -> String {
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// What a finished handshake gives a session.
pub struct Handshake {
    pub cipher: Cipher,
    /// The peer's static key (hex).
    pub key: String,
    pub code: String,
    /// The newest control nonce read so far (the responder's reveal).
    pub last: u64,
}

/// Noise XX over `stream`: `u16 BE length` + message each way. Payloads:
/// none in message 1; `PROTOCOL` + the responder's commitment (BLAKE2s of
/// its nonce) in 2; `PROTOCOL` + the initiator's nonce in 3. Then the
/// responder reveals its nonce (the first control frame, raw) and the
/// initiator checks it against the commitment.
pub fn handshake(
    stream: &mut (impl Read + Write),
    private: &[u8],
    initiator: bool,
) -> Result<Handshake> {
    let builder = snow::Builder::new(NOISE.parse()?).local_private_key(private)?;
    let mut ours = [0u8; NONCE];
    DefaultResolver
        .resolve_rng()
        .context("no random number generator")?
        .try_fill_bytes(&mut ours)?;
    let versioned = |nonce: &[u8]| [&[PROTOCOL][..], nonce].concat();
    // The initiator gets the commitment, the responder the nonce.
    let (hs, theirs) = if initiator {
        let mut hs = builder.build_initiator()?;
        send_message(stream, &mut hs, &[])?;
        let commitment = protocol_payload(&recv_message(stream, &mut hs)?)?;
        send_message(stream, &mut hs, &versioned(&ours))?;
        (hs, commitment)
    } else {
        let mut hs = builder.build_responder()?;
        if !recv_message(stream, &mut hs)?.is_empty() {
            bail!("unexpected handshake payload");
        }
        send_message(stream, &mut hs, &versioned(&blake2s(&[&ours])))?;
        let nonce = protocol_payload(&recv_message(stream, &mut hs)?)?;
        (hs, nonce)
    };
    let key = hex(hs.get_remote_static().context("no remote key")?);
    let hash = hs.get_handshake_hash().to_vec();
    let cipher = Cipher::new(hs.into_stateless_transport_mode()?);
    let mut last = 0;
    let digest = if initiator {
        let revealed = cipher
            .open(&read_frame(stream)?, &mut last)
            .context("unreadable pairing nonce")?;
        if blake2s(&[&revealed]) != theirs {
            bail!("the pairing nonce does not match its commitment");
        }
        code_digest(&hash, &ours, &revealed)
    } else {
        stream.write_all(&cipher.seal(&ours)?)?;
        code_digest(&hash, &theirs, &ours)
    };
    Ok(Handshake {
        cipher,
        key,
        code: pairing_code(&digest),
        last,
    })
}

fn send_message(stream: &mut impl Write, hs: &mut HandshakeState, payload: &[u8]) -> Result<()> {
    let mut msg = vec![0u8; u16::MAX as usize];
    let n = hs.write_message(payload, &mut msg)?;
    stream.write_all(&(n as u16).to_be_bytes())?;
    stream.write_all(&msg[..n])?;
    Ok(())
}

/// One handshake message; its payload.
fn recv_message(stream: &mut impl Read, hs: &mut HandshakeState) -> Result<Vec<u8>> {
    let msg = read_frame(stream)?;
    let mut payload = vec![0u8; msg.len()];
    let n = hs.read_message(&msg, &mut payload)?;
    payload.truncate(n);
    Ok(payload)
}

/// A message 2 / 3 payload (`PROTOCOL` + nonce or commitment): the nonce
/// or commitment. Another protocol is refused by name, so mismatched
/// versions fail clearly instead of pairing with a wrong code.
fn protocol_payload(payload: &[u8]) -> Result<[u8; NONCE]> {
    match payload {
        [] => bail!("the other computer runs an older hako: update both computers"),
        [version, ..] if *version != PROTOCOL => bail!(
            "the other computer speaks hako network protocol {version}, this one {PROTOCOL}: update both computers"
        ),
        [_, nonce @ ..] => nonce
            .try_into()
            .ok()
            .context("unexpected handshake payload"),
    }
}

/// A control message (TCP, after the handshake).
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    /// First frame each way. `port`: the sender's control / audio port;
    /// `pair`: the user asked to pair (a reconnect is refused quietly by a
    /// machine that does not know the key).
    Hello {
        name: String,
        version: String,
        port: u16,
        pair: bool,
    },
    Accepted,
    Denied,
    /// Too many pairing requests wait: try again later.
    Busy,
    /// `t`: sender's clock (µs), echoed by `Pong` for the round trip.
    Ping {
        t: u64,
    },
    Pong {
        t: u64,
    },
    Bye,
}

impl Control {
    fn encode(&self) -> Vec<u8> {
        let v = match self {
            Self::Hello {
                name,
                version,
                port,
                pair,
            } => {
                json!({"type": "hello", "name": name, "version": version, "port": port, "pair": pair})
            }
            Self::Accepted => json!({"type": "accepted"}),
            Self::Denied => json!({"type": "denied"}),
            Self::Busy => json!({"type": "busy"}),
            Self::Ping { t } => json!({"type": "ping", "t": t}),
            Self::Pong { t } => json!({"type": "pong", "t": t}),
            Self::Bye => json!({"type": "bye"}),
        };
        v.to_string().into_bytes()
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        let t = || v["t"].as_u64();
        Some(match v["type"].as_str()? {
            "hello" => Self::Hello {
                name: v["name"].as_str()?.into(),
                version: v["version"].as_str()?.into(),
                port: u16::try_from(v["port"].as_u64()?)
                    .ok()
                    .filter(|&p| p != 0)?,
                pair: v["pair"].as_bool()?,
            },
            "accepted" => Self::Accepted,
            "denied" => Self::Denied,
            "busy" => Self::Busy,
            "ping" => Self::Ping { t: t()? },
            "pong" => Self::Pong { t: t()? },
            "bye" => Self::Bye,
            _ => return None,
        })
    }
}

/// A session's transport keys and its one send-nonce counter (control and
/// audio). `Send + Sync`, encrypts through `&self`: the audio send path
/// uses it from PipeWire's data thread without a lock.
pub struct Cipher {
    noise: StatelessTransportState,
    next: AtomicU64,
    /// Newest audio nonce accepted (only the UDP thread writes it).
    audio_seen: AtomicU64,
}

impl Cipher {
    pub fn new(noise: StatelessTransportState) -> Self {
        Self {
            noise,
            next: AtomicU64::new(1),
            audio_seen: AtomicU64::new(0),
        }
    }

    fn nonce(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// A whole control frame: `u16 BE length` + `u64 BE nonce` + ciphertext.
    /// Write frames in the order they are sealed (nonces must increase).
    pub fn seal_control(&self, msg: &Control) -> Result<Vec<u8>> {
        self.seal(&msg.encode())
    }

    /// A control frame carrying `body` as it is.
    fn seal(&self, body: &[u8]) -> Result<Vec<u8>> {
        let mut frame = vec![0u8; 2 + 8 + body.len() + TAG];
        let nonce = self.nonce();
        frame[2..10].copy_from_slice(&nonce.to_be_bytes());
        let n = self.noise.write_message(nonce, body, &mut frame[10..])?;
        frame[..2].copy_from_slice(&u16::try_from(8 + n)?.to_be_bytes());
        Ok(frame)
    }

    /// A frame without its length; `last`: the newest nonce read so far.
    pub fn open_control(&self, frame: &[u8], last: &mut u64) -> Option<Control> {
        Control::parse(&self.open(frame, last)?)
    }

    /// `seal`'s body back.
    fn open(&self, frame: &[u8], last: &mut u64) -> Option<Vec<u8>> {
        if frame.len() < 8 + TAG {
            return None;
        }
        let nonce = u64::from_be_bytes(frame[..8].try_into().ok()?);
        if nonce <= *last || nonce & AUDIO_NONCE != 0 {
            return None;
        }
        let mut plain = vec![0u8; frame.len()];
        let n = self
            .noise
            .read_message(nonce, &frame[8..], &mut plain)
            .ok()?;
        *last = nonce;
        plain.truncate(n);
        Some(plain)
    }

    /// Seal one audio block into `packet` (`plain`: scratch of
    /// `MAX_AUDIO_PLAIN`); the packet's length, 0 if the block is too long.
    /// No allocation: runs on PipeWire's data thread.
    pub fn seal_audio(
        &self,
        pos: u64,
        samples: &[i16],
        plain: &mut [u8],
        packet: &mut [u8],
    ) -> usize {
        if samples.len() > MAX_SAMPLES {
            return 0;
        }
        let len = 8 + samples.len() * 2;
        plain[..8].copy_from_slice(&pos.to_be_bytes());
        for (out, s) in plain[8..len].as_chunks_mut::<2>().0.iter_mut().zip(samples) {
            *out = s.to_le_bytes();
        }
        let nonce = self.nonce() | AUDIO_NONCE;
        packet[0] = AUDIO;
        packet[1..9].copy_from_slice(&nonce.to_be_bytes());
        match self
            .noise
            .write_message(nonce, &plain[..len], &mut packet[9..])
        {
            Ok(n) => 9 + n,
            Err(_) => 0,
        }
    }

    /// Open an audio packet into `samples`: its frame position and sample
    /// count. Tampered, replayed or older packets give `None`.
    pub fn open_audio(
        &self,
        packet: &[u8],
        plain: &mut [u8],
        samples: &mut [i16],
    ) -> Option<(u64, usize)> {
        if packet.len() < 9 + 8 + TAG || packet.len() > MAX_AUDIO_PACKET || packet[0] != AUDIO {
            return None;
        }
        let nonce = u64::from_be_bytes(packet[1..9].try_into().ok()?);
        // ponytail: newer-only drops reordered packets; a replay window if
        // Wi-Fi reorders.
        if nonce & AUDIO_NONCE == 0 || nonce <= self.audio_seen.load(Ordering::Relaxed) {
            return None;
        }
        let n = self.noise.read_message(nonce, &packet[9..], plain).ok()?;
        self.audio_seen.store(nonce, Ordering::Relaxed);
        let body = plain.get(8..n)?;
        let pos = u64::from_be_bytes(plain[..8].try_into().ok()?);
        let count = body.len() / 2;
        for (s, b) in samples.iter_mut().zip(body.as_chunks::<2>().0) {
            *s = i16::from_le_bytes(*b);
        }
        Some((pos, count))
    }
}

/// Read one control frame (`u16 BE length` + frame).
pub fn read_control(stream: &mut impl Read, cipher: &Cipher, last: &mut u64) -> Result<Control> {
    cipher
        .open_control(&read_frame(stream)?, last)
        .context("unreadable control frame")
}

/// `u16 BE length` + that many bytes: the bytes.
fn read_frame(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let mut frame = vec![0u8; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}

/// A name from the network: no control or invisible format characters
/// (bidi overrides, zero-width, tags), trimmed, at most `MAX_NAME`
/// characters, `fallback` when nothing is left.
pub fn clean_name(name: &str, fallback: &str) -> String {
    let shown: String = name
        .chars()
        .filter(|&c| !c.is_control() && !is_format(c))
        .collect();
    let name: String = shown.trim().chars().take(MAX_NAME).collect();
    match name.trim_end() {
        "" => fallback.to_string(),
        name => name.to_string(),
    }
}

/// Invisible formatting characters (Unicode `Cf`) a name could hide behind.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// A discovery beacon: who is here and where its control port is.
#[derive(Debug, Clone, PartialEq)]
pub struct Beacon {
    pub id: String,
    pub name: String,
    pub key: String,
    pub port: u16,
}

impl Beacon {
    pub fn encode(&self) -> Vec<u8> {
        json!({"hako": PROTOCOL, "id": self.id, "name": self.name, "key": self.key, "port": self.port})
            .to_string()
            .into_bytes()
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        if v["hako"].as_u64()? != u64::from(PROTOCOL) {
            return None;
        }
        let key = v["key"].as_str()?;
        validate_key(key).ok()?;
        let id = peer_id(key);
        if v["id"].as_str()? != id {
            return None;
        }
        let port = u16::try_from(v["port"].as_u64()?)
            .ok()
            .filter(|&p| p != 0)?;
        Some(Self {
            name: clean_name(v["name"].as_str()?, &id),
            id,
            key: key.into(),
            port,
        })
    }
}

/// A beacon heard: from where (its control address) and when.
pub struct Seen {
    pub beacon: Beacon,
    pub addr: SocketAddr,
    pub at: Instant,
}

/// Drop machines whose beacons stopped (`BEACON_TTL`).
pub fn expire(nearby: &mut HashMap<String, Seen>, now: Instant) {
    nearby.retain(|_, s| now.duration_since(s.at) < BEACON_TTL);
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// Both ends of a finished handshake over loopback.
    pub(in crate::net) fn pair() -> (Handshake, Handshake, Identity, Identity) {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let b_private = b.private.clone();
        let responder = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            handshake(&mut stream, &b_private, false).unwrap()
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        let ha = handshake(&mut stream, &a.private, true).unwrap();
        (ha, responder.join().unwrap(), a, b)
    }

    #[test]
    fn identity_file_round_trips() {
        let id = Identity::generate().unwrap();
        let back = Identity::parse(&id.text()).unwrap();
        assert_eq!(
            (back.private, back.public),
            (id.private.clone(), id.public.clone())
        );
        assert!(Identity::parse("zz\n").is_err());
        let other = Identity::generate().unwrap();
        let mismatched = format!("{}\n{}\n", hex(&id.private), other.public);
        assert!(
            Identity::parse(&mismatched).is_err(),
            "public key of another"
        );
        assert_eq!(unhex(&hex(&[0, 0xab, 0xff])), Some(vec![0, 0xab, 0xff]));
        assert_eq!(unhex("+f"), None);
    }

    #[test]
    fn handshake_authenticates_both_keys_and_gives_one_code() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<StatelessTransportState>();
        let (a, b, ia, ib) = pair();
        assert_eq!(a.key, ib.public);
        assert_eq!(b.key, ia.public);
        assert_eq!(a.code, b.code);
        assert_eq!(a.code.len(), 7);
        assert_eq!(pairing_code(&[0, 0x0f, 0x42, 0x40]), "000 000");
        assert_eq!(pairing_code(&[0, 0x01, 0xe2, 0x40]), "123 456");

        // Control: in order once, never replayed.
        let frame = a.cipher.seal_control(&Control::Ping { t: 7 }).unwrap();
        let mut last = 0;
        assert_eq!(
            b.cipher.open_control(&frame[2..], &mut last),
            Some(Control::Ping { t: 7 })
        );
        assert_eq!(b.cipher.open_control(&frame[2..], &mut last), None);
        let hello = Control::Hello {
            name: "Desk".into(),
            version: "1".into(),
            port: 47821,
            pair: true,
        };
        let frame = a.cipher.seal_control(&hello).unwrap();
        assert_eq!(
            read_control(&mut &frame[..], &b.cipher, &mut last).unwrap(),
            hello
        );
        // A Hello naming port 0 (no audio port) is unreadable.
        let no_port = Control::Hello {
            name: "Desk".into(),
            version: "1".into(),
            port: 0,
            pair: true,
        };
        let frame = a.cipher.seal_control(&no_port).unwrap();
        assert!(read_control(&mut &frame[..], &b.cipher, &mut last).is_err());
    }

    #[test]
    fn the_code_depends_on_both_nonces() {
        let hash = [7u8; 32];
        let (a, b) = ([1u8; NONCE], [2u8; NONCE]);
        let code = code_digest(&hash, &a, &b);
        assert_ne!(code, code_digest(&hash, &[3u8; NONCE], &b), "initiator's");
        assert_ne!(code, code_digest(&hash, &a, &[3u8; NONCE]), "responder's");
        assert_ne!(code, code_digest(&[8u8; 32], &a, &b), "handshake's");
    }

    /// `handshake` as initiator against a hand-driven responder that sends
    /// `payload` in message 2 and then reveals `reveal`.
    fn against_responder(payload: Vec<u8>, reveal: [u8; NONCE]) -> Result<Handshake> {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let responder = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut hs = snow::Builder::new(NOISE.parse().unwrap())
                .local_private_key(&b.private)
                .unwrap()
                .build_responder()
                .unwrap();
            let steps = recv_message(&mut stream, &mut hs)
                .and_then(|_| send_message(&mut stream, &mut hs, &payload))
                .and_then(|()| recv_message(&mut stream, &mut hs));
            if steps.is_ok() {
                let cipher = Cipher::new(hs.into_stateless_transport_mode().unwrap());
                let _ = stream.write_all(&cipher.seal(&reveal).unwrap());
            }
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        let got = handshake(&mut stream, &a.private, true);
        drop(stream);
        responder.join().unwrap();
        got
    }

    #[test]
    fn a_reveal_must_match_its_commitment() {
        let nonce = [5u8; NONCE];
        let commit = |n: &[u8]| [&[PROTOCOL][..], &blake2s(&[n])].concat();
        assert!(against_responder(commit(&nonce), nonce).is_ok());
        let e = against_responder(commit(&nonce), [6u8; NONCE])
            .err()
            .unwrap();
        assert!(format!("{e:#}").contains("commitment"), "{e:#}");
    }

    #[test]
    fn other_protocols_and_unexpected_payloads_are_refused() {
        let nonce = [5u8; NONCE];
        let older = against_responder(Vec::new(), nonce).err().unwrap();
        assert!(format!("{older:#}").contains("older hako"), "{older:#}");
        let mut newer = vec![PROTOCOL + 1];
        newer.extend_from_slice(&blake2s(&[&nonce]));
        let newer = against_responder(newer, nonce).err().unwrap();
        assert!(format!("{newer:#}").contains("protocol"), "{newer:#}");
        let short = against_responder(vec![PROTOCOL, 1, 2], nonce)
            .err()
            .unwrap();
        assert!(format!("{short:#}").contains("payload"), "{short:#}");

        // A responder refuses anything in message 1.
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let responder = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            handshake(&mut stream, &b.private, false)
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        let mut hs = snow::Builder::new(NOISE.parse().unwrap())
            .local_private_key(&a.private)
            .unwrap()
            .build_initiator()
            .unwrap();
        send_message(&mut stream, &mut hs, b"grind me").unwrap();
        let e = responder.join().unwrap().err().unwrap();
        assert!(format!("{e:#}").contains("payload"), "{e:#}");
    }

    #[test]
    fn audio_packets_round_trip_and_reject_tampering_and_replays() {
        let (a, b, _, _) = pair();
        let mut plain = [0u8; MAX_AUDIO_PLAIN];
        let mut packet = [0u8; MAX_AUDIO_PACKET];
        let mut samples = [0i16; MAX_SAMPLES];
        let block: Vec<i16> = (0..480).map(|i| i as i16 * 37 - 9000).collect();
        let n = a.cipher.seal_audio(4800, &block, &mut plain, &mut packet);
        assert!(n > 0 && n <= MAX_AUDIO_PACKET && n < 1100, "{n} bytes");
        let mut tampered = packet;
        tampered[20] ^= 1;
        assert_eq!(
            b.cipher
                .open_audio(&tampered[..n], &mut plain, &mut samples),
            None
        );
        assert_eq!(
            b.cipher.open_audio(&packet[..n], &mut plain, &mut samples),
            Some((4800, 480))
        );
        assert_eq!(&samples[..480], &block[..]);
        assert_eq!(
            b.cipher.open_audio(&packet[..n], &mut plain, &mut samples),
            None,
            "replayed"
        );
        let too_long = vec![0i16; MAX_SAMPLES + 2];
        assert_eq!(
            a.cipher.seal_audio(0, &too_long, &mut plain, &mut packet),
            0
        );

        // Same key both ways: a control frame replayed as audio (a spoofed
        // UDP packet) and an audio packet as control are both refused.
        let frame = a.cipher.seal_control(&Control::Ping { t: 1 }).unwrap();
        let mut as_audio = vec![AUDIO];
        as_audio.extend_from_slice(&frame[2..]);
        assert_eq!(
            b.cipher.open_audio(&as_audio, &mut plain, &mut samples),
            None
        );
        let n = a.cipher.seal_audio(0, &block, &mut plain, &mut packet);
        assert_eq!(b.cipher.open_control(&packet[1..n], &mut 0), None);
    }

    #[test]
    fn beacons_parse_and_expire() {
        let key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let beacon = Beacon {
            id: peer_id(key),
            name: "Desk".into(),
            key: key.into(),
            port: 47821,
        };
        assert_eq!(Beacon::parse(&beacon.encode()), Some(beacon.clone()));
        let other = |v: Value| Beacon::parse(v.to_string().as_bytes());
        let mut v: Value = serde_json::from_slice(&beacon.encode()).unwrap();
        v["hako"] = json!(1);
        assert_eq!(other(v.clone()), None, "older protocol");
        v["hako"] = json!(PROTOCOL);
        v["id"] = json!("ffffffffffffffff");
        assert_eq!(other(v.clone()), None, "id not from key");
        v["id"] = json!(beacon.id);
        v["name"] = json!("\u{7}");
        assert_eq!(other(v).unwrap().name, beacon.id, "no printable name");
        assert_eq!(Beacon::parse(b"hello"), None);

        let now = Instant::now();
        let seen = |age: u64| Seen {
            beacon: beacon.clone(),
            addr: "10.0.0.2:47821".parse().unwrap(),
            at: now - Duration::from_secs(age),
        };
        let mut nearby =
            HashMap::from([("fresh".to_string(), seen(1)), ("old".to_string(), seen(7))]);
        expire(&mut nearby, now);
        assert_eq!(nearby.keys().collect::<Vec<_>>(), ["fresh"]);
    }

    #[test]
    fn names_lose_invisible_characters_and_leading_spaces_cost_nothing() {
        let padded = format!("{}Workstation", " ".repeat(30));
        assert_eq!(clean_name(&padded, "x"), "Workstation");
        assert_eq!(
            clean_name("\u{202E}moc.\u{200B}ab\u{E0041}\u{FEFF}d\u{2066}", "x"),
            "moc.abd"
        );
        assert_eq!(clean_name("\u{200B} \u{00AD}", "x"), "x");
        assert_eq!(
            clean_name("\u{0600}De\u{110BD}s\u{1BCA0}k\u{1D173}\u{13430}", "x"),
            "Desk"
        );
        let long = format!(" {}", "n".repeat(40));
        assert_eq!(clean_name(&long, "x"), "n".repeat(MAX_NAME));
    }
}
