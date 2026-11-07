//! The session: its state, timers, control messages, the receive path and lane retirement
//! (§7.2, §7.5, §8.1; Go: core/session.go). Of the public methods, `new` is here with the struct
//! it fills and `snapshot` in status.rs; lib.rs has `handle`, `next` and `close`, as Go kept them
//! in api.go.
//!
//! Owner: C2. The `Session` struct is shared by every module: a new field goes through the
//! integrator (SKELETON.md).

use std::net::SocketAddr;

use crate::buf::{BufIx, Pool};
use crate::estimate::Meter;
use crate::flow::Flow;
use crate::keys::{Challenger, EpochKey, Receiver, Sender, Slot};
use crate::lane::{Arrival, LaneIx, Lanes, PENDING_COPIES};
use crate::wire::{Echo, Probe, Window};
use crate::{
    DATA_LEN, FlowId, HEADER_LEN, IDLE_AFTER, Link, LinkInfo, LogEvent, MAX_CANDS, MAX_ECHOES, MAX_LINKS, MAX_PACKET,
    MODE_BANDWIDTH, MODE_PERFORMANCE, MeshLane, Mono, Msg, Out, REORDER_SLOTS, REPLAY_WINDOW, Rand, Role, SMALL_BIT,
    STATUS_EVENTS, SockId, TYPE_DATA, TYPE_PROBE, packet_rid,
};

/// No time: a timer that is not set, a run that has not started. Go: `never`.
pub(crate) const NEVER: Mono = Mono::MAX;

/// A bond's mode (§14): the client sets it, both directions use it, and a new bond runs
/// performance. JSON names it "performance" or "bandwidth"; nothing else parses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Performance,
    Bandwidth,
}

impl Mode {
    /// "performance" or "bandwidth" (MODE_PERFORMANCE, MODE_BANDWIDTH).
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Performance => MODE_PERFORMANCE,
            Mode::Bandwidth => MODE_BANDWIDTH,
        }
    }

    /// The mode `s` names: exactly "performance" or "bandwidth".
    pub fn parse(s: &str) -> Option<Mode> {
        [Mode::Performance, Mode::Bandwidth].into_iter().find(|m| m.as_str() == s)
    }
}

/// One pairing of two processes, named by their two inst values (§7.2): its keys, lanes, flows
/// and scheduler. It never blocks, never allocates on the send path in steady state and never
/// reads a clock. Given the same events, times and random bytes, it calls [`Out`] identically.
pub struct Session {
    pub(crate) role: Role,
    /// node's key for this session in the RidTable.
    pub(crate) owner: u64,
    /// My receive id: the peer puts it on every packet it seals for me. 0 until the first
    /// `handle` allocates it (Go allocated in NewSession, which had the table).
    pub(crate) rid: u16,
    pub(crate) tx: Sender,
    pub(crate) rx: Receiver,
    /// Path-validation challenges (R2-03).
    pub(crate) chal: Challenger,
    pub(crate) mode: Mode,
    /// The time of the event being handled, and of the last one afterwards (status reads it).
    pub(crate) now: Mono,

    pub(crate) links: Vec<Link>,
    /// The Tailscale lane; None while the mesh is down.
    pub(crate) mesh: Option<MeshLane>,
    pub(crate) peer: Vec<LinkInfo>,
    /// Sorted by id, so iteration order is deterministic.
    pub(crate) lanes: Lanes,
    /// Flow slots, one per flow direction (§13), at most MAX_FLOWS; found through `tx_flows` (by
    /// key, flows I send) and `rx_flows` (by the peer's number, flows I receive).
    pub(crate) flows: Vec<Flow>,
    pub(crate) tx_flows: std::collections::BTreeMap<crate::flow::Key, usize>,
    pub(crate) rx_flows: std::collections::BTreeMap<FlowId, usize>,
    /// The number my next new flow tries first.
    pub(crate) next_flow: u16,
    /// The highest probe Seq sent on any lane; new and rebuilt lanes start there (R2-10).
    pub(crate) seq_top: u32,

    // Activity, cadence and punching (§8.1).
    pub(crate) active: bool,
    pub(crate) confirmed: bool,
    /// Idle, and the lanes other than the idle-probe lane are dormant.
    pub(crate) dozing: bool,
    pub(crate) last_data: Mono,
    /// The last activation or wake.
    pub(crate) since: Mono,
    /// The last wake; Mono::MIN before any. Echoes of probes sent before it carry no OWD sample
    /// (`echo`): a paused clock stepped every OWD (notes/rust.md, harden: integration).
    pub(crate) woke: Mono,
    /// The newest fresh echo on any lane.
    pub(crate) last_echo: Mono,
    /// The client's idle-probe lane.
    pub(crate) idle: Option<LaneIx>,
    pub(crate) punch_at: Mono,
    pub(crate) punch_until: Mono,
    pub(crate) next_punch: Mono,
    pub(crate) punch_pending: bool,
    /// SentAt of the peer probe whose lagging bits apply, µs.
    pub(crate) lag_as_of: i64,

    /// Small-bit first arrivals that other lanes owe (§8.4): PENDING_COPIES slots, by
    /// `arr_n % PENDING_COPIES`.
    pub(crate) arr: Box<[Arrival]>,
    pub(crate) arr_n: u64,
    /// First arrivals for rescues (§11.5): RESCUE_RING entries, by counter.
    pub(crate) firsts: Box<[crate::stats::First]>,
    /// The counter, flow, seq, length and type of each data packet opened, by counter %
    /// REPLAY_WINDOW: a later copy, which is not opened (§10.4), is counted only if it matches,
    /// and records which seq its lane brought (§15).
    pub(crate) opened: Box<[Opened]>,

    /// Where my bulk goes (§11), `spread[..nspread]`, fastest (lowest base OWD) first.
    pub(crate) spread: [LaneIx; MAX_LINKS + 1],
    pub(crate) nspread: usize,
    /// Bits per second of my small flows.
    pub(crate) small_rate: f64,

    pub(crate) pool: Pool,
    /// The opened packet: `packet` decrypts into it, `data` delivers from it. MAX_PACKET bytes.
    pub(crate) rxbuf: Box<[u8]>,
    /// The probe being received; its echoes are `ein[..pin.n]`.
    pub(crate) pin: Probe,
    pub(crate) ein: [Echo; MAX_ECHOES],
    /// The echoes of the probe being sent.
    pub(crate) eout: [Echo; MAX_ECHOES],
    /// The earliest time `handle` wants a Tick.
    pub(crate) next: Mono,

    pub(crate) rewinds: u64,
    /// Until when a rewind of mine is in flight: my probes carry PROBE_REWIND (§15).
    pub(crate) rewind_until: Mono,
    /// When a probe last said a rewind of the peer's is in flight (PROBE_REWIND).
    pub(crate) peer_rewind: Mono,
    pub(crate) unrecovered: u64,
    pub(crate) flows_dropped: u64,
    pub(crate) last_auth: Option<Mono>,
    pub(crate) metered_bytes: Meter,
    /// The last STATUS_EVENTS events: entry `nevents % STATUS_EVENTS` is the next to write.
    pub(crate) events: [LogEvent; STATUS_EVENTS],
    pub(crate) nevents: usize,
}

impl Session {
    /// Starts the session with one peer process. `owner` is node's key for this session in
    /// `rids`. Draws the session's send key, then its path-validation key (R2-03), 32 bytes each,
    /// and never uses `rand` again (Go kept the reader and never read it again either); its own
    /// copies of both are zeroed on return. Go: `newSession`.
    pub fn new(role: Role, owner: u64, mut rand: Rand) -> Session {
        let (mut k, mut ck) = (zeroize::Zeroizing::new([0u8; 32]), zeroize::Zeroizing::new([0u8; 32]));
        rand(&mut *k);
        rand(&mut *ck);
        Session {
            role,
            owner,
            rid: 0,
            tx: Sender { key: EpochKey::new(0, *k), ctr: 0, rid: 0 },
            rx: Receiver::default(),
            chal: Challenger::new(*ck),
            mode: Mode::Performance,
            now: 0,
            links: Vec::new(),
            mesh: None,
            peer: Vec::new(),
            lanes: Lanes::default(),
            flows: Vec::new(),
            tx_flows: Default::default(),
            rx_flows: Default::default(),
            next_flow: 1,
            seq_top: 0,
            active: false,
            confirmed: false,
            dozing: false,
            last_data: 0,
            since: 0,
            woke: Mono::MIN,
            last_echo: 0,
            idle: None,
            punch_at: Mono::MIN / 2,
            punch_until: 0,
            next_punch: 0,
            punch_pending: false,
            lag_as_of: i64::MIN,
            arr: vec![Arrival::default(); PENDING_COPIES].into_boxed_slice(),
            arr_n: 0,
            firsts: vec![crate::stats::First::default(); crate::stats::RESCUE_RING].into_boxed_slice(),
            opened: vec![Opened::default(); REPLAY_WINDOW as usize].into_boxed_slice(),
            spread: [LaneIx::default(); MAX_LINKS + 1],
            nspread: 0,
            small_rate: 0.0,
            pool: Pool::default(),
            rxbuf: vec![0; MAX_PACKET].into_boxed_slice(),
            pin: Probe::default(),
            ein: [Echo::default(); MAX_ECHOES],
            eout: [Echo::default(); MAX_ECHOES],
            next: NEVER,
            rewinds: 0,
            rewind_until: Mono::MIN,
            peer_rewind: Mono::MIN,
            unrecovered: 0,
            flows_dropped: 0,
            last_auth: None,
            metered_bytes: Meter::default(),
            events: Default::default(),
            nevents: 0,
        }
    }

    /// The spread, fastest first.
    pub(crate) fn spread(&self) -> &[LaneIx] {
        &self.spread[..self.nspread]
    }

    /// Runs every timer: idle, punch rounds, classification, reorder timers, each lane's probes
    /// and liveness, the scheduler, the copy set, the pacers and rewind rings.
    pub(crate) fn tick(&mut self, out: &mut dyn Out, now: Mono) {
        self.next = NEVER;
        self.active &= now.saturating_sub(self.last_data) < IDLE_AFTER;
        if self.active {
            self.wake_at(self.last_data.saturating_add(IDLE_AFTER));
        }
        if self.punch_pending {
            self.punch(now);
        }
        if !self.active && !self.dozing && now >= self.punch_until {
            self.go_idle(now);
        }
        if now < self.punch_until {
            if now >= self.next_punch {
                self.punch_probes(out, now);
            }
            self.wake_at(self.next_punch.min(self.punch_until));
        }
        self.expire(now);
        self.classify(now);
        for f in 0..self.flows.len() {
            if self.flows[f].used && self.flows[f].n > 0 {
                self.release(out, now, f);
            }
        }
        for l in self.lanes.ixs() {
            self.lane_tick(out, now, l);
        }
        self.schedule(out, now);
        self.set_copies(out, now);
        for l in self.lanes.ixs() {
            self.drain(out, now, l);
            self.evict(now, l);
        }
    }

    /// Asks for a Tick at `t` at the latest.
    pub(crate) fn wake_at(&mut self, t: Mono) {
        self.next = self.next.min(t);
    }

    /// Consumes one control message (§7.1): hello (sends my key, then as links), links, key, mode.
    /// The peer's links are cut to MAX_LINKS with MAX_CANDS each.
    pub(crate) fn control(&mut self, out: &mut dyn Out, now: Mono, m: &Msg) {
        match m.t.as_str() {
            "hello" | "links" => {
                // A hello, first or after a reconnect, gets my key and counts as a control-channel
                // connect for the punch cadence; then it is a links message.
                if m.t == "hello" {
                    if self.role == Role::Anchor {
                        self.set_mode(&m.mode);
                    }
                    self.send_key(out);
                }
                self.peer = m.links[..m.links.len().min(MAX_LINKS)].to_vec();
                self.peer.iter_mut().for_each(|l| l.cands.truncate(MAX_CANDS));
                self.set_links(out, now);
                self.punch(now);
            }
            "key" => {
                // From the first or a fresh key on, counters below m.c are an earlier session's
                // (R2-09): open drops them before any work.
                if self.rx.install_from(m.e, &m.k, m.c) {
                    self.peer_new(out);
                }
                if m.rid != 0 {
                    self.tx.rid = m.rid;
                }
            }
            "mode" => self.set_mode(&m.mode),
            _ => {}
        }
    }

    /// Forgets what the peer's previous session numbered: under its new key, counters, probe Seq
    /// and flow seq start again (§7.2). Held datagrams are delivered first.
    pub(crate) fn peer_new(&mut self, out: &mut dyn Out) {
        for l in self.lanes.iter_mut() {
            l.win = Window::default();
            (l.rx_seq, l.rx_last, l.last_small, l.expect, l.lagging) = (0, 0, 0, self.arr_n, false);
        }
        for f in 0..self.flows.len() {
            if !self.flows[f].used {
                continue;
            }
            // Held seqs lie within REORDER_SLOTS of next, so this many steps deliver them all; the
            // bound only keeps a broken count from spinning (Go looped while f.n > 0).
            for _ in 0..REORDER_SLOTS {
                if self.flows[f].n == 0 {
                    break;
                }
                self.step(out, f);
            }
            // The old session's gap given up teaches nothing about the new one's (R2-13, C5).
            let fl = &mut self.flows[f];
            (fl.next, fl.fill, fl.gap_n, fl.lanes) = (1, 0, 0, Default::default());
        }
    }

    /// Applies the bond's mode (§14); the new mode's scheduler starts from no lane. Any name but
    /// "performance" and "bandwidth", and the mode already running, change nothing. Bandwidth
    /// paces each lane at its own rate (§12, §14.2), so a lane still unlimited from performance
    /// starts again from PACE_FLOOR: earliest arrival by an unlimited pace overfilled the path's
    /// buffer, and the lane turned suspect again and again before its first cut.
    pub(crate) fn set_mode(&mut self, m: &str) {
        if let Some(m) = Mode::parse(m).filter(|&x| x != self.mode) {
            (self.mode, self.nspread) = (m, 0);
            for l in self.lanes.ixs() {
                if m == Mode::Bandwidth && !self.lanes[l].rate.is_finite() {
                    self.restart(0, l);
                }
            }
        }
    }

    /// Sends my receive id and current send key, the only way a key leaves memory (§7.2), with my
    /// next counter (R2-09).
    pub(crate) fn send_key(&mut self, out: &mut dyn Out) {
        let k = &self.tx.key;
        out.control(Msg { t: "key".into(), rid: self.rid, e: k.e, k: k.k.to_vec(), c: self.tx.ctr, ..Msg::default() });
    }

    /// Seals the payload in pool buffer `buf` (`n` bytes after the header) under the next counter
    /// and returns the packet's length; entering a new epoch sends key (§7.2).
    pub(crate) fn seal(&mut self, out: &mut dyn Out, buf: BufIx, typ: u8, n: usize) -> usize {
        let (len, epoch) = self.tx.seal(self.pool.buf_mut(buf), typ, n);
        if epoch {
            self.send_key(out);
        }
        len
    }

    /// Receives one lane packet in the order of §10.4: size and rid, a later copy of data
    /// counted for its lane without opening it, open into `rxbuf`, the lane's window and
    /// statistics, the session window, then probe or data.
    pub(crate) fn packet(&mut self, out: &mut dyn Out, now: Mono, sock: SockId, from: SocketAddr, b: &[u8]) {
        // core never sends a larger packet, and opening one would cost work before the tag fails.
        if packet_rid(b) != Some(self.rid) || b.len() > MAX_PACKET {
            return;
        }
        let (typ, probe) = (b[0], b[0] == TYPE_PROBE);
        if !probe && typ & !SMALL_BIT != TYPE_DATA {
            return;
        }
        let c = u64::from_be_bytes(b[3..HEADER_LEN].try_into().unwrap_or_default());
        if !probe && self.rx.key(c).is_some() && self.rx.win.seen(c) {
            // §10.4 step 2: a later copy is not opened; the session window, read only, has its
            // counter. It counts for its lane's arrival record alone (`later`).
            if let Some(l) = self.lane_at(sock, from) {
                self.later(now, l, c, typ, b.len());
            }
            return;
        }
        let Some((n, slot)) = self.rx.open(&mut self.rxbuf, b) else { return };
        let l = if probe {
            if !self.pin.decode(&self.rxbuf[..n], &mut self.ein) {
                return;
            }
            self.lane_by_id(self.pin.lane).filter(|&l| self.lanes[l].sock == sock)
        } else if n < DATA_LEN {
            return;
        } else {
            self.lane_at(sock, from)
        };
        let (id, seq) = (FlowId(u16::from_be_bytes([self.rxbuf[0], self.rxbuf[1]])), be32(&self.rxbuf[2..]));
        if !probe {
            self.opened[(c % REPLAY_WINDOW) as usize] = Opened { c, id, seq, len: b.len() as u16, typ };
        }
        if let Some(l) = l {
            // Step 3: the lane's own window, then its statistics.
            let x = &mut self.lanes[l];
            if !x.win.check(c) {
                return;
            }
            (x.rx_pkts, x.rx_bytes) = (x.rx_pkts.wrapping_add(1), x.rx_bytes.wrapping_add(b.len() as u32));
            if !probe {
                x.rx_last = c;
            }
            if x.cand == Some(from) || !x.validated && x.peer == Some(from) {
                x.amp_in = x.amp_in.saturating_add(b.len());
            }
            // A later copy too (`later`): its lane is FIFO, which §15's lane rule relies on.
            if typ == TYPE_DATA
                && let Some(f) = self.flow(id, false)
            {
                self.flows[f].carried(l, now, seq);
            }
        }
        let first = self.rx.win.check(c); // step 4
        if let Some(l) = l {
            self.copied(now, l, c, typ & SMALL_BIT != 0, first);
            if !probe {
                self.arrived(now, l, c, first, b.len());
            }
        }
        if !first {
            return;
        }
        if slot == Slot::Next {
            self.rx.advance();
        }
        self.last_auth = Some(now);
        if probe { self.on_probe(out, now, l, from, b.len()) } else { self.data(out, now, l, typ, n) }
    }

    /// A later copy of data with counter `c`, type `typ`, `size` bytes, on `l`, which the session
    /// window already has and which is not opened (§10.4). Unless the opened copy of `c` had its
    /// type and length (every copy is the same bytes), it is dropped with no state; else it counts
    /// for its lane
    /// alone: the lane's window, the counts its echo reports (§11.2), its lagging mark and
    /// statistics (§11.4, §11.5), and for bulk the seq it brought (§15). Nothing it carries is
    /// read, delivered or answered: it records what a replay of the real copy on `l` would.
    fn later(&mut self, now: Mono, l: LaneIx, c: u64, typ: u8, size: usize) {
        let o = self.opened[(c % REPLAY_WINDOW) as usize];
        if o.c != c || o.typ != typ || usize::from(o.len) != size {
            return;
        }
        let x = &mut self.lanes[l];
        if !x.win.check(c) {
            return;
        }
        (x.rx_pkts, x.rx_bytes, x.rx_last) = (x.rx_pkts.wrapping_add(1), x.rx_bytes.wrapping_add(size as u32), c);
        if typ == TYPE_DATA
            && let Some(f) = self.flow(o.id, false)
        {
            self.flows[f].carried(l, now, o.seq);
        }
        self.copied(now, l, c, typ & SMALL_BIT != 0, false);
        self.arrived(now, l, c, false, size);
    }

    /// Forgets a lane whose link went away (it is in `lanes.gone`); bulk it carried is rewound
    /// onto another lane of the spread, else the best that answers, taken first (R2-04), which
    /// takes over at once (§10.2). In performance the bulk is dropped while another lane of the
    /// spread is left: it carries the same packets.
    pub(crate) fn retire(&mut self, out: &mut dyn Out, now: Mono, l: LaneIx) {
        let (mut to, mut n) = (self.spread, 0);
        if let Some(i) = self.spread().iter().position(|&x| x == l) {
            self.spread.copy_within(i + 1..self.nspread, i);
            self.nspread -= 1;
            to = self.spread;
            if self.nspread == 0 || self.mode == Mode::Bandwidth {
                n = self.nspread;
                if let Some(t) = self.two().0.filter(|_| n == 0) {
                    self.take(now, t);
                    (to[0], n) = (t, 1);
                }
                self.rewinds += 1;
            }
        }
        self.rewind(out, now, l, &to[..n]);
        self.idle.take_if(|x| *x == l);
    }

    /// Adds an event to the session's last STATUS_EVENTS (§14.2).
    pub(crate) fn log(&mut self, now: Mono, msg: String) {
        self.events[self.nevents % STATUS_EVENTS] = LogEvent { at: now, msg };
        self.nevents += 1;
    }
}

/// An opened data packet, for its later copies (`Session::opened`): REPLAY_WINDOW of them, so
/// every counter the session window still holds has its entry.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Opened {
    c: u64,
    id: FlowId,
    seq: u32,
    len: u16,
    typ: u8,
}

/// The big-endian u32 at the front of `b`: a data payload's seq.
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests;
