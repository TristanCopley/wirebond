//! Links and their two directions (Go netsim/link.go): a bottleneck (token bucket, policer or
//! Mahimahi trace) feeding a FIFO, then loss, delay and jitter that never reorder; the radio-wide
//! delay bumps (spikes, AWDL, Starlink), the hotel's measured jitter replay, cold radio, MTU and
//! fragment loss; ground-truth OWD and frame time (§17.2, §17.3).
//!
//! A direction is named by [`DirId`] (its link and [`Side`]). Operations that need the clock, the
//! event queue or the link's radio-wide bumps are `World` methods taking a `DirId`; pure state is
//! on [`Dir`]. Owner: N1.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::ops::{Index, IndexMut};
use std::sync::Arc;

use wirebond_core::SockId;

use crate::nat::{Nat, NatKind};
use crate::node::{DevId, Pkt};
use crate::sim::{Dur, Entry, MS, Rng, SEC, Task, World, secs, splitmix};
use crate::trace::{Trace, TracePos};

/// One direction of a link (§17.2).
#[derive(Clone, Debug, Default)]
pub struct Profile {
    /// Bottleneck bits per second; 0: unlimited.
    pub rate: f64,
    /// Token-bucket depth in bytes (a shaper's burst); at least two packets.
    pub burst: usize,
    /// Drop-tail queue limit, as time at `rate`; 0: unlimited.
    pub buffer: Dur,
    /// A policer: drop above `rate`, no queue.
    pub police: bool,
    /// Mahimahi delivery opportunities instead of `rate`.
    pub trace: Option<Arc<Trace>>,
    /// Propagation delay.
    pub delay: Dur,
    /// Uniform 0..jitter, redrawn every `slot` (1 ms if zero).
    pub jitter: Dur,
    pub slot: Dur,
    /// Replay the hotel's measured ping distribution (the hotel train).
    pub hotel: bool,
    /// Independent loss per packet (per fragment).
    pub loss: f64,
    /// Gilbert–Elliott: P(good → bad) per packet; every packet in bad is lost.
    pub ge_bad: f64,
    /// P(bad → good) per packet.
    pub ge_good: f64,
    /// IPv4 MTU; 0: 1,500.
    pub mtu: usize,
    /// Drop every packet that needs fragmenting on this direction.
    pub frag_drop: bool,
}

/// One link of a device: two directions, a NAT, and the delay bumps that hit both directions
/// because they come from the radio.
#[derive(Clone, Debug)]
pub struct LinkSpec {
    pub label: &'static str,
    pub metered: bool,
    pub default: bool,
    pub up: Profile,
    pub down: Profile,
    pub nat: NatKind,
    /// The socket's address.
    pub local: SocketAddr,
    /// The NAT's public address (IPv4: the models are IPv4, as Go's Rebind assumed).
    pub public: Ipv4Addr,
    /// Ramp to 150 ms over 20 ms every 5 ± 2 s.
    pub spikes: bool,
    /// SPEC §23.3's hotel spikes: 10–50 ms (uniform), every 2–5 s (uniform), each a step that
    /// drains at slope −1.
    pub short_spikes: bool,
    /// The Mac's AWDL channel hops (a guess until measured).
    pub awdl: bool,
    /// Peak of spikes at :12, :27, :42 and :57 of each minute; 0: none.
    pub starlink: Dur,
    /// 260 ms on the first packet after 11 s idle.
    pub cold: bool,
}

impl Default for LinkSpec {
    fn default() -> LinkSpec {
        LinkSpec {
            label: "",
            metered: false,
            default: false,
            up: Profile::default(),
            down: Profile::default(),
            nat: NatKind::OneToOne,
            local: SocketAddr::from(([0, 0, 0, 0], 0)),
            public: Ipv4Addr::UNSPECIFIED,
            spikes: false,
            short_spikes: false,
            awdl: false,
            starlink: 0,
            cold: false,
        }
    }
}

// Model constants. The hotel numbers come from the owner's 100 pings (CONTEXT.md); the rest from
// §17.2 and [Testing 7, Testing 13]. See Go link.go for the derivation of the hotel shape.
pub const COLD_IDLE: Dur = 11 * SEC;
pub const COLD_DELAY: Dur = 260 * MS;
pub const SPIKE_EVERY: Dur = 5 * SEC;
pub const SPIKE_SPREAD: Dur = 2 * SEC;
pub const SPIKE_RISE: Dur = 20 * MS;
pub const SPIKE_PEAK: Dur = 150 * MS;
/// SPEC §23.3's hotel spikes ([`LinkSpec::short_spikes`]): 10–50 ms every 2–5 s.
pub const SHORT_MIN: Dur = 10 * MS;
pub const SHORT_MAX: Dur = 50 * MS;
pub const SHORT_EVERY_MIN: Dur = 2 * SEC;
pub const SHORT_EVERY_MAX: Dur = 5 * SEC;
/// Hotel jitter: Poisson delay episodes per direction at this rate per second, rising over 20 ms
/// to a level uniform in HOTEL_MIN–HOTEL_MAX, held HOTEL_HOLD_MIN–HOTEL_HOLD_MAX, draining at
/// slope −1 (notes/netsim.md 1).
pub const HOTEL_RATE: f64 = 0.036;
pub const HOTEL_MIN: Dur = 150 * MS;
pub const HOTEL_MAX: Dur = 350 * MS;
pub const HOTEL_HOLD_MIN: Dur = 200 * MS;
pub const HOTEL_HOLD_MAX: Dur = SEC;
/// AWDL: a guess until measured on the Mac (CONTEXT.md open item 11): every 1 ± 0.3 s the radio
/// leaves the channel for 10–60 ms, both directions, and traffic waits for it.
pub const AWDL_EVERY: Dur = SEC;
pub const AWDL_SPREAD: Dur = 300 * MS;
pub const AWDL_MIN: Dur = 10 * MS;
pub const AWDL_MAX: Dur = 60 * MS;
/// IPv4 and UDP headers.
pub const IP_UDP: usize = 28;
/// Tailscale's per-packet overhead on the default link, headers included.
pub const TS_OVER: usize = 32 + IP_UDP;

/// A link: index into the world's links (Go *Link).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkId(pub usize);

/// Which direction of a link: Up leaves the device, Down reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Up,
    Down,
}

/// One direction of one link (Go *Dir; the Recorder keys its queues by it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DirId {
    pub link: LinkId,
    pub side: Side,
}

impl LinkId {
    pub fn up(self) -> DirId {
        DirId { link: self, side: Side::Up }
    }
    pub fn down(self) -> DirId {
        DirId { link: self, side: Side::Down }
    }
    pub fn dir(self, side: Side) -> DirId {
        DirId { link: self, side }
    }
}

/// One interface of a device and its path to the internet (Go Link).
pub struct Link {
    pub spec: LinkSpec,
    /// 1–255 within the device: its index in the device's links + 1.
    pub id: u8,
    pub dev: DevId,
    /// The link's socket: `SockId::from(id)`.
    pub sock: SockId,
    pub local: SocketAddr,
    /// The STUN mapping node knows; None until STUN answers.
    pub mapped: Option<SocketAddr>,
    pub nat: Nat,
    pub up: Dir,
    pub down: Dir,
    /// Radio-wide trains: spikes, AWDL, Starlink.
    pub(crate) bumps: Vec<Bumps>,
    /// Last departure on either direction, for cold radio (Go used).
    pub(crate) used: Dur,
    /// A STUN request is pending since; None: none (Go −1).
    pub(crate) stun_at: Option<Dur>,
}

impl Link {
    /// Every wire byte offered to the link, both directions, STUN included (Go Bytes).
    pub fn bytes(&self) -> i64 {
        self.up.bytes + self.down.bytes
    }

    pub fn dir(&self, side: Side) -> &Dir {
        match side {
            Side::Up => &self.up,
            Side::Down => &self.down,
        }
    }

    pub fn dir_mut(&mut self, side: Side) -> &mut Dir {
        match side {
            Side::Up => &mut self.up,
            Side::Down => &mut self.down,
        }
    }

    /// One direction together with the link's radio trains, which its jitter reads.
    fn dir_bumps(&mut self, side: Side) -> (&mut Dir, &mut [Bumps]) {
        let Link { up, down, bumps, .. } = self;
        match side {
            Side::Up => (up, bumps),
            Side::Down => (down, bumps),
        }
    }
}

/// One direction of a link: a bottleneck feeding a FIFO, then loss, delay and jitter that never
/// reorder (Go Dir). The Go back-pointer to the link is the [`DirId`] the World methods take;
/// Go's unused tbase and tperiod are dropped.
pub struct Dir {
    pub p: Profile,
    /// Blackholed.
    pub hole: bool,
    /// Wire bytes offered.
    pub bytes: i64,
    /// Packets dropped (any cause).
    pub drops: i64,
    /// Packets offered.
    pub pkts: i64,
    /// Of the drops, those the full queue or the policer made.
    pub qdrops: i64,
    pub(crate) r: Rng,
    /// Seeds the time-keyed jitter draws.
    pub(crate) seed: u64,
    /// The hotel train, when `p.hotel`.
    pub(crate) hotel: Option<Bumps>,
    pub(crate) q: VecDeque<QEnt>,
    /// Bytes queued; the drop-tail limit in bytes (0: none).
    pub(crate) qb: usize,
    pub(crate) limit: usize,
    pub(crate) tok: f64,
    pub(crate) depth: f64,
    pub(crate) tok_at: Dur,
    /// The head of the queue is scheduled to leave.
    pub(crate) busy: bool,
    /// Gilbert–Elliott state.
    pub(crate) bad: bool,
    /// Newest arrival: the FIFO rule (Go initial −1).
    pub(crate) last: Dur,
    /// Trace position (Go topp, left).
    pub(crate) tpos: TracePos,
    /// Packets past the bottleneck, in arrival order; the first is in the event heap (see
    /// `Task::Arrive`). New in Rust: Go put every one in its heap.
    pub(crate) flight: VecDeque<Flight>,
}

/// A packet in flight on a direction: it arrives at `at` as the event numbered `seq`, both drawn
/// when it departed.
pub(crate) struct Flight {
    pub(crate) at: Dur,
    pub(crate) seq: u64,
    pub(crate) then: Then,
    pub(crate) p: Pkt,
}

/// A queued packet: n wire bytes in k fragments, and where it goes on arrival (Go qent).
pub(crate) struct QEnt {
    pub(crate) p: Pkt,
    pub(crate) n: usize,
    pub(crate) k: usize,
    pub(crate) then: Then,
}

/// What happens to a packet when its direction delivers it (Go's `then func(*pkt)`). The two
/// routing continuations are variants so the per-packet path allocates no closure.
pub enum Then {
    /// It left link `from` through its NAT with public source `src`, addressed to `to`:
    /// [`World::arrive`].
    Arrive { from: LinkId, src: SocketAddr, to: SocketAddr },
    /// It reaches device `dev` on `link` (None: the Tailscale lane) from `src`: [`World::recv`].
    Recv { dev: DevId, link: Option<LinkId>, src: SocketAddr },
    /// Anything else (the model tests).
    Call(ThenFn),
}

/// A packet's continuation as a closure ([`Then::Call`]).
pub type ThenFn = Box<dyn FnOnce(&mut World, Pkt)>;

impl Dir {
    /// A direction with profile p drawing from r (Go newDir): seeds the jitter, sets Slot to 1 ms
    /// if zero, depth = max(burst, 3,000), full bucket, the drop-tail limit from rate × buffer, and
    /// the hotel train (its own sub-Rng drawn from r) when p.hotel.
    pub(crate) fn new(mut p: Profile, mut r: Rng) -> Dir {
        let seed = r.u64();
        if p.slot == 0 {
            p.slot = MS;
        }
        let depth = (p.burst as f64).max(3000.0);
        let hotel = p.hotel.then(|| hotel_train(r.split()));
        let mut d = Dir {
            p,
            hole: false,
            bytes: 0,
            drops: 0,
            pkts: 0,
            qdrops: 0,
            r,
            seed,
            hotel,
            q: VecDeque::new(),
            qb: 0,
            limit: 0,
            tok: depth,
            depth,
            tok_at: 0,
            busy: false,
            bad: false,
            last: -1,
            tpos: TracePos::default(),
            flight: VecDeque::new(),
        };
        let rate = d.rate();
        if rate > 0.0 && d.p.buffer > 0 {
            d.limit = (rate / 8.0 * secs(d.p.buffer)) as usize;
        }
        d
    }

    /// The direction's IPv4 MTU (0 in the profile: 1,500).
    pub fn mtu(&self) -> usize {
        if self.p.mtu == 0 { 1500 } else { self.p.mtu }
    }

    /// Adds the tokens earned since tok_at.
    pub(crate) fn refill(&mut self, now: Dur) {
        if now > self.tok_at {
            self.tok = self.depth.min(self.tok + self.p.rate / 8.0 * secs(now - self.tok_at));
            self.tok_at = now;
        }
    }

    /// Draws one packet's (one fragment's) loss: Gilbert–Elliott, then independent loss.
    pub(crate) fn lost(&mut self) -> bool {
        if self.p.ge_bad > 0.0 {
            self.bad = if self.bad { self.r.f64() >= self.p.ge_good } else { self.r.f64() < self.p.ge_bad };
            if self.bad {
                return true;
            }
        }
        self.p.loss > 0.0 && self.r.f64() < self.p.loss
    }

    /// The bottleneck's rate in bits per second, a trace's average for a trace.
    pub fn rate(&self) -> f64 {
        self.p.trace.as_ref().map_or(self.p.rate, |t| t.rate())
    }

    /// The time the bottleneck needs to drain what it holds now (0 for a policer).
    pub fn queue(&self) -> Dur {
        let r = self.rate();
        if r > 0.0 && !self.p.police { (self.qb as f64 * 8.0 / r * 1e9) as Dur } else { 0 }
    }
}

/// The number of IPv4 fragments of a packet of ip bytes at mtu, and their wire bytes.
pub fn frags(ip: usize, mtu: usize) -> (usize, usize) {
    if ip <= mtu {
        return (1, ip);
    }
    let per = (mtu - 20) & !7;
    let k = (ip - 20).div_ceil(per);
    (k, ip + (k - 1) * 20)
}

/// The extra delay of a packet leaving dir at t (see [`World::jitter`]); bumps are its link's
/// radio trains.
fn jitter_of(dir: &mut Dir, bumps: &mut [Bumps], t: Dur) -> Dur {
    let mut j = 0;
    if dir.p.jitter > 0 {
        let u = splitmix(dir.seed ^ (t / dir.p.slot) as u64);
        j = (u % dir.p.jitter as u64) as Dur;
    }
    let mut b = dir.hotel.as_mut().map_or(0, |h| h.at(t));
    for x in bumps {
        b = b.max(x.at(t));
    }
    j + b
}

impl World {
    /// Changes d's bottleneck rate from now; packets already queued keep their times (Go SetRate).
    pub fn set_rate(&mut self, d: DirId, bps: f64) {
        let now = self.now;
        let dir = &mut self[d];
        dir.refill(now);
        dir.p.rate = bps;
    }

    /// Offers p (n wire bytes in k fragments) to d; then runs when it arrives (Go Dir.send).
    /// Counts bytes and packets; a hole or a fragmented packet with frag_drop drops it; unlimited
    /// departs now; a policer drops above rate; otherwise drop-tail queue, served in order.
    pub(crate) fn dir_send(&mut self, d: DirId, p: Pkt, n: usize, k: usize, then: Then) {
        let now = self.now;
        let dir = &mut self[d];
        dir.bytes += n as i64;
        dir.pkts += 1;
        if dir.hole || k > 1 && dir.p.frag_drop {
            dir.drops += 1;
            return;
        }
        let e = QEnt { p, n, k, then };
        if dir.p.trace.is_none() && dir.p.rate == 0.0 {
            self.depart(d, e, now);
        } else if dir.p.police {
            dir.refill(now);
            if dir.tok < n as f64 {
                dir.drops += 1;
                dir.qdrops += 1;
                return;
            }
            dir.tok -= n as f64;
            self.depart(d, e, now);
        } else {
            if dir.limit > 0 && dir.qb + n > dir.limit {
                dir.drops += 1;
                dir.qdrops += 1;
                return;
            }
            dir.q.push_back(e);
            dir.qb += n;
            if !dir.busy {
                dir.busy = true;
                self.serve(d);
            }
        }
    }

    /// Schedules the head of d's queue to leave when the bottleneck lets it (Go serve): a
    /// `Task::Serve` that pops it, departs it and serves the next.
    pub(crate) fn serve(&mut self, d: DirId) {
        let now = self.now;
        let dir = &mut self[d];
        let n = dir.q.front().expect("serve: the queue is empty").n;
        let t = if let Some(tr) = &dir.p.trace {
            tr.ready(&mut dir.tpos, now, n)
        } else {
            dir.refill(now);
            let mut t = now;
            let n = n as f64;
            if dir.tok < n {
                // A rate set to 0 means unlimited: the queue goes at once. Go divided by zero
                // here, and amd64 turns the +Inf wait into a time in the past.
                if dir.p.rate > 0.0 {
                    t += ((n - dir.tok) / (dir.p.rate / 8.0) * 1e9) as Dur;
                }
                dir.tok = n;
                dir.tok_at = t;
            }
            dir.tok -= n;
            t
        };
        self.push(t, Task::Serve(d));
    }

    /// The head of d's queue leaves the bottleneck now: the event [`World::serve`] scheduled (Go's
    /// closure in serve). Only this event pops the queue, so its head is the packet serve saw, and
    /// the clock stands at the time serve chose.
    pub(crate) fn dir_leave(&mut self, d: DirId) {
        let now = self.now;
        let dir = &mut self[d];
        let e = dir.q.pop_front().expect("a Serve event with an empty queue");
        dir.qb -= e.n;
        self.depart(d, e, now);
        let dir = &mut self[d];
        dir.busy = !dir.q.is_empty();
        if dir.busy {
            self.serve(d);
        }
    }

    /// Applies loss, delay, jitter, cold radio and the FIFO rule to a packet leaving d's
    /// bottleneck at t, and queues its arrival in d's flight, as an event of its own (a
    /// `Task::Arrive` once it is first) (Go depart).
    pub(crate) fn depart(&mut self, d: DirId, e: QEnt, t: Dur) {
        let Link { up, down, bumps, spec, used, .. } = &mut self.links[d.link.0];
        let dir = match d.side {
            Side::Up => up,
            Side::Down => down,
        };
        for _ in 0..e.k {
            if dir.lost() {
                dir.drops += 1;
                return;
            }
        }
        let mut at = t + dir.p.delay + jitter_of(dir, bumps, t);
        if spec.cold {
            if t - *used > COLD_IDLE {
                at += COLD_DELAY;
            }
            *used = t;
        }
        at = at.max(dir.last);
        dir.last = at;
        // The time and number push would give it (Go's At), so events keep their order.
        let at = at.max(self.now);
        self.seq += 1;
        let seq = self.seq;
        dir.flight.push_back(Flight { at, seq, then: e.then, p: e.p });
        if dir.flight.len() == 1 {
            let task = Task::Arrive(d);
            self.q.push(Entry { at, seq, task });
        }
    }

    /// The first packet in flight on d arrives now (the `Task::Arrive` depart queued): the next
    /// takes its place in the heap, then the packet's continuation runs.
    pub(crate) fn dir_arrive(&mut self, d: DirId) {
        let dir = &mut self[d];
        let f = dir.flight.pop_front().expect("an Arrive event with nothing in flight");
        if let Some(n) = dir.flight.front() {
            let (at, seq) = (n.at, n.seq);
            self.q.push(Entry { at, seq, task: Task::Arrive(d) });
        }
        self.then(f.then, f.p);
    }

    /// Runs a delivered packet's continuation.
    pub(crate) fn then(&mut self, t: Then, p: Pkt) {
        match t {
            Then::Arrive { from, src, to } => self.arrive(p, from, src, to),
            Then::Recv { dev, link, src } => self.recv(dev, link, src, p),
            Then::Call(f) => f(self, p),
        }
    }

    /// The extra delay of a packet leaving d at t: a function of time, not of the packet, so the
    /// frame-time criteria replay the same draws (§17.3): slot jitter + the largest bump (d's
    /// hotel train, the link's radio trains). Mutable because trains grow lazily (Go jitter).
    pub fn jitter(&mut self, d: DirId, t: Dur) -> Dur {
        let (dir, bumps) = self.links[d.link.0].dir_bumps(d.side);
        jitter_of(dir, bumps, t)
    }

    /// The ground-truth one-way delay of a small packet sent on d now (§17.3): queue, delay,
    /// jitter and the FIFO rule (Go OWD).
    pub fn owd(&mut self, d: DirId) -> Dur {
        let now = self.now;
        let (dir, bumps) = self.links[d.link.0].dir_bumps(d.side);
        let dep = now + dir.queue();
        dir.last.max(dep + dir.p.delay + jitter_of(dir, bumps, dep)) - now
    }

    /// When k packets of n wire bytes, the first sent at t0, would all arrive on d alone (§17.3):
    /// packet i departs at t0 + i·n·8 ÷ rate and arrives at max(previous arrival, departure +
    /// delay + jitter(departure)) (Go FrameTime).
    pub fn frame_time(&mut self, d: DirId, t0: Dur, n: usize, k: usize) -> Dur {
        let (dir, bumps) = self.links[d.link.0].dir_bumps(d.side);
        let r = dir.rate();
        let mut arr = 0;
        for i in 0..k {
            let mut dep = t0;
            if r > 0.0 {
                dep += ((i * n) as f64 * 8.0 / r * 1e9) as Dur;
            }
            arr = arr.max(dep + dir.p.delay + jitter_of(dir, bumps, dep));
        }
        arr
    }

    /// The ground-truth round trip of the fastest client link now; 60 ms with no link (Go RTT).
    pub fn rtt(&mut self) -> Dur {
        let mut best = Dur::MAX;
        for i in 0..self[DevId::Client].links.len() {
            let l = self[DevId::Client].links[i];
            best = best.min(self.owd(l.up()) + self.owd(l.down()));
        }
        if best == Dur::MAX { 60 * MS } else { best }
    }
}

impl Index<LinkId> for World {
    type Output = Link;
    fn index(&self, l: LinkId) -> &Link {
        &self.links[l.0]
    }
}

impl IndexMut<LinkId> for World {
    fn index_mut(&mut self, l: LinkId) -> &mut Link {
        &mut self.links[l.0]
    }
}

impl Index<DirId> for World {
    type Output = Dir;
    fn index(&self, d: DirId) -> &Dir {
        self.links[d.link.0].dir(d.side)
    }
}

impl IndexMut<DirId> for World {
    fn index_mut(&mut self, d: DirId) -> &mut Dir {
        self.links[d.link.0].dir_mut(d.side)
    }
}

// ---------------------------------------------------------------------------------------------
// Delay bumps

/// One delay episode: rises linearly to peak over rise, holds it for hold, then falls at slope
/// −1, so packets sent while it drains arrive together when it ends, as from a stall (Go bump).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Bump {
    pub(crate) start: Dur,
    pub(crate) rise: Dur,
    pub(crate) peak: Dur,
    pub(crate) hold: Dur,
}

impl Bump {
    /// The extra delay at t.
    pub(crate) fn at(&self, t: Dur) -> Dur {
        let x = t - self.start;
        if x < 0 {
            0
        } else if x < self.rise {
            self.peak * x / self.rise
        } else if x < self.rise + self.hold {
            self.peak
        } else {
            (self.peak - (x - self.rise - self.hold)).max(0)
        }
    }

    pub(crate) fn end(&self) -> Dur {
        self.start + self.rise + self.hold + self.peak
    }
}

/// What makes a train's next bump after `prev` (Go's gen closures). Each random train owns its
/// own sub-Rng (Go shared one per link or direction).
#[derive(Clone, Debug)]
pub(crate) enum Gen {
    /// Exp(HOTEL_RATE) gaps, peak uniform HOTEL_MIN–HOTEL_MAX, hold HOTEL_HOLD_MIN–HOTEL_HOLD_MAX.
    Hotel(Rng),
    /// Every SPIKE_EVERY ± SPIKE_SPREAD, rising over SPIKE_RISE to the peak.
    Spike(Rng, Dur),
    /// §23.3: every SHORT_EVERY_MIN–SHORT_EVERY_MAX, a step of SHORT_MIN–SHORT_MAX.
    Short(Rng),
    /// Every AWDL_EVERY ± AWDL_SPREAD, a gap of AWDL_MIN–AWDL_MAX.
    Awdl(Rng),
    /// At :12, :27, :42 and :57 of each minute, rising over SPIKE_RISE to the peak.
    Starlink(Dur),
}

/// A train of bumps from a generator, made lazily in time order so a query for any time sees
/// the same train (Go bumps).
#[derive(Clone, Debug)]
pub(crate) struct Bumps {
    pub(crate) make: Gen,
    pub(crate) list: VecDeque<Bump>,
    pub(crate) upto: Dur,
}

impl Bumps {
    pub(crate) fn new(make: Gen) -> Bumps {
        Bumps { make, list: VecDeque::new(), upto: 0 }
    }

    /// The train's next bump after prev (Go gen(prev)), drawing in Go's order.
    pub(crate) fn next(&mut self, prev: Dur) -> Bump {
        match &mut self.make {
            Gen::Hotel(r) => {
                let start = prev + (r.exp() / HOTEL_RATE * 1e9) as Dur;
                let peak = HOTEL_MIN + r.dur(HOTEL_MAX - HOTEL_MIN);
                let hold = HOTEL_HOLD_MIN + r.dur(HOTEL_HOLD_MAX - HOTEL_HOLD_MIN);
                Bump { start, rise: SPIKE_RISE, peak, hold }
            }
            Gen::Spike(r, peak) => Bump {
                start: prev + SPIKE_EVERY - SPIKE_SPREAD + r.dur(2 * SPIKE_SPREAD),
                rise: SPIKE_RISE,
                peak: *peak,
                hold: 0,
            },
            Gen::Short(r) => {
                let start = prev + SHORT_EVERY_MIN + r.dur(SHORT_EVERY_MAX - SHORT_EVERY_MIN);
                Bump { start, rise: 0, peak: SHORT_MIN + r.dur(SHORT_MAX - SHORT_MIN), hold: 0 }
            }
            Gen::Awdl(r) => {
                let gap = AWDL_MIN + r.dur(AWDL_MAX - AWDL_MIN);
                Bump { start: prev + AWDL_EVERY - AWDL_SPREAD + r.dur(2 * AWDL_SPREAD), rise: 0, peak: gap, hold: 0 }
            }
            Gen::Starlink(peak) => {
                const EVERY: Dur = 15 * SEC;
                let mut next = (prev - 12 * SEC) / EVERY * EVERY + 12 * SEC;
                while next <= prev {
                    next += EVERY;
                }
                Bump { start: next, rise: SPIKE_RISE, peak: *peak, hold: 0 }
            }
        }
    }

    /// The train's delay at t: generates up to t + 1 s, forgets bumps that ended before t − 2 s.
    pub(crate) fn at(&mut self, t: Dur) -> Dur {
        while self.upto <= t + SEC {
            let b = self.next(self.upto);
            self.list.push_back(b);
            self.upto = b.start;
        }
        while self.list.len() > 1 && self.list[0].end() < t - 2 * SEC {
            self.list.pop_front();
        }
        let mut v = 0;
        for b in &self.list {
            if b.start > t {
                break;
            }
            v = v.max(b.at(t));
        }
        v
    }
}

pub(crate) fn hotel_train(r: Rng) -> Bumps {
    Bumps::new(Gen::Hotel(r))
}

pub(crate) fn spike_train(r: Rng, peak: Dur) -> Bumps {
    Bumps::new(Gen::Spike(r, peak))
}

pub(crate) fn short_train(r: Rng) -> Bumps {
    Bumps::new(Gen::Short(r))
}

pub(crate) fn awdl_train(r: Rng) -> Bumps {
    Bumps::new(Gen::Awdl(r))
}

/// Spikes at :12, :27, :42 and :57 of every minute [Landscape 8].
pub(crate) fn starlink_train(peak: Dur) -> Bumps {
    Bumps::new(Gen::Starlink(peak))
}

#[cfg(test)]
pub(crate) mod tests {
    //! Go models_test.go's link tests. Helpers: `test_link` (Go testLink) and `drive` (Go drive).

    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::env::{ADDR_HOTEL, PUB_HOTEL, hotel, hotel_clean};
    use crate::fake::{Evil, fake_maker};
    use crate::sim::{NS, US, fmt_dur};

    /// A world with one client link whose two directions use p (Go testLink).
    pub(crate) fn test_link(p: Profile, s: LinkSpec) -> (World, LinkId) {
        let mut w = World::new(1, fake_maker(false, Evil::None));
        let s = LinkSpec { up: p.clone(), down: p, local: ADDR_HOTEL, public: PUB_HOTEL, ..s };
        let l = w.add_link(DevId::Client, s);
        (w, l)
    }

    /// Offers n-byte packets to d every gap until until and returns, per packet, when it was sent
    /// and when it arrived (None: dropped) (Go drive).
    pub(crate) fn drive(w: &mut World, d: DirId, n: usize, gap: Dur, until: Dur) -> Vec<(Dur, Option<Dur>)> {
        let r = Rc::new(RefCell::new(Vec::new()));
        let rr = r.clone();
        w.every(0, gap, move |w| {
            let i = {
                let mut r = rr.borrow_mut();
                r.push((w.now(), None));
                r.len() - 1
            };
            let rr = rr.clone();
            let then = Then::Call(Box::new(move |w, _| rr.borrow_mut()[i].1 = Some(w.now())));
            w.dir_send(d, Pkt::new(vec![0; n]), n, 1, then);
            w.now() + gap < until
        });
        w.run(until + 2 * SEC).unwrap();
        r.take()
    }

    /// Sends one n-byte packet on d now and logs its arrival time in log.
    fn send_logged(w: &mut World, d: DirId, n: usize, log: &Rc<RefCell<Vec<Dur>>>) {
        let log = log.clone();
        let then = Then::Call(Box::new(move |w, _| log.borrow_mut().push(w.now())));
        w.dir_send(d, Pkt::default(), n, 1, then);
    }

    /// Fails once with every complaint.
    fn report(errs: &[String]) {
        assert!(errs.is_empty(), "{}", errs.join("\n"));
    }

    /// Go TestShaper: 38 Mbps ± 2%, the 64 KB burst passes unqueued, the 180 ms buffer bounds delay.
    #[test]
    fn shaper() {
        let p = Profile { jitter: 0, ..hotel_clean() };
        let (mut w, l) = test_link(p.clone(), LinkSpec::default());
        // 76 Mbps: twice the rate.
        let r = drive(&mut w, l.up(), 1500, 157_894 * NS, 2 * SEC);
        let (mut bytes, mut drops, mut max_delay) = (0usize, 0i64, 0);
        let mut errs = Vec::new();
        for (i, &(sent, got)) in r.iter().enumerate() {
            let Some(got) = got else {
                drops += 1;
                continue;
            };
            if i < 40 && got - sent != p.delay {
                let waited = fmt_dur(got - sent - p.delay);
                errs.push(format!("packet {i} inside the 64 KB burst waited {waited}"));
            }
            if (500 * MS..2 * SEC).contains(&got) {
                bytes += 1500;
            }
            max_delay = max_delay.max(got - sent);
        }
        let rate = bytes as f64 * 8.0 / 1.5;
        if (rate - 38e6).abs() > 0.02 * 38e6 {
            errs.push(format!("shaper delivered {:.1} Mbps, want 38", rate / 1e6));
        }
        let limit = p.delay + p.buffer + MS;
        if max_delay > limit || max_delay < p.delay + p.buffer - 5 * MS {
            errs.push(format!(
                "longest delay {}, want about {} (the 180 ms buffer)",
                fmt_dur(max_delay),
                fmt_dur(p.delay + p.buffer)
            ));
        }
        let counted = w[l.up()].drops;
        if drops == 0 || counted != drops {
            errs.push(format!("drops {drops} (Dir counts {counted}), want some at twice the rate"));
        }
        report(&errs);
    }

    /// Go TestPolicer: about 10 Mbps passes, nothing queued.
    #[test]
    fn policer() {
        let p = Profile { rate: 10e6, burst: 16 << 10, police: true, delay: 30 * MS, ..Profile::default() };
        let (mut w, l) = test_link(p.clone(), LinkSpec::default());
        // 20 Mbps.
        let r = drive(&mut w, l.up(), 1000, 400 * US, 2 * SEC);
        let mut got = 0;
        for &(sent, arrived) in &r {
            if let Some(a) = arrived {
                got += 1;
                let queued = a - sent - p.delay;
                assert_eq!(queued, 0, "a policer queued a packet for {}", fmt_dur(queued));
            }
        }
        let rate = (got * 1000 * 8) as f64 / 2.0;
        assert!((9e6..=10.5e6).contains(&rate), "policer passed {:.1} Mbps, want about 10", rate / 1e6);
    }

    /// Go TestJitterNeverReorders: hotel jitter, spikes and AWDL never reorder; some packet > 130 ms.
    #[test]
    fn jitter_never_reorders() {
        let s = LinkSpec { spikes: true, awdl: true, ..LinkSpec::default() };
        let (mut w, l) = test_link(hotel(), s);
        let r = drive(&mut w, l.down(), 200, MS, 60 * SEC);
        let mut slow = 0;
        for (i, p) in r.windows(2).enumerate() {
            if let ((_, Some(a0)), (_, Some(a1))) = (p[0], p[1]) {
                assert!(a1 >= a0, "packet {} arrived before packet {i}", i + 1);
            }
            if let (sent, Some(a)) = p[1]
                && a - sent > 130 * MS
            {
                slow += 1;
            }
        }
        assert!(slow > 0, "no packet above 130 ms in a minute of hotel jitter, spikes and AWDL gaps");
    }

    /// Go TestHotelJitter: RTT excess mean 9–22 ms, deviation 45–75 ms, max < 2 × HOTEL_MAX + 4 ms.
    /// It checks the replayed distribution against the owner's pings: a ping's RTT excess over the
    /// idle base is the up direction's jitter plus the down direction's.
    #[test]
    fn hotel_jitter() {
        let p = Profile { jitter: 0, ..hotel() };
        let (mut w, l) = test_link(p, LinkSpec::default());
        let mut r = Rng::new(3, 4);
        let xs: Vec<f64> = (0..20_000)
            .map(|i| {
                let at = i * SEC + r.dur(SEC);
                (w.jitter(l.up(), at) + w.jitter(l.down(), at + 15 * MS)) as f64 / 1e6
            })
            .collect();
        let n = xs.len() as f64;
        let (sum, sq, top) = xs.iter().fold((0.0, 0.0, 0f64), |(s, q, t), &x| (s + x, q + x * x, t.max(x)));
        let mean = sum / n;
        let sd = (sq / n - mean * mean).sqrt();
        eprintln!("RTT excess over 20,000 samples: mean {mean:.1} ms, sd {sd:.1} ms, max {top:.0} ms");
        assert!(
            (9.0..=22.0).contains(&mean) && (45.0..=75.0).contains(&sd) && top <= 2.0 * HOTEL_MAX as f64 / 1e6 + 4.0,
            "hotel jitter mean {mean:.1} ms, sd {sd:.1} ms, max {top:.0} ms; want about 14, 60 and \
             under 700"
        );
    }

    /// Go TestSpikesAndStarlink: 100–145 spikes in 10 minutes; Starlink at :12, :27, :42, :57.
    #[test]
    fn spikes_and_starlink() {
        let s = LinkSpec { spikes: true, ..LinkSpec::default() };
        let (mut w, l) = test_link(Profile::default(), s);
        let mut errs = Vec::new();
        let (mut n, mut high, mut at) = (0, false, 0);
        while at < 600 * SEC {
            let v = w[l].bumps[0].at(at);
            if v > 100 * MS && !high {
                n += 1;
            }
            high = v > 100 * MS;
            at += MS;
        }
        if !(100..=145).contains(&n) {
            errs.push(format!("{n} spikes in 10 minutes, want about 120 (every 5 ± 2 s)"));
        }
        let mut s = starlink_train(120 * MS);
        for sec in [12, 27, 42, 57, 72, 117] {
            let at = sec * SEC + SPIKE_RISE;
            let (on, before) = (s.at(at), s.at(at - SEC));
            if on != 120 * MS || before != 0 {
                errs.push(format!("no 120 ms Starlink spike at {sec} s ({})", fmt_dur(on)));
            }
        }
        report(&errs);
    }

    /// Go TestFragments: frags(1471, 1280) = (2, 1491); frag_drop passes only the small packet.
    #[test]
    fn fragments() {
        let mut errs = Vec::new();
        let (k, n) = frags(1471, 1280);
        if (k, n) != (2, 1491) {
            errs.push(format!("1,471 bytes at MTU 1,280: {k} fragments, {n} bytes; want 2 and 1,491"));
        }
        let p = Profile { mtu: 1400, frag_drop: true, delay: MS, ..Profile::default() };
        let (mut w, l) = test_link(p, LinkSpec::default());
        let got: Rc<RefCell<Vec<usize>>> = Rc::default();
        for size in [1443, 200] {
            let (k, n) = frags(size + IP_UDP, w[l.up()].mtu());
            let got = got.clone();
            let then = Then::Call(Box::new(move |_, p| got.borrow_mut().push(p.data.len())));
            w.dir_send(l.up(), Pkt::new(vec![0; size]), n, k, then);
        }
        w.run(SEC).unwrap();
        let got = got.take();
        if got != [200] {
            errs.push(format!("delivered {got:?}, want only the 200-byte packet"));
        }
        report(&errs);
    }

    /// Go TestColdRadio: 300 ms cold, 40 ms warm, 300 ms after 11 s idle.
    #[test]
    fn cold_radio() {
        let p = Profile { delay: 40 * MS, ..Profile::default() };
        let s = LinkSpec { cold: true, ..LinkSpec::default() };
        let (mut w, l) = test_link(p, s);
        let d: Rc<RefCell<Vec<Dur>>> = Rc::default();
        for at in [0, 500 * MS, 12 * SEC] {
            let d = d.clone();
            w.at(at, move |w| {
                let then = Then::Call(Box::new(move |w, _| d.borrow_mut().push(w.now() - at)));
                w.dir_send(l.up(), Pkt::default(), 100, 1, then);
            });
        }
        w.run(20 * SEC).unwrap();
        assert_eq!(d.take(), [300 * MS, 40 * MS, 300 * MS], "want 300 ms cold, 40 ms warm, 300 ms after 11 s idle");
    }

    /// Go TestGilbertElliott: loss 0.01/0.26 ± 0.5%, mean burst 4 ± 0.5.
    #[test]
    fn gilbert_elliott() {
        let p = Profile { ge_bad: 0.01, ge_good: 0.25, ..Profile::default() };
        let (mut w, l) = test_link(p, LinkSpec::default());
        let (mut lost, mut runs, mut prev) = (0, 0, false);
        for _ in 0..200_000 {
            let x = w[l.up()].lost();
            if x {
                lost += 1;
                if !prev {
                    runs += 1;
                }
            }
            prev = x;
        }
        let mut errs = Vec::new();
        let loss = f64::from(lost) / 200_000.0;
        if (loss - 0.01 / 0.26).abs() > 0.005 {
            errs.push(format!("loss {:.2}%, want {:.2}%", loss * 100.0, 100.0 * 0.01 / 0.26));
        }
        let burst = f64::from(lost) / f64::from(runs);
        if (burst - 4.0).abs() > 0.5 {
            errs.push(format!("mean burst {burst:.1} packets, want 4"));
        }
        report(&errs);
    }

    /// Not in Go: the token bucket's times, set_rate (to a lower rate, then to 0, which lets the
    /// queue go at once), a hole, and the ground truth (OWD, queue, frame time, RTT) of §17.3.
    #[test]
    fn bucket_and_ground_truth() {
        let mut w = World::new(1, fake_maker(false, Evil::None));
        assert_eq!(w.rtt(), 60 * MS, "RTT with no link");
        // 8 Mbps is a byte per microsecond; the bucket holds three 1,000-byte packets.
        let p = Profile { rate: 8e6, burst: 3000, delay: 30 * MS, ..Profile::default() };
        let (mut w, l) = test_link(p, LinkSpec::default());
        assert_eq!((w.owd(l.up()), w.rtt()), (30 * MS, 60 * MS));
        assert_eq!(
            w.frame_time(l.down(), SEC, 1000, 11),
            SEC + 10 * MS + 30 * MS,
            "the 11th packet leaves 10 ms after the first"
        );
        let log: Rc<RefCell<Vec<Dur>>> = Rc::default();
        for _ in 0..5 {
            send_logged(&mut w, l.up(), 1000, &log);
        }
        assert_eq!((w[l.up()].queue(), w.owd(l.up())), (5 * MS, 35 * MS));
        w.run(SEC).unwrap();
        let want = [30, 30, 30, 31, 32].map(|x| x * MS);
        assert_eq!(log.take(), want, "three in the burst, then one per ms");
        assert_eq!(w[l.up()].queue(), 0);

        w.set_rate(l.up(), 4e6);
        for _ in 0..5 {
            send_logged(&mut w, l.up(), 1000, &log);
        }
        w.run(2 * SEC).unwrap();
        let want = [30, 30, 30, 32, 34].map(|x| SEC + x * MS);
        assert_eq!(log.take(), want, "at 4 Mbps, one per 2 ms");

        for _ in 0..5 {
            send_logged(&mut w, l.up(), 1000, &log);
        }
        w.set_rate(l.up(), 0.0);
        send_logged(&mut w, l.up(), 1000, &log);
        w.run(3 * SEC).unwrap();
        assert_eq!(log.take(), [2 * SEC + 30 * MS; 6], "rate 0 is unlimited");

        w[l.up()].hole = true;
        send_logged(&mut w, l.up(), 1000, &log);
        w.run(4 * SEC).unwrap();
        let d = &w[l.up()];
        assert_eq!((log.take().len(), d.drops, d.pkts, d.bytes), (0, 1, 17, 17_000));
        assert_eq!(w[l].bytes(), 17_000);
    }

    /// Not in Go: a direction's packets in flight wait in its own queue with only the first in the
    /// heap, and still run in (time, sequence) order among other events: an event queued for the
    /// same time before a packet departed runs before its arrival, one queued after runs after,
    /// and a continuation may send on its own direction.
    #[test]
    fn arrivals_keep_event_order() {
        let p = Profile { delay: 10 * MS, ..Profile::default() };
        let (mut w, l) = test_link(p, LinkSpec::default());
        let log: Rc<RefCell<Vec<&str>>> = Rc::default();
        let note = |s: &'static str| {
            let log = log.clone();
            move |_: &mut World| log.borrow_mut().push(s)
        };
        let then = |s: &'static str| {
            let log = log.clone();
            Then::Call(Box::new(move |_, _| log.borrow_mut().push(s)))
        };
        w.at(10 * MS, note("before"));
        let third = then("third");
        let lg = log.clone();
        let first = Then::Call(Box::new(move |w: &mut World, _| {
            lg.borrow_mut().push("first");
            w.dir_send(l.up(), Pkt::default(), 100, 1, third);
        }));
        w.dir_send(l.up(), Pkt::default(), 100, 1, first);
        w.dir_send(l.up(), Pkt::default(), 100, 1, then("second"));
        w.at(10 * MS, note("after"));
        w.run(SEC).unwrap();
        assert_eq!(log.take(), ["before", "first", "second", "after", "third"]);
        assert_eq!(w.events(), 5);
    }

    /// Not in Go: a drop-tail queue holds `buffer` at the rate, counted as queue drops.
    #[test]
    fn drop_tail_limit() {
        let p = Profile { rate: 8e6, burst: 3000, buffer: 10 * MS, ..Profile::default() };
        let (mut w, l) = test_link(p, LinkSpec::default());
        // 10 ms at 8 Mbps is 10,000 bytes: ten 1,000-byte packets fit, the rest are dropped.
        let log: Rc<RefCell<Vec<Dur>>> = Rc::default();
        for _ in 0..15 {
            send_logged(&mut w, l.up(), 1000, &log);
        }
        w.run(SEC).unwrap();
        let d = &w[l.up()];
        assert_eq!((log.take().len(), d.drops, d.qdrops), (10, 5, 5));
    }
}
