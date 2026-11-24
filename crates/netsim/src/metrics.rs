//! What the acceptance criteria measure (Go netsim/metrics_test.go, which lived in a test file
//! because only tests use it; the Rust integration tests need it from the library): the Recorder
//! that samples lane roles, bulk bytes and queues; quantiles; and frame, gap and lateness
//! metrics. Owner: N3.

use std::collections::BTreeMap;

use crate::apps::{Frame, Moonlight, Sample};
use crate::env::is_bulk;
use crate::link::DirId;
use crate::node::DevId;
use crate::sim::{Dur, HookKind, MS, World};

/// The Recorder's period for lane roles and bulk bytes.
pub const WINDOW: Dur = 100 * MS;

/// The Recorder's index for the Tailscale lane; client links are 0…7 by id − 1 (Go mesh).
pub const MESH_LANE: i8 = 8;

/// Lanes the Recorder counts: eight client links and the Tailscale lane.
pub const LANES: usize = 9;

/// The period of the queue samples.
const QUEUE_EVERY: Dur = 10 * MS;

/// Samples what the acceptance criteria need (§17.3): per WINDOW, the bulk bytes the server sent
/// on each lane and the lane its Snapshot calls primary; every 10 ms, the queue of each
/// watched direction. Owned by the world (`world.rec`), started by [`record`].
#[derive(Clone, Debug, Default)]
pub struct Recorder {
    /// Bulk bytes (TYPE_DATA without the small bit) per window and lane.
    pub bytes: Vec<[i64; LANES]>,
    /// The server's primary per window, from Snapshot; −1: none reported.
    pub roles: Vec<i8>,
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

    /// Closes one window: its bytes, and the server's primary lane (Go window).
    pub(crate) fn rec_window(&mut self) {
        let lanes = self[DevId::Server].snap().lanes;
        let Some(r) = self.rec.as_mut() else {
            return;
        };
        r.bytes.push(r.cur);
        r.cur = [0; LANES];
        let mut prim = -1;
        for l in lanes.iter().filter(|l| l.out.role == "primary") {
            prim = match l.id {
                0 => MESH_LANE,
                id => ((id >> 8) as i8).wrapping_sub(1),
            };
        }
        r.roles.push(prim);
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

    /// The fraction of windows in [from, to) whose primary is lane.
    pub fn share(&self, lane: i8, from: Dur, to: Dur) -> f64 {
        let p = self.primary();
        let (a, b) = span(from, to, p.len());
        let n = p[a..b].iter().filter(|&&x| x == lane).count();
        n as f64 / (b - a).max(1) as f64
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

/// The p-th quantile of xs (nearest rank); `Dur::MAX` (+Inf) when xs is empty (Go Pct).
pub fn pct(xs: &[Dur], p: f64) -> Dur {
    if xs.is_empty() {
        return Dur::MAX;
    }
    let mut s = xs.to_vec();
    let k = ((p * s.len() as f64).ceil() as usize).saturating_sub(1);
    *s.select_nth_unstable(k.min(xs.len() - 1)).1
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
    use wirebond_core::{LaneOut, LaneStatus, Role, SMALL_BIT, TYPE_DATA};

    use super::*;
    use crate::fake::{Evil, Fake, fake_maker};
    use crate::link::LinkId;
    use crate::sim::{HookArgs, Rng, SEC, key_reader};

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
    }

    /// A Recorder from windows of bulk bytes on lanes 0, 1 and the Tailscale lane, with the
    /// primary the server reported.
    fn rec(windows: &[([i64; 3], i8)]) -> Recorder {
        let mut r = Recorder::default();
        for &([a, b, mesh], role) in windows {
            let mut x = [0; LANES];
            (x[0], x[1], x[MESH_LANE as usize]) = (a, b, mesh);
            r.bytes.push(x);
            r.roles.push(role);
        }
        r
    }

    #[test]
    fn primary_follows_the_report_else_the_bytes() {
        let r = rec(&[
            ([0, 0, 0], -1),    // no bulk yet
            ([0, 0, 100], -1),  // the Tailscale lane
            ([90, 0, 100], -1), // still the most
            ([100, 0, 85], -1), // at least 80% of the most: stays
            ([100, 0, 79], -1), // below: moves
            ([0, 0, 0], -1),    // nothing: stays
            ([0, 0, 0], 1),     // reported
            ([500, 0, 0], -1),  // the reported lane carried nothing
            ([50, 50, 0], -1),  // a tie with the previous: stays
        ]);
        assert_eq!(r.primary(), [-1, 8, 8, 8, 0, 0, 1, 0, 0]);
        assert_eq!(r.share(8, 100 * MS, 600 * MS), 0.6);
        assert_eq!(r.share(0, 0, SEC), 4.0 / 9.0);
        assert_eq!(r.share(0, 5 * SEC, 6 * SEC), 0.0);
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
        assert_eq!(r.roles, [-1, 1]);
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
}
