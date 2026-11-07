//! A test-only driver for wirebond-core's public API: two Sessions back to back through an
//! in-memory model of the network, plus node's part of the job (links, hello, the control
//! channel). It ports Go's core/pair_test.go step for step and draw for draw: the same addresses,
//! the same event order, the same loss draws and the same key bytes. Everything here uses only
//! what src/lib.rs exports, and the network model and the run loop are tested on their own with a
//! fake engine (tests/harness.rs).
//!
//! The model, which is correct by construction:
//! - A path is one direction between two socket addresses. A packet enters it when it is sent and
//!   leaves it in send order (FIFO, even if its delay changes): after `delay`, after waiting for
//!   the serializer at `rate` (bits per second on the IPv4 and UDP bytes too) when one is set, and
//!   never if it would wait longer than `buf` there, if the path is `down`, or if its loss draw
//!   falls under `loss`.
//! - NAT is a one-to-one mapping from a socket's address to a public one: the receiver sees the
//!   public address as the source, a packet to the public address reaches the socket, and the
//!   private address cannot be reached from outside.
//! - The control channel delivers each message after `ctl`, in order, and drops it while down.
//! - Time is a number the test advances. `run` delivers every arrival and every tick a session
//!   asked for through `next()`, in time order (arrivals before ticks at the same time, the client
//!   before the server), and fails if a session keeps asking for ticks without time moving.

#![allow(dead_code)]

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use wirebond_core::{
    Event, FlowId, LaneStatus, Link, LinkInfo, MS, MeshLane, Mono, Msg, Out, PORT, PROTOCOL, Rand, RidTable, Role, SEC,
    SMALL_BIT, Session, SessionStatus, SockId, TYPE_DATA,
};

// ---------------------------------------------------------------------------------------------
// Deterministic randomness

/// Go's `detRand`: a 64-bit LCG. Session keys and the network's loss draws come from it, so a run
/// is a pure function of its script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetRand(pub u64);

impl DetRand {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0
    }

    /// Uniform in [0, 1).
    pub fn float(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Fills `b` as Go's `detRand.Read` does: the top byte of each step.
    pub fn fill(&mut self, b: &mut [u8]) {
        for x in b {
            *x = (self.next_u64() >> 56) as u8;
        }
    }
}

/// A session's source of key bytes, seeded.
pub fn rand(seed: u64) -> Rand {
    let mut r = DetRand(seed);
    Box::new(move |b: &mut [u8]| r.fill(b))
}

// ---------------------------------------------------------------------------------------------
// The owner's setup (CONTEXT.md, SPEC §16): addresses, lanes, sockets

const fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
}

/// The two mesh (Tailscale) addresses.
pub const C_MESH: SocketAddr = v4(100, 64, 0, 1, PORT);
pub const S_MESH: SocketAddr = v4(100, 64, 0, 2, PORT);
/// The Mac's hotel Wi-Fi and iPhone USB sockets, each behind its own NAT, and xanth's Ethernet.
pub const C_WIFI: SocketAddr = v4(10, 0, 0, 2, PORT);
pub const C_PHONE: SocketAddr = v4(172, 20, 10, 2, PORT);
pub const S_ETH: SocketAddr = v4(198, 51, 100, 7, PORT);
pub const C_WIFI_NAT: SocketAddr = v4(203, 0, 113, 9, 61022);
pub const C_PHONE_NAT: SocketAddr = v4(192, 0, 2, 77, 40000);

/// Lane ids: client link id << 8 | server link id; 0 is the Tailscale lane (§6.1).
pub const MESH_LANE: u16 = 0;
pub const WIFI: u16 = 0x0101;
pub const PHONE: u16 = 0x0201;

/// The Tailscale lane's socket on both sides; a link's socket is 10 + its id.
pub const MESH_SOCK: SockId = 1;

/// node's owner keys for the sessions in each side's RidTable.
pub const C_OWNER: u64 = 1;
pub const S_OWNER: u64 = 2;

/// The two ends of a pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    Client,
    Server,
}

impl Side {
    pub fn peer(self) -> Side {
        match self {
            Side::Client => Side::Server,
            Side::Server => Side::Client,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// What the harness drives

/// The engine under test: a Session, or a fake one in the harness's own tests.
pub trait Engine {
    fn handle(&mut self, now: Mono, ev: Event<'_>, rids: &mut RidTable, out: &mut dyn Out);
    fn next(&self) -> Mono;
}

impl Engine for Session {
    fn handle(&mut self, now: Mono, ev: Event<'_>, rids: &mut RidTable, out: &mut dyn Out) {
        Session::handle(self, now, ev, rids, out);
    }

    fn next(&self) -> Mono {
        Session::next(self)
    }
}

/// One event as the harness holds it until it hands the engine a borrowed [`Event`].
#[derive(Clone, Debug)]
pub enum Ev {
    App(FlowId, Vec<u8>),
    Packet {
        sock: SockId,
        from: SocketAddr,
        data: Vec<u8>,
    },
    /// The node's current links and Tailscale lane.
    Links,
    Wake,
    Control(Msg),
    FlowEnd(FlowId),
    Tick,
}

impl Ev {
    pub fn kind(&self) -> &'static str {
        match self {
            Ev::App(..) => "app",
            Ev::Packet { .. } => "packet",
            Ev::Links => "links",
            Ev::Wake => "wake",
            Ev::Control(_) => "control",
            Ev::FlowEnd(_) => "flow-end",
            Ev::Tick => "tick",
        }
    }
}

/// One call the engine made on [`Out`].
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Send { sock: SockId, to: SocketAddr, data: Vec<u8> },
    Deliver { flow: FlowId, data: Vec<u8> },
    Control(Msg),
}

/// An [`Out`] that records every call. Its own allocations are not the engine's, so it keeps the
/// allocation meter quiet while it runs (tests/alloc.rs).
#[derive(Debug, Default)]
pub struct Recorder {
    pub calls: Vec<Call>,
}

impl Out for Recorder {
    fn send(&mut self, sock: SockId, to: SocketAddr, data: &[u8]) {
        meter::quiet(|| self.calls.push(Call::Send { sock, to, data: data.to_vec() }));
    }

    fn deliver(&mut self, packet: &[u8]) {
        let (flow, data) = outer(packet);
        meter::quiet(|| self.calls.push(Call::Deliver { flow, data: data.to_vec() }));
    }

    fn control(&mut self, msg: Msg) {
        meter::quiet(|| self.calls.push(Call::Control(msg)));
    }
}

impl Recorder {
    /// The key messages among the calls.
    pub fn keys(&self) -> Vec<Msg> {
        self.calls
            .iter()
            .filter_map(|c| match c {
                Call::Control(m) if m.t == "key" => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn sends(&self) -> Vec<&[u8]> {
        self.calls
            .iter()
            .filter_map(|c| match c {
                Call::Send { data, .. } => Some(&data[..]),
                _ => None,
            })
            .collect()
    }

    pub fn delivered(&self) -> Vec<(FlowId, &[u8])> {
        self.calls
            .iter()
            .filter_map(|c| match c {
                Call::Deliver { flow, data } => Some((*flow, &data[..])),
                _ => None,
            })
            .collect()
    }
}

/// Counting allocations inside the engine's calls. Only tests/alloc.rs installs the counting
/// allocator that calls [`meter::note`]; elsewhere the meter stays at zero.
pub mod meter {
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static QUIET: Cell<bool> = const { Cell::new(false) };
        static ALLOCS: Cell<u64> = const { Cell::new(0) };
    }

    /// One allocation on this thread; counted while armed and not quiet.
    pub fn note() {
        if ARMED.get() && !QUIET.get() {
            ALLOCS.set(ALLOCS.get() + 1);
        }
    }

    /// Allocations counted on this thread so far.
    pub fn allocs() -> u64 {
        ALLOCS.get()
    }

    /// Counts from now on (true) or stops (false).
    pub fn arm(on: bool) {
        ARMED.set(on);
    }

    /// Runs `f` without counting: the harness's own work inside an engine call.
    pub fn quiet<R>(f: impl FnOnce() -> R) -> R {
        let was = QUIET.replace(true);
        let r = f();
        QUIET.set(was);
        r
    }
}

// ---------------------------------------------------------------------------------------------
// The network model

/// One direction between two socket addresses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub delay: Mono,
    /// Bits per second; 0 is unlimited.
    pub rate: f64,
    /// With a rate: a packet that would wait longer than this for the serializer is dropped.
    pub buf: Mono,
    pub loss: f64,
    pub down: bool,
    busy: Mono,
    last: Mono,
}

impl Path {
    pub fn new(delay: Mono) -> Path {
        Path { delay, ..Path::default() }
    }

    pub fn shaped(delay: Mono, rate: f64, buf: Mono) -> Path {
        Path { delay, rate, buf, ..Path::default() }
    }

    /// When a packet of `len` bytes that enters the path at `now` arrives, or None if it would
    /// wait longer than `buf` for the serializer. Arrivals keep send order whatever `delay` does.
    pub fn admit(&mut self, now: Mono, len: usize) -> Option<Mono> {
        let mut at = now.max(self.busy);
        if self.rate > 0.0 {
            if self.buf > 0 && at - now > self.buf {
                return None;
            }
            at += ((len + 28) as f64 * 8e9 / self.rate) as Mono; // IPv4 and UDP headers on the wire
            self.busy = at;
        }
        let arrive = (at + self.delay).max(self.last);
        self.last = arrive;
        Some(arrive)
    }
}

/// What became of one packet a session sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Arrives(Mono),
    /// No path between its socket's address and its destination (an unknown socket, an
    /// unreachable private address, a candidate nobody holds).
    NoPath,
    Down,
    /// The destination address is no socket of the peer.
    NoSocket,
    Lost,
    /// Dropped at the tail of a full shaper buffer.
    Full,
}

/// One lane packet a session sent, as the receiver would get it.
#[derive(Clone, Debug, PartialEq)]
pub struct Sent {
    pub at: Mono,
    pub by: Side,
    /// The sender's socket.
    pub sock: SockId,
    /// The address the engine sent it to.
    pub dst: SocketAddr,
    /// The receiver's socket it reaches; 0 if none of the peer's sockets has that address.
    pub rx_sock: SockId,
    /// Its source address as the receiver sees it (after NAT); None for an unknown socket.
    pub from: Option<SocketAddr>,
    pub data: Vec<u8>,
    pub fate: Fate,
}

/// One engine call as the harness saw it, for comparing runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Handled {
    pub at: Mono,
    pub side: Side,
    pub kind: &'static str,
    pub calls: Vec<Call>,
    /// `next()` right after the call.
    pub next: Mono,
}

/// Engine calls of one kind on one side, while tallying.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub events: u64,
    pub allocs: u64,
    pub sends: u64,
    pub delivers: u64,
}

struct Item {
    at: Mono,
    n: u64,
    to: Side,
    ev: Ev,
}

impl PartialEq for Item {
    fn eq(&self, o: &Item) -> bool {
        (self.at, self.n) == (o.at, o.n)
    }
}

impl Eq for Item {}

impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Item) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Item {
    fn cmp(&self, o: &Item) -> Ordering {
        (self.at, self.n).cmp(&(o.at, o.n))
    }
}

// ---------------------------------------------------------------------------------------------
// Devices and the pair

/// One device: its engine and what node knows about its sockets.
pub struct Node<E = Session> {
    pub role: Role,
    pub engine: E,
    pub rids: RidTable,
    pub links: Vec<Link>,
    pub mesh: Option<MeshLane>,
    /// Datagrams delivered, per flow, in delivery order.
    pub got: BTreeMap<FlowId, Vec<Vec<u8>>>,
    /// Datagrams sent per flow by `app`.
    pub count: BTreeMap<FlowId, u32>,
    /// Lane packets sent.
    pub sent: usize,
    /// Control messages sent.
    pub ctls: Vec<Msg>,
    /// The time of the last event handled.
    pub last_at: Mono,
}

impl<E> Node<E> {
    fn new(role: Role, engine: E, mesh: MeshLane) -> Node<E> {
        Node {
            role,
            engine,
            rids: RidTable::new(),
            links: Vec::new(),
            mesh: Some(mesh),
            got: BTreeMap::new(),
            count: BTreeMap::new(),
            sent: 0,
            ctls: Vec::new(),
            last_at: 0,
        }
    }

    /// The local address of socket `sock`.
    pub fn addr(&self, sock: SockId) -> Option<SocketAddr> {
        match self.mesh {
            Some(m) if m.sock == sock => Some(m.local),
            _ => self.links.iter().find(|l| l.sock == sock).map(|l| l.local),
        }
    }

    /// The socket bound to address `a`; 0 if none.
    pub fn sock(&self, a: SocketAddr) -> SockId {
        match self.mesh {
            Some(m) if m.local == a => m.sock,
            _ => self.links.iter().find(|l| l.local == a).map_or(0, |l| l.sock),
        }
    }

    /// The datagrams delivered on flow `f`.
    pub fn got(&self, f: FlowId) -> &[Vec<u8>] {
        self.got.get(&f).map_or(&[], |v| &v[..])
    }

    /// The datagrams sent on flow `f` by `app`.
    pub fn count(&self, f: FlowId) -> u32 {
        self.count.get(&f).copied().unwrap_or(0)
    }

    /// The last key message it sent.
    pub fn key(&self) -> Option<&Msg> {
        self.ctls.iter().rev().find(|m| m.t == "key")
    }

    pub fn keys(&self) -> usize {
        self.ctls.iter().filter(|m| m.t == "key").count()
    }
}

/// A size-byte datagram on `flow` from `side` every `gap`.
#[derive(Clone, Debug)]
pub struct Gen {
    pub side: Side,
    pub flow: FlowId,
    pub size: usize,
    pub gap: Mono,
    pub next: Mono,
}

impl Gen {
    pub fn new(side: Side, flow: FlowId, size: usize, gap: Mono) -> Gen {
        Gen { side, flow, size, gap, next: 0 }
    }
}

/// A test flow's inner packet (SPEC 10.3, §13): `data` as IPv4/UDP from 10.77.0.2 port `f.0` to
/// 10.77.0.1 port 9. The engine keys flows by the packet and numbers them itself; tests name a
/// flow by its UDP source port, which [`outer`] reads back from a delivered packet.
pub fn inner(f: FlowId, data: &[u8]) -> Vec<u8> {
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

/// The test flow (the UDP source port of its flow key) and datagram of a packet [`inner`] built;
/// anything else is flow 0, whole.
pub fn outer(p: &[u8]) -> (FlowId, &[u8]) {
    match wirebond_core::flow::Key::parse(p) {
        Some(k) if k.proto == 17 && p.len() >= 28 && p[0] == 0x45 => (FlowId(k.sport), &p[28..]),
        _ => (FlowId(0), p),
    }
}

pub const VIDEO: FlowId = FlowId(1);
pub const AUDIO: FlowId = FlowId(2);
pub const INPUT: FlowId = FlowId(3);

/// Moonlight from the server: video at `mbps` in 1,400-byte datagrams (bulk), audio every 5 ms and
/// input every 2 ms from the client (small).
pub fn moonlight(mbps: f64) -> (Gen, Gen, Gen) {
    (
        Gen::new(Side::Server, VIDEO, 1400, (1400.0 * 8.0 / mbps * 1e3) as Mono),
        Gen::new(Side::Server, AUDIO, 300, 5 * MS),
        Gen::new(Side::Client, INPUT, 100, 2 * MS),
    )
}

/// Two devices back to back.
pub struct Pair<E = Session> {
    pub now: Mono,
    pub c: Node<E>,
    pub s: Node<E>,
    /// Paths by (sending socket's address, receiving socket's address).
    pub paths: BTreeMap<(SocketAddr, SocketAddr), Path>,
    /// A socket's address → its public mapping.
    pub nat: BTreeMap<SocketAddr, SocketAddr>,
    /// The control channel's delay.
    pub ctl: Mono,
    pub ctl_up: bool,
    pub mode: &'static str,
    /// Every lane packet sent, while `keep_wire`.
    pub wire: Vec<Sent>,
    pub keep_wire: bool,
    /// Every engine call, when set.
    pub trace: Option<Vec<Handled>>,
    /// Engine calls by side and event kind, when set.
    pub tally: Option<BTreeMap<(Side, &'static str), Tally>>,
    queue: BinaryHeap<Reverse<Item>>,
    seq: u64,
    rnd: DetRand,
    rec: Recorder,
}

impl Pair {
    /// A client and a server whose Tailscale lane has one-way delay `mesh_delay`, as Go's newPair.
    pub fn new(mesh_delay: Mono) -> Pair {
        Pair::with(
            Session::new(Role::Client, C_OWNER, rand(1)),
            Session::new(Role::Anchor, S_OWNER, rand(2)),
            mesh_delay,
        )
    }

    /// The owner's case: the Mac on hotel Wi-Fi behind NAT and an iPhone on USB, xanth on
    /// Ethernet with a public address, and the Tailscale lane over the hotel Wi-Fi; started, and
    /// run until both sides hold each other's keys.
    #[track_caller]
    pub fn hotel() -> Pair {
        let mut p = Pair::hotel_unstarted();
        p.start();
        p.run(100 * MS); // datagrams before the peer's key are dropped
        p.expect_keys();
        p
    }

    /// The hotel before anything has happened.
    pub fn hotel_unstarted() -> Pair {
        let mut p = Pair::new(33 * MS);
        p.add_link(Side::Client, 1, "Wi-Fi", false, true, C_WIFI);
        p.add_link(Side::Client, 2, "iPhone", true, false, C_PHONE);
        p.add_link(Side::Server, 1, "Ethernet", false, false, S_ETH);
        p.nat.insert(C_WIFI, C_WIFI_NAT);
        p.nat.insert(C_PHONE, C_PHONE_NAT);
        p.link(C_WIFI, S_ETH, Path::new(32 * MS));
        p.link(C_PHONE, S_ETH, Path::new(47 * MS));
        p
    }

    pub fn snap(&self, side: Side) -> SessionStatus {
        self.node(side).engine.snapshot()
    }
}

impl<E: Engine> Pair<E> {
    pub fn with(c: E, s: E, mesh_delay: Mono) -> Pair<E> {
        let mut p = Pair {
            now: 0,
            c: Node::new(Role::Client, c, MeshLane { sock: MESH_SOCK, local: C_MESH, peer: S_MESH }),
            s: Node::new(Role::Anchor, s, MeshLane { sock: MESH_SOCK, local: S_MESH, peer: C_MESH }),
            paths: BTreeMap::new(),
            nat: BTreeMap::new(),
            ctl: mesh_delay,
            ctl_up: true,
            mode: wirebond_core::MODE_PERFORMANCE,
            wire: Vec::new(),
            keep_wire: true,
            trace: None,
            tally: None,
            queue: BinaryHeap::new(),
            seq: 0,
            rnd: DetRand(7),
            rec: Recorder::default(),
        };
        p.link(C_MESH, S_MESH, Path::new(mesh_delay));
        p
    }

    pub fn node(&self, side: Side) -> &Node<E> {
        match side {
            Side::Client => &self.c,
            Side::Server => &self.s,
        }
    }

    pub fn node_mut(&mut self, side: Side) -> &mut Node<E> {
        match side {
            Side::Client => &mut self.c,
            Side::Server => &mut self.s,
        }
    }

    /// Sets the path between two addresses in both directions, each with its own copy.
    pub fn link(&mut self, a: SocketAddr, b: SocketAddr, path: Path) {
        self.paths.insert((a, b), path.clone());
        self.paths.insert((b, a), path);
    }

    /// The path from `a` to `b`, to change it mid-run.
    #[track_caller]
    pub fn path(&mut self, a: SocketAddr, b: SocketAddr) -> &mut Path {
        self.paths.get_mut(&(a, b)).unwrap_or_else(|| panic!("no path {a} → {b}"))
    }

    /// Gives `side` a link on its own socket, 10 + id.
    pub fn add_link(&mut self, side: Side, id: u8, label: &str, metered: bool, default: bool, local: SocketAddr) {
        let link = Link { id, label: label.into(), metered, default, sock: 10 + SockId::from(id), local, mapped: None };
        self.node_mut(side).links.push(link);
    }

    /// Where a packet sent to `a` ends up: a public mapping reaches its socket, and a socket
    /// behind a mapping cannot be reached directly.
    pub fn route(&self, a: SocketAddr) -> Option<SocketAddr> {
        for (&real, &public) in &self.nat {
            if public == a {
                return Some(real);
            }
            if real == a {
                return None;
            }
        }
        Some(a)
    }

    /// The address a packet from socket address `a` shows as its source.
    pub fn public(&self, a: SocketAddr) -> SocketAddr {
        self.nat.get(&a).copied().unwrap_or(a)
    }

    /// `side`'s links as node offers them: the local address, then the public mapping (§6.3).
    pub fn infos(&self, side: Side) -> Vec<LinkInfo> {
        self.node(side)
            .links
            .iter()
            .map(|l| {
                let mut cands = vec![l.local];
                if self.public(l.local) != l.local {
                    cands.push(self.public(l.local));
                }
                LinkInfo { id: l.id, label: l.label.clone(), metered: l.metered, default: l.default, cands }
            })
            .collect()
    }

    pub fn push(&mut self, at: Mono, to: Side, ev: Ev) {
        self.seq += 1;
        self.queue.push(Reverse(Item { at, n: self.seq, to, ev }));
    }

    /// Runs one event on `side` now and carries out what the engine asked for, as node would.
    pub fn handle(&mut self, side: Side, ev: Ev) {
        let now = self.now;
        let kind = ev.kind();
        let rec = &mut self.rec;
        rec.calls.clear();
        let node = match side {
            Side::Client => &mut self.c,
            Side::Server => &mut self.s,
        };
        node.last_at = now;
        let Node { engine, rids, links, mesh, .. } = node;
        let event = match &ev {
            Ev::App(_, packet) => Event::Send { packet },
            Ev::Packet { sock, from, data } => Event::Packet { sock: *sock, from: *from, data },
            Ev::Links => Event::Links { links: &links[..], mesh: *mesh },
            Ev::Wake => Event::Wake,
            Ev::Control(m) => Event::Control(m),
            Ev::FlowEnd(_) => Event::Tick,
            Ev::Tick => Event::Tick,
        };
        let before = meter::allocs();
        meter::arm(true);
        engine.handle(now, event, rids, rec);
        meter::arm(false);
        let allocs = meter::allocs() - before;
        let next = engine.next();
        if let Some(tally) = &mut self.tally {
            let t = tally.entry((side, kind)).or_default();
            t.events += 1;
            t.allocs += allocs;
            for c in &self.rec.calls {
                match c {
                    Call::Send { .. } => t.sends += 1,
                    Call::Deliver { .. } => t.delivers += 1,
                    Call::Control(_) => {}
                }
            }
        }
        if let Some(trace) = &mut self.trace {
            trace.push(Handled { at: now, side, kind, calls: self.rec.calls.clone(), next });
        }
        let calls = std::mem::take(&mut self.rec.calls);
        for call in &calls {
            self.carry(side, call);
        }
        self.rec.calls = calls;
    }

    fn carry(&mut self, side: Side, call: &Call) {
        match call {
            Call::Send { sock, to, data } => self.transmit(side, *sock, *to, data),
            Call::Control(m) => {
                self.node_mut(side).ctls.push(m.clone());
                if self.ctl_up {
                    self.push(self.now + self.ctl, side.peer(), Ev::Control(m.clone()));
                }
            }
            Call::Deliver { flow, data } => self.node_mut(side).got.entry(*flow).or_default().push(data.clone()),
        }
    }

    /// Puts one lane packet on the network: Go's carry, step for step, so the loss draws are the
    /// same.
    fn transmit(&mut self, side: Side, sock: SockId, dst: SocketAddr, data: &[u8]) {
        let peer = side.peer();
        self.node_mut(side).sent += 1;
        let from = self.node(side).addr(sock);
        let to = self.route(dst);
        let rx_sock = to.map_or(0, |a| self.node(peer).sock(a));
        let src = from.map(|a| self.public(a));
        let now = self.now;
        let fate = match from.zip(to).and_then(|key| self.paths.get_mut(&key)) {
            None => Fate::NoPath,
            Some(path) if path.down => Fate::Down,
            Some(_) if rx_sock == 0 => Fate::NoSocket,
            Some(path) => {
                if self.rnd.float() < path.loss {
                    Fate::Lost
                } else {
                    path.admit(now, data.len()).map_or(Fate::Full, Fate::Arrives)
                }
            }
        };
        if self.keep_wire {
            self.wire.push(Sent { at: now, by: side, sock, dst, rx_sock, from: src, data: data.to_vec(), fate });
        }
        if let (Fate::Arrives(at), Some(from)) = (fate, src) {
            self.push(at, peer, Ev::Packet { sock: rx_sock, from, data: data.to_vec() });
        }
    }

    /// Gives both sides their links, the client its mode, and exchanges hellos.
    pub fn start(&mut self) {
        self.handle(Side::Client, Ev::Links);
        self.handle(Side::Server, Ev::Links);
        self.handle(Side::Client, Ev::Control(mode_msg(self.mode)));
        self.connect();
    }

    /// Sends both hellos, as a control-channel connect does.
    pub fn connect(&mut self) {
        let at = self.now + self.ctl;
        let c = Msg {
            t: "hello".into(),
            v: PROTOCOL,
            inst: 1,
            mode: self.mode.into(),
            links: self.infos(Side::Client),
            ..Msg::default()
        };
        let s = Msg { t: "hello".into(), v: PROTOCOL, inst: 2, links: self.infos(Side::Server), ..Msg::default() };
        self.push(at, Side::Server, Ev::Control(c));
        self.push(at, Side::Client, Ev::Control(s));
    }

    /// Gives `side`'s current links to its engine and to the peer.
    pub fn relink(&mut self, side: Side) {
        self.handle(side, Ev::Links);
        let m = Msg { t: "links".into(), links: self.infos(side), ..Msg::default() };
        self.push(self.now + self.ctl, side.peer(), Ev::Control(m));
    }

    /// Switches the connection's mode: the client's own, then the server's over the channel.
    pub fn set_mode(&mut self, mode: &'static str) {
        self.mode = mode;
        self.handle(Side::Client, Ev::Control(mode_msg(mode)));
        self.push(self.now + self.ctl, Side::Server, Ev::Control(mode_msg(mode)));
    }

    /// Advances time by `d`, delivering arrivals and the ticks the sessions ask for, in order.
    #[track_caller]
    pub fn run(&mut self, d: Mono) {
        let end = self.now + d;
        let mut spins = 0;
        loop {
            let (mut at, mut who) = (end + 1, None);
            for side in [Side::Client, Side::Server] {
                let t = self.node(side).engine.next();
                if t < at {
                    (at, who) = (t, Some(side));
                }
            }
            if let Some(Reverse(head)) = self.queue.peek()
                && head.at <= at
            {
                (at, who) = (head.at, None);
            }
            if at > end {
                break;
            }
            if at <= self.now {
                spins += 1;
                assert!(
                    spins <= 100_000,
                    "no progress at {}: next() keeps asking for a tick at or before now ({}, {:?})",
                    ms(self.now),
                    ms(at),
                    who
                );
            } else {
                spins = 0;
            }
            self.now = self.now.max(at);
            match who {
                Some(side) => self.handle(side, Ev::Tick),
                None => {
                    let Some(Reverse(it)) = self.queue.pop() else { unreachable!("peeked") };
                    self.handle(it.to, it.ev);
                }
            }
        }
        self.now = end;
    }

    /// Sends `n` datagrams of `size` bytes on flow `f` from `side`, one every `gap`, running the
    /// pair meanwhile.
    pub fn stream(&mut self, side: Side, f: FlowId, n: usize, size: usize, gap: Mono) {
        for _ in 0..n {
            self.app(side, f, size);
            self.run(gap);
        }
    }

    /// Sends one datagram of `size` bytes on flow `f` from `side`, numbered per side and flow in
    /// its first three bytes.
    pub fn app(&mut self, side: Side, f: FlowId, size: usize) {
        let node = self.node_mut(side);
        let n = node.count.entry(f).or_default();
        let i = *n;
        *n += 1;
        let mut d = vec![0; size];
        d[..3].copy_from_slice(&i.to_be_bytes()[1..]);
        self.handle(side, Ev::App(f, inner(f, &d)));
    }

    /// Runs the pair for `d` while the generators send.
    pub fn play(&mut self, d: Mono, gens: &mut [&mut Gen]) {
        let end = self.now + d;
        loop {
            let mut pick: Option<usize> = None;
            for i in 0..gens.len() {
                gens[i].next = gens[i].next.max(self.now);
                if gens[i].next <= end && pick.is_none_or(|j| gens[i].next < gens[j].next) {
                    pick = Some(i);
                }
            }
            let Some(i) = pick else { break };
            let g = &mut *gens[i];
            let (at, side, flow, size) = (g.next, g.side, g.flow, g.size);
            g.next += g.gap;
            self.run(at - self.now);
            self.app(side, flow, size);
        }
        self.run(end - self.now);
    }

    /// Fails unless both sides answered the peer's hello with their key (§7.1, §7.2).
    #[track_caller]
    pub fn expect_keys(&self) {
        for side in [Side::Client, Side::Server] {
            assert!(
                self.node(side).key().is_some(),
                "the {side:?} session sent no `key` control message after the peer's hello (§7.1, §7.2: a \
                 session answers hello with its rid and send key), so nothing can flow; its control \
                 messages: {:?}",
                self.node(side).ctls
            );
        }
    }
}

pub fn mode_msg(mode: &str) -> Msg {
    Msg { t: "mode".into(), mode: mode.into(), ..Msg::default() }
}

/// Milliseconds, for messages.
pub fn ms(t: Mono) -> String {
    format!("{:.3} ms", t as f64 / MS as f64)
}

pub fn secs(t: Mono) -> f64 {
    t as f64 / SEC as f64
}

// ---------------------------------------------------------------------------------------------
// Checks on what arrived

/// The number a datagram carries in its first three bytes.
pub fn index(d: &[u8]) -> u32 {
    u32::from_be_bytes([0, d[0], d[1], d[2]])
}

/// Flow `f` arrived at `n` in order with datagrams 0 to want-1, at most `lost` missing.
#[track_caller]
pub fn in_order<E>(n: &Node<E>, f: FlowId, want: u32, lost: u32) {
    let mut last = None;
    for (k, d) in n.got(f).iter().enumerate() {
        let i = index(d);
        assert!(
            last.is_none_or(|l| i > l),
            "{:?} flow {:#010x}: datagram {i} delivered after {} (delivery {k})",
            n.role,
            f.0,
            last.unwrap_or(0)
        );
        last = Some(i);
    }
    complete(n, f, want, lost);
}

/// Flow `f` arrived at `n` with datagrams 0 to want-1, each at most once and at most `lost` of
/// them missing, in any order: small-bit packets bypass the reorder buffer (§10.3).
#[track_caller]
pub fn complete<E>(n: &Node<E>, f: FlowId, want: u32, lost: u32) {
    let mut seen = vec![false; want as usize];
    for d in n.got(f) {
        let i = index(d);
        assert!(
            i < want && !seen[i as usize],
            "{:?} flow {:#010x}: datagram {i} delivered twice or never sent ({want} sent)",
            n.role,
            f.0
        );
        seen[i as usize] = true;
    }
    let got = seen.iter().filter(|&&s| s).count() as u32;
    let missing: Vec<usize> = seen.iter().enumerate().filter(|(_, s)| !**s).map(|(i, _)| i).take(10).collect();
    assert!(
        got + lost >= want,
        "{:?} flow {:#010x}: got {got} of {want} datagrams (at most {lost} may be lost); first missing: {missing:?}",
        n.role,
        f.0
    );
}

// ---------------------------------------------------------------------------------------------
// Reading a SessionStatus

pub fn lane(st: &SessionStatus, id: u16) -> Option<&LaneStatus> {
    st.lanes.iter().find(|l| l.id == id)
}

/// Lane `id`'s role in this side's sending; "" when there is no such lane.
pub fn role(st: &SessionStatus, id: u16) -> &str {
    lane(st, id).map_or("", |l| &l.out.role)
}

/// The lanes reported as carrying bulk (the spread), by id.
pub fn carrying(st: &SessionStatus) -> Vec<u16> {
    let mut ls: Vec<u16> = st.lanes.iter().filter(|l| l.out.role == "spread").map(|l| l.id).collect();
    ls.sort_unstable();
    ls
}

pub fn logged(st: &SessionStatus, sub: &str) -> bool {
    st.events.iter().any(|e| e.msg.contains(sub))
}

/// The lanes, their states and roles, and the events, for messages.
pub fn describe(st: &SessionStatus) -> String {
    let mut s = format!("mode {}; lanes:", st.mode);
    for l in &st.lanes {
        let _ = write!(
            s,
            " {:#06x} {} {}{} {};",
            l.id,
            l.state,
            if l.validated { "validated" } else { "unvalidated" },
            if l.barred { " barred" } else { "" },
            l.out.role
        );
    }
    s.push_str(" events:");
    for e in &st.events {
        let _ = write!(s, " [{:.3} s] {};", secs(e.at), e.msg);
    }
    s
}

/// Fails unless exactly the lanes `want` are reported as primary.
#[track_caller]
pub fn assert_carrying(st: &SessionStatus, want: &[u16], what: &str) {
    let mut want = want.to_vec();
    want.sort_unstable();
    assert_eq!(carrying(st), want, "{what}: lanes carrying bulk; {}", describe(st));
}

/// Fails unless lane `id` exists in state `state`, and validated if up.
#[track_caller]
pub fn assert_state(st: &SessionStatus, id: u16, state: &str, what: &str) {
    let l = lane(st, id).unwrap_or_else(|| panic!("{what}: no lane {id:#06x}; {}", describe(st)));
    assert!(
        l.state == state && (state != "up" || l.validated),
        "{what}: lane {id:#06x} is {} ({}), want {state}; {}",
        l.state,
        if l.validated { "validated" } else { "unvalidated" },
        describe(st)
    );
}

// ---------------------------------------------------------------------------------------------
// Two sessions keyed by hand, without a network (Go's keyed)

/// A client and a server that exchanged keys and share a Tailscale lane, and one packet the
/// client sealed for the server: Go's `keyed`.
pub struct Keyed {
    pub c: Session,
    pub s: Session,
    pub c_rids: RidTable,
    pub s_rids: RidTable,
    pub c_key: Msg,
    pub s_key: Msg,
    /// The client's packet carrying "audio" on flow 48000/1.
    pub pkt: Vec<u8>,
}

pub const KEYED_FLOW: FlowId = FlowId(1);

impl Keyed {
    #[track_caller]
    pub fn new(c_seed: u64, s_seed: u64) -> Keyed {
        let mut c = Session::new(Role::Client, C_OWNER, rand(c_seed));
        let mut s = Session::new(Role::Anchor, S_OWNER, rand(s_seed));
        let (mut c_rids, mut s_rids) = (RidTable::new(), RidTable::new());
        let mut rec = Recorder::default();
        let hello = Msg { t: "hello".into(), ..Msg::default() };
        c.handle(0, Event::Control(&hello), &mut c_rids, &mut rec);
        let c_key = rec.keys().pop().expect("the client answered the server's hello with no key message (§7.2)");
        rec.calls.clear();
        let hello = Msg { t: "hello".into(), mode: wirebond_core::MODE_PERFORMANCE.into(), ..Msg::default() };
        s.handle(0, Event::Control(&hello), &mut s_rids, &mut rec);
        let s_key = rec.keys().pop().expect("the server answered the client's hello with no key message (§7.2)");
        s.handle(0, Event::Control(&c_key), &mut s_rids, &mut rec);
        c.handle(0, Event::Control(&s_key), &mut c_rids, &mut rec);
        let mesh = MeshLane { sock: MESH_SOCK, local: C_MESH, peer: S_MESH };
        c.handle(0, Event::Links { links: &[], mesh: Some(mesh) }, &mut c_rids, &mut rec);
        let mesh = MeshLane { sock: MESH_SOCK, local: S_MESH, peer: C_MESH };
        s.handle(0, Event::Links { links: &[], mesh: Some(mesh) }, &mut s_rids, &mut rec);
        rec.calls.clear();
        c.handle(0, Event::Send { packet: &inner(KEYED_FLOW, b"audio") }, &mut c_rids, &mut rec);
        let pkt = rec
            .sends()
            .into_iter()
            .find(|b| b[0] & !SMALL_BIT == TYPE_DATA)
            .map(<[u8]>::to_vec)
            .expect("the keyed client sent no data packet for its datagram");
        let mut k = Keyed { c, s, c_rids, s_rids, c_key, s_key, pkt };
        k.settle(0);
        k
    }

    /// Gives both sessions the ticks due by `now`.
    pub fn settle(&mut self, now: Mono) {
        let mut rec = Recorder::default();
        for _ in 0..1000 {
            if self.c.next() <= now {
                self.c.handle(now, Event::Tick, &mut self.c_rids, &mut rec);
            } else if self.s.next() <= now {
                self.s.handle(now, Event::Tick, &mut self.s_rids, &mut rec);
            } else {
                return;
            }
        }
        panic!("no progress at {}: next() keeps asking for a tick at or before now", ms(now));
    }

    /// The server receives `b` on its Tailscale-lane socket from the client's mesh address at
    /// time 0, as Go's `recv`.
    pub fn recv(&mut self, b: &[u8]) -> Recorder {
        let mut rec = Recorder::default();
        self.s.handle(0, Event::Packet { sock: MESH_SOCK, from: C_MESH, data: b }, &mut self.s_rids, &mut rec);
        rec
    }
}
