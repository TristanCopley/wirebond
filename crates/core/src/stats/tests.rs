//! Tests of stats.rs (§11.5): first arrivals, rescues and their ring, reorder wait percentiles,
//! bytes and share.

use super::{RESCUE_RING, WAIT_STEP};
use crate::lane::{Lane, LaneIx};
use crate::pair::*;
use crate::session::Mode;
use crate::{MS, Mono, RESCUE_GAP, Role, Session};

/// A bare session with `n` lanes, ids 1…n, in `mode`.
fn session(n: usize, mode: Mode) -> Session {
    let mut s = Session::new(Role::Anchor, 12, DetRand::rand(12));
    s.mode = mode;
    for i in 0..n {
        s.lanes.live.push(Lane::new(i as u16 + 1, i as u16 + 2));
    }
    s
}

/// A data packet with counter `c` on lane `l` at `now`, first copy or not, as `packet` notes it.
fn on(s: &mut Session, now: Mono, l: LaneIx, c: u64, first: bool) {
    s.lanes[l].rx_last = c;
    s.arrived(now, l, c, first, 1_000);
}

#[test]
fn first_arrivals_and_rescues() {
    let (a, b) = (LaneIx(0), LaneIx(1));
    let mut s = session(2, Mode::Performance);
    // 1: a first, b's copy RESCUE_GAP − 1 ns later: no rescue.
    on(&mut s, 0, a, 1, true);
    on(&mut s, RESCUE_GAP - 1, b, 1, false);
    // 2: a first, b's copy RESCUE_GAP later: a rescue for a.
    on(&mut s, 100 * MS, a, 2, true);
    on(&mut s, 100 * MS + RESCUE_GAP, b, 2, false);
    // A third copy changes nothing.
    on(&mut s, 200 * MS, b, 2, false);
    // 3: b first, a's copy close behind.
    on(&mut s, 300 * MS, b, 3, true);
    on(&mut s, 301 * MS, a, 3, false);
    let st = |s: &Session, l: LaneIx| (s.lanes[l].stats.firsts, s.lanes[l].stats.rescues);
    assert_eq!((st(&s, a), st(&s, b)), ((2, 1), (1, 0)));
    // 4: a first and b's copy never comes, while b carries later packets: a rescue once the ring
    // reuses its slot.
    on(&mut s, 400 * MS, a, 4, true);
    for c in 5..4 + RESCUE_RING as u64 {
        on(&mut s, 400 * MS, b, c, true);
    }
    assert_eq!(st(&s, a), (3, 1), "still within the ring");
    on(&mut s, 500 * MS, b, 4 + RESCUE_RING as u64, true);
    assert_eq!(st(&s, a), (3, 2), "no copy, and b passed it: a rescue");
    // 5: a packet no other lane carried after it (one lane alone) is no rescue.
    let mut s = session(2, Mode::Performance);
    on(&mut s, 0, a, 1, true);
    on(&mut s, MS, a, 1 + RESCUE_RING as u64, true);
    assert_eq!(st(&s, a), (2, 0));
    // Bandwidth counts bytes only.
    let mut s = session(2, Mode::Bandwidth);
    on(&mut s, 0, a, 1, true);
    on(&mut s, RESCUE_GAP, b, 1, false);
    assert_eq!((st(&s, a), s.lanes[a].stats.rx_bytes, s.lanes[b].stats.rx_bytes), ((0, 0), 1_000, 1_000));
}

#[test]
fn reorder_wait_percentiles() {
    let mut s = session(2, Mode::Bandwidth);
    // 98 packets at once, one held 3.2 ms and one 40 ms, all on lane 1.
    for _ in 0..98 {
        s.waited(Some(1), 0);
    }
    s.waited(Some(1), 3_200_000);
    s.waited(Some(1), 40 * MS);
    s.waited(Some(9), 7 * MS); // a lane that went: nothing
    let x = &s.lanes[LaneIx(0)].stats;
    assert_eq!((x.wait(0.5), x.wait(0.99), x.wait(1.0)), (0, 7 * WAIT_STEP, 40 * MS));
    assert_eq!(s.lanes[LaneIx(1)].stats.wait(0.99), 0, "no samples");
    // Performance keeps none.
    s.mode = Mode::Performance;
    s.waited(Some(2), 40 * MS);
    assert!(s.lanes[LaneIx(1)].stats.waits.is_empty());
}

/// End to end in the hotel pair, performance: the receiver counts first arrivals on both direct
/// lanes and the bytes each carried; status shows them, with shares summing to 1.
#[test]
fn status_shows_contribution() {
    let mut p = hotel();
    let (mut video, mut audio, mut input) = p.moonlight(10.0);
    p.play(2 * crate::SEC, &mut [&mut video, &mut audio, &mut input]);
    let st = p.c.s.snapshot();
    let firsts: u64 = st.lanes.iter().map(|l| l.first_arrivals).sum();
    assert!(firsts > 1_000, "first arrivals {firsts}");
    let wifi = st.lanes.iter().find(|l| l.id == 0x0101).expect("the Wi-Fi lane");
    assert!(wifi.first_arrivals > 0 && wifi.rx_bytes > 0 && wifi.tx_bytes > 0, "{wifi:?}");
    let share: f64 = st.lanes.iter().map(|l| l.share).sum();
    assert!((share - 1.0).abs() < 1e-9, "shares sum to {share}");
}
