//! Tests of session.rs and lib.rs's Session methods. Owner: C2. Ports from core/pair_test.go:
//! TestPairKeyExchange, TestPairMeshFlow, TestPairForged; from core/session_test.go:
//! TestSendAllocs (with `pair::allocs` and an Out that does not allocate).
//!
//! The ports drive whole sessions; the other tests need only session.rs and lib.rs.

use std::net::SocketAddr;

use super::Mode;
use crate::flow::FLOW_LANES;
use crate::flow::FlowLane;
use crate::lane::{Lane, LaneIx};
use crate::pair::*;
use crate::wire::Window;
use crate::{
    Event, HEADER_LEN, MAX_PACKET, MS, Msg, Out, RidTable, Role, SMALL_BIT, Session, SockId, TAG_LEN, TYPE_DATA,
    TYPE_PROBE,
};

/// Go: TestPairKeyExchange. Each side sends its rid and key on the peer's hello, and each takes
/// the other's.
#[test]
fn pair_key_exchange() {
    let mut p = Pair::new(20 * MS);
    p.start();
    p.run(100 * MS);
    for (n, peer) in [(&p.c, &p.s), (&p.s, &p.c)] {
        assert!(n.s.tx.rid == peer.s.rid && n.s.rx.cur.k == peer.s.tx.key.k, "{:?}: keys not exchanged", n.role);
    }
}

/// Go: TestPairMeshFlow. A flow each way over the Tailscale lane arrives whole and in order.
#[test]
fn pair_mesh_flow() {
    let mut p = Pair::new(15 * MS);
    p.start();
    p.run(100 * MS);
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 200, 300, MS);
    p.stream(Side::S, f, 200, 1400, MS);
    p.run(100 * MS);
    in_order(&p.s, f, 200, 0);
    in_order(&p.c, f, 200, 0);
}

/// Go: TestPairForged. Real packets replayed, corrupted and re-addressed are all dropped, with
/// no reply and no change to the receiver's keys or window (§7.6 rule 1).
#[test]
fn pair_forged() {
    let mut p = Pair::new(10 * MS);
    p.start();
    p.run(50 * MS);
    let f = crate::FlowId(1);
    p.stream(Side::C, f, 50, 100, MS);
    p.run(50 * MS);
    let got = p.s.got.get(&f).map_or(0, Vec::len);
    for it in &p.wire {
        let Ev::Packet { sock, from, data } = &it.ev else { continue };
        if it.to != Side::S || p.now - it.at < 3 * p.ctl {
            continue; // still in flight: not a replay yet
        }
        let mut bad = vec![data.clone()];
        for i in [0, 1, 2, 5, HEADER_LEN, data.len() - 1] {
            let mut b = data.clone();
            b[i] ^= 0x10;
            bad.push(b);
        }
        for b in &bad {
            let before = rx_state(&p.s.s);
            let out = handle(&mut p.s.s, p.now, Event::Packet { sock: *sock, from: *from, data: b });
            assert!(
                out.is_empty() && rx_state(&p.s.s) == before,
                "forged or replayed packet changed state or got {} actions",
                out.len()
            );
        }
    }
    assert_eq!(p.s.got.get(&f).map_or(0, Vec::len), got, "a replayed packet was delivered");
}

/// An Out that only counts, so the measured `handle` calls allocate nothing of their own.
#[derive(Default)]
struct Count {
    sends: usize,
    bytes: usize,
    delivers: usize,
    controls: usize,
}

impl Out for Count {
    fn send(&mut self, _: SockId, _: SocketAddr, data: &[u8]) {
        self.sends += 1;
        self.bytes += data.len();
    }

    fn deliver(&mut self, _: &[u8]) {
        self.delivers += 1;
    }

    fn control(&mut self, _: Msg) {
        self.controls += 1;
    }
}

/// Go: TestSendAllocs. `handle` allocates nothing for an App event in steady state (§4.2,
/// §17.1).
#[test]
fn send_allocs() {
    let (mut c, _, _) = keyed();
    // Two keyed flows (§13), their packets built once: building them would allocate.
    let video = crate::pair::inner(crate::FlowId(1), &[0u8; 1380]);
    let audio = crate::pair::inner(crate::FlowId(2), &[0u8; 200]);
    let (mut out, mut rids, mut now) = (Count::default(), RidTable::new(), 0);
    let mut step = |c: &mut Session, out: &mut Count| {
        now += MS / 10;
        c.handle(now, Event::Send { packet: &video }, &mut rids, out);
        c.handle(now, Event::Send { packet: &audio }, &mut rids, out);
    };
    // Warm up, plus one unmeasured batch as Go's AllocsPerRun runs; then one measured run of
    // 5,000 steps, so that even one allocation in the whole batch (a probe tick, a pacer release)
    // shows.
    for _ in 0..25_000 {
        step(&mut c, &mut out);
    }
    let (before, sends) = (allocs(), out.sends);
    for _ in 0..5_000 {
        step(&mut c, &mut out);
    }
    let n = allocs() - before;
    assert_eq!(n, 0, "send path: {n} allocations in 5,000 steps");
    assert!(out.sends > sends && out.bytes > 0, "nothing was sent");
}

/// The modes as messages name them; nothing else is one, from either mode. A new session runs
/// performance (§14).
#[test]
fn mode_names() {
    assert_eq!((Mode::Performance.as_str(), Mode::Bandwidth.as_str()), ("performance", "bandwidth"));
    assert_eq!(
        (Mode::parse("performance"), Mode::parse("bandwidth")),
        (Some(Mode::Performance), Some(Mode::Bandwidth))
    );
    let mut s = Session::new(Role::Client, 1, DetRand::rand(1));
    assert_eq!(s.mode, Mode::Performance, "a new session runs performance");
    for start in [Mode::Bandwidth, Mode::Performance] {
        s.set_mode(start.as_str());
        assert_eq!(s.mode, start);
        for m in ["", "stable", "combine", "capacity", "Performance", "BANDWIDTH", "turbo"] {
            s.set_mode(m);
            assert_eq!(s.mode, start, "{m:?}");
        }
    }
}

/// A new mode restarts the scheduler from no lane; an unknown or unchanged one, which a peer's
/// hello can carry, changes nothing (§11).
#[test]
fn set_mode() {
    let mut s = Session::new(Role::Client, 1, DetRand::rand(1));
    let choose = |s: &mut Session| {
        s.nspread = 1;
    };
    let cleared = |s: &Session| s.nspread == 0;
    choose(&mut s);
    for m in ["stable", "combine", "turbo", "", "Bandwidth", "performance"] {
        s.set_mode(m);
        assert!(s.mode == Mode::Performance && !cleared(&s), "{m:?}");
    }
    s.set_mode("bandwidth");
    assert!(s.mode == Mode::Bandwidth && cleared(&s));
    choose(&mut s);
    s.set_mode("bandwidth");
    assert!(s.mode == Mode::Bandwidth && !cleared(&s));
    s.set_mode("stable"); // 1.x's name: nothing
    assert!(s.mode == Mode::Bandwidth && !cleared(&s));
    s.set_mode("performance");
    assert!(s.mode == Mode::Performance && cleared(&s));
}

/// The client's own mode message and the peer's other messages reach `control` through `handle`;
/// what core does not consume (view, error, anything else) changes nothing and gets no reply.
#[test]
fn control_mode() {
    let mut s = Session::new(Role::Client, 1, DetRand::rand(1));
    for t in ["view", "error", "", "hello2"] {
        let m = Msg { t: t.into(), mode: "bandwidth".into(), ..Msg::default() };
        assert!(handle(&mut s, 0, Event::Control(&m)).is_empty(), "{t:?}");
        assert_eq!(s.mode, Mode::Performance, "{t:?}");
    }
    let m = Msg { t: "mode".into(), mode: "bandwidth".into(), ..Msg::default() };
    assert!(handle(&mut s, 0, Event::Control(&m)).is_empty());
    assert_eq!(s.mode, Mode::Bandwidth);
    assert_eq!(s.rid, 1, "the rid is allocated on the first handle");
}

/// `key` carries my receive id and my current send key: the first 32 bytes drawn (§7.2).
#[test]
fn send_key() {
    let mut s = Session::new(Role::Anchor, 1, DetRand::rand(2));
    assert!(recv(&mut s, &[]).is_empty()); // allocates the rid
    let mut acts = Acts::default();
    s.send_key(&mut acts);
    let mut k = [0; 32];
    DetRand(2).fill(&mut k);
    match &acts.0[..] {
        [Act::Control(m)] => {
            assert!(m.t == "key" && m.rid == s.rid && m.rid != 0 && m.e == 0 && m.k == k, "{m:?}");
            assert!(m.links.is_empty() && m.mode.is_empty() && m.view.is_none(), "{m:?}");
        }
        a => panic!("{a:?}"),
    }
}

/// §7.5 starts with the size, the rid and the type: a packet that fails them is dropped before
/// any key is touched, with no reply and no state (§7.6 rule 1).
#[test]
fn packet_drops_before_opening() {
    let mut s = Session::new(Role::Anchor, 1, DetRand::rand(12));
    assert!(recv(&mut s, &[]).is_empty());
    let rid = s.rid;
    let pkt = |typ: u8, rid: u16, len: usize| {
        let mut b = vec![0u8; len];
        b[0] = typ;
        b[1..3].copy_from_slice(&rid.to_be_bytes());
        b
    };
    let bad = [
        vec![TYPE_DATA],
        pkt(TYPE_DATA, rid, HEADER_LEN + TAG_LEN - 1),
        pkt(TYPE_DATA, rid + 1, 100),
        pkt(TYPE_PROBE, rid ^ 0x8000, 100),
        pkt(TYPE_DATA, rid, MAX_PACKET + 1),
        pkt(TYPE_PROBE, rid, 4096),
        pkt(0, rid, 100),
        pkt(0x03, rid, 100),
        pkt(TYPE_DATA | 0x10, rid, 100),
        pkt(TYPE_PROBE | SMALL_BIT, rid, 100),
    ];
    let before = rx_state(&s);
    for b in &bad {
        let out = recv(&mut s, b);
        assert!(out.is_empty() && rx_state(&s) == before && s.last_auth.is_none(), "{} bytes, type {}", b.len(), b[0]);
    }
}

/// A key from outside the chain: the peer's new session numbers from 1 again, so every lane's
/// window, probe Seq, counters and lagging state, and every flow's order, start again (notes/core.md
/// 31). The receive statistics stay: echoes report them as differences.
#[test]
fn peer_new_forgets_numbering() {
    let mut s = Session::new(Role::Client, 1, DetRand::rand(1));
    s.arr_n = 42;
    for id in [0, 0x0101] {
        let mut l = Lane::new(id, 1);
        (l.rx_seq, l.rx_last, l.last_small, l.expect, l.lagging, l.rx_pkts) = (9, 100, 50, 3, true, 7);
        (l.win.top, l.win.bits[1]) = (100, 1 << 36);
        s.lanes.live.push(l);
    }
    s.flows.resize(4, Default::default()); // slots are made as flows open (§13)
    let f = &mut s.flows[3];
    (f.used, f.id, f.next, f.fill) = (true, crate::FlowId(1), 17, 5 * MS);
    f.lanes[0] = FlowLane { l: Some(LaneIx(1)), at: 3, seq: 16 };
    let mut acts = Acts::default();
    s.peer_new(&mut acts);
    assert!(acts.0.is_empty());
    for l in s.lanes.iter() {
        assert!(l.win == Window::default() && (l.rx_seq, l.rx_last, l.last_small, l.expect) == (0, 0, 0, 42));
        assert!(!l.lagging && l.rx_pkts == 7);
    }
    let f = &s.flows[3];
    assert!(f.used && f.next == 1 && f.fill == 0 && f.lanes == [FlowLane::default(); FLOW_LANES]);
    assert!(s.flows.iter().enumerate().all(|(i, f)| i == 3 || !f.used && f.next == 0));
}
