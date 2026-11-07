//! Tests of bandwidth.rs. Owner: C6. Ports from core/combine_test.go: TestCombine. New: the blind
//! sender: with every return path blackholed for 2 s in bandwidth, bulk stays paced on two lanes in
//! different groups (never `send_all`), and the spread resumes when echoes return. And the spread's
//! rules on hand-built sessions.
//!
//! The pair tests drive two whole sessions through the pair driver; the unit tests use bandwidth.rs
//! and stable.rs alone.

use std::net::SocketAddr;

use crate::buf::Pkt;
use crate::estimate::{NO_MIN, Winmin};
use crate::lane::{Lane, LaneIx, State};
use crate::pair::*;
use crate::session::{Mode, NEVER};
use crate::{
    BANDWIDTH_FLOOR_HOLD, BANDWIDTH_SKEW_MAX, BAR, MIN_WINDOW, MODE_BANDWIDTH, MS, Mono, Msg, PACE_FLOOR,
    REWIND_WINDOW, Role, SEC, Session, TYPE_DATA,
};

// ---------------------------------------------------------------------------------------------
// The pair

/// Sets bandwidth on both sides, as the client's `mode` message does.
fn combine_mode(p: &mut Pair) {
    let m = Msg { t: "mode".into(), mode: MODE_BANDWIDTH.into(), ..Msg::default() };
    p.handle(Side::C, Ev::Control(m.clone()));
    let at = p.now + p.ctl;
    p.push(at, Side::S, Ev::Control(m));
}

/// The ids of the server's spread, fastest first.
fn spread(p: &Pair) -> Vec<u16> {
    p.s.s.spread().iter().map(|&l| p.s.s.lanes[l].id).collect()
}

/// TestCombine: bulk larger than either lane spreads over both, at about the sum of their rates,
/// and reaches the receiver in order (§11.2, §10.3).
#[test]
fn bandwidth() {
    let mut p = hotel();
    combine_mode(&mut p);
    for (a, rate) in [(C_WIFI, 20e6), (C_PHONE, 40e6)] {
        let pa = p.paths.get_mut(&(S_ETH, a)).unwrap();
        (pa.rate, pa.buf) = (rate, 60 * MS);
    }
    let (mut video, mut audio, mut input) = p.moonlight(45.0);
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_eq!(p.s.s.mode, Mode::Bandwidth);
    let (sent, got) = (p.s.count[&video.f], p.c.got[&video.f].len());
    p.play(4 * SEC, &mut [&mut video, &mut audio, &mut input]);
    p.run(200 * MS);
    let steady = &p.c.got[&video.f][got..]; // after the rates' startup
    let sent = p.s.count[&video.f] - sent;
    assert!(steady.len() as f64 >= 0.9 * sent as f64, "delivered {} of {sent} datagrams in steady state", steady.len());
    let (mut late, mut top) = (0, -1);
    for d in steady {
        let i = i64::from(d[0]) << 16 | i64::from(d[1]) << 8 | i64::from(d[2]);
        if i < top {
            late += 1;
        } else {
            top = i;
        }
    }
    assert!(late <= steady.len() / 500, "{late} of {} delivered out of order in steady state", steady.len());
    for id in [0x0101, 0x0201] {
        let l = &p.s.s.lanes[p.s.s.lane_by_id(id).unwrap()];
        assert!(l.sent_bytes >= 5_000_000, "lane {id:#x} carried only {} bytes", l.sent_bytes);
    }
    complete(&p.c, audio.f, p.s.count[&audio.f], 0);
    complete(&p.s, input.f, p.c.count[&input.f], 0);
}

/// The blind sender: every lane's return direction is blackholed for 2 s in bandwidth, so the
/// sender sees no echo. The lanes turn suspect and leave (§14.2), and bulk stays paced on the
/// blind pair, two lanes in different groups (§11.4): none goes to every lane (`send_all`, the only way bulk reaches the Tailscale lane while the
/// hotel lane is up or suspect); the video sent from then on arrives whole and in order; and the
/// spread resumes when echoes return (§8.4).
#[test]
fn blind_sender() {
    let mut p = hotel();
    combine_mode(&mut p);
    let (mut video, mut audio, mut input) = p.moonlight(10.0);
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_eq!(spread(&p), [0x0101, 0x0201]);
    let back = [(C_WIFI, S_ETH), (C_PHONE, S_ETH), (C_MESH, S_MESH)];
    for k in back {
        p.paths.get_mut(&k).unwrap().down = true;
    }
    let wire = p.wire.len();
    let (sent, got) = (p.s.count[&video.f], p.c.got[&video.f].len());
    for _ in 0..40 {
        p.play(50 * MS, &mut [&mut video, &mut audio, &mut input]);
        assert!(p.s.s.nspread > 0, "blind for {} ms", (p.now - p.s.s.last_echo) / MS);
    }
    assert!(p.s.s.blind(p.now));
    assert_eq!(spread(&p), [0x0101, 0x0201], "the blind pair");
    let bulk = |it: &&Item| matches!(&it.ev, Ev::Packet { data, .. } if data[0] == TYPE_DATA);
    let to_mesh = p.wire[wire..].iter().filter(|it| it.to == Side::C && it.dst == Some(C_MESH)).filter(bulk);
    assert_eq!(to_mesh.count(), 0, "bulk went to every lane");
    // The lanes that turned suspect left with a rewind (§14.2), which the blind pair's two lanes
    // carried; the video of the blackhole arrived, each datagram at most once.
    let num = |d: &Vec<u8>| usize::from(d[0]) << 16 | usize::from(d[1]) << 8 | usize::from(d[2]);
    let mut during: Vec<usize> = p.c.got[&video.f][got..].iter().map(num).filter(|&i| i >= sent).collect();
    let n = during.len();
    during.sort_unstable();
    during.dedup();
    assert_eq!(during.len(), n, "a datagram delivered twice");
    for k in back {
        p.paths.get_mut(&k).unwrap().down = false;
    }
    let (sent, got) = (p.s.count[&video.f], p.c.got[&video.f].len());
    p.play(4 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert!(!p.s.s.blind(p.now));
    assert_eq!(spread(&p), [0x0101, 0x0201], "after the blackhole");
    p.run(SEC);
    // From the echoes' return on: every datagram, each once and in order.
    let since: Vec<usize> = p.c.got[&video.f][got..].iter().map(num).filter(|&i| i >= sent).collect();
    assert_eq!(since, (sent..p.s.count[&video.f]).collect::<Vec<_>>(), "video from the echoes' return on");
    complete(&p.c, audio.f, p.s.count[&audio.f], 0);
}

// ---------------------------------------------------------------------------------------------
// The spread's rules on hand-built sessions: bandwidth.rs and stable.rs alone

/// A server session in bandwidth, active, with hand-built lanes `(id, group, base OWD ms)`, each up
/// and validated, its OWD (and newest OWD) the base OWD + 2 ms and its RTT twice that; id 0 is the
/// Tailscale lane.
fn session(lanes: &[(u16, i32, Mono)]) -> Session {
    let mut s = Session::new(Role::Anchor, 1, DetRand::rand(1));
    (s.active, s.mode, s.rid) = (true, Mode::Bandwidth, 1);
    for (i, &(id, group, owd)) in lanes.iter().enumerate() {
        let mut l = Lane::new(id, if id == 0 { 1 } else { 11 });
        (l.mesh, l.group, l.validated, l.state, l.peer) = (id == 0, group, true, State::Up, Some(peer(i)));
        (l.owd, l.owd_now, l.rtt) = ((owd + 2) * MS, (owd + 2) * MS, 2 * (owd + 2) * MS);
        l.min_owd = Winmin { at: 0, cur: owd * MS, old: NO_MIN };
        s.lanes.live.push(l);
    }
    s
}

/// The peer's address of hand-built lane `i`.
fn peer(i: usize) -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 10 + i as u8], 7849))
}

fn ix(i: usize) -> LaneIx {
    LaneIx::at(i)
}

/// The spread `eligible` chooses, by lane index.
fn chosen(s: &mut Session, now: Mono) -> Vec<usize> {
    let (keep, n) = s.eligible(now);
    keep[..n].iter().map(|l| l.get()).collect()
}

/// Sets the spread, fastest first.
fn set_spread(s: &mut Session, ls: &[usize]) {
    for (i, &l) in ls.iter().enumerate() {
        s.spread[i] = ix(l);
    }
    s.nspread = ls.len();
}

/// xanth in bandwidth at the hotel: the Tailscale lane (0, 33 ms), Wi-Fi over IPv4 (1, 32 ms) and
/// IPv6 (2, 34 ms) in group 0, the iPhone (3, 45 ms) in group 1.
fn hotel_lanes() -> Session {
    session(&[(0, 0, 33), (0x0101, 0, 32), (0x0102, 0, 34), (0x0201, 1, 45)])
}

/// §14.2: up (not suspect), validated and not barred; one lane per group, its lowest base OWD,
/// whichever lane carried bulk before.
#[test]
fn one_lane_per_group() {
    let mut s = hotel_lanes();
    let now = 10 * SEC;
    // Each group's lowest base OWD, fastest first; the mesh lane shares Wi-Fi's group.
    assert_eq!(chosen(&mut s, now), [1, 3]);
    // Not the lane already carrying bulk: the group's lowest base OWD.
    set_spread(&mut s, &[2, 3]);
    assert_eq!(chosen(&mut s, now), [1, 3]);
    // Suspect, barred, unvalidated or down: not eligible.
    s.lanes[ix(1)].state = State::Suspect;
    assert_eq!(chosen(&mut s, now), [2, 3]);
    s.lanes[ix(2)].validated = false;
    assert_eq!(chosen(&mut s, now), [3]);
    s.lanes[ix(2)].validated = true;
    s.lanes[ix(3)].bar_until = now + 1;
    assert_eq!(chosen(&mut s, now), [2]);
    s.lanes[ix(3)].bar_until = now;
    assert_eq!(chosen(&mut s, now), [2, 3]);
    s.lanes[ix(3)].state = State::Down;
    assert_eq!(chosen(&mut s, now), [2]);
}

/// §14.2: carrying at least a tenth of the fastest lane's rate, once the rate is known (out of
/// startup, with bulk within MIN_WINDOW). Performance has no such rule (§14.1).
#[test]
fn a_tenth_of_the_fastest_rate() {
    let mut s = hotel_lanes();
    let now = 10 * SEC;
    for (l, rate) in [(1, 40e6), (3, 4e6)] {
        let x = &mut s.lanes[ix(l)];
        (x.rate, x.startup, x.bulk_at) = (rate, false, now - SEC);
    }
    assert_eq!(chosen(&mut s, now), [1, 3]);
    s.lanes[ix(3)].rate = 3.9e6;
    assert_eq!(chosen(&mut s, now), [1]);
    s.lanes[ix(3)].startup = true;
    assert_eq!(chosen(&mut s, now), [1, 3], "in startup");
    (s.lanes[ix(3)].startup, s.lanes[ix(3)].bulk_at) = (false, now - MIN_WINDOW);
    assert_eq!(chosen(&mut s, now), [1, 3], "no bulk within MIN_WINDOW");
    s.lanes[ix(3)].bulk_at = now - SEC;
    s.lanes[ix(1)].startup = true;
    assert_eq!(chosen(&mut s, now), [1, 3], "the fastest's rate is not known");
    s.lanes[ix(1)].startup = false;
    s.mode = Mode::Performance;
    assert_eq!(chosen(&mut s, now), [1, 3]);
}

/// The mesh lane takes a group only while no direct lane of it is usable: validated and still
/// answering (up or suspect), barred or not (§14.1, §14.2). The mesh lane crosses its group's
/// link, so a direct lane barred for dropping full-size packets leaves the group to the other
/// groups' lanes, not to the mesh lane, whose fragments that link drops too (netsim's A6).
#[test]
fn mesh_lane_only_for_a_dead_group() {
    let mut s = hotel_lanes();
    let now = 10 * SEC;
    // Wi-Fi suspect: still usable, so the mesh lane, which crosses the same Wi-Fi, stays out.
    for l in [1, 2] {
        s.lanes[ix(l)].state = State::Suspect;
    }
    assert_eq!(chosen(&mut s, now), [3]);
    // Wi-Fi barred: still usable, so the mesh lane stays out too.
    for l in [1, 2] {
        (s.lanes[ix(l)].state, s.lanes[ix(l)].bar_until) = (State::Up, now + 1);
    }
    assert_eq!(chosen(&mut s, now), [3]);
    for l in [1, 2] {
        s.lanes[ix(l)].bar_until = 0;
    }
    assert_eq!(chosen(&mut s, now), [1, 3]);
    // Wi-Fi down: the mesh lane takes its group.
    for l in [1, 2] {
        s.lanes[ix(l)].state = State::Down;
    }
    assert_eq!(chosen(&mut s, now), [0, 3]);
    // A mesh lane sharing every group (-1) joins only while no direct lane at all is usable.
    s.lanes[ix(0)].group = -1;
    assert_eq!(chosen(&mut s, now), [3]);
    s.lanes[ix(3)].bar_until = now + 1;
    assert_eq!(chosen(&mut s, now), [], "the iPhone is barred but usable");
    s.lanes[ix(3)].state = State::Punching;
    assert_eq!(chosen(&mut s, now), [0]);
}

#[test]
fn spread_within_the_skew() {
    let mut s = session(&[(0x0101, 0, 32), (0x0201, 1, 32 + 50), (0x0301, 2, 32 + 51)]);
    let now = 10 * SEC;
    assert_eq!(chosen(&mut s, now), [0, 1]);
    // Fastest first; equal base OWDs in group order.
    s.lanes[ix(0)].min_owd.cur = 82 * MS;
    s.lanes[ix(2)].min_owd.cur = 20 * MS;
    assert_eq!(chosen(&mut s, now), [2]);
    s.lanes[ix(2)].min_owd.cur = 82 * MS;
    assert_eq!(chosen(&mut s, now), [0, 1, 2]);
    // A lane with no base OWD yet is not known to be within the skew, unless no lane has one.
    s.lanes[ix(1)].min_owd.cur = NO_MIN;
    assert_eq!(chosen(&mut s, now), [0, 2]);
    for l in [0, 2] {
        s.lanes[ix(l)].min_owd.cur = NO_MIN;
    }
    assert_eq!(chosen(&mut s, now), [0, 1, 2]);
    assert_eq!(BANDWIDTH_SKEW_MAX, 50 * MS);
}

/// §14.2, leaving the spread: a lane carrying bulk that stays in overuse at PACE_FLOOR for
/// BANDWIDTH_FLOOR_HOLD is barred for BAR. No fits bound is learned (§25).
#[test]
fn floor_bars_a_lane_that_drops_packets() {
    let mut s = hotel_lanes();
    let phone = ix(3);
    set_spread(&mut s, &[1, 3]);
    let l = &mut s.lanes[phone];
    (l.rate, l.rtt, l.delivery) = (PACE_FLOOR, 94 * MS, 3e6);
    let t0 = 10 * SEC;
    s.lanes[phone].last_over = t0;
    assert_eq!(chosen(&mut s, t0), [1, 3]);
    assert_eq!(s.lanes[phone].floor_at, t0);
    s.lanes[phone].last_over = t0 + BANDWIDTH_FLOOR_HOLD - 10 * MS;
    assert_eq!(chosen(&mut s, t0 + BANDWIDTH_FLOOR_HOLD - MS), [1, 3]);
    let now = t0 + BANDWIDTH_FLOOR_HOLD;
    assert_eq!(chosen(&mut s, now), [1]);
    let l = &s.lanes[phone];
    assert_eq!(l.bar_until, now + BAR);
    // Overuse more than 2 RTT ago, a rate above the floor, or no bulk: the hold starts again.
    let wifi = ix(1);
    let l = &mut s.lanes[wifi];
    (l.rate, l.rtt, l.last_over) = (PACE_FLOOR, 0, now - 200 * MS);
    assert_eq!(chosen(&mut s, now), [1]);
    assert_eq!(s.lanes[wifi].floor_at, NEVER);
    s.lanes[wifi].last_over = now - 199 * MS; // 2 × RATE_WINDOW without an RTT
    chosen(&mut s, now);
    assert_eq!(s.lanes[wifi].floor_at, now);
    s.lanes[wifi].rate = PACE_FLOOR + 1.0;
    chosen(&mut s, now + MS);
    assert_eq!(s.lanes[wifi].floor_at, NEVER);
}

/// The blind sender's spread: `two()`, fastest first; the floor's holds start again.
#[test]
fn blind_spread_is_two() {
    let mut s = hotel_lanes();
    for l in s.lanes.iter_mut() {
        (l.state, l.floor_at) = (State::Suspect, 0);
    }
    s.lanes[ix(1)].min_owd.cur = 50 * MS; // the iPhone's base OWD is now the lower
    let (keep, n) = s.blind_spread();
    assert_eq!(keep[..n], [ix(3), ix(1)]);
    assert!(s.lanes.iter().all(|l| l.floor_at == NEVER));
    // A lane that went down while blind stays in the pair (§11.4: down at DOWN_AFTER even while
    // blind, and the pair carries bulk until echoes return); one no longer validated leaves it.
    s.lanes[ix(3)].state = State::Down;
    let (keep, n) = s.blind_spread();
    assert_eq!(keep[..n], [ix(3), ix(1)]);
    s.lanes[ix(3)].validated = false;
    let (keep, n) = s.blind_spread();
    assert_eq!(keep[..n], [ix(1)]);
    for l in s.lanes.iter_mut() {
        l.validated = false;
    }
    assert_eq!(s.blind_spread().1, 0);
}

// ---------------------------------------------------------------------------------------------
// bandwidth() on hand-built sessions: these also move and send packets, so they need C1's pool,
// C3's send, C4's meters, take and pacer, and C5's shift and rewind. Ignored until those are in.

/// Echoes arrived on every lane 5 ms before `t`.
fn fresh(s: &mut Session, t: Mono) {
    s.last_echo = t - 5 * MS;
    for l in s.lanes.iter_mut() {
        l.last_echo = t - 5 * MS;
    }
}

/// Puts `n` sealed bulk packets, counters from `c`, in l's pacer queue (`queued`) or its rewind
/// ring, as queued or sent at `at`.
fn packets(s: &mut Session, l: LaneIx, c: u64, n: u64, at: Mono, queued: bool) {
    for c in c..c + n {
        let b = s.pool.get().unwrap();
        s.pool.buf_mut(b)[3..11].copy_from_slice(&c.to_be_bytes());
        let (p, x) = (Pkt { c, at, buf: Some(b), n: 1435 }, &mut s.lanes[l]);
        if queued {
            x.q.push_back(p);
            x.q_bytes += 1435;
        } else {
            x.rw.push_back(p);
        }
    }
}

/// The counters sent to `to`, in order.
fn sent(out: &Acts, to: SocketAddr) -> Vec<u64> {
    out.0
        .iter()
        .filter_map(|a| match a {
            Act::Send { to: t, data, .. } if *t == to => Some(u64::from_be_bytes(data[3..11].try_into().unwrap())),
            _ => None,
        })
        .collect()
}

/// xanth's spread over Wi-Fi (1) and the iPhone (3), echoes fresh at `now`.
fn spreading(now: Mono) -> Session {
    let mut s = hotel_lanes();
    set_spread(&mut s, &[1, 3]);
    fresh(&mut s, now);
    s
}

fn spread_of(s: &Session) -> Vec<usize> {
    s.spread().iter().map(|l| l.get()).collect()
}

#[test]
fn steady_spread_stays() {
    let now = 30 * SEC;
    let mut s = spreading(now);
    let mut out = Acts::default();
    s.schedule(&mut out, now);
    assert_eq!((spread_of(&s), s.rewinds), (vec![1, 3], 0));
    assert!(out.0.is_empty());
    // §14.2: a suspect lane is not eligible. It leaves, rewound, and its group's next lane joins.
    s.lanes[ix(1)].state = State::Suspect;
    s.schedule(&mut out, now);
    assert_eq!((spread_of(&s), s.rewinds), (vec![2, 3], 1));
}

/// A lane that turns suspect leaves, and its unacknowledged packets of the rewind window, after
/// the echoed counter, are re-sent across the lanes that stay, its queue behind them (§14.2);
/// meanwhile my probes say a rewind is in flight (§15).
#[test]
fn suspect_lane_leaves_with_rewind() {
    let now = 30 * SEC;
    let mut s = spreading(now);
    let wifi = ix(1);
    let w = &mut s.lanes[wifi];
    (w.state, w.last_echo, w.acked) = (State::Suspect, now - 150 * MS, 101);
    packets(&mut s, wifi, 90, 10, now - REWIND_WINDOW - MS, false); // older than the window
    packets(&mut s, wifi, 100, 10, now - 250 * MS, false);
    packets(&mut s, wifi, 110, 3, now - MS, true);
    let mut out = Acts::default();
    s.schedule(&mut out, now);
    assert_eq!((spread_of(&s), s.rewinds), (vec![2, 3], 1));
    let mut got = sent(&out, peer(2));
    got.extend(sent(&out, peer(3)));
    for l in [2, 3] {
        got.extend(s.lanes[ix(l)].q.iter().map(|p| p.c));
    }
    got.sort_unstable();
    assert_eq!(got, (102..113).collect::<Vec<_>>());
    assert!(s.lanes[wifi].rw.is_empty() && s.lanes[wifi].q.is_empty());
    assert!(now < s.rewind_until, "in flight");
}

/// While blind (§11.4) the spread is the blind pair, and a lane that leaves it takes its queue
/// along, unrewound.
#[test]
fn blind_pair_without_rewind() {
    let now = 30 * SEC;
    let mut s = spreading(now);
    for l in s.lanes.iter_mut() {
        l.state = State::Suspect;
    }
    let t = now + 600 * MS;
    s.schedule(&mut Acts::default(), t);
    assert!(s.blind(t));
    assert_eq!((spread_of(&s), s.rewinds), (vec![1, 3], 0));
    // A lane down while blind stays in the pair; one that leaves it (its address moved, so it is
    // unvalidated) takes its queue along, unrewound.
    s.lanes[ix(3)].state = State::Down;
    s.schedule(&mut Acts::default(), t + MS);
    assert_eq!((spread_of(&s), s.rewinds), (vec![1, 3], 0));
    s.lanes[ix(3)].validated = false;
    packets(&mut s, ix(3), 200, 2, t, true);
    s.schedule(&mut Acts::default(), t + 2 * MS);
    assert_eq!((spread_of(&s), s.rewinds, s.lanes[ix(3)].q.len()), (vec![1], 0, 0));
}

/// R2-04: a lane that joins the spread restarts a stale rate before any packet moves onto it; a
/// rate learned within MIN_WINDOW holds.
#[test]
fn joining_lane_is_taken_first() {
    let now = 30 * SEC;
    let phone = ix(3);
    for (bulk_at, rate, startup) in [(now - 20 * SEC, PACE_FLOOR, true), (now - SEC, 6e6, false)] {
        let mut s = spreading(now);
        s.nspread = 1; // Wi-Fi alone
        let p = &mut s.lanes[phone];
        (p.rate, p.startup, p.bulk_at) = (6e6, false, bulk_at);
        s.schedule(&mut Acts::default(), now);
        assert_eq!(spread_of(&s), [1, 3]);
        let p = &s.lanes[phone];
        assert_eq!((p.rate, p.startup, p.took_at), (rate, startup, now));
    }
}

/// §14.2, earliest arrival: newest OWD + 2 × dev + (queued bytes + size) ÷ pace, a lane other
/// than the fastest winning by ECF_MARGIN.
#[test]
fn earliest_arrival() {
    let now = 30 * SEC;
    let (wifi, phone) = (ix(1), ix(3));
    let mut s = spreading(now);
    // Both pace at PACE_FLOOR (nothing offered), so 1,400 bytes take 2.24 ms; Wi-Fi's newest OWD
    // is 34 ms, the iPhone's 47.
    assert_eq!(s.earliest(now, 1400), Some(wifi));
    s.lanes[wifi].q_bytes = 8_900; // 34 + 16.48 ms against 49.24: the iPhone wins by 1.24 ms
    assert_eq!(s.earliest(now, 1400), Some(phone));
    s.lanes[wifi].q_bytes = 8_600; // by 0.76 ms
    assert_eq!(s.earliest(now, 1400), Some(wifi));
    s.lanes[wifi].q_bytes = 8_900;
    s.lanes[phone].dev = MS; // 2 × dev: 51.24 against 50.48
    assert_eq!(s.earliest(now, 1400), Some(wifi));
    assert_eq!(s.bulk_lanes(now, 1400).collect::<Vec<_>>(), [wifi]);
    // The newest OWD, not the base OWD: a standing queue on the Wi-Fi's path (60 ms more) moves
    // the packet to the iPhone, though the Wi-Fi's base OWD is lower.
    s.lanes[phone].dev = 0;
    s.lanes[wifi].q_bytes = 0;
    s.lanes[wifi].owd_now += 60 * MS;
    assert_eq!(s.earliest(now, 1400), Some(phone));
    assert_eq!(s.bulk_lanes(now, 1400).collect::<Vec<_>>(), [phone]);
    s.nspread = 0;
    assert_eq!(s.earliest(now, 1400), None);
    assert!(s.bulk_lanes(now, 1400).next().is_none());
}
