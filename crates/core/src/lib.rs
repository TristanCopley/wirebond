//! wirebond's engine (SPEC 2.0 §4.2): the wire format, sealing, the key ratchet, replay windows,
//! probes, estimates, liveness, rate control and pacer, flows (§13), rewind, reorder (§15), and
//! the two modes, performance and bandwidth (§14).
//!
//! The engine is driven by events and emits actions. It never blocks, never starts a thread,
//! never reads a clock (the caller passes `now`), never uses global randomness (the caller passes
//! a source) and does no I/O. The daemon (`crates/wirebond`, node) and netsim are its only
//! drivers.
//!
//! This file is the public API both drivers are written against (notes/2.0-build.md). In 2.0 a
//! bond carries whole IP packets: the daemon hands the session each packet it read from the TUN
//! device ([`Session::send_packet`], or [`Event::Send`]) and writes each packet the session
//! delivers ([`Out::deliver`]) to the TUN device. The session keys flows itself ([`flow::Key`],
//! §13) and numbers them per direction; a data payload is flow (2) + seq (4) + one inner packet
//! (§10.3), 33 bytes of overhead.

// No unsafe code; under test, only the counting allocator in pair.rs opts out, so the allocation
// tests can count (TestSendAllocs, TestSteadyAllocs).
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

use std::net::SocketAddr;

pub mod b64;
mod bandwidth;
mod buf;
mod consts;
mod estimate;
pub mod flow;
mod keys;
mod lane;
mod performance;
mod rate;
mod rewind;
mod session;
mod stats;
mod status;
mod wire;

#[cfg(test)]
mod pair;

pub use consts::*;
pub use session::{Mode, Session};

// ---------------------------------------------------------------------------------------------
// Time, roles, modes, ids

/// Monotonic time in nanoseconds from an arbitrary origin. The caller passes it to
/// [`Session::handle`]; the engine never reads a clock. Probes carry it in microseconds.
pub type Mono = i64;

/// The end of the bond a session is (§3). The client dials the control channel, sets the mode and
/// sends the idle probe; the anchor delivers locally or forwards (the daemon's business).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    Anchor,
}

/// The modes as the control protocol, the CLI, config.json and status name them (§14, §16):
/// nothing else is accepted.
pub const MODE_PERFORMANCE: &str = "performance";
pub const MODE_BANDWIDTH: &str = "bandwidth";

/// The protocol major carried in hello's `v` (§16).
pub const PROTOCOL: u32 = 2;

/// One of the daemon's UDP sockets: a link socket or the mesh-lane socket. 0 is none.
pub type SockId = u16;

/// A flow number on the wire (§10.3, §13): the sender numbers its flows per bond and direction,
/// from 1, reusing a number after FLOW_IDLE without traffic. My flow 3 and the peer's flow 3 are
/// different flows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct FlowId(pub u16);

// ---------------------------------------------------------------------------------------------
// The wire (§7.3, §7.4): its constants (TYPE_DATA … OVERHEAD) are in consts.rs.

/// The receive id in a lane packet's header (bytes 1–2), or None if `b` is too short to be a lane
/// packet. node uses it to pick the session before calling [`Session::handle`].
pub fn packet_rid(b: &[u8]) -> Option<u16> {
    (b.len() >= HEADER_LEN + TAG_LEN).then(|| u16::from_be_bytes([b[1], b[2]]))
}

// ---------------------------------------------------------------------------------------------
// What node tells the engine

/// The Tailscale lane as node sees it: its socket and the two mesh addresses, both on [`PORT`]
/// (§6.1). `None` in [`Event::Links`] while the mesh is down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeshLane {
    pub sock: SockId,
    pub local: SocketAddr,
    pub peer: SocketAddr,
}

/// One local link as node sees it: one interface and one address family (§6.1, §6.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// 1–255; kept while the interface name and family stay the same.
    pub id: u8,
    /// "Wi-Fi", "Ethernet", "iPhone" or the OS's interface name; unique per interface, because
    /// the engine groups lanes into bottleneck groups by label (§6.1).
    pub label: String,
    /// The iPhone heuristic (§6.2).
    pub metered: bool,
    /// The client's default-route link (§6.1).
    pub default: bool,
    /// The link's UDP socket, pinned to the interface.
    pub sock: SockId,
    /// The socket's local address.
    pub local: SocketAddr,
    /// STUN mapping; None until STUN answers.
    pub mapped: Option<SocketAddr>,
}

/// One input to [`Session::handle`]. Byte slices are borrowed: the engine copies what it keeps.
#[derive(Clone, Copy, Debug)]
pub enum Event<'a> {
    /// One whole IPv4 or IPv6 packet the daemon read from its TUN device, to send to the peer
    /// (§4.3). The session keys its flow ([`flow::Key`]), classes it and seals it once. A packet
    /// that does not parse as IP, or is longer than [`MAX_INNER`], is dropped.
    Send { packet: &'a [u8] },
    /// A datagram that arrived on a lane socket; node already removed STUN replies (§6.3).
    Packet { sock: SockId, from: SocketAddr, data: &'a [u8] },
    /// The full current link list, replacing the previous one, and the Tailscale lane.
    Links { links: &'a [Link], mesh: Option<MeshLane> },
    /// The device woke from sleep (§8.4).
    Wake,
    /// One control message: the peer's hello, key, links or mode, or the client's own mode.
    Control(&'a Msg),
    /// [`Session::next`] was reached.
    Tick,
}

/// Where the engine's actions go. The engine calls these synchronously from inside
/// [`Session::handle`]; borrowed bytes are valid only for the call.
pub trait Out {
    /// Send `data` from socket `sock` to `to`.
    fn send(&mut self, sock: SockId, to: SocketAddr, data: &[u8]);
    /// Deliver one whole inner IP packet that arrived from the peer: the daemon checks its
    /// addresses (§10.5 rule 6) and writes it to the TUN device. Small-flow packets come at once;
    /// bulk in seq order per flow (§15).
    fn deliver(&mut self, packet: &[u8]);
    /// Send `msg` to the peer over the control channel; dropped while it is down.
    fn control(&mut self, msg: Msg);
}

// ---------------------------------------------------------------------------------------------
// Control messages (§7.1). node encodes and decodes them as one JSON line each (at most MAX_MSG
// bytes). node composes and sends hello and links and answers view and error; the engine consumes
// the peer's hello, key, links and mode, and the client's own mode, and emits key. node filters a
// peer's candidates (loopback, unspecified, multicast, broadcast, link-local, mesh ranges; at most
// MAX_LINKS links with MAX_CANDS each) before it passes hello or links to the engine.

use serde::{Deserialize, Serialize, Serializer};

fn zero<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// Writes `f` as Go's encoding/json writes it, for the floats of [`LaneOut`], so a view message
/// is the Go implementation's byte for byte (tests/vectors.rs). Go writes a whole number below
/// 1e21 without a fraction or an exponent (95, not serde_json's 95.0; 1e20 in full, not 1e+20)
/// and 1e21 and above as serde_json does (1e+21). Two differences remain, which no lane's
/// measurement reaches: Go writes 0.000001 ≤ |f| < 0.00001 without the exponent serde_json uses,
/// and −0 as -0 where this writes 0.
fn gofloat<S: Serializer>(f: &f64, s: S) -> Result<S::Ok, S::Error> {
    if f.fract() != 0.0 || f.abs() >= 1e21 {
        s.serialize_f64(*f) // NaN and ±∞ too, which Go cannot write at all
    } else if f.abs() < (1u64 << 53) as f64 {
        s.serialize_i64(*f as i64) // below 2^53, every whole number is an i64 that converts exactly
    } else {
        // Go writes the shortest digits that read back as f, padded with zeros, as Display does.
        s.serialize_i128(f.to_string().parse().map_err(serde::ser::Error::custom)?)
    }
}

/// One control-channel message. The JSON is the Go implementation's, field for field.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Msg {
    /// "hello", "key", "links", "mode", "view" or "error".
    pub t: String,
    /// hello: protocol major ([`PROTOCOL`]).
    #[serde(default, skip_serializing_if = "zero")]
    pub v: u32,
    /// hello: release version, such as "v0.3.0".
    #[serde(default, skip_serializing_if = "zero")]
    pub version: String,
    /// hello: random, fixed for the process.
    #[serde(default, skip_serializing_if = "zero")]
    pub inst: u64,
    /// mode, and hello to an anchor: "performance" or "bandwidth".
    #[serde(default, skip_serializing_if = "zero")]
    pub mode: String,
    /// hello, links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkInfo>,
    /// key: the id the peer puts on packets it seals for me.
    #[serde(default, skip_serializing_if = "zero")]
    pub rid: u16,
    /// key: the epoch of `k`.
    #[serde(default, skip_serializing_if = "zero")]
    pub e: u64,
    /// key: my send key toward the peer for epoch `e`, 32 bytes, base64 in JSON as Go writes it.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "b64")]
    pub k: Vec<u8>,
    /// key: my next send counter when I sent it. A receiver that takes `k` as its first key, or as
    /// a key from outside the chain it holds, drops every packet sealed below `c`, so an earlier
    /// session's packets under the same key cannot replay into a new session on a reused rid
    /// (R2-09, notes/open.md). A Go peer omits it: 0, no floor.
    #[serde(default, skip_serializing_if = "zero")]
    pub c: u64,
    /// view: the answer; a view without it asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<View>,
    /// error.
    #[serde(default, skip_serializing_if = "zero")]
    pub code: String,
    /// error.
    #[serde(default, skip_serializing_if = "zero")]
    pub message: String,
}

/// One link as a peer offers it in hello and links (§6.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkInfo {
    pub id: u8,
    pub label: String,
    #[serde(default, skip_serializing_if = "zero")]
    pub metered: bool,
    #[serde(default, skip_serializing_if = "zero")]
    pub default: bool,
    #[serde(default)]
    pub cands: Vec<SocketAddr>,
}

/// One side's measurements of its own sending on each lane, which the peer shows as that lane's
/// "in" direction (§14.2).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    #[serde(default)]
    pub lanes: Vec<LaneOut>,
}

/// What one side measures about its own sending on one lane (§8.3, §14.3). Its floats are
/// written as Go writes them: 0, not 0.0.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneOut {
    pub id: u16,
    /// spread, copies or standby.
    pub role: String,
    #[serde(rename = "rttMs", serialize_with = "gofloat")]
    pub rtt_ms: f64,
    #[serde(rename = "owdRelMs", serialize_with = "gofloat")]
    pub owd_rel_ms: f64,
    #[serde(rename = "queueMs", serialize_with = "gofloat")]
    pub queue_ms: f64,
    /// 0–1.
    #[serde(serialize_with = "gofloat")]
    pub loss: f64,
    /// The controller's rate; 0 while unlimited.
    #[serde(serialize_with = "gofloat")]
    pub rate_mbps: f64,
    pub sent_bytes: u64,
    pub copy_bytes: u64,
}

// ---------------------------------------------------------------------------------------------
// What the engine tells node and netsim

/// The read-only view of a session that status prints (§14) and netsim checks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStatus {
    pub mode: String,
    pub lanes: Vec<LaneStatus>,
    pub flows: Vec<FlowStatus>,
    /// Bytes sent on metered lanes over the last minute, scaled to an hour.
    pub metered_gb_per_hour: f64,
    /// Rewinds sent (§10.2).
    pub rewinds: u64,
    /// Bulk sequence gaps the receiver gave up on: by §10.3 rules 2 and 3, or because a small
    /// packet of the flow passed them while datagrams were held behind them.
    pub unrecovered: u64,
    /// Datagrams dropped because MAX_FLOWS flows were live.
    pub flows_dropped: u64,
    /// Bytes held in pacer queues, reorder buffers and rewind rings; ≤ BOND_BYTES.
    pub bytes: usize,
    /// Arrival of the newest authenticated packet; None if none yet.
    pub last_auth: Option<Mono>,
    pub problems: Vec<Problem>,
    /// The last STATUS_EVENTS, oldest first.
    pub events: Vec<LogEvent>,
}

/// One lane in a [`SessionStatus`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaneStatus {
    /// Client link id << 8 | server link id; 0 is the Tailscale lane.
    pub id: u16,
    /// "direct" or "mesh".
    pub kind: &'static str,
    /// This side's link label; "" for the Tailscale lane.
    pub local: String,
    /// The peer's link label; "" for the Tailscale lane.
    pub remote: String,
    /// Either end's link is metered.
    pub metered: bool,
    /// punching, up, suspect, down or dormant (§8.4).
    pub state: &'static str,
    /// The peer address passed path validation (§7.6 rule 2).
    pub validated: bool,
    /// Barred from bulk (§11.1 rule 2).
    pub barred: bool,
    pub sock: SockId,
    /// The current return address.
    pub peer: Option<SocketAddr>,
    /// This side's sending on the lane.
    pub out: LaneOut,
    /// Bytes carried (§11.5): data packets I sent on this lane and received on it, copies
    /// included.
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    /// This lane's fraction of the bond's bytes, both directions (§11.5); 0 before any.
    pub share: f64,
    /// Performance (§11.5): packets received whose first copy came on this lane.
    pub first_arrivals: u64,
    /// Performance (§11.5): first copies on this lane with every other copy RESCUE_GAP later or
    /// never.
    pub rescues: u64,
    /// Bandwidth (§11.5): how long packets that came on this lane waited in reorder, p50 and p99.
    pub reorder_wait_p50: Mono,
    pub reorder_wait_p99: Mono,
}

/// One flow direction in a [`SessionStatus`] (§13).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlowStatus {
    /// Its number: mine for a flow I send (`tx`), the peer's for one I receive.
    pub flow: FlowId,
    /// I send it (true) or receive it.
    pub tx: bool,
    /// Its key, from its packets' headers.
    pub key: Option<flow::Key>,
    /// "bulk" or "small", for the direction with the higher rate.
    pub class: &'static str,
    /// The higher of the two directions' rates over the last 200 ms.
    pub mbps: f64,
    /// Lanes that carried it in the last 100 ms, either direction.
    pub lanes: Vec<u16>,
}

/// A condition the engine detects; node adds the fix and command (§14.4).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Problem {
    /// bulk_on_slower_lane or bulk_exceeds_every_lane.
    pub code: &'static str,
    /// For a person, with the numbers.
    pub message: String,
}

/// One entry of a session's event log, such as "47998 moved Wi-Fi → iPhone (Wi-Fi lagging
/// 22 ms)". node turns `at` into wall-clock time for status.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogEvent {
    pub at: Mono,
    pub msg: String,
}

// ---------------------------------------------------------------------------------------------
// Receive ids and sessions

/// Maps receive ids to sessions, so ids are unique across every session on the device (§7.2).
/// node owns one and passes it to every call that may allocate or free ids; a session is named
/// in it by the opaque `owner` key node gave [`Session::new`].
#[derive(Debug, Default)]
pub struct RidTable {
    owners: std::collections::HashMap<u16, u64>,
    next: u16,
}

impl RidTable {
    pub fn new() -> RidTable {
        RidTable::default()
    }

    /// The owner key of the session that holds `rid`, if any.
    pub fn lookup(&self, rid: u16) -> Option<u64> {
        self.owners.get(&rid).copied()
    }
}

/// A source of random bytes for keys: the OS's in node, a seeded generator in netsim.
pub type Rand = Box<dyn FnMut(&mut [u8]) + Send>;

// Session::new is in session.rs with the struct it fills, Session::snapshot in status.rs.
impl Session {
    /// Applies one event at monotonic time `now`, calling `out` for each resulting action.
    pub fn handle(&mut self, now: Mono, ev: Event<'_>, rids: &mut RidTable, out: &mut dyn Out) {
        if self.rid == 0 {
            self.rid = rids.alloc(self.owner); // 0 again if the table is full: retried next time
        }
        self.now = now;
        match ev {
            Event::Send { packet } => self.send_inner(out, now, packet),
            // Timers run on any event but a lane packet, whose actions are only its deliveries
            // and its reply probe, so no reply is ever larger than the packet that caused it
            // (§7.6 rule 3).
            Event::Packet { sock, from, data } => return self.packet(out, now, sock, from, data),
            Event::Links { links, mesh } => {
                (self.links, self.mesh) = (links.to_vec(), mesh);
                self.set_links(out, now);
                self.punch(now);
            }
            Event::Wake => self.wake(now),
            Event::Control(m) => self.control(out, now, m),
            Event::Tick => {}
        }
        if now >= self.next {
            self.tick(out, now);
        }
    }

    /// Sends one whole IP packet read from the TUN device: shorthand for `handle` with
    /// [`Event::Send`], the daemon's hot path (§4.3).
    pub fn send_packet(&mut self, now: Mono, packet: &[u8], rids: &mut RidTable, out: &mut dyn Out) {
        self.handle(now, Event::Send { packet }, rids, out);
    }

    /// The earliest time `handle` wants an [`Event::Tick`]; `Mono::MAX` when nothing is due, as
    /// for an idle server. Ask after every `handle`.
    pub fn next(&self) -> Mono {
        self.next
    }

    /// Frees the session's receive ids. node calls it when the session ends (§7.6 rule 8).
    pub fn close(&mut self, rids: &mut RidTable) {
        rids.free(self.rid, self.owner);
    }
}
