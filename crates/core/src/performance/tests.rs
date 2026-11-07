//! Tests of performance.rs on hand-built sessions: the spread has no skew limit, every lane of it
//! gets every bulk packet, and a lane that leaves drops its queue, unrewound. The pair tests
//! (tests/pair.rs) drive whole sessions in performance.

use std::net::SocketAddr;

use crate::buf::Pkt;
use crate::estimate::{NO_MIN, Winmin};
use crate::lane::{Lane, LaneIx, State};
use crate::pair::*;
use crate::session::Mode;
use crate::{MS, Mono, Role, SEC, Session};

/// A server session in `mode`, active, with hand-built lanes `(id, group, base OWD ms)`, each up,
/// validated and echoed 5 ms before `now`; id 0 is the Tailscale lane.
fn session(mode: Mode, now: Mono, lanes: &[(u16, i32, Mono)]) -> Session {
    let mut s = Session::new(Role::Anchor, 1, DetRand::rand(1));
    (s.active, s.mode, s.rid, s.last_echo) = (true, mode, 1, now - 5 * MS);
    for (i, &(id, group, owd)) in lanes.iter().enumerate() {
        let mut l = Lane::new(id, if id == 0 { 1 } else { 11 });
        (l.mesh, l.group, l.validated, l.state, l.peer) = (id == 0, group, true, State::Up, Some(peer(i)));
        (l.owd, l.owd_now, l.rtt, l.last_echo) = ((owd + 2) * MS, (owd + 2) * MS, 2 * (owd + 2) * MS, now - 5 * MS);
        l.min_owd = Winmin { at: 0, cur: owd * MS, old: NO_MIN };
        s.lanes.live.push(l);
    }
    s
}

fn peer(i: usize) -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 10 + i as u8], 7849))
}

fn ixs(s: &Session) -> Vec<usize> {
    s.spread().iter().map(|l| l.get()).collect()
}

/// One lane per group, the Tailscale lane out while its group's direct lane is up, and no skew
/// limit: the iPhone 200 ms slower still races (bandwidth leaves it out past BANDWIDTH_SKEW_MAX).
#[test]
fn every_group_races_whatever_its_delay() {
    let now = 30 * SEC;
    let lanes = [(0, 0, 33), (0x0101, 0, 32), (0x0102, 0, 34), (0x0201, 1, 232)];
    let mut out = Acts::default();
    let mut s = session(Mode::Performance, now, &lanes);
    s.schedule(&mut out, now);
    assert_eq!(ixs(&s), [1, 3]);
    let mut c = session(Mode::Bandwidth, now, &lanes);
    c.schedule(&mut out, now);
    assert_eq!(ixs(&c), [1]);
}

/// Every bulk packet goes on every lane of the spread, fastest first; bandwidth picks one.
#[test]
fn every_packet_on_every_lane() {
    let now = 30 * SEC;
    let lanes = [(0x0101, 0, 32), (0x0201, 1, 45)];
    let mut out = Acts::default();
    for (mode, want) in [(Mode::Performance, vec![0, 1]), (Mode::Bandwidth, vec![0])] {
        let mut s = session(mode, now, &lanes);
        s.schedule(&mut out, now);
        assert_eq!(s.bulk_lanes(now, 1200).map(LaneIx::get).collect::<Vec<_>>(), want, "{mode:?}");
    }
}

/// A lane that falls silent while another answers leaves the spread and drops its queue: the
/// lane that stays carries the same packets, so nothing is rewound or moved onto it.
#[test]
fn a_leaving_lane_drops_its_queue() {
    let now = 30 * SEC;
    let mut s = session(Mode::Performance, now, &[(0x0101, 0, 32), (0x0201, 1, 45)]);
    let mut out = Acts::default();
    s.schedule(&mut out, now);
    let (wifi, phone) = (LaneIx::at(0), LaneIx::at(1));
    for c in 100..103 {
        let b = s.pool.get().unwrap();
        let x = &mut s.lanes[wifi];
        x.q.push_back(Pkt { c, at: now - MS, buf: Some(b), n: 1435 });
        x.q_bytes += 1435;
    }
    let w = &mut s.lanes[wifi];
    (w.state, w.last_echo) = (State::Suspect, now - 150 * MS);
    s.schedule(&mut out, now);
    assert_eq!((ixs(&s), s.rewinds), (vec![1], 0));
    assert!(s.lanes[wifi].q.is_empty() && s.lanes[phone].q.is_empty() && out.0.is_empty());
}
