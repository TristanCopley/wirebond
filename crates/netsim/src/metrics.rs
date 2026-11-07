//! What the acceptance criteria measure (Go netsim/metrics_test.go, which lived in a test file
//! because only tests use it; the Rust integration tests need it from the library): the Recorder
//! that samples lane roles, bulk bytes and queues; quantiles; frame, gap and lateness metrics; and
//! A1b's input metric at the wirebond layer (notes/netsim.md 24). Owner: N3.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use wirebond_core::{Event, HEADER_LEN, SMALL_BIT, TYPE_DATA};

use crate::apps::{Frame, MoonId, Moonlight, Sample};
use crate::check::header;
use crate::env::is_bulk;
use crate::link::{DirId, LinkId};
use crate::node::DevId;
use crate::sim::{Dur, HOUR, HookArgs, HookKind, MS, World};

/// The Recorder's period for lane roles and bulk bytes.
pub const WINDOW: Dur = 100 * MS;

/// The Recorder's index for the Tailscale lane; client links are 0…7 by id − 1 (Go mesh).
pub const MESH_LANE: i8 = 8;

/// Lanes the Recorder counts: eight client links and the Tailscale lane.
pub const LANES: usize = 9;

/// The period of the queue samples.
const QUEUE_EVERY: Dur = 10 * MS;

/// Samples what the acceptance criteria need (§17.3): per WINDOW, the bulk bytes the server sent
/// on each lane and the lane its Snapshot calls primary or trial; every 10 ms, the queue of each
/// watched direction. Owned by the world (`world.rec`), started by [`record`].
#[derive(Clone, Debug, Default)]
pub struct Recorder {
    /// Bulk bytes (TYPE_DATA without the small bit) per window and lane.
    pub bytes: Vec<[i64; LANES]>,
    /// The server's primary per window, from Snapshot; −1: none reported.
    pub roles: Vec<i8>,
    /// The server's trial lane per window; −1: none.
    pub trials: Vec<i8>,
    /// Queue samples every 10 ms per watched direction (Go keyed them by *Dir).
    pub queues: BTreeMap<DirId, Vec<Dur>>,
    pub(crate) cur: [i64; LANES],
    pub(crate) dirs: Vec<DirId>,
}

/// Starts a Recorder on w (`w.rec`): a hook counting the server's bulk bytes entering client
/// links per lane, and a window every WINDOW (Go record).
pub fn record(w: &mut World) {
    w.rec = Some(Recorder::default());
    w.hook(|w, a| {
        if a.kind == HookKind::Inbound && is_bulk(a.data) {
            let i = a.link.map_or(MESH_LANE as usize, |l| usize::from(w[l].id) - 1);
            if let Some(r) = w.rec.as_mut() {
                r.cur[i] += a.data.len() as i64;
            }
        }
    });
    w.every(WINDOW, WINDOW, |w| {
        w.rec_window();
        true
    });
}

impl World {
    /// Samples d's queue every 10 ms from now, zero-filled before (Go Recorder.Watch).
    pub fn watch(&mut self, d: DirId) {
        let now = self.now;
        let r = self.rec.as_mut().expect("watch needs the Recorder: metrics::record starts it");
        let first = r.dirs.is_empty();
        r.dirs.push(d);
        r.queues.insert(d, vec![0; (now / QUEUE_EVERY).max(0) as usize]);
        if first {
            self.every(now, QUEUE_EVERY, |w| {
                let World { rec, links, .. } = w;
                if let Some(r) = rec {
                    for d in &r.dirs {
                        let q = links[d.link.0].dir(d.side).queue();
                        r.queues.entry(*d).or_default().push(q);
                    }
                }
                true
            });
        }
    }

    /// Closes one window: its bytes, and the server's primary and trial lanes (Go window).
    pub(crate) fn rec_window(&mut self) {
        let lanes = self[DevId::Server].snap().lanes;
        let Some(r) = self.rec.as_mut() else {
            return;
        };
        r.bytes.push(r.cur);
        r.cur = [0; LANES];
        let (mut prim, mut trial) = (-1, -1);
        for l in &lanes {
            let i = match l.id {
                0 => MESH_LANE,
                id => ((id >> 8) as i8).wrapping_sub(1),
            };
            match l.out.role.as_str() {
                "primary" => prim = i,
                "trial" => trial = i,
                _ => {}
            }
        }
        r.roles.push(prim);
        r.trials.push(trial);
    }
}

/// A window's bytes on lane; 0 for a lane the Recorder does not count.
fn lane_bytes(b: &[i64; LANES], lane: i8) -> i64 {
    usize::try_from(lane).ok().and_then(|i| b.get(i)).copied().unwrap_or(0)
}

/// The first lane with the most bytes.
fn first_max(b: &[i64; LANES]) -> usize {
    (1..LANES).fold(0, |best, i| if b[i] > b[best] { i } else { best })
}

/// Primary changes among windows [a, b) of p, and of those the returns to a lane with a shorter
/// delay than the one bulk leaves (Moves' count, with the delays given).
fn rec_moves(p: &[i8], a: usize, b: usize, delay: impl Fn(i8) -> Dur) -> (usize, usize) {
    let (mut moves, mut returns) = (0, 0);
    for i in a.max(1)..b {
        if p[i - 1] >= 0 && p[i] != p[i - 1] {
            moves += 1;
            if delay(p[i]) < delay(p[i - 1]) {
                returns += 1;
            }
        }
    }
    (moves, returns)
}

impl Recorder {
    /// The lane carrying the server's bulk in each window: the one its Snapshot calls primary,
    /// else the one that carried the most bulk bytes, staying with the previous one while it
    /// carried at least 80% of the most (a trial or rewind duplicates bulk). −1: no bulk yet.
    pub fn primary(&self) -> Vec<i8> {
        let mut out = Vec::with_capacity(self.bytes.len());
        let mut prev = -1;
        for (b, &role) in self.bytes.iter().zip(&self.roles) {
            let mut p = role;
            if p < 0 {
                p = prev;
                let best = first_max(b);
                if b[best] > 0 && (prev < 0 || lane_bytes(b, prev) < b[best] * 8 / 10) {
                    p = best as i8;
                }
            }
            out.push(p);
            prev = p;
        }
        out
    }

    /// The fraction of windows in [from, to) in which lane carried bulk bytes (performance sends
    /// every packet on every lane of its spread, so no lane is primary).
    pub fn carried(&self, lane: i8, from: Dur, to: Dur) -> f64 {
        let (a, b) = span(from, to, self.bytes.len());
        let n = self.bytes[a..b].iter().filter(|w| lane_bytes(w, lane) > 0).count();
        n as f64 / (b - a).max(1) as f64
    }

    /// The fraction of windows in [from, to) whose primary is lane.
    pub fn share(&self, lane: i8, from: Dur, to: Dur) -> f64 {
        let p = self.primary();
        let (a, b) = span(from, to, p.len());
        let n = p[a..b].iter().filter(|&&x| x == lane).count();
        n as f64 / (b - a).max(1) as f64
    }

    /// The start of the first window at or after from whose primary is lane; None: never.
    pub fn first_primary(&self, lane: i8, from: Dur) -> Option<Dur> {
        let p = self.primary();
        let a = (from / WINDOW).max(0) as usize;
        (a..p.len()).find(|&i| p[i] == lane).map(|i| i as Dur * WINDOW)
    }

    /// Primary changes in [from, to), and of those the returns to a lane whose link has a shorter
    /// propagation delay than the one bulk leaves (Go Moves; w gives the client links' delays).
    pub fn moves(&self, w: &World, from: Dur, to: Dur) -> (usize, usize) {
        let p = self.primary();
        let (a, b) = span(from, to, p.len());
        rec_moves(&p, a, b, |lane| self.delay(w, lane))
    }

    /// A lane's down propagation delay; an hour for an unknown lane (Go delay).
    pub(crate) fn delay(&self, w: &World, lane: i8) -> Dur {
        let link = if lane == MESH_LANE {
            w.default_link(DevId::Client)
        } else {
            let ls = &w[DevId::Client].links;
            usize::try_from(lane).ok().and_then(|i| ls.get(i)).copied()
        };
        link.map_or(HOUR, |l| w[l].down.p.delay)
    }

    /// The runs of windows in [from, to) in which lane carried bulk while it was not primary:
    /// trials, reported by Snapshot or seen as at least 20% of the primary's bulk bytes; the
    /// window in which bulk moved off lane is not one (Go TrialRuns, notes/netsim.md 23).
    pub fn trial_runs(&self, lane: i8, from: Dur, to: Dur) -> usize {
        let p = self.primary();
        let (a, b) = span(from, to, p.len());
        let (mut runs, mut inside) = (0, false);
        for i in a..b {
            let mut on = self.trials[i] == lane;
            if self.trials[i] < 0 && p[i] >= 0 && p[i] != lane && (i == 0 || p[i - 1] != lane) {
                let x = lane_bytes(&self.bytes[i], lane);
                on = x > 0 && x >= lane_bytes(&self.bytes[i], p[i]) / 5;
            }
            if on && !inside {
                runs += 1;
            }
            inside = on;
        }
        runs
    }

    /// The 95th percentile over time of d's queue in [from, to) (Go QueueP95).
    pub fn queue_p95(&self, d: DirId, from: Dur, to: Dur) -> Dur {
        let q = self.queues.get(&d).map_or(&[][..], Vec::as_slice);
        let at = |t: Dur| ((t / QUEUE_EVERY).max(0) as usize).min(q.len());
        let b = at(to);
        pct(&q[at(from).min(b)..b], 0.95)
    }
}

/// The window indices [a, b) of [from, to), clamped to n windows (Go span). Times before 0 count
/// as 0, and a ≤ b always (Go's slices panicked when from was past to).
pub fn span(from: Dur, to: Dur, n: usize) -> (usize, usize) {
    let at = |t: Dur| ((t / WINDOW).max(0) as usize).min(n);
    let b = at(to);
    (at(from).min(b), b)
}

/// Evaluates A1b's input metric over [from, to) after the run ([`input_copies`]).
pub type InputEval = Box<dyn Fn(&World, Dur, Dur) -> Vec<Dur>>;

/// The p-th quantile of xs (nearest rank); `Dur::MAX` (+Inf) when xs is empty (Go Pct).
pub fn pct(xs: &[Dur], p: f64) -> Dur {
    if xs.is_empty() {
        return Dur::MAX;
    }
    let mut s = xs.to_vec();
    let k = ((p * s.len() as f64).ceil() as usize).saturating_sub(1);
    *s.select_nth_unstable(k.min(xs.len() - 1)).1
}

/// A1b's input metric at the wirebond layer (notes/netsim.md 24): for each input datagram, each
/// ENet try, the time from its send to wirebond delivering its first copy, beyond the lowest
/// ground-truth OWD at that send among the client links, leaving out each link on which the link
/// model lost the copy wirebond sent. A datagram wirebond never sent, or never delivered while a
/// copy reached the server, is `Dur::MAX`; one whose every copy the links lost is not counted.
/// Installs a hook now; the returned function evaluates [from, to) after the run (Go inputCopies,
/// which took the env only to reach its world).
pub fn input_copies(w: &mut World, m: MoonId) -> InputEval {
    let st = Rc::new(RefCell::new(InputCopies::default()));
    let hooked = Rc::clone(&st);
    w.hook(move |w, a| input_hook(w, a, m, &hooked));
    Box::new(move |w: &World, from: Dur, to: Dur| st.borrow().eval(&w[m], from, to))
}

/// What input_copies remembers of the copies of one input try (Go's rec).
#[derive(Clone, Debug, Default, PartialEq)]
struct InputRec {
    sent: Dur,
    got: Dur,
    /// Each client link's OWD toward the server at the first copy's send.
    owd: Vec<Dur>,
    /// Bits of the client links that sent a copy, and of those whose copy reached the server;
    /// the Tailscale lane counts as the default link.
    sent_on: u16,
    delivered: u16,
}

/// input_copies' state, shared by its hook and its evaluation: one record per sealed input try,
/// by counter (every copy of a small packet carries the same one) and by (ENet seq, try).
#[derive(Debug, Default)]
struct InputCopies {
    recs: Vec<InputRec>,
    by_ctr: BTreeMap<u64, usize>,
    by_try: BTreeMap<(u32, u32), usize>,
}

impl InputCopies {
    /// The client sent a copy of input try t sealed under ctr on the link with bit; owd is used
    /// for its first copy.
    fn sent(&mut self, ctr: u64, t: (u32, u32), now: Dur, owd: Vec<Dur>, bit: u16) {
        let i = match self.by_ctr.get(&ctr) {
            Some(&i) => i,
            None => {
                let i = self.recs.len();
                self.recs.push(InputRec { sent: now, owd, ..InputRec::default() });
                self.by_ctr.insert(ctr, i);
                self.by_try.insert(t, i);
                i
            }
        };
        self.recs[i].sent_on |= bit;
    }

    /// The link model delivered to the server a packet sealed under ctr, sent on the link with bit.
    fn received(&mut self, ctr: u64, bit: u16) {
        if let Some(&i) = self.by_ctr.get(&ctr) {
            self.recs[i].delivered |= bit;
        }
    }

    /// wirebond delivered input try t at the server.
    fn delivered(&mut self, t: (u32, u32), now: Dur) {
        if let Some(&i) = self.by_try.get(&t)
            && self.recs[i].got == 0
        {
            self.recs[i].got = now;
        }
    }

    /// The metric over the inputs of m sent in [from, to).
    fn eval(&self, m: &Moonlight, from: Dur, to: Dur) -> Vec<Dur> {
        let mut xs = Vec::new();
        for (i, input) in m.inputs.iter().enumerate() {
            if input.sent < from || input.sent >= to {
                continue;
            }
            for t in 1..=m.enet[i].tries {
                match self.by_try.get(&(i as u32, t)).map(|&r| &self.recs[r]) {
                    None => xs.push(Dur::MAX),
                    Some(r) if r.got == 0 && r.delivered != 0 => xs.push(Dur::MAX),
                    Some(r) if r.got > 0 => {
                        let lost = r.sent_on & !r.delivered;
                        let kept = |j: usize| lost.checked_shr(j as u32).unwrap_or(0) & 1 == 0;
                        let owd = r.owd.iter().enumerate().filter(|&(j, _)| kept(j));
                        let reference = owd.map(|(_, &o)| o).min().unwrap_or(Dur::MAX);
                        xs.push(r.got - r.sent - reference);
                    }
                    Some(_) => {} // every copy lost by the links: theirs, not counted
                }
            }
        }
        xs
    }
}

/// input_copies' hook: the client's input copies (S), what reached the server (R) and what
/// wirebond delivered there (D). It holds no borrow of the state across a call into the world.
fn input_hook(w: &mut World, a: &HookArgs<'_>, m: MoonId, st: &RefCell<InputCopies>) {
    match a.kind {
        HookKind::Sent if a.dev == DevId::Client => {
            let Some(app) = a.event.and_then(|e| match e {
                Event::Send { packet } => crate::sim::outer(packet).map(|i| i.data),
                _ => None,
            }) else {
                return;
            };
            if a.data.len() < HEADER_LEN || a.data[0] != TYPE_DATA | SMALL_BIT {
                return;
            }
            let Some((_, s, h)) = w.registry().find(app) else {
                return;
            };
            if s != w[m].input {
                return;
            }
            let (_, ctr) = header(a.data);
            let known = st.borrow().by_ctr.contains_key(&ctr);
            let owd = if known { Vec::new() } else { w.owds(true) };
            let bit = input_bit(w, a.link);
            st.borrow_mut().sent(ctr, (h.seq, h.aux), w.now, owd, bit);
        }
        HookKind::Received if a.dev == DevId::Server && a.data.len() >= HEADER_LEN => {
            let (_, ctr) = header(a.data);
            let known = st.borrow().by_ctr.contains_key(&ctr);
            if known {
                let bit = input_bit(w, a.link);
                st.borrow_mut().received(ctr, bit);
            }
        }
        HookKind::Delivered if a.dev == DevId::Server => {
            if let Some((_, s, h)) = w.registry().find(a.data)
                && s == w[m].input
            {
                st.borrow_mut().delivered((h.seq, h.aux), w.now);
            }
        }
        _ => {}
    }
}

/// The bit of client link l in input_copies' masks; the Tailscale lane (None) counts as the
/// default link.
fn input_bit(w: &World, l: Option<LinkId>) -> u16 {
    let l = l.or_else(|| w.default_link(DevId::Client));
    let shift = l.and_then(|l| u32::from(w[l].id).checked_sub(1));
    shift.and_then(|s| 1u16.checked_shl(s)).unwrap_or(0)
}

impl Moonlight {
    /// Frames purged in [from, to) (Go Unrecovered).
    pub fn unrecovered(&self, from: Dur, to: Dur) -> usize {
        self.lost_at.iter().filter(|&&t| t >= from && t < to).count()
    }

    /// For each frame sent in [from, to): its frame delay minus its frame time on client link i
    /// alone (§17.3), so "X OWD + d" holds at p99 when pct(…, 0.99) ≤ d; never decoded is
    /// `Dur::MAX` (Go Excess).
    pub fn excess(&self, i: usize, from: Dur, to: Dur) -> Vec<Dur> {
        let sent = self.frames.iter().filter(|f| f.sent >= from && f.sent < to);
        let excess = |f: &Frame| match f.done {
            0 => Dur::MAX,
            done => done - f.ideal[i],
        };
        sent.map(excess).collect()
    }

    /// The longest time in [from, to) without a displayed frame (Go MaxGap).
    pub fn max_gap(&self, from: Dur, to: Dur) -> Dur {
        let (mut prev, mut gap) = (from, 0);
        for &t in self.shown.iter().filter(|&&t| t >= from && t < to) {
            gap = gap.max(t - prev);
            prev = t;
        }
        gap.max(to - prev)
    }

    /// The longest gap between two video datagrams delivered in [from, to) (Go BulkGap).
    pub fn bulk_gap(&self, from: Dur, to: Dur) -> Dur {
        let gaps = self.video_gaps.iter().filter(|x| x[0] >= from && x[0] - x[1] < to);
        gaps.fold(0, |g, x| g.max(x[1]))
    }
}

/// For each sample sent in [from, to): its delay beyond the reference, the lowest client link's
/// OWD at the moment it was sent (r None) or link r's; never delivered is `Dur::MAX` (Go Late,
/// where ref < 0 meant None).
pub fn late(ss: &[Sample], r: Option<usize>, from: Dur, to: Dur) -> Vec<Dur> {
    let sent = ss.iter().filter(|s| s.sent >= from && s.sent < to);
    let late = |s: &Sample| {
        if s.got == 0 {
            return Dur::MAX;
        }
        let o = match r {
            Some(i) => s.owd[i],
            None => s.owd.iter().copied().min().unwrap_or(Dur::MAX),
        };
        s.got - s.sent - o
    };
    sent.map(late).collect()
}

#[cfg(test)]
mod tests {
    use wirebond_core::{LaneOut, LaneStatus, Role};

    use super::*;
    use crate::apps::EnetState;
    use crate::fake::{Evil, Fake, fake_maker};
    use crate::sim::{Rng, SEC, key_reader};

    fn world() -> World {
        World::new(1, fake_maker(false, Evil::None))
    }

    #[test]
    fn pct_is_the_nearest_rank() {
        assert_eq!(pct(&[], 0.5), Dur::MAX);
        assert_eq!((pct(&[7], 0.0), pct(&[7], 1.0)), (7, 7));
        let xs: Vec<Dur> = (1..=100).rev().collect();
        let ps = [0.0, 0.5, 0.95, 0.99, 0.999, 1.0].map(|p| pct(&xs, p));
        assert_eq!(ps, [1, 50, 95, 99, 100, 100]);
        assert_eq!(xs[0], 100, "the samples are left as they were");
        assert_eq!(pct(&[3, Dur::MAX, 1, 2], 0.75), 3);
        assert_eq!(pct(&[3, Dur::MAX, 1, 2], 0.76), Dur::MAX);
    }

    #[test]
    fn span_clamps_to_the_windows() {
        assert_eq!(span(0, SEC, 100), (0, 10));
        assert_eq!(span(150 * MS, 1_250 * MS, 100), (1, 12));
        assert_eq!(span(5 * SEC, 20 * SEC, 100), (50, 100));
        assert_eq!(span(30 * SEC, 40 * SEC, 100), (100, 100));
        assert_eq!(span(-SEC, 300 * MS, 100), (0, 3));
        assert_eq!(span(3 * SEC, SEC, 100), (10, 10));
    }

    #[test]
    fn late_is_beyond_the_lowest_or_a_given_link() {
        let s = |sent, got, owd: [Dur; 2]| Sample { sent, got, owd: owd.to_vec() };
        let ss = [
            s(MS, 40 * MS, [30 * MS, 35 * MS]),
            s(2 * MS, 0, [30 * MS, 35 * MS]),
            s(3 * MS, 50 * MS, [45 * MS, 40 * MS]),
            s(10 * MS, 60 * MS, [1, 1]),
        ];
        assert_eq!(late(&ss, None, 0, 10 * MS), [9 * MS, Dur::MAX, 7 * MS]);
        assert_eq!(late(&ss, Some(0), MS, 4 * MS), [9 * MS, Dur::MAX, 2 * MS]);
        assert_eq!(late(&ss, Some(1), MS, 4 * MS), [4 * MS, Dur::MAX, 7 * MS]);
        assert_eq!(late(&ss, None, 10 * MS, 20 * MS), [50 * MS - 1]);
        assert!(late(&ss, None, 20 * MS, SEC).is_empty());
    }

    #[test]
    fn frame_metrics() {
        let fr = |sent, done, ideal: [Dur; 2]| Frame { sent, done, ideal: ideal.to_vec(), ..Frame::default() };
        let m = Moonlight {
            frames: vec![
                fr(SEC, SEC + 40 * MS, [SEC + 32 * MS, SEC + 50 * MS]),
                fr(SEC + 8 * MS, 0, [SEC + 40 * MS, SEC + 58 * MS]),
                fr(2 * SEC, 2 * SEC + 31 * MS, [2 * SEC + 32 * MS, 2 * SEC + 45 * MS]),
            ],
            lost_at: vec![SEC + 20 * MS, 2 * SEC, 3 * SEC],
            shown: vec![1_100 * MS, 1_150 * MS, 1_400 * MS, 3 * SEC],
            video_gaps: vec![[1_050 * MS, 30 * MS], [2_010 * MS, 200 * MS], [5 * SEC, 25 * MS]],
            ..Moonlight::default()
        };
        assert_eq!(m.unrecovered(SEC, 2 * SEC), 1);
        assert_eq!(m.unrecovered(SEC, 3 * SEC + 1), 3);
        assert_eq!(m.excess(0, SEC, 3 * SEC), [8 * MS, Dur::MAX, -MS]);
        assert_eq!(m.excess(1, 0, 2 * SEC), [-10 * MS, Dur::MAX]);
        // From 1 s to the first frame, between frames, and from the last to the end.
        assert_eq!(m.max_gap(SEC, 1_500 * MS), 250 * MS);
        assert_eq!(m.max_gap(SEC, 2 * SEC), 600 * MS);
        assert_eq!(m.max_gap(5 * SEC, 6 * SEC), SEC);
        // A gap counts if it ends at or after from and starts before to.
        assert_eq!(m.bulk_gap(SEC, 2 * SEC), 200 * MS);
        assert_eq!(m.bulk_gap(2_020 * MS, 10 * SEC), 25 * MS);
        assert_eq!(m.bulk_gap(6 * SEC, 7 * SEC), 0);
    }

    /// A Recorder from windows of bulk bytes on lanes 0, 1 and the Tailscale lane, with the
    /// primary and trial lanes the server reported.
    fn rec(windows: &[([i64; 3], i8, i8)]) -> Recorder {
        let mut r = Recorder::default();
        for &([a, b, mesh], role, trial) in windows {
            let mut x = [0; LANES];
            (x[0], x[1], x[MESH_LANE as usize]) = (a, b, mesh);
            r.bytes.push(x);
            r.roles.push(role);
            r.trials.push(trial);
        }
        r
    }

    #[test]
    fn primary_follows_the_report_else_the_bytes() {
        let r = rec(&[
            ([0, 0, 0], -1, -1),    // no bulk yet
            ([0, 0, 100], -1, -1),  // the Tailscale lane
            ([90, 0, 100], -1, -1), // still the most
            ([100, 0, 85], -1, -1), // at least 80% of the most: stays
            ([100, 0, 79], -1, -1), // below: moves
            ([0, 0, 0], -1, -1),    // nothing: stays
            ([0, 0, 0], 1, -1),     // reported
            ([500, 0, 0], -1, -1),  // the reported lane carried nothing
            ([50, 50, 0], -1, -1),  // a tie with the previous: stays
        ]);
        assert_eq!(r.primary(), [-1, 8, 8, 8, 0, 0, 1, 0, 0]);
        assert_eq!(r.share(8, 100 * MS, 600 * MS), 0.6);
        assert_eq!(r.share(0, 0, SEC), 4.0 / 9.0);
        assert_eq!(r.share(0, 5 * SEC, 6 * SEC), 0.0);
        assert_eq!(r.first_primary(0, 0), Some(400 * MS));
        assert_eq!(r.first_primary(0, 600 * MS), Some(700 * MS));
        assert_eq!(r.first_primary(1, 0), Some(600 * MS));
        assert_eq!(r.first_primary(8, -SEC), Some(100 * MS));
        assert_eq!(r.first_primary(8, 450 * MS), None);
        assert_eq!(r.first_primary(5, 0), None);
    }

    #[test]
    fn moves_count_returns_to_shorter_delays() {
        let p = [-1, 8, 8, 0, 0, 8, 1, 1, 0];
        let delay = |lane| match lane {
            0 => 30 * MS,
            1 => 35 * MS,
            _ => 31 * MS,
        };
        assert_eq!(rec_moves(&p, 0, 9, delay), (4, 2));
        assert_eq!(rec_moves(&p, 4, 9, delay), (3, 1));
        assert_eq!(rec_moves(&p, 0, 3, delay), (0, 0));
        // Through a world: lanes it has no link for are an hour away, so no move returns.
        let r = rec(&[([0; 3], 0, -1), ([0; 3], 1, -1), ([0; 3], 0, -1)]);
        assert_eq!(r.moves(&world(), 0, SEC), (2, 0));
        assert_eq!(r.delay(&world(), 3), HOUR);
        assert_eq!(r.delay(&world(), -1), HOUR);
    }

    #[test]
    fn trial_runs_count_reported_and_seen_trials() {
        let r = rec(&[
            ([100, 0, 0], 0, -1),   // 0
            ([100, 0, 0], 0, -1),   // 1
            ([100, 30, 0], 0, 1),   // 2: a reported trial of lane 1
            ([100, 30, 0], 0, 1),   // 3: the same run
            ([100, 0, 0], 0, -1),   // 4
            ([100, 20, 0], -1, -1), // 5: seen, 20% of the primary's bytes
            ([100, 0, 0], -1, -1),  // 6
            ([100, 50, 0], -1, -1), // 7: seen
            ([100, 50, 0], -1, -1), // 8: the same run
            ([100, 19, 0], -1, -1), // 9: too little
            ([100, 0, 0], 0, -1),   // 10
            ([0, 100, 0], 1, -1),   // 11: lane 1 primary
            ([0, 100, 0], 1, -1),   // 12
            ([100, 60, 0], 0, -1),  // 13: bulk moved off lane 1 here: not a trial
            ([100, 60, 0], 0, -1),  // 14: now it is
        ]);
        assert_eq!(r.trial_runs(1, 0, 2 * SEC), 4);
        assert_eq!(r.trial_runs(1, 300 * MS, 600 * MS), 2);
        assert_eq!(r.trial_runs(0, 0, 2 * SEC), 0);
        assert_eq!(r.trial_runs(1, 3 * SEC, 4 * SEC), 0);
    }

    #[test]
    fn queue_p95_over_the_samples_in_the_span() {
        let d = LinkId(0).down();
        let mut r = Recorder::default();
        r.queues.insert(d, (0..100).map(|i| i * MS).collect());
        assert_eq!(r.queue_p95(d, 0, SEC), 94 * MS);
        assert_eq!(r.queue_p95(d, 500 * MS, SEC), 97 * MS);
        assert_eq!(r.queue_p95(d, 2 * SEC, 3 * SEC), Dur::MAX);
        assert_eq!(r.queue_p95(LinkId(1).down(), 0, SEC), Dur::MAX);
    }

    fn lane(id: u16, role: &str) -> LaneStatus {
        let out = LaneOut { role: role.into(), ..LaneOut::default() };
        LaneStatus { id, out, ..LaneStatus::default() }
    }

    #[test]
    fn record_counts_bulk_entering_the_lanes_and_reads_roles() {
        let mut w = world();
        record(&mut w);
        assert_eq!(w.q.len(), 1, "the window timer");
        let hook = |w: &mut World, kind, data: &[u8]| {
            let (dev, link, event) = (DevId::Server, None, None);
            w.fire(&HookArgs { kind, dev, link, data, event });
        };
        hook(&mut w, HookKind::Inbound, &[TYPE_DATA; 1000]);
        hook(&mut w, HookKind::Inbound, &[TYPE_DATA | SMALL_BIT; 200]);
        hook(&mut w, HookKind::Sent, &[TYPE_DATA; 300]);
        hook(&mut w, HookKind::Inbound, &[TYPE_DATA; 500]);
        w.rec_window();
        // The server reports client link 2's lane as primary and the Tailscale lane as trial.
        let mut f = Fake::new(Role::Anchor, key_reader(Rng::new(1, 2)), false, Evil::None);
        f.lanes = vec![lane(0, "trial"), lane(0x0201, "primary"), lane(0x0101, "copies")];
        w[DevId::Server].sess = Some(Box::new(f));
        hook(&mut w, HookKind::Inbound, &[TYPE_DATA; 100]);
        w.rec_window();
        let r = w.rec.as_ref().unwrap();
        let mesh = |n| {
            let mut b = [0; LANES];
            b[MESH_LANE as usize] = n;
            b
        };
        assert_eq!(r.bytes, [mesh(1500), mesh(100)]);
        assert_eq!((r.roles.as_slice(), r.trials.as_slice()), (&[-1, 1][..], &[-1, 8][..]));
        assert_eq!(r.cur, [0; LANES]);
    }

    #[test]
    fn watch_zero_fills_and_starts_one_sampler() {
        let mut w = world();
        record(&mut w);
        w.now = 55 * MS;
        let (a, b) = (LinkId(0).down(), LinkId(1).down());
        w.watch(a);
        w.watch(b);
        let r = w.rec.as_ref().unwrap();
        assert_eq!(r.dirs, [a, b]);
        assert_eq!((r.queues[&a].as_slice(), r.queues[&b].as_slice()), (&[0; 5][..], &[0; 5][..]));
        assert_eq!(w.q.len(), 2, "the window timer and one queue sampler");
    }

    #[test]
    fn input_copies_count_the_first_copy_beyond_the_links_that_delivered() {
        let mut m = Moonlight::default();
        let input = |sent| Sample { sent, ..Sample::default() };
        m.inputs = [10, 20, 30, 40, 50, 1000].map(|t| input(t * MS)).to_vec();
        let tries = [1, 1, 2, 1, 1, 1];
        let enet = |tries| EnetState { tries, ..EnetState::default() };
        m.enet = tries.map(enet).to_vec();
        let mut st = InputCopies::default();
        let (hotel, cell) = (1, 2);
        let owd = || vec![30 * MS, 40 * MS];
        // 0: both copies reach the server; the first delivery counts, 2 ms beyond the hotel's.
        st.sent(100, (0, 1), 10 * MS, owd(), hotel);
        st.sent(100, (0, 1), 10 * MS, Vec::new(), cell);
        st.received(100, hotel);
        st.received(100, cell);
        st.delivered((0, 1), 42 * MS);
        st.delivered((0, 1), 50 * MS);
        // 1: the link lost the hotel's copy: measured against the cell, 1 ms beyond.
        st.sent(101, (1, 1), 20 * MS, owd(), hotel);
        st.sent(101, (1, 1), 20 * MS, Vec::new(), cell);
        st.received(101, cell);
        st.delivered((1, 1), 61 * MS);
        // 2: try 1 reached the server and was never delivered; wirebond never sent try 2.
        st.sent(102, (2, 1), 30 * MS, owd(), hotel);
        st.received(102, hotel);
        // 3: the links lost every copy: not counted.
        st.sent(103, (3, 1), 40 * MS, owd(), hotel | cell);
        // 4: on time over the Tailscale lane (the hotel's bit).
        st.sent(104, (4, 1), 50 * MS, vec![35 * MS, 40 * MS], hotel);
        st.received(104, hotel);
        st.delivered((4, 1), 85 * MS);
        // Packets it never saw sent change nothing.
        st.received(999, cell);
        st.delivered((9, 9), 90 * MS);
        assert_eq!(st.recs.len(), 5);
        let all = [2 * MS, MS, Dur::MAX, Dur::MAX, 0];
        assert_eq!(st.eval(&m, 0, SEC), all);
        assert_eq!(st.eval(&m, 20 * MS, 31 * MS), all[1..4]);
        assert_eq!(st.eval(&m, SEC, 2 * SEC), [Dur::MAX]);
        assert_eq!(st.recs[0].sent_on, hotel | cell);
    }
}
