//! Tests of rate.rs. Owner: C4. Ports from core/rate_test.go: TestCut (TestReorder is flow.rs's).
//! New: take and restart (R2-04's rate half), the controller as notes/core.md 36 and 46–53 leave
//! it, the pacer's token bucket, and the rewind ring's bounds.
//!
//! The controller asks `carries`: the tests set the primary and the spread as it reads them. They
//! keep the pacer from sending (no credit, or no validated address) and put no buffers in queues
//! or rings.

use super::*;
use crate::estimate::Meter;
use crate::pair::*;
use crate::{Role, SEC};

const A: LaneIx = LaneIx(0);
const T0: Mono = 100 * SEC;
const RTT: Mono = 40 * MS;

/// A session in `mode` at T0 with one lane, validated and up, with an RTT of 40 ms, that carries
/// bulk (the primary, or the spread) and took it long ago; its rate is `rate`, out of startup.
fn bulk(mode: Mode, rate: f64) -> Session {
    let mut s = Session::new(Role::Client, 1, DetRand::rand(1));
    (s.mode, s.now) = (mode, T0);
    let mut l = Lane::new(0x0101, 3);
    (l.validated, l.state, l.peer, l.rtt) = (true, State::Up, Some(S_ETH), RTT);
    (l.rate, l.startup) = (rate, false);
    s.lanes.live.push(l);
    (s.spread[0], s.nspread) = (A, 1);
    s
}

/// Puts a second lane, validated and up, beside A in the bandwidth spread: A shares the stream.
fn share(s: &mut Session) {
    let mut l = Lane::new(0x0201, 4);
    (l.validated, l.state, l.peer, l.rtt) = (true, State::Up, Some(S_ETH), RTT);
    s.lanes.live.push(l);
    (s.spread[1], s.nspread) = (LaneIx(1), 2);
}

/// Sets the lane's offered meter to `bps` over the RATE_WINDOW ending at `now`.
fn offer(s: &mut Session, now: Mono, bps: f64) {
    s.lanes[A].offered = Meter::default();
    s.lanes[A].offered.add(now, RATE_WINDOW, (bps * 0.1 / 8.0).round() as usize);
}

/// An echo of the lane at `now` with queueing delay `qd`, `bps` offered, and `lossy`.
fn echo(s: &mut Session, now: Mono, qd: Mono, bps: f64, lossy: bool) -> f64 {
    s.lanes[A].qd = qd;
    offer(s, now, bps);
    s.adjust(now, A, lossy);
    s.lanes[A].rate
}

/// A bulk packet of `n` bytes queued at `at`, without a buffer.
fn pkt(c: u64, at: Mono, n: u32) -> Pkt {
    Pkt { c, at, buf: None, n }
}

/// Empties the lane's bucket as of `now`, so that a drain then sends nothing.
fn starve(s: &mut Session, now: Mono) {
    (s.lanes[A].tokens, s.lanes[A].filled) = (0.0, now);
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

/// TestCut: the rate after an overuse is never NaN and never below PACE_FLOOR, for a policer's
/// loss with qd = 0, a full stall's delivery of 0 and an unlimited rate (§9).
#[test]
fn cut_is_never_nan_nor_under_the_floor() {
    for (rate, delivery, qd, target, want) in [
        (INF, 0.0, 0, TARGET_FLOOR, PACE_FLOOR),
        (INF, 30e6, 0, TARGET_FLOOR, 30e6),
        (INF, 30e6, 150 * MS, TARGET_FLOOR, 30e6 * (1.0 - 130.0 / 300.0)),
        (INF, 30e6, 3600 * SEC, TARGET_FLOOR, 30e6 * (0.5 + 0.02 / 7200.0)), // f = 0.5 + target/(2·qd)
        (40e6, 100e6, 30 * MS, TARGET_FLOOR, 34e6),
        (6e6, 1e6, 0, TARGET_FLOOR, PACE_FLOOR),
    ] {
        let got = cut(rate, qd, target, delivery);
        assert!(
            !got.is_nan() && got >= PACE_FLOOR && (got - want).abs() <= 1.0,
            "cut({rate}, qd {qd}, delivery {delivery}) = {got}, want {want}"
        );
    }
    let mut l = Lane::new(1, 1);
    (l.state, l.validated) = (State::Up, true);
    assert_eq!(l.pace(0), PACE_FLOOR, "an idle unlimited lane paces at PACE_FLOOR");
    // Rust's min and max drop a NaN where Go's would return it: still never NaN.
    for (rate, delivery) in [(f64::NAN, 30e6), (INF, f64::NAN), (f64::NAN, f64::NAN)] {
        let got = cut(rate, 30 * MS, TARGET_FLOOR, delivery);
        assert!(!got.is_nan() && got >= PACE_FLOOR, "cut({rate}, delivery {delivery}) = {got}");
    }
}

#[test]
fn pace_is_twice_the_offered_rate_within_the_rate() {
    let mut s = bulk(Mode::Performance, INF);
    offer(&mut s, T0, 30e6);
    assert!(near(s.lanes[A].pace(T0), 60e6));
    s.lanes[A].rate = 40e6;
    assert_eq!(s.lanes[A].pace(T0), 40e6);
    offer(&mut s, T0, 1e6);
    assert_eq!(s.lanes[A].pace(T0), PACE_FLOOR);
}

#[test]
fn take_restarts_a_stale_rate_before_bulk_moves_onto_the_lane() {
    let mut s = bulk(Mode::Performance, PACE_FLOOR);
    s.nspread = 0; // about to take bulk
    let cut_long_ago = |s: &mut Session, bulk_at| {
        let ln = &mut s.lanes[A];
        (ln.rate, ln.startup, ln.over_n, ln.regain, ln.bulk_at) = (PACE_FLOOR, false, 3, 20e6, bulk_at);
    };
    // A rate learned more than MIN_WINDOW ago is stale: taking the lane restarts it.
    cut_long_ago(&mut s, T0 - MIN_WINDOW);
    s.take(T0, A);
    let ln = &s.lanes[A];
    assert_eq!((ln.rate, ln.startup, ln.over_n, ln.regain, ln.took_at), (INF, true, 0, 0.0, T0));
    // One learned within MIN_WINDOW holds: a lane that left bulk for a moment keeps it.
    cut_long_ago(&mut s, T0 - MIN_WINDOW + 1);
    s.take(T0, A);
    let ln = &s.lanes[A];
    assert_eq!((ln.rate, ln.startup, ln.over_n, ln.took_at), (PACE_FLOOR, false, 3, T0));
    // In bandwidth a restart is PACE_FLOOR, in startup (§12; no fits bound in 2.0, §25).
    s.mode = Mode::Bandwidth;
    cut_long_ago(&mut s, T0 - MIN_WINDOW);
    s.take(T0, A);
    assert_eq!((s.lanes[A].rate, s.lanes[A].startup), (PACE_FLOOR, true));
    s.mode = Mode::Performance;

    // R2-04: taken first, a rewind's 150 kB moved onto the lane paces at twice its offered rate,
    // not at the PACE_FLOOR an old cut left. (Not validated: the pacer sends nothing here.)
    let rewind = |s: &mut Session, now| {
        for c in 0..100 {
            s.enqueue(&mut Acts::default(), now, A, pkt(c, 0, 1500));
        }
        s.lanes[A].pace(now)
    };
    s.lanes[A].validated = false;
    let now = T0 + 2 * SEC;
    cut_long_ago(&mut s, now - MIN_WINDOW);
    s.take(now, A);
    assert!(near(rewind(&mut s, now), 2.0 * 150_000.0 * 8.0 / 0.1), "{}", s.lanes[A].pace(now));
    assert_eq!((s.lanes[A].bulk_at, s.lanes[A].q.len(), s.lanes[A].q_bytes), (now, 100, 150_000));
    // Moved first, the same packets make the old cut look fresh (enqueue marks the lane as
    // carrying bulk now), and a take after them keeps it: callers take first.
    let mut s = bulk(Mode::Performance, PACE_FLOOR);
    (s.nspread, s.lanes[A].validated) = (0, false);
    cut_long_ago(&mut s, now - MIN_WINDOW);
    assert_eq!(rewind(&mut s, now), PACE_FLOOR);
    s.take(now, A);
    assert_eq!((s.lanes[A].rate, s.lanes[A].pace(now)), (PACE_FLOOR, PACE_FLOOR));
}

#[test]
fn adjust_cuts_on_two_queued_echoes_at_most_once_per_rtt() {
    let mut s = bulk(Mode::Performance, INF);
    s.restart(T0, A); // startup, unlimited
    s.lanes[A].delivery = 25e6;
    assert_eq!(echo(&mut s, T0, 30 * MS, 30e6, false), INF); // one echo over target: not yet
    assert_eq!(s.lanes[A].over_n, 1);
    // Two: down to delivery, by f = 1 − (qd − target)/(2 qd); startup ends and leaves regain.
    let r = echo(&mut s, T0 + 20 * MS, 30 * MS, 30e6, false);
    assert!(near(r, 25e6 * (1.0 - 10.0 / 60.0)), "{r}");
    let ln = &s.lanes[A];
    assert!(!ln.startup && near(ln.regain, BETA * 25e6));
    let t = T0 + 20 * MS;
    assert_eq!((ln.last_cut, ln.cut_qd, ln.last_over, ln.clean_at), (t, 30 * MS, t, NEVER));
    // Within an RTT, no second cut; in stable within 3 RTTs only a queue still growing.
    assert_eq!(echo(&mut s, t + RTT / 2, 30 * MS, 30e6, false), r);
    assert_eq!(echo(&mut s, t + 2 * RTT, 30 * MS, 30e6, false), r);
    let r2 = echo(&mut s, t + 2 * RTT + 1, 40 * MS, 30e6, false);
    assert!(near(r2, BETA * r), "{r2}"); // f × delivery = 18.75 Mbit/s is above 0.85 × rate
    assert_eq!(echo(&mut s, t + 2 * RTT + RTT / 2, 50 * MS, 30e6, false), r2);
    assert!(echo(&mut s, t + 5 * RTT + 1, 40 * MS, 30e6, false) < r2); // 3 RTTs on
    // Bandwidth runs at bandwidth and keeps the spec's once per RTT.
    let mut s = bulk(Mode::Bandwidth, 40e6);
    s.lanes[A].delivery = 25e6;
    echo(&mut s, T0, 30 * MS, 40e6, false);
    let r = echo(&mut s, T0 + 20 * MS, 30 * MS, 40e6, false);
    assert!(near(r, 25e6 * (1.0 - 10.0 / 60.0)), "{r}");
    assert!(echo(&mut s, T0 + 20 * MS + RTT, 30 * MS, 40e6, false) < r);
}

#[test]
fn adjust_cuts_only_a_lane_carrying_bulk_it_has_a_sample_of() {
    // Not carrying bulk: overuse makes the lane unclean but leaves its rate.
    let mut s = bulk(Mode::Performance, 40e6);
    (s.nspread, s.lanes[A].delivery) = (0, 1e6);
    echo(&mut s, T0, 30 * MS, 0.0, false);
    assert_eq!(echo(&mut s, T0 + 20 * MS, 30 * MS, 0.0, false), 40e6);
    assert_eq!((s.lanes[A].last_over, s.lanes[A].clean_at), (T0 + 20 * MS, NEVER));
    // Took bulk less than RTT + RATE_WINDOW ago: its delivery is not yet of that bulk.
    let mut s = bulk(Mode::Performance, 40e6);
    (s.lanes[A].took_at, s.lanes[A].delivery) = (T0 - RTT - RATE_WINDOW + 20 * MS + 1, 1e6);
    echo(&mut s, T0, 30 * MS, 30e6, false);
    assert_eq!(echo(&mut s, T0 + 20 * MS, 30 * MS, 30e6, false), 40e6);
    assert_eq!(echo(&mut s, T0 + 21 * MS, 30 * MS, 30e6, false), PACE_FLOOR);
}

#[test]
fn adjust_loss_alone_keeps_the_lanes_room() {
    // (rate, loss, delivery) with 30 Mbit/s offered and no queue → the rate after a lossy echo.
    let lossy = |rate: f64, loss: f64, delivery: f64| {
        let mut s = bulk(Mode::Performance, rate);
        (s.lanes[A].loss, s.lanes[A].delivery) = (loss, delivery);
        echo(&mut s, T0, 0, 30e6, true)
    };
    // Light loss (a radio's): no lower than twice the offered rate.
    assert_eq!(lossy(80e6, 0.04, 10e6), 60e6);
    assert_eq!(lossy(50e6, 0.04, 10e6), 50e6);
    // Heavy loss on a lane that delivers the offered rate: no lower than that.
    assert_eq!(lossy(80e6, 0.10, 29e6), 30e6);
    // A policer, delivering less: cut to delivery.
    assert_eq!(lossy(80e6, 0.10, 20e6), 20e6);
    // Exactly 5% is heavy, as Go's constant 1 − TrialPass makes it.
    assert_eq!(lossy(80e6, 1.0 - 95.0 / 100.0, 10e6), 10e6);
    assert_eq!(lossy(80e6, 1.0 - 96.0 / 100.0, 10e6), 60e6);
    // Light loss with a queue counts as a queue: no lower than the offered rate it delivers.
    let mut s = bulk(Mode::Performance, 80e6);
    (s.lanes[A].loss, s.lanes[A].delivery) = (0.01, 29e6);
    echo(&mut s, T0, 30 * MS, 30e6, false);
    assert_eq!(echo(&mut s, T0 + 20 * MS, 30 * MS, 30e6, true), 30e6);
}

#[test]
fn adjust_raises_only_while_the_rate_binds() {
    let mut s = bulk(Mode::Performance, 40e6);
    // App-limited at 30 Mbit/s with nothing queued: no increase.
    assert_eq!(echo(&mut s, T0, 0, 30e6, false), 40e6);
    // A backlog with the rate under 2 × offered binds: × STARTUP_GAIN once the queue drained.
    s.lanes[A].q.push_back(pkt(1, T0, 1500));
    assert_eq!(echo(&mut s, T0 + RTT, 0, 30e6, false), 40e6 * STARTUP_GAIN);
    assert_eq!(s.lanes[A].last_inc, T0 + RTT);
    assert_eq!(echo(&mut s, T0 + RTT + RTT / 2, 0, 30e6, false), 50e6); // once per RTT
    assert_eq!(echo(&mut s, T0 + 2 * RTT, 0, 30e6, false), 62.5e6);
    assert_eq!(echo(&mut s, T0 + 3 * RTT, 0, 30e6, false), 62.5e6); // at 2 × offered: no more
    // A queue not drained (qd at half the target or more): + INCREASE_STEP.
    let r = echo(&mut s, T0 + 4 * RTT, TARGET_FLOOR / 2, 40e6, false);
    assert!(near(r, 62.5e6 * (1.0 + INCREASE_STEP)), "{r}");
    // Not within an RTT of an overuse.
    s.lanes[A].last_over = T0 + 5 * RTT - 1;
    assert_eq!(echo(&mut s, T0 + 6 * RTT - 2, 0, 60e6, false), r);
    assert!(echo(&mut s, T0 + 6 * RTT - 1, 0, 60e6, false) > r);
    // Unlimited never grows; offered at or above the rate binds without a backlog.
    let mut s = bulk(Mode::Performance, INF);
    assert_eq!(echo(&mut s, T0, 0, 30e6, false), INF);
    let mut s = bulk(Mode::Performance, 20e6);
    assert_eq!(echo(&mut s, T0, 0, 20e6, false), 25e6);
}

#[test]
fn adjust_combine_climbs_back_to_regain_after_startup() {
    // Startup from PACE_FLOOR: × STARTUP_GAIN per RTT while offered keeps up. A shares the
    // spread, as A4's hotel shares it with the cell (a lane alone: the next test).
    let mut s = bulk(Mode::Bandwidth, 0.0);
    share(&mut s);
    s.restart(T0, A);
    assert_eq!(s.lanes[A].rate, PACE_FLOOR);
    assert_eq!(echo(&mut s, T0, 0, PACE_FLOOR, false), PACE_FLOOR * STARTUP_GAIN);
    // Startup overshot to 72.8 Mbit/s on a 37 Mbit/s lane: the cut, sized by a 100 ms queue,
    // leaves it far below what it delivered (notes/core.md 50a).
    s.lanes[A].rate = 72.8e6;
    s.lanes[A].delivery = 37e6;
    echo(&mut s, T0 + RTT, 100 * MS, 72.8e6, false);
    let r = echo(&mut s, T0 + 2 * RTT, 100 * MS, 72.8e6, false);
    assert!(near(r, 0.6 * 37e6) && near(s.lanes[A].regain, BETA * 37e6), "{r}");
    // Once drained, × STARTUP_GAIN per RTT up to regain, then + INCREASE_STEP.
    let r1 = echo(&mut s, T0 + 3 * RTT, 0, 40e6, false);
    assert!(near(r1, r * STARTUP_GAIN), "{r1}");
    let r2 = echo(&mut s, T0 + 4 * RTT, 0, 40e6, false);
    assert!(near(r2, BETA * 37e6) && s.lanes[A].regain == 0.0, "{r2}");
    let r3 = echo(&mut s, T0 + 5 * RTT, 0, 40e6, false);
    assert!(near(r3, r2 * (1.0 + INCREASE_STEP)), "{r3}");
    // Not drained: + INCREASE_STEP even with regain pending.
    let mut s = bulk(Mode::Bandwidth, 20e6);
    s.lanes[A].regain = 30e6;
    let r = echo(&mut s, T0, TARGET_FLOOR / 2, 20e6, false);
    assert!(near(r, 20e6 * (1.0 + INCREASE_STEP)) && s.lanes[A].regain == 30e6, "{r}");
    // Bandwidth never climbs on a backlog alone: only offered at or above the rate binds.
    let mut s = bulk(Mode::Bandwidth, 20e6);
    s.lanes[A].q.push_back(pkt(1, T0, 1500));
    assert_eq!(echo(&mut s, T0, 0, 15e6, false), 20e6);
}

/// A bandwidth spread of one lane carries the whole stream, as stable's primary does: earliest
/// arrival has no other lane to hand bulk to, so its offered rate is the stream's, not one its own
/// pace draws. It starts as stable's primary, unlimited from its first binding increase until its
/// first overuse, and once a cut has drained it climbs back by STARTUP_GAIN per RTT while it
/// binds, that is while the stream's offered rate reaches it; a lane that shares the spread keeps
/// bandwidth's startup and INCREASE_STEP (swarm seeds 13, 35, 79, 82, 98, 104 and 107;
/// notes/rust.md, harden: integration).
#[test]
fn adjust_combine_lane_alone_starts_and_climbs_as_stable() {
    // Restarted alone (the Tailscale lane under a hard NAT, back up after a stall), offered the
    // 21 Mbit/s stream: unlimited at once, so it paces at twice the offered rate.
    let mut s = bulk(Mode::Bandwidth, 0.0);
    s.restart(T0, A);
    assert_eq!(echo(&mut s, T0, 0, 21e6, false), INF);
    assert!(near(s.lanes[A].pace(T0), 42e6), "{}", s.lanes[A].pace(T0));
    // Its first overuse, a loss burst of 6% with no queue, ends that: down to what it delivers,
    // at least the offered rate it carries.
    (s.lanes[A].loss, s.lanes[A].delivery) = (0.06, 20e6);
    assert_eq!(echo(&mut s, T0 + RTT, 0, 21e6, true), 21e6);
    assert!(!s.lanes[A].startup);
    // Sharing the spread, startup climbs by STARTUP_GAIN from PACE_FLOOR as before.
    let mut s = bulk(Mode::Bandwidth, 0.0);
    share(&mut s);
    s.restart(T0, A);
    assert_eq!(echo(&mut s, T0, 0, 21e6, false), PACE_FLOOR * STARTUP_GAIN);
    // Alone at a startup rate above the offered one (the cell once A17 bars the hotel): it does
    // not bind, so nothing changes.
    let mut s = bulk(Mode::Bandwidth, 37.25e6);
    s.lanes[A].startup = true;
    assert_eq!(echo(&mut s, T0, 0, 32.6e6, false), 37.25e6);

    // Out of startup at 20 Mbit/s, a loss burst of 6% with no queue on a lane that delivers 18 of
    // the 22 offered cuts it to BETA × rate; once the loss has passed and the queue is drained,
    // the lane alone climbs back by STARTUP_GAIN per RTT until its rate passes the offered one.
    let climb = |shared: bool, qd: Mono| {
        let mut s = bulk(Mode::Bandwidth, 20e6);
        if shared {
            share(&mut s);
        }
        (s.lanes[A].loss, s.lanes[A].delivery) = (0.06, 18e6);
        assert!(near(echo(&mut s, T0, 0, 22e6, true), BETA * 20e6));
        s.lanes[A].loss = 0.0;
        [1, 2, 3].map(|i| echo(&mut s, T0 + i * RTT, qd, 22e6, false))
    };
    let alone = climb(false, 0);
    let (a, b) = (BETA * 20e6 * STARTUP_GAIN, BETA * 20e6 * STARTUP_GAIN * STARTUP_GAIN);
    assert!(near(alone[0], a) && near(alone[1], b) && alone[2] == alone[1], "{alone:?}");
    // Sharing the spread, or alone with a queue not drained (qd at half the target or more):
    // + INCREASE_STEP.
    for (shared, qd) in [(true, 0), (false, TARGET_FLOOR / 2)] {
        let r = climb(shared, qd);
        let want = BETA * 20e6 * (1.0 + INCREASE_STEP).powi(3);
        assert!(near(r[2], want), "shared {shared}, qd {qd}: {r:?}");
    }
}

#[test]
fn drain_holds_at_most_pace_burst_plus_a_packet() {
    let mut s = bulk(Mode::Performance, INF);
    s.lanes[A].validated = false; // drains nothing: only the bucket fills
    // At PACE_FLOOR a millisecond is 625 bytes: the bucket holds two full-size packets.
    s.drain(&mut Acts::default(), T0, A);
    assert_eq!(s.lanes[A].tokens, 2.0 * MAX_PACKET as f64);
    // At 80 Mbit/s, a millisecond's 10 kB plus one packet, however late the tick.
    s.lanes[A].rate = 80e6;
    offer(&mut s, T0 + SEC, 50e6);
    s.drain(&mut Acts::default(), T0 + SEC, A);
    assert!(near(s.lanes[A].tokens, 10_000.0 + MAX_PACKET as f64), "{}", s.lanes[A].tokens);
    // Credit accrues at pace.
    starve(&mut s, T0 + SEC);
    s.drain(&mut Acts::default(), T0 + SEC + MS / 2, A);
    assert!(near(s.lanes[A].tokens, 5_000.0), "{}", s.lanes[A].tokens);
    assert_eq!(s.lanes[A].filled, T0 + SEC + MS / 2);
}

#[test]
fn drain_ticks_at_the_first_millisecond_after_the_head_is_due() {
    let mut s = bulk(Mode::Performance, INF);
    s.lanes[A].q.push_back(pkt(1, T0, 1471));
    s.lanes[A].q_bytes = 1471;
    // At PACE_FLOOR, 1,471 bytes of credit take 2.35 ms.
    starve(&mut s, T0);
    s.next = NEVER;
    let mut out = Acts::default();
    s.drain(&mut out, T0, A);
    assert_eq!((s.next, s.lanes[A].q.len(), out.0.len()), (T0 + 3 * MS, 1, 0));
    // Due on a millisecond: the tick comes after it, never at now again (notes/core.md 22).
    (s.lanes[A].tokens, s.next) = (221.0, NEVER); // 1,250 bytes short: 2 ms
    s.drain(&mut out, T0, A);
    assert_eq!((s.next, s.lanes[A].q.len(), out.0.len()), (T0 + 3 * MS, 1, 0));
}

#[test]
fn drain_drops_what_waited_past_queue_age() {
    let mut s = bulk(Mode::Performance, INF);
    for (c, at) in [(1, T0 - QUEUE_AGE - 1), (2, T0 - QUEUE_AGE - 1), (3, T0 - QUEUE_AGE), (4, T0)] {
        s.lanes[A].q.push_back(pkt(c, at, 1471));
        s.lanes[A].q_bytes += 1471;
    }
    starve(&mut s, T0);
    let mut out = Acts::default();
    s.drain(&mut out, T0, A);
    let ln = &s.lanes[A];
    assert_eq!((ln.q.len(), ln.q[0].c, ln.q_bytes, out.0.len()), (2, 3, 2 * 1471, 0));
}

/// An unvalidated or punching lane drains nothing; a suspect or down one only while the scheduler
/// gives it bulk: a lane goes down at DOWN_AFTER even while blind, and the blind pair's lanes carry
/// bulk until echoes return (§11.4).
#[test]
fn drain_sends_nothing_unvalidated_nor_suspect_without_bulk() {
    for (validated, state, prim, sends) in [
        (false, State::Up, Some(A), false),
        (true, State::Suspect, None, false),
        (true, State::Down, None, false),
        (true, State::Punching, Some(A), false),
        (true, State::Suspect, Some(A), true),
        (true, State::Down, Some(A), true),
    ] {
        let mut s = bulk(Mode::Performance, INF);
        (s.lanes[A].validated, s.lanes[A].state, s.nspread) = (validated, state, usize::from(prim.is_some()));
        s.lanes[A].q.push_back(pkt(1, T0, 1471));
        s.lanes[A].q_bytes = 1471;
        let mut out = Acts::default();
        s.drain(&mut out, T0, A); // with a full bucket
        // Drained when it sends: this packet has no buffer, so nothing goes out either way.
        let want = if sends { (0, 0) } else { (1, 0) };
        assert_eq!((s.lanes[A].q.len(), out.0.len()), want, "validated {validated}, {state:?}");
    }
}

#[test]
fn keep_and_evict_bound_the_rewind_ring() {
    let mut s = bulk(Mode::Performance, INF);
    // Suspect timeout 40 + 20 + 20 ms plus the RTT is under REWIND_WINDOW: 300 ms.
    assert!(s.suspect_after(A) + RTT < REWIND_WINDOW);
    s.keep(T0, A, pkt(1, 0, 0)); // a small packet's counter
    assert_eq!((s.lanes[A].rw.len(), s.lanes[A].rw[0].at), (1, T0));
    s.keep(T0 + REWIND_WINDOW, A, pkt(2, 0, 0));
    assert_eq!(s.lanes[A].rw.len(), 2);
    s.keep(T0 + REWIND_WINDOW + 1, A, pkt(3, 0, 0));
    assert_eq!((s.lanes[A].rw.len(), s.lanes[A].rw[0].c), (2, 2));
    // A slower lane keeps its suspect timeout plus RTT: 200 + 20 + 20 + 200 ms.
    s.lanes[A].rtt = 200 * MS;
    let w = s.suspect_after(A) + 200 * MS;
    assert_eq!(w, 440 * MS);
    s.keep(T0 + REWIND_WINDOW + w, A, pkt(4, 0, 0)); // 2 is 440 ms old: still kept
    assert_eq!((s.lanes[A].rw.len(), s.lanes[A].rw[0].c), (3, 2));
    s.evict(T0 + REWIND_WINDOW + w + 1, A);
    assert_eq!((s.lanes[A].rw.len(), s.lanes[A].rw[0].c), (2, 3));
    s.evict(T0 + REWIND_WINDOW + w + 2, A);
    assert_eq!((s.lanes[A].rw.len(), s.lanes[A].rw[0].c), (1, 4));
    // At most as many entries as the pool has buffers.
    for c in 0..MAX_BUFS as u64 + 5 {
        s.keep(T0 + 2 * SEC, A, pkt(c, 0, 0));
    }
    assert_eq!((s.lanes[A].rw.len(), s.lanes[A].rw[0].c), (MAX_BUFS, 5));
}

/// The eviction as it was before it worked its window out lazily: the window first, on every
/// call. The reference for `evict`.
fn evict_eager(s: &mut Session, now: Mono, l: LaneIx) {
    let w = REWIND_WINDOW.max(s.suspect_after(l) + s.lanes[l].rtt);
    while let Some(p) = s.lanes[l].rw.front().copied()
        && (now - p.at > w || s.lanes[l].rw.len() > MAX_BUFS)
    {
        s.lanes[l].rw.pop_front();
        if let Some(b) = p.buf {
            s.pool.put(b);
        }
    }
}

/// `keep` and `evict` keep what the eager eviction keeps, on two sessions given the same random
/// packets and lane state: packets at one instant, microseconds apart or after gaps of seconds,
/// evictions on their own later on, RTTs that leave the window at REWIND_WINDOW or make it
/// longer (none: DOWN_AFTER), a largest RTT of DEV_WINDOW above the smoothed one, and bursts
/// past MAX_BUFS. Both rings hold a suffix of the same packets, so equal lengths are equal rings.
#[test]
fn evict_keeps_what_the_eager_eviction_keeps() {
    let draw = |g: &mut DetRand, n: u64| (g.next() >> 32) % n;
    let (mut lazy, mut longer) = (0, 0);
    for seed in 0..64 {
        let mut g = DetRand(seed);
        let (mut a, mut b) = (bulk(Mode::Performance, INF), bulk(Mode::Performance, INF));
        let (mut t, mut c) = (T0, 0);
        for step in 0..2_000 {
            if draw(&mut g, 40) == 0 {
                let rtt = [0, 20 * MS, RTT, 200 * MS, 900 * MS][draw(&mut g, 5) as usize];
                let dev = draw(&mut g, 100) as Mono * MS;
                for s in [&mut a, &mut b] {
                    (s.lanes[A].rtt, s.lanes[A].rttdev) = (rtt, dev);
                }
            }
            if draw(&mut g, 20) == 0 {
                let v = draw(&mut g, 1_500) as Mono * MS;
                for s in [&mut a, &mut b] {
                    s.lanes[A].rtts.add(t, v);
                }
            }
            t += match draw(&mut g, 20) {
                0 => 0,
                1 => draw(&mut g, 3_000) as Mono * MS,
                _ => draw(&mut g, 2_000) as Mono * 1_000,
            };
            let n = if draw(&mut g, 400) == 0 { MAX_BUFS as u64 + 3 } else { 1 + draw(&mut g, 4) };
            for _ in 0..n {
                c += 1;
                (a.now, b.now) = (t, t);
                a.keep(t, A, pkt(c, 0, 0));
                b.lanes[A].rw.push_back(pkt(c, t, 0)); // keep, with the eager eviction
                evict_eager(&mut b, t, A);
            }
            if draw(&mut g, 4) == 0 {
                let e = t + draw(&mut g, 1_000) as Mono * MS;
                (a.now, b.now) = (e, e);
                a.evict(e, A);
                evict_eager(&mut b, e, A);
            }
            let (ra, rb) = (&a.lanes[A].rw, &b.lanes[A].rw);
            assert_eq!(
                (ra.len(), ra.front(), ra.back()),
                (rb.len(), rb.front(), rb.back()),
                "seed {seed}, step {step}"
            );
            match ra.front() {
                Some(p) if a.now - p.at <= REWIND_WINDOW => lazy += 1,
                Some(_) => longer += 1,
                None => {}
            }
        }
        assert_eq!(a.lanes[A].rw, b.lanes[A].rw, "seed {seed}");
    }
    // Both ways ran: rings left with no entry older than REWIND_WINDOW, and rings whose longer
    // window kept older ones.
    assert!(lazy > 10_000 && longer > 10_000, "{lazy} and {longer}");
}
