//! Cross-implementation test vectors: the Rust engine against the Go one, byte for byte.
//!
//! `vectors.json` was written by the Go implementation, the reference, in its
//! core/vectors_test.go (removed after 382cb8d: `git show 382cb8d:core/vectors_test.go`), and is
//! fixed since: Go's bytes as they were. Through the public API only, these tests check
//! that [`Msg`] writes and reads control lines as Go does, and that a [`Session`] receives Go's
//! packets as a Go session does and seals as a Go session seals (notes/rust.md,
//! "Cross-implementation test vectors"). Probes are not pinned: the Rust port adds a challenge
//! token to them (notes/open.md R2-03).

use std::net::SocketAddr;

use serde::Deserialize;
use wirebond_core::*;

#[derive(Deserialize)]
struct Vectors {
    constants: Constants,
    messages: Vec<Message>,
    escaped: Vec<Escaped>,
    ratchet: Ratchet,
    receive: Script,
    seal: Seal,
}

#[derive(Deserialize)]
struct Constants {
    header_len: usize,
    tag_len: usize,
    data_len: usize,
    overhead: usize,
    type_data: u8,
    type_probe: u8,
    small_bit: u8,
    epoch_bits: u32,
    epoch_label: String,
    replay_window: u64,
    protocol: u32,
    port: u16,
    max_msg: usize,
    max_packet: usize,
    max_datagram: usize,
}

/// A control message as Go's node writes it: json.Marshal, then '\n' on the wire.
#[derive(Deserialize)]
struct Message {
    name: String,
    line: String,
    /// A key message's key, hex.
    k: Option<String>,
}

/// A message with characters Go escapes and serde_json does not; `plain` is it unescaped.
#[derive(Deserialize)]
struct Escaped {
    name: String,
    line: String,
    plain: String,
}

#[derive(Deserialize)]
struct Ratchet {
    /// K_0 … K_3, hex.
    keys: Vec<String>,
}

#[derive(Deserialize)]
struct Script {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Seal {
    /// K_0, hex: the client's send key, the only bytes its rand yields.
    key: String,
    steps: Vec<Step>,
    /// The counter Go's client sealed the datagram under.
    counter: u64,
    /// What a Go client sends for the datagram under each counter from 0, hex.
    packets: Vec<String>,
}

/// One event of a script and what the Go session did with it.
#[derive(Deserialize)]
struct Step {
    name: String,
    at: Mono,
    links: Option<Links>,
    control: Option<String>,
    packet: Option<Packet>,
    app: Option<App>,
    /// The key message the session answered with.
    key: Option<Key>,
    #[serde(default)]
    deliver: Vec<Delivery>,
    /// A packet that got no action of any kind.
    #[serde(default)]
    silent: bool,
}

#[derive(Deserialize)]
struct Links {
    links: Vec<LinkV>,
    mesh: MeshV,
}

#[derive(Deserialize)]
struct LinkV {
    id: u8,
    label: String,
    #[serde(default)]
    metered: bool,
    #[serde(default)]
    default: bool,
    sock: SockId,
    local: SocketAddr,
    mapped: Option<SocketAddr>,
}

#[derive(Deserialize)]
struct MeshV {
    sock: SockId,
    local: SocketAddr,
    peer: SocketAddr,
}

#[derive(Deserialize)]
struct Packet {
    sock: SockId,
    from: SocketAddr,
    data: String,
    /// What packet_rid reads.
    rid: Option<u16>,
    /// For a packet Go sealed: its counter and the sealed payload, hex.
    counter: Option<u64>,
    plain: Option<String>,
}

#[derive(Deserialize)]
struct App {
    flow: u32,
    data: String,
}

#[derive(Deserialize)]
struct Key {
    rid: u16,
    e: u64,
}

#[derive(Deserialize, Debug, PartialEq)]
struct Delivery {
    flow: u32,
    data: String,
}

fn vectors() -> Vectors {
    serde_json::from_str(include_str!("vectors.json")).expect("vectors.json")
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex: {s}");
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A test flow's inner packet: `data` as IPv4/UDP from 10.77.0.2 port `f.0` to 10.77.0.1 port 9
/// (tests/common's `inner`; this file stands alone).
fn inner(f: FlowId, data: &[u8]) -> Vec<u8> {
    let total = (28 + data.len()) as u16;
    let mut p = vec![0x45, 0];
    p.extend_from_slice(&total.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0, 10, 77, 0, 2, 10, 77, 0, 1]);
    p.extend_from_slice(&f.0.to_be_bytes());
    p.extend_from_slice(&9u16.to_be_bytes());
    p.extend_from_slice(&(total - 20).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(data);
    p
}

/// The test flow (UDP source port) and datagram of a packet [`inner`] built; anything else is
/// flow 0, whole.
fn outer(p: &[u8]) -> (FlowId, &[u8]) {
    match flow::Key::parse(p) {
        Some(k) if k.proto == 17 && p.len() >= 28 && p[0] == 0x45 => (FlowId(k.sport), &p[28..]),
        _ => (FlowId(0), p),
    }
}

// ---------------------------------------------------------------------------------------------
// The wire's constants and layout, and Msg: these hold on the contract alone.

#[test]
#[ignore = "2.0 wave 2: Go-compatible 1.x vectors (8-byte data header, 1.x JSON); regenerate for the 2.0 payload (SPEC 10.3)"]
fn constants() {
    let c = vectors().constants;
    assert_eq!(c.header_len, HEADER_LEN);
    assert_eq!(c.tag_len, TAG_LEN);
    assert_eq!(c.data_len, DATA_LEN);
    assert_eq!(c.overhead, OVERHEAD);
    assert_eq!(c.type_data, TYPE_DATA);
    assert_eq!(c.type_probe, TYPE_PROBE);
    assert_eq!(c.small_bit, SMALL_BIT);
    assert_eq!(c.epoch_bits, EPOCH_BITS);
    assert_eq!(c.epoch_label.as_bytes(), EPOCH_LABEL);
    assert_eq!(c.replay_window, REPLAY_WINDOW);
    assert_eq!(c.protocol, PROTOCOL);
    assert_eq!(c.port, PORT);
    assert_eq!(c.max_msg, MAX_MSG);
    assert_eq!(c.max_packet, MAX_PACKET);
    assert_eq!(c.max_datagram, MAX_INNER);
}

/// Every line Go's node writes reads as a Msg that writes the same bytes, and a key message's
/// base64 is the key's bytes.
#[test]
#[ignore = "2.0 wave 2: Go-compatible 1.x vectors (8-byte data header, 1.x JSON); regenerate for the 2.0 payload (SPEC 10.3)"]
fn messages() {
    let v = vectors();
    for m in &v.messages {
        let msg: Msg = serde_json::from_str(&m.line)
            .unwrap_or_else(|e| panic!("{}: Msg does not read Go's line: {e}\n {}", m.name, m.line));
        let ours = serde_json::to_string(&msg).expect("Msg serializes");
        assert!(ours == m.line, "{}: Msg writes Go's message differently\n   go: {}\n rust: {ours}", m.name, m.line);
        if let Some(k) = &m.k {
            assert_eq!(hex(&msg.k), *k, "{}: the key's bytes", m.name);
        }
    }
    // A sender entering epochs 1–3 sends the ratchet's K_1 … K_3.
    let epochs = v.messages.iter().filter(|m| m.name.starts_with("key, entering epoch"));
    assert!(epochs.map(|m| m.k.as_ref()).eq(v.ratchet.keys[1..].iter().map(Some)));
}

/// Go escapes <, >, & and U+2028/U+2029 in strings; serde_json writes them as they are. Each
/// reads the other's line as the same message.
#[test]
#[ignore = "2.0 wave 2: Go-compatible 1.x vectors (8-byte data header, 1.x JSON); regenerate for the 2.0 payload (SPEC 10.3)"]
fn escaped() {
    for e in &vectors().escaped {
        let read = |line: &str| -> Msg { serde_json::from_str(line).unwrap_or_else(|err| panic!("{}: {err}", e.name)) };
        let go = read(&e.line);
        assert_eq!(go, read(&e.plain), "{}: Go's escapes read as another message", e.name);
        let ours = serde_json::to_string(&go).expect("Msg serializes");
        assert_eq!(ours, e.plain, "{}", e.name);
    }
}

/// packet_rid reads Go's packets as Go's PacketRID does, and each packet Go sealed has the layout
/// of §7.3 and §7.4: type, rid, counter, the sealed payload, the tag.
#[test]
#[ignore = "2.0 wave 2: Go-compatible 1.x vectors (8-byte data header, 1.x JSON); regenerate for the 2.0 payload (SPEC 10.3)"]
fn packet_layout() {
    let v = vectors();
    for st in &v.receive.steps {
        let Some(p) = &st.packet else { continue };
        let data = unhex(&p.data);
        assert_eq!(packet_rid(&data), p.rid, "{}: packet_rid", st.name);
        if let (Some(counter), Some(plain)) = (p.counter, &p.plain) {
            let len = HEADER_LEN + plain.len() / 2 + TAG_LEN;
            assert_eq!(data.len(), len, "{}", st.name);
            let ctr = u64::from_be_bytes(data[3..HEADER_LEN].try_into().expect("8 bytes"));
            assert_eq!(ctr, counter, "{}", st.name);
        }
        for d in &st.deliver {
            let f = FlowId(d.flow as u16);
            assert_eq!(outer(&inner(f, &[])).0, f, "{}", st.name);
        }
    }
    let app = v.seal.steps.iter().find_map(|st| st.app.as_ref()).expect("an app step");
    for p in &v.seal.packets {
        let data = unhex(p);
        assert_eq!(data[0], TYPE_DATA | SMALL_BIT);
        assert_eq!(data.len(), OVERHEAD + app.data.len() / 2);
    }
}

// ---------------------------------------------------------------------------------------------
// A Session against Go's.

/// Records what a Session does in one call.
#[derive(Default)]
struct Rec {
    sends: Vec<Vec<u8>>,
    delivers: Vec<Delivery>,
    controls: Vec<Msg>,
}

impl Out for Rec {
    fn send(&mut self, _: SockId, _: SocketAddr, data: &[u8]) {
        self.sends.push(data.to_vec());
    }
    fn deliver(&mut self, packet: &[u8]) {
        let (flow, data) = outer(packet);
        let data = hex(data);
        self.delivers.push(Delivery { flow: u32::from(flow.0), data });
    }
    fn control(&mut self, msg: Msg) {
        self.controls.push(msg);
    }
}

impl Rec {
    fn keys(&self) -> Vec<&Msg> {
        self.controls.iter().filter(|m| m.t == "key").collect()
    }
}

/// A rand that yields `key` over and over, so a 32-byte key drawn from it is `key`.
fn cyclic(key: Vec<u8>) -> Rand {
    Box::new(move |b: &mut [u8]| {
        for (i, x) in b.iter_mut().enumerate() {
            *x = key[i % key.len()];
        }
    })
}

const OWNER: u64 = 7;

/// Drives a Session through a script as core/vectors_test.go drove Go's: before each step, a tick
/// whenever next() is due (at that time, never before the last event), then the step's event.
struct Driver {
    s: Session,
    rids: RidTable,
    last: Mono,
    script: &'static str,
}

impl Driver {
    fn new(role: Role, rand: Rand, script: &'static str) -> Driver {
        let s = Session::new(role, OWNER, rand);
        Driver { s, rids: RidTable::new(), last: 0, script }
    }

    /// Runs step i and checks what the session did against what Go's did.
    fn step(&mut self, i: usize, st: &Step) -> Rec {
        let script = self.script;
        let ctx = |what: String| format!("vectors.json, {script} step {i} ({}): {what}", st.name);
        for n in 0.. {
            let next = self.s.next();
            if next > st.at {
                break;
            }
            assert!(n < 100, "{}", ctx(format!("next() asks for a tick at {next} again and again")));
            self.last = self.last.max(next);
            let mut out = Rec::default();
            self.s.handle(self.last, Event::Tick, &mut self.rids, &mut out);
            let late = &out.delivers;
            assert!(late.is_empty(), "{}", ctx(format!("a tick before it delivered {late:?}")));
        }
        assert!(st.at >= self.last, "{}", ctx("time goes back".into()));
        self.last = st.at;

        let (links, msg, data);
        let ev = if let Some(l) = &st.links {
            let link = |l: &LinkV| Link {
                id: l.id,
                label: l.label.clone(),
                metered: l.metered,
                default: l.default,
                sock: l.sock,
                local: l.local,
                mapped: l.mapped,
            };
            links = l.links.iter().map(link).collect::<Vec<_>>();
            let (sock, local, peer) = (l.mesh.sock, l.mesh.local, l.mesh.peer);
            Event::Links { links: &links, mesh: Some(MeshLane { sock, local, peer }) }
        } else if let Some(line) = &st.control {
            msg = serde_json::from_str::<Msg>(line).expect("a control line");
            Event::Control(&msg)
        } else if let Some(p) = &st.packet {
            data = unhex(&p.data);
            Event::Packet { sock: p.sock, from: p.from, data: &data }
        } else if let Some(a) = &st.app {
            data = inner(FlowId(a.flow as u16), &unhex(&a.data));
            Event::Send { packet: &data }
        } else {
            panic!("{}", ctx("a step with no event".into()));
        };
        let mut out = Rec::default();
        self.s.handle(st.at, ev, &mut self.rids, &mut out);

        if let Some(k) = &st.key {
            let keys = out.keys();
            assert!(
                keys.len() == 1 && keys[0].e == k.e && keys[0].k.len() == 32,
                "{}",
                ctx(format!(
                    "Go answered with one key message for epoch {}, a 32-byte key (§7.1, §7.2); \
                     the session sent {keys:?}",
                    k.e
                ))
            );
        }
        assert!(
            out.delivers == st.deliver,
            "{}",
            ctx(format!("Go delivered {:?}; the session delivered {:?}", st.deliver, out.delivers))
        );
        if st.silent {
            assert!(
                out.sends.is_empty() && out.delivers.is_empty() && out.controls.is_empty(),
                "{}",
                ctx(format!(
                    "Go drops this packet without a trace (§7.5, §7.6 rule 1); the session sent {} \
                     packets and {} control messages",
                    out.sends.len(),
                    out.controls.len()
                ))
            );
        }
        out
    }
}

/// A fresh server receives a Go client's packets: small and bulk, on its IPv4 link and its
/// Tailscale lane, across three epoch boundaries and into the client's next session, with
/// tampered, replayed, forged and malformed packets in between. It delivers what Go's server
/// delivered and nothing else, and what Go dropped gets no action.
#[test]
#[ignore = "2.0 wave 2: Go-compatible 1.x vectors (8-byte data header, 1.x JSON); regenerate for the 2.0 payload (SPEC 10.3)"]
fn receive() {
    let v = vectors();
    let mut d = Driver::new(Role::Anchor, cyclic(vec![0xe0; 32]), "receive");
    for (i, st) in v.receive.steps.iter().enumerate() {
        let out = d.step(i, st);
        if let Some(k) = &st.key {
            // The client sealed its packets for the rid Go's fresh RidTable hands out first.
            let rid = out.keys()[0].rid;
            assert!(
                rid == k.rid,
                "vectors.json, receive step {i} ({}): the session announced rid {rid}, but the \
                 client's packets are sealed for rid {}, the first a fresh RidTable hands out in \
                 Go (core/keys.go alloc: 1, 2, 3 …)",
                st.name,
                k.rid
            );
            assert_eq!(d.rids.lookup(rid), Some(OWNER), "receive step {i}: the RidTable's owner");
        }
    }
}

/// A fresh client whose rand yields only K_0 takes the server's hello and key and sends one
/// datagram: the packet is byte for byte the one a Go client sends under the same counter.
#[test]
#[ignore = "2.0 wave 2: Go-compatible 1.x vectors (8-byte data header, 1.x JSON); regenerate for the 2.0 payload (SPEC 10.3)"]
fn seal() {
    let v = vectors();
    let k0 = unhex(&v.seal.key);
    let mut d = Driver::new(Role::Client, cyclic(k0.clone()), "seal");
    let mut sent = Vec::new();
    for (i, st) in v.seal.steps.iter().enumerate() {
        let out = d.step(i, st);
        if st.key.is_some() {
            let k = &out.keys()[0].k;
            assert!(
                *k == k0,
                "vectors.json, seal step {i} ({}): the session's send key is {}, not K_0 {}, \
                 though its rand yields only K_0: it takes its key otherwise than as one 32-byte \
                 draw, so its packets cannot match Go's",
                st.name,
                hex(k),
                v.seal.key
            );
        }
        if st.app.is_some() {
            sent = out.sends;
        }
    }
    let data: Vec<_> = sent.iter().filter(|b| b.len() > HEADER_LEN && b[0] & !SMALL_BIT == TYPE_DATA).collect();
    assert!(
        !data.is_empty(),
        "vectors.json, seal: the session sent no data packet for the datagram; Go sends a small \
         flow's datagram at once, on every validated lane while none is confirmed (§8.1, §10.1)"
    );
    assert!(data.iter().all(|b| *b == data[0]), "a packet is sealed once (§7.3), but the lanes got different bytes");
    let pkt = data[0];
    let counter = u64::from_be_bytes(pkt[3..HEADER_LEN].try_into().expect("8 bytes"));
    let Some(want) = v.seal.packets.get(counter as usize) else {
        panic!(
            "vectors.json, seal: the session sealed the datagram under counter {counter}; the \
             vectors hold Go's packet for counters 0–{} (Go's own client used {})",
            v.seal.packets.len() - 1,
            v.seal.counter
        );
    };
    let want = unhex(want);
    assert!(
        *pkt == want,
        "vectors.json, seal: under counter {counter} the session's packet differs from Go's\n  \
         header go {} rust {}\n  sealed go {} rust {}",
        hex(&want[..HEADER_LEN]),
        hex(&pkt[..HEADER_LEN]),
        hex(&want[HEADER_LEN..]),
        hex(&pkt[HEADER_LEN..])
    );
}
