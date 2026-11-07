//! The event engine (Go netsim/sim.go): one seeded random source, one clock and an event queue
//! ordered by (time, sequence number), so a seed always replays the same run. Everything runs on
//! one thread; an endpoint's `handle` is never re-entered, because every reaction is a new event.
//! Also here: the [`Endpoint`] trait that `wirebond_core::Session` satisfies, the action recorder
//! a device hands to `handle`, the PRNG, the CRC, the hooks and the event-log hash.
//!
//! Owner: N1 (with link.rs, nat.rs, trace.rs).

use std::any::Any;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::binary_heap::PeekMut;
use std::fmt::Display;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use sha2::{Digest, Sha256};
use wirebond_core::{Event, Mono, Msg, Out, Rand, RidTable, Role, Session, SessionStatus, SockId};

use crate::apps::{Apps, PORT_AUDIO, PORT_CONTROL, PORT_VIDEO, Registry};
use crate::check::{Adversary, Checker};
use crate::link::{DirId, Link, LinkId};
use crate::metrics::Recorder;
use crate::node::{CtlChan, DevId, Device};

// ---------------------------------------------------------------------------------------------
// Simulated time

/// Simulated time and durations, in nanoseconds (Go's time.Duration). World time starts at 0; a
/// device's monotonic clock ([`wirebond_core::Mono`]) is world time plus the device's offset.
pub type Dur = i64;

pub use wirebond_core::{MIN, MS, SEC};
pub const NS: Dur = 1;
pub const US: Dur = 1_000;
pub const HOUR: Dur = 60 * MIN;

/// The default one-way delay between the anchor and the internet server ([`World::egress`]).
pub const EGRESS: Dur = 5 * MS;

/// The golden-ratio increment of splitmix64; Go's World seeds its PCG with seed ^ GOLDEN.
pub const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// Formats d as Go's %v of a time.Duration does closely enough for failure messages ("12.345s").
pub fn fmt_dur(d: Dur) -> String {
    format!("{:.3}s", d as f64 / 1e9)
}

/// d in seconds, computed as Go's Duration.Seconds is (whole seconds plus the remainder), so the
/// rates and limits the models derive from durations round as Go's did.
pub(crate) fn secs(d: Dur) -> f64 {
    (d / SEC) as f64 + (d % SEC) as f64 / 1e9
}

/// Lower-case hex of b (Go's %x of a byte slice).
pub(crate) fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::with_capacity(2 * b.len()), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

// ---------------------------------------------------------------------------------------------
// Applications' flows and the engine's inner packets (SPEC §10.3, §13)

/// A simulated application's flow, as the simulator names it: the far end's port in the high 16
/// bits and the client's port in the low 16. The engine never sees this name: an application
/// datagram travels as one real inner IP packet ([`inner`]) between the client's bond address and
/// the far end, which is either the anchor itself (Moonlight on the anchor, §6.1) or a server on
/// the internet behind it (the anchor's egress, §6.2), and the engine keys the flow from the
/// packet's addresses and ports itself (§13). [`outer`] turns a delivered packet back into the
/// flow and the datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct FlowId(pub u32);

impl FlowId {
    pub const fn new(port: u16, num: u16) -> FlowId {
        FlowId((port as u32) << 16 | num as u32)
    }
    /// The far end's port.
    pub const fn port(self) -> u16 {
        (self.0 >> 16) as u16
    }
    /// The client's port.
    pub const fn num(self) -> u16 {
        self.0 as u16
    }
}

/// The bond addresses (§2): the client's and the anchor's, IPv4 and IPv6.
pub const CLIENT4: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 2);
pub const ANCHOR4: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
pub const CLIENT6: Ipv6Addr = Ipv6Addr::new(0xfd77, 0xb0d, 0, 0, 0, 0, 0, 2);
pub const ANCHOR6: Ipv6Addr = Ipv6Addr::new(0xfd77, 0xb0d, 0, 0, 0, 0, 0, 1);
/// The "internet" server behind the anchor (§6.2; tier 3's server, §23.1), IPv4 and IPv6.
pub const SERVER4: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 10);
pub const SERVER6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x10);

/// IP protocol numbers.
pub const TCP: u8 = 6;
pub const UDP: u8 = 17;

/// How one application flow travels, fixed by its far end's port (Moonlight's own ports as
/// Sunshine uses them, the rest as the scenarios use them):
///
/// | Port | Flow | Transport | Far end |
/// | --- | --- | --- | --- |
/// | 47998 | Moonlight video | UDP over IPv4 | the anchor, 10.77.0.1 |
/// | 47999 | Moonlight control and input | UDP over IPv6 | the anchor, fd77:b0d::1 |
/// | 48000 | Moonlight audio | UDP over IPv6 | the anchor, fd77:b0d::1 |
/// | 9000–9099 | saturating UDP ([`World::flood`]) | UDP over IPv6 | the internet server |
/// | 9100–9199 | TCP-like bulk ([`World::bulk`]) | TCP over IPv4 | the internet server |
/// | 9200–9299 | TCP-like bulk | TCP over IPv6 | the internet server |
/// | any other | echo loads and tests | UDP over IPv4 | the anchor |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Path {
    /// TCP (true) or UDP.
    pub tcp: bool,
    pub v6: bool,
    /// The far end is the internet server behind the anchor, not the anchor.
    pub internet: bool,
}

/// The [`Path`] of a flow whose far end listens on `port`.
pub const fn path(port: u16) -> Path {
    match port {
        47999 | 48000 => Path { tcp: false, v6: true, internet: false },
        9000..=9099 => Path { tcp: false, v6: true, internet: true },
        9100..=9199 => Path { tcp: true, v6: false, internet: true },
        9200..=9299 => Path { tcp: true, v6: true, internet: true },
        _ => Path { tcp: false, v6: false, internet: false },
    }
}

/// The IP and transport headers [`inner`] puts in front of a datagram of a flow on `port`: 28
/// (IPv4/UDP), 48 (IPv6/UDP), 40 (IPv4/TCP) or 60 (IPv6/TCP).
pub const fn inner_hdr(port: u16) -> usize {
    let p = path(port);
    (if p.v6 { 40 } else { 20 }) + if p.tcp { 20 } else { 8 }
}

/// The IPv4/UDP headers of the video flow (the frame-time model's packet size).
pub const INNER_HDR: usize = inner_hdr(47998);

/// The far end's address of a flow on `port`.
pub const fn far(port: u16) -> IpAddr {
    match path(port) {
        Path { v6: false, internet: false, .. } => IpAddr::V4(ANCHOR4),
        Path { v6: true, internet: false, .. } => IpAddr::V6(ANCHOR6),
        Path { v6: false, internet: true, .. } => IpAddr::V4(SERVER4),
        Path { v6: true, internet: true, .. } => IpAddr::V6(SERVER6),
    }
}

/// The client's bond address in the family of a flow on `port`.
pub const fn near(port: u16) -> IpAddr {
    if path(port).v6 { IpAddr::V6(CLIENT6) } else { IpAddr::V4(CLIENT4) }
}

/// Application datagram `data` of flow `f` as the whole inner IP packet the sending side's TUN
/// device reads (§4.3): from the client's bond address and port `f.num()` to the far end's address
/// and port `f.port()` when the client sends (`from` [`Role::Client`]); the reverse when the
/// anchor sends, its own reply or the internet server's after the anchor's NAT turned the
/// destination back into the client's bond address. UDP or TCP over IPv4 or IPv6 as [`path`]
/// says; the IPv4 header checksum is set, the transport checksum left 0 (nothing on the simulated
/// path checks it).
pub fn inner(from: Role, f: FlowId, data: &[u8]) -> Vec<u8> {
    let p = path(f.port());
    let (mut src, mut dst) = (near(f.port()), far(f.port()));
    let (mut sport, mut dport) = (f.num(), f.port());
    if from == Role::Anchor {
        (src, dst, sport, dport) = (dst, src, dport, sport);
    }
    let l4 = if p.tcp { 20 } else { 8 } + data.len();
    let proto = if p.tcp { TCP } else { UDP };
    let mut b = Vec::with_capacity(inner_hdr(f.port()) + data.len());
    match (src, dst) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            b.extend_from_slice(&[0x45, 0]);
            b.extend_from_slice(&((20 + l4) as u16).to_be_bytes());
            b.extend_from_slice(&[0, 0, 0x40, 0, 64, proto, 0, 0]); // DF, TTL 64
            b.extend_from_slice(&s.octets());
            b.extend_from_slice(&d.octets());
            let mut sum = b.chunks(2).fold(0u32, |a, w| a + u32::from(u16::from_be_bytes([w[0], w[1]])));
            while sum > 0xffff {
                sum = (sum & 0xffff) + (sum >> 16);
            }
            b[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
        }
        (s, d) => {
            b.extend_from_slice(&[0x60, 0, 0, 0]);
            b.extend_from_slice(&(l4 as u16).to_be_bytes());
            b.extend_from_slice(&[proto, 64]);
            for a in [s, d] {
                if let IpAddr::V6(a) = a {
                    b.extend_from_slice(&a.octets());
                }
            }
        }
    }
    b.extend_from_slice(&sport.to_be_bytes());
    b.extend_from_slice(&dport.to_be_bytes());
    if p.tcp {
        // seq: the datagram's own sequence number (its tag's); ack 0; a 20-byte header, PSH|ACK.
        let seq = data.get(8..12).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
        b.extend_from_slice(&seq.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 0, 0x50, 0x18, 0xff, 0xff, 0, 0, 0, 0]);
    } else {
        b.extend_from_slice(&(l4 as u16).to_be_bytes());
        b.extend_from_slice(&[0, 0]);
    }
    b.extend_from_slice(data);
    b
}

/// One inner packet [`outer`] read: its flow (named by the far end's port and the client's port,
/// whichever way it travels), its source and destination addresses and the datagram it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inner<'a> {
    pub flow: FlowId,
    pub src: IpAddr,
    pub dst: IpAddr,
    pub data: &'a [u8],
}

/// Reads an inner UDP or TCP packet over IPv4 or IPv6 (no IPv4 options and no IPv6 extension
/// headers, which [`inner`] never makes); None for anything else, an IPv4 fragment included
/// ([`World::reassemble`] makes it whole first). The side whose address is a client bond address
/// names the low half of the flow.
pub fn outer(p: &[u8]) -> Option<Inner<'_>> {
    let (src, dst, proto, at): (IpAddr, IpAddr, u8, usize) = match p.first()? >> 4 {
        4 if p.len() >= 20 && p[0] == 0x45 && !is_fragment(p) => {
            let s = <[u8; 4]>::try_from(&p[12..16]).ok()?;
            let d = <[u8; 4]>::try_from(&p[16..20]).ok()?;
            (s.into(), d.into(), p[9], 20)
        }
        6 if p.len() >= 40 => {
            let s = <[u8; 16]>::try_from(&p[8..24]).ok()?;
            let d = <[u8; 16]>::try_from(&p[24..40]).ok()?;
            (s.into(), d.into(), p[6], 40)
        }
        _ => return None,
    };
    let hdr = match proto {
        UDP => 8,
        TCP => usize::from(p.get(at + 12)? >> 4) * 4,
        _ => return None,
    };
    let data = p.get(at + hdr.max(4)..)?;
    let (sport, dport) = (u16::from_be_bytes([p[at], p[at + 1]]), u16::from_be_bytes([p[at + 2], p[at + 3]]));
    let flow = if is_client(src) { FlowId::new(dport, sport) } else { FlowId::new(sport, dport) };
    Some(Inner { flow, src, dst, data })
}

/// Whether `p` is an IPv4 fragment: MF set or a non-zero offset.
pub fn is_fragment(p: &[u8]) -> bool {
    p.len() >= 20 && p[0] >> 4 == 4 && u16::from_be_bytes([p[6], p[7]]) & 0x3fff != 0
}

/// Sets an IPv4 header's checksum (its first 20 bytes; [`inner`] makes no options).
fn ip4_sum(h: &mut [u8]) {
    h[10..12].fill(0);
    let mut sum = h[..20].chunks(2).fold(0u32, |a, w| a + u32::from(u16::from_be_bytes([w[0], w[1]])));
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    h[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
}

/// The fragments a kernel makes of IPv4 packet `p` for a device MTU of `mtu` (RFC 791), with IP
/// id `id`: DF cleared, every fragment but the last with MF set and a payload a multiple of 8
/// bytes. `p` whole when it fits, or is not IPv4 without options.
pub fn fragment(p: &[u8], mtu: usize, id: u16) -> Vec<Vec<u8>> {
    if p.len() <= mtu || p.len() < 20 || p[0] != 0x45 || mtu < 28 {
        return vec![p.to_vec()];
    }
    let step = (mtu - 20) & !7;
    let body = &p[20..];
    let mut out = Vec::with_capacity(body.len().div_ceil(step));
    for (i, c) in body.chunks(step).enumerate() {
        let mut f = p[..20].to_vec();
        f[2..4].copy_from_slice(&((20 + c.len()) as u16).to_be_bytes());
        f[4..6].copy_from_slice(&id.to_be_bytes());
        let more = if (i + 1) * step < body.len() { 0x2000 } else { 0 };
        f[6..8].copy_from_slice(&((more | (i * step / 8)) as u16).to_be_bytes());
        ip4_sum(&mut f);
        f.extend_from_slice(c);
        out.push(f);
    }
    out
}

/// IPv4 fragments a device holds while the rest of their datagram is on its way, at most this
/// many datagrams; past it the oldest is given up and counted in [`World::reasm_fails`], as a
/// kernel's reassembly queue limit and timeout do.
pub const REASM_MAX: usize = 256;

/// One datagram a device is reassembling: the device, its source, destination, id and protocol
/// (the IPv4 header's bytes 12-20, 4-6 and 9), and its fragments as (offset, MF, packet).
pub(crate) type Reasm = (usize, [u8; 11], Vec<(usize, bool, Vec<u8>)>);

/// Whether a is one of the client's bond addresses.
pub fn is_client(a: IpAddr) -> bool {
    a == IpAddr::V4(CLIENT4) || a == IpAddr::V6(CLIENT6)
}

/// Whether a is one of the anchor's bond addresses.
pub fn is_anchor(a: IpAddr) -> bool {
    a == IpAddr::V4(ANCHOR4) || a == IpAddr::V6(ANCHOR6)
}

// ---------------------------------------------------------------------------------------------
// The endpoint netsim drives

/// The part of core's API that netsim drives (Go's Endpoint). `wirebond_core::Session` satisfies
/// it; so does the test fake ([`crate::fake::Fake`]), which proves the simulator's plumbing
/// without core. `Any` lets the stand-in tell a core Session (which owns receive ids in the
/// device's [`RidTable`]) from the fake, as Go's type assertion did, and lets tests downcast to the
/// fake (`(ep as &dyn Any).downcast_ref::<Fake>()`).
pub trait Endpoint: Any {
    /// Applies one event at monotonic time `now`, calling `out` for each resulting action.
    fn handle(&mut self, now: Mono, ev: Event<'_>, rids: &mut RidTable, out: &mut dyn Out);
    /// The earliest time `handle` wants an `Event::Tick`; `Mono::MAX` when nothing is due.
    fn next(&self) -> Mono;
    /// The read-only view the checker and the metrics read.
    fn snapshot(&self) -> SessionStatus;
    /// Frees the endpoint's receive ids.
    fn close(&mut self, rids: &mut RidTable);
}

impl Endpoint for Session {
    fn handle(&mut self, now: Mono, ev: Event<'_>, rids: &mut RidTable, out: &mut dyn Out) {
        Session::handle(self, now, ev, rids, out)
    }
    fn next(&self) -> Mono {
        Session::next(self)
    }
    fn snapshot(&self) -> SessionStatus {
        Session::snapshot(self)
    }
    fn close(&mut self, rids: &mut RidTable) {
        Session::close(self, rids)
    }
}

/// Makes an endpoint the way `Session::new` does (Go's Maker): the role, the owner key the device
/// files the session under in its [`RidTable`], and the session's source of key bytes.
pub type Maker = Box<dyn Fn(Role, u64, Rand) -> Box<dyn Endpoint>>;

/// Makes real core Sessions (Go's Core).
pub fn core_maker() -> Maker {
    Box::new(|role, owner, rand| Box::new(Session::new(role, owner, rand)) as Box<dyn Endpoint>)
}

/// A key source for one Session from a seeded generator (Go's keyReader).
pub fn key_reader(mut r: Rng) -> Rand {
    Box::new(move |b: &mut [u8]| r.fill(b))
}

/// One action an endpoint emitted during one `handle` call (Go's core.Action). The device records
/// them while `handle` runs and carries them out after it returns, so `handle` is never
/// re-entered; the checker reads the whole list first.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Send `data` from socket `sock` to `to` (Go ActSend).
    Send { sock: SockId, to: SocketAddr, data: Vec<u8> },
    /// Deliver the inner packet whose datagram is `data`, on flow `flow`, from `src` to `dst`
    /// (Go ActDeliver; [`outer`] read it: flow 0, no addresses and the whole packet if it could
    /// not).
    Deliver { flow: FlowId, src: Option<IpAddr>, dst: Option<IpAddr>, data: Vec<u8> },
    /// Send the message over the control channel (Go ActControl).
    Control(Msg),
}

/// The [`Out`] a device passes to `Endpoint::handle`: it records the actions in order (Go's
/// `out []core.Action`). Owned by the device and reused across calls.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Actions(pub Vec<Action>);

impl Out for Actions {
    fn send(&mut self, sock: SockId, to: SocketAddr, data: &[u8]) {
        self.0.push(Action::Send { sock, to, data: data.to_vec() });
    }
    fn deliver(&mut self, packet: &[u8]) {
        let a = match outer(packet) {
            Some(i) => Action::Deliver { flow: i.flow, src: Some(i.src), dst: Some(i.dst), data: i.data.to_vec() },
            None => Action::Deliver { flow: FlowId(0), src: None, dst: None, data: packet.to_vec() },
        };
        self.0.push(a);
    }
    fn control(&mut self, msg: Msg) {
        self.0.push(Action::Control(msg));
    }
}

// ---------------------------------------------------------------------------------------------
// Randomness (no rand crate): xoshiro256** seeded through splitmix64

/// A seeded random source (Go's *rand.Rand over PCG): xoshiro256**. Every model draws from its own
/// `Rng`, split off the world's with [`World::sub`], so adding a model never shifts another's
/// draws. Values differ from Go's PCG: Go and Rust event-log hashes are not comparable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// A generator from two seed words (Go's rand.NewPCG(a, b)): every output depends on both. It
    /// is [`Rng::seeded`]'s state with the first output spent, since that one depends on b alone
    /// (`Rng::new(seed, 0x5a5a)` would otherwise start alike for every seed).
    pub fn new(a: u64, b: u64) -> Rng {
        let mut r = Rng::seeded(a, b);
        r.u64();
        r
    }

    /// The state the two words give, whose first output depends on b alone. Only callers whose b
    /// varies use it directly: [`World::new`], whose b is the seed, and [`Rng::split`], whose words
    /// are random draws.
    fn seeded(a: u64, b: u64) -> Rng {
        let mut s = [splitmix(a), splitmix(b ^ GOLDEN), splitmix(b), splitmix(a ^ splitmix(b))];
        if s == [0; 4] {
            s[0] = GOLDEN;
        }
        Rng { s }
    }

    /// A generator in exactly this state (for the reference vectors in the tests).
    pub fn from_state(s: [u64; 4]) -> Rng {
        Rng { s }
    }

    /// The next 64 random bits (Go Uint64).
    pub fn u64(&mut self) -> u64 {
        let s = &mut self.s;
        let r = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        r
    }

    /// The next 32 random bits (Go Uint32).
    pub fn u32(&mut self) -> u32 {
        (self.u64() >> 32) as u32
    }

    /// Uniform in [0, n), n > 0 (Go IntN, Int64N, Uint64N), without modulo bias (Lemire).
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below(0)");
        let mut m = self.u64() as u128 * n as u128;
        if (m as u64) < n {
            let t = n.wrapping_neg() % n;
            while (m as u64) < t {
                m = self.u64() as u128 * n as u128;
            }
        }
        (m >> 64) as u64
    }

    /// Uniform in [0, n) as a duration, n > 0 (Go time.Duration(r.Int64N(int64(n)))).
    pub fn dur(&mut self, n: Dur) -> Dur {
        self.below(n as u64) as Dur
    }

    /// Uniform in [0, 1) (Go Float64).
    pub fn f64(&mut self) -> f64 {
        (self.u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Exponential with rate 1 (Go ExpFloat64).
    pub fn exp(&mut self) -> f64 {
        -(1.0 - self.f64()).ln()
    }

    /// Fills b with random bytes (Go's keyReader.Read).
    pub fn fill(&mut self, b: &mut [u8]) {
        for c in b.chunks_mut(8) {
            let x = self.u64().to_le_bytes();
            c.copy_from_slice(&x[..c.len()]);
        }
    }

    /// A generator of its own, seeded from two draws of this one, as Go's
    /// `rand.New(rand.NewPCG(r.Uint64(), r.Uint64()))`: [`World::sub`], and each bump train's own.
    pub(crate) fn split(&mut self) -> Rng {
        let a = self.u64();
        let b = self.u64();
        Rng::seeded(a, b)
    }
}

/// splitmix64's output for state x: a stateless hash for draws that must be a function of time,
/// such as small jitter (Go's splitmix).
pub fn splitmix(x: u64) -> u64 {
    let mut x = x.wrapping_add(GOLDEN);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Slicing-by-8 tables for CRC-32C: `CRC_TABLES[k][i]` is the CRC of byte i followed by k zero
/// bytes, so eight bytes cost eight lookups that do not wait on each other.
static CRC_TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0x82f6_3b78 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut k = 1;
        while k < 8 {
            let p = t[k - 1][i];
            t[k][i] = (p >> 8) ^ t[0][(p & 0xff) as usize];
            k += 1;
        }
        i += 1;
    }
    t
};

/// CRC-32C (Castagnoli) of b (Go's crc): the registry's datagram check, the checker's seal ring
/// and the log's digest. It runs over every datagram several times, and Go's used the CPU's CRC
/// instruction; slicing by 8 is about 4 times faster than a byte at a time (the instruction
/// needs unsafe code, which this crate forbids).
pub fn crc(b: &[u8]) -> u32 {
    let t = &CRC_TABLES;
    let mut c = !0u32;
    let (words, rest) = b.as_chunks::<8>();
    for w in words {
        let x = u64::from_le_bytes(*w);
        let lo = c ^ x as u32;
        let hi = (x >> 32) as u32;
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][(lo >> 8 & 0xff) as usize]
            ^ t[5][(lo >> 16 & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xff) as usize]
            ^ t[2][(hi >> 8 & 0xff) as usize]
            ^ t[1][(hi >> 16 & 0xff) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &x in rest {
        c = t[0][((c ^ x as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

// ---------------------------------------------------------------------------------------------
// Hooks (Go's World.Hook and the tests' hook(w, f))

/// What a hook sees (Go's kind byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookKind {
    /// 'S': a lane packet a Session sent, with its link (None for the Tailscale lane).
    Sent,
    /// 'I': a server packet entering a client link (None: the Tailscale lane, at the server).
    Inbound,
    /// 'R': a genuine lane packet the link model delivered to the peer, with the sender's link.
    Received,
    /// 'D': an app datagram delivered.
    Delivered,
}

/// One hook call's arguments.
#[derive(Clone, Copy, Debug)]
pub struct HookArgs<'a> {
    pub kind: HookKind,
    /// The device that sent (S, I), received (R) or delivered (D).
    pub dev: DevId,
    pub link: Option<LinkId>,
    pub data: &'a [u8],
    /// For `Sent`: the event whose actions the device is carrying out (Go's Device.Event, which
    /// A1b's input metric reads). None for the other kinds.
    pub event: Option<Event<'a>>,
}

/// A hook: sees every lane packet sent, every server packet entering a client link, every genuine
/// lane packet delivered by the link model and every app datagram delivered, for scenario metrics.
/// It gets the whole world (taken out of it while it runs; see [`World::fire`]).
pub type Hook = Box<dyn FnMut(&mut World, &HookArgs<'_>)>;

// ---------------------------------------------------------------------------------------------
// The event queue

/// One queued event: ordered by (at, seq), earliest first.
pub(crate) struct Entry {
    pub(crate) at: Dur,
    pub(crate) seq: u64,
    pub(crate) task: Task,
}

/// What an event does. `Call` is Go's func(); the others are the hot paths (two per packet on a
/// shaped direction, one per periodic tick, one per Session Tick), kept free of a closure
/// allocation. Every variant is small: a heap entry is 48 bytes.
pub(crate) enum Task {
    Call(Box<dyn FnOnce(&mut World)>),
    /// The first packet in flight on a direction arrives: run its continuation (Go's closure in
    /// Dir.depart). A direction never reorders, so its packets in flight are in (time, sequence)
    /// order in its own queue ([`crate::link::Dir`]'s `flight`) and only the first is in the heap,
    /// under the time and sequence number it drew when it departed: the events run in exactly
    /// the order they would with one heap entry per packet.
    Arrive(DirId),
    /// The head of a direction's queue leaves its bottleneck (Go's closure in Dir.serve).
    Serve(DirId),
    /// One tick of [`World::every`]: runs f, and while it returns true queues itself again a
    /// period later, moving the same box (Go's tick closure, which allocated per tick).
    Every(Dur, Box<dyn FnMut(&mut World) -> bool>),
    /// A device's Session Tick that `World::sched_tick` queued with this generation (Go's closure
    /// in schedTick): `World::tick_due` runs it unless a newer schedule replaced it.
    Tick(DevId, u64),
}

impl PartialEq for Entry {
    fn eq(&self, o: &Entry) -> bool {
        self.at == o.at && self.seq == o.seq
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Entry) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Entry {
    /// Reversed, because BinaryHeap pops the greatest.
    fn cmp(&self, o: &Entry) -> Ordering {
        (o.at, o.seq).cmp(&(self.at, self.seq))
    }
}

// ---------------------------------------------------------------------------------------------
// The world

/// The event log hashes its 32-byte records in chunks of this many bytes (Go's 1 << 14).
const LOG_CHUNK: usize = 1 << 14;

/// One simulation (Go's World): one seeded random source, one clock, and an event queue ordered by
/// (time, sequence number), so a seed always replays the same run. It owns every device, link,
/// model and check; ids index into it (`world[DevId::Client]`, `world[link_id]`, `world[dir_id]`).
pub struct World {
    /// The Mac, then xanth (Go's Client and Server): index with [`DevId`].
    pub devs: [Device; 2],
    /// Safety invariants after every event (Go Check).
    pub check: bool,
    /// Wall-clock limit for one `run`, so a runaway core cannot hang a test (Go Wall).
    pub wall: std::time::Duration,
    /// Tailscale-lane packets are dropped (Go MeshDown).
    pub mesh_down: bool,
    /// The baseline of SPEC §1 (P1) and §23.3 (A11): the client's node offers no link, so the
    /// bond has the mesh lane alone, riding the client's default link (or [`World::mesh_link`]).
    /// Set before the world starts.
    pub mesh_only: bool,
    /// Links pulled out ([`World::pull`]).
    pub pulled: Vec<LinkId>,
    /// The Tailscale lane's own path ([`World::mesh_path`]); None: it rides the client's default
    /// link (§17.2).
    pub mesh_link: Option<LinkId>,
    /// The on-path adversary, if a scenario made one (Go Adv).
    pub adv: Option<Adversary>,
    /// The traffic models' state: index with [`crate::MoonId`], [`crate::FloodId`], [`crate::BulkId`].
    pub apps: Apps,
    /// The acceptance criteria's sampler, once [`crate::metrics::record`] started it.
    pub rec: Option<Recorder>,
    /// One way between the anchor and the internet server behind it (§6.2): what a flow whose
    /// [`path`] ends on the internet adds each way to what the bond takes.
    pub egress: Dur,
    /// Inner packets the anchor dropped by §10.5 rule 6 and §6.3 (a source that is not the
    /// client's bond address, or one client bond address to another) and the client dropped
    /// because they were not addressed to its bond address.
    pub ip_dropped: u64,
    /// Both TUN devices' MTU (§5.1): an IPv4 inner packet longer than this is fragmented by the
    /// sending side's kernel ([`fragment`]) and reassembled by the receiving side's
    /// ([`World::reassemble`]); 0 (the default): every inner packet is read whole.
    pub tun_mtu: usize,
    /// Datagrams whose fragments were given up incomplete (Linux's `Ip.ReasmFails`).
    pub reasm_fails: u64,
    /// Datagrams being reassembled, oldest first.
    pub(crate) reasm: Vec<Reasm>,
    /// The IP id of the next datagram [`fragment`] splits.
    pub(crate) ip_id: u16,
    /// Hooks in the order added (Go's chained Hook).
    pub(crate) hooks: Vec<Hook>,
    pub(crate) now: Dur,
    pub(crate) seq: u64,
    pub(crate) q: BinaryHeap<Entry>,
    pub(crate) rng: Rng,
    pub(crate) mk: Maker,
    pub(crate) ctl: CtlChan,
    pub(crate) reg: Registry,
    pub(crate) chk: Checker,
    /// Every link of both devices; [`LinkId`] indexes it (Go's w.links).
    pub(crate) links: Vec<Link>,
    /// Session pairings (self inst, peer inst); pair 0 is "no session".
    pub(crate) pairs: Vec<[u64; 2]>,
    /// Handle calls so far: the order invariant is per call.
    pub(crate) calls: u64,
    /// Carrying out a handle call's actions: programs' sends wait for the next event.
    pub(crate) busy: bool,
    pub(crate) log: Sha256,
    pub(crate) buf: Vec<u8>,
    /// The first failure (Go's fail string; None for "").
    pub(crate) fail: Option<String>,
    /// Events run so far.
    pub(crate) n: u64,
}

impl World {
    /// An empty world for seed whose sessions come from mk (Go New): the Mac and xanth with their
    /// mesh addresses, xanth sharing Moonlight's ports, no links yet.
    pub fn new(seed: u64, mk: Maker) -> World {
        let mut rng = Rng::seeded(seed, seed ^ GOLDEN);
        let client = Device::new("mac", Role::Client, "100.101.1.2:7849".parse().unwrap(), &mut rng);
        let mut server = Device::new("xanth", Role::Anchor, "100.101.9.9:7849".parse().unwrap(), &mut rng);
        server.shared = vec![47984, 47989, PORT_VIDEO, PORT_CONTROL, PORT_AUDIO, 48010];
        World {
            devs: [client, server],
            check: true,
            wall: std::time::Duration::from_secs(600),
            mesh_down: false,
            mesh_only: false,
            pulled: Vec::new(),
            mesh_link: None,
            adv: None,
            apps: Apps::default(),
            rec: None,
            egress: EGRESS,
            ip_dropped: 0,
            tun_mtu: 0,
            reasm_fails: 0,
            reasm: Vec::new(),
            ip_id: 0,
            hooks: Vec::new(),
            now: 0,
            seq: 0,
            q: BinaryHeap::new(),
            rng,
            mk,
            ctl: CtlChan::default(),
            reg: Registry::default(),
            chk: Checker::default(),
            links: Vec::new(),
            pairs: vec![[0, 0]],
            calls: 0,
            busy: false,
            log: Sha256::new(),
            buf: Vec::new(),
            fail: None,
            n: 0,
        }
    }

    /// The simulated time since the start (Go Now).
    pub fn now(&self) -> Dur {
        self.now
    }

    /// Events run so far (Go's w.n, which a self-test logs).
    pub fn events(&self) -> u64 {
        self.n
    }

    /// A random source of its own, drawn from the world's, so adding a model never shifts the
    /// draws of another (Go sub).
    pub fn sub(&mut self) -> Rng {
        self.rng.split()
    }

    /// Runs f at simulated time t (now, if t is past) (Go At).
    pub fn at(&mut self, t: Dur, f: impl FnOnce(&mut World) + 'static) {
        self.push(t, Task::Call(Box::new(f)));
    }

    /// Queues a task at t (now, if t is past) behind every task already queued for t.
    pub(crate) fn push(&mut self, t: Dur, task: Task) {
        self.seq += 1;
        self.q.push(Entry { at: t.max(self.now), seq: self.seq, task });
    }

    /// Runs f every period from start while it returns true (Go Every).
    pub fn every(&mut self, start: Dur, period: Dur, f: impl FnMut(&mut World) -> bool + 'static) {
        self.push(start, Task::Every(period, Box::new(f)));
    }

    /// Advances the world to until, or to the first failure, which it returns (Go Run). It fails
    /// on more than 600,000 events per simulated second (a runaway Session) and when the run takes
    /// more than `wall` of wall-clock time (checked every 4,096 events). On success the clock
    /// stands at until (Go set it there even when until was past; the clock never goes back).
    pub fn run(&mut self, until: Dur) -> Result<(), String> {
        let span = until.saturating_sub(self.now) / SEC;
        let budget = (self.n as i64).saturating_add(span.saturating_mul(600_000)).saturating_add(1_000_000);
        // The wall clock only stops a runaway run; it never decides anything else.
        let start = std::time::Instant::now();
        while self.fail.is_none() {
            let Some(top) = self.q.peek_mut() else { break };
            if top.at > until {
                break;
            }
            let e = PeekMut::pop(top);
            self.now = e.at;
            self.exec(e.task);
            self.n += 1;
            if self.n as i64 > budget {
                self.fail("more than 600,000 events per simulated second: a runaway Session?");
            }
            if self.n & 0xfff == 0 && start.elapsed() > self.wall {
                let wall = self.wall;
                self.fail(format_args!("the run took more than {wall:?} of wall time"));
            }
        }
        if let Some(f) = &self.fail {
            return Err(f.clone());
        }
        self.now = self.now.max(until);
        Ok(())
    }

    /// Runs one event.
    fn exec(&mut self, task: Task) {
        match task {
            Task::Call(f) => f(self),
            Task::Arrive(d) => self.dir_arrive(d),
            Task::Serve(d) => self.dir_leave(d),
            Task::Every(period, mut f) => {
                if f(self) {
                    let t = self.now + period;
                    self.push(t, Task::Every(period, f));
                }
            }
            Task::Tick(d, g) => self.tick_due(d, g),
        }
    }

    /// Records the first failure, prefixed with the time; `run` stops after the current event
    /// (Go Fail(format, args...): callers pass `format_args!(...)` or a String).
    pub fn fail(&mut self, msg: impl Display) {
        if self.fail.is_none() {
            self.fail = Some(format!("at {}: {msg}", fmt_dur(self.now)));
        }
    }

    /// The first failure, if any (Go Failed, which returned "" for none).
    pub fn failed(&self) -> Option<&str> {
        self.fail.as_deref()
    }

    /// Adds one fixed-size record to the event-log hash: time, kind, device, a, b, the data's
    /// length and the CRC of its first 64 bytes (Go logf). Kinds: an event's code
    /// ([`event_code`]), b'S', b'D', b'C'.
    pub(crate) fn logf(&mut self, kind: u8, dev: u8, a: u64, b: u32, data: &[u8]) {
        let mut r = [0u8; 32];
        r[0..8].copy_from_slice(&(self.now as u64).to_be_bytes());
        r[8] = kind;
        r[9] = dev;
        r[10..18].copy_from_slice(&a.to_be_bytes());
        r[18..22].copy_from_slice(&b.to_be_bytes());
        r[22..26].copy_from_slice(&(data.len() as u32).to_be_bytes());
        r[26..30].copy_from_slice(&crc(&data[..data.len().min(64)]).to_be_bytes());
        if self.buf.capacity() == 0 {
            self.buf.reserve_exact(LOG_CHUNK + r.len());
        }
        self.buf.extend_from_slice(&r);
        if self.buf.len() >= LOG_CHUNK {
            self.log.update(&self.buf);
            self.buf.clear();
        }
    }

    /// The event-log hash so far, 24 hex digits: two runs of one seed must give the same (Go Hash).
    /// Reading it does not end the log: later records extend it, as Go's Sum(nil) did.
    pub fn hash(&mut self) -> String {
        self.log.update(&self.buf);
        self.buf.clear();
        hex(&self.log.clone().finalize()[..12])
    }

    /// The index of the session pairing (self inst, peer inst), added if new (Go pair).
    pub fn pair(&mut self, self_inst: u64, peer: u64) -> u16 {
        let p = [self_inst, peer];
        if let Some(i) = self.pairs.iter().position(|&x| x == p) {
            return i as u16;
        }
        self.pairs.push(p);
        (self.pairs.len() - 1) as u16
    }

    /// Adds f to the world's hooks; each hook sees every call after the ones added before it (the
    /// tests' hook(w, f), which chained Go's single Hook).
    pub fn hook(&mut self, f: impl FnMut(&mut World, &HookArgs<'_>) + 'static) {
        self.hooks.push(Box::new(f));
    }

    /// Calls every hook with a. The hooks are taken out of the world while they run, so each gets
    /// `&mut World`; hooks added meanwhile are kept, after the others.
    pub(crate) fn fire(&mut self, a: &HookArgs<'_>) {
        if self.hooks.is_empty() {
            return;
        }
        let mut hs = std::mem::take(&mut self.hooks);
        for h in &mut hs {
            h(self, a);
        }
        hs.append(&mut self.hooks);
        self.hooks = hs;
    }
}

/// The code an event has in the event log (Go logged byte(ev.Kind): App 1 … Tick 7).
pub(crate) fn event_code(ev: &Event<'_>) -> u8 {
    match ev {
        Event::Send { .. } => 1,
        Event::Packet { .. } => 2,
        Event::Links { .. } => 3,
        Event::Wake => 4,
        Event::Control(_) => 5,
        Event::Tick => 7,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;
    use crate::fake::{Evil, fake_maker};

    fn world(seed: u64) -> World {
        World::new(seed, fake_maker(false, Evil::None))
    }

    /// Every application flow becomes a real inner packet both ways (UDP and TCP, IPv4 and IPv6,
    /// to the anchor or the internet server), which the engine keys by its 5-tuple and outer
    /// reads back as the same flow and datagram; the IPv4 header checksum verifies.
    #[test]
    fn inner_packets_are_real_ip_and_read_back() {
        use wirebond_core::flow::Key;
        let data = [0xa5u8, 1, 2, 3, 4, 5, 6, 7, 0, 0, 0, 9, 1, 2];
        for (port, proto, v6, internet) in [
            (47998, UDP, false, false),
            (47999, UDP, true, false),
            (48000, UDP, true, false),
            (9000, UDP, true, true),
            (9100, TCP, false, true),
            (9200, TCP, true, true),
            (40001, UDP, false, false),
        ] {
            let f = FlowId::new(port, 50002);
            assert_eq!(path(port), Path { tcp: proto == TCP, v6, internet });
            for from in [Role::Client, Role::Anchor] {
                let p = inner(from, f, &data);
                assert_eq!(p.len(), inner_hdr(port) + data.len());
                let k = Key::parse(&p).expect("the engine keys it");
                let i = outer(&p).expect("outer reads it");
                assert_eq!((i.flow, i.data, k.proto), (f, &data[..], proto));
                let (me, it) = (near(port), far(port));
                let want = if from == Role::Client { (me, it, 50002, port) } else { (it, me, port, 50002) };
                assert_eq!((k.src, k.dst, k.sport, k.dport), want);
                assert_eq!((i.src, i.dst), (want.0, want.1));
                assert_eq!(is_anchor(it), !internet);
                if !v6 {
                    let sum = p[..20].chunks(2).fold(0u32, |a, w| a + u32::from(u16::from_be_bytes([w[0], w[1]])));
                    assert_eq!((sum & 0xffff) + (sum >> 16), 0xffff, "IPv4 header checksum");
                }
            }
        }
        assert_eq!(outer(&[0x45; 10]), None);
        assert_eq!(INNER_HDR, 28);
    }

    /// A 1,400-byte TUN device splits a 1,420-byte IPv4 packet into fragments of 1,396 and 44
    /// bytes, each with a verifying header checksum, which outer does not read as datagrams; the
    /// far kernel reassembles them in any order into the packet sent, with DF cleared; a datagram
    /// missing a fragment waits, and past REASM_MAX waiting datagrams the oldest is given up.
    #[test]
    fn fragments_are_reassembled() {
        let mut w = world(1);
        let f = FlowId::new(PORT_VIDEO, 50002);
        let p = inner(Role::Anchor, f, &[7; 1392]);
        assert_eq!(fragment(&p, 2000, 1), std::slice::from_ref(&p), "it fits");
        let fr = fragment(&p, 1400, 5);
        assert_eq!(fr.iter().map(Vec::len).collect::<Vec<_>>(), [1396, 44]);
        for x in &fr {
            let sum = x[..20].chunks(2).fold(0u32, |a, w| a + u32::from(u16::from_be_bytes([w[0], w[1]])));
            assert_eq!((sum & 0xffff) + (sum >> 16), 0xffff, "IPv4 header checksum");
            assert!(is_fragment(x) && outer(x).is_none());
        }
        assert_eq!(w.reassemble(DevId::Client, fr[1].clone()), None, "the last first: waits");
        let whole = w.reassemble(DevId::Client, fr[0].clone()).expect("whole");
        assert_eq!(
            (&whole[..4], &whole[4..8], &whole[8..10]),
            (&p[..4], &[0, 5, 0, 0][..], &p[8..10]),
            "id 5, DF clear"
        );
        assert!(whole[12..] == p[12..], "the addresses and the datagram");
        assert_eq!(outer(&whole).map(|i| (i.flow, i.data.len())), Some((f, 1392)));
        for id in 0..=REASM_MAX as u16 {
            assert_eq!(w.reassemble(DevId::Server, fragment(&p, 1400, id).remove(0)), None);
        }
        assert_eq!((w.reasm.len(), w.reasm_fails), (REASM_MAX, 1));
    }

    #[test]
    fn xoshiro_reference() {
        let mut r = Rng::from_state([1, 2, 3, 4]);
        assert_eq!([r.u64(), r.u64(), r.u64()], [11520, 0, 1509978240]);
    }

    #[test]
    fn splitmix_reference() {
        assert_eq!(splitmix(0), 0xe220_a839_7b1d_cdaf);
    }

    #[test]
    fn crc32c_reference() {
        assert_eq!(crc(b"123456789"), 0xe306_9283);
    }

    /// The sliced CRC equals CRC-32C's bitwise definition at every length and alignment.
    #[test]
    fn crc32c_matches_the_bitwise_definition() {
        fn bitwise(b: &[u8]) -> u32 {
            let mut c = !0u32;
            for &x in b {
                c ^= u32::from(x);
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0x82f6_3b78 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        let mut b = vec![0u8; 300];
        Rng::new(5, 6).fill(&mut b);
        for start in 0..8 {
            for end in start..b.len() {
                assert_eq!(crc(&b[start..end]), bitwise(&b[start..end]), "{start}..{end}");
            }
        }
    }

    #[test]
    fn below_is_in_range_and_uniform_enough() {
        let mut r = Rng::new(1, 2);
        let mut n = [0u32; 3];
        for _ in 0..30_000 {
            n[r.below(3) as usize] += 1;
        }
        assert!(n.iter().all(|&c| (9_000..11_000).contains(&c)), "{n:?}");
    }

    /// Go TestEngineOrder: 2,000 events at random times run in (time, sequence) order, and the
    /// clock ends at the run's end.
    #[test]
    fn engine_order() {
        let mut w = world(7);
        let mut r = Rng::new(1, 2);
        let got: Rc<RefCell<Vec<(Dur, usize)>>> = Rc::default();
        for i in 0..2000 {
            let at = r.below(50) as Dur * MS;
            let got = got.clone();
            w.at(at, move |w| got.borrow_mut().push((w.now(), i)));
        }
        w.run(SEC).unwrap();
        let got = got.take();
        for p in got.windows(2) {
            let ((t0, i0), (t1, i1)) = (p[0], p[1]);
            assert!(t1 > t0 || t1 == t0 && i1 > i0, "event {i1} at {} ran after {}", fmt_dur(t1), fmt_dur(t0));
        }
        assert!(got.len() == 2000 && w.now() == SEC, "ran {} events, ended at {}", got.len(), fmt_dur(w.now()));
    }

    /// Events scheduled in the past run now, after what is already queued for now; `every` repeats
    /// while it returns true; the clock stops at each run's end.
    #[test]
    fn at_and_every() {
        let mut w = world(1);
        let log: Rc<RefCell<Vec<(Dur, &str)>>> = Rc::default();
        let l = log.clone();
        w.every(10 * MS, 20 * MS, move |w| {
            l.borrow_mut().push((w.now(), "tick"));
            w.now() < 50 * MS
        });
        let l = log.clone();
        w.at(30 * MS, move |w| {
            let l2 = l.clone();
            w.at(0, move |w| l2.borrow_mut().push((w.now(), "past")));
            l.borrow_mut().push((w.now(), "at"));
        });
        w.run(25 * MS).unwrap();
        assert_eq!(w.now(), 25 * MS);
        w.run(SEC).unwrap();
        let want = [(10 * MS, "tick"), (30 * MS, "at"), (30 * MS, "tick"), (30 * MS, "past"), (50 * MS, "tick")];
        assert_eq!(log.take(), want);
        assert_eq!((w.now(), w.events()), (SEC, 5));
    }

    /// The first failure stops the run after its event, keeps its time and is returned again.
    #[test]
    fn run_stops_at_the_first_failure() {
        let mut w = world(1);
        let ran = Rc::new(Cell::new(0));
        for t in [10, 20, 30] {
            let ran = ran.clone();
            w.at(t * MS, move |w| {
                ran.set(ran.get() + 1);
                if w.now() >= 20 * MS {
                    w.fail(format_args!("boom {t}"));
                }
            });
        }
        let err = w.run(SEC).unwrap_err();
        assert_eq!(err, "at 0.020s: boom 20");
        assert_eq!((ran.get(), w.now()), (2, 20 * MS));
        assert_eq!(w.failed(), Some("at 0.020s: boom 20"));
        assert_eq!(w.run(2 * SEC).unwrap_err(), err);
    }

    /// An event that keeps queueing itself at one time trips the runaway budget.
    #[test]
    fn runaway_budget() {
        fn again(w: &mut World) {
            w.at(w.now(), again);
        }
        let mut w = world(1);
        w.at(0, again);
        let err = w.run(0).unwrap_err();
        assert!(err.contains("more than 600,000 events"), "{err}");
        assert_eq!(w.events(), 1_000_001);
    }

    /// The log hash is a function of the records: same records, same hash; 24 hex digits; reading
    /// it does not end the log.
    #[test]
    fn hash_follows_the_log() {
        let run = |extra: bool| {
            let mut w = world(1);
            for i in 0..2000u32 {
                w.now = Dur::from(i) * MS;
                w.logf(b'S', 1, u64::from(i), i, &[i as u8; 100]);
            }
            let h = w.hash();
            if extra {
                w.logf(b'D', 2, 0, 0, b"x");
            }
            (h, w.hash())
        };
        let (a, a2) = run(false);
        let (b, b2) = run(true);
        assert_eq!(a.len(), 24);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_eq!((&a, &a2), (&b, &a));
        assert_ne!(b2, b);
    }

    #[test]
    fn pairs_are_numbered_from_one() {
        let mut w = world(1);
        assert_eq!([w.pair(5, 6), w.pair(7, 8), w.pair(5, 6), w.pair(6, 5)], [1, 2, 1, 3]);
    }

    #[test]
    fn go_seconds() {
        assert_eq!(secs(180 * MS), 0.18);
        assert_eq!(secs(1500 * MS), 1.5);
        assert_eq!(secs(-1500 * MS), -1.5);
    }
}
