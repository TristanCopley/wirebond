//! Tests of estimate.rs. Owner: C4. No Go unit tests (the pair tests exercise the estimates).
//! Series::mad and max over DEV_WINDOW, Winmin's two halves, Meter's rate and that `peek` leaves
//! the meter unchanged, ewma's first sample; echo processing (RTT, OWD, qd, target jitter, loss
//! with LOSS_MIN, delivery, liveness, the suspect timeout); R2-03: an echo whose token does not
//! match the echoed probe's challenge does not validate. New: no OWD sample from a probe sent
//! before a wake (swarm seeds 39 and 85).
//!
//! Echo processing calls into other modules. These tests keep off the paths that would change
//! what they measure: lanes are up or have no probe of the peer (no `set_state`), `wait_seq` is 0
//! (no `next_wait`), offered is 0 and `took_at` NEVER (the controller never asks `carries`). The
//! R2-03 test computes the expected tokens with the session's own `Challenger`; the wake test
//! ends with bandwidth's spread, which takes its lanes then.

use super::*;
use crate::buf::Pkt;
use crate::lane::{Lane, SentProbe};
use crate::pair::*;
use crate::session::Mode;
use crate::{MS, Role, SEC, SMALL_WINDOW};

const L1: u16 = 0x0101;
const L2: u16 = 0x0102;
const A: LaneIx = LaneIx(0);
const B: LaneIx = LaneIx(1);
const T0: Mono = 100 * SEC;

/// A client session at `now` with lanes L1 (index A) and L2 (index B), punching, without a probe
/// of the peer, and with the cut gate shut (`took_at` NEVER: no sample covers bulk they took).
fn session(now: Mono) -> Session {
    let mut s = Session::new(Role::Client, 1, DetRand::rand(11));
    s.now = now;
    for (id, sock) in [(L1, 3), (L2, 4)] {
        let mut l = Lane::new(id, sock);
        l.took_at = NEVER;
        s.lanes.live.push(l);
    }
    s
}

/// The same with both lanes validated and up.
fn up(now: Mono) -> Session {
    let mut s = session(now);
    for l in s.lanes.iter_mut() {
        (l.validated, l.state) = (true, State::Up);
    }
    s
}

/// Records probe `seq` as sent on `l` at `at` to address generation `agen`, after `pkts` packets.
fn sent(s: &mut Session, l: LaneIx, seq: u32, agen: u32, pkts: u32, at: Mono) {
    let ln = &mut s.lanes[l];
    ln.sent[seq as usize % RTT_RING] = SentProbe { seq, agen, pkts, at };
    ln.tx_seq = ln.tx_seq.max(seq);
}

/// An echo of L1's probe `seq`.
fn ech(seq: u32) -> Echo {
    Echo { lane: L1, seq, ..Echo::default() }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

#[test]
fn series_mad_and_max_over_dev_window() {
    let mut s = Series::default();
    assert_eq!((s.mad(0), s.max(0)), (0, None));
    for i in 0..9 {
        s.add(i * MS, (i + 1) * MS); // 1..9 ms: median 5, deviations 0 1 1 2 2 3 3 4 4
    }
    assert_eq!((s.mad(10 * MS), s.max(10 * MS)), (2 * MS, Some(9 * MS)));
    s.add(9 * MS, SEC); // a spike moves the median deviation by one sample, not by its size
    assert_eq!((s.mad(10 * MS), s.max(10 * MS)), (3 * MS, Some(SEC)));
    // A sample counts while it is at most DEV_WINDOW old. Of two, the upper is the median.
    assert_eq!((s.mad(8 * MS + DEV_WINDOW), s.max(8 * MS + DEV_WINDOW)), (991 * MS, Some(SEC)));
    assert_eq!((s.mad(9 * MS + DEV_WINDOW), s.max(9 * MS + DEV_WINDOW)), (0, Some(SEC)));
    assert_eq!((s.mad(9 * MS + DEV_WINDOW + 1), s.max(9 * MS + DEV_WINDOW + 1)), (0, None));
    // OWDs from an unsynchronized clock may be negative.
    let mut s = Series::default();
    for v in [-5 * MS, 0, 5 * MS] {
        s.add(0, v);
    }
    assert_eq!((s.mad(0), s.max(0)), (5 * MS, Some(5 * MS)));
}

#[test]
fn series_keeps_the_newest_mad_samples() {
    let mut s = Series::default();
    let n = MAD_SAMPLES as Mono;
    for v in 0..n + 10 {
        s.add(0, v);
    }
    // 10..=137 remain: median 74, deviations 0, 1, 1, …, 63, 63, 64; the 65th is 32.
    assert_eq!((s.n, s.max(0), s.mad(0)), (MAD_SAMPLES + 10, Some(n + 9), 32));
}

#[test]
fn series_max_keeps_its_largest_sample_while_it_is_in_the_window() {
    let mut s = Series::default();
    for (i, v) in [10, 30, 20].into_iter().enumerate() {
        s.add(i as Mono * MS, v * MS);
    }
    // A scan finds 30 ms in slot 1 and keeps it.
    assert_eq!((s.max(3 * MS), s.top.get()), (Some(30 * MS), Some(1)));
    // A smaller sample leaves it; an equal one takes over, being newer.
    s.add(3 * MS, 25 * MS);
    assert_eq!((s.max(4 * MS), s.top.get()), (Some(30 * MS), Some(1)));
    s.add(4 * MS, 30 * MS);
    assert_eq!(s.top.get(), Some(4));
    // Slot 1 has left the window; slot 4, still in it, is the answer without a scan.
    assert_eq!((s.max(MS + DEV_WINDOW + 1), s.top.get()), (Some(30 * MS), Some(4)));
    // A query back in time scans: the window holds more again.
    s.top.set(Some(2)); // a wrong cache, which only a scan corrects
    assert_eq!((s.max(0), s.top.get()), (Some(30 * MS), Some(4)));
    // Once the kept sample has left the window, a scan finds the largest one left.
    s.add(100 * MS, 5 * MS);
    assert_eq!((s.max(4 * MS + DEV_WINDOW), s.top.get()), (Some(30 * MS), Some(4)));
    assert_eq!((s.max(4 * MS + DEV_WINDOW + 1), s.top.get()), (Some(5 * MS), Some(5)));
    assert_eq!((s.max(100 * MS + DEV_WINDOW + 1), s.top.get()), (None, None));

    // The ring overwriting the kept sample leaves the max unknown, and the next query scans.
    let mut s = Series::default();
    s.add(0, SEC);
    for i in 1..MAD_SAMPLES as Mono {
        s.add(i * MS, i * MS);
    }
    assert_eq!((s.max(SEC), s.top.get()), (Some(SEC), Some(0)));
    s.add(SEC, 0);
    assert_eq!(s.top.get(), None);
    assert_eq!((s.max(SEC), s.top.get()), (Some(127 * MS), Some(127)));
    // The samples are the value, the cache is not; Default (a wake) clears it.
    let mut fresh = Series::default();
    (fresh.at, fresh.v, fresh.n) = (s.at, s.v, s.n);
    assert!(fresh == s && fresh.top.get().is_none() && s.top.get().is_some());
    assert_ne!(fresh, Series::default());
}

#[test]
fn wake_clears_the_owd_series_and_their_caches() {
    let mut s = up(T0);
    let ln = &mut s.lanes[A];
    for i in 0..10 {
        ln.owds.add(T0 + i * MS, 50 * MS);
        ln.owd_in.add(T0 + i * MS, 40 * MS);
        ln.rtts.add(T0 + i * MS, 60 * MS);
    }
    assert_eq!((ln.owd_in.max(T0 + 10 * MS), ln.rtts.max(T0 + 10 * MS)), (Some(40 * MS), Some(60 * MS)));
    assert!(ln.owd_in.top.get().is_some() && ln.rtts.top.get().is_some());
    s.wake(T0 + 20 * MS);
    // A paused clock stepped every OWD; an RTT, measured on my clock alone, still holds.
    let ln = &s.lanes[A];
    assert_eq!((ln.owds.top.get(), ln.owd_in.top.get()), (None, None));
    assert_eq!((ln.owd_in.max(T0 + 20 * MS), ln.rtts.max(T0 + 20 * MS)), (None, Some(60 * MS)));
}

/// Go's `max`, the scan `Series::max` made on every query before it kept its largest sample: the
/// reference for the cache.
fn max_scan(s: &Series, now: Mono) -> Option<Mono> {
    let mut m = None;
    for i in 0..s.n.min(MAD_SAMPLES) {
        let v = s.v[i] as Mono;
        if now - s.at[i] <= DEV_WINDOW && m.is_none_or(|top| v > top) {
            m = Some(v);
        }
    }
    m
}

/// Go's `mad`, as `Series::mad` was before selection: two full sorts. The reference.
fn mad_sort(s: &Series, now: Mono) -> Mono {
    let mut x = [0.0; MAD_SAMPLES];
    let mut k = 0;
    for i in 0..s.n.min(MAD_SAMPLES) {
        if now - s.at[i] <= DEV_WINDOW {
            x[k] = s.v[i];
            k += 1;
        }
    }
    if k == 0 {
        return 0;
    }
    let x = &mut x[..k];
    x.sort_unstable_by(f64::total_cmp);
    let m = x[k / 2];
    for v in x.iter_mut() {
        *v = (*v - m).abs();
    }
    x.sort_unstable_by(f64::total_cmp);
    x[k / 2] as Mono
}

/// splitmix64: the property test's draws.
struct Draw(u64);

impl Draw {
    fn u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let z = (self.0 ^ self.0 >> 30).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        let z = (z ^ z >> 27).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ z >> 31
    }

    /// Uniform in 0..n (n > 0).
    fn below(&mut self, n: u64) -> u64 {
        self.u64() % n
    }

    /// Uniform in 0..n as a Mono (0 < n).
    fn upto(&mut self, n: Mono) -> Mono {
        self.below(n as u64) as Mono
    }
}

/// Sample `n` of class `c`: 0, four values (ties); 1, RTTs and OWDs in whole µs, negative too
/// (an unsynchronized clock); 2, near ±2^53 and ±2^60, where an f64 holds every second, then
/// every 256th integer, so distinct samples tie once converted; 3, anywhere in i64; 4, the
/// extremes, which convert back saturated (i64::MAX − 1 becomes 2^63, then i64::MAX); 5, falling,
/// as an RTT draining a queue, so the largest sample is the oldest. Never NaN: samples are
/// integers.
fn sample(g: &mut Draw, c: u64, n: usize) -> Mono {
    match c {
        0 => g.upto(4) * MS,
        1 => (g.upto(600_000) - 100_000) * 1_000,
        2 => {
            let v = [1 << 53, 1 << 60][g.below(2) as usize] + g.upto(1024) - 512;
            if g.below(2) == 0 { v } else { -v }
        }
        3 => g.u64() as Mono,
        4 => [Mono::MIN, Mono::MIN + 1, Mono::MAX, Mono::MAX - 1, -1, 0, 1 << 62, -(1 << 62)][g.below(8) as usize],
        _ => SEC - n as Mono * 100_000 + g.upto(300_000),
    }
}

/// The cached `max` and `mad` by selection are Go's scan and sorts (`max_scan`, `mad_sort`) bit
/// for bit, on 2,048 random sequences of adds, queries, wakes and clones, over a million queries
/// of each: ties, values past 2^53 (distinct samples that tie as f64), the i64 extremes, samples
/// at one instant or stepping back in time, queries back in time, on a sample's edge of the
/// window or past every sample, empty windows, and the ring wrapping (the longest sequences add
/// about 1,650 samples). Each way `max` can go is taken many times.
#[test]
fn series_cached_max_and_selected_mad_are_the_scan_and_the_sorts() {
    let (mut queries, mut wraps, mut wakes) = (0u64, 0u64, 0u64);
    // Queries answered by the kept sample; scans with none kept, back in time, and with the kept
    // sample out of the window; adds that overwrote the kept sample.
    let (mut hit, mut unknown, mut back, mut left, mut overwrote) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for seed in 0..2_048 {
        let mut g = Draw(seed);
        let class = g.below(7); // 6: a mix of the six
        let steps = if g.below(16) == 0 { 3_000 } else { 1 + g.below(700) };
        // The samples' spacing: a window holds a few, a few dozen, or several rings' worth, so
        // that the ring overwrites the kept sample.
        let pace = [DEV_WINDOW / 4, DEV_WINDOW / 64, DEV_WINDOW / 512][g.below(3) as usize];
        // Mono times as a clock gives them, also late ones: a difference never overflows.
        let mut t = g.upto(1 << 40) + if g.below(8) == 0 { 1 << 61 } else { 0 };
        let (mut s, mut last) = (Series::default(), t);
        for _ in 0..steps {
            t = match g.below(100) {
                0 => t + DEV_WINDOW + g.upto(DEV_WINDOW), // past the window: it empties
                1 => (t - g.upto(DEV_WINDOW)).max(0),     // a clock stepping back
                2..7 => t,                                // the same instant
                _ => t + g.upto(2 * pace),
            };
            if g.below(100) < 55 {
                let c = if class == 6 { g.below(6) } else { class };
                overwrote += u64::from(s.top.get() == Some(s.n % MAD_SAMPLES));
                s.add(t, sample(&mut g, c, s.n));
                wraps += u64::from(s.n > MAD_SAMPLES);
            }
            match g.below(1_000) {
                0..2 => (s, wakes) = (Series::default(), wakes + 1), // wake
                2..6 => s = s.clone(),
                _ => {}
            }
            // Mostly at the current instant, as `self.now`; also a little later, back in time, on
            // a sample's edge of the window, and past every sample.
            for _ in 0..1 + g.below(2) {
                let q = match g.below(20) {
                    0 | 1 => last - g.upto(2 * DEV_WINDOW),
                    2 | 3 if s.n > 0 => s.at[g.below(s.n.min(MAD_SAMPLES) as u64) as usize] + DEV_WINDOW + g.upto(2),
                    4 => t + DEV_WINDOW + g.upto(DEV_WINDOW),
                    5..8 => t + g.upto(pace),
                    _ => t,
                };
                last = q;
                match s.top.get() {
                    None => unknown += 1,
                    Some(_) if q < s.asked.get() => back += 1,
                    Some(t) if q - s.at[t] > DEV_WINDOW => left += 1,
                    Some(_) => hit += 1,
                }
                assert_eq!(s.max(q), max_scan(&s, q), "seed {seed}: max at {q}");
                assert_eq!(s.mad(q), mad_sort(&s, q), "seed {seed}: mad at {q}");
                queries += 1;
            }
        }
    }
    let ways = [hit, unknown, back, left, overwrote, wraps, wakes];
    assert!(queries >= 1_000_000 && ways.iter().all(|&n| n >= 1_000), "{queries} queries; {ways:?}");
}

#[test]
fn winmin_keeps_two_halves() {
    let mut w = Winmin::new();
    assert_eq!(w.get(), NO_MIN);
    w.add(T0, 30 * MS);
    w.add(T0 + SEC, 50 * MS);
    assert_eq!(w.get(), 30 * MS);
    w.add(T0 + MIN_WINDOW, 40 * MS); // a new half: the old one still counts
    assert_eq!(w.get(), 30 * MS);
    w.add(T0 + 2 * MIN_WINDOW - 1, 45 * MS);
    assert_eq!(w.get(), 30 * MS);
    w.add(T0 + 2 * MIN_WINDOW, 45 * MS); // 2 × MIN_WINDOW on: 30 ms is gone
    assert_eq!(w.get(), 40 * MS);
    w.add(T0 + 2 * MIN_WINDOW + SEC, 20 * MS);
    assert_eq!(w.get(), 20 * MS);
    // One half per sample, however long the gap: the previous half counts until the next.
    w.add(T0 + 5 * MIN_WINDOW, 60 * MS);
    assert_eq!(w.get(), 20 * MS);
    w.add(T0 + 6 * MIN_WINDOW, 70 * MS);
    assert_eq!(w.get(), 60 * MS);
    w.reset();
    assert_eq!((w, w.get()), (Winmin::new(), NO_MIN));
}

#[test]
fn meter_rate_over_two_buckets() {
    let w = RATE_WINDOW;
    let mut m = Meter::default();
    m.add(T0, w, 1250); // 1,250 bytes in 100 ms: 100 kbit/s
    assert!(near(m.rate(T0, w), 1e5));
    m.add(T0 + w / 2, w, 1250);
    assert!(near(m.rate(T0 + w / 2, w), 2e5));
    assert!(near(m.rate(T0 + w, w), 2e5)); // rolled: all of it in the previous bucket
    assert!(near(m.rate(T0 + 3 * w / 2, w), 1e5)); // half of the previous bucket still counts
    m.add(T0 + 3 * w / 2, w, 2500);
    assert!(near(m.rate(T0 + 3 * w / 2, w), 3e5));
    assert!(near(m.rate(T0 + 2 * w, w), 2e5)); // rolled again
    assert_eq!(m.rate(T0 + 4 * w, w), 0.0); // two widths without bytes: empty
    // Other widths: a flow's SMALL_WINDOW, the metered minute.
    let mut m = Meter::default();
    m.add(T0, SMALL_WINDOW, 25_000);
    assert!(near(m.rate(T0, SMALL_WINDOW), 1e6));
    let mut m = Meter::default();
    m.add(T0, 60 * SEC, 7_500_000);
    assert!(near(m.rate(T0, 60 * SEC), 1e6));
}

#[test]
fn meter_peek_changes_nothing() {
    let mut m = Meter::default();
    m.add(T0, RATE_WINDOW, 1500);
    m.add(T0 + RATE_WINDOW / 2, RATE_WINDOW, 1500);
    let before = m;
    for t in [T0, T0 + RATE_WINDOW, T0 + 3 * RATE_WINDOW / 2, T0 + 5 * RATE_WINDOW] {
        let peeked = m.peek(t, RATE_WINDOW);
        assert_eq!(m, before);
        let mut rolled = m;
        assert_eq!(peeked, rolled.rate(t, RATE_WINDOW));
    }
}

#[test]
fn ewma_takes_the_first_sample_whole() {
    assert_eq!(ewma(0, 40 * MS), 40 * MS);
    assert_eq!(ewma(40 * MS, 48 * MS), 41 * MS);
    assert_eq!(ewma(40 * MS, 32 * MS), 39 * MS);
    assert_eq!(ewma(-40 * MS, -48 * MS), -41 * MS); // OWDs on the peer's clock
    // The peer's clock may be anything: no overflow.
    for (a, b) in [(Mono::MAX, Mono::MIN), (Mono::MIN, Mono::MAX), (1, Mono::MIN), (-1, Mono::MAX)] {
        ewma(a, b);
    }
}

#[test]
fn echo_validates_only_with_the_probes_challenge() {
    let mut s = session(T0); // L1 punching and unvalidated, address generation 1
    let tok = |s: &Session, agen, seq| s.chal.token(L1, agen, seq);
    let echo = |s: &mut Session, seq: u32, agen: u32, token: u64| {
        sent(s, A, seq, agen, 0, T0 - 30 * MS);
        s.echo(T0, Some(A), 0, Echo { token, ..ech(seq) });
    };
    // A changed token, none, the token of the probe before (the predictable Seq of R2-03), this
    // probe's for another address, or for a punching candidate: no validation.
    let wrong = [(5, tok(&s, 1, 5) ^ 1), (6, 0), (7, tok(&s, 1, 6)), (8, tok(&s, 2, 8)), (9, tok(&s, 0, 9))];
    for (seq, token) in wrong {
        echo(&mut s, seq, 1, token);
        assert!(!s.lanes[A].validated && !s.confirmed, "seq {seq}: validated with token {token:#x}");
    }
    // Liveness and RTT need no token.
    let ln = &s.lanes[A];
    assert_eq!((ln.echo_seq, ln.last_echo, s.last_echo, ln.rtt), (9, T0, T0, 30 * MS));
    // The challenge of the probe echoed validates, and confirms the session.
    let right = tok(&s, 1, 10);
    echo(&mut s, 10, 1, right);
    assert!(s.lanes[A].validated && s.confirmed);
    assert_eq!(s.lanes[A].state, State::Punching); // up needs the peer's probe too (§6.3)
    echo(&mut s, 11, 1, 0); // later echoes without a token do not undo it
    assert!(s.lanes[A].validated);

    // A claim (§7.6 rule 2): probes to the candidate address, generation 2, validate it only with
    // their own challenge, and only while the claim is open.
    let right = tok(&s, 2, 12);
    echo(&mut s, 12, 2, right);
    assert!(!s.lanes[A].cvalid, "no claim open");
    (s.lanes[A].cand, s.lanes[A].cgen) = (Some(C_PHONE), 2);
    for (seq, token) in [(13, tok(&s, 2, 13) ^ 1 << 40), (14, tok(&s, 1, 14)), (15, tok(&s, 2, 13))] {
        echo(&mut s, seq, 2, token);
        assert!(!s.lanes[A].cvalid, "seq {seq}: claim validated with token {token:#x}");
    }
    let right = tok(&s, 2, 16);
    echo(&mut s, 16, 2, right);
    assert!(s.lanes[A].cvalid);
    // A punching candidate's probe (generation 0) validates nothing.
    let mut s = session(T0);
    let right = tok(&s, 0, 1);
    echo(&mut s, 1, 0, right);
    assert!(!s.lanes[A].validated && !s.lanes[A].cvalid);
}

#[test]
fn echo_takes_rtt_only_from_its_own_lane() {
    let mut s = up(T0);
    sent(&mut s, A, 1, 1, 0, T0 - 40 * MS);
    s.echo(T0, Some(B), 0, ech(1)); // A's echo carried on B: liveness, no RTT
    assert_eq!((s.lanes[A].echo_seq, s.lanes[A].rtt, s.lanes[A].rtt_seq), (1, 0, 0));
    sent(&mut s, A, 2, 1, 0, T0 - 40 * MS);
    s.echo(T0, None, 0, ech(2)); // on no lane: no RTT
    assert_eq!((s.lanes[A].echo_seq, s.lanes[A].rtt), (2, 0));
    sent(&mut s, A, 3, 1, 0, T0 - 40 * MS);
    s.echo(T0, Some(A), 0, Echo { held: 15_000, ..ech(3) }); // held 15 ms at the peer
    let ln = &s.lanes[A];
    assert_eq!((ln.rtt, ln.last_rtt, ln.min_rtt.get(), ln.rttdev, ln.rtt_seq), (25 * MS, 25 * MS, 25 * MS, 0, 3));
    sent(&mut s, A, 4, 1, 0, T0 - 13 * MS);
    s.echo(T0 + 20 * MS, Some(A), 0, ech(4)); // 33 ms: an EWMA_GAIN step
    let ln = &s.lanes[A];
    assert_eq!((ln.rtt, ln.last_rtt, ln.min_rtt.get(), ln.rttdev), (26 * MS, 33 * MS, 25 * MS, 8 * MS));
    // An echo already seen on B still gives L1's RTT when it comes on L1; a held time as long as
    // the round trip gives none.
    sent(&mut s, A, 5, 1, 0, T0);
    s.echo(T0 + 40 * MS, Some(B), 0, ech(5));
    s.echo(T0 + 41 * MS, Some(A), 0, ech(5));
    assert_eq!((s.lanes[A].last_rtt, s.lanes[A].rtt_seq), (41 * MS, 5));
    sent(&mut s, A, 6, 1, 0, T0 + 40 * MS);
    s.echo(T0 + 60 * MS, Some(A), 0, Echo { held: 20_000, ..ech(6) });
    assert_eq!((s.lanes[A].last_rtt, s.lanes[A].rtt_seq, s.lanes[A].echo_seq), (41 * MS, 6, 6));
}

#[test]
fn echo_ignores_stale_and_unknown_echoes() {
    let mut s = up(T0);
    sent(&mut s, A, 7, 1, 0, T0 - 20 * MS);
    s.echo(T0, Some(A), 0, ech(0)); // seq 0: nothing received
    s.echo(T0, Some(A), 0, ech(8)); // never sent
    s.echo(T0, Some(A), 0, ech(7 + RTT_RING as u32)); // its slot holds another probe
    s.echo(T0, Some(A), 0, Echo { lane: 0x0303, ..ech(7) }); // no such lane
    assert_eq!((s.lanes[A].echo_seq, s.lanes[A].rtt, s.last_echo), (0, 0, 0));
    s.echo(T0, Some(A), 0, ech(7));
    sent(&mut s, A, 6, 1, 0, T0 - 30 * MS);
    s.echo(T0 + MS, Some(B), 0, Echo { last: 99, ..ech(6) }); // older than the newest: not fresh
    assert_eq!((s.lanes[A].echo_seq, s.lanes[A].last_echo, s.lanes[A].acked), (7, T0, 0));
}

#[test]
fn echo_measures_owd_and_queueing_delay() {
    let mut s = up(T0);
    let owd = |s: &mut Session, seq: u32, t: Mono, rtt: Mono, o: Mono| {
        sent(s, A, seq, 1, 0, t - rtt);
        s.echo(t, (rtt > 0).then_some(A), 0, Echo { owd: o / 1_000, ..ech(seq) });
        let ln = &s.lanes[A];
        (ln.owd, ln.owd_now, ln.qd)
    };
    assert_eq!(owd(&mut s, 1, T0, 0, 50 * MS), (50 * MS, 50 * MS, 0));
    // A queue of 20 ms, with no RTT to bound it.
    assert_eq!(owd(&mut s, 2, T0 + 20 * MS, 0, 70 * MS), (52_500_000, 70 * MS, 20 * MS));
    // RTT bounds qd: a clock that stepped 20 ms is no queue when the round trip did not grow.
    assert_eq!(owd(&mut s, 3, T0 + 40 * MS, 40 * MS, 70 * MS).2, 0);
    assert_eq!(owd(&mut s, 4, T0 + 60 * MS, 52 * MS, 64 * MS).2, 12 * MS);
    assert_eq!(owd(&mut s, 5, T0 + 80 * MS, 70 * MS, 55 * MS).2, 5 * MS);
    // A minimum that a reset left empty bounds nothing (notes/core.md 38).
    s.lanes[A].min_rtt.reset();
    s.lanes[A].min_owd.reset();
    assert_eq!(owd(&mut s, 6, T0 + 100 * MS, 0, 64 * MS).2, 0);
    // OWDs on the peer's clock may be negative.
    assert_eq!(owd(&mut s, 7, T0 + 120 * MS, 0, -10 * MS).1, -10 * MS);
}

/// Swarm seeds 39 and 85 (notes/rust.md, harden: integration): a wake resets the minima (§8.3),
/// but the echoes of probes sent before it still arrive, with OWDs on the clock relation that a
/// paused clock left behind, the pause below every later one. Taken, such a sample held lane A's
/// base OWD the pause below lane B's for MIN_WINDOW to 2 × MIN_WINDOW, and bandwidth's skew filter
/// (§11.2 rule 2) kept B out of the spread. Go takes it (estimate.go `echo`).
#[test]
fn echo_takes_no_owd_from_a_probe_sent_before_the_wake() {
    let mut s = up(T0);
    (s.lanes[A].group, s.lanes[B].group) = (0, 1);
    // Lane l's probe `seq`, sent at `at` and echoed at `now` with OWD `o`, on lane `on` after the
    // peer held it `held` µs.
    let echo = |s: &mut Session, l: LaneIx, seq: u32, at: Mono, now: Mono, o: Mono, on: Option<LaneIx>, held: u32| {
        sent(s, l, seq, 1, 0, at);
        s.now = now;
        s.echo(now, on, 0, Echo { lane: s.lanes[l].id, seq, held, owd: o / 1_000, ..Echo::default() });
    };
    // Before the sleep: A at 50 ms, then a 70 ms queue, over the target (no jitter: 20 ms); B at
    // 52 ms.
    for seq in 1..=5 {
        let t = T0 + seq as Mono * 20 * MS;
        echo(&mut s, A, seq, t - 30 * MS, t, 50 * MS, None, 0);
    }
    echo(&mut s, A, 6, T0 + 90 * MS, T0 + 120 * MS, 120 * MS, None, 0);
    echo(&mut s, B, 1, T0 + 90 * MS, T0 + 120 * MS, 52 * MS, None, 0);
    assert_eq!((s.lanes[A].min_owd.get(), s.lanes[A].qd, s.lanes[A].over_n), (50 * MS, 70 * MS, 1));
    // Probe 7 is on its way as the clock pauses for 2.3 s; the device wakes 10 ms later on its own
    // clock. The minima and the OWD estimates start again, and qd with them.
    let pause = 2_300 * MS;
    sent(&mut s, A, 7, 1, 0, T0 + 130 * MS);
    let woke = T0 + 140 * MS;
    s.wake(woke);
    for l in [A, B] {
        let ln = &s.lanes[l];
        assert_eq!((ln.min_owd.get(), ln.min_rtt.get(), ln.owd, ln.owds.n, ln.qd), (NO_MIN, NO_MIN, 0, 0, 0));
    }
    // Probe 7's echo arrives, on its own lane, held by the peer through the pause. It is a fresh
    // echo, for liveness; its RTT, the pause short on my paused clock, is no sample either.
    echo(&mut s, A, 7, T0 + 130 * MS, woke + 20 * MS, 120 * MS, Some(A), (pause / 1_000) as u32);
    let ln = &s.lanes[A];
    assert_eq!((ln.echo_seq, ln.last_echo, ln.min_rtt.get()), (7, woke + 20 * MS, NO_MIN));
    // Its OWD is on the old relation: no sample. qd stays 0, so A's queue, which the pause drained,
    // is not a second qd over target in a row, an overuse (Go's first sample after the reset gives
    // 0 too).
    assert_eq!((ln.min_owd.get(), ln.owds.n, ln.owd, ln.qd, ln.over_n), (NO_MIN, 0, 0, 0, 0));
    assert_eq!(ln.owd_now, 120 * MS, "the newest sample is still probe 6's");
    // Probes sent from the wake on (the wake's own punch sends at its instant) carry every OWD the
    // pause higher, and give both lanes their base OWD again.
    echo(&mut s, A, 8, woke, woke + 80 * MS, 50 * MS + pause, None, 0);
    echo(&mut s, B, 2, woke, woke + 80 * MS, 52 * MS + pause, None, 0);
    for (l, base) in [(A, 50 * MS + pause), (B, 52 * MS + pause)] {
        let ln = &s.lanes[l];
        assert_eq!((ln.min_owd.get(), ln.owd, ln.owd_now, ln.qd, ln.dev), (base, base, base, 0, 0));
    }
    // So bandwidth spreads over both lanes; with probe 7's sample A's base OWD sat 2.2 s below B's.
    (s.mode, s.active) = (Mode::Bandwidth, true);
    s.schedule(&mut Acts::default(), woke + 80 * MS);
    assert_eq!(s.spread(), [A, B]);

    // An activation is no clock step: a probe sent before it still gives its OWD.
    let mut s = up(T0);
    s.since = T0;
    echo(&mut s, A, 1, T0 - MS, T0 + 30 * MS, 50 * MS, None, 0);
    assert_eq!((s.lanes[A].min_owd.get(), s.lanes[A].owd), (50 * MS, 50 * MS));
}

#[test]
fn echo_takes_target_jitter_only_while_app_limited() {
    let mut s = up(T0);
    let mut seq = 0;
    let mut feed = |s: &mut Session, t: Mono, owds: &[Mono]| {
        for (i, &o) in owds.iter().enumerate() {
            seq += 1;
            let t = t + i as Mono * 20 * MS;
            sent(s, A, seq, 1, 0, t - 30 * MS);
            s.echo(t, None, 0, Echo { owd: o / 1_000, ..ech(seq) });
        }
        (s.lanes[A].dev, s.lanes[A].dev_app)
    };
    let (lo, hi) = (50 * MS, 64 * MS);
    assert_eq!(feed(&mut s, T0, &[50 * MS, 60 * MS, 50 * MS, 60 * MS]), (10 * MS, 10 * MS));
    // Overuse within DEV_WINDOW: the deviation is the lane's own queue, not jitter.
    let t = T0 + 4 * SEC;
    s.lanes[A].last_over = t - SEC;
    assert_eq!(feed(&mut s, t, &[lo, hi, lo, hi]), (14 * MS, 10 * MS));
    // A pacer queue: not app-limited either.
    s.lanes[A].last_over = 0;
    s.lanes[A].q.push_back(Pkt::default());
    assert_eq!(feed(&mut s, t + 100 * MS, &[lo, hi]), (14 * MS, 10 * MS));
    s.lanes[A].q.clear();
    assert_eq!(feed(&mut s, t + 200 * MS, &[lo, hi]), (14 * MS, 14 * MS));
}

/// Echoes L1's probe `seq`, sent at `T0 + 20 ms × seq` after `sent_pkts` packets, `got` packets
/// and `bytes` bytes having arrived at the peer, which received it 10 ms later by its clock (as
/// SentAt − Held of the carrying probe, µs). Returns (loss, delivery, lossy as adjust saw it).
fn loss(s: &mut Session, seq: u32, sent_pkts: u32, got: u32, bytes: u32) -> (f64, f64, bool) {
    let at = T0 + seq as Mono * 20 * MS;
    sent(s, A, seq, 1, sent_pkts, at);
    let rx_us = (at + 10 * MS) / 1_000;
    s.echo(at + 25 * MS, None, rx_us + 5_000, Echo { held: 5_000, pkts: got, bytes, ..ech(seq) });
    let ln = &s.lanes[A];
    (ln.loss, ln.delivery, ln.last_over == at + 25 * MS) // qd is 0: overuse is loss
}

#[test]
fn echo_loss_needs_loss_min_packets() {
    // Probes every 20 ms; the window reaches back RATE_WINDOW (five probes). 4 of 100 lost: 4%,
    // but under LOSS_MIN packets.
    let mut s = up(T0);
    for k in 1..=5 {
        assert_eq!(loss(&mut s, k, 20 * k, 20 * k, 0), (0.0, 0.0, false));
    }
    let (l, _, lossy) = loss(&mut s, 6, 120, 116, 0);
    assert!(near(l, 0.04) && !lossy, "loss {l}");
    // 5 of 100: overuse.
    let (l, _, lossy) = loss(&mut s, 7, 140, 135, 0);
    assert!(near(l, 0.05) && lossy, "loss {l}");
    // 2 of 20 is 10%, still under LOSS_MIN: a radio's random loss at a low rate.
    let mut s = up(T0);
    for k in 1..=5 {
        loss(&mut s, k, 4 * k, 4 * k, 0);
    }
    let (l, _, lossy) = loss(&mut s, 6, 24, 22, 0);
    assert!(near(l, 0.1) && !lossy, "loss {l}");
    // More arrived than were sent (a count from before a move): no loss.
    assert_eq!(loss(&mut s, 7, 28, 40, 0).0, 0.0);
}

#[test]
fn echo_delivery_over_the_rate_window() {
    let mut s = up(T0);
    for k in 1..=6 {
        loss(&mut s, k, 0, 0, 125_000 * k); // 125 kB per 20 ms
    }
    // From probe 1 to 6: 625 kB over 100 ms, 50 Mbit/s.
    assert!(near(s.lanes[A].delivery, 50e6), "{}", s.lanes[A].delivery);
    // Probe 7 reached the peer 50 ms after it was sent: arrivals spread over more time than the
    // sends (a queue building), and the longer span counts. From probe 2 (received at T0 + 50 ms)
    // to 7 (at T0 + 190 ms): 625 kB over 140 ms.
    let at = T0 + 7 * 20 * MS;
    sent(&mut s, A, 7, 1, 0, at);
    s.echo(at + 60 * MS, None, (at + 50 * MS) / 1_000, Echo { bytes: 125_000 * 7, ..ech(7) });
    assert!(near(s.lanes[A].delivery, 625_000.0 * 8.0 / 0.14), "{}", s.lanes[A].delivery);
    // Byte counts wrap.
    let mut s = up(T0);
    loss(&mut s, 1, 0, 0, u32::MAX - 62_499);
    loss(&mut s, 2, 0, 0, 62_500);
    assert!(near(s.lanes[A].delivery, 125_000.0 * 8.0 / 0.02), "{}", s.lanes[A].delivery);
}

#[test]
fn echo_loss_counts_only_probes_to_the_current_address() {
    let mut s = up(T0);
    for k in 1..=3 {
        loss(&mut s, k, 20 * k, 20 * k, 0);
    }
    // The lane moved after probe 3 (C3's `moved`): generation 2, history restarted.
    (s.lanes[A].pgen, s.lanes[A].hist_n, s.lanes[A].hist_seq) = (2, 0, 3);
    let at = T0 + 4 * 20 * MS;
    sent(&mut s, A, 4, 1, 80, at); // sent to the old address before the move: not counted
    s.echo(at + 25 * MS, None, 0, Echo { pkts: 20, ..ech(4) });
    assert_eq!((s.lanes[A].hist_n, s.lanes[A].loss), (0, 0.0));
    for k in 5..=10 {
        let at = T0 + k as Mono * 20 * MS;
        sent(&mut s, A, k, 2, 20 * k, at);
        s.echo(at + 25 * MS, None, 0, Echo { pkts: 20 * k, ..ech(k) });
    }
    assert_eq!((s.lanes[A].hist_n, s.lanes[A].loss), (6, 0.0));
}

#[test]
fn echo_keeps_liveness_and_clean_runs() {
    let mut s = up(T0);
    s.lanes[A].misses = 2;
    sent(&mut s, A, 1, 1, 0, T0 - 30 * MS);
    s.echo(T0, Some(A), 0, Echo { last: 4711, ..ech(1) });
    let ln = &s.lanes[A];
    assert_eq!((ln.echo_seq, ln.last_echo, ln.misses, ln.acked, s.last_echo), (1, T0, 0, 4711, T0));
    assert_eq!(ln.clean_at, NEVER); // the first echo in 100 s ends no gap in time
    assert!(s.confirmed); // validated, and sent since the last activation
    // A clean run starts with an echo in time and goes on through echoes in time.
    for (seq, t) in [(2, T0 + 20 * MS), (3, T0 + 40 * MS)] {
        sent(&mut s, A, seq, 1, 0, t - 20 * MS);
        s.echo(t, Some(A), 0, ech(seq));
        assert_eq!(s.lanes[A].clean_at, T0 + 20 * MS);
    }
    // A gap past the suspect timeout ends it, and so does the peer's lagging mark.
    let late = T0 + 40 * MS + s.suspect_after(A) + 1;
    s.now = late;
    sent(&mut s, A, 4, 1, 0, late - 30 * MS);
    s.echo(late, None, 0, ech(4));
    assert_eq!(s.lanes[A].clean_at, NEVER);
    sent(&mut s, A, 5, 1, 0, late);
    s.echo(late + 20 * MS, None, 0, ech(5));
    assert_eq!(s.lanes[A].clean_at, late + 20 * MS);
    s.lanes[A].peer_lag = true;
    sent(&mut s, A, 6, 1, 0, late + 20 * MS);
    s.echo(late + 40 * MS, None, 0, ech(6));
    assert_eq!(s.lanes[A].clean_at, NEVER);
    // A probe sent before the session last became active confirms nothing.
    let mut s = up(T0);
    s.since = T0;
    sent(&mut s, A, 1, 1, 0, T0 - MS);
    s.echo(T0 + 30 * MS, None, 0, ech(1));
    assert!(!s.confirmed);
}

#[test]
fn suspect_after_from_smoothed_rtt() {
    let mut s = up(T0);
    assert_eq!(s.suspect_after(A), DOWN_AFTER); // no RTT yet
    (s.lanes[A].rtt, s.lanes[A].rttdev) = (30 * MS, 2 * MS);
    assert_eq!(s.suspect_after(A), 30 * MS + SUSPECT_MIN + PROBE_ACTIVE);
    s.lanes[A].rttdev = 10 * MS;
    assert_eq!(s.suspect_after(A), 30 * MS + 40 * MS + PROBE_ACTIVE);
    // At least the largest RTT of DEV_WINDOW (a bounded jitter's peak).
    s.lanes[A].rtts.add(T0, 120 * MS);
    assert_eq!(s.suspect_after(A), 120 * MS + PROBE_ACTIVE);
    s.now = T0 + DEV_WINDOW + 1;
    assert_eq!(s.suspect_after(A), 70 * MS + PROBE_ACTIVE);
}

#[test]
fn echo_survives_hostile_values() {
    let mut s = up(T0);
    let mut seq = 0;
    for (owd, held, sent_at, pkts, bytes) in [
        (i64::MAX, u32::MAX, i64::MIN, u32::MAX, u32::MAX),
        (i64::MIN, 0, i64::MAX, 0, 0),
        (i64::MAX / 1_000 + 1, u32::MAX, i64::MIN + 1, 1, 1),
        (i64::MIN / 1_000 - 1, 1, 0, u32::MAX, 0),
    ] {
        for carrier in [Some(A), None] {
            seq += 1;
            sent(&mut s, A, seq, 1, seq.wrapping_mul(0x9e37_79b9), T0 - MS);
            s.echo(T0 + seq as Mono, carrier, sent_at, Echo { owd, held, pkts, bytes, last: u64::MAX, ..ech(seq) });
            let ln = &s.lanes[A];
            assert!((0.0..=1.0).contains(&ln.loss) && ln.delivery >= 0.0, "{} {}", ln.loss, ln.delivery);
        }
    }
    assert_eq!(s.lanes[A].echo_seq, seq);
}
