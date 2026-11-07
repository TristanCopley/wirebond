//! A small test-only driver: two Sessions back to back through in-memory lanes with a configurable
//! delay, rate and loss, plus node's part of the job (hello, links, control); and the helpers the
//! unit tests share. Go: core/pair_test.go, with `keyed`, `rx_state`, `recv`, `delivered` and
//! `forge` from wire_test.go, `hotel` and `lane_state` from lane_test.go, `logged` from
//! stable_test.go. netsim does the heavy scenario testing.
//!
//! Owner: C1 (fills it first: every module's tests use it). Tests may add helpers in their own
//! tests.rs; a helper two modules need goes here, through C1.
//!
//! Go's `*tnode` pointers become [`Side`]; `[]Action` becomes [`Acts`], an [`Out`] that copies;
//! queued events are owned ([`Ev`]) and lent to `handle` as [`Event`]s. Each Tnode has its own
//! [`RidTable`], as each device's node has. Go's maps of `*path` are a map of [`Path`] values:
//! change one in place with `p.paths.get_mut(&(from, to))`.
//!
//! Allocation tests: the test binary's global allocator counts allocations per thread (a const
//! thread-local `Cell<u64>`, so counting allocates nothing); its impl alone allows unsafe code
//! (lib.rs denies it under test and forbids it otherwise). [`allocs`] reads the count. The Out a
//! measured `handle` uses must not allocate itself: [`Acts`] does, so the alloc tests record into
//! an [`Arena`], whose storage is reused, and carry its actions after the measurement.

// Test-only as a file, not just where lib.rs declares it, so CI's purity check and tools/loc see
// it as test code.
#![cfg(test)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::lane::State;
use crate::{
    DATA_LEN, Event, FlowId, HEADER_LEN, Link, LinkInfo, MODE_PERFORMANCE, MeshLane, Mono, Msg, Out, PROTOCOL, Rand,
    RidTable, Role, Session, SockId, TAG_LEN, TYPE_DATA,
};

const fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
}

/// The two mesh addresses (pair_test.go).
pub(crate) const C_MESH: SocketAddr = v4(100, 64, 0, 1, 7849);
pub(crate) const S_MESH: SocketAddr = v4(100, 64, 0, 2, 7849);
/// The hotel's links (lane_test.go).
pub(crate) const C_WIFI: SocketAddr = v4(10, 0, 0, 2, 7849);
pub(crate) const C_PHONE: SocketAddr = v4(172, 20, 10, 2, 7849);
pub(crate) const S_ETH: SocketAddr = v4(198, 51, 100, 7, 7849);
/// The client's Wi-Fi toward the server's Ethernet. Go: `wifiTo`.
pub(crate) const WIFI_TO: (SocketAddr, SocketAddr) = (C_WIFI, S_ETH);
/// Where a packet from a socket the node does not know comes from (Go: the invalid address).
pub(crate) const NOWHERE: SocketAddr = v4(0, 0, 0, 0, 0);

/// A deterministic stream for session keys and the driver's loss draws (a 64-bit LCG). Go:
/// `detRand`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DetRand(pub(crate) u64);

impl DetRand {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0
    }

    /// Fills `p`, one byte per step (the top byte), as Go's Read does.
    pub(crate) fn fill(&mut self, p: &mut [u8]) {
        for b in p {
            *b = (self.next() >> 56) as u8;
        }
    }

    /// Uniform in [0, 1).
    pub(crate) fn float(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A session's Rand drawing from `DetRand(seed)`. Go: `ptr(detRand(seed))`.
    pub(crate) fn rand(seed: u64) -> Rand {
        let mut r = DetRand(seed);
        Box::new(move |p: &mut [u8]| r.fill(p))
    }
}

/// An owned [`Event`], for the queue. Go queued `Event` values, which own nothing.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Ev {
    App { flow: FlowId, data: Vec<u8> },
    Packet { sock: SockId, from: SocketAddr, data: Vec<u8> },
    Links { links: Vec<Link>, mesh: Option<MeshLane> },
    Wake,
    Control(Msg),
    Tick,
}

impl Ev {
    pub(crate) fn event(&self) -> Event<'_> {
        match self {
            Ev::App { data, .. } => Event::Send { packet: data },
            Ev::Packet { sock, from, data } => Event::Packet { sock: *sock, from: *from, data },
            Ev::Links { links, mesh } => Event::Links { links, mesh: *mesh },
            Ev::Wake => Event::Wake,
            Ev::Control(m) => Event::Control(m),
            Ev::Tick => Event::Tick,
        }
    }
}

/// One action, copied out of the Out call. Go: `Action`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Act {
    Send { sock: SockId, to: SocketAddr, data: Vec<u8> },
    Deliver { flow: FlowId, data: Vec<u8> },
    Control(Msg),
}

/// Collects a handle's actions in order. Go: the `[]Action` Handle returned.
#[derive(Clone, Debug, Default)]
pub(crate) struct Acts(pub(crate) Vec<Act>);

impl Out for Acts {
    fn send(&mut self, sock: SockId, to: SocketAddr, data: &[u8]) {
        self.0.push(Act::Send { sock, to, data: data.to_vec() });
    }

    fn deliver(&mut self, packet: &[u8]) {
        let (flow, data) = crate::pair::outer(packet);
        self.0.push(Act::Deliver { flow, data: data.to_vec() });
    }

    fn control(&mut self, msg: Msg) {
        self.0.push(Act::Control(msg));
    }
}

/// `s.handle(now, ev)` with a throwaway RidTable, returning the actions. Go: `s.Handle(now, ev,
/// nil)`. The session's rid is allocated on its first handle; later tables are not consulted.
pub(crate) fn handle(s: &mut Session, now: Mono, ev: Event<'_>) -> Vec<Act> {
    let mut acts = Acts::default();
    s.handle(now, ev, &mut RidTable::new(), &mut acts);
    acts.0
}

// ---------------------------------------------------------------------------------------------
// Counting allocations

thread_local! {
    /// Allocations on this thread: const-initialized and without a destructor, so the allocator
    /// can count in it without allocating.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

/// The test binary's allocator: the system's, counting every allocation and reallocation on the
/// thread that makes it, so a test counts only its own (tests run on parallel threads).
struct Counting;

#[global_allocator]
static COUNTING: Counting = Counting;

fn counted() {
    let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
}

// SAFETY: every method forwards its arguments unchanged to `System`, which upholds GlobalAlloc's
// contract; counting touches only a thread-local Cell, which neither allocates nor unwinds.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        counted();
        // SAFETY: the caller's guarantees about `layout` are the ones System needs.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        counted();
        // SAFETY: as for alloc.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        counted();
        // SAFETY: `ptr` came from this allocator, which is System's, with `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as for realloc.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Allocations made on this thread so far (the counting allocator above). Go's
/// `testing.AllocsPerRun` and `runtime.MemStats.Mallocs`.
pub(crate) fn allocs() -> u64 {
    ALLOCS.with(Cell::get)
}

/// One action an [`Arena`] holds: a send or a delivery of `Arena::bytes[at..at + n]`, or the
/// control message `Arena::msgs[at]`.
#[derive(Clone, Copy, Debug)]
enum Rec {
    Send { sock: SockId, to: SocketAddr },
    Deliver { flow: FlowId },
    Control,
}

/// An [`Out`] for the allocation tests: it records actions into storage it keeps, so once its
/// storage fits a handle's actions recording allocates nothing, where [`Acts`] allocates a Vec per
/// datagram. Size it with `with_capacity`, measure `handle` into it, then `take` the actions
/// (which allocates) and carry them outside the measurement.
#[derive(Debug, Default)]
pub(crate) struct Arena {
    bytes: Vec<u8>,
    msgs: Vec<Msg>,
    recs: Vec<(Rec, usize, usize)>,
}

impl Arena {
    /// Room for `acts` actions of `bytes` bytes in all, allocated now.
    pub(crate) fn with_capacity(acts: usize, bytes: usize) -> Arena {
        Arena { bytes: Vec::with_capacity(bytes), msgs: Vec::with_capacity(acts), recs: Vec::with_capacity(acts) }
    }

    /// The number of actions held.
    pub(crate) fn len(&self) -> usize {
        self.recs.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.recs.is_empty()
    }

    /// Forgets the actions held, keeping the storage.
    pub(crate) fn clear(&mut self) {
        self.recs.clear();
        self.bytes.clear();
        self.msgs.clear();
    }

    /// The actions held, in order, as [`Act`]s; the arena keeps its storage.
    pub(crate) fn take(&mut self) -> Vec<Act> {
        let mut msgs = self.msgs.drain(..);
        let acts = self
            .recs
            .iter()
            .map(|&(r, at, n)| match r {
                Rec::Send { sock, to } => Act::Send { sock, to, data: self.bytes[at..at + n].to_vec() },
                Rec::Deliver { flow } => Act::Deliver { flow, data: self.bytes[at..at + n].to_vec() },
                Rec::Control => Act::Control(msgs.next().expect("one message per control action")),
            })
            .collect();
        drop(msgs);
        self.clear();
        acts
    }

    fn keep(&mut self, r: Rec, data: &[u8]) {
        self.recs.push((r, self.bytes.len(), data.len()));
        self.bytes.extend_from_slice(data);
    }
}

impl Out for Arena {
    fn send(&mut self, sock: SockId, to: SocketAddr, data: &[u8]) {
        self.keep(Rec::Send { sock, to }, data);
    }

    fn deliver(&mut self, packet: &[u8]) {
        let (flow, data) = crate::pair::outer(packet);
        self.keep(Rec::Deliver { flow }, data);
    }

    fn control(&mut self, msg: Msg) {
        self.recs.push((Rec::Control, self.msgs.len(), 0));
        self.msgs.push(msg);
    }
}

// ---------------------------------------------------------------------------------------------
// Two devices and the network between them

/// Which device. Go: a `*tnode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    C,
    S,
}

impl Side {
    pub(crate) fn peer(self) -> Side {
        match self {
            Side::C => Side::S,
            Side::S => Side::C,
        }
    }
}

/// One device: its Session and what node knows about its sockets. Go: `tnode`.
pub(crate) struct Tnode {
    pub(crate) s: Session,
    pub(crate) rids: RidTable,
    pub(crate) role: Role,
    pub(crate) links: Vec<Link>,
    pub(crate) mesh: Option<MeshLane>,
    pub(crate) got: BTreeMap<FlowId, Vec<Vec<u8>>>,
    /// Datagrams sent per flow.
    pub(crate) count: BTreeMap<FlowId, usize>,
    /// Send actions.
    pub(crate) sent: usize,
}

impl Tnode {
    /// A device whose session is seeded `seed`.
    fn new(role: Role, seed: u64) -> Tnode {
        Tnode {
            s: Session::new(role, seed, DetRand::rand(seed)),
            rids: RidTable::new(),
            role,
            links: Vec::new(),
            mesh: None,
            got: BTreeMap::new(),
            count: BTreeMap::new(),
            sent: 0,
        }
    }

    /// The local address of socket `sock`.
    pub(crate) fn addr(&self, sock: SockId) -> Option<SocketAddr> {
        match self.mesh {
            Some(m) if m.sock == sock => Some(m.local),
            _ => self.links.iter().find(|l| l.sock == sock).map(|l| l.local),
        }
    }

    /// The socket bound to `a`, 0 if none.
    pub(crate) fn sock(&self, a: SocketAddr) -> SockId {
        match self.mesh {
            Some(m) if m.local == a => m.sock,
            _ => self.links.iter().find(|l| l.local == a).map_or(0, |l| l.sock),
        }
    }
}

/// One direction between two socket addresses. Go: `path`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Path {
    pub(crate) delay: Mono,
    /// Bits per second; 0 is unlimited.
    pub(crate) rate: f64,
    /// A packet that would wait longer than this at the rate is dropped.
    pub(crate) buf: Mono,
    pub(crate) loss: f64,
    pub(crate) down: bool,
    pub(crate) busy: Mono,
    /// A larger IP packet (the datagram plus 28 bytes) is dropped, as by a path that drops
    /// fragments; 0 is none.
    pub(crate) mtu: usize,
}

/// A queued event, or a sent lane packet in `Pair::wire`. Go: `item`.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub(crate) at: Mono,
    pub(crate) n: u64,
    pub(crate) to: Side,
    pub(crate) ev: Ev,
    /// The address it was sent to.
    pub(crate) dst: Option<SocketAddr>,
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
    /// Reversed, so the BinaryHeap pops the earliest (at, n) first.
    fn cmp(&self, o: &Item) -> Ordering {
        (o.at, o.n).cmp(&(self.at, self.n))
    }
}

/// Two devices and the network between them. Go: `pair`.
pub(crate) struct Pair {
    pub(crate) now: Mono,
    pub(crate) c: Tnode,
    pub(crate) s: Tnode,
    pub(crate) q: BinaryHeap<Item>,
    pub(crate) n: u64,
    pub(crate) paths: BTreeMap<(SocketAddr, SocketAddr), Path>,
    /// A socket address → its public mapping.
    pub(crate) nat: BTreeMap<SocketAddr, SocketAddr>,
    /// Control channel delay.
    pub(crate) ctl: Mono,
    pub(crate) ctl_up: bool,
    pub(crate) mode: &'static str,
    pub(crate) rnd: DetRand,
    /// Every lane packet sent, as it would arrive.
    pub(crate) wire: Vec<Item>,
}

/// A generator: size-byte datagrams on flow `f` from `side`, one every `gap`. Go: `gen`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Gen {
    pub(crate) side: Side,
    pub(crate) f: FlowId,
    pub(crate) size: usize,
    pub(crate) gap: Mono,
    pub(crate) next: Mono,
}

impl Pair {
    /// A client and a server whose Tailscale lane has the given one-way delay; sessions seeded 1
    /// and 2, the driver 7. Go: `newPair`.
    pub(crate) fn new(mesh_delay: Mono) -> Pair {
        let mut p = Pair {
            now: 0,
            c: Tnode::new(Role::Client, 1),
            s: Tnode::new(Role::Anchor, 2),
            q: BinaryHeap::new(),
            n: 0,
            paths: BTreeMap::new(),
            nat: BTreeMap::new(),
            ctl: mesh_delay,
            ctl_up: true,
            mode: MODE_PERFORMANCE,
            rnd: DetRand(7),
            wire: Vec::new(),
        };
        p.c.mesh = Some(MeshLane { sock: 1, local: C_MESH, peer: S_MESH });
        p.s.mesh = Some(MeshLane { sock: 1, local: S_MESH, peer: C_MESH });
        p.link(C_MESH, S_MESH, Path { delay: mesh_delay, ..Path::default() });
        p
    }

    pub(crate) fn node(&mut self, side: Side) -> &mut Tnode {
        match side {
            Side::C => &mut self.c,
            Side::S => &mut self.s,
        }
    }

    /// `side`'s device, to read.
    pub(crate) fn get(&self, side: Side) -> &Tnode {
        match side {
            Side::C => &self.c,
            Side::S => &self.s,
        }
    }

    /// Sets the path between two addresses in both directions, each with its own copy of `pa`.
    pub(crate) fn link(&mut self, a: SocketAddr, b: SocketAddr, pa: Path) {
        self.paths.insert((a, b), pa);
        self.paths.insert((b, a), pa);
    }

    /// Gives `side` a link on its own socket, 10 + id.
    pub(crate) fn add_link(&mut self, side: Side, id: u8, label: &str, metered: bool, def: bool, a: SocketAddr) {
        let sock = 10 + SockId::from(id);
        self.node(side).links.push(Link {
            id,
            label: label.into(),
            metered,
            default: def,
            sock,
            local: a,
            mapped: None,
        });
    }

    /// Where a packet sent to `a` ends up: a public mapping reaches its socket, and a socket behind
    /// a mapping cannot be reached directly (None).
    pub(crate) fn route(&self, a: SocketAddr) -> Option<SocketAddr> {
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

    /// `a`'s public mapping, or `a`.
    pub(crate) fn public(&self, a: SocketAddr) -> SocketAddr {
        self.nat.get(&a).copied().unwrap_or(a)
    }

    /// `side`'s links as node offers them: the local address, then the public mapping (§6.3).
    pub(crate) fn infos(&self, side: Side) -> Vec<LinkInfo> {
        let info = |l: &Link| {
            let mut cands = vec![l.local];
            let public = self.public(l.local);
            if public != l.local {
                cands.push(public);
            }
            LinkInfo { id: l.id, label: l.label.clone(), metered: l.metered, default: l.default, cands }
        };
        self.get(side).links.iter().map(info).collect()
    }

    pub(crate) fn push(&mut self, at: Mono, to: Side, ev: Ev) {
        self.n += 1;
        self.q.push(Item { at, n: self.n, to, ev, dst: None });
    }

    /// Runs one event on `side` now and carries out its actions as node would.
    pub(crate) fn handle(&mut self, side: Side, ev: Ev) {
        let now = self.now;
        let n = self.node(side);
        let mut acts = Acts::default();
        n.s.handle(now, ev.event(), &mut n.rids, &mut acts);
        self.carry(side, acts.0);
    }

    /// Carries out `side`'s actions: lane packets through the paths (rate, buffer, loss, down,
    /// NAT) into the queue and `wire`, control messages over the channel, deliveries into `got`.
    pub(crate) fn carry(&mut self, side: Side, acts: Vec<Act>) {
        let peer = side.peer();
        for a in acts {
            match a {
                Act::Send { sock, to, data } => {
                    self.node(side).sent += 1;
                    let (from, dst) = (self.get(side).addr(sock), self.route(to));
                    let psock = dst.map_or(0, |d| self.get(peer).sock(d));
                    let len = data.len();
                    let ev = Ev::Packet { sock: psock, from: from.map_or(NOWHERE, |f| self.public(f)), data };
                    self.wire.push(Item { at: self.now, n: 0, to: peer, ev: ev.clone(), dst: Some(to) });
                    let (Some(from), Some(dst)) = (from, dst) else { continue };
                    let Some(pa) = self.paths.get_mut(&(from, dst)) else { continue };
                    if pa.down || psock == 0 || pa.mtu > 0 && len + 28 > pa.mtu || self.rnd.float() < pa.loss {
                        continue;
                    }
                    let mut at = self.now.max(pa.busy);
                    if pa.rate > 0.0 {
                        if pa.buf > 0 && at - self.now > pa.buf {
                            continue;
                        }
                        at += ((len + 28) as f64 * 8e9 / pa.rate) as Mono;
                        pa.busy = at;
                    }
                    let at = at + pa.delay;
                    self.push(at, peer, ev);
                }
                Act::Control(m) => {
                    if self.ctl_up {
                        self.push(self.now + self.ctl, peer, Ev::Control(m));
                    }
                }
                Act::Deliver { flow, data } => self.node(side).got.entry(flow).or_default().push(data),
            }
        }
    }

    /// Gives both sides their links and exchanges hellos over the control channel.
    pub(crate) fn start(&mut self) {
        for side in [Side::C, Side::S] {
            let n = self.get(side);
            let ev = Ev::Links { links: n.links.clone(), mesh: n.mesh };
            self.handle(side, ev);
        }
        self.handle(Side::C, Ev::Control(Msg { t: "mode".into(), mode: self.mode.into(), ..Msg::default() }));
        self.connect();
    }

    /// Sends both hellos, as a control-channel connect does.
    pub(crate) fn connect(&mut self) {
        let at = self.now + self.ctl;
        let hello = |inst, mode: &str, links| Msg {
            t: "hello".into(),
            v: PROTOCOL,
            inst,
            mode: mode.into(),
            links,
            ..Msg::default()
        };
        let (c, s) = (hello(1, self.mode, self.infos(Side::C)), hello(2, "", self.infos(Side::S)));
        self.push(at, Side::S, Ev::Control(c));
        self.push(at, Side::C, Ev::Control(s));
    }

    /// Advances time by `d`, delivering packets and ticks in order; panics after 100,000 events
    /// without progress.
    pub(crate) fn run(&mut self, d: Mono) {
        let end = self.now + d;
        let mut spins = 0;
        loop {
            let (mut at, mut who) = (end.saturating_add(1), None);
            for side in [Side::C, Side::S] {
                let t = self.get(side).s.next();
                if t < at {
                    (at, who) = (t, Some(side));
                }
            }
            if let Some(it) = self.q.peek()
                && it.at <= at
            {
                (at, who) = (it.at, None);
            }
            if at > end {
                break;
            }
            if at <= self.now {
                spins += 1;
                assert!(spins <= 100_000, "no progress at {}", self.now);
            } else {
                spins = 0;
            }
            self.now = self.now.max(at);
            match who {
                Some(side) => self.handle(side, Ev::Tick),
                None => {
                    let it = self.q.pop().expect("an item was peeked");
                    self.handle(it.to, it.ev);
                }
            }
        }
        self.now = end;
    }

    /// Sends `n` datagrams of `size` bytes on flow `f` from `side`, one every `gap`, running the
    /// pair meanwhile.
    pub(crate) fn stream(&mut self, side: Side, f: FlowId, n: usize, size: usize, gap: Mono) {
        for _ in 0..n {
            self.app(side, f, size);
            self.run(gap);
        }
    }

    /// Sends one datagram of `size` bytes on flow `f` from `side`, numbered per node and flow in
    /// its first three bytes.
    pub(crate) fn app(&mut self, side: Side, f: FlowId, size: usize) {
        let count = self.node(side).count.entry(f).or_default();
        let i = *count;
        *count += 1;
        let mut data = vec![0; size];
        for (b, v) in data.iter_mut().zip([i >> 16, i >> 8, i]) {
            *b = v as u8;
        }
        self.handle(side, Ev::App { flow: f, data: crate::pair::inner(f, &data) });
    }

    /// Runs the pair for `d` while the generators send.
    pub(crate) fn play(&mut self, d: Mono, gs: &mut [&mut Gen]) {
        let end = self.now + d;
        loop {
            for g in gs.iter_mut() {
                g.next = g.next.max(self.now);
            }
            // The earliest due, the first of equals, as Go's strict < picks.
            let Some(i) = (0..gs.len()).filter(|&i| gs[i].next <= end).min_by_key(|&i| gs[i].next) else { break };
            self.run(gs[i].next - self.now);
            let (side, f, size) = (gs[i].side, gs[i].f, gs[i].size);
            self.app(side, f, size);
            gs[i].next += gs[i].gap;
        }
        self.run(end - self.now);
    }

    /// A stream from the server: video at `mbps` in 1,400-byte datagrams (bulk), audio every 5 ms
    /// and input every 2 ms (small).
    pub(crate) fn moonlight(&self, mbps: f64) -> (Gen, Gen, Gen) {
        let g = |side, f, size, gap| Gen { side, f, size, gap, next: 0 };
        (
            g(Side::S, crate::FlowId(1), 1400, (1400.0 * 8.0 / mbps * 1e3) as Mono),
            g(Side::S, crate::FlowId(2), 300, 5 * crate::MS),
            g(Side::C, crate::FlowId(3), 100, 2 * crate::MS),
        )
    }
}

/// The number `Pair::app` wrote into a datagram.
fn num(f: FlowId, d: &[u8]) -> usize {
    match d {
        [a, b, c, ..] => usize::from(*a) << 16 | usize::from(*b) << 8 | usize::from(*c),
        _ => panic!("flow {:x}: a {}-byte datagram has no number", f.0, d.len()),
    }
}

/// Flow `f` arrived at `n` in order with datagrams 0 to want−1, at most `lost` missing; panics
/// otherwise.
pub(crate) fn in_order(n: &Tnode, f: FlowId, want: usize, lost: usize) {
    let mut last = None;
    for d in n.got.get(&f).into_iter().flatten() {
        let i = num(f, d);
        assert!(last.is_none_or(|l| i > l), "flow {:x}: datagram {i} after {}", f.0, last.unwrap_or(0));
        last = Some(i);
    }
    complete(n, f, want, lost);
}

/// Flow `f` arrived at `n` with datagrams 0 to want−1, each at most once and at most `lost`
/// missing, in any order; panics otherwise.
pub(crate) fn complete(n: &Tnode, f: FlowId, want: usize, lost: usize) {
    let mut seen = BTreeSet::new();
    for d in n.got.get(&f).into_iter().flatten() {
        let i = num(f, d);
        assert!(i < want && seen.insert(i), "flow {:x}: datagram {i} twice or unexpected", f.0);
    }
    let need = want.saturating_sub(lost);
    assert!(seen.len() >= need, "flow {:x}: got {} datagrams, want {want} (≤ {lost} lost)", f.0, seen.len());
}

// ---------------------------------------------------------------------------------------------
// Helpers of the unit tests

/// The first key message among `out`; panics if there is none.
pub(crate) fn key_msg(out: &[Act]) -> Msg {
    out.iter()
        .find_map(|a| match a {
            Act::Control(m) if m.t == "key" => Some(m.clone()),
            _ => None,
        })
        .expect("no key message")
}

/// A client and a server Session (seeds 11 and 12) that have exchanged keys and share a
/// Tailscale lane, and one packet the client sealed for the server (audio on 48000/1).
pub(crate) fn keyed() -> (Session, Session, Vec<u8>) {
    let mut c = Session::new(Role::Client, 11, DetRand::rand(11));
    let mut s = Session::new(Role::Anchor, 12, DetRand::rand(12));
    let hello = Msg { t: "hello".into(), ..Msg::default() };
    let ck = key_msg(&handle(&mut c, 0, Event::Control(&hello)));
    let sk = key_msg(&handle(&mut s, 0, Event::Control(&Msg { mode: MODE_PERFORMANCE.into(), ..hello })));
    handle(&mut s, 0, Event::Control(&ck));
    handle(&mut c, 0, Event::Control(&sk));
    handle(&mut c, 0, Event::Links { links: &[], mesh: Some(MeshLane { sock: 1, local: C_MESH, peer: S_MESH }) });
    handle(&mut s, 0, Event::Links { links: &[], mesh: Some(MeshLane { sock: 1, local: S_MESH, peer: C_MESH }) });
    let out = handle(&mut c, 0, Event::Send { packet: &crate::pair::inner(crate::FlowId(1), b"audio") });
    let pkt = out.into_iter().find_map(|a| match a {
        Act::Send { data, .. } => Some(data),
        _ => None,
    });
    (c, s, pkt.expect("no packet sent"))
}

/// The receiver's state, comparable. Go: `rxState`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RxState {
    pub(crate) keys: [(u64, [u8; 32], bool); 3],
    pub(crate) win: crate::wire::Window,
    /// The counter floor of the peer's key messages (R2-09).
    pub(crate) floor: u64,
}

pub(crate) fn rx_state(s: &Session) -> RxState {
    let k = |e: &crate::keys::EpochKey| (e.e, e.k, e.is_set());
    RxState { keys: [k(&s.rx.prev), k(&s.rx.cur), k(&s.rx.next)], win: s.rx.win.clone(), floor: s.rx.floor }
}

/// `b` as a lane packet from the client's mesh address on socket 1, at time 0.
pub(crate) fn recv(s: &mut Session, b: &[u8]) -> Vec<Act> {
    handle(s, 0, Event::Packet { sock: 1, from: C_MESH, data: b })
}

/// The deliveries among `out`.
pub(crate) fn delivered(out: &[Act]) -> Vec<(FlowId, Vec<u8>)> {
    out.iter()
        .filter_map(|a| match a {
            Act::Deliver { flow, data } => Some((*flow, data.clone())),
            _ => None,
        })
        .collect()
}

/// A packet for `s` with counter `ctr` and an arbitrary body.
pub(crate) fn forge(s: &Session, ctr: u64) -> Vec<u8> {
    let mut b = vec![0; HEADER_LEN + DATA_LEN + 20 + TAG_LEN];
    b[0] = TYPE_DATA;
    b[1..3].copy_from_slice(&s.rid.to_be_bytes());
    b[3..HEADER_LEN].copy_from_slice(&ctr.to_be_bytes());
    b
}

/// The owner's case: the Mac on hotel Wi-Fi behind NAT and an iPhone on USB, xanth on Ethernet
/// with a public address, and the Tailscale lane over the hotel Wi-Fi; started and run 100 ms.
pub(crate) fn hotel() -> Pair {
    let mut p = Pair::new(33 * crate::MS);
    p.add_link(Side::C, 1, "Wi-Fi", false, true, C_WIFI);
    p.add_link(Side::C, 2, "iPhone", true, false, C_PHONE);
    p.add_link(Side::S, 1, "Ethernet", false, false, S_ETH);
    p.nat.insert(C_WIFI, v4(203, 0, 113, 9, 61022));
    p.nat.insert(C_PHONE, v4(192, 0, 2, 77, 40000));
    p.link(C_WIFI, S_ETH, Path { delay: 32 * crate::MS, ..Path::default() });
    p.link(C_PHONE, S_ETH, Path { delay: 47 * crate::MS, ..Path::default() });
    p.start();
    p.run(100 * crate::MS); // datagrams before the peer's key are dropped
    p
}

/// Lane `id` of `n` is in state `st` (and validated if up); panics otherwise.
pub(crate) fn lane_state(n: &Tnode, id: u16, st: State) {
    let Some(l) = n.s.lane_by_id(id).map(|i| &n.s.lanes[i]) else {
        panic!("{:?} lane {id:#x}: want {}, have no such lane", n.role, st.name());
    };
    assert!(
        l.state == st && (st != State::Up || l.validated),
        "{:?} lane {id:#x}: want {}, have {} (validated {}, peer {:?}, claim {:?})",
        n.role,
        st.name(),
        l.state.name(),
        l.validated,
        l.peer,
        l.cand
    );
}

/// The harness itself, where it needs no Session to run: routing, NAT, paths, the control
/// channel, deliveries and the queue's order.
/// A test flow's inner packet (SPEC 10.3, §13): `data` as IPv4/UDP from 10.77.0.2 port `f.0` to
/// 10.77.0.1 port 9. The engine keys flows by the packet and numbers them itself; tests name a
/// flow by its UDP source port, which [`outer`] reads back from a delivered packet.
pub(crate) fn inner(f: FlowId, data: &[u8]) -> Vec<u8> {
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
pub(crate) fn outer(p: &[u8]) -> (FlowId, &[u8]) {
    match crate::flow::Key::parse(p) {
        Some(k) if k.proto == 17 && p.len() >= 28 && p[0] == 0x45 => (FlowId(k.sport), &p[28..]),
        _ => (FlowId(0), p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Mono = crate::MS;

    fn send(sock: SockId, to: SocketAddr, n: usize) -> Act {
        Act::Send { sock, to, data: vec![7; n] }
    }

    /// The hotel's devices and paths, not started.
    fn rooms() -> Pair {
        let mut p = Pair::new(33 * MS);
        p.add_link(Side::C, 1, "Wi-Fi", false, true, C_WIFI);
        p.add_link(Side::C, 2, "iPhone", true, false, C_PHONE);
        p.add_link(Side::S, 1, "Ethernet", false, false, S_ETH);
        p.nat.insert(C_WIFI, v4(203, 0, 113, 9, 61022));
        p.link(C_WIFI, S_ETH, Path { delay: 32 * MS, ..Path::default() });
        p
    }

    #[test]
    fn addresses() {
        let p = rooms();
        assert_eq!(
            (p.c.addr(1), p.c.addr(11), p.c.addr(12), p.c.addr(13)),
            (Some(C_MESH), Some(C_WIFI), Some(C_PHONE), None)
        );
        assert_eq!((p.s.sock(S_MESH), p.s.sock(S_ETH), p.s.sock(C_WIFI)), (1, 11, 0));
        let wifi = v4(203, 0, 113, 9, 61022);
        assert_eq!((p.route(wifi), p.route(C_WIFI), p.route(S_ETH)), (Some(C_WIFI), None, Some(S_ETH)));
        assert_eq!((p.public(C_WIFI), p.public(C_PHONE)), (wifi, C_PHONE));
        let infos = p.infos(Side::C);
        assert_eq!((infos[0].cands.clone(), infos[1].cands.clone()), (vec![C_WIFI, wifi], vec![C_PHONE]));
        assert!(infos[0].default && !infos[0].metered && infos[1].metered && infos[1].label == "iPhone");
    }

    #[test]
    fn carry() {
        let mut p = rooms();
        let wifi = v4(203, 0, 113, 9, 61022);
        p.now = 5 * MS;
        let f = crate::FlowId(1);
        let acts = vec![
            send(11, S_ETH, 100),  // NATed out: arrives from the mapping, after the path's delay
            send(11, S_MESH, 100), // no path from Wi-Fi to the mesh address: recorded, lost
            send(13, S_ETH, 100),  // a socket the node does not have
            Act::Control(Msg { t: "links".into(), ..Msg::default() }),
            Act::Deliver { flow: f, data: vec![1, 2, 3] },
        ];
        p.carry(Side::C, acts);
        assert_eq!((p.c.sent, p.wire.len(), p.q.len()), (3, 3, 2));
        assert_eq!(p.c.got[&f], vec![vec![1, 2, 3]]);
        assert!(matches!(p.wire[2].ev, Ev::Packet { from: NOWHERE, .. }));
        let first = p.q.pop().unwrap();
        assert!(matches!(first.ev, Ev::Packet { sock: 11, from, .. } if from == wifi));
        assert_eq!((first.at, first.to, first.dst), (37 * MS, Side::S, None));
        let ctl = p.q.pop().unwrap();
        assert!(matches!(ctl.ev, Ev::Control(ref m) if m.t == "links") && ctl.at == 38 * MS);
        // The server reaches the client only through its mapping.
        p.carry(Side::S, vec![send(11, C_WIFI, 50), send(11, wifi, 50)]);
        assert_eq!(p.q.len(), 1);
        assert!(matches!(p.q.pop().unwrap().ev, Ev::Packet { sock: 11, from: S_ETH, .. }));
        // A control channel that is down carries nothing.
        p.ctl_up = false;
        p.carry(Side::S, vec![Act::Control(Msg::default())]);
        assert!(p.q.is_empty());
    }

    #[test]
    fn rate_buffer_loss_down() {
        let mut p = rooms();
        let w = p.paths.get_mut(&WIFI_TO).unwrap();
        (w.rate, w.buf) = (8e6, 2 * MS); // 1 µs per byte on the wire
        p.carry(Side::C, (0..4).map(|_| send(11, S_ETH, 972)).collect()); // 1 ms each with 28 bytes of IP and UDP
        let at: Vec<_> = std::iter::from_fn(|| p.q.pop()).map(|it| it.at).collect();
        assert_eq!(at, [33 * MS, 34 * MS, 35 * MS]); // the fourth would wait 3 ms > buf
        p.paths.get_mut(&WIFI_TO).unwrap().down = true;
        p.carry(Side::C, vec![send(11, S_ETH, 10)]);
        assert!(p.q.is_empty());
        let w = p.paths.get_mut(&WIFI_TO).unwrap();
        (w.down, w.rate, w.loss) = (false, 0.0, 0.5);
        p.carry(Side::C, (0..1000).map(|_| send(11, S_ETH, 10)).collect());
        assert!((400..600).contains(&p.q.len()), "{} of 1000 arrived at 50% loss", p.q.len());
        assert_eq!(p.wire.len(), 1005);
    }

    #[test]
    fn queue_order() {
        let mut p = Pair::new(MS);
        p.push(5, Side::C, Ev::Tick);
        p.push(3, Side::S, Ev::Wake);
        p.push(5, Side::S, Ev::Tick);
        let order: Vec<_> = std::iter::from_fn(|| p.q.pop()).map(|it| (it.at, it.n, it.to)).collect();
        assert_eq!(order, [(3, 2, Side::S), (5, 1, Side::C), (5, 3, Side::S)]);
        // Nothing queued and no timers: run only moves the clock.
        p.run(10 * MS);
        assert_eq!(p.now, 10 * MS);
    }

    #[test]
    fn generators() {
        let p = Pair::new(MS);
        let (v, a, i) = p.moonlight(10.0);
        assert_eq!((v.gap, v.size, v.side, a.gap, i.gap, i.side), (1_120_000, 1400, Side::S, 5 * MS, 2 * MS, Side::C));
        let mut n = Tnode::new(Role::Client, 3);
        let f = crate::FlowId(1);
        n.got.insert(f, [0usize, 1, 3, 2].iter().map(|&i| vec![0, 0, i as u8, 9]).collect());
        complete(&n, f, 4, 0);
        complete(&n, f, 5, 1);
        let fails = |check: &dyn Fn()| std::panic::catch_unwind(std::panic::AssertUnwindSafe(check)).is_err();
        assert!(fails(&|| in_order(&n, f, 4, 0)), "out of order");
        assert!(fails(&|| complete(&n, f, 3, 0)), "unexpected datagram");
        assert!(fails(&|| complete(&n, f, 6, 1)), "too many lost");
    }

    #[test]
    fn arena() {
        let mut a = Arena::with_capacity(8, 4096);
        let f = crate::FlowId(2);
        let p = crate::pair::inner(f, &[3]);
        let before = allocs();
        a.send(3, C_MESH, &[1, 2]);
        a.deliver(&p);
        a.send(4, S_MESH, &[]);
        assert_eq!((allocs() - before, a.len()), (0, 3));
        a.control(Msg::default());
        let acts = a.take();
        assert!(a.is_empty());
        assert_eq!(acts[0], Act::Send { sock: 3, to: C_MESH, data: vec![1, 2] });
        assert_eq!(acts[1], Act::Deliver { flow: f, data: vec![3] });
        assert_eq!(acts[2], Act::Send { sock: 4, to: S_MESH, data: vec![] });
        assert_eq!(acts[3], Act::Control(Msg::default()));
        let v: Vec<u8> = Vec::with_capacity(1);
        assert!(allocs() > before);
        drop(v);
    }
}
