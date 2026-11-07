//! Tests of status.rs. Owner: C2. Ports from core/status_test.go: TestSnapshot, TestDeterminism
//! (compares `Pair::wire` of two runs, one of them reading snapshots). The Rust `Problem` has no
//! `mbps`: TestSnapshot checks the number in the message instead.
//!
//! The ports and the R2-06, R2-14 pair and problems tests drive whole sessions; the others need
//! only status.rs. The new pair and problems tests were checked against Go's core with the same
//! fixes: they fail without them.

use std::collections::BTreeMap;

use crate::lane::{Lane, LaneIx, State};
use crate::pair::*;
use crate::{BOND_BYTES, MS, Mono, Role, SEC, STATUS_EVENTS, Session};

/// Go: TestSnapshot, in performance. After video outgrows the hotel Wi-Fi, status shows both
/// groups' lanes carrying bulk, the flows' classes, the metered rate (every packet crosses the
/// iPhone) and no problem (§14.2–§14.4).
#[test]
fn snapshot() {
    let mut p = hotel();
    let w = p.paths.get_mut(&(S_ETH, C_WIFI)).expect("the hotel Wi-Fi path");
    (w.rate, w.buf) = (20e6, 180 * MS);
    let (mut video, mut audio, mut input) = p.moonlight(10.0);
    p.play(3 * SEC, &mut [&mut video, &mut audio, &mut input]);
    video.gap /= 3;
    p.play(3 * SEC, &mut [&mut video, &mut audio, &mut input]);
    let st = p.s.s.snapshot();
    let roles: BTreeMap<u16, &str> = st.lanes.iter().map(|l| (l.id, l.out.role.as_str())).collect();
    let want = BTreeMap::from([(0, "standby"), (0x0101, "spread"), (0x0201, "spread")]);
    assert_eq!(roles, want, "roles");
    // Flows by direction and key (§13): the server sends video (bulk) and audio and receives
    // input; `pair::inner` puts the test flow in the source port.
    let classes: BTreeMap<(bool, u16), &str> =
        st.flows.iter().map(|f| ((f.tx, f.key.map_or(0, |k| k.sport)), f.class)).collect();
    let want = BTreeMap::from([((true, 1), "bulk"), ((true, 2), "small"), ((false, 3), "small")]);
    assert_eq!(classes, want, "classes");
    let ids: Vec<(bool, u16)> = st.flows.iter().map(|f| (f.tx, f.flow.0)).collect();
    assert!(ids.contains(&(true, 1)) && ids.contains(&(true, 2)) && ids.contains(&(false, 1)), "numbers {ids:?}");
    // 10 Mbps for 3 s and 30 Mbps for 3 s of the last minute on the iPhone: 2 Mbps over it, 0.9
    // GB/h, and a little more with the packets' overhead.
    assert!((0.85..=1.2).contains(&st.metered_gb_per_hour), "metered {:.2} GB/h", st.metered_gb_per_hour);
    assert!(st.problems.is_empty(), "problems {:?}", st.problems);
    assert!(
        st.events.len() <= STATUS_EVENTS && st.bytes > 0 && st.bytes <= BOND_BYTES,
        "events {:?}, bytes {}",
        st.events,
        st.bytes
    );
}

/// Go: TestDeterminism. The same events, times and rand stream give the same packets, and reading
/// a snapshot changes nothing (netsim compares two runs of a seed).
#[test]
fn determinism() {
    let run = |snap: bool| {
        let mut p = hotel();
        let (mut video, mut audio, mut input) = p.moonlight(10.0);
        for i in 0..60 {
            if i == 20 || i == 40 {
                p.paths.get_mut(&(S_ETH, C_WIFI)).expect("the hotel Wi-Fi path").down = i == 20;
            }
            p.play(50 * MS, &mut [&mut video, &mut audio, &mut input]);
            if snap {
                p.s.s.snapshot();
                p.c.s.snapshot();
            }
        }
        p.wire
    };
    let (a, b) = (run(false), run(true));
    assert_eq!(a.len(), b.len(), "{} packets, then {}", a.len(), b.len());
    let packet = |it: &Item| match &it.ev {
        Ev::Packet { sock, data, .. } => Some((it.dst, *sock, data.clone())),
        _ => None,
    };
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        assert!(packet(x).is_some() && packet(x) == packet(y), "packet {i} differs");
    }
}

/// R2-06: the client sends only small input, so none of its lanes is spread, although it has a
/// spread chosen while the session is active; the server, which sends the video, has one.
#[test]
fn roles_follow_the_bulk() {
    let mut p = hotel();
    let (mut video, mut audio, mut input) = p.moonlight(10.0);
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    let roles = |s: &Session| s.snapshot().lanes.into_iter().map(|l| l.out.role).collect::<Vec<_>>();
    let (c, s) = (roles(&p.c.s), roles(&p.s.s));
    assert!(p.c.s.nspread > 0, "the client has no spread to hide");
    assert!(c.iter().all(|r| r == "copies" || r == "standby") && c.iter().any(|r| r == "copies"), "client {c:?}");
    assert!(s.iter().any(|r| r == "spread"), "server {s:?}");
}

/// A session with a spread, a bulk flow and these lanes (id, state, OWD), offering 30 Mbps of
/// bulk on the first lane.
fn bulk_on(lanes: &[(u16, State, Mono)]) -> Session {
    let mut s = Session::new(Role::Anchor, 1, DetRand::rand(2));
    s.now = 600 * SEC;
    for &(id, state, owd) in lanes {
        let mut l = Lane::new(id, 10);
        (l.state, l.validated, l.remote) = (state, true, format!("L{id:x}"));
        l.owd = owd;
        s.lanes.live.push(l);
    }
    (s.spread[0], s.nspread) = (LaneIx(0), 1);
    let o = &mut s.lanes.live[0].offered;
    (o.at, o.cur, o.prev) = (s.now, 0.0, 30e6 / 8.0 / 10.0); // 30 Mbps over RATE_WINDOW
    s.flows.push(crate::flow::Flow { used: true, mine: true, id: crate::FlowId(1), ..Default::default() });
    s
}

/// The engine raises no problem: §17's are the daemon's, and 1.x's bulk_exceeds_every_lane is
/// gone with fits learning (§25), whatever the lanes carry.
#[test]
fn problems() {
    let codes = |s: &Session| s.problems(s.now).into_iter().map(|p| (p.code, p.message)).collect::<Vec<_>>();
    let s = bulk_on(&[(0x0101, State::Down, 50 * MS), (0x0201, State::Dormant, 30 * MS)]);
    assert_eq!(codes(&s), []);
    let s = bulk_on(&[(0x0101, State::Up, 50 * MS), (0x0201, State::Suspect, 30 * MS)]);
    assert_eq!(codes(&s), [], "bulk above every lane");
    let s = bulk_on(&[(0x0101, State::Up, 50 * MS), (0x0201, State::Up, 30 * MS)]);
    assert_eq!(codes(&s), [], "one lane carries the stream");
}

/// R2-14 end to end: the iPhone lane cannot punch, so it never has an OWD sample, and the others
/// are relative to the faster of them (Wi-Fi, 32 ms), not to its OWD 0. The pair's two sides share
/// one clock, so Go showed each lane's whole OWD here (33 and 32 ms); across two devices' clocks it
/// showed their offset.
#[test]
fn owd_relative_with_an_unpunched_lane() {
    let mut p = Pair::new(33 * MS);
    p.add_link(Side::C, 1, "Wi-Fi", false, true, C_WIFI);
    p.add_link(Side::C, 2, "iPhone", true, false, C_PHONE);
    p.add_link(Side::S, 1, "Ethernet", false, false, S_ETH);
    p.link(C_WIFI, S_ETH, Path { delay: 32 * MS, ..Path::default() });
    p.start();
    p.run(3 * SEC);
    for n in [&p.c, &p.s] {
        let rel: BTreeMap<u16, f64> = n.s.snapshot().lanes.iter().map(|l| (l.id, l.out.owd_rel_ms)).collect();
        let iphone = n.s.lane_by_id(0x0201).map(|l| n.s.lanes[l].state);
        assert!(
            iphone == Some(State::Punching)
                && rel[&0x0201] == 0.0
                && rel[&0x0101] == 0.0
                && (0.5..=1.5).contains(&rel[&0]),
            "{:?}: {rel:?}",
            n.role
        );
    }
}

/// R2-14: OWDs carry the offset between the two devices' clocks, so the lanes without an OWD
/// sample (OWD 0) set no base and show 0, whichever way the clocks differ.
#[test]
fn owd_relative_to_sampled_lanes() {
    for off in [3 * 3600 * SEC, -2 * 3600 * SEC] {
        let mut s = Session::new(Role::Client, 1, DetRand::rand(1));
        for (id, owd, n) in [(0, 0, 0), (0x0101, off + 40 * MS, 5), (0x0201, off + 55 * MS, 130), (0x0202, 0, 0)] {
            let mut l = Lane::new(id, 1);
            (l.owd, l.owds.n) = (owd, n);
            s.lanes.live.push(l);
        }
        // 0x0201 shows 15 ms only above a base of off + 40 ms: 0x0101's OWD, not 0.
        let rel = |s: &Session| s.snapshot().lanes.iter().map(|l| l.out.owd_rel_ms).collect::<Vec<_>>();
        assert_eq!(rel(&s), [0.0, 0.0, 15.0, 0.0], "offset {off}");
        for l in s.lanes.iter_mut() {
            l.owds.n = 0; // as after a wake
        }
        assert_eq!(rel(&s), [0.0; 4], "offset {off}: no lane has a sample");
    }
}

/// The event ring keeps the last STATUS_EVENTS, oldest first.
#[test]
fn event_log() {
    let mut s = Session::new(Role::Client, 1, DetRand::rand(1));
    assert!(s.snapshot().events.is_empty());
    for t in 0..3 {
        s.log(t, format!("e{t}"));
    }
    assert_eq!(s.snapshot().events.iter().map(|e| e.msg.as_str()).collect::<Vec<_>>(), ["e0", "e1", "e2"]);
    for t in 3..(2 * STATUS_EVENTS as Mono + 5) {
        s.log(t, format!("e{t}"));
    }
    let ev = s.snapshot().events;
    assert_eq!(ev.len(), STATUS_EVENTS);
    assert!(ev.iter().zip(STATUS_EVENTS as Mono + 5..).all(|(e, t)| e.at == t && e.msg == format!("e{t}")));
}
