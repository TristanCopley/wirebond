//! The safety invariants of §17.2 that need more than one datagram (Go netsim/check.go; the
//! registry in apps.rs checks deliveries), checked after every handle call, and the on-path
//! adversary (A6, A14, the swarm, the fuzzer). What core keeps sealed (probe sequence numbers,
//! echoes) is approximated from what netsim can see; each check says how (notes/netsim.md 5–8).
//! Owner: N4 (with tests/swarm.rs and tests/fuzz.rs).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::SocketAddr;

use wirebond_core::{
    BOND_BYTES, Event, HEADER_LEN, IDLE_AFTER, LaneStatus, LinkInfo, Msg, SMALL_BIT, SessionStatus, SockId, TAG_LEN,
    TYPE_DATA, TYPE_PROBE, VALIDATE_AMP,
};

use crate::node::{DevId, MESH_SOCK, Pkt};
use crate::sim::{Action, Dur, MS, Rng, World, crc};

/// Counters per sending key remembered to catch a counter sealed twice: about 10 s at 13,000
/// packets a second, far more than any rewind or trial resends.
pub const SEAL_WINDOW: usize = 1 << 17;

/// The invariants' memory (Go checker). Go's maps keyed by *Device are keyed by [`DevId`]; maps
/// that are never iterated could be HashMaps if profiling asks (none is iterated today).
#[derive(Default)]
pub struct Checker {
    pub(crate) seals: BTreeMap<SealKey, Box<SealRing>>,
    /// Each key's pair of processes (Go map[string(key)]).
    pub(crate) keys: BTreeMap<Vec<u8>, [u64; 2]>,
    /// Bytes sent to an address while it was not a validated lane.
    pub(crate) sent: BTreeMap<PathKey, usize>,
    pub(crate) recv: BTreeMap<PathKey, usize>,
    /// A packet from the device's socket to the address reached its peer.
    pub(crate) reach: BTreeSet<PathKey>,
    /// The candidates each device's peer offered: punching may probe them without limit.
    pub(crate) cands: [BTreeSet<SocketAddr>; 2],
    /// (device, rid, counter, CRC) of every authentic probe seen.
    pub(crate) probes: BTreeSet<ProbeKey>,
}

/// A sending key: its process, its peer process, the device's Session count (a Session draws its
/// own random key, notes/netsim.md 21) and the rid it is sent under (Go sealKey).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SealKey {
    pub(crate) d: DevId,
    pub(crate) inst: u64,
    pub(crate) peer: u64,
    pub(crate) sess: u32,
    pub(crate) rid: u16,
}

/// The last SEAL_WINDOW counters of one key and their CRCs (Go sealRing): counter + 1, 0 empty.
pub(crate) struct SealRing {
    pub(crate) ctr: Vec<u64>,
    pub(crate) crc: Vec<u32>,
}

impl SealRing {
    pub(crate) fn new() -> SealRing {
        SealRing { ctr: vec![0; SEAL_WINDOW], crc: vec![0; SEAL_WINDOW] }
    }
}

/// A device's socket and a remote address (Go pathKey).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PathKey {
    pub(crate) d: DevId,
    pub(crate) sock: SockId,
    pub(crate) addr: SocketAddr,
}

/// One probe: the device that received it, its rid and counter, and the CRC of its bytes (Go
/// probeKey, without the CRC). A sealed packet is its bytes: a replay, a duplicate or a relay of a
/// probe repeats them, while a later Session's probe that happens to reuse the rid and counter
/// (a restart's fresh rid table, a fresh key's counters) is another packet. Go's key, without
/// the CRC, took such a probe for a copy of the old one and reported its genuine move (the swarm's
/// seed 4 in notes/open.md; notes/rust.md, netsim N4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProbeKey {
    pub(crate) d: DevId,
    pub(crate) rid: u16,
    pub(crate) ctr: u64,
    pub(crate) sum: u32,
}

/// A lane packet's rid and counter (bytes 1–2 and 3–10); b is at least HEADER_LEN bytes.
pub fn header(b: &[u8]) -> (u16, u64) {
    let rid = u16::from_be_bytes([b[1], b[2]]);
    let mut ctr = [0; 8];
    ctr.copy_from_slice(&b[3..11]);
    (rid, u64::from_be_bytes(ctr))
}

/// The lane of s on socket sock whose peer is peer (Go laneOf).
pub(crate) fn lane_of(s: &SessionStatus, sock: SockId, peer: SocketAddr) -> Option<&LaneStatus> {
    s.lanes.iter().find(|l| l.sock == sock && l.peer == Some(peer))
}

/// A lane's address in a failure message ("none" for Go's invalid AddrPort).
struct Show(Option<SocketAddr>);

impl fmt::Display for Show {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(a) => a.fmt(f),
            None => f.write_str("none"),
        }
    }
}

impl Checker {
    /// Counts n bytes that reached d's socket sock from `from` (Go recvd).
    pub(crate) fn recvd(&mut self, d: DevId, sock: SockId, from: SocketAddr, n: usize) {
        *self.recv.entry(PathKey { d, sock, addr: from }).or_default() += n;
    }

    /// Records the candidates d's peer offered (Go offered).
    pub(crate) fn offered(&mut self, d: DevId, ls: &[LinkInfo]) {
        for l in ls {
            self.cands[d.idx()].extend(l.cands.iter().copied());
        }
    }
}

impl World {
    /// Whether d may probe a without limit: the peer's mesh address or an offered candidate (Go
    /// known).
    pub(crate) fn known(&self, d: DevId, a: SocketAddr) -> bool {
        a == self[d.peer()].mesh || self.chk.cands[d.idx()].contains(&a)
    }

    /// Checks one handle call of d whose actions are acts (Go after): the cap on held bytes,
    /// no answer to an adversary's packet, data only to validated addresses that ground truth has
    /// reached, probes within VALIDATE_AMP to unvalidated unknown addresses, no reply larger than
    /// its trigger, keys, seals, and lane moves. Updates d's before-snapshot.
    pub(crate) fn check_after(&mut self, d: DevId, ev: &Event<'_>, p: Option<&Pkt>, acts: &[Action]) {
        let after = self[d].snap();
        let before = std::mem::take(&mut self[d].last_snap);
        self.check_call(d, ev, p, acts, &before, &after);
        self[d].last_snap = after;
    }

    /// The body of [`World::check_after`] between the two snapshots.
    fn check_call(
        &mut self,
        d: DevId,
        ev: &Event<'_>,
        p: Option<&Pkt>,
        acts: &[Action],
        before: &SessionStatus,
        after: &SessionStatus,
    ) {
        let name = self[d].name;
        if after.bytes > BOND_BYTES {
            self.fail(format_args!("{name}: the Session holds {} bytes, above SessionBytes", after.bytes));
        }
        // A packet whose bytes a session sealed is authentic; the adversary's corrupted, forged
        // and wrong-rid packets are not. A probe is newer when its bytes arrive for the first
        // time: links are FIFO, so a genuine probe's Seq is then the highest seen on its lane.
        let authentic = p.is_none_or(|p| p.adv < Adv::Corrupt);
        let packet = match *ev {
            Event::Packet { sock, from, data } => Some((sock, from, data)),
            _ => None,
        };
        let probe = packet.filter(|&(_, _, b)| b.len() >= HEADER_LEN && b[0] == TYPE_PROBE);
        let mut newer = false;
        if let Some((_, _, b)) = probe
            && authentic
        {
            let (rid, ctr) = header(b);
            newer = self.chk.probes.insert(ProbeKey { d, rid, ctr, sum: crc(b) });
        }
        let mut reply = 0;
        for a in acts {
            let (sock, to, b) = match a {
                Action::Send { sock, to, data } => (*sock, *to, data.as_slice()),
                Action::Control(m) => {
                    if m.t == "key" {
                        self.check_key(d, m);
                    }
                    continue;
                }
                Action::Deliver { .. } => continue,
            };
            if b.len() < HEADER_LEN + TAG_LEN {
                self.fail(format_args!("{name}: sent a {}-byte lane packet", b.len()));
                return;
            }
            self.check_sealed(d, b);
            let k = PathKey { d, sock, addr: to };
            let is_reply = packet.is_some_and(|(s, f, _)| s == sock && f == to);
            if is_reply && !authentic {
                let kind = p.map(|p| p.adv).unwrap_or_default();
                self.fail(format_args!("{name}: answered a packet the adversary made (kind {kind:?}) from {to}"));
            }
            let validated = |s: &SessionStatus| lane_of(s, sock, to).is_some_and(|l| l.validated);
            let valid = validated(before) || validated(after);
            if !valid {
                *self.chk.sent.entry(k).or_default() += b.len();
            }
            match b[0] & !SMALL_BIT {
                // Validated per Snapshot, and in ground truth: path validation echoes a probe sent
                // to the address, so something from this socket has reached the peer through it.
                // The Tailscale lane's mesh addresses need no validation.
                TYPE_DATA => {
                    if !valid {
                        self.fail(format_args!(
                            "{name}: data (byte 0 {:#04x}) sent from socket {sock} to {to}, which \
                             is not a validated lane",
                            b[0]
                        ));
                    } else if !self.chk.reach.contains(&k) && !(sock == MESH_SOCK && to == self[d.peer()].mesh) {
                        self.fail(format_args!(
                            "{name}: data sent from socket {sock} to {to}, through which nothing \
                             from that socket has reached the peer"
                        ));
                    }
                }
                TYPE_PROBE => {
                    if b[0] != TYPE_PROBE {
                        self.fail(format_args!("{name}: a probe with the small bit"));
                    }
                    if is_reply {
                        reply += b.len();
                    }
                    if !valid && !self.known(d, to) {
                        let (sent, recv) = self.path_bytes(d, sock, to);
                        if sent as u64 > VALIDATE_AMP * recv as u64 {
                            self.fail(format_args!(
                                "{name}: {sent} bytes sent to unvalidated {to}, which sent {recv} \
                                 (ValidateAmp {VALIDATE_AMP})"
                            ));
                        }
                    }
                }
                _ => self.fail(format_args!("{name}: sent a packet of unknown type {:#04x}", b[0])),
            }
        }
        // A reply is a probe sent back to a probe's source in the same call: while idle (§8.1's
        // answer to the idle probe), or toward a source that is neither a validated lane nor an
        // offered candidate (an address an attacker could name). Punching toward candidates and
        // the regular probes of an active lane are not replies.
        if let Some((sock, from, b)) = probe
            && reply > b.len()
        {
            let idle = self.now - self[d].last_data >= IDLE_AFTER;
            let lane = lane_of(before, sock, from).is_some_and(|l| l.validated);
            if idle || !lane && !self.known(d, from) {
                self.fail(format_args!(
                    "{name}: answered a {}-byte probe from {from} with {reply} bytes of probes",
                    b.len()
                ));
            }
        }
        self.check_moves(d, ev, before, after, newer);
    }

    /// Rule 2 of §7.6 from Snapshot: a lane's peer address changes only on the first arrival of
    /// an authentic probe from the new address on the lane's own socket; a move outside a packet
    /// must leave the lane unvalidated and point at an offered candidate; the Tailscale lane never
    /// moves (Go moves; newer: this call's probe arrived for the first time).
    pub(crate) fn check_moves(
        &mut self,
        d: DevId,
        ev: &Event<'_>,
        before: &SessionStatus,
        after: &SessionStatus,
        newer: bool,
    ) {
        let name = self[d].name;
        let mesh = self[d.peer()].mesh;
        // The lane id inside the seal is not visible, so a probe on any lane of the socket counts.
        let (packet, sock, from) = match *ev {
            Event::Packet { sock, from, .. } => (true, sock, Some(from)),
            _ => (false, 0, None),
        };
        for a in &after.lanes {
            if a.id == 0 {
                if let Some(p) = a.peer
                    && p != mesh
                {
                    self.fail(format_args!("{name}: the Tailscale lane's address is {p}, not the peer's mesh address"));
                }
                continue;
            }
            let Some(b) = before.lanes.iter().rfind(|b| b.id == a.id) else {
                continue;
            };
            if b.peer.is_none() || b.peer == a.peer || newer && sock == a.sock && from == a.peer {
                continue;
            }
            if packet || a.validated {
                self.fail(format_args!(
                    "{name}: lane {:04x} moved {} → {} without a newer authenticated probe from \
                     it on its socket",
                    a.id,
                    Show(b.peer),
                    Show(a.peer)
                ));
            } else if !a.peer.is_some_and(|p| self.known(d, p)) {
                self.fail(format_args!(
                    "{name}: lane {:04x} moved to {}, which no candidate named, without a probe",
                    a.id,
                    Show(a.peer)
                ));
            }
        }
    }

    /// No (key, counter) sealed twice with different bytes, nor again after SEAL_WINDOW newer
    /// counters (Go sealed). A key is named by its sending process, its peer process, the Session
    /// and the rid it is sent under: when one side's Session ends by SessionLinger while the peer
    /// process lives on, the pair's next Session holds a new random key, whose counters start
    /// again.
    pub(crate) fn check_sealed(&mut self, d: DevId, b: &[u8]) {
        let (rid, ctr) = header(b);
        let dev = &self[d];
        let (name, k) = (dev.name, SealKey { d, inst: dev.inst, peer: dev.peer_inst, sess: dev.sessions, rid });
        let r = self.chk.seals.entry(k).or_insert_with(|| Box::new(SealRing::new()));
        let (i, c, sum) = ((ctr % SEAL_WINDOW as u64) as usize, ctr.wrapping_add(1), crc(b));
        if r.ctr[i] == c {
            if r.crc[i] != sum {
                self.fail(format_args!("{name}: counter {ctr} under rid {rid} sealed twice with different bytes"));
            }
        } else if r.ctr[i] > c {
            self.fail(format_args!(
                "{name}: counter {ctr} under rid {rid} sent again after {SEAL_WINDOW} newer \
                 counters"
            ));
        } else {
            (r.ctr[i], r.crc[i]) = (c, sum);
        }
    }

    /// A key belongs to one pair of processes (§7.2) (Go key).
    pub(crate) fn check_key(&mut self, d: DevId, m: &Msg) {
        let (name, pair) = (self[d].name, [self[d].inst, self[d].peer_inst]);
        match self.chk.keys.get_mut(m.k.as_slice()) {
            Some(old) => {
                let moved = *old != pair;
                *old = pair;
                if moved {
                    self.fail(format_args!("{name}: a key sent to an earlier process was sent again to a new one"));
                }
            }
            None => {
                self.chk.keys.insert(m.k.clone(), pair);
            }
        }
    }

    /// The bytes d sent from sock to addr while it was not a validated lane, and the bytes that
    /// reached sock from addr (A6 reads Go's chk.sent and chk.recv for the relay's address).
    pub fn path_bytes(&self, d: DevId, sock: SockId, addr: SocketAddr) -> (usize, usize) {
        let k = PathKey { d, sock, addr };
        let get = |m: &BTreeMap<PathKey, usize>| m.get(&k).copied().unwrap_or(0);
        (get(&self.chk.sent), get(&self.chk.recv))
    }
}

// ---------------------------------------------------------------------------------------------
// The adversary

/// What the adversary did to a packet (Go adv* constants). Ordered: the kinds from Corrupt on
/// are not authentic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Adv {
    #[default]
    Genuine,
    /// A genuine probe re-sent from a third address, ahead of the original.
    Relay,
    /// A genuine packet recorded earlier, sent again.
    Replay,
    /// A genuine packet duplicated on its lane.
    Dup,
    /// A genuine packet with one bit flipped.
    Corrupt,
    /// Random bytes behind a plausible header.
    Forge,
    /// A genuine packet under another rid.
    WrongRid,
}

/// The number of kinds, Genuine included (Go advKinds): `p` and `injected` are indexed by
/// `Adv as usize`.
pub const ADV_KINDS: usize = 7;

impl Adv {
    /// The attacks, Relay to WrongRid (Go's loops from advRelay to advKinds).
    pub const ATTACKS: [Adv; 6] = [Adv::Relay, Adv::Replay, Adv::Dup, Adv::Corrupt, Adv::Forge, Adv::WrongRid];

    pub fn idx(self) -> usize {
        self as usize
    }
}

/// An on-path network in front of `target`: it records the packets reaching it and injects
/// replays, corruptions, forgeries, wrong-rid copies, duplicates and probes relayed from `third`,
/// each with probability p[kind] per arriving packet (A6) (Go Adversary). Owned by the world
/// (`world.adv`).
pub struct Adversary {
    pub target: DevId,
    pub third: SocketAddr,
    pub p: [f64; ADV_KINDS],
    pub on: bool,
    pub injected: [u64; ADV_KINDS],
    pub(crate) r: Rng,
    /// Up to 4,096 recorded packets, then random replacement.
    pub(crate) rec: Vec<Recorded>,
}

/// A packet the adversary recorded (Go recorded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Recorded {
    pub(crate) sock: SockId,
    pub(crate) src: SocketAddr,
    pub(crate) data: Vec<u8>,
}

/// How many packets the adversary keeps.
const ADV_RECORD: usize = 4096;

/// A packet the adversary sends: to the target's socket `sock` from `src` at `at`.
struct AdvSend {
    at: Dur,
    sock: SockId,
    src: SocketAddr,
    pkt: Pkt,
}

impl Adversary {
    /// Keeps a copy of a packet reaching the target, reusing a replaced record's buffer; returns
    /// its index.
    fn adv_record(&mut self, sock: SockId, src: SocketAddr, data: &[u8]) -> usize {
        if self.rec.len() < ADV_RECORD {
            self.rec.push(Recorded { sock, src, data: data.to_vec() });
            return self.rec.len() - 1;
        }
        let i = self.r.below(self.rec.len() as u64) as usize;
        let r = &mut self.rec[i];
        (r.sock, r.src) = (sock, src);
        r.data.clear();
        r.data.extend_from_slice(data);
        i
    }

    /// Makes the packet of kind k from r at now and counts it (the body of Go inject); None when
    /// k does not apply: a relay of a non-probe, Genuine, a replay with nothing recorded, or a
    /// packet too short to be a lane packet.
    fn adv_make(&mut self, k: Adv, r: Recorded, now: Dur) -> Option<AdvSend> {
        let Recorded { mut sock, mut src, data: mut b } = r;
        if b.len() < HEADER_LEN + TAG_LEN {
            return None;
        }
        match k {
            Adv::Genuine => return None,
            Adv::Relay => {
                if b[0] != TYPE_PROBE {
                    return None;
                }
                src = self.third;
            }
            Adv::Replay => {
                if self.rec.is_empty() {
                    return None;
                }
                let o = &self.rec[self.r.below(self.rec.len() as u64) as usize];
                (sock, src) = (o.sock, o.src);
                b.clear();
                b.extend_from_slice(&o.data);
            }
            Adv::Dup => {}
            Adv::Corrupt => {
                let i = self.r.below(b.len() as u64) as usize;
                b[i] ^= 1 << self.r.below(8);
            }
            Adv::Forge => {
                let (_, ctr) = header(&b);
                for x in &mut b[HEADER_LEN..] {
                    *x = self.r.u32() as u8;
                }
                let ctr = ctr.wrapping_add(1).wrapping_add(self.r.below(1000));
                b[3..11].copy_from_slice(&ctr.to_be_bytes());
            }
            Adv::WrongRid => {
                let (rid, _) = header(&b);
                let rid = rid ^ (1 + self.r.below(65535)) as u16;
                b[1..3].copy_from_slice(&rid.to_be_bytes());
            }
        }
        self.injected[k.idx()] += 1;
        let at = if k == Adv::Relay { now } else { now + self.r.dur(2 * MS) };
        let pkt = Pkt { data: b, adv: k, t0: now, ..Pkt::default() };
        Some(AdvSend { at, sock, src, pkt })
    }
}

impl World {
    /// Puts an adversary in front of target, from 203.0.113.66:4444, on, all probabilities 0
    /// (Go NewAdversary); returns it to set up.
    pub fn new_adversary(&mut self, target: DevId) -> &mut Adversary {
        let r = self.sub();
        self.adv.insert(Adversary {
            target,
            third: SocketAddr::from(([203, 0, 113, 66], 4444)),
            p: [0.0; ADV_KINDS],
            on: true,
            injected: [0; ADV_KINDS],
            r,
            rec: Vec::new(),
        })
    }

    /// Records a genuine packet reaching d and injects what p draws; returns how long the
    /// original is held back: a relayed probe gets 5 ms ahead of it (Go see).
    pub(crate) fn adv_see(&mut self, d: DevId, sock: SockId, src: SocketAddr, p: &Pkt) -> Dur {
        let Some(a) = self.adv.as_mut() else {
            return 0;
        };
        if d != a.target || !a.on || p.adv != Adv::Genuine || p.data.len() < HEADER_LEN + TAG_LEN {
            return 0;
        }
        let i = a.adv_record(sock, src, &p.data);
        let mut hold = 0;
        for k in Adv::ATTACKS {
            let Some(a) = self.adv.as_mut() else {
                break;
            };
            let pk = a.p[k.idx()];
            if pk > 0.0 && a.r.f64() < pk {
                let r = a.rec[i].clone();
                if self.adv_inject(k, r) && k == Adv::Relay {
                    hold = 5 * MS;
                }
            }
        }
        hold
    }

    /// Injects one packet of kind k made from r, within 2 ms (a relay at once); false when k does
    /// not apply (a relay of a non-probe) (Go inject).
    pub(crate) fn adv_inject(&mut self, k: Adv, r: Recorded) -> bool {
        let now = self.now;
        let Some(a) = self.adv.as_mut() else {
            return false;
        };
        let target = a.target;
        let Some(AdvSend { at, sock, src, pkt }) = a.adv_make(k, r, now) else {
            return false;
        };
        self.at(at, move |w| w.packet(target, sock, src, pkt));
        true
    }

    /// Sends every recorded packet again now, as an attacker does after a restart (A14) (Go
    /// ReplayAll).
    pub fn replay_all(&mut self) {
        let now = self.now;
        let Some(a) = self.adv.as_mut() else {
            return;
        };
        let target = a.target;
        a.injected[Adv::Replay.idx()] += a.rec.len() as u64;
        let rec = std::mem::take(&mut a.rec);
        for r in &rec {
            let (sock, src) = (r.sock, r.src);
            let pkt = Pkt { data: r.data.clone(), adv: Adv::Replay, t0: now, ..Pkt::default() };
            self.at(now, move |w| w.packet(target, sock, src, pkt));
        }
        if let Some(a) = self.adv.as_mut() {
            a.rec = rec;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The checker and the adversary on a world whose Sessions are a stub that reports a chosen
    //! Snapshot, so every invariant is driven directly. Scheduled injections are counted in the
    //! queue, not run: delivering them is the node stand-in's.

    use std::net::{Ipv4Addr, SocketAddrV4};

    use wirebond_core::{Mono, Out, RidTable};

    use super::*;
    use crate::sim::{Endpoint, Maker, SEC};

    /// A Session that reports `0` as its Snapshot and does nothing else.
    struct Stub(SessionStatus);

    impl Endpoint for Stub {
        fn handle(&mut self, _: Mono, _: Event<'_>, _: &mut RidTable, _: &mut dyn Out) {}
        fn next(&self) -> Mono {
            Mono::MAX
        }
        fn snapshot(&self) -> SessionStatus {
            self.0.clone()
        }
        fn close(&mut self, _: &mut RidTable) {}
    }

    const SV: DevId = DevId::Server;
    /// The client's hotel mapping, its mapping after an address rebind, and the adversary.
    const A: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 45776));
    const B: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 10), 57153));
    const THIRD: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 66), 4444));

    /// Two devices with a stub Session each, paired, active (data just flowed).
    fn world() -> World {
        let mk: Maker = Box::new(|_, _, _| Box::new(Stub(SessionStatus::default())));
        let mut w = World::new(1, mk);
        w.now = 10 * SEC;
        for d in DevId::BOTH {
            w[d].sess = Some(Box::new(Stub(SessionStatus::default())));
            w[d].sessions = 1;
            w[d].peer_inst = w[d.peer()].inst;
            w[d].last_data = w.now;
        }
        w
    }

    /// A lane packet of n bytes: type, rid, counter, then fill.
    fn pkt(typ: u8, rid: u16, ctr: u64, n: usize, fill: u8) -> Vec<u8> {
        let mut b = vec![fill; n];
        b[0] = typ;
        b[1..3].copy_from_slice(&rid.to_be_bytes());
        b[3..11].copy_from_slice(&ctr.to_be_bytes());
        b
    }

    fn lane(id: u16, sock: SockId, peer: SocketAddr, validated: bool) -> LaneStatus {
        LaneStatus { id, sock, peer: Some(peer), validated, ..LaneStatus::default() }
    }

    fn status(lanes: Vec<LaneStatus>) -> SessionStatus {
        SessionStatus { lanes, ..SessionStatus::default() }
    }

    fn send(sock: SockId, to: SocketAddr, data: Vec<u8>) -> Action {
        Action::Send { sock, to, data }
    }

    fn offer(w: &mut World, a: SocketAddr) {
        let l = LinkInfo { cands: vec![a], ..LinkInfo::default() };
        w.chk.offered(SV, &[l]);
    }

    /// One checked handle call of d whose Session reports `after` once it returns.
    fn call(w: &mut World, d: DevId, ev: Event<'_>, p: Option<&Pkt>, acts: &[Action], after: SessionStatus) {
        w[d].sess = Some(Box::new(Stub(after)));
        w.check_after(d, &ev, p, acts);
    }

    /// One checked call of the server for packet b (made by `adv`) from `from` on socket sock,
    /// counted as received first, as the stand-in's packet() does.
    fn arrive(
        w: &mut World,
        (from, sock): (SocketAddr, SockId),
        b: &[u8],
        adv: Adv,
        acts: &[Action],
        after: SessionStatus,
    ) {
        w.chk.recvd(SV, sock, from, b.len());
        let p = Pkt { adv, ..Pkt::new(b.to_vec()) };
        let ev = Event::Packet { sock, from, data: b };
        call(w, SV, ev, Some(&p), acts, after);
    }

    fn failed(w: &World) -> String {
        w.failed().unwrap_or("").to_string()
    }

    #[test]
    fn header_reads_rid_and_counter() {
        let b = pkt(TYPE_DATA, 0x0102, 0x0304_0506_0708_090a, 27, 0);
        assert_eq!(header(&b), (0x0102, 0x0304_0506_0708_090a));
    }

    #[test]
    fn lane_of_matches_socket_and_peer() {
        let s = status(vec![lane(0x0101, 1, A, true), lane(0x0201, 2, A, false)]);
        assert_eq!(lane_of(&s, 2, A).map(|l| l.id), Some(0x0201));
        assert_eq!(lane_of(&s, 1, A).map(|l| l.id), Some(0x0101));
        assert!(lane_of(&s, 1, B).is_none());
        assert!(lane_of(&s, 3, A).is_none());
    }

    #[test]
    fn known_addresses_and_path_bytes() {
        let mut w = world();
        let mesh = w[DevId::Client].mesh;
        assert!(w.known(SV, mesh) && !w.known(SV, A));
        offer(&mut w, A);
        assert!(w.known(SV, A) && !w.known(DevId::Client, A));
        w.chk.recvd(SV, 1, A, 60);
        w.chk.recvd(SV, 1, A, 40);
        assert_eq!(w.path_bytes(SV, 1, A), (0, 100));
        assert_eq!(w.path_bytes(SV, 2, A), (0, 0));
    }

    #[test]
    fn a_counter_is_sealed_once() {
        let mut w = world();
        let b = pkt(TYPE_DATA, 5, 7, 40, 1);
        w.check_sealed(SV, &b);
        w.check_sealed(SV, &b); // the same bytes again: a resend
        w.check_sealed(DevId::Client, &pkt(TYPE_DATA, 5, 7, 40, 2)); // another sender
        assert_eq!(w.failed(), None);
        w[SV].sessions += 1; // a new Session's key: its counters start again
        w.check_sealed(SV, &pkt(TYPE_DATA, 5, 7, 40, 3));
        assert_eq!(w.failed(), None);
        w.check_sealed(SV, &pkt(TYPE_DATA, 5, 7, 40, 4));
        let f = failed(&w);
        assert!(f.contains("xanth: counter 7 under rid 5 sealed twice"), "{f}");

        let mut w = world();
        w.check_sealed(SV, &pkt(TYPE_DATA, 5, 7 + SEAL_WINDOW as u64, 40, 1));
        w.check_sealed(SV, &pkt(TYPE_DATA, 5, 7, 40, 1));
        let f = failed(&w);
        assert!(f.contains("sent again after 131072 newer counters"), "{f}");
    }

    #[test]
    fn a_key_belongs_to_one_pair_of_processes() {
        let mut w = world();
        let m = Msg { t: "key".into(), rid: 9, k: vec![7; 32], ..Msg::default() };
        w.check_key(SV, &m);
        w.check_key(SV, &m);
        assert_eq!(w.failed(), None);
        w[SV].peer_inst ^= 2; // the client restarted
        let acts = [Action::Control(m)];
        call(&mut w, SV, Event::Tick, None, &acts, SessionStatus::default());
        let f = failed(&w);
        assert!(f.contains("key sent to an earlier process"), "{f}");
    }

    #[test]
    fn held_bytes_are_capped() {
        let mut w = world();
        let full = SessionStatus { bytes: BOND_BYTES, ..SessionStatus::default() };
        call(&mut w, SV, Event::Tick, None, &[], full.clone());
        assert_eq!(w.failed(), None);
        let over = SessionStatus { bytes: BOND_BYTES + 1, ..full };
        call(&mut w, SV, Event::Tick, None, &[], over);
        assert!(failed(&w).contains("above SessionBytes"), "{}", failed(&w));
    }

    #[test]
    fn malformed_sends_fail() {
        for (b, want) in [
            (vec![TYPE_DATA; 26], "sent a 26-byte lane packet"),
            (pkt(0x03, 1, 1, 40, 0), "unknown type 0x03"),
            (pkt(TYPE_PROBE | SMALL_BIT, 1, 1, 40, 0), "a probe with the small bit"),
        ] {
            let mut w = world();
            let acts = [send(1, A, b)];
            let s = status(vec![lane(0x0101, 1, A, true)]);
            call(&mut w, SV, Event::Tick, None, &acts, s);
            assert!(failed(&w).contains(want), "{want}: {}", failed(&w));
        }
    }

    #[test]
    fn data_goes_only_to_validated_addresses_ground_truth_reached() {
        let unvalidated = status(vec![lane(0x0101, 1, A, false)]);
        let valid = status(vec![lane(0x0101, 1, A, true)]);
        let acts = [send(1, A, pkt(TYPE_DATA | SMALL_BIT, 1, 1, 60, 0))];
        let mut w = world();
        call(&mut w, SV, Event::Tick, None, &acts, unvalidated.clone());
        let f = failed(&w);
        assert!(
            f.contains("data (byte 0 0x81) sent from socket 1 to 203.0.113.9:45776, which is not a validated lane"),
            "{f}"
        );

        // Validated per Snapshot, but nothing from socket 1 has reached the peer through A.
        let mut w = world();
        call(&mut w, SV, Event::Tick, None, &acts, valid.clone());
        assert!(failed(&w).contains("has reached the peer"), "{}", failed(&w));

        // Both: fine; validated in the before-snapshot counts too.
        let mut w = world();
        w.chk.reach.insert(PathKey { d: SV, sock: 1, addr: A });
        call(&mut w, SV, Event::Tick, None, &acts, valid);
        let acts = [send(1, A, pkt(TYPE_DATA, 1, 2, 60, 0))];
        call(&mut w, SV, Event::Tick, None, &acts, unvalidated);
        assert_eq!(w.failed(), None);
        assert_eq!(w.path_bytes(SV, 1, A), (0, 0), "bytes to a validated lane are not counted");

        // The Tailscale lane's mesh addresses need no ground truth.
        let mut w = world();
        let mesh = w[DevId::Client].mesh;
        let acts = [send(MESH_SOCK, mesh, pkt(TYPE_DATA, 1, 1, 60, 0))];
        let s = status(vec![lane(0, MESH_SOCK, mesh, true)]);
        call(&mut w, SV, Event::Tick, None, &acts, s);
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn an_unvalidated_address_gets_probes_within_validate_amp() {
        let probes = |from: u64, n: u64, to: SocketAddr, sock: SockId| -> Vec<Action> {
            let p = |i| send(sock, to, pkt(TYPE_PROBE, 1, from + i, 60, 0));
            (0..n).map(p).collect()
        };
        let mut w = world();
        w.chk.recvd(SV, 1, THIRD, 60);
        let none = SessionStatus::default();
        call(&mut w, SV, Event::Tick, None, &probes(0, 3, THIRD, 1), none.clone());
        assert_eq!(w.failed(), None);
        assert_eq!(w.path_bytes(SV, 1, THIRD), (180, 60));
        call(&mut w, SV, Event::Tick, None, &probes(3, 1, THIRD, 1), none.clone());
        let f = failed(&w);
        assert!(f.contains("240 bytes sent to unvalidated 203.0.113.66:4444, which sent 60 (ValidateAmp 3)"), "{f}");

        // Punching an offered candidate, or the peer's mesh address, has no limit.
        let mut w = world();
        let mesh = w[DevId::Client].mesh;
        offer(&mut w, A);
        let mut acts = probes(0, 10, A, 1);
        acts.extend(probes(10, 10, mesh, MESH_SOCK));
        call(&mut w, SV, Event::Tick, None, &acts, none);
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn a_relayed_probe_gets_probes_only() {
        // A genuine probe relayed from a third address ahead of the original: one probe of no
        // more bytes back to it is fine, while the lane keeps its address (R2-03: core asks the
        // relay for an answer it cannot give).
        let s = status(vec![lane(0x0101, 1, A, true)]);
        let mut w = world();
        w.chk.reach.insert(PathKey { d: SV, sock: 1, addr: A });
        w[SV].last_snap = s.clone();
        let relayed = pkt(TYPE_PROBE, 3, 50, 80, 9);
        let acts = [send(1, THIRD, pkt(TYPE_PROBE, 1, 1, 80, 0))];
        arrive(&mut w, (THIRD, 1), &relayed, Adv::Relay, &acts, s.clone());
        assert_eq!(w.failed(), None);
        assert_eq!(w.path_bytes(SV, 1, THIRD), (80, 80));

        // Data to it fails whether or not core calls it validated.
        let data = [send(1, THIRD, pkt(TYPE_DATA, 1, 2, 80, 0))];
        let mut w = world();
        call(&mut w, SV, Event::Tick, None, &data, s);
        assert!(failed(&w).contains("not a validated lane"), "{}", failed(&w));
        let mut w = world();
        let s = status(vec![lane(0x0101, 1, THIRD, true)]);
        call(&mut w, SV, Event::Tick, None, &data, s);
        assert!(failed(&w).contains("has reached the peer"), "{}", failed(&w));
    }

    #[test]
    fn nothing_answers_the_adversarys_packets() {
        let s = status(vec![lane(0x0101, 1, A, true)]);
        let acts = [send(1, A, pkt(TYPE_PROBE, 1, 1, 60, 0))];
        for adv in [Adv::Corrupt, Adv::Forge, Adv::WrongRid] {
            let mut w = world();
            let b = pkt(TYPE_DATA, 3, 50, 80, 9);
            arrive(&mut w, (A, 1), &b, adv, &acts, s.clone());
            let want = format!("xanth: answered a packet the adversary made (kind {adv:?}) from");
            assert!(failed(&w).contains(&want), "{}", failed(&w));
        }
        // Genuine bytes may be answered, a duplicate's included; the answer is not a reply when
        // it goes elsewhere.
        let mut w = world();
        arrive(&mut w, (A, 1), &pkt(TYPE_DATA, 3, 50, 80, 9), Adv::Dup, &acts, s.clone());
        let b = pkt(TYPE_DATA, 3, 51, 80, 9);
        arrive(&mut w, (B, 1), &b, Adv::Forge, &acts, s);
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn no_reply_is_larger_than_its_trigger() {
        let valid = status(vec![lane(0x0101, 1, A, true)]);
        let probe = pkt(TYPE_PROBE, 3, 50, 60, 9);
        let big = [send(1, A, pkt(TYPE_PROBE, 1, 1, 61, 0))];

        // Active, toward a validated lane: the lane's regular probes are not replies.
        let mut w = world();
        w[SV].last_snap = valid.clone();
        arrive(&mut w, (A, 1), &probe, Adv::Genuine, &big, valid.clone());
        assert_eq!(w.failed(), None);

        // Idle: the answer to the idle probe is no larger than the probe.
        let mut w = world();
        w[SV].last_snap = valid.clone();
        w[SV].last_data = w.now - IDLE_AFTER;
        let same = [send(1, A, pkt(TYPE_PROBE, 1, 1, 60, 0))];
        arrive(&mut w, (A, 1), &probe, Adv::Genuine, &same, valid.clone());
        assert_eq!(w.failed(), None);
        let next = pkt(TYPE_PROBE, 3, 51, 60, 9);
        let big = [send(1, A, pkt(TYPE_PROBE, 1, 2, 61, 0))];
        arrive(&mut w, (A, 1), &next, Adv::Genuine, &big, valid);
        let f = failed(&w);
        assert!(f.contains("xanth: answered a 60-byte probe from 203.0.113.9:45776 with 61 bytes of probes"), "{f}");

        // Active, toward an address that is neither a validated lane nor a candidate ...
        let mut w = world();
        arrive(&mut w, (A, 1), &probe, Adv::Genuine, &big, SessionStatus::default());
        assert!(failed(&w).contains("answered a 60-byte probe"), "{}", failed(&w));
        // ... unless the peer offered it.
        let mut w = world();
        offer(&mut w, A);
        arrive(&mut w, (A, 1), &probe, Adv::Genuine, &big, SessionStatus::default());
        assert_eq!(w.failed(), None);
    }

    /// The server's lane 0101 at A, validated, and packet b (made by adv) from B on socket sock:
    /// the failure, if any, when the lane is at B after that call (validated per `valid`).
    fn moved_by(w: &mut World, b: &[u8], adv: Adv, sock: SockId, valid: bool) -> String {
        w[SV].last_snap = status(vec![lane(0x0101, 1, A, true)]);
        let after = status(vec![lane(0x0101, 1, B, valid)]);
        arrive(w, (B, sock), b, adv, &[], after);
        failed(w)
    }

    #[test]
    fn a_lane_moves_only_on_a_newer_authentic_probe_from_its_new_address() {
        let probe = pkt(TYPE_PROBE, 3, 50, 80, 9);
        let mut w = world();
        assert_eq!(moved_by(&mut w, &probe, Adv::Genuine, 1, false), "");
        // The same bytes again (a duplicate, a replay, the original behind its relay) are not
        // newer.
        let f = moved_by(&mut w, &probe, Adv::Dup, 1, false);
        let want = "xanth: lane 0101 moved 203.0.113.9:45776 → 203.0.113.10:57153 without a \
                    newer authenticated probe from it on its socket";
        assert!(f.contains(want), "{f}");

        // A later Session's probe that reuses the rid and counter is another packet (the swarm's
        // seed 4 in notes/open.md: a restart's fresh rid table and key).
        let mut w = world();
        assert_eq!(moved_by(&mut w, &probe, Adv::Genuine, 1, true), "");
        let next = pkt(TYPE_PROBE, 3, 50, 80, 10);
        assert_eq!(moved_by(&mut w, &next, Adv::Genuine, 1, true), "");

        // Not authentic, on another socket, or data.
        let data = pkt(TYPE_DATA, 3, 50, 80, 9);
        for (b, adv, sock) in [
            (&probe, Adv::Forge, 1),
            (&probe, Adv::Corrupt, 1),
            (&probe, Adv::WrongRid, 1),
            (&probe, Adv::Genuine, 2),
            (&data, Adv::Genuine, 1),
        ] {
            let mut w = world();
            let f = moved_by(&mut w, b, adv, sock, false);
            assert!(f.contains("without a newer authenticated probe"), "{adv:?} {sock}: {f}");
        }
    }

    #[test]
    fn a_move_outside_a_packet_is_to_a_candidate_and_unvalidated() {
        let before = status(vec![lane(0x0101, 1, A, true)]);
        let to_b = |valid| status(vec![lane(0x0101, 1, B, valid)]);
        let mut w = world();
        offer(&mut w, B);
        w[SV].last_snap = before.clone();
        call(&mut w, SV, Event::Tick, None, &[], to_b(false));
        assert_eq!(w.failed(), None);

        let mut w = world();
        w[SV].last_snap = before.clone();
        call(&mut w, SV, Event::Tick, None, &[], to_b(false));
        let f = failed(&w);
        assert!(f.contains("lane 0101 moved to 203.0.113.10:57153, which no candidate named"), "{f}");

        let mut w = world();
        offer(&mut w, B);
        w[SV].last_snap = before;
        call(&mut w, SV, Event::Wake, None, &[], to_b(true));
        assert!(failed(&w).contains("without a newer authenticated probe"), "{}", failed(&w));

        // A lane without an address, or a new lane, may take one.
        let mut w = world();
        let mut none = lane(0x0101, 1, A, false);
        none.peer = None;
        w[SV].last_snap = status(vec![none]);
        let after = status(vec![lane(0x0101, 1, B, false), lane(0x0201, 2, A, false)]);
        call(&mut w, SV, Event::Tick, None, &[], after);
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn the_tailscale_lane_never_moves() {
        let mut w = world();
        let mesh = w[DevId::Client].mesh;
        let at = |a| status(vec![lane(0, MESH_SOCK, a, true)]);
        call(&mut w, SV, Event::Tick, None, &[], at(mesh));
        assert_eq!(w.failed(), None);
        call(&mut w, SV, Event::Tick, None, &[], at(A));
        let f = failed(&w);
        assert!(f.contains("the Tailscale lane's address is 203.0.113.9:45776"), "{f}");
    }

    #[test]
    fn check_after_keeps_the_snapshot_for_the_next_call() {
        let mut w = world();
        let s = status(vec![lane(0x0101, 1, A, true)]);
        call(&mut w, SV, Event::Tick, None, &[], s.clone());
        assert_eq!(w[SV].last_snap, s);
    }

    fn recorded(b: &[u8]) -> Recorded {
        Recorded { sock: 1, src: A, data: b.to_vec() }
    }

    #[test]
    fn the_adversary_makes_each_kind() {
        let mut w = world();
        let now = w.now;
        let a = w.new_adversary(SV);
        assert!(a.on && a.third == THIRD && a.target == SV && a.p == [0.0; ADV_KINDS]);
        let probe = pkt(TYPE_PROBE, 0x0a0b, 1000, 80, 7);
        let data = pkt(TYPE_DATA, 0x0a0b, 1001, 120, 8);
        a.rec.push(recorded(&data));

        let s = a.adv_make(Adv::Relay, recorded(&probe), now).unwrap();
        let got = (s.at, s.sock, s.src, &s.pkt.data, s.pkt.adv);
        assert_eq!(got, (now, 1, THIRD, &probe, Adv::Relay));
        assert!(a.adv_make(Adv::Relay, recorded(&data), now).is_none(), "a relay of data");
        assert!(a.adv_make(Adv::Genuine, recorded(&data), now).is_none());
        assert!(a.adv_make(Adv::Dup, recorded(&[TYPE_DATA; 26]), now).is_none());

        for k in [Adv::Replay, Adv::Dup, Adv::Corrupt, Adv::Forge, Adv::WrongRid] {
            let s = a.adv_make(k, recorded(&probe), now).unwrap();
            assert!(s.at >= now && s.at < now + 2 * MS, "{k:?} within 2 ms");
            assert_eq!((s.pkt.adv, s.pkt.from, s.pkt.t0), (k, None, now));
            let b = &s.pkt.data;
            match k {
                Adv::Replay => assert_eq!((b, s.src), (&data, A)),
                Adv::Dup => assert_eq!((b, s.src), (&probe, A)),
                Adv::Corrupt => {
                    let bits = b.iter().zip(&probe).map(|(x, y)| (x ^ y).count_ones());
                    assert_eq!(bits.sum::<u32>(), 1);
                }
                Adv::Forge => {
                    let ((rid, ctr), (_, old)) = (header(b), header(&probe));
                    assert!(b[0] == TYPE_PROBE && rid == 0x0a0b);
                    assert!(ctr > old && ctr <= old + 1000, "{ctr}");
                    assert_ne!(b[HEADER_LEN..], probe[HEADER_LEN..]);
                }
                _ => {
                    assert_ne!(header(b).0, 0x0a0b);
                    assert_eq!((b[0], &b[3..]), (probe[0], &probe[3..]));
                }
            }
        }
        assert_eq!(a.injected, [0, 1, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn the_adversary_sees_only_its_targets_genuine_packets() {
        let mut w = world();
        let probe = Pkt::new(pkt(TYPE_PROBE, 3, 50, 80, 9));
        assert_eq!(w.adv_see(SV, 1, A, &probe), 0, "no adversary");
        w.new_adversary(SV).p[Adv::Relay.idx()] = 1.0;
        let q = w.q.len();
        assert_eq!(w.adv_see(DevId::Client, 1, A, &probe), 0, "not the target");
        let forged = Pkt { adv: Adv::Forge, ..probe.clone() };
        assert_eq!(w.adv_see(SV, 1, A, &forged), 0, "not genuine");
        let short = Pkt::new(vec![TYPE_PROBE; 26]);
        assert_eq!(w.adv_see(SV, 1, A, &short), 0, "too short");
        w.adv.as_mut().unwrap().on = false;
        assert_eq!(w.adv_see(SV, 1, A, &probe), 0, "off");
        assert!(w.adv.as_ref().unwrap().rec.is_empty() && w.q.len() == q);

        // A relayed probe gets 5 ms ahead of the original; data is recorded, not relayed.
        w.adv.as_mut().unwrap().on = true;
        assert_eq!(w.adv_see(SV, 1, A, &probe), 5 * MS);
        let data = Pkt::new(pkt(TYPE_DATA, 3, 51, 80, 9));
        assert_eq!(w.adv_see(SV, 1, A, &data), 0);
        let a = w.adv.as_ref().unwrap();
        let got = (a.rec.len(), a.injected[Adv::Relay.idx()], w.q.len());
        assert_eq!(got, (2, 1, q + 1));
    }

    #[test]
    fn the_adversary_injects_with_its_probabilities() {
        let mut w = world();
        w.new_adversary(SV).p = [0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
        let q = w.q.len();
        let probe = Pkt::new(pkt(TYPE_PROBE, 3, 50, 80, 9));
        assert_eq!(w.adv_see(SV, 1, A, &probe), 5 * MS);
        assert_eq!(w.adv.as_ref().unwrap().injected, [0, 1, 1, 1, 1, 1, 1]);
        assert_eq!(w.q.len(), q + 6);

        let mut w = world();
        w.new_adversary(SV).p[Adv::Dup.idx()] = 0.25;
        for i in 0..4000 {
            w.adv_see(SV, 1, A, &Pkt::new(pkt(TYPE_DATA, 3, i, 80, 9)));
        }
        let n = w.adv.as_ref().unwrap().injected[Adv::Dup.idx()];
        assert!((900..1100).contains(&n), "{n} duplicates of 4,000 at 25%");
    }

    #[test]
    fn the_adversary_keeps_4096_packets_and_replays_them_all() {
        let mut w = world();
        w.new_adversary(SV);
        for i in 0..5000 {
            let b = pkt(TYPE_DATA, 3, i, 40 + (i % 50) as usize, 9);
            w.adv_see(SV, 1, A, &Pkt::new(b));
        }
        let a = w.adv.as_ref().unwrap();
        assert_eq!(a.rec.len(), ADV_RECORD);
        let later = a.rec.iter().any(|r| header(&r.data).1 >= ADV_RECORD as u64);
        assert!(later, "later packets replace earlier ones");
        let q = w.q.len();
        w.replay_all();
        let a = w.adv.as_ref().unwrap();
        let got = (a.injected[Adv::Replay.idx()], a.rec.len(), w.q.len());
        assert_eq!(got, (ADV_RECORD as u64, ADV_RECORD, q + ADV_RECORD));
    }
}
