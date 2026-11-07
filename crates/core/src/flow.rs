//! Flows (SPEC 2.0 §13, §15): keys, numbering, classes, sealing inner packets once, copies and
//! bulk, and per-flow order on receive.
//!
//! A flow slot holds one flow direction: one I send (`mine`, numbered by me and found by its
//! [`Key`] in `Session::tx_flows`) or one I receive (numbered by the peer and found by that number
//! in `Session::rx_flows`). Slots live in `Session::flows`, at most MAX_FLOWS; a new flow beyond
//! that evicts the flow idle longest, and a flow without a packet for FLOW_IDLE ends (`expire`).
//! `end_flow` frees a slot whole (R2-12): its held buffers go back to the pool and the slot is
//! `Flow::default()` again. A new flow allocates its reorder buffer on its first gap.
//!
//! Flow seqs are compared in serial arithmetic (`after`), so a flow keeps its order across the
//! u32 wrap (R2-24). The reorder timer learns from a late packet only if it belongs to the last
//! gap given up and came within REORDER_RESCUE of it, and `fill` never exceeds what makes the
//! timer REORDER_MAX (R2-13). A small packet that passes held bulk of its own flow delivers what
//! is held before it first, so held bulk never comes behind it.
//!
//! 2.0 wave 2 (notes/2.0-build.md): index slots without scans, per-lane first arrivals, rescues
//! and reorder wait (§11.5).

use std::net::IpAddr;

use crate::buf::{BufIx, Pkt};
use crate::estimate::{Meter, NO_MIN};
use crate::lane::{Lane, LaneIx, State};
use crate::performance::apart;
use crate::session::Mode;
use crate::wire::{after, be};
use crate::{
    BOND_BYTES, DATA_LEN, DUP_GRACE, FLOW_IDLE, FlowId, HEADER_LEN, MAX_ECHOES, MAX_FLOWS, MAX_INNER, MAX_LINKS, Mono,
    Out, RATE_WINDOW, REORDER_LANES, REORDER_MAX, REORDER_RESCUE, REORDER_SHARE, REORDER_SLOTS, SKEW_DECAY,
    SKEW_MARGIN, SMALL_BIT, SMALL_COPY_BUDGET, SMALL_RATE, SMALL_WINDOW, Session, TYPE_DATA,
};

/// Lanes remembered per flow for reorder's lane rule (§15).
pub(crate) const FLOW_LANES: usize = 4;
/// A lane outside the spread takes small-flow copies only while its send rate plus the copies
/// stays under this share of its rate.
pub(crate) const COPY_LOAD_MAX: f64 = 0.5;

const ICMP: u8 = 1;
const TCP: u8 = 6;
const UDP: u8 = 17;
const ICMP6: u8 = 58;

/// A flow's key (§13): the protocol and addresses, plus the source and destination port (TCP,
/// UDP) or the echo id in `sport` (ICMP and ICMPv6 echo). Any other protocol, and every fragment
/// of a datagram, the first included (IPv4: MF set or a non-zero offset; IPv6: a Fragment header),
/// has ports 0, so a datagram's fragments share one flow and keep their order (a first fragment
/// keyed by its ports went into another flow than the rest, and the kernel failed to reassemble
/// them). The two directions of one conversation have different keys (source and destination
/// swap), and are different flows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    /// The IP protocol number; for IPv6, the upper-layer header's, after extension headers.
    pub proto: u8,
    pub src: IpAddr,
    pub dst: IpAddr,
    pub sport: u16,
    pub dport: u16,
}

impl Key {
    /// The key of one whole IPv4 or IPv6 packet, from its headers; None if `p` does not start with
    /// a whole IP header. IPv6's hop-by-hop, routing and destination-options headers are skipped
    /// (at most 8); a Fragment header ends the walk, its next header being the protocol, which
    /// every fragment of the datagram carries alike. Never panics on any input.
    pub fn parse(p: &[u8]) -> Option<Key> {
        match p.first()? >> 4 {
            4 => {
                let ihl = usize::from(p[0] & 0x0f) * 4;
                if ihl < 20 || p.len() < ihl {
                    return None;
                }
                let src = IpAddr::from(<[u8; 4]>::try_from(&p[12..16]).ok()?);
                let dst = IpAddr::from(<[u8; 4]>::try_from(&p[16..20]).ok()?);
                let frag = u16::from_be_bytes([p[6], p[7]]) & 0x3fff != 0; // MF or an offset
                let (sport, dport) = if frag { (0, 0) } else { ports(p[9], ICMP, &p[ihl..]) };
                Some(Key { proto: p[9], src, dst, sport, dport })
            }
            6 => {
                if p.len() < 40 {
                    return None;
                }
                let src = IpAddr::from(<[u8; 16]>::try_from(&p[8..24]).ok()?);
                let dst = IpAddr::from(<[u8; 16]>::try_from(&p[24..40]).ok()?);
                let (mut next, mut at) = (p[6], 40usize);
                for _ in 0..8 {
                    match next {
                        0 | 43 | 60 => {
                            let h = p.get(at..at + 2)?;
                            (next, at) = (h[0], at + (usize::from(h[1]) + 1) * 8);
                        }
                        44 => {
                            let h = p.get(at..at + 8)?;
                            return Some(Key { proto: h[0], src, dst, sport: 0, dport: 0 });
                        }
                        _ => break,
                    }
                }
                let l4 = p.get(at..).unwrap_or_default();
                let (sport, dport) = ports(next, ICMP6, l4);
                Some(Key { proto: next, src, dst, sport, dport })
            }
            _ => None,
        }
    }
}

/// The ports at the front of a TCP or UDP header, or the echo id of an ICMP (`icmp`: ICMP or
/// ICMPv6) echo request or reply; (0, 0) for anything else or a truncated header.
fn ports(proto: u8, icmp: u8, l4: &[u8]) -> (u16, u16) {
    let be16 = |i: usize| u16::from_be_bytes([l4[i], l4[i + 1]]);
    let echo = |t: u8| if icmp == ICMP { t == 8 || t == 0 } else { t == 128 || t == 129 };
    match proto {
        TCP | UDP if l4.len() >= 4 => (be16(0), be16(2)),
        p if p == icmp && l4.len() >= 6 && echo(l4[0]) => (be16(4), 0),
        _ => (0, 0),
    }
}

/// The largest `fill` kept: SKEW_MARGIN × it is REORDER_MAX, the timer's cap (R2-13).
const FILL_MAX: Mono = (REORDER_MAX as f64 / SKEW_MARGIN) as Mono;

/// Pool bytes past which a datagram behind a gap is delivered out of order rather than held, so
/// reorder buffers leave room for pacer queues within BOND_BYTES (§7.6 rule 5).
const REORDER_CAP: usize = (BOND_BYTES as f64 * REORDER_SHARE) as usize;

/// A lane that recently brought a flow's packets, for reorder rule 2 (§10.3). Go: the anonymous
/// struct in `flow.lanes`. `l` None: empty, or a lane that has since gone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FlowLane {
    pub(crate) l: Option<LaneIx>,
    pub(crate) at: Mono,
    /// The highest seq it brought.
    pub(crate) seq: u32,
}

/// One forwarded UDP flow, keyed by FlowId in both directions (§5.1, §10). Go: `flow`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Flow {
    pub(crate) id: FlowId,
    pub(crate) used: bool,
    /// I send this flow (its number is mine), or receive it (the peer's).
    pub(crate) mine: bool,
    /// Its key (§13): a flow I send is found by it; one I receive learns it from its first packet.
    pub(crate) key: Option<Key>,
    /// Its last packet, for FLOW_IDLE and eviction.
    pub(crate) last: Mono,
    /// Class of my sending direction (§10.1); a new flow starts small.
    pub(crate) small: bool,
    /// The peer's direction carries the small bit.
    pub(crate) rx_small: bool,
    /// The last seq I sent.
    pub(crate) tx_seq: u32,
    /// My sending rate when last classified.
    pub(crate) r: f64,
    pub(crate) tx: Meter,
    pub(crate) rx: Meter,

    // Receiving: per-flow order (§10.3).
    /// The next seq to deliver; a new flow starts at 1.
    pub(crate) next: u32,
    /// By seq % REORDER_SLOTS (`buf` None when empty); empty until the first gap, then
    /// REORDER_SLOTS long.
    pub(crate) held: Vec<Pkt>,
    /// Datagrams held.
    pub(crate) n: usize,
    /// A decaying maximum of how long gaps took to fill, at most FILL_MAX (R2-13).
    pub(crate) fill: Mono,
    /// When the datagram behind the last gap given up arrived.
    pub(crate) gap_at: Mono,
    /// The seqs that gap skipped: `gap_n` of them from `gap_seq`. Only a late datagram among them
    /// teaches `fill` how long a gap needs; `gap_n` 0 (a new flow, or after a jump) teaches
    /// nothing (R2-13). Rust only: a new field (notes/rust.md, C5).
    pub(crate) gap_seq: u32,
    pub(crate) gap_n: u32,
    pub(crate) lanes: [FlowLane; FLOW_LANES],
    /// The lane id + 1 each held seq came on (0: none), by seq % REORDER_SLOTS, beside `held`,
    /// for reorder wait (§11.5).
    pub(crate) held_on: Vec<u16>,
}

impl Flow {
    /// Notes that lane `l` brought `seq` of the flow, in l's entry, else an empty one, else the
    /// oldest. The entry keeps the highest seq in serial order, a new one starting from `seq`
    /// itself (R2-24: from 0, a seq past 2^31 would never count as higher). An empty entry goes
    /// before the oldest: Go compared `at` alone, and Mono's origin is arbitrary, so a lane
    /// recorded at or before 0 gave way while entries were free.
    pub(crate) fn carried(&mut self, l: LaneIx, now: Mono, seq: u32) {
        // Two steps, not one `min_by_key` over all four: this runs for every bulk packet, and
        // `find` stops at the lane's own entry.
        if let Some(e) = self.lanes.iter_mut().find(|e| e.l == Some(l)) {
            e.at = now;
            if after(seq, e.seq) {
                e.seq = seq;
            }
            return;
        }
        if let Some(e) = self.lanes.iter_mut().min_by_key(|e| (e.l.is_some(), e.at)) {
            *e = FlowLane { l: Some(l), at: now, seq };
        }
    }

    /// When seq `q` arrived, if it is held. With nothing held (`n` 0) the buffer is not read.
    fn held_at(&self, q: u32) -> Option<Mono> {
        let h = self.held.get(q as usize % REORDER_SLOTS).filter(|_| self.n > 0)?;
        (h.buf.is_some() && h.c == u64::from(q)).then_some(h.at)
    }
}

impl Session {
    /// The slot of the flow I receive under the peer's number `id`, or with `open` a new slot for
    /// it (§13).
    pub(crate) fn flow(&mut self, id: FlowId, open: bool) -> Option<usize> {
        if let Some(&f) = self.rx_flows.get(&id) {
            return Some(f);
        }
        if !open {
            return None;
        }
        let f = self.new_slot()?;
        self.flows[f] = Flow { id, used: true, small: true, next: 1, ..Flow::default() };
        self.rx_flows.insert(id, f);
        Some(f)
    }

    /// The slot of the flow I send with key `k`, numbering a new one (§13): numbers run from 1
    /// and skip those in use, so a number comes back only after its flow ended.
    pub(crate) fn tx_flow(&mut self, k: Key) -> Option<usize> {
        if let Some(&f) = self.tx_flows.get(&k) {
            return Some(f);
        }
        let f = self.new_slot()?;
        let mut id = FlowId(self.next_flow.max(1));
        while self.flows.iter().any(|x| x.used && x.mine && x.id == id) {
            id = FlowId(id.0.checked_add(1).unwrap_or(1));
        }
        self.next_flow = id.0.checked_add(1).unwrap_or(1);
        self.flows[f] = Flow { id, used: true, mine: true, key: Some(k), small: true, next: 1, ..Flow::default() };
        self.tx_flows.insert(k, f);
        Some(f)
    }

    /// A free slot: an unused one, a new one below MAX_FLOWS, else the flow idle longest, ended
    /// and counted in `flows_dropped` (§13).
    fn new_slot(&mut self) -> Option<usize> {
        if let Some(f) = self.flows.iter().position(|f| !f.used) {
            return Some(f);
        }
        if self.flows.len() < MAX_FLOWS {
            self.flows.push(Flow::default());
            return Some(self.flows.len() - 1);
        }
        let f = (0..self.flows.len()).min_by_key(|&f| self.flows[f].last)?;
        self.flows_dropped += 1;
        self.end_flow(f);
        Some(f)
    }

    /// Frees flow slot `f` whole (R2-12): its held buffers go back to the pool, and its key or
    /// number is forgotten.
    pub(crate) fn end_flow(&mut self, f: usize) {
        let fl = std::mem::take(&mut self.flows[f]);
        if !fl.used {
            return;
        }
        match fl.key.filter(|_| fl.mine) {
            Some(k) => self.tx_flows.remove(&k),
            None => self.rx_flows.remove(&fl.id),
        };
        for b in fl.held.into_iter().filter_map(|h| h.buf) {
            self.pool.put(b);
        }
    }

    /// Ends every flow without a packet for FLOW_IDLE (§13), and asks for the tick that ends the
    /// next one, so an idle session's flows end on time too.
    pub(crate) fn expire(&mut self, now: Mono) {
        let mut next = Mono::MAX;
        for f in 0..self.flows.len() {
            if !self.flows[f].used {
                continue;
            }
            let end = self.flows[f].last.saturating_add(FLOW_IDLE);
            if now >= end {
                self.end_flow(f);
            } else {
                next = next.min(end);
            }
        }
        self.wake_at(next);
    }

    /// Notes data in either direction; the first after idle wakes the session (§8.1).
    pub(crate) fn touch(&mut self, now: Mono) {
        if !self.active {
            self.activate(now);
        }
        self.last_data = now;
    }

    /// Marks each flow's sending direction small or bulk (§10.1): under SMALL_RATE over
    /// SMALL_WINDOW, cheapest first, until the small flows total SMALL_COPY_BUDGET. Sorts slot indices
    /// with `sort_unstable_by` and the slot as tiebreak (Go's stable sort; no allocation).
    pub(crate) fn classify(&mut self, now: Mono) {
        let mut fs = [0usize; MAX_FLOWS];
        let mut n = 0;
        for (i, f) in self.flows.iter_mut().enumerate().filter(|(_, f)| f.used && f.mine) {
            f.r = f.tx.rate(now, SMALL_WINDOW);
            fs[n] = i;
            n += 1;
        }
        fs[..n].sort_unstable_by(|&a, &b| self.flows[a].r.total_cmp(&self.flows[b].r).then(a.cmp(&b)));
        self.small_rate = 0.0;
        for &i in &fs[..n] {
            let f = &mut self.flows[i];
            f.small = f.r < SMALL_RATE && self.small_rate + f.r <= SMALL_COPY_BUDGET;
            if f.small {
                self.small_rate += f.r;
            }
        }
    }

    /// Tests: one datagram of test flow `id`, sent as `pair::inner` wraps it.
    #[cfg(test)]
    pub(crate) fn app(&mut self, out: &mut dyn Out, now: Mono, id: FlowId, d: &[u8]) {
        self.send_inner(out, now, &crate::pair::inner(id, d));
    }

    /// Seals one inner IP packet from the TUN device once and sends it as small or bulk (§10.3,
    /// §13): its flow is found by key, or numbered.
    pub(crate) fn send_inner(&mut self, out: &mut dyn Out, now: Mono, d: &[u8]) {
        if self.tx.rid == 0 || d.len() > MAX_INNER {
            return; // before the peer's key, or too large to carry
        }
        let Some(k) = Key::parse(d) else { return };
        let Some(f) = self.tx_flow(k) else { return };
        let Some(i) = self.pool.get() else { return };
        self.touch(now);
        let fl = &mut self.flows[f];
        fl.last = now;
        fl.tx_seq = fl.tx_seq.wrapping_add(1);
        fl.tx.add(now, SMALL_WINDOW, d.len());
        // A new flow starts small: one that reaches SMALL_RATE turns bulk at once, before its
        // bursts go unpaced on every copy lane until the next classify.
        fl.small = fl.small && fl.tx.rate(now, SMALL_WINDOW) < SMALL_RATE;
        let (small, seq, id) = (fl.small, fl.tx_seq, fl.id);
        let b = self.pool.buf_mut(i);
        b[HEADER_LEN..HEADER_LEN + 2].copy_from_slice(&id.0.to_be_bytes()); // flow
        b[HEADER_LEN + 2..HEADER_LEN + DATA_LEN].copy_from_slice(&seq.to_be_bytes());
        b[HEADER_LEN + DATA_LEN..HEADER_LEN + DATA_LEN + d.len()].copy_from_slice(d);
        let len = self.seal(out, i, if small { TYPE_DATA | SMALL_BIT } else { TYPE_DATA }, DATA_LEN + d.len());
        if small { self.copies(out, now, i, len) } else { self.bulk(out, now, i, len) }
    }

    /// Sends the sealed small packet in pool buffer `buf` (`n` bytes) on every lane of the copy
    /// set, unpaced, or on every known lane while none is confirmed (§8.1, §10.1); each lane's
    /// rewind ring keeps its counter. Then puts the buffer back.
    fn copies(&mut self, out: &mut dyn Out, now: Mono, buf: BufIx, n: usize) {
        let c = be(&self.pool.buf(buf)[3..HEADER_LEN]); // its counter (§7.3)
        let mut sent = false;
        for l in self.lanes.ixs() {
            let x = &self.lanes[l];
            let Some(to) = x.peer.filter(|_| self.confirmed && x.in_copy && x.validated) else { continue };
            self.send(out, l, to, buf, n);
            self.lanes[l].copy_bytes += n as u64;
            self.keep(now, l, Pkt { c, at: now, buf: None, n: 0 });
            sent = true;
        }
        if !sent {
            self.send_all(out, buf, n);
        }
        self.pool.put(buf);
    }

    /// Queues the sealed bulk packet in pool buffer `buf` (`n` bytes) on the lanes the scheduler
    /// chose (`bulk_lanes`); each extra lane gets its own copy of the bytes; with none, unpaced,
    /// once per bottleneck group (`send_unpaced`; §11).
    fn bulk(&mut self, out: &mut dyn Out, now: Mono, buf: BufIx, n: usize) {
        let p = Pkt { c: be(&self.pool.buf(buf)[3..HEADER_LEN]), at: now, buf: Some(buf), n: n as u32 };
        let mut ls = self.bulk_lanes(now, n);
        let Some(first) = ls.next() else {
            // No lane carries bulk: never worse than one lane (notes/core.md 24).
            self.send_unpaced(out, now, buf, n);
            self.pool.put(buf);
            return;
        };
        for l in ls {
            if let Some(i) = self.pool.get() {
                self.pool.copy(buf, i, n);
                self.enqueue(out, now, l, Pkt { buf: Some(i), ..p });
            }
        }
        self.enqueue(out, now, first, p);
    }

    /// Sends the sealed bulk packet in pool buffer `buf` (`n` bytes), which no lane carries, at
    /// once (notes/core.md 24, 37): on every validated lane but those that share a bottleneck
    /// group (§6.1) with a validated lane that answers and comes before them, so each group's
    /// shaper takes the stream once. A lane answers if it was echoed within its suspect timeout;
    /// one that answers comes first, a direct lane before the Tailscale lane (§11.1 rule 6), then
    /// the lower OWD. A group none of whose lanes answers (at start, after idle, blind) still gets
    /// the packet on every validated lane: never worse than one.
    ///
    /// Go sent it on every validated lane (`sendAll`). Once every lane was down, as after a sleep
    /// longer than the probe ring, the direct lane and the Tailscale lane over the same hotel
    /// Wi-Fi took the stream twice through its shaper. The overuse this caused reset `clean_at` on
    /// every echo, so no lane had the clean UP_AFTER that brings it back up and bulk stayed
    /// doubled: video never played again (the swarm's seed 5; notes/rust.md, accept:
    /// integration).
    fn send_unpaced(&mut self, out: &mut dyn Out, now: Mono, buf: BufIx, n: usize) {
        // Whether a lane answers: `owds` holds a sample once a lane was echoed since it was made
        // or I woke, and the newest echo is within its suspect timeout.
        let answers = |s: &Session, l: LaneIx| {
            s.lanes[l].owds.n > 0 && now.saturating_sub(s.lanes[l].last_echo) <= s.suspect_after(l)
        };
        for l in self.lanes.ixs() {
            let x = &self.lanes[l];
            let Some(to) = x.peer.filter(|_| x.validated) else { continue };
            let behind = self.lanes.ixs().any(|m| {
                let y = &self.lanes[m];
                m != l
                    && y.validated
                    && y.peer.is_some()
                    && !apart(x, y)
                    && answers(self, m)
                    && (!answers(self, l) || x.mesh && !y.mesh || x.mesh == y.mesh && (y.owd, m) < (x.owd, l))
            });
            if !behind {
                self.send(out, l, to, buf, n);
            }
        }
    }

    /// Picks the lanes small packets go on (§10.1) and tells the peer at once when one joins or
    /// leaves (§8.4): the lanes carrying bulk; the lowest-OWD validated direct lane of each group
    /// that answers, while its send rate plus the copies stays under COPY_LOAD_MAX × its rate;
    /// and the Tailscale lane while fewer than two groups have such a lane.
    pub(crate) fn set_copies(&mut self, out: &mut dyn Out, now: Mono) {
        // The best lane of each bottleneck group (§6.1), by group index: direct lanes only.
        let mut best = [None::<LaneIx>; MAX_LINKS];
        let answers = |x: &Lane| matches!(x.state, State::Up | State::Suspect);
        for l in self.lanes.ixs().filter(|&l| !self.lanes[l].mesh) {
            let x = &self.lanes[l];
            let Some(b) = usize::try_from(x.group).ok().and_then(|g| best.get_mut(g)) else { continue };
            if x.validated && answers(x) && b.is_none_or(|b| x.owd < self.lanes[b].owd) {
                *b = Some(l);
            }
        }
        let groups = best.iter().flatten().count();
        for l in self.lanes.ixs() {
            let carries = self.carries(l);
            let x = &mut self.lanes[l];
            let copy = carries
                || x.mesh && groups < 2 && answers(x)
                || best.contains(&Some(l))
                    && x.send_rate.rate(now, RATE_WINDOW) + self.small_rate < COPY_LOAD_MAX * x.rate;
            if copy != x.in_copy {
                x.in_copy = copy;
                self.send_probe(out, now, l, self.lanes[l].peer, MAX_ECHOES);
            }
        }
    }

    /// Acts on one authenticated data payload, `self.rxbuf[..n]`, that arrived on a lane (None:
    /// no lane has its address; delivered, but it moves nothing, §7.5): small packets at once,
    /// bulk through the reorder buffer (§10.3). The lane is not used here: `packet` records it
    /// through `Flow::carried` for every copy (notes/core.md 19).
    pub(crate) fn data(&mut self, out: &mut dyn Out, now: Mono, l: Option<LaneIx>, typ: u8, n: usize) {
        let lane = l.map(|l| self.lanes[l].id);
        if n < DATA_LEN {
            return; // packet drops these; never slice past one
        }
        let (id, seq) = (FlowId(be(&self.rxbuf[..2]) as u16), be(&self.rxbuf[2..DATA_LEN]) as u32);
        let Some(f) = self.flow(id, true) else { return };
        self.touch(now);
        let fl = &mut self.flows[f];
        fl.last = now;
        if fl.key.is_none() {
            fl.key = Key::parse(&self.rxbuf[DATA_LEN..n]);
        }
        fl.rx.add(now, SMALL_WINDOW, n - DATA_LEN);
        fl.rx_small = typ & SMALL_BIT != 0;
        if fl.rx_small {
            // Small packets bypass the reorder buffer (§10.3), and a flow that turns bulk resumes
            // after them. One at or past `next` passes what is held before it first (`pass`).
            let passes = !after(fl.next, seq);
            if passes {
                self.pass(out, f, seq);
            }
            out.deliver(&self.rxbuf[DATA_LEN..n]);
            if passes {
                self.release(out, now, f); // what is held after it, and the next gap's timer
            }
            return;
        }
        // Exactly 2^31 ahead is neither after nor before `next` in serial order: it counts as far
        // ahead too, so `hold` only ever sees seqs within REORDER_SLOTS past `next` (notes/rust.md,
        // review fixes: flow-half-range).
        if !after(fl.next, seq) && seq.wrapping_sub(fl.next) as usize >= REORDER_SLOTS {
            // Too far ahead to hold: deliver what is held, then jump (notes/core.md 30). No gap was
            // timed: nothing to learn (R2-13). Held seqs lie within REORDER_SLOTS past `next`, so
            // this many steps deliver them all; the bound only keeps a broken count from spinning.
            for _ in 0..REORDER_SLOTS {
                if self.flows[f].n == 0 {
                    break;
                }
                self.step(out, f);
            }
            (self.flows[f].next, self.flows[f].gap_n) = (seq, 0);
            self.unrecovered += 1;
        }
        let fl = &mut self.flows[f];
        if after(fl.next, seq) {
            // Its gap was skipped; Moonlight discards stale frames itself. One of the last gap
            // given up shows how long that gap needed (notes/core.md 27). A seq passed by small
            // packets or a jump shows nothing, nor does one later than REORDER_RESCUE, the longest
            // any gap waits: a stall's leftover that no timer could have kept in order (R2-13).
            let needed = now.saturating_sub(fl.gap_at);
            if seq.wrapping_sub(fl.gap_seq) < fl.gap_n && needed <= REORDER_RESCUE {
                fl.fill = fl.fill.max(needed).min(FILL_MAX);
            }
            out.deliver(&self.rxbuf[DATA_LEN..n]);
            self.waited(lane, 0);
        } else if seq == fl.next {
            // A gap filled: how long the datagram behind it waited.
            if let Some(at) = fl.held_at(seq.wrapping_add(1)) {
                let decayed = fl.fill - fl.fill / Mono::from(SKEW_DECAY);
                fl.fill = now.saturating_sub(at).max(decayed).min(FILL_MAX);
            }
            out.deliver(&self.rxbuf[DATA_LEN..n]);
            fl.next = seq.wrapping_add(1);
            self.waited(lane, 0);
        } else if !self.hold(f, now, seq, n, lane) {
            // Over the byte cap: deliver out of order rather than hold.
            out.deliver(&self.rxbuf[DATA_LEN..n]);
            self.waited(lane, 0);
            return;
        }
        self.release(out, now, f);
    }

    /// Moves flow slot `f` past `seq`, which a small packet brought: the datagrams held up to it
    /// are delivered first, in order, each gap behind one of them given up as `release` gives a
    /// gap up. Delivered alone, the small packet would leave them to come later, behind it and
    /// several in one call when one gap is given up, which §17.2's order invariant forbids (Go
    /// did; notes/rust.md, harden: integration, group C). A missing run just before `seq` is no
    /// gap given up: nothing but the small packet waited behind it, and the flow passes it as when
    /// nothing is held. The gaps passed here teach `fill` nothing: no timer ran out (R2-13).
    fn pass(&mut self, out: &mut dyn Out, f: usize, seq: u32) {
        let mut gap = false;
        // Held seqs lie within REORDER_SLOTS past `next`, so this many steps deliver them all; the
        // bound only keeps a broken count from spinning. `seq` itself is held only from a broken
        // or hostile peer; stepping it too keeps `n` exact. A gap is given up when a held datagram
        // ends it.
        for _ in 0..REORDER_SLOTS {
            if self.flows[f].n == 0 || after(self.flows[f].next, seq) {
                break;
            }
            let missing = self.step(out, f);
            self.unrecovered += u64::from(gap && !missing);
            gap = missing;
        }
        self.flows[f].next = seq.wrapping_add(1);
    }

    /// Holds the datagram `rxbuf[DATA_LEN..n]`, seq `seq` of flow slot `f`, in a pool buffer
    /// until its gap fills (§10.3); false when the pool is past REORDER_CAP or full.
    fn hold(&mut self, f: usize, now: Mono, seq: u32, n: usize, lane: Option<u16>) -> bool {
        // The reorder buffer, made on the first gap (exactly REORDER_SLOTS) and kept until the
        // flow ends.
        self.flows[f].held.resize(REORDER_SLOTS, Pkt::default());
        self.flows[f].held_on.resize(REORDER_SLOTS, 0);
        let Some(i) = self.pool.get() else { return false };
        if self.pool.held() > REORDER_CAP {
            self.pool.put(i);
            return false;
        }
        let len = n - DATA_LEN;
        self.pool.buf_mut(i)[..len].copy_from_slice(&self.rxbuf[DATA_LEN..n]);
        let fl = &mut self.flows[f];
        let h = &mut fl.held[seq as usize % REORDER_SLOTS];
        match h.buf {
            // The same seq under another counter, which only a broken or hostile peer sends:
            // keep `n` exact, so the loops that step until it is 0 end.
            Some(b) => self.pool.put(b),
            None => fl.n += 1,
        }
        *h = Pkt { c: u64::from(seq), at: now, buf: Some(i), n: len as u32 };
        fl.held_on[seq as usize % REORDER_SLOTS] = lane.map_or(0, |id| id.saturating_add(1));
        true
    }

    /// Moves flow slot `f` past one seq, delivering it if held; reports a seq that was missing.
    /// The bytes delivered (a pool buffer here, `rxbuf` in `data`) go to `out` straight: Go
    /// appended an ActDeliver.
    pub(crate) fn step(&mut self, out: &mut dyn Out, f: usize) -> bool {
        let fl = &mut self.flows[f];
        let seq = fl.next;
        fl.next = seq.wrapping_add(1);
        let Some(h) = fl.held.get_mut(seq as usize % REORDER_SLOTS) else { return true };
        let Some(b) = h.buf.take_if(|_| h.c == u64::from(seq)) else { return true };
        fl.n -= 1;
        let (at, on) = (h.at, fl.held_on.get(seq as usize % REORDER_SLOTS).copied().unwrap_or(0));
        out.deliver(&self.pool.buf(b)[..h.n as usize]);
        self.pool.put(b);
        self.waited(on.checked_sub(1), self.now.saturating_sub(at));
        false
    }

    /// Delivers flow slot `f`'s held datagrams in order; a gap waits until it fills, until every
    /// lane that carried the flow within REORDER_LANES has passed it, or until its timer runs
    /// out (§10.3).
    pub(crate) fn release(&mut self, out: &mut dyn Out, now: Mono, f: usize) {
        while self.flows[f].n > 0 {
            let fl = &self.flows[f];
            let next = fl.next;
            if fl.held_at(next).is_some() {
                self.step(out, f);
                continue;
            }
            // The held datagram behind the gap: one lies within REORDER_SLOTS while `n` counts
            // them (`hold` keeps it exact).
            let mut behind = (1..REORDER_SLOTS as u32).map(|k| next.wrapping_add(k));
            let Some((q, first)) = behind.find_map(|q| Some((q, fl.held_at(q)?))) else { return };
            // Each lane is FIFO, so a gap every lane that carried the flow within REORDER_LANES
            // has passed is lost (§15). The base OWDs (inbound) of those lanes: the skew between
            // them, and in performance the lowest of those that passed the gap and the highest of
            // those that did not.
            let (mut passed, mut lag) = (true, false);
            let (mut min, mut max, mut lo, mut hi) = (Mono::MAX, Mono::MIN, Mono::MAX, Mono::MIN);
            for e in &fl.lanes {
                let Some(l) = e.l.filter(|_| now.saturating_sub(e.at) <= REORDER_LANES) else { continue };
                let x = &self.lanes[l];
                let past = after(e.seq, next);
                passed = passed && past;
                lag = lag || x.lagging;
                let b = x.in_min.get();
                if b != NO_MIN {
                    (min, max) = (min.min(b), max.max(b));
                    if past {
                        lo = lo.min(b);
                    } else {
                        hi = hi.max(b);
                    }
                }
            }
            let wait = if self.mode == Mode::Performance && lo != Mono::MAX {
                // §14.1: the slower lane's copy is owed only by how much slower its base OWD is.
                hi.saturating_sub(lo).max(0).saturating_add(DUP_GRACE)
            } else if lag || now.saturating_sub(self.peer_rewind) <= REORDER_LANES {
                REORDER_RESCUE
            } else if min <= max {
                // The skew, or what gaps that filled showed it to be (`fill`, decaying by
                // SKEW_DECAY), × SKEW_MARGIN.
                (((max - min).max(fl.fill) as f64 * SKEW_MARGIN) as Mono).min(REORDER_MAX)
            } else {
                REORDER_MAX
            };
            let due = first.saturating_add(wait);
            if !passed && now < due {
                self.wake_at(due);
                return;
            }
            // Given up: remember the seqs skipped, and skip to the datagram behind the gap.
            self.unrecovered += 1;
            let fl = &mut self.flows[f];
            (fl.gap_at, fl.gap_seq, fl.gap_n, fl.next) = (first, next, q.wrapping_sub(next), q);
            self.step(out, f);
        }
    }
}

#[cfg(test)]
mod tests;
