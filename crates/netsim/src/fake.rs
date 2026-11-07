//! The test fake (Go netsim/fake_test.go): a stand-in Session that proves netsim's plumbing
//! without core. It frames each app datagram in core's header layout with a checksum for a tag,
//! sends bulk on one lane and small datagrams on every lane, delivers what passes its checksum and
//! replay set, and probes every 20 ms through next and Tick. In direct mode it also punches the
//! peer's candidates and uses a direct lane once a probe arrives on it. An [`Evil`] fake breaks
//! one invariant on purpose, so the self-tests show the checker catches it.
//!
//! It registers nothing in the device's RidTable; the stand-in routes to it without the rid check
//! (it is not a `wirebond_core::Session`). Owner: N3.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use crate::sim::{FlowId, inner, outer};

/// The fake's own data payload: flow (4) + seq (4), as 1.x's was.
const DATA_LEN: usize = 8;

use wirebond_core::{
    BOND_BYTES, Event, HEADER_LEN, LaneStatus, Link, MeshLane, Mono, Msg, Out, Rand, RidTable, Role, SMALL_BIT,
    SessionStatus, SockId, TAG_LEN, TYPE_DATA, TYPE_PROBE,
};

use crate::apps::{be32, be64};
use crate::sim::{Endpoint, MS, Maker};

/// Ways a fake can be evil (Go evil* constants).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Evil {
    #[default]
    None,
    /// Deliver every datagram twice.
    Twice,
    /// Restart the counter after 50 packets.
    Reuse,
    /// Flip a bit of every delivered datagram.
    Corrupt,
    /// Send data to every candidate.
    Unvalid,
    /// Report more bytes held than BOND_BYTES.
    Bytes,
    /// Move a lane's address on a Tick.
    Move,
    /// Answer every packet with a bigger probe, authentic or not.
    Answer,
    /// Hold every other bulk datagram and release it after the next.
    Disorder,
    /// Send the same key in every new session.
    ForgeKeys,
    /// Take any probe's source as a validated lane, as a relayed probe would want.
    Trust,
}

/// Where a probe went or came from: a socket and an address (Go dst).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Dst {
    pub(crate) sock: SockId,
    pub(crate) to: SocketAddr,
}

/// The fake endpoint (Go fake).
pub struct Fake {
    pub(crate) role: Role,
    pub(crate) rand: Rand,
    pub(crate) direct: bool,
    pub(crate) evil: Evil,
    pub(crate) links: Vec<Link>,
    pub(crate) mesh: Option<MeshLane>,
    pub(crate) mode: String,
    pub(crate) my_rid: u16,
    pub(crate) peer_rid: u16,
    /// Empty until the first hello.
    pub(crate) key: Vec<u8>,
    pub(crate) ctr: u64,
    /// Counters received (the replay set).
    pub(crate) seen: BTreeSet<u64>,
    pub(crate) seq: BTreeMap<FlowId, u32>,
    pub(crate) cands: Vec<SocketAddr>,
    pub(crate) lanes: Vec<LaneStatus>,
    pub(crate) next: Mono,
    /// Wake events seen (TestSleepWake reads it).
    pub wakes: u32,
    pub(crate) held: Option<Vec<u8>>,
    /// Where each of our probes went, by counter.
    pub(crate) probes: BTreeMap<u64, Dst>,
    /// Counter + 1 of the newest probe received on a socket from an address.
    pub(crate) last_rx: BTreeMap<Dst, u64>,
}

/// Makes fakes (Go fakeMaker).
pub fn fake_maker(direct: bool, evil: Evil) -> Maker {
    Box::new(move |role, _owner, rand| Box::new(Fake::new(role, rand, direct, evil)) as Box<dyn Endpoint>)
}

/// The fake's integrity check: a CRC, enough against random corruption and forgery (Go fakeTag).
pub fn fake_tag(b: &[u8]) -> u64 {
    crate::sim::crc(b) as u64
}

/// The big-endian u16 at the start of b.
fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// A probe's payload: the echo (8 bytes) and the sender's socket (2).
const PROBE_PAYLOAD: usize = 10;

/// App datagrams this long or longer are bulk; shorter ones are small and go on every lane.
const FAKE_BULK: usize = 400;

/// Sets a lane's role without allocating once it holds the name.
fn set_role(role: &mut String, name: &str) {
    if role != name {
        role.clear();
        role.push_str(name);
    }
}

impl Fake {
    pub fn new(role: Role, rand: Rand, direct: bool, evil: Evil) -> Fake {
        Fake {
            role,
            rand,
            direct,
            evil,
            links: Vec::new(),
            mesh: None,
            mode: String::new(),
            my_rid: 0,
            peer_rid: 0,
            key: Vec::new(),
            ctr: 0,
            seen: BTreeSet::new(),
            seq: BTreeMap::new(),
            cands: Vec::new(),
            lanes: Vec::new(),
            next: 0,
            wakes: 0,
            held: None,
            probes: BTreeMap::new(),
            last_rx: BTreeMap::new(),
        }
    }

    /// Frames payload as core would: type, the peer's rid, counter, payload, a 16-byte tag (the
    /// CRC tag and 8 zero bytes) (Go seal).
    pub(crate) fn seal(&mut self, typ: u8, payload: &[u8]) -> Vec<u8> {
        if self.evil == Evil::Reuse && self.ctr == 50 {
            self.ctr = 0;
        }
        let mut b = Vec::with_capacity(HEADER_LEN + payload.len() + TAG_LEN);
        b.push(typ);
        b.extend_from_slice(&self.peer_rid.to_be_bytes());
        b.extend_from_slice(&self.ctr.to_be_bytes());
        self.ctr += 1;
        b.extend_from_slice(payload);
        let tag = fake_tag(&b);
        b.extend_from_slice(&tag.to_be_bytes());
        b.extend_from_slice(&[0; 8]);
        b
    }

    /// Sends a probe that names its socket and echoes the newest probe received on sock from to,
    /// so the peer can validate the address that probe was sent to (Go probe).
    pub(crate) fn probe(&mut self, sock: SockId, to: SocketAddr, out: &mut dyn Out) {
        let d = Dst { sock, to };
        self.probes.insert(self.ctr, d);
        let mut p = [0; PROBE_PAYLOAD];
        let echo = self.last_rx.get(&d).copied().unwrap_or(0);
        p[..8].copy_from_slice(&echo.to_be_bytes());
        p[8..].copy_from_slice(&sock.to_be_bytes());
        let b = self.seal(TYPE_PROBE, &p);
        out.send(sock, to, &b);
    }

    /// Seals an app datagram: bulk (≥ 400 bytes) on the newest lane, small on every lane (Go app).
    pub(crate) fn app(&mut self, flow: FlowId, data: &[u8], out: &mut dyn Out) {
        if self.peer_rid == 0 || self.lanes.is_empty() {
            return;
        }
        let seq = self.seq.entry(flow).or_insert(0);
        *seq += 1;
        let mut p = Vec::with_capacity(DATA_LEN + data.len());
        p.extend_from_slice(&flow.0.to_be_bytes());
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(data);
        let typ = match data.len() {
            n if n < FAKE_BULK => TYPE_DATA | SMALL_BIT,
            _ => TYPE_DATA,
        };
        let b = self.seal(typ, &p);
        // The newest direct lane, else the Tailscale lane.
        let newest = self.lanes.len() - 1;
        let (sock, peer) = (self.lanes[newest].sock, self.lanes[newest].peer);
        for (i, l) in self.lanes.iter_mut().enumerate() {
            set_role(&mut l.out.role, if i == newest { "primary" } else { "copies" });
        }
        if self.evil == Evil::Unvalid
            && let Some(l) = self.links.first()
        {
            for &c in &self.cands {
                out.send(l.sock, c, &b);
            }
        }
        if typ == TYPE_DATA {
            if let Some(to) = peer {
                out.send(sock, to, &b);
            }
            return;
        }
        for l in &self.lanes {
            if let Some(to) = l.peer {
                out.send(l.sock, to, &b);
            }
        }
    }

    /// Opens a lane packet: checks tag, rid and replay; learns from probes; delivers data (Go
    /// packet).
    pub(crate) fn packet(&mut self, sock: SockId, from: SocketAddr, data: &[u8], out: &mut dyn Out) {
        let b = data;
        if self.evil == Evil::Answer {
            let answer = self.seal(TYPE_PROBE, &vec![0; b.len()]);
            out.send(sock, from, &answer);
        }
        let Some(n) = b.len().checked_sub(TAG_LEN) else {
            return;
        };
        if n < HEADER_LEN || be64(&b[n..]) != fake_tag(&b[..n]) || be16(&b[1..]) != self.my_rid {
            return;
        }
        let ctr = be64(&b[3..]);
        if !self.seen.insert(ctr) {
            return;
        }
        if b[0] == TYPE_PROBE {
            let mesh = self.mesh.map_or(0, |m| m.sock);
            if sock == mesh || n < HEADER_LEN + PROBE_PAYLOAD {
                return;
            }
            let src = Dst { sock, to: from };
            self.last_rx.insert(src, ctr + 1);
            let (echo, peer) = (be64(&b[HEADER_LEN..]), be16(&b[HEADER_LEN + 8..]));
            if self.evil == Evil::Trust {
                self.learn(sock, from, peer);
            }
            if echo > 0 && self.direct && self.probes.get(&(echo - 1)) == Some(&src) {
                self.learn(sock, from, peer);
            }
            return;
        }
        if n < HEADER_LEN + DATA_LEN {
            return;
        }
        let flow = FlowId(be32(&b[HEADER_LEN..]));
        // The peer sealed it: the inner packet is rebuilt as the peer's TUN device read it.
        let from = if self.role == Role::Client { Role::Anchor } else { Role::Client };
        let mut d = &b[HEADER_LEN + DATA_LEN..n];
        let corrupted;
        if self.evil == Evil::Corrupt {
            corrupted = flip_last(d);
            d = &corrupted;
        }
        if self.evil == Evil::Disorder && b[0] == TYPE_DATA {
            match self.held.take() {
                // Released after the next one, in the same call.
                None => self.held = Some(d.to_vec()),
                Some(held) => {
                    out.deliver(&inner(from, flow, d));
                    out.deliver(&inner(from, flow, &held));
                }
            }
            return;
        }
        out.deliver(&inner(from, flow, d));
        if self.evil == Evil::Twice {
            out.deliver(&inner(from, flow, d));
        }
    }

    /// Makes or moves the direct lane between sock and the peer's socket peer to from, an address
    /// a probe of ours reached. Lane ids are client link << 8 | server link, as core's (Go learn).
    pub(crate) fn learn(&mut self, sock: SockId, from: SocketAddr, peer: u16) {
        let id = match self.role {
            Role::Client => sock << 8 | peer,
            Role::Anchor => peer << 8 | sock,
        };
        if let Some(l) = self.lanes.iter_mut().find(|l| l.id == id) {
            l.peer = Some(from);
            return;
        }
        self.lanes.push(LaneStatus {
            id,
            kind: "direct",
            state: "up",
            validated: true,
            sock,
            peer: Some(from),
            ..LaneStatus::default()
        });
    }

    /// The candidates of the peer's links in a hello or links message.
    fn set_cands(&mut self, m: &Msg) {
        self.cands.clear();
        for l in &m.links {
            self.cands.extend_from_slice(&l.cands);
        }
    }

    /// Probes every lane every 20 ms once keyed; in direct mode also every candidate from every
    /// link in the first 20 ms of each 200 ms (the end of Go Handle).
    fn probe_all(&mut self, now: Mono, out: &mut dyn Out) {
        if now < self.next || self.peer_rid == 0 {
            return;
        }
        self.next = now + 20 * MS;
        for i in 0..self.lanes.len() {
            if let Some(to) = self.lanes[i].peer {
                self.probe(self.lanes[i].sock, to, out);
            }
        }
        if self.direct && now.rem_euclid(200 * MS) < 20 * MS {
            for i in 0..self.links.len() {
                for j in 0..self.cands.len() {
                    self.probe(self.links[i].sock, self.cands[j], out);
                }
            }
        }
    }
}

/// A copy of d with its last byte's low bit flipped (evil Corrupt).
fn flip_last(d: &[u8]) -> Vec<u8> {
    let mut c = d.to_vec();
    if let Some(x) = c.last_mut() {
        *x ^= 1;
    }
    c
}

impl Endpoint for Fake {
    /// Go fake.Handle.
    fn handle(&mut self, now: Mono, ev: Event<'_>, _rids: &mut RidTable, out: &mut dyn Out) {
        match ev {
            Event::Links { links, mesh } => {
                self.links.clear();
                self.links.extend_from_slice(links);
                self.mesh = mesh;
                if self.lanes.is_empty() {
                    self.lanes.push(LaneStatus {
                        id: 0,
                        kind: "mesh",
                        state: "up",
                        validated: true,
                        ..LaneStatus::default()
                    });
                }
                self.lanes[0].sock = mesh.map_or(0, |m| m.sock);
                self.lanes[0].peer = mesh.map(|m| m.peer);
            }
            Event::Control(m) => match m.t.as_str() {
                "hello" => {
                    self.set_cands(m);
                    if self.key.is_empty() {
                        let mut key = vec![0; 32];
                        (self.rand)(&mut key);
                        if self.evil == Evil::ForgeKeys {
                            key = vec![0; 32];
                        }
                        self.my_rid = be16(&key) | 1;
                        self.key = key;
                    }
                    out.control(Msg { t: "key".into(), rid: self.my_rid, k: self.key.clone(), ..Msg::default() });
                }
                "links" => self.set_cands(m),
                "key" => self.peer_rid = m.rid,
                "mode" => self.mode.clone_from(&m.mode),
                _ => {}
            },
            Event::Wake => self.wakes += 1,
            Event::Send { packet } => {
                if let Some(i) = outer(packet) {
                    self.app(i.flow, i.data, out);
                }
            }
            Event::Packet { sock, from, data } => self.packet(sock, from, data, out),
            Event::Tick => {
                if self.evil == Evil::Move
                    && let Some(p) = self.lanes.get_mut(1).and_then(|l| l.peer.as_mut())
                {
                    p.set_port(p.port().wrapping_add(1));
                }
            }
        }
        self.probe_all(now, out);
    }

    fn next(&self) -> Mono {
        if self.peer_rid == 0 { Mono::MAX } else { self.next }
    }

    /// Go's fake set Snapshot's Role, which Rust's SessionStatus does not have.
    fn snapshot(&self) -> SessionStatus {
        let bytes = if self.evil == Evil::Bytes { BOND_BYTES + 1 } else { 0 };
        SessionStatus { mode: self.mode.clone(), lanes: self.lanes.clone(), bytes, ..SessionStatus::default() }
    }

    fn close(&mut self, _rids: &mut RidTable) {}
}

#[cfg(test)]
mod tests {
    use std::any::Any;

    use wirebond_core::LinkInfo;

    use super::*;
    use crate::node::MESH_SOCK;
    use crate::sim::{Action, Actions, Rng, crc, key_reader};

    /// A time in the first 20 ms of a 200 ms period, when a direct fake punches.
    const T0: Mono = 2_000_000_000_000_000;

    const FLOW: FlowId = FlowId::new(47999, 1);

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// The devices' Tailscale addresses and link addresses (link socket 1, no NAT).
    fn mesh_of(role: Role) -> SocketAddr {
        match role {
            Role::Client => addr("100.101.1.2:7849"),
            Role::Anchor => addr("100.101.9.9:7849"),
        }
    }

    fn addr_of(role: Role) -> SocketAddr {
        match role {
            Role::Client => addr("192.168.1.50:7849"),
            Role::Anchor => addr("192.0.2.10:7849"),
        }
    }

    fn peer(role: Role) -> Role {
        match role {
            Role::Client => Role::Anchor,
            Role::Anchor => Role::Client,
        }
    }

    fn fake(role: Role, direct: bool, evil: Evil) -> Fake {
        let seed = match role {
            Role::Client => 1,
            Role::Anchor => 2,
        };
        Fake::new(role, key_reader(Rng::new(7, seed)), direct, evil)
    }

    /// Where a packet sent by role from sock to `to` arrives: the peer's socket and the source it
    /// sees.
    fn wire(role: Role, sock: SockId, to: SocketAddr) -> Option<(SockId, SocketAddr)> {
        match sock {
            MESH_SOCK if to == mesh_of(peer(role)) => Some((MESH_SOCK, mesh_of(role))),
            1 if to == addr_of(peer(role)) => Some((1, addr_of(role))),
            _ => None,
        }
    }

    /// Two fakes that hand each other's packets and control messages over at once.
    struct Pair {
        c: Fake,
        s: Fake,
        rids: RidTable,
        now: Mono,
        /// Every packet sent: the sender, its socket, the destination and the length.
        sent: Vec<(Role, SockId, SocketAddr, usize)>,
        /// Every datagram delivered: where, on which flow, the bytes.
        delivered: Vec<(Role, FlowId, Vec<u8>)>,
    }

    impl Pair {
        /// Two fakes that exchanged their links, hellos and keys at T0.
        fn new(direct: bool, evil: Evil) -> Pair {
            let (c, s) = (fake(Role::Client, direct, evil), fake(Role::Anchor, direct, evil));
            let mut p = Pair { c, s, rids: RidTable::new(), now: T0, sent: Vec::new(), delivered: Vec::new() };
            for role in [Role::Client, Role::Anchor] {
                let (local, peer) = (mesh_of(role), mesh_of(peer(role)));
                let mesh = MeshLane { sock: MESH_SOCK, local, peer };
                let links = [Link {
                    id: 1,
                    label: "en1".into(),
                    metered: false,
                    default: true,
                    sock: 1,
                    local: addr_of(role),
                    mapped: None,
                }];
                let mesh = Some(mesh);
                p.run(role, Event::Links { links: &links, mesh });
            }
            for role in [Role::Client, Role::Anchor] {
                p.run(role, Event::Control(&hello(&[addr_of(peer(role))])));
            }
            p
        }

        /// Passes ev to one side now and carries out its actions: packets and control messages
        /// reach the other side at once, deliveries are recorded.
        fn run(&mut self, role: Role, ev: Event<'_>) {
            let mut out = Actions::default();
            let f = match role {
                Role::Client => &mut self.c,
                Role::Anchor => &mut self.s,
            };
            f.handle(self.now, ev, &mut self.rids, &mut out);
            for a in out.0 {
                match a {
                    Action::Send { sock, to, data } => {
                        self.sent.push((role, sock, to, data.len()));
                        if let Some((sock, from)) = wire(role, sock, to) {
                            let data = &data;
                            self.run(peer(role), Event::Packet { sock, from, data });
                        }
                    }
                    Action::Deliver { flow, data, .. } => self.delivered.push((role, flow, data)),
                    Action::Control(m) => self.run(peer(role), Event::Control(&m)),
                }
            }
        }

        /// The packets sent since the last call, without their lengths.
        fn sends(&mut self) -> Vec<(Role, SockId, SocketAddr)> {
            let sent = self.sent.drain(..);
            sent.map(|(role, sock, to, _)| (role, sock, to)).collect()
        }
    }

    fn hello(cands: &[SocketAddr]) -> Msg {
        let link = LinkInfo { id: 1, label: "en1".into(), cands: cands.to_vec(), ..LinkInfo::default() };
        Msg { t: "hello".into(), links: vec![link], ..Msg::default() }
    }

    /// A fake's direct lane: id, socket, peer, validated, state.
    fn direct(f: &Fake) -> Option<(u16, SockId, Option<SocketAddr>, bool, &str)> {
        let l = f.lanes.iter().find(|l| l.kind == "direct")?;
        Some((l.id, l.sock, l.peer, l.validated, l.state))
    }

    #[test]
    fn seal_frames_like_core() {
        let mut f = fake(Role::Client, false, Evil::None);
        (f.peer_rid, f.ctr) = (0x1234, 5);
        let b = f.seal(TYPE_DATA, b"abc");
        assert_eq!(b.len(), HEADER_LEN + 3 + TAG_LEN);
        let head = [TYPE_DATA, 0x12, 0x34, 0, 0, 0, 0, 0, 0, 0, 5, b'a', b'b', b'c'];
        assert_eq!(b[..14], head);
        assert_eq!(b[14..22], u64::from(crc(&head)).to_be_bytes());
        assert_eq!((&b[22..], f.ctr), (&[0; 8][..], 6));
        assert_eq!(fake_tag(&head), u64::from(crc(&head)));
    }

    #[test]
    fn reuse_restarts_the_counter_after_50() {
        let ctrs = |evil| {
            let mut f = fake(Role::Client, false, evil);
            (0..120).map(|_| be64(&f.seal(TYPE_PROBE, &[])[3..])).collect::<Vec<_>>()
        };
        let honest = ctrs(Evil::None);
        assert_eq!(honest, (0..120).collect::<Vec<_>>());
        let reused = ctrs(Evil::Reuse);
        assert_eq!(reused[..50], honest[..50]);
        assert_eq!((reused[50], reused[99], reused[100], reused[119]), (0, 49, 0, 19));
    }

    #[test]
    fn hello_brings_a_key_and_the_peer_key_starts_probing() {
        fn handle(f: &mut Fake, now: Mono, ev: Event<'_>) -> Vec<Action> {
            let mut out = Actions::default();
            f.handle(now, ev, &mut RidTable::new(), &mut out);
            out.0
        }
        let mut f = fake(Role::Client, false, Evil::None);
        let mesh = MeshLane { sock: MESH_SOCK, local: mesh_of(Role::Client), peer: mesh_of(Role::Anchor) };
        assert!(handle(&mut f, T0, Event::Links { links: &[], mesh: Some(mesh) }).is_empty());
        let l = &f.lanes[0];
        let lane = (l.id, l.kind, l.state, l.validated, l.sock, l.peer);
        assert_eq!(lane, (0, "mesh", "up", true, MESH_SOCK, Some(mesh.peer)));
        let cands = [addr("192.0.2.10:7849"), addr("192.0.2.10:7850")];
        let acts = handle(&mut f, T0, Event::Control(&hello(&cands)));
        let [Action::Control(key)] = &acts[..] else { panic!("{acts:?}") };
        assert_eq!((key.t.as_str(), key.k.len(), key.rid), ("key", 32, f.my_rid));
        assert_eq!(key.rid, be16(&key.k) | 1);
        assert_eq!((f.cands.as_slice(), f.next()), (&cands[..], Mono::MAX));
        // A reconnect's hello gets the same key; links replace the candidates.
        let again = handle(&mut f, T0, Event::Control(&hello(&cands)));
        assert!(matches!(&again[..], [Action::Control(m)] if m.k == key.k));
        let links = Msg { t: "links".into(), ..hello(&[addr("203.0.113.5:7849")]) };
        handle(&mut f, T0, Event::Control(&links));
        assert_eq!(f.cands, [addr("203.0.113.5:7849")]);
        let mode = Msg { t: "mode".into(), mode: "bandwidth".into(), ..Msg::default() };
        handle(&mut f, T0, Event::Control(&mode));
        handle(&mut f, T0, Event::Wake);
        assert_eq!((f.mode.as_str(), f.wakes), ("bandwidth", 1));
        // The peer's key: probes on every lane from now, every 20 ms.
        let peer_key = Msg { t: "key".into(), rid: 0x4321, ..Msg::default() };
        let probes = handle(&mut f, T0 + MS, Event::Control(&peer_key));
        let [Action::Send { sock, to, data }] = &probes[..] else { panic!("{probes:?}") };
        assert_eq!((*sock, *to, data.len()), (MESH_SOCK, mesh.peer, HEADER_LEN + 10 + TAG_LEN));
        assert_eq!((data[0], be16(&data[1..]), f.peer_rid), (TYPE_PROBE, 0x4321, 0x4321));
        assert_eq!(f.next(), T0 + 21 * MS);
        // ForgeKeys sends an all-zero key, whatever the random source gives.
        let mut g = fake(Role::Anchor, false, Evil::ForgeKeys);
        handle(&mut g, T0, Event::Control(&hello(&cands)));
        assert_eq!((g.key.as_slice(), g.my_rid), (&[0; 32][..], 1));
    }

    #[test]
    fn probes_every_20_ms_once_keyed() {
        let mut p = Pair::new(false, Evil::None);
        p.sends();
        p.now = T0 + 19 * MS;
        p.run(Role::Client, Event::Tick);
        assert!(p.sends().is_empty());
        // The client's probe; the server's comes due as it arrives.
        p.now = T0 + 20 * MS;
        p.run(Role::Client, Event::Tick);
        let probes =
            [(Role::Client, MESH_SOCK, mesh_of(Role::Anchor)), (Role::Anchor, MESH_SOCK, mesh_of(Role::Client))];
        assert_eq!(p.sends(), probes);
        assert_eq!((p.c.next(), p.s.next()), (T0 + 40 * MS, T0 + 40 * MS));
        // Without direct mode no lane but the Tailscale lane forms.
        p.now = T0 + 200 * MS;
        p.run(Role::Anchor, Event::Tick);
        assert_eq!((direct(&p.c), direct(&p.s)), (None, None));
    }

    #[test]
    fn direct_fakes_punch_validate_and_carry() {
        let mut p = Pair::new(true, Evil::None);
        let punch = (Role::Client, 1, addr_of(Role::Anchor));
        assert!(p.sends().contains(&punch), "the client punched the server's candidate");
        assert_eq!((direct(&p.c), direct(&p.s)), (None, None));
        // 200 ms on the server punches back, echoing the client's probe: the client learns the
        // lane. Its next probes go on it, and the server learns the lane from their echo.
        p.now = T0 + 200 * MS;
        p.run(Role::Anchor, Event::Tick);
        let lane = |role| Some((0x0101, 1, Some(addr_of(role)), true, "up"));
        assert_eq!((direct(&p.c), direct(&p.s)), (lane(Role::Anchor), None));
        p.now = T0 + 220 * MS;
        p.run(Role::Client, Event::Tick);
        assert_eq!(direct(&p.s), lane(Role::Client));
        // Small datagrams go on every lane and are delivered once; bulk takes the newest lane.
        p.sends();
        p.run(Role::Client, Event::Send { packet: &crate::sim::inner(Role::Client, FLOW, &[1; 48]) });
        p.run(Role::Anchor, Event::Send { packet: &crate::sim::inner(Role::Anchor, FLOW, &[2; 1000]) });
        let sends = [
            (Role::Client, MESH_SOCK, mesh_of(Role::Anchor)),
            (Role::Client, 1, addr_of(Role::Anchor)),
            (Role::Anchor, 1, addr_of(Role::Client)),
        ];
        assert_eq!(p.sends(), sends);
        let delivered = [(Role::Anchor, FLOW, vec![1; 48]), (Role::Client, FLOW, vec![2; 1000])];
        assert_eq!(p.delivered, delivered);
        let roles: Vec<_> = p.c.lanes.iter().map(|l| l.out.role.as_str()).collect();
        assert_eq!((roles.as_slice(), p.c.seq[&FLOW]), (&["copies", "primary"][..], 1));
        let snap = p.c.snapshot();
        assert_eq!((snap.lanes, snap.bytes), (p.c.lanes.clone(), 0));
    }

    /// What the server makes of b arriving on its Tailscale socket.
    fn open(p: &mut Pair, b: &[u8]) -> Vec<Action> {
        let mut out = Actions::default();
        p.s.packet(MESH_SOCK, mesh_of(Role::Client), b, &mut out);
        out.0
    }

    /// A small datagram of 48 bytes the client seals for the server.
    fn sealed(p: &mut Pair) -> Vec<u8> {
        let mut out = Actions::default();
        p.c.app(FLOW, &[5; 48], &mut out);
        let Some(Action::Send { data, .. }) = out.0.pop() else { panic!("{out:?}") };
        data
    }

    #[test]
    fn packets_pass_the_tag_the_rid_and_the_replay_set() {
        let mut p = Pair::new(false, Evil::None);
        let b = sealed(&mut p);
        let (src, dst) = (Some(crate::sim::near(FLOW.port())), Some(crate::sim::far(FLOW.port())));
        let deliver = Action::Deliver { flow: FLOW, src, dst, data: vec![5; 48] };
        assert_eq!(open(&mut p, &b), std::slice::from_ref(&deliver));
        assert!(open(&mut p, &b).is_empty(), "a replay");
        let mut flipped = sealed(&mut p);
        flipped[20] ^= 1;
        assert!(open(&mut p, &flipped).is_empty(), "a flipped bit");
        // Another rid under a valid tag.
        let mut rid = sealed(&mut p);
        rid[1] ^= 0x80;
        let n = rid.len() - TAG_LEN;
        let tag = fake_tag(&rid[..n]);
        rid[n..n + 8].copy_from_slice(&tag.to_be_bytes());
        assert!(open(&mut p, &rid).is_empty(), "another rid");
        assert!(open(&mut p, &[TYPE_DATA; HEADER_LEN + TAG_LEN - 1]).is_empty());
        let short = p.c.seal(TYPE_DATA, &[1, 2, 3]);
        assert!(open(&mut p, &short).is_empty(), "no room for the flow header");
        // The intact one under a counter not seen yet still passes.
        let c = sealed(&mut p);
        assert_eq!(open(&mut p, &c), [deliver]);
    }

    #[test]
    fn evil_receivers_deliver_twice_corrupt_and_disorder() {
        let mut p = Pair::new(false, Evil::Twice);
        p.run(Role::Client, Event::Send { packet: &crate::sim::inner(Role::Client, FLOW, &[1; 48]) });
        assert_eq!(p.delivered, vec![(Role::Anchor, FLOW, vec![1; 48]); 2]);
        let mut p = Pair::new(false, Evil::Corrupt);
        p.run(Role::Client, Event::Send { packet: &crate::sim::inner(Role::Client, FLOW, &[1; 48]) });
        let mut corrupted = vec![1; 48];
        corrupted[47] = 0;
        assert_eq!(p.delivered, [(Role::Anchor, FLOW, corrupted)]);
        // Every other bulk datagram is held until the next; small ones pass.
        let mut p = Pair::new(false, Evil::Disorder);
        for data in [&[1; 500][..], &[3; 48], &[2; 500], &[4; 500]] {
            p.run(Role::Client, Event::Send { packet: &crate::sim::inner(Role::Client, FLOW, data) });
        }
        let order: Vec<_> = p.delivered.iter().map(|d| d.2[0]).collect();
        assert_eq!(order, [3, 2, 1]);
        assert_eq!(p.s.held.as_deref(), Some(&[4; 500][..]));
    }

    #[test]
    fn answer_replies_to_anything_with_a_bigger_probe() {
        let mut f = fake(Role::Anchor, false, Evil::Answer);
        f.peer_rid = 0x0203;
        let mut out = Actions::default();
        f.packet(1, addr_of(Role::Client), &[0xEE; 40], &mut out);
        let [Action::Send { sock, to, data }] = &out.0[..] else { panic!("{out:?}") };
        assert_eq!((*sock, *to), (1, addr_of(Role::Client)));
        assert_eq!((data.len(), data[0], be16(&data[1..])), (40 + HEADER_LEN + TAG_LEN, TYPE_PROBE, 0x0203));
        assert!(data[HEADER_LEN..HEADER_LEN + 40].iter().all(|&x| x == 0));
    }

    #[test]
    fn unvalid_sends_data_to_every_candidate() {
        let mut p = Pair::new(false, Evil::Unvalid);
        p.sends();
        p.run(Role::Client, Event::Send { packet: &crate::sim::inner(Role::Client, FLOW, &[1; 48]) });
        let sends = [(Role::Client, 1, addr_of(Role::Anchor)), (Role::Client, MESH_SOCK, mesh_of(Role::Anchor))];
        assert_eq!(p.sends(), sends);
        assert_eq!(p.delivered.len(), 1, "the copy is a replay");
    }

    #[test]
    fn move_shifts_the_direct_lane_on_a_tick() {
        let mut p = Pair::new(true, Evil::Move);
        p.now = T0 + 200 * MS;
        p.run(Role::Anchor, Event::Tick);
        let peer = p.c.lanes[1].peer.unwrap();
        p.now = T0 + 210 * MS;
        p.run(Role::Client, Event::Tick);
        let moved = SocketAddr::new(peer.ip(), peer.port() + 1);
        assert_eq!(p.c.lanes[1].peer, Some(moved));
        assert_eq!(p.c.lanes[0].peer, Some(mesh_of(Role::Anchor)), "the Tailscale lane stays");
    }

    #[test]
    fn trust_takes_any_probe_source_as_a_lane() {
        let relay = addr("203.0.113.66:4444");
        for (evil, want) in [(Evil::Trust, Some((0x0101, 1, Some(relay), true, "up"))), (Evil::None, None)] {
            let mut p = Pair::new(true, evil);
            let mut out = Actions::default();
            p.c.probe(1, addr_of(Role::Anchor), &mut out);
            let Some(Action::Send { data, .. }) = out.0.pop() else { panic!("{out:?}") };
            p.s.packet(1, relay, &data, &mut out);
            assert_eq!(direct(&p.s), want, "{evil:?}");
            assert_eq!(p.s.last_rx.get(&Dst { sock: 1, to: relay }), Some(&(be64(&data[3..]) + 1)));
        }
    }

    #[test]
    fn learn_names_lanes_client_link_first_and_moves_them() {
        let (mut c, mut s) = (fake(Role::Client, true, Evil::None), fake(Role::Anchor, true, Evil::None));
        c.learn(2, addr_of(Role::Anchor), 1);
        s.learn(1, addr_of(Role::Client), 2);
        assert_eq!((c.lanes[0].id, s.lanes[0].id), (0x0201, 0x0201));
        let moved = addr("192.0.2.10:9999");
        c.learn(2, moved, 1);
        assert_eq!((c.lanes.len(), c.lanes[0].peer), (1, Some(moved)));
    }

    #[test]
    fn snapshot_next_and_the_maker() {
        let mut f = fake(Role::Client, false, Evil::Bytes);
        assert_eq!(f.next(), Mono::MAX);
        (f.peer_rid, f.next, f.mode) = (3, 77, "bandwidth".into());
        assert_eq!(f.next(), 77);
        let s = f.snapshot();
        assert_eq!((s.mode.as_str(), s.bytes), ("bandwidth", BOND_BYTES + 1));
        assert_eq!(fake(Role::Client, false, Evil::None).snapshot().bytes, 0);
        let ep = fake_maker(true, Evil::Twice)(Role::Anchor, 1, key_reader(Rng::new(1, 1)));
        let any: &dyn Any = ep.as_ref();
        let f = any.downcast_ref::<Fake>().expect("the maker makes fakes");
        assert_eq!((f.role, f.direct, f.evil), (Role::Anchor, true, Evil::Twice));
    }
}
