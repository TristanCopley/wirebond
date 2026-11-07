//! Lanes: building them from links, probes and echoes on the send side, path validation, lane
//! states, punching, idle and wake, and the receiver's copy deadlines (§6.1, §6.3, §7.6 rule 2,
//! §8.1, §8.2, §8.4; Go: core/lane.go).
//!
//! Owner: C3.
//!
//! Lanes live in [`Lanes`], sorted by id, and everything else names one by [`LaneIx`], its index
//! there. Go held `*lane` pointers everywhere (the primary, the spread, the idle lane, each flow's
//! recent lanes, each pending copy); Rust holds indices, which stay valid until the next
//! `set_links`, the only place lanes are added or removed. `set_links` remaps every stored index
//! (`map_lanes`) and keeps the lanes that go away in `Lanes::gone` while `retire` rewinds their
//! bulk, then clears what still names them (`drop_gone`). Go kept reading a retired lane through
//! a flow's or a pending copy's stale pointer; Rust forgets those references instead
//! (notes/rust.md).

use std::net::SocketAddr;
use std::ops::{Index, IndexMut};

use crate::buf::{BufIx, Ring};
use crate::estimate::{EchoRec, INF, Meter, Series, Winmin, ewma};
use crate::flow::FlowLane;
use crate::session::NEVER;
use crate::wire::{Echo, Probe, Window, after, probe_len, seq_newer};
use crate::{
    DOWN_AFTER, ECHO_HISTORY, ECHO_WINDOW, HEADER_LEN, IDLE_MISSES, LAG_FLOOR, LAG_JITTER, MAX_ECHOES, MAX_LINKS, MIN,
    MIN_WINDOW, Mono, Out, PROBE_ACTIVE, PROBE_ANSWER, PROBE_COPY, PROBE_IDLE, PROBE_PUNCH, PROBE_REWIND, PUNCH_FOR,
    PUNCH_GAP, RATE_WINDOW, RTT_RING, Role, Session, SockId, TAG_LEN, TYPE_PROBE, VALIDATE_AMP,
};

/// Small-flow first arrivals kept with pending copy deadlines, oldest dropped first.
pub(crate) const PENDING_COPIES: usize = 4096;

/// Echo flags bit 0: the echoed lane lags (§8.4).
const LAGGING: u8 = 1;

/// The largest inbound OWD taken from a probe, in µs either way (about 35 years). An
/// authenticated peer's SentAt is any i64; an OWD past this is garbage, and the arithmetic built
/// on it (averages, deadlines) must not overflow. Clock offsets between two daemons' monotonic
/// clocks are far inside it.
const OWD_MAX_US: i64 = 1 << 50;

/// Where [`Session::probe`] sends: as [`Session::target`] answers, the address generation the
/// probe goes to and whether it validates the address (VALIDATE_AMP and a challenge); then whether
/// it asks the address to answer at once (PROBE_ANSWER).
type Target = ((u32, bool), bool);

/// The address generation after `a` and `b`. Never 0, which names a punching candidate.
fn next_gen(a: u32, b: u32) -> u32 {
    a.max(b).wrapping_add(1).max(1)
}

/// The lane with `id` in `old`, taken out with its index there if `keeps` holds for it (`rebuild`;
/// ids are unique, so no other lane can match).
fn reuse(old: &mut [Option<Lane>], id: u16, keeps: impl Fn(&Lane) -> bool) -> Option<(Lane, Option<usize>)> {
    let i = old.iter().position(|o| o.as_ref().is_some_and(|o| o.id == id && keeps(o)))?;
    Some((old[i].take()?, Some(i)))
}

/// Lane states (§8.4), kept by each sender for its own direction. The order matters: `two`
/// prefers a lower state. Go: the `punching`… iota constants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum State {
    #[default]
    Punching,
    Up,
    Suspect,
    Down,
    Dormant,
}

impl State {
    /// How status names it. Go: `stateNames`.
    pub(crate) const fn name(self) -> &'static str {
        ["punching", "up", "suspect", "down", "dormant"][self as usize]
    }
}

/// One probe I sent, kept for RTT, loss and validation (§8.3). Go: `sentProbe`. Its challenge is
/// recomputed from (lane, agen, seq) by `Session::chal`, so it is not stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SentProbe {
    pub(crate) seq: u32,
    /// The address generation it went to, 0 for a candidate.
    pub(crate) agen: u32,
    /// `tx_pkts` when it was sent.
    pub(crate) pkts: u32,
    pub(crate) at: Mono,
}

/// A lane's index in [`Session::lanes`](crate::Session). Valid from one `set_links` to the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LaneIx(pub(crate) u8);

impl LaneIx {
    pub(crate) fn at(i: usize) -> LaneIx {
        LaneIx(i as u8)
    }

    pub(crate) fn get(self) -> usize {
        self.0 as usize
    }
}

/// The session's lanes: `live`, sorted by id so iteration is deterministic, and, only while
/// `set_links` retires them, `gone` (index `live.len() + i`). Iteration covers live lanes only,
/// as Go's `s.lanes` did after a rebuild; indexing reaches both.
#[derive(Default)]
pub(crate) struct Lanes {
    pub(crate) live: Vec<Lane>,
    pub(crate) gone: Vec<Lane>,
}

impl Lanes {
    /// Live lanes.
    pub(crate) fn len(&self) -> usize {
        self.live.len()
    }

    /// The live lanes' indices, without borrowing the lanes, so a loop body may call `&mut self`
    /// methods: `for l in self.lanes.ixs() { self.lane_tick(out, now, l) }`.
    pub(crate) fn ixs(&self) -> impl Iterator<Item = LaneIx> + use<> {
        (0..self.live.len() as u8).map(LaneIx)
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, Lane> {
        self.live.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> std::slice::IterMut<'_, Lane> {
        self.live.iter_mut()
    }
}

impl Index<LaneIx> for Lanes {
    type Output = Lane;

    fn index(&self, i: LaneIx) -> &Lane {
        let (i, n) = (i.get(), self.live.len());
        if i < n { &self.live[i] } else { &self.gone[i - n] }
    }
}

impl IndexMut<LaneIx> for Lanes {
    fn index_mut(&mut self, i: LaneIx) -> &mut Lane {
        let (i, n) = (i.get(), self.live.len());
        if i < n { &mut self.live[i] } else { &mut self.gone[i - n] }
    }
}

/// A UDP path between one of my sockets and one of the peer's (§6.1). Go: `lane`; `time.Duration`
/// fields are [`Mono`] nanoseconds, `netip.AddrPort` fields `Option<SocketAddr>` (None for Go's
/// invalid zero value).
#[derive(Default)]
pub(crate) struct Lane {
    /// Client link id << 8 | server link id; 0 is the Tailscale lane.
    pub(crate) id: u16,
    pub(crate) sock: SockId,
    /// My link's address; None for the Tailscale lane.
    pub(crate) addr: Option<SocketAddr>,
    /// The return address (§7.6 rule 2); None until known.
    pub(crate) peer: Option<SocketAddr>,
    /// A newer probe's address, validating while `peer` carries on.
    pub(crate) cand: Option<SocketAddr>,
    /// The generations of `peer` and `cand`: a new one for each address.
    pub(crate) pgen: u32,
    pub(crate) cgen: u32,
    /// The peer link's candidates.
    pub(crate) cands: Vec<SocketAddr>,
    /// Link labels.
    pub(crate) local: String,
    pub(crate) remote: String,
    pub(crate) metered: bool,
    pub(crate) mesh: bool,
    /// The client link's bottleneck group; -1 shares every group (§6.1).
    pub(crate) group: i32,

    pub(crate) state: State,
    pub(crate) misses: u8,
    pub(crate) validated: bool,
    pub(crate) got_probe: bool,
    /// `cand` echoed a probe (with its challenge): it becomes `peer` on its next probe.
    pub(crate) cvalid: bool,
    /// The peer's SentAt from which its lagging marks count, µs.
    pub(crate) lag_from: i64,
    /// In my copy set (§8.4).
    pub(crate) in_copy: bool,
    /// The peer's newest view: this lane lags.
    pub(crate) peer_lag: bool,
    /// The peer's newest LastCounter for this lane (§10.2).
    pub(crate) acked: u64,
    /// Bytes from and to an unvalidated address (§7.6 rule 2).
    pub(crate) amp_in: usize,
    pub(crate) amp_out: usize,
    pub(crate) suspect_at: Mono,
    /// Start of the current clean run, NEVER if not clean.
    pub(crate) clean_at: Mono,
    pub(crate) last_echo: Mono,
    pub(crate) next_probe: Mono,
    pub(crate) tx_seq: u32,
    pub(crate) tx_pkts: u32,
    /// The oldest probe to `peer` not yet echoed; 0 if none.
    pub(crate) wait_seq: u32,
    pub(crate) echo_seq: u32,
    pub(crate) rtt_seq: u32,
    /// RTT_RING probes, by Seq % RTT_RING.
    pub(crate) sent: Box<[SentProbe]>,
    pub(crate) sent_bytes: u64,
    pub(crate) copy_bytes: u64,

    // Estimates of my direction (§8.3).
    pub(crate) rtt: Mono,
    pub(crate) last_rtt: Mono,
    pub(crate) rttdev: Mono,
    pub(crate) owd: Mono,
    pub(crate) dev: Mono,
    /// The newest sample, for earliest arrival (§11.2).
    pub(crate) owd_now: Mono,
    pub(crate) qd: Mono,
    pub(crate) min_rtt: Winmin,
    pub(crate) min_owd: Winmin,
    pub(crate) rtts: Series,
    pub(crate) owds: Series,
    pub(crate) hist: [EchoRec; ECHO_HISTORY],
    pub(crate) hist_n: usize,
    /// Loss and delivery count echoes of later probes only.
    pub(crate) hist_seq: u32,
    pub(crate) loss: f64,
    /// Bits per second.
    pub(crate) delivery: f64,

    // Rate, pacer and rewind (§9, §10.2).
    /// Bits per second, INF while unlimited.
    pub(crate) rate: f64,
    /// Bytes.
    pub(crate) tokens: f64,
    /// The rate to climb back to after startup's overshoot; 0 if none.
    pub(crate) regain: f64,
    pub(crate) startup: bool,
    pub(crate) over_n: u32,
    pub(crate) q_bytes: usize,
    /// Jitter while app-limited, for target.
    pub(crate) dev_app: Mono,
    /// qd at the last cut.
    pub(crate) cut_qd: Mono,
    pub(crate) last_cut: Mono,
    pub(crate) last_inc: Mono,
    pub(crate) filled: Mono,
    pub(crate) last_over: Mono,
    /// The pacer queue.
    pub(crate) q: Ring,
    /// The rewind ring.
    pub(crate) rw: Ring,
    pub(crate) offered: Meter,
    pub(crate) send_rate: Meter,

    // performance and bandwidth (§11).
    /// When it last queued bulk, to tell a stale rate (§9).
    pub(crate) bulk_at: Mono,
    /// When it began to carry bulk.
    pub(crate) took_at: Mono,
    pub(crate) bar_until: Mono,
    /// In overuse at PACE_FLOOR since (§11.2 rule 4); NEVER if not.
    pub(crate) floor_at: Mono,
    pub(crate) bars: u32,

    // The peer's direction, as I receive it (§7.5, §8.4).
    pub(crate) win: Window,
    pub(crate) rx_seq: u32,
    pub(crate) rx_pkts: u32,
    pub(crate) rx_bytes: u32,
    pub(crate) snap_pkts: u32,
    pub(crate) snap_bytes: u32,
    pub(crate) rx_at: Mono,
    /// µs, of the newest probe.
    pub(crate) rx_owd: i64,
    /// The challenge of the newest probe received, returned in this lane's echo (R2-03). Replaces
    /// Go's `rxFrom`, which only fed the Observed field.
    pub(crate) rx_tok: u64,
    /// Counter of the newest data packet.
    pub(crate) rx_last: u64,
    pub(crate) in_owd: Mono,
    pub(crate) in_dev: Mono,
    /// Base inbound OWD, for lagging deadlines.
    pub(crate) in_min: Winmin,
    pub(crate) owd_in: Series,
    pub(crate) copy_in: bool,
    pub(crate) lagging: bool,
    /// Copies in a row past their deadlines, up to 2.
    pub(crate) overdue: u8,
    /// The oldest copy it may still owe (§8.4).
    pub(crate) expect: u64,
    /// The newest counter on it.
    pub(crate) last_small: u64,
    /// Contribution statistics (§11.5).
    pub(crate) stats: crate::stats::LaneStats,
}

impl Lane {
    /// A fresh lane: Go's `newLane`, which names the fields that do not start at zero.
    /// `set_links` then starts `tx_seq` at the session's `seq_top` (R2-10).
    pub(crate) fn new(id: u16, sock: SockId) -> Lane {
        let mut l = Lane { id, sock, pgen: 1, rate: INF, bulk_at: Mono::MIN / 2, ..Lane::default() };
        (l.clean_at, l.floor_at) = (NEVER, NEVER);
        (l.min_rtt, l.min_owd, l.in_min) = (Winmin::new(), Winmin::new(), Winmin::new());
        l.sent = vec![SentProbe::default(); RTT_RING].into();
        l
    }

    /// What I received on this lane, for a probe of mine (§8.2): its `token` is `rx_tok`; `chal`
    /// is 0 (send_probe sets it on the probe's own lane).
    pub(crate) fn echo(&self, now: Mono) -> Echo {
        Echo {
            lane: self.id,
            flags: if self.lagging { LAGGING } else { 0 },
            seq: self.rx_seq,
            held: (now.saturating_sub(self.rx_at) / 1_000) as u32, // wraps, as Go's uint32() does
            owd: self.rx_owd,
            pkts: self.snap_pkts,
            bytes: self.snap_bytes,
            last: self.rx_last,
            token: self.rx_tok,
            chal: 0,
        }
    }

    /// The first probe after `seq` sent to the current address, or 0. Bounded by RTT_RING, so it
    /// cannot spin at a wrap (R2-24).
    pub(crate) fn next_wait(&self, seq: u32) -> u32 {
        // The probes after seq, in serial order: none if seq is not before tx_seq. Of those, only
        // the last RTT_RING are still in the ring; the older ones' slots hold newer probes.
        let d = self.tx_seq.wrapping_sub(seq);
        let n = if d > i32::MAX as u32 { 0 } else { d.min(RTT_RING as u32) };
        let first = seq.wrapping_add(d.saturating_sub(RTT_RING as u32));
        let sp = |q: u32| &self.sent[q as usize % RTT_RING];
        (1..=n).map(|i| first.wrapping_add(i)).find(|&q| sp(q).seq == q && sp(q).agen == self.pgen).unwrap_or(0)
    }
}

/// The first arrival of a small-bit packet, which the other lanes in the peer's copy set then owe
/// (§8.4). Go: `arrival`. `l` is None for an empty slot or a lane that has since gone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Arrival {
    pub(crate) c: u64,
    pub(crate) at: Mono,
    pub(crate) l: Option<LaneIx>,
}

impl Session {
    /// Rebuilds the lanes from my links and the peer's (§6.1): the Tailscale lane and one per
    /// same-family pair of links, sorted by id. A lane keeps its state while its socket, my
    /// link's address and one of the peer link's candidates stay; a rebuilt lane keeps only its
    /// return address, generation and (if the candidates overlap) validation. New and rebuilt
    /// lanes start `tx_seq` at `seq_top` (R2-10). Then `retire` for each lane that went (they sit
    /// in `lanes.gone` meanwhile), `drop_gone`, and the bottleneck groups.
    pub(crate) fn set_links(&mut self, out: &mut dyn Out, now: Mono) {
        self.rebuild();
        for i in self.lanes.len()..self.lanes.len() + self.lanes.gone.len() {
            self.retire(out, now, LaneIx::at(i));
        }
        self.drop_gone();
        // Bottleneck groups (§6.1, notes/core.md 6): the client's links by label; the Tailscale
        // lane joins the default link's group, or shares every group (-1) when no link is default.
        // A label's group is its index among the distinct labels in link order. Go: `appendGroup`.
        let client = self.role == Role::Client;
        let links: Vec<(&str, bool)> = match client {
            true => self.links.iter().take(MAX_LINKS).map(|l| (l.label.as_str(), l.default)).collect(),
            false => self.peer.iter().take(MAX_LINKS).map(|l| (l.label.as_str(), l.default)).collect(),
        };
        let mut labels = Vec::new();
        for &(label, _) in &links {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        let group = |label: &str| labels.iter().position(|&x| x == label).map_or(-1, |i| i as i32);
        // The last default link's group.
        let def = links.iter().rev().find(|l| l.1).map_or(-1, |l| group(l.0));
        for l in self.lanes.iter_mut() {
            l.group = if l.mesh { def } else { group(if client { &l.local } else { &l.remote }) };
        }
    }

    /// After the retires: clears every flow's and pending copy's reference to a gone lane (index
    /// ≥ `lanes.len()`) and drops the gone lanes.
    pub(crate) fn drop_gone(&mut self) {
        // retire has cleared idle and the spread already; this makes sure no
        // stored index outlives its lane.
        let n = self.lanes.len();
        self.map_lanes(|l| (l.get() < n).then_some(l));
        self.lanes.gone.clear();
    }

    /// Rewrites every stored lane index through `f`: `idle`,
    /// `spread[..nspread]`, each flow's `lanes[].l` and each `arr[].l`. A field that comes to hold
    /// a LaneIx must be added here (through the integrator). An index `f` maps to None is
    /// forgotten: it leaves the spread, and a flow's entry for it becomes the empty one, the
    /// oldest for the next lane to take.
    fn map_lanes(&mut self, f: impl Fn(LaneIx) -> Option<LaneIx>) {
        let g = |l: Option<LaneIx>| l.and_then(&f);
        self.idle = g(self.idle);
        let (spread, n) = (self.spread, std::mem::take(&mut self.nspread));
        for l in spread[..n].iter().filter_map(|&l| f(l)) {
            self.spread[self.nspread] = l;
            self.nspread += 1;
        }
        for e in self.flows.iter_mut().flat_map(|fl| fl.lanes.iter_mut()) {
            *e = g(e.l).map_or(FlowLane::default(), |l| FlowLane { l: Some(l), ..*e });
        }
        self.arr.iter_mut().for_each(|a| a.l = g(a.l));
        self.firsts.iter_mut().for_each(|a| a.l = g(a.l));
    }

    /// The first half of `set_links`: builds the new lane list from the links, moves the lanes
    /// that stay into it, puts the old lanes that go (removed, or replaced by a rebuilt lane) in
    /// `lanes.gone` in their old order, and remaps every stored index.
    ///
    /// A lane id names the lane on the wire, so it is unique: a second pair of links that makes
    /// an id already built (a peer offering one link id twice) is skipped. Go built both lanes
    /// when the id was new and merged them into one otherwise.
    fn rebuild(&mut self) {
        let mut old: Vec<Option<Lane>> = std::mem::take(&mut self.lanes.live).into_iter().map(Some).collect();
        // A lane built here is fresh, with every Seq it counts starting at `seq_top`, so the peer,
        // which may still hold an older lane under this id, takes its probes as newer (R2-10), and
        // echoes of an older lane's probes count for nothing.
        let s = self.seq_top;
        let fresh = |id, sock| Lane { tx_seq: s, echo_seq: s, rtt_seq: s, hist_seq: s, ..Lane::new(id, sock) };
        // The new lanes, each with its old index if it stays.
        let mut next: Vec<(Lane, Option<usize>)> = Vec::new();
        if let Some(m) = self.mesh.filter(|m| m.sock != 0) {
            // The Tailscale lane: its addresses are the two mesh addresses, which never move.
            let make = || (Lane { peer: Some(m.peer), mesh: true, validated: true, ..fresh(0, m.sock) }, None);
            next.push(reuse(&mut old, 0, |l| l.sock == m.sock && l.peer == Some(m.peer)).unwrap_or_else(make));
        }
        let is4 = |a: SocketAddr| a.ip().to_canonical().is_ipv4(); // Go: Addr().Unmap().Is4()
        for my in self.links.iter().take(MAX_LINKS) {
            for pl in self.peer.iter().take(MAX_LINKS) {
                // Client link id << 8 | server link id.
                let id = u16::from_be_bytes(if self.role == Role::Client { [my.id, pl.id] } else { [pl.id, my.id] });
                if pl.cands.first().is_none_or(|&c| is4(c) != is4(my.local)) || next.iter().any(|(l, _)| l.id == id) {
                    continue;
                }
                // A lane keeps its state while its socket, my link's address and one of the peer
                // link's candidates stay, so a limit learned on one network never follows a label
                // to the next.
                let same = |o: &Lane| pl.cands.iter().any(|c| o.cands.contains(c));
                let kept = reuse(&mut old, id, |o| o.sock == my.sock && o.addr == Some(my.local) && same(o));
                let (mut l, src) = kept.unwrap_or_else(|| {
                    let mut l = fresh(id, my.sock);
                    if let Some(o) = old.iter().flatten().find(|o| o.id == id) {
                        // A new network: fresh estimates, but the return address moves only on a
                        // probe (§7.6 rule 2). If the peer's link moved, its old address may be
                        // someone else's.
                        (l.peer, l.pgen, l.validated) = (o.peer, o.pgen, o.validated && same(o));
                    }
                    (l, None)
                });
                (l.addr, l.metered) = (Some(my.local), my.metered || pl.metered);
                l.cands.clone_from(&pl.cands);
                l.local.clone_from(&my.label);
                l.remote.clone_from(&pl.label);
                next.push((l, src));
            }
        }
        next.sort_unstable_by_key(|(l, _)| l.id); // ids are unique: deterministic
        // The old lanes that go follow the new ones, in their old order; `map` takes each old
        // index to its lane's place.
        let (live, mut map) = (next.len(), vec![LaneIx::default(); old.len()]);
        next.extend(old.into_iter().enumerate().filter_map(|(i, l)| l.map(|l| (l, Some(i)))));
        for (j, i) in next.iter().enumerate().filter_map(|(j, (_, i))| i.map(|i| (j, i))) {
            map[i] = LaneIx::at(j);
        }
        let gone = next.split_off(live);
        self.lanes.live = next.into_iter().map(|(l, _)| l).collect();
        self.lanes.gone = gone.into_iter().map(|(l, _)| l).collect();
        self.map_lanes(|l| map.get(l.get()).copied());
    }

    /// The live lane with `id`. Go: `laneByID`.
    pub(crate) fn lane_by_id(&self, id: u16) -> Option<LaneIx> {
        self.lanes.iter().position(|l| l.id == id).map(LaneIx::at)
    }

    /// The lane a data packet arrived on: its socket and return address. Go: `laneAt`.
    pub(crate) fn lane_at(&self, sock: SockId, from: SocketAddr) -> Option<LaneIx> {
        self.lanes.iter().position(|l| l.sock == sock && l.peer == Some(from)).map(LaneIx::at)
    }

    /// Sends the packet in pool buffer `buf` (`n` bytes) on every lane with a validated address:
    /// the first datagrams after idle, until a lane is confirmed (§8.1).
    pub(crate) fn send_all(&mut self, out: &mut dyn Out, buf: BufIx, n: usize) {
        for l in self.lanes.ixs() {
            if let (true, Some(to)) = (self.lanes[l].validated, self.lanes[l].peer) {
                self.send(out, l, to, buf, n);
            }
        }
    }

    /// Sends the packet in pool buffer `buf` (`n` bytes) from l's socket to `to`, counting it on
    /// the lane (tx_pkts when to is l's peer, sent_bytes, send_rate) and in `metered_bytes`.
    /// Uses `self.now`. Go: `send`, which appended an ActSend.
    pub(crate) fn send(&mut self, out: &mut dyn Out, l: LaneIx, to: SocketAddr, buf: BufIx, n: usize) {
        let x = &mut self.lanes[l];
        if x.peer == Some(to) {
            x.tx_pkts = x.tx_pkts.wrapping_add(1); // as the peer counts arrivals on the lane (§8.3 loss)
        }
        x.sent_bytes = x.sent_bytes.wrapping_add(n as u64);
        x.send_rate.add(self.now, RATE_WINDOW, n);
        if x.metered {
            self.metered_bytes.add(self.now, MIN, n);
        }
        out.send(x.sock, to, &self.pool.buf(buf)[..n]);
    }

    /// Sends a probe on `l` to `to` with at most `max` echoes (§8.2): its own lane's echo first,
    /// then lanes heard within ECHO_WINDOW or lagging (built in `self.eout`). An address being
    /// validated gets at most VALIDATE_AMP × the bytes received from it, PROBE_ANSWER, and a
    /// challenge: `self.chal.token(l, agen, seq)` in the own-lane echo's `chal`, which such a probe
    /// always carries when `max` ≥ 1 (with seq 0 if nothing was received on the lane yet). R2-03.
    pub(crate) fn send_probe(&mut self, out: &mut dyn Out, now: Mono, l: LaneIx, to: Option<SocketAddr>, max: usize) {
        if let Some(to) = to {
            self.probe(out, now, l, to, max, (self.target(l, to), true));
        }
    }

    /// `send_probe` with its [`Target`] given: without the ask, an address being validated still
    /// gets the challenge, within the same VALIDATE_AMP.
    fn probe(&mut self, out: &mut dyn Out, now: Mono, l: LaneIx, to: SocketAddr, max: usize, target: Target) {
        if self.tx.rid == 0 {
            return; // no key from the peer yet
        }
        let ((agen, check), ask) = target;
        let n = self.fill_echoes(now, l, max.min(MAX_ECHOES), check);
        let size = HEADER_LEN + probe_len(n) + TAG_LEN;
        let x = &self.lanes[l];
        if check && x.amp_out.saturating_add(size) > x.amp_in.saturating_mul(VALIDATE_AMP as usize) {
            return;
        }
        let Some(i) = self.pool.get() else { return };
        let x = &mut self.lanes[l];
        x.tx_seq = x.tx_seq.wrapping_add(1).max(1); // Seq 0 is never a probe
        let seq = x.tx_seq;
        let flags = if x.in_copy { PROBE_COPY } else { 0 }
            | if check && ask { PROBE_ANSWER } else { 0 }
            | if now < self.rewind_until { PROBE_REWIND } else { 0 };
        let p = Probe { lane: x.id, seq, sent_at: now / 1_000, flags, n }; // probes carry µs
        if check {
            x.amp_out += size;
            if n > 0 {
                // fill_echoes put the lane's own echo first: it carries the challenge (R2-03).
                self.eout[0].chal = self.chal.token(p.lane, agen, seq);
            }
        }
        if after(seq, self.seq_top) {
            self.seq_top = seq;
        }
        let len = p.encode(&self.eout[..n], &mut self.pool.buf_mut(i)[HEADER_LEN..]);
        let len = self.seal(out, i, TYPE_PROBE, len);
        self.send(out, l, to, i, len);
        let x = &mut self.lanes[l];
        if x.peer == Some(to) && x.wait_seq == 0 {
            x.wait_seq = seq;
        }
        x.sent[seq as usize % RTT_RING] = SentProbe { seq, agen, pkts: x.tx_pkts, at: now };
        self.pool.put(i);
    }

    /// The address generation a probe on `l` to `to` goes to, and whether `to` is being validated
    /// (§7.6 rule 2): the return address's while unvalidated, or a claim's. 0 for a punching
    /// candidate.
    fn target(&self, l: LaneIx, to: SocketAddr) -> (u32, bool) {
        match &self.lanes[l] {
            x if x.peer == Some(to) => (x.pgen, !x.validated),
            x if x.cand == Some(to) => (x.cgen, true),
            _ => (0, false),
        }
    }

    /// Builds a probe's echoes in `eout` and returns how many, at most `max` (§8.2): l's own
    /// echo first, even when older than ECHO_WINDOW, so the answer to an idle probe can echo it
    /// without being larger (§7.6 rule 3), and always when `validating`, since it carries the
    /// challenge (R2-03; Seq 0 if nothing was received on l yet); then the lanes heard within
    /// ECHO_WINDOW or lagging.
    fn fill_echoes(&mut self, now: Mono, l: LaneIx, max: usize, validating: bool) -> usize {
        let mut n = 0;
        let x = &self.lanes[l];
        if max > 0 && (x.rx_seq != 0 || validating) {
            self.eout[0] = x.echo(now);
            n = 1;
        }
        for (i, x) in self.lanes.iter().enumerate() {
            if n == max {
                break;
            }
            if LaneIx::at(i) != l && x.rx_seq != 0 && (x.lagging || now.saturating_sub(x.rx_at) <= ECHO_WINDOW) {
                self.eout[n] = x.echo(now);
                n += 1;
            }
        }
        n
    }

    /// Acts on the authenticated probe in `self.pin`/`self.ein` that arrived on lane `l` (None if
    /// it names none on this socket) from `from`, `size` bytes: inbound OWD and copy set, the
    /// challenge to return (`rx_tok`), the return address (§7.6 rule 2), the peer's lagging view,
    /// each echo (`Session::echo`), and at most one probe in answer (§7.6 rule 3, §8.1).
    pub(crate) fn on_probe(&mut self, out: &mut dyn Out, now: Mono, l: Option<LaneIx>, from: SocketAddr, size: usize) {
        let p = self.pin;
        let n = p.n.min(MAX_ECHOES);
        let newer = l.filter(|&l| seq_newer(p.seq, self.lanes[l].rx_seq));
        let to = newer.and_then(|l| self.rx_probe(now, l, from, size)); // a new address gets a probe at once
        self.lag_view(now);
        for i in 0..n {
            self.echo(now, l, p.sent_at, self.ein[i]);
        }
        if let Some(l) = newer.filter(|&l| self.lanes[l].cvalid && self.lanes[l].cand == Some(from)) {
            // The claim answered our challenge: the lane moves on its next probe (§7.6 rule 2).
            // The peer marks this lane lagging for the copies the old address lost, until copies
            // sent here reach it; those marks are stale.
            let x = &mut self.lanes[l];
            x.peer = x.cand.take();
            x.lag_from = p.sent_at.saturating_add(x.rtt.saturating_add(LAG_FLOOR) / 1_000);
            self.moved(l, self.lanes[l].cgen);
        }
        // One probe at once to a new address, an answer to a peer validating my address, and the
        // server's answer to an idle probe; none larger than the probe that caused it (§7.6 rule
        // 3, §8.1). An idle server's answers ask for none in turn: it answers every probe, so a
        // peer that answers every ask while its address never validates here (Go's: R2-03) would
        // trade probes with it as fast as the path carries them for as long as the session stayed
        // idle (notes/rust.md, system: integration).
        let Some(l) = l else { return };
        let idle = self.role == Role::Anchor && !self.active && now >= self.punch_until;
        let answer = idle || newer.is_some() && p.flags & PROBE_ANSWER != 0;
        if to.is_some() {
            self.send_probe(out, now, l, to, n);
        } else if answer && let Some(peer) = self.lanes[l].peer {
            self.probe(out, now, l, peer, n, (self.target(l, peer), !idle));
        }
    }

    /// Takes a newer probe on `l` (`self.pin`) from `from`, `size` bytes: what the lane's echo
    /// returns (Seq, arrival, OWD, the packet snapshot, the challenge it carried in its own echo),
    /// the inbound OWD estimates (§8.3), whether the peer copies on it (§8.4), and the return
    /// address (`move_peer`, whose new address it returns).
    fn rx_probe(&mut self, now: Mono, l: LaneIx, from: SocketAddr, size: usize) -> Option<SocketAddr> {
        let p = self.pin;
        let owd_us = (now / 1_000).saturating_sub(p.sent_at).clamp(-OWD_MAX_US, OWD_MAX_US);
        let owd = owd_us * 1_000;
        let tok = self.ein[..p.n.min(MAX_ECHOES)].iter().find(|e| e.lane == p.lane).map_or(0, |e| e.chal);
        let x = &mut self.lanes[l];
        (x.rx_seq, x.rx_at, x.rx_owd, x.rx_tok, x.got_probe) = (p.seq, now, owd_us, tok, true);
        (x.snap_pkts, x.snap_bytes) = (x.rx_pkts, x.rx_bytes);
        x.owd_in.add(now, owd);
        x.in_min.add(now, owd);
        (x.in_owd, x.in_dev) = (ewma(x.in_owd, owd), x.owd_in.mad(now));
        if p.flags & PROBE_REWIND != 0 {
            self.peer_rewind = now;
        }
        let copy = p.flags & PROBE_COPY != 0;
        if copy != x.copy_in {
            (x.copy_in, x.lagging, x.expect) = (copy, false, self.arr_n);
        }
        self.move_peer(l, from, size)
    }

    /// §7.6 rule 2 for a newer probe on `l` from `from`, `size` bytes, on the lane's own socket.
    /// The Tailscale lane, and a probe from the return address, end any claim. A lane not yet
    /// validated moves at once and carries only probes until an echo validates it. A validated
    /// one carries on at its address while a new one validates, as a claim, so a probe relayed
    /// ahead of the real one gets the relay probes only and changes nothing else. Returns the new
    /// address, which gets a probe at once.
    fn move_peer(&mut self, l: LaneIx, from: SocketAddr, size: usize) -> Option<SocketAddr> {
        let x = &mut self.lanes[l];
        if x.mesh || x.peer == Some(from) {
            x.cand = None;
        } else if !x.validated {
            let g = next_gen(x.pgen, x.cgen);
            (x.peer, x.cand, x.amp_in, x.amp_out) = (Some(from), None, size, 0);
            self.moved(l, g);
            return Some(from);
        } else if x.cand != Some(from) {
            (x.cand, x.cgen, x.cvalid, x.amp_in, x.amp_out) = (Some(from), next_gen(x.pgen, x.cgen), false, size, 0);
            return Some(from);
        }
        None
    }

    /// The peer's newest view of which of my lanes lag (§8.4), from its probe with the newest
    /// SentAt only, so a probe late on a slow lane cannot flag a lane that already recovered
    /// (notes/core.md 8b). Marks of a claimed lane, and those sent before copies to its new
    /// address could arrive, report what the old address lost and are ignored (note 32).
    fn lag_view(&mut self, now: Mono) {
        let p = self.pin;
        if p.sent_at <= self.lag_as_of {
            return;
        }
        self.lag_as_of = p.sent_at;
        self.lanes.iter_mut().for_each(|x| x.peer_lag = false);
        for i in 0..p.n.min(MAX_ECHOES) {
            let e = self.ein[i];
            let Some(l) = self.lane_by_id(e.lane) else { continue };
            let x = &mut self.lanes[l];
            if e.flags & LAGGING != 0 && p.sent_at >= x.lag_from && x.cand.is_none() {
                x.peer_lag = true;
                if x.state == State::Up {
                    self.set_state(now, l, State::Suspect);
                }
            }
        }
    }

    /// Starts address generation `agen` on l's new return address: its minima and delivery history
    /// start again.
    pub(crate) fn moved(&mut self, l: LaneIx, agen: u32) {
        let x = &mut self.lanes[l];
        (x.pgen, x.wait_seq, x.hist_n, x.hist_seq) = (agen, 0, 0, x.tx_seq);
        x.min_rtt.reset();
        x.min_owd.reset();
    }

    /// Moves `l` to `st` (§11.4). Down forgets the base OWD and minimum RTT. A state other than Up
    /// ends the clean run.
    pub(crate) fn set_state(&mut self, now: Mono, l: LaneIx, st: State) {
        let x = &mut self.lanes[l];
        if x.state == st {
            return;
        }
        match st {
            State::Suspect => {
                (x.suspect_at, x.clean_at) = (now, NEVER);
            }
            State::Down => {
                x.min_rtt.reset();
                x.min_owd.reset();
                x.clean_at = NEVER;
            }
            // A lane back up restarts its rate, unless it carried bulk within MIN_WINDOW, as the
            // blind pair's lanes do while they are down (§11.4): that rate still holds (§12).
            State::Up if x.state != State::Suspect && now.saturating_sub(x.bulk_at) >= MIN_WINDOW => {
                self.restart(now, l)
            }
            State::Up => {}
            State::Punching | State::Dormant => x.clean_at = NEVER,
        }
        self.lanes[l].state = st;
        self.wake_at(now); // the scheduler reacts at once, a failover above all
    }

    /// Starts a punch round (§6.3, §8.1), at most one per PUNCH_GAP; a request inside the gap
    /// starts one when it ends.
    pub(crate) fn punch(&mut self, now: Mono) {
        self.punch_pending = now.saturating_sub(self.punch_at) < PUNCH_GAP;
        if self.punch_pending {
            self.wake_at(self.punch_at.saturating_add(PUNCH_GAP));
            return;
        }
        (self.punch_at, self.punch_until, self.next_punch) = (now, now.saturating_add(PUNCH_FOR), now);
        self.wake_lanes(now);
    }

    /// Probes every lane at once; dormant ones must be confirmed again (§8.4).
    pub(crate) fn wake_lanes(&mut self, now: Mono) {
        self.dozing = false;
        self.lanes.iter_mut().filter(|x| x.state == State::Dormant).for_each(|x| x.state = State::Punching);
        self.lanes.iter_mut().for_each(|x| x.next_probe = now);
        self.wake_at(now);
    }

    /// Runs l's timers: my direction's liveness and probes, and the peer's copies (§8.1, §8.4).
    pub(crate) fn lane_tick(&mut self, out: &mut dyn Out, now: Mono, l: LaneIx) {
        // My direction's liveness while active (§8.4, notes/core.md 10, 34, 40b): suspect once two
        // probes in a row are unechoed past the suspect timeout, since one alone may have been
        // lost, or its echo, which the next probe shows; down once the oldest is DOWN_AFTER old.
        // Blind or not (§11.4): a lane with no echo for DOWN_AFTER is down, so the daemon sees
        // the bond fail at DOWN_AFTER, not only once the probe ring has wrapped.
        let x = &self.lanes[l];
        if self.active && matches!(x.state, State::Up | State::Suspect) && x.wait_seq != 0 {
            let sp = x.sent[x.wait_seq as usize % RTT_RING];
            let age = now.saturating_sub(sp.at);
            if sp.seq != x.wait_seq || age > DOWN_AFTER {
                self.set_state(now, l, State::Down);
            } else if x.state != State::Up {
                self.wake_at(sp.at.saturating_add(DOWN_AFTER + 1));
            } else if let next @ 1.. = x.next_wait(x.wait_seq) {
                let at = x.sent[next as usize % RTT_RING].at;
                let wait = self.suspect_after(l);
                if now.saturating_sub(at) > wait {
                    self.set_state(now, l, State::Suspect);
                } else {
                    self.wake_at(at.saturating_add(wait).saturating_add(1));
                }
            }
        }
        let every = match self.role {
            _ if self.active => Some(PROBE_ACTIVE),
            _ if now < self.punch_until => Some(PROBE_PUNCH),
            Role::Client if self.idle == Some(l) => Some(PROBE_IDLE),
            _ => None,
        };
        let x = &mut self.lanes[l];
        if let (Some(every), Some(peer)) = (every, x.peer) {
            if now >= x.next_probe {
                if !self.active && now >= self.punch_until && x.wait_seq != 0 {
                    x.misses = x.misses.saturating_add(1);
                    if u32::from(x.misses) >= IDLE_MISSES {
                        // Three idle probes unanswered (§8.1).
                        self.set_state(now, l, State::Down);
                        self.pick_idle(now);
                        return;
                    }
                }
                self.send_probe(out, now, l, Some(peer), MAX_ECHOES);
                if !self.lanes[l].cvalid {
                    self.send_probe(out, now, l, self.lanes[l].cand, MAX_ECHOES);
                }
                self.lanes[l].next_probe = now.saturating_add(every);
            }
            self.wake_at(self.lanes[l].next_probe);
        }
        // The peer's copies on l (§8.4, notes/core.md 8c, 33a): two copies in a row missing past
        // their deadlines mark l lagging, and a probe on my fastest other lane says so at once.
        // One alone was more likely lost, which the next copy shows within the copy interval, than
        // stalled. Each missed deadline is consumed, so the probe goes once per episode.
        //
        // A mark with no copy left to judge, while no small packet first arrived on any lane for
        // ECHO_WINDOW, is cleared: only copies clear a mark, so in a session with no small flow
        // the peer kept the lane suspect for good (Rust only: notes/rust.md, core: integration).
        // The evidence of two late copies stays (`overdue`), so one more late copy marks it again.
        let mut a = self.expected(l);
        while self.lanes[l].copy_in
            && let Some(arr) = a
        {
            let d = self.deadline(l, &arr);
            if now < d {
                self.wake_at(d);
                break;
            }
            let x = &mut self.lanes[l];
            (x.expect, x.overdue) = (x.expect + 1, (x.overdue + 1).min(2));
            if x.overdue == 2 && !x.lagging {
                x.lagging = true;
                // My validated up lane other than l with the lowest OWD, the first of equals.
                let mut f: Option<LaneIx> = None;
                for (i, x) in self.lanes.iter().enumerate() {
                    let i = LaneIx::at(i);
                    if i != l && x.validated && x.state == State::Up && f.is_none_or(|f| x.owd < self.lanes[f].owd) {
                        f = Some(i);
                    }
                }
                if let Some(f) = f {
                    self.send_probe(out, now, f, self.lanes[f].peer, MAX_ECHOES);
                }
            }
            a = self.expected(l);
        }
        let last = self.arr[(self.arr_n.wrapping_sub(1) % PENDING_COPIES as u64) as usize].at;
        if a.is_none() && self.lanes[l].lagging && now.saturating_sub(last) > ECHO_WINDOW {
            self.lanes[l].lagging = false;
        }
    }

    /// Probes every candidate of each lane not yet up, preferred candidates last, then its return
    /// address (§6.3).
    ///
    /// While the return address is unvalidated and the peer offered it, it gets a candidate probe
    /// too (Rust only: notes/rust.md, accept: integration). Its own probes are held to
    /// VALIDATE_AMP × the bytes that came from it, which guards an address learned from a packet.
    /// Go skipped it here, so once that budget was spent while nothing came back (the probes
    /// lost, or a new NAT mapping of mine, which lets none of the peer's probes in until it has
    /// sent to the peer), the lane punched only the peer's other candidates and stayed punching
    /// for good. The candidate probe reopens the mapping, so the peer's probes come in again and
    /// refill the budget. It carries no echo, so no challenge: it validates nothing.
    pub(crate) fn punch_probes(&mut self, out: &mut dyn Out, now: Mono) {
        for l in self.lanes.ixs() {
            let x = &self.lanes[l];
            if x.mesh || matches!(x.state, State::Up | State::Suspect) {
                continue;
            }
            // Candidate probes carry no echoes: a peer listing other hosts gets nothing amplified
            // (notes/core.md 7).
            for i in (0..x.cands.len()).rev() {
                let x = &self.lanes[l];
                let c = x.cands[i];
                if x.peer != Some(c) {
                    self.send_probe(out, now, l, Some(c), 0);
                } else if !x.validated {
                    self.probe(out, now, l, c, 0, ((x.pgen, false), false));
                }
            }
            // The return address last, so it carries the newest Seq: lane_tick probes it in this
            // tick when its probe is due, as it is unless the two cadences fell out of step (Go
            // probed it here as well, twice per round).
            if now < self.lanes[l].next_probe {
                self.send_probe(out, now, l, self.lanes[l].peer, MAX_ECHOES);
            }
        }
        self.next_punch = now.saturating_add(PROBE_PUNCH);
    }

    /// Records a packet with counter `c` on lane `l` for the copy deadlines (§8.4).
    pub(crate) fn copied(&mut self, now: Mono, l: LaneIx, c: u64, small: bool, first: bool) {
        // A copy is sent when it is sealed and l is FIFO: any packet passes the copies sealed
        // before it that l still owes, which were lost, and a lost copy is not a stall.
        let x = &mut self.lanes[l];
        x.last_small = x.last_small.max(c.saturating_sub(1));
        if small && first {
            // A first arrival, which the other lanes in the copy set then owe.
            (x.lagging, x.overdue) = (false, 0);
            self.arr[(self.arr_n % PENDING_COPIES as u64) as usize] = Arrival { c, at: now, l: Some(l) };
            self.arr_n += 1;
        } else if small && self.expected(l).is_some_and(|a| a.c == c && now < self.deadline(l, &a)) {
            // A copy on time clears the mark.
            let x = &mut self.lanes[l];
            (x.lagging, x.overdue) = (false, 0);
        }
        let x = &mut self.lanes[l];
        x.last_small = x.last_small.max(c);
    }

    /// The oldest copy `l` still owes, advancing `l.expect` past those it does not. Returned by
    /// value (Go returned a pointer into `arr`). Skips arrivals whose lane has gone.
    pub(crate) fn expected(&mut self, l: LaneIx) -> Option<Arrival> {
        let x = &mut self.lanes[l];
        // Pending deadlines are kept for at most PENDING_COPIES counters, oldest dropped first.
        x.expect = x.expect.max(self.arr_n.saturating_sub(PENDING_COPIES as u64));
        while x.expect < self.arr_n {
            let a = self.arr[(x.expect % PENDING_COPIES as u64) as usize];
            // Skip copies that arrived first on l, and those l delivered or passed.
            if a.l.is_some_and(|al| al != l) && a.c > x.last_small {
                return Some(a);
            }
            x.expect += 1;
        }
        None
    }

    /// When lane `l` owes the copy `a` (§8.4, notes/core.md 8). Uses `self.now`.
    ///
    /// max(LAG_FLOOR, LAG_JITTER × l's jitter) after l would have brought it even as late as it
    /// has lately (the most of its newest OWD, its largest over DEV_WINDOW and its average plus
    /// LAG_JITTER × its jitter), had a.l brought it as early as it can, at its base OWD; and not
    /// before LAG_FLOOR (notes/core.md 33b). That is §8.4's max(10 ms, 3 × dev_A) + OWD_A − OWD_B
    /// with A's latest OWD and B's base. The floor is added, not only a bound on the whole wait:
    /// without it a lane slower than a.l by LAG_FLOOR or more and without jitter owed the copy at
    /// the very time it came, and a full-size copy, slower at the lane's rate than the probes that
    /// measured its OWD, came after (notes/rust.md, netem: A2). A lane that falls silent misses
    /// it; a jittery lane and a queue that builds gradually do not.
    pub(crate) fn deadline(&self, l: LaneIx, a: &Arrival) -> Mono {
        let x = &self.lanes[l];
        let jitter = x.in_dev.saturating_mul(LAG_JITTER as Mono);
        let late = x.rx_owd.saturating_mul(1_000).max(x.in_owd.saturating_add(jitter));
        let late = x.owd_in.max(self.now).map_or(late, |m| late.max(m));
        let base = a.l.map(|b| &self.lanes[b]).map_or(late, |b| b.in_min.get().min(b.in_owd));
        a.at.saturating_add(LAG_FLOOR.max(late.saturating_sub(base).saturating_add(LAG_FLOOR.max(jitter))))
    }

    /// Wakes an idle session on its first datagram (§8.1).
    pub(crate) fn activate(&mut self, now: Mono) {
        (self.active, self.since, self.confirmed, self.last_echo) = (true, now, false, now);
        self.lanes.iter_mut().for_each(|x| x.wait_seq = 0);
        self.wake_lanes(now);
        self.punch(now);
    }

    /// Leaves the client one idle-probe lane; the others are dormant (§8.1).
    pub(crate) fn go_idle(&mut self, now: Mono) {
        self.dozing = true;
        self.lanes.iter_mut().filter(|x| x.state != State::Down).for_each(|x| x.state = State::Dormant);
        self.pick_idle(now);
    }

    /// Chooses the client's idle-probe lane: direct, unmetered first, lowest RTT, the first of
    /// equals.
    pub(crate) fn pick_idle(&mut self, now: Mono) {
        let direct = self.lanes.iter().enumerate().filter(|(_, x)| !x.mesh && x.validated && x.state != State::Down);
        self.idle = match self.role {
            Role::Client => direct.min_by_key(|(_, x)| (x.metered, x.rtt)).map(|(i, _)| LaneIx::at(i)),
            Role::Anchor => None,
        };
        if let Some(i) = self.idle {
            let x = &mut self.lanes[i];
            (x.state, x.misses, x.next_probe) = (State::Up, 0, now.saturating_add(PROBE_IDLE));
        }
    }

    /// Handles a wake from sleep (§8.4): re-probe, reset the minima, nothing suspect or lagging
    /// on what was in flight.
    ///
    /// A paused clock stepped every OWD, so the OWD estimates start again, and only from probes
    /// sent from now on (`woke`, `echo`): the echoes of probes sent before the sleep still arrive,
    /// and would refill the minima on the old clock. qd, which the minima define, starts again
    /// from 0, as the first sample after a reset gives it; the pause drained the path's queue.
    pub(crate) fn wake(&mut self, now: Mono) {
        (self.since, self.woke, self.last_echo) = (now, now, now);
        for x in self.lanes.iter_mut() {
            x.min_rtt.reset();
            x.min_owd.reset();
            x.in_min.reset();
            (x.wait_seq, x.expect) = (0, self.arr_n);
            (x.owd, x.in_owd, x.owds, x.owd_in, x.qd) = (0, 0, Series::default(), Series::default(), 0);
        }
        self.punch(now);
        self.log(now, "woke; lanes re-probed".to_string());
    }
}

#[cfg(test)]
mod tests;
