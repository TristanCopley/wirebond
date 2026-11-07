//! Tests of lane.rs. Owner: C3. Ports from core/lane_test.go: TestLanesUp, TestStall,
//! TestRebind, TestRelayedProbe, TestIdle, TestWake (`hotel` and `lane_state` are in pair.rs).
//! New (R2-03): a peer that answers a validation probe it never received, by echoing the
//! predictable Seq from its own address, does not validate the claimed address.
//!
//! The ported tests drive two sessions through pair.rs. The unit tests before them build one side's
//! lanes from the hotel's links and call lane.rs directly.

use std::net::SocketAddr;

use super::*;
use crate::estimate::NO_MIN;
use crate::pair::*;
use crate::{Event, Link, LinkInfo, MS, MeshLane, PROBE_IDLE, PUNCH_FOR, PUNCH_GAP, SEC, TYPE_PROBE, VALIDATE_AMP};

const fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
    SocketAddr::V4(std::net::SocketAddrV4::new(std::net::Ipv4Addr::new(a, b, c, d), port))
}

/// The hotel's NAT mappings (pair.rs `hotel`).
const NAT_WIFI: SocketAddr = v4(203, 0, 113, 9, 61022);
const NAT_PHONE: SocketAddr = v4(192, 0, 2, 77, 40000);
/// A third party's address, which an on-path network relays a probe from (A6).
const VICTIM: SocketAddr = v4(192, 0, 2, 200, 5555);

/// The lanes of either side of the hotel, by index: sorted by id 0, 0x0101, 0x0201.
const MESH: LaneIx = LaneIx(0);
const WIFI: LaneIx = LaneIx(1);
const PHONE: LaneIx = LaneIx(2);

fn link(id: u8, label: &str, metered: bool, default: bool, local: SocketAddr) -> Link {
    Link { id, label: label.to_string(), metered, default, sock: 10 + u16::from(id), local, mapped: None }
}

fn info(id: u8, label: &str, metered: bool, default: bool, cands: &[SocketAddr]) -> LinkInfo {
    LinkInfo { id, label: label.to_string(), metered, default, cands: cands.to_vec() }
}

/// One side of the hotel without the network: its links, its Tailscale lane and the peer's links
/// as hello gives them, and the lanes `set_links` builds from them (none goes, so nothing is
/// retired).
fn hotel_side(role: Role) -> Session {
    let mut s = Session::new(role, 1, DetRand::rand(1));
    match role {
        Role::Client => {
            s.links = vec![link(1, "Wi-Fi", false, true, C_WIFI), link(2, "iPhone", true, false, C_PHONE)];
            s.mesh = Some(MeshLane { sock: 1, local: C_MESH, peer: S_MESH });
            s.peer = vec![info(1, "Ethernet", false, false, &[S_ETH])];
        }
        Role::Anchor => {
            s.links = vec![link(1, "Ethernet", false, false, S_ETH)];
            s.mesh = Some(MeshLane { sock: 1, local: S_MESH, peer: C_MESH });
            s.peer = vec![
                info(1, "Wi-Fi", false, true, &[C_WIFI, NAT_WIFI]),
                info(2, "iPhone", true, false, &[C_PHONE, NAT_PHONE]),
            ];
        }
    }
    s.set_links(&mut Acts::default(), 0);
    s
}

fn ids(s: &Session) -> Vec<u16> {
    s.lanes.iter().map(|l| l.id).collect()
}

#[test]
fn lanes_from_links() {
    for role in [Role::Client, Role::Anchor] {
        let s = hotel_side(role);
        assert_eq!(ids(&s), [0, 0x0101, 0x0201], "{role:?}: both roles name a lane client link << 8 | server link");
        let groups: Vec<i32> = s.lanes.iter().map(|l| l.group).collect();
        assert_eq!(groups, [0, 0, 1], "{role:?}: the Tailscale lane belongs to the default link's group");
        let m = &s.lanes[MESH];
        assert!(m.mesh && m.validated && m.peer.is_some(), "{role:?}: the Tailscale lane is validated at once");
        for l in s.lanes.iter().skip(1) {
            assert!(!l.mesh && !l.validated && l.peer.is_none() && l.state == State::Punching, "{role:?} {:#x}", l.id);
        }
        assert!(!s.lanes[WIFI].metered && s.lanes[PHONE].metered, "{role:?}: either end's metered link");
    }
    let s = hotel_side(Role::Anchor);
    let l = &s.lanes[PHONE];
    assert_eq!((l.sock, l.addr, l.local.as_str(), l.remote.as_str()), (11, Some(S_ETH), "Ethernet", "iPhone"));
    assert_eq!(l.cands, [C_PHONE, NAT_PHONE]);
}

#[test]
fn lanes_pair_links_of_one_family() {
    let mut s = hotel_side(Role::Client);
    // The Wi-Fi interface's IPv6 link pairs with the server's IPv6 link only, and shares the
    // Wi-Fi group; an IPv4-mapped address counts as IPv4; a peer link without candidates makes
    // no lane; a link id offered twice makes one lane.
    s.links.push(link(3, "Wi-Fi", false, false, "[2001:db8::2]:7849".parse().unwrap()));
    s.links.push(link(4, "Ethernet", false, false, "[::ffff:10.0.2.2]:7849".parse().unwrap()));
    s.peer.push(info(2, "Ethernet", false, false, &["[2001:db8:1::7]:7849".parse().unwrap()]));
    s.peer.push(info(3, "LTE", false, false, &[]));
    s.peer.push(info(1, "Ethernet again", false, false, &[v4(198, 51, 100, 9, 7849)]));
    s.set_links(&mut Acts::default(), 0);
    assert_eq!(ids(&s), [0, 0x0101, 0x0201, 0x0302, 0x0401]);
    let groups: Vec<i32> = s.lanes.iter().map(|l| l.group).collect();
    assert_eq!(groups, [0, 0, 1, 0, 2]);
    assert_eq!(s.lanes[WIFI].remote, "Ethernet", "the first link with an id makes its lane");
}

#[test]
fn no_default_link_shares_every_group() {
    let mut s = hotel_side(Role::Client);
    s.links[0].default = false; // a Tailscale exit node holds the default route (§6.1)
    s.set_links(&mut Acts::default(), 0);
    assert_eq!(s.lanes[MESH].group, -1);
}

#[test]
fn lanes_keep_their_state_and_indices() {
    let mut s = hotel_side(Role::Client);
    let x = &mut s.lanes[PHONE];
    (x.rtt, x.validated, x.peer, x.tx_seq) = (47 * MS, true, Some(S_ETH), 900);
    s.idle = Some(PHONE);
    (s.spread[0], s.spread[1], s.nspread) = (PHONE, MESH, 2);
    s.flows.resize(4, Default::default()); // slots are made as flows open (§13)
    s.flows[3].lanes[1].l = Some(PHONE);
    s.arr[7] = Arrival { c: 9, at: 5, l: Some(PHONE) };
    s.seq_top = 1234;
    // A second server link: its lanes sort between the old ones, which move up.
    s.peer.push(info(2, "Fiber", false, false, &[v4(198, 51, 100, 8, 7849)]));
    s.set_links(&mut Acts::default(), 0);
    assert_eq!(ids(&s), [0, 0x0101, 0x0102, 0x0201, 0x0202]);
    let phone = LaneIx(3);
    let x = &s.lanes[phone];
    assert_eq!((x.id, x.rtt, x.validated, x.tx_seq), (0x0201, 47 * MS, true, 900), "a lane that stays keeps its state");
    assert_eq!(s.idle, Some(phone));
    assert_eq!(s.spread(), [phone, MESH]);
    assert_eq!((s.flows[3].lanes[1].l, s.arr[7].l), (Some(phone), Some(phone)));
    // R2-10: new lanes count their Seq from the session's highest, so a peer that still holds
    // a lane under the same id takes their probes as newer.
    for id in [0x0102, 0x0202] {
        let l = &s.lanes[s.lane_by_id(id).unwrap()];
        assert_eq!((l.tx_seq, l.echo_seq, l.rtt_seq, l.hist_seq), (1234, 1234, 1234, 1234), "{id:#x}");
    }
}

#[test]
fn rebuilt_lanes_start_afresh() {
    let mut s = hotel_side(Role::Client);
    for l in [WIFI, PHONE] {
        let x = &mut s.lanes[l];
        (x.peer, x.pgen, x.validated, x.rtt, x.tx_seq) = (Some(S_ETH), 3, true, 32 * MS, 500);
    }
    s.lanes[MESH].tx_seq = 700;
    s.seq_top = 800;
    s.idle = Some(WIFI);
    (s.spread[0], s.spread[1], s.spread[2], s.nspread) = (PHONE, MESH, WIFI, 3);
    s.flows.resize(1, Default::default()); // slots are made as flows open (§13)
    s.flows[0].lanes[0] = FlowLane { l: Some(PHONE), at: 9, seq: 4 };
    s.flows[0].lanes[1] = FlowLane { l: Some(MESH), at: 8, seq: 3 };
    s.arr[0] = Arrival { c: 1, at: 1, l: Some(PHONE) };
    // The Mac joins another Wi-Fi network (a new address on the same link) and the iPhone goes.
    s.links[0].local = v4(10, 0, 1, 5, 7849);
    s.links.pop();
    s.rebuild();
    assert_eq!(ids(&s), [0, 0x0101]);
    let x = &s.lanes[WIFI];
    assert_eq!((x.rtt, x.addr), (0, Some(v4(10, 0, 1, 5, 7849))), "fresh estimates on a new network");
    assert_eq!((x.peer, x.pgen, x.validated), (Some(S_ETH), 3, true), "the return address moves only on a probe");
    assert_eq!((x.tx_seq, x.echo_seq), (800, 800), "R2-10: a rebuilt lane counts on from the highest Seq");
    // The old Wi-Fi and iPhone lanes wait in `gone`, in their old order, for retire.
    assert_eq!(s.lanes.gone.len(), 2);
    let (old_wifi, old_phone) = (LaneIx(2), LaneIx(3));
    assert_eq!((s.lanes[old_wifi].id, s.lanes[old_wifi].rtt, s.lanes[old_phone].id), (0x0101, 32 * MS, 0x0201));
    assert_eq!(s.idle, Some(old_wifi));
    assert_eq!(s.spread(), [old_phone, MESH, old_wifi]);
    assert_eq!(
        (s.flows[0].lanes[0].l, s.flows[0].lanes[1].l, s.arr[0].l),
        (Some(old_phone), Some(MESH), Some(old_phone))
    );
    // Then nothing names a gone lane any more.
    s.drop_gone();
    assert!(s.lanes.gone.is_empty());
    assert_eq!((s.idle, s.spread()), (None, &[MESH][..]));
    assert_eq!((s.flows[0].lanes[0], s.flows[0].lanes[1].l), (FlowLane::default(), Some(MESH)));
    assert_eq!(s.arr[0].l, None);

    // The server's link moves: no candidate stays, so its old address may be someone else's.
    let mut s = hotel_side(Role::Client);
    let x = &mut s.lanes[WIFI];
    (x.peer, x.validated) = (Some(S_ETH), true);
    s.peer[0].cands = vec![v4(198, 51, 100, 99, 7849)];
    s.rebuild();
    let x = &s.lanes[WIFI];
    assert_eq!((x.peer, x.validated, s.lanes.gone.len()), (Some(S_ETH), false, 2));

    // R2-10's case: the Tailscale lane after its mesh address returns on a new socket.
    let mut s = hotel_side(Role::Client);
    s.seq_top = 3000;
    s.mesh = Some(MeshLane { sock: 5, local: C_MESH, peer: S_MESH });
    s.rebuild();
    let x = &s.lanes[MESH];
    assert_eq!((x.sock, x.mesh, x.validated, x.peer, x.tx_seq), (5, true, true, Some(S_MESH), 3000));
    assert_eq!(s.lanes.gone.len(), 1);
}

#[test]
fn echo_returns_what_arrived() {
    let mut l = Lane::new(0x0101, 11);
    (l.rx_seq, l.rx_at, l.rx_owd, l.snap_pkts, l.snap_bytes) = (41, 2 * SEC, -3_000, 17, 9_000);
    (l.rx_last, l.rx_tok, l.lagging) = (77, 0xfeed, true);
    let e = l.echo(2 * SEC + 1_500_000);
    let want = Echo {
        lane: 0x0101,
        flags: 1,
        seq: 41,
        held: 1_500,
        owd: -3_000,
        pkts: 17,
        bytes: 9_000,
        last: 77,
        token: 0xfeed,
        chal: 0,
    };
    assert_eq!(e, want, "the challenge of the newest probe goes back as the token (R2-03)");
}

/// Records that `l` sent probe `seq` to address generation `agen`.
fn sent(l: &mut Lane, seq: u32, agen: u32) {
    l.tx_seq = seq;
    l.sent[seq as usize % RTT_RING] = SentProbe { seq, agen, pkts: 0, at: 0 };
}

#[test]
fn next_wait_counts_the_return_address_only() {
    let mut l = Lane::new(0x0101, 11);
    (l.pgen, l.cgen) = (2, 3);
    for (seq, agen) in [(1, 2), (2, 0), (3, 3), (4, 2), (5, 0)] {
        sent(&mut l, seq, agen);
    }
    assert_eq!(l.next_wait(0), 1);
    assert_eq!(l.next_wait(1), 4, "probes to a candidate and to a claim do not count");
    assert_eq!(l.next_wait(4), 0);
    assert_eq!(l.next_wait(5), 0);
    assert_eq!(l.next_wait(9), 0, "after the newest probe");
}

#[test]
fn next_wait_is_bounded_and_wraps() {
    // R2-24: Go walked every Seq up to tx_seq, 2^32 of them when tx_seq is u32::MAX.
    let mut l = Lane::new(0x0101, 11);
    l.pgen = 1;
    sent(&mut l, u32::MAX - 1, 1);
    sent(&mut l, u32::MAX, 0);
    assert_eq!(l.next_wait(u32::MAX - 2), u32::MAX - 1);
    assert_eq!(l.next_wait(u32::MAX - 1), 0);
    sent(&mut l, 1, 1); // the Seq after u32::MAX: 0 is never a probe
    assert_eq!(l.next_wait(u32::MAX - 1), 1, "across the wrap");
    assert_eq!(l.next_wait(5), 0, "seq after tx_seq");
    // Only the last RTT_RING probes are in the ring: an older one is gone, a newer one found.
    let mut l = Lane::new(0x0101, 11);
    l.pgen = 1;
    for seq in 1..=1000 {
        sent(&mut l, seq, u32::from(seq == 700 || seq == 800));
    }
    assert_eq!(l.next_wait(10), 800);
    sent(&mut l, u32::MAX, 0);
    assert_eq!(l.next_wait(10), 0, "a ring of other probes: no match within RTT_RING steps");
}

#[test]
fn seq_order_is_serial() {
    assert!(after(5, 3) && !after(3, 5) && !after(7, 7) && after(1, u32::MAX));
    assert!(seq_newer(1, 0) && seq_newer(u32::MAX, 0), "nothing received yet: any probe is newer");
    assert!(!seq_newer(0, 0) && !seq_newer(0, 5), "Seq 0 is never a probe");
    assert!(seq_newer(2, u32::MAX - 1) && !seq_newer(u32::MAX - 1, 2), "across the wrap (R2-24)");
    assert_eq!((next_gen(1, 0), next_gen(3, 7), next_gen(u32::MAX, 1)), (2, 8, 1));
}

#[test]
fn down_forgets_the_base_owd() {
    let mut s = hotel_side(Role::Client);
    let x = &mut s.lanes[WIFI];
    x.state = State::Up;
    (x.min_rtt.cur, x.min_owd.cur, x.clean_at) = (30 * MS, 12 * MS, SEC);
    s.next = NEVER;
    s.set_state(7 * SEC, WIFI, State::Down);
    let x = &s.lanes[WIFI];
    assert_eq!(x.state, State::Down);
    assert_eq!((x.min_rtt.get(), x.min_owd.get(), x.clean_at), (NO_MIN, NO_MIN, NEVER));
    assert_eq!(s.next, 7 * SEC, "the scheduler reacts at once");
    s.next = NEVER;
    s.set_state(8 * SEC, WIFI, State::Down);
    assert_eq!(s.next, NEVER, "the same state again changes nothing");
}

#[test]
fn up_from_suspect_keeps_the_rate() {
    let mut s = hotel_side(Role::Client);
    let x = &mut s.lanes[WIFI];
    (x.state, x.rate, x.clean_at) = (State::Suspect, 12e6, NEVER);
    s.set_state(SEC, WIFI, State::Up);
    assert_eq!((s.lanes[WIFI].state, s.lanes[WIFI].rate), (State::Up, 12e6));
    s.lanes[WIFI].clean_at = 2 * SEC;
    s.set_state(3 * SEC, WIFI, State::Dormant);
    assert_eq!((s.lanes[WIFI].state, s.lanes[WIFI].clean_at), (State::Dormant, NEVER));
}

#[test]
fn punch_rounds_keep_their_gap() {
    let mut s = hotel_side(Role::Client);
    s.lanes[PHONE].state = State::Dormant;
    s.dozing = true;
    s.punch(10 * SEC);
    assert_eq!((s.punch_at, s.punch_until, s.next_punch), (10 * SEC, 10 * SEC + PUNCH_FOR, 10 * SEC));
    assert!(!s.punch_pending && !s.dozing);
    assert_eq!(s.lanes[PHONE].state, State::Punching, "dormant lanes must be confirmed again");
    assert!(s.lanes.iter().all(|l| l.next_probe == 10 * SEC), "every lane probes at once");
    // A request inside the gap starts a round when the gap ends.
    s.next = NEVER;
    s.punch(12 * SEC);
    assert!(s.punch_pending);
    assert_eq!((s.punch_at, s.next), (10 * SEC, 10 * SEC + PUNCH_GAP));
    s.punch(10 * SEC + PUNCH_GAP);
    assert!(!s.punch_pending);
    assert_eq!(s.punch_at, 10 * SEC + PUNCH_GAP);
}

#[test]
fn idle_lane_is_direct_and_unmetered_first() {
    let mut s = hotel_side(Role::Client);
    for (l, rtt) in s.lanes.iter_mut().zip([5 * MS, 64 * MS, 20 * MS]) {
        (l.validated, l.state, l.rtt, l.peer, l.misses) = (true, State::Up, rtt, Some(S_ETH), 2);
    }
    s.go_idle(SEC);
    assert!(s.dozing);
    assert_eq!(s.idle, Some(WIFI), "unmetered before a faster metered lane; never the Tailscale lane");
    let states: Vec<State> = s.lanes.iter().map(|l| l.state).collect();
    assert_eq!(states, [State::Dormant, State::Up, State::Dormant]);
    assert_eq!((s.lanes[WIFI].misses, s.lanes[WIFI].next_probe), (0, SEC + PROBE_IDLE));
    // A down lane stays down and is never chosen.
    s.lanes[WIFI].state = State::Down;
    s.go_idle(2 * SEC);
    assert_eq!((s.idle, s.lanes[WIFI].state), (Some(PHONE), State::Down));
    // Among unmetered lanes, the lowest RTT.
    (s.lanes[WIFI].state, s.lanes[PHONE].metered) = (State::Up, false);
    s.pick_idle(3 * SEC);
    assert_eq!(s.idle, Some(PHONE));
    // A server has none: it only answers.
    let mut s = hotel_side(Role::Anchor);
    for l in s.lanes.iter_mut() {
        (l.validated, l.state) = (true, State::Up);
    }
    s.go_idle(SEC);
    assert_eq!(s.idle, None);
    assert!(s.lanes.iter().all(|l| l.state == State::Dormant));
}

#[test]
fn idle_ticks() {
    // An idle server wants no tick at all, and sends nothing.
    let mut s = hotel_side(Role::Anchor);
    for l in s.lanes.iter_mut() {
        (l.peer, l.validated, l.state) = (Some(C_MESH), true, State::Dormant);
    }
    s.next = NEVER;
    let mut out = Acts::default();
    for l in s.lanes.ixs() {
        s.lane_tick(&mut out, 10 * SEC, l);
    }
    assert!(out.0.is_empty());
    assert_eq!(s.next, NEVER);
    // An idle client wants a tick for its next idle probe.
    let mut c = hotel_side(Role::Client);
    for l in [WIFI, PHONE] {
        (c.lanes[l].peer, c.lanes[l].validated) = (Some(S_ETH), true);
    }
    c.go_idle(SEC);
    c.next = NEVER;
    for l in c.lanes.ixs() {
        c.lane_tick(&mut out, 2 * SEC, l);
    }
    assert!(out.0.is_empty());
    assert_eq!(c.next, SEC + PROBE_IDLE);
    // Its third idle probe unanswered: the lane is down and the next lane takes over (§8.1).
    let x = &mut c.lanes[WIFI];
    (x.wait_seq, x.misses) = (5, 2);
    c.lane_tick(&mut out, SEC + PROBE_IDLE, WIFI);
    assert!(out.0.is_empty());
    assert_eq!((c.lanes[WIFI].state, c.idle, c.lanes[PHONE].state), (State::Down, Some(PHONE), State::Up));
}

#[test]
fn activate_wakes_every_lane() {
    let mut s = hotel_side(Role::Client);
    for l in s.lanes.iter_mut() {
        (l.state, l.wait_seq) = (State::Dormant, 9);
    }
    s.lanes[WIFI].state = State::Up;
    s.dozing = true;
    s.activate(20 * SEC);
    assert!(s.active && !s.confirmed && !s.dozing);
    assert_eq!((s.since, s.last_echo, s.punch_until), (20 * SEC, 20 * SEC, 20 * SEC + PUNCH_FOR));
    assert_eq!(s.woke, Mono::MIN, "an activation steps no clock: the idle probes' OWDs still count");
    let states: Vec<State> = s.lanes.iter().map(|l| l.state).collect();
    assert_eq!(states, [State::Punching, State::Up, State::Punching]);
    assert!(s.lanes.iter().all(|l| l.wait_seq == 0 && l.next_probe == 20 * SEC));
}

#[test]
fn wake_restarts_clocks_and_minima() {
    let mut s = hotel_side(Role::Client);
    s.arr_n = 42;
    for l in s.lanes.iter_mut() {
        (l.min_rtt.cur, l.min_owd.cur, l.in_min.cur, l.state) = (3, 4, 5, State::Up);
        (l.wait_seq, l.expect, l.owd, l.in_owd, l.qd) = (8, 1, 9, 10, 11);
        (l.owds.v[0], l.owds.n, l.owd_in.v[0], l.owd_in.n) = (7.0, 1, 7.0, 1);
    }
    s.punch_at = 99 * SEC;
    assert_eq!(s.woke, Mono::MIN, "no wake yet: every probe's OWD counts");
    s.wake(100 * SEC);
    assert_eq!((s.since, s.woke, s.last_echo), (100 * SEC, 100 * SEC, 100 * SEC));
    for l in s.lanes.iter() {
        assert_eq!((l.min_rtt.get(), l.min_owd.get(), l.in_min.get()), (NO_MIN, NO_MIN, NO_MIN));
        assert_eq!((l.wait_seq, l.expect, l.owd, l.in_owd, l.qd), (0, 42, 0, 0, 0));
        assert!(l.owds == Series::default() && l.owd_in == Series::default(), "a paused clock stepped every OWD");
        assert_eq!(l.state, State::Up, "no lane turns suspect on the wake alone");
    }
    assert!(s.punch_pending, "the wake's punch round waits for the gap");
    assert!(s.events[..s.nevents].iter().any(|e| (e.at, e.msg.as_str()) == (100 * SEC, "woke; lanes re-probed")));
}

#[test]
fn copies_owed() {
    let mut s = hotel_side(Role::Anchor);
    s.copied(SEC, WIFI, 10, true, true); // first arrivals on Wi-Fi: the other lanes owe them
    s.copied(SEC, WIFI, 11, true, true);
    s.copied(SEC, MESH, 3, false, true); // not small: nobody owes it
    assert_eq!(s.arr_n, 2);
    assert_eq!(s.expected(WIFI), None, "a lane owes no copy that came first on it");
    assert_eq!(s.expected(PHONE), Some(Arrival { c: 10, at: SEC, l: Some(WIFI) }));
    // Lanes are FIFO: a packet passes the copies sealed before it, which were lost.
    s.copied(SEC, PHONE, 10, false, true);
    assert_eq!(s.expected(PHONE).map(|a| a.c), Some(11));
    s.copied(SEC, PHONE, 12, false, true);
    assert_eq!(s.expected(PHONE), None);
    // An arrival on a lane that has gone sets no deadline (drop_gone).
    (s.lanes[PHONE].expect, s.lanes[PHONE].last_small) = (0, 0);
    s.arr[0].l = None;
    assert_eq!(s.expected(PHONE).map(|a| a.c), Some(11));
    // Deadlines are kept for PENDING_COPIES counters, the oldest dropped first.
    for c in 0..PENDING_COPIES as u64 + 3 {
        s.copied(SEC, WIFI, 100 + c, true, true);
    }
    (s.lanes[PHONE].expect, s.lanes[PHONE].last_small) = (0, 0);
    assert_eq!(s.expected(PHONE).map(|a| a.c), Some(103));
    assert_eq!(s.lanes[PHONE].expect, 5);
}

#[test]
fn addresses_move_only_as_rule_2_says() {
    let mut s = hotel_side(Role::Anchor);
    let moved = v4(203, 0, 113, 9, 62000);
    assert_eq!(s.move_peer(MESH, NAT_WIFI, 100), None);
    assert_eq!(s.lanes[MESH].peer, Some(C_MESH), "the Tailscale lane's addresses never move");
    // Not validated (a punching lane, §6.3 item 4): it moves at once, and its path is new.
    let x = &mut s.lanes[WIFI];
    (x.wait_seq, x.hist_n, x.tx_seq, x.min_rtt.cur) = (5, 3, 9, 7);
    assert_eq!(s.move_peer(WIFI, NAT_WIFI, 120), Some(NAT_WIFI));
    let x = &s.lanes[WIFI];
    assert_eq!((x.peer, x.pgen, x.amp_in, x.amp_out), (Some(NAT_WIFI), 2, 120, 0));
    assert_eq!((x.wait_seq, x.hist_n, x.hist_seq, x.min_rtt.get()), (0, 0, 9, NO_MIN));
    assert_eq!(s.move_peer(WIFI, NAT_WIFI, 120), None);
    // Validated: a new address is a claim, probed while the lane carries on at its address.
    (s.lanes[WIFI].validated, s.lanes[WIFI].amp_out) = (true, 50);
    assert_eq!(s.move_peer(WIFI, moved, 130), Some(moved));
    let x = &s.lanes[WIFI];
    assert_eq!((x.cand, x.cgen, x.cvalid, x.amp_in, x.amp_out), (Some(moved), 3, false, 130, 0));
    assert_eq!((x.peer, x.pgen), (Some(NAT_WIFI), 2));
    s.lanes[WIFI].cvalid = true;
    assert_eq!(s.move_peer(WIFI, moved, 130), None, "the claim's own next probe opens nothing");
    assert!(s.lanes[WIFI].cvalid);
    assert_eq!(s.move_peer(WIFI, NAT_WIFI, 100), None);
    assert_eq!(s.lanes[WIFI].cand, None, "a newer probe from the validated address ends the claim");
}

#[test]
fn probes_validate_the_return_address_and_claims() {
    let mut s = hotel_side(Role::Anchor);
    let moved = v4(203, 0, 113, 9, 62000);
    let x = &mut s.lanes[WIFI];
    (x.peer, x.pgen, x.cand, x.cgen) = (Some(NAT_WIFI), 2, Some(moved), 3);
    assert_eq!(s.target(WIFI, NAT_WIFI), (2, true), "not validated yet");
    s.lanes[WIFI].validated = true;
    assert_eq!(s.target(WIFI, NAT_WIFI), (2, false));
    assert_eq!(s.target(WIFI, moved), (3, true), "a claim");
    assert_eq!(s.target(WIFI, C_WIFI), (0, false), "a punching candidate");
}

/// A punch round's probes, as (to, bytes) in the order sent.
fn punched(s: &mut Session, now: Mono) -> Vec<(SocketAddr, usize)> {
    let mut out = Acts::default();
    s.punch_probes(&mut out, now);
    out.0
        .into_iter()
        .filter_map(|a| match a {
            Act::Send { to, data, .. } => Some((to, data.len())),
            _ => None,
        })
        .collect()
}

/// A punch round probes an unvalidated return address the peer offered as a candidate, even
/// with its VALIDATE_AMP budget spent: with a candidate probe, unchecked, before its own probe
/// (which then carries the newest Seq and the challenge). A return address the peer did not
/// offer, or a validated one, gets no candidate probe (Rust only: notes/rust.md, accept:
/// integration).
#[test]
fn punch_rounds_probe_an_offered_return_address() {
    let mut s = hotel_side(Role::Anchor);
    s.tx.rid = 9; // the peer's key: without it no probe is sent
    let (now, moved) = (10 * SEC, v4(203, 0, 113, 9, 62000));
    let cand = HEADER_LEN + probe_len(0) + TAG_LEN;
    // Both direct lanes unvalidated, their budgets spent (3 × 43 bytes sent for 43 received);
    // the iPhone lane's return address is the peer's mapping, the Wi-Fi lane's one it never
    // offered (a port-dependent mapping, §6.3 item 4). Neither lane's own probe is due in this
    // tick (lane_tick's), so the round tries it itself.
    for (l, peer) in [(PHONE, NAT_PHONE), (WIFI, moved)] {
        let x = &mut s.lanes[l];
        (x.peer, x.pgen, x.amp_in, x.amp_out, x.next_probe) = (Some(peer), 2, 43, 129, now + SEC);
    }
    let (pkts, seq) = (s.lanes[PHONE].tx_pkts, s.lanes[PHONE].tx_seq);
    let got = punched(&mut s, now);
    assert_eq!(
        got,
        [(NAT_WIFI, cand), (C_WIFI, cand), (NAT_PHONE, cand), (C_PHONE, cand)],
        "the unoffered return address gets nothing, the offered one a candidate probe"
    );
    let x = &s.lanes[PHONE];
    assert_eq!((x.amp_out, x.tx_pkts, x.wait_seq), (129, pkts + 1, seq + 1), "unchecked, and to the lane's address");
    assert_eq!(x.sent[(seq + 1) as usize % RTT_RING].agen, 2, "the return address's generation");
    // With budget left, the lane's own probe follows: 1 echo, its own, which carries the
    // challenge (R2-03).
    s.lanes[PHONE].amp_in = 1000;
    let got = punched(&mut s, now);
    let phone: Vec<_> = got.iter().filter(|(to, _)| *to == NAT_PHONE).map(|&(_, n)| n).collect();
    assert_eq!(phone, [cand, HEADER_LEN + probe_len(1) + TAG_LEN]);
    assert_eq!(got.last(), Some(&(NAT_PHONE, HEADER_LEN + probe_len(1) + TAG_LEN)), "the newest probe");
    // Validated: its own probe is never held back, and it is the only one.
    s.lanes[PHONE].validated = true;
    let got = punched(&mut s, now);
    assert_eq!(got.iter().filter(|(to, _)| *to == NAT_PHONE).count(), 1);
}

#[test]
fn probes_echo_their_own_lane_first() {
    let mut s = hotel_side(Role::Client);
    let now = 10 * SEC;
    (s.lanes[WIFI].rx_seq, s.lanes[WIFI].rx_at) = (7, 5 * SEC); // older than ECHO_WINDOW
    (s.lanes[PHONE].rx_seq, s.lanes[PHONE].rx_at) = (8, 9 * SEC);
    assert_eq!(s.fill_echoes(now, WIFI, MAX_ECHOES, false), 2);
    assert_eq!((s.eout[0].lane, s.eout[0].seq, s.eout[1].lane), (0x0101, 7, 0x0201));
    assert_eq!(s.fill_echoes(now, PHONE, MAX_ECHOES, false), 1, "Wi-Fi was not heard within ECHO_WINDOW");
    s.lanes[WIFI].lagging = true;
    assert_eq!(s.fill_echoes(now, PHONE, MAX_ECHOES, false), 2, "a lagging lane is always echoed");
    assert_eq!(s.fill_echoes(now, WIFI, 1, false), 1);
    assert_eq!(s.fill_echoes(now, WIFI, 0, false), 0);
    // Nothing received on the Tailscale lane: its probes echo the others only, unless they
    // validate an address, whose challenge rides in the lane's own echo (R2-03).
    assert_eq!(s.fill_echoes(now, MESH, MAX_ECHOES, false), 2);
    assert_eq!(s.eout[0].lane, 0x0101);
    assert_eq!(s.fill_echoes(now, MESH, MAX_ECHOES, true), 3);
    assert_eq!((s.eout[0].lane, s.eout[0].seq), (0, 0));
}

#[test]
fn lagging_marks_from_the_newest_probe() {
    let mut s = hotel_side(Role::Client);
    let moved = v4(198, 51, 100, 7, 9000);
    s.pin = Probe { lane: 0x0201, seq: 3, sent_at: 5_000, flags: 0, n: 2 };
    s.ein[0] = Echo { lane: 0x0101, flags: LAGGING, ..Echo::default() };
    s.ein[1] = Echo { lane: 0x0201, ..Echo::default() };
    s.lanes[PHONE].peer_lag = true; // an older view
    s.lag_view(SEC);
    assert!(s.lanes[WIFI].peer_lag && !s.lanes[PHONE].peer_lag);
    assert_eq!(s.lag_as_of, 5_000);
    // A probe sent earlier that arrives late on a slow lane changes nothing (notes/core.md 8b).
    s.pin.sent_at = 4_000;
    s.ein[0].flags = 0;
    s.lag_view(SEC);
    assert!(s.lanes[WIFI].peer_lag);
    // Marks sent before copies to a new address could arrive, or for a claimed lane, are stale.
    (s.pin.sent_at, s.ein[0].flags, s.lanes[WIFI].lag_from) = (6_000, LAGGING, 7_000);
    s.lag_view(SEC);
    assert!(!s.lanes[WIFI].peer_lag);
    (s.pin.sent_at, s.lanes[WIFI].cand) = (8_000, Some(moved));
    s.lag_view(SEC);
    assert!(!s.lanes[WIFI].peer_lag);
    (s.pin.sent_at, s.lanes[WIFI].cand) = (9_000, None);
    s.lag_view(SEC);
    assert!(s.lanes[WIFI].peer_lag);
}

// -------------------------------------------------------------------------------------------
// Ported from core/lane_test.go, and R2-03's forged validation: two sessions through pair.rs.

/// Lane `id` of side `n`.
fn lane(n: &Tnode, id: u16) -> &Lane {
    let l = n.s.lane_by_id(id).unwrap_or_else(|| panic!("{:?} has no lane {id:#x}", n.role));
    &n.s.lanes[l]
}

/// Runs the pair until the client sends a probe on its Wi-Fi lane, and returns the last one sent
/// in that millisecond as the server would receive it.
fn client_probe(p: &mut Pair) -> Vec<u8> {
    let from = p.nat[&C_WIFI];
    for _ in 0..1000 {
        let mark = p.wire.len();
        p.run(MS);
        let mut probes = p.wire[mark..].iter().filter_map(|it| match &it.ev {
            Ev::Packet { sock: 11, from: f, data }
                if it.to == Side::S && *f == from && data.first() == Some(&TYPE_PROBE) =>
            {
                Some(data.clone())
            }
            _ => None,
        });
        if let Some(b) = probes.next_back() {
            return b;
        }
    }
    panic!("no probe on the client's Wi-Fi lane for a second");
}

#[test]
fn lanes_up() {
    let mut p = hotel();
    p.run(2 * SEC);
    for n in [&p.c, &p.s] {
        for id in [0, 0x0101, 0x0201] {
            lane_state(n, id, State::Up);
        }
        let g = [lane(n, 0).group, lane(n, 0x0101).group, lane(n, 0x0201).group];
        assert_eq!(g, [0, 0, 1], "{:?}: the Tailscale lane belongs to the default link's group", n.role);
    }
    // Both direct lanes take copies, one per group; the Tailscale lane shares Wi-Fi's group.
    let l = &p.c.s.lanes;
    assert!(l[WIFI].in_copy && l[PHONE].in_copy && !l[MESH].in_copy, "copy set wrong");
    // The server learned the client's NAT mapping for each lane.
    assert_eq!(lane(&p.s, 0x0201).peer, Some(p.nat[&C_PHONE]), "server's iPhone lane");
}

/// A stall on the Wi-Fi lane is seen at the receiver as missing copies and reported at once, so
/// the sender marks the lane suspect within about 100 ms (§8.4); it goes down after DOWN_AFTER
/// and comes back after UP_AFTER of clean echoes.
#[test]
fn stall() {
    let mut p = hotel();
    p.run(SEC);
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 200, 100, 5 * MS);
    assert!(
        lane(&p.s, 0x0101).copy_in && lane(&p.s, 0x0201).copy_in,
        "receiver does not expect copies on both direct lanes"
    );
    p.paths.get_mut(&WIFI_TO).expect("the hotel's Wi-Fi path").down = true;
    let start = p.now;
    while lane(&p.c, 0x0101).state == State::Up {
        p.stream(Side::C, f, 1, 100, 5 * MS);
        assert!(p.now - start <= SEC, "stall not detected");
    }
    let (d, lagging) = (p.now - start, lane(&p.s, 0x0101).lagging);
    assert!(d <= 120 * MS && lagging, "stall detected after {} ms, receiver lagging {lagging}", d / MS);
    p.stream(Side::C, f, 450, 100, 5 * MS);
    lane_state(&p.c, 0x0101, State::Down);
    p.run(100 * MS);
    let n = p.c.count[&f];
    complete(&p.s, f, n, 0); // input survives on the iPhone lane
    p.paths.get_mut(&WIFI_TO).expect("the hotel's Wi-Fi path").down = false;
    p.stream(Side::C, f, 300, 100, 5 * MS);
    lane_state(&p.c, 0x0101, State::Up);
}

/// A NAT rebinding moves the server's return address on the client's next probe, and the new
/// address is validated within about one RTT (§7.6 rule 2, A7).
#[test]
fn rebind() {
    let mut p = hotel();
    p.run(SEC);
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 100, 100, 5 * MS);
    let moved = v4(203, 0, 113, 9, 62000);
    p.nat.insert(C_WIFI, moved);
    p.stream(Side::C, f, 30, 100, 5 * MS);
    let l = lane(&p.s, 0x0101);
    assert!(
        l.peer == Some(moved) && l.validated && l.state == State::Up,
        "after rebinding: {:?} validated {} {}",
        l.peer,
        l.validated,
        l.state.name()
    );
    p.run(100 * MS);
    complete(&p.s, f, 130, 0);
}

/// An on-path network relays a real probe from a victim's address before the real one arrives.
/// The victim gets probes only, within VALIDATE_AMP × the bytes received from it, the lane carries
/// on at the real address meanwhile, and the next real probe ends the claim (§7.6 rule 2, A6).
#[test]
fn relayed_probe() {
    let mut p = hotel();
    p.run(SEC);
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 100, 100, 5 * MS);
    let nat = p.nat[&C_WIFI];
    let relayed = client_probe(&mut p);
    let mark = p.wire.len();
    p.handle(Side::S, Ev::Packet { sock: 11, from: VICTIM, data: relayed.clone() });
    let l = lane(&p.s, 0x0101);
    assert!(
        l.cand == Some(VICTIM) && l.peer == Some(nat) && l.validated,
        "after the relayed probe: claim {:?}, address {:?}, validated {}",
        l.cand,
        l.peer,
        l.validated
    );
    p.stream(Side::C, f, 100, 100, 5 * MS);
    let mut sent = 0;
    for it in &p.wire[mark..] {
        if let Ev::Packet { data, .. } = &it.ev
            && it.dst == Some(VICTIM)
        {
            sent += data.len();
            assert_eq!(data[0], TYPE_PROBE, "data sent to an unvalidated address");
        }
    }
    assert!(
        sent > 0 && sent as u64 <= VALIDATE_AMP * relayed.len() as u64,
        "{sent} bytes sent to the victim for {} received",
        relayed.len()
    );
    let l = lane(&p.s, 0x0101);
    assert!(
        l.peer == Some(nat) && l.validated && l.cand.is_none(),
        "the claim did not end: claim {:?}, address {:?}, validated {}",
        l.cand,
        l.peer,
        l.validated
    );
    p.run(100 * MS);
    complete(&p.s, f, 200, 0);
}

/// The client sends its next probe on the Wi-Fi lane from the victim's address, with the lane's
/// echo forged: Seq `seq`, which it can predict, and `token` for the challenge it never saw.
fn forge_echo(p: &mut Pair, seq: u32, token: u64) {
    let l = p.c.s.lane_by_id(0x0101).expect("the client's Wi-Fi lane");
    let x = &mut p.c.s.lanes[l];
    (x.rx_seq, x.rx_tok) = (seq, token);
    let (to, now, mut out) = (x.peer, p.now, Acts::default());
    p.c.s.send_probe(&mut out, now, l, to, MAX_ECHOES);
    for a in out.0 {
        if let Act::Send { data, .. } = a {
            p.handle(Side::S, Ev::Packet { sock: 11, from: VICTIM, data });
        }
    }
}

/// R2-03: a hostile client relays one of its own probes from a victim's address, which opens a
/// claim, then answers the server's validation probes to the victim, which it never received:
/// its echoes carry their Seq, predictable from the Seqs it has seen, but not their challenges.
/// The claim never validates and the lane never moves; the next real probe ends the claim. With
/// a challenge's own token, the same echo validates the claim and moves the lane.
#[test]
fn forged_validation() {
    let mut p = hotel();
    p.run(SEC);
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 100, 100, 5 * MS);
    let nat = p.nat[&C_WIFI];
    let sl = p.s.s.lane_by_id(0x0101).expect("the server's Wi-Fi lane");
    let relayed = client_probe(&mut p);
    p.handle(Side::S, Ev::Packet { sock: 11, from: VICTIM, data: relayed });
    let x = &p.s.s.lanes[sl];
    assert_eq!((x.cand, x.peer), (Some(VICTIM), Some(nat)), "the relayed probe opens a claim");
    let (cgen, pgen) = (x.cgen, x.pgen);
    let mut seq = x.tx_seq;
    // Five guesses, each at a fresh validation probe: none, a constant, and the challenges of the
    // previous probe, of this Seq to the lane's validated address, and of this Seq on another lane.
    for k in 0..5 {
        if k > 0 {
            let now = p.now; // the claim's next probe, as lane_tick sends one each PROBE_ACTIVE
            p.s.s.send_probe(&mut Acts::default(), now, sl, Some(VICTIM), MAX_ECHOES);
            assert_ne!(p.s.s.lanes[sl].tx_seq, seq, "the server stopped probing the claim");
            seq = p.s.s.lanes[sl].tx_seq;
        }
        let sp = p.s.s.lanes[sl].sent[seq as usize % RTT_RING];
        assert_eq!((sp.seq, sp.agen), (seq, cgen), "probe {seq} went to the claim");
        let c = &p.s.s.chal;
        let guess = [
            0,
            0x0123_4567_89ab_cdef,
            c.token(0x0101, cgen, seq.wrapping_sub(1)),
            c.token(0x0101, pgen, seq),
            c.token(0x0201, cgen, seq),
        ][k];
        forge_echo(&mut p, seq, guess);
        let x = &p.s.s.lanes[sl];
        assert!(
            !x.cvalid && x.cand == Some(VICTIM) && x.peer == Some(nat) && x.validated,
            "guess {k} ({guess:#x}) for probe {seq}: claim {:?} valid {}, address {:?}",
            x.cand,
            x.cvalid,
            x.peer
        );
    }
    // The real client carries on: its next probe ends the claim, and the stream stays whole.
    p.stream(Side::C, f, 100, 100, 5 * MS);
    let x = &p.s.s.lanes[sl];
    assert!(
        x.cand.is_none() && x.peer == Some(nat) && x.validated,
        "the claim did not end: {:?} at {:?}",
        x.cand,
        x.peer
    );
    p.run(100 * MS);
    complete(&p.s, f, 200, 0);
    // The control: a new claim, answered with its challenge's own token, validates and moves.
    let relayed = client_probe(&mut p);
    p.handle(Side::S, Ev::Packet { sock: 11, from: VICTIM, data: relayed });
    let x = &p.s.s.lanes[sl];
    let (cgen, seq) = (x.cgen, x.tx_seq);
    assert_eq!((x.cand, x.sent[seq as usize % RTT_RING].agen), (Some(VICTIM), cgen));
    let token = p.s.s.chal.token(0x0101, cgen, seq);
    forge_echo(&mut p, seq, token);
    let x = &p.s.s.lanes[sl];
    assert!(
        x.peer == Some(VICTIM) && x.cand.is_none(),
        "the challenge's token must validate: {:?} at {:?}",
        x.cand,
        x.peer
    );
}

/// An idle session sends one probe every PROBE_IDLE on one unmetered direct lane and the server
/// answers each; nothing goes on the metered lane (§8.1, A8).
#[test]
fn idle() {
    let mut p = hotel();
    p.run(SEC);
    p.stream(Side::C, crate::FlowId(1), 50, 100, 5 * MS);
    p.run(10 * SEC);
    let mark = p.wire.len();
    p.run(10 * crate::MIN);
    let (mut bytes, mut metered) = (0, 0);
    let phone = p.nat[&C_PHONE];
    for it in &p.wire[mark..] {
        if let Ev::Packet { sock, from, data } = &it.ev {
            bytes += data.len() + 28;
            metered += usize::from(*sock == 12 || *from == phone);
        }
    }
    let n = p.wire.len() - mark;
    assert!(
        bytes <= 100_000 / 6 && metered == 0 && n >= 40,
        "idle 10 min: {bytes} bytes, {n} packets, {metered} on the metered lane"
    );
    lane_state(&p.c, 0x0101, State::Up);
    lane_state(&p.c, 0x0201, State::Dormant);
    // Idle, the client wants a tick for its next idle probe and the server none at all.
    let d = p.c.s.next() - p.now;
    assert!(
        d > 0 && d <= PROBE_IDLE && p.s.s.next() == NEVER,
        "idle: client next in {d} ns, server next {}",
        p.s.s.next()
    );
    // The first datagram after idle goes on every known lane, then the lanes punch again.
    let f = crate::FlowId(2); // another key than the first stream's
    p.stream(Side::C, f, 100, 100, 5 * MS);
    p.run(100 * MS);
    complete(&p.s, f, 100, 0);
    lane_state(&p.c, 0x0201, State::Up);
}

/// One probe on the iPhone lane from the client to the idle server, with at most `max` echoes and
/// the lane's echo returning `tok`. The client takes what the server sends back, which is
/// returned; an answer the client would send to it goes nowhere (the caller answers instead).
fn idle_exchange(p: &mut Pair, max: usize, tok: u64) -> Vec<Vec<u8>> {
    let (from, now, mut out) = (p.nat[&C_PHONE], p.now, Acts::default());
    let (cl, sl) = (p.c.s.lane_by_id(0x0201).expect("client lane"), p.s.s.lane_by_id(0x0201).expect("server lane"));
    p.c.s.lanes[cl].rx_tok = tok;
    let to = p.c.s.lanes[cl].peer;
    p.c.s.send_probe(&mut out, now, cl, to, max);
    let [Act::Send { data, .. }] = &out.0[..] else { panic!("the client sent {:?}", out.0) };
    let back: Vec<Vec<u8>> = handle(&mut p.s.s, now, Event::Packet { sock: 11, from, data })
        .into_iter()
        .filter_map(|a| match a {
            Act::Send { to, data, .. } if to == from => Some(data),
            _ => None,
        })
        .collect();
    for data in &back {
        handle(&mut p.c.s, now, Event::Packet { sock: 12, from: S_ETH, data });
        assert_eq!(p.c.s.lanes[cl].rx_seq, p.s.s.lanes[sl].tx_seq, "the client did not take the server's probe");
    }
    back
}

/// A Go client never returns the challenge (its echo carries the address it saw where a Rust echo
/// carries the token, which reads as none: R2-03), so a Rust server never validates its address,
/// and it answers at once every probe that asks (PROBE_ANSWER). An idle server answers every probe
/// at once (§8.1) and asks for nothing in turn, so the exchange ends with its answer. When that
/// answer asked, a Go client and a Rust server traded probes as fast as the path carried them for
/// as long as the session stayed idle (notes/rust.md, system: integration, interop go/rust). A
/// Rust client returns the challenge the answer carried on its next probe on the lane, which
/// validates the address; and asked by that probe, the idle server answers without asking.
#[test]
fn idle_answers_ask_nothing() {
    let mut p = hotel();
    p.run(SEC);
    p.stream(Side::C, crate::FlowId(1), 50, 100, 5 * MS);
    p.run(10 * SEC);
    let sl = p.s.s.lane_by_id(0x0201).expect("the server's iPhone lane");
    p.s.s.lanes[sl].validated = false; // as a Go client's address stays
    // The Go client's probe (as when the lane leaves its copy set going idle), then its answer to
    // each probe of the server's that asks, at no more echoes than that probe.
    let (mut max, mut asks) = (MAX_ECHOES, 0);
    while asks < 100 {
        let back = idle_exchange(&mut p, max, 0);
        assert_eq!(back.len(), 1, "the idle server answers each probe once");
        if p.c.s.pin.flags & PROBE_ANSWER == 0 {
            break;
        }
        (max, asks) = (p.c.s.pin.n, asks + 1);
    }
    assert_eq!(asks, 0, "the idle server's answers asked for {asks} answers in a row");
    assert!(!p.s.s.lanes[sl].validated, "an echo without the challenge validated the address");
    // A Rust client's next probe on the lane returns the challenge of the answer it took. Its own
    // view of the lane is unvalidated meanwhile, as after a move (with the bytes of the probe that
    // moved it to spend), so the probe asks.
    let cl = p.c.s.lane_by_id(0x0201).expect("the client's iPhone lane");
    let x = &mut p.c.s.lanes[cl];
    (x.validated, x.amp_in, x.amp_out) = (false, 200, 0);
    let tok = x.rx_tok;
    let back = idle_exchange(&mut p, MAX_ECHOES, tok);
    let (c, s) = (&p.c.s.lanes[cl], &p.s.s.lanes[sl]);
    assert!(
        back.len() == 1 && p.c.s.pin.flags & PROBE_ANSWER == 0 && c.validated && s.validated,
        "{} answers, the last asking {}; validated: client {}, server {}",
        back.len(),
        p.c.s.pin.flags & PROBE_ANSWER != 0,
        c.validated,
        s.validated
    );
}

/// A wake re-probes every lane but turns none suspect on its own (§8.4).
#[test]
fn wake_turns_no_lane_suspect() {
    let mut p = hotel();
    p.run(SEC);
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 100, 100, 5 * MS);
    p.handle(Side::C, Ev::Wake);
    for _ in 0..100 {
        p.stream(Side::C, f, 1, 100, 5 * MS);
        for l in p.c.s.lanes.iter() {
            assert_eq!(l.state, State::Up, "lane {:#x} {} after a wake", l.id, l.state.name());
        }
    }
}

/// R2-24: a lane's probe Seq wraps after 2^32 probes, sooner when a peer asks for many answers.
/// Both sessions start the hotel's lanes a few probes short of the wrap (new lanes count from the
/// session's highest Seq, R2-10), and every lane carries on across it: the receiver takes the
/// wrapped Seqs as newer, the sender keeps finding the probe it waits for, and the echoes keep the
/// lanes up. Go took no probe after the wrap, and its nextWait walked 2^32 Seqs at it. It also
/// needs estimate.rs's `echo` to compare Seqs serially (`e.seq` against `rtt_seq`, `echo_seq`,
/// `wait_seq`, `hist_seq`): with plain comparisons every echo after the wrap reads as old.
#[test]
fn probe_seqs_wrap() {
    let mut p = Pair::new(33 * MS);
    p.add_link(Side::C, 1, "Wi-Fi", false, true, C_WIFI);
    p.add_link(Side::C, 2, "iPhone", true, false, C_PHONE);
    p.add_link(Side::S, 1, "Ethernet", false, false, S_ETH);
    p.nat.insert(C_WIFI, NAT_WIFI);
    p.nat.insert(C_PHONE, NAT_PHONE);
    p.link(C_WIFI, S_ETH, Path { delay: 32 * MS, ..Path::default() });
    p.link(C_PHONE, S_ETH, Path { delay: 47 * MS, ..Path::default() });
    let top = u32::MAX - 40;
    (p.c.s.seq_top, p.s.s.seq_top) = (top, top);
    p.start();
    p.run(SEC);
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 400, 100, 5 * MS); // about 100 probes on every lane each way
    for n in [&p.c, &p.s] {
        for id in [0, 0x0101, 0x0201] {
            lane_state(n, id, State::Up);
            let l = lane(n, id);
            assert!(
                l.tx_seq < top && l.rx_seq < top,
                "{:?} lane {id:#x}: sent {}, received {}",
                n.role,
                l.tx_seq,
                l.rx_seq
            );
        }
    }
    p.run(100 * MS);
    complete(&p.s, f, 400, 0);
}

/// A lagging mark that has no copy left to judge, with no small packet arriving on any lane for
/// ECHO_WINDOW, is cleared. In a session with no small flow nothing else clears it, and the sender
/// kept the lane suspect for good (a latent defect of Go's engine; notes/rust.md, core:
/// integration). Here the hotel's Wi-Fi stalls as a stream starts: its first, small, datagrams
/// mark Wi-Fi lagging at the server, then the stream turns bulk, Wi-Fi recovers, and no small
/// packet comes again.
#[test]
fn lagging_mark_without_copies_clears() {
    let mut p = hotel();
    p.run(SEC);
    p.paths.get_mut(&WIFI_TO).expect("the hotel's Wi-Fi path").down = true;
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 300, 1_000, MS); // small for its first datagrams, then 8 Mbit/s of bulk
    assert!(lane(&p.s, 0x0101).lagging, "the server did not mark Wi-Fi lagging");
    lane_state(&p.c, 0x0101, State::Suspect);
    p.paths.get_mut(&WIFI_TO).expect("the hotel's Wi-Fi path").down = false;
    p.stream(Side::C, f, 1_000, 1_000, MS);
    assert!(lane(&p.s, 0x0101).lagging, "cleared while ECHO_WINDOW had not passed since the last copy");
    p.stream(Side::C, f, 3_000, 1_000, MS);
    assert!(!lane(&p.s, 0x0101).lagging, "the mark outlived its copies");
    lane_state(&p.c, 0x0101, State::Up);
    p.run(100 * MS);
    complete(&p.s, f, 4_300, 0);
}

/// §8.4's deadline: a copy that came first on lane B is owed on lane A max(LAG_FLOOR, 3 × dev_A)
/// after the lateness the two lanes' OWDs predict, as the spec's max(10 ms, 3 × dev_A) + OWD_A −
/// OWD_B has it, with A's latest OWD and B's base (notes/core.md 33b). Before notes/rust.md
/// "netem: A2" the floor only bounded the whole wait, so a lane slower than B by LAG_FLOOR or more
/// and without jitter was owed the copy at the very time it came: the system harness's cell, 15
/// ms slower than the hotel, was marked lagging by a stream's first, small, datagrams (a full-size
/// copy is slower at the cell's 80 Mbit/s than the probes that measured its OWD), and the Mac
/// kept it suspect for 2 s while the hotel overloaded under the stream.
#[test]
fn copies_are_owed_lag_floor_past_the_lateness_owds_predict() {
    let mut s = hotel_side(Role::Anchor);
    let at = 10 * SEC;
    s.now = at;
    let w = &mut s.lanes[WIFI];
    (w.in_owd, w.rx_owd) = (30 * MS, 30_000);
    w.in_min.add(at, 30 * MS);
    w.owd_in.add(at, 30 * MS);
    let ph = &mut s.lanes[PHONE];
    (ph.in_owd, ph.rx_owd, ph.in_dev) = (45 * MS, 45_000, 0);
    ph.owd_in.add(at, 45 * MS);
    // The iPhone, 15 ms slower and without jitter: the 15 ms, then LAG_FLOOR.
    let first = Arrival { c: 1, at, l: Some(WIFI) };
    assert_eq!(s.deadline(PHONE, &first), at + 15 * MS + LAG_FLOOR);
    // With jitter, 3 × dev beyond its average OWD and again past it (as before).
    s.lanes[PHONE].in_dev = 5 * MS;
    assert_eq!(s.deadline(PHONE, &first), at + (60 - 30) * MS + 15 * MS);
    // Wi-Fi, faster even at its latest, owes a copy that came first on the iPhone after
    // LAG_FLOOR, and a jittery Wi-Fi after 3 × dev less what it is faster by (as before).
    s.lanes[PHONE].in_min.add(at, 45 * MS);
    let later = Arrival { c: 2, at, l: Some(PHONE) };
    assert_eq!(s.deadline(WIFI, &later), at + LAG_FLOOR);
    s.lanes[WIFI].in_dev = 6 * MS;
    assert_eq!(s.deadline(WIFI, &later), at + (48 - 45) * MS + 18 * MS);
}
