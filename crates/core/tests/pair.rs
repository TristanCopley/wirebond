//! wirebond-core through its public API alone: two sessions back to back through the in-memory
//! network of tests/common, driven the way node drives them. Each test ports a session-level test
//! of the Go reference (core/pair_test.go, stable_test.go, combine_test.go, lane_test.go,
//! status_test.go, wire_test.go) with its scenario and numbers, and asserts what the Go test
//! asserts as far as the public API shows it:
//!
//! - which lane carries bulk: the role the sender's snapshot gives each lane ("spread",
//!   "copies", "standby"), where Go read the scheduler's fields;
//! - lane states, validation, return addresses, fits, counters and events: the snapshot;
//! - keys: the key messages a session sends, its RidTable, and the packets on the wire, which
//!   must carry the peer's rid and open under the key their sender announced (§7.3);
//! - "a bad packet changed nothing": no call on Out, the same snapshot and next(), and then the
//!   same behaviour as an identical session that never saw it.
//!
//! notes/rust.md lists what the public API cannot express.

mod common;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use common::*;
use std::net::SocketAddr;
use wirebond_core::{
    BOND_BYTES, DATA_LEN, EPOCH_BITS, Event, FlowId, HEADER_LEN, MAX_PACKET, MIN, MODE_BANDWIDTH, MS, Mono, Msg,
    OVERHEAD, PROBE_IDLE, RidTable, Role, SEC, SMALL_BIT, STATUS_EVENTS, Session, SessionStatus, SockId, TAG_LEN,
    TYPE_DATA, TYPE_PROBE, VALIDATE_AMP, packet_rid,
};

/// Opens a lane packet with `key` as §7.3 seals it: AES-256-GCM, nonce 0 ‖ rid ‖ counter, the
/// 11-byte header as additional data.
fn open(key: &[u8], pkt: &[u8]) -> Option<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let mut nonce = [0; 12];
    nonce[2..].copy_from_slice(pkt.get(1..HEADER_LEN)?);
    cipher.decrypt(&nonce.into(), Payload { msg: &pkt[HEADER_LEN..], aad: &pkt[..HEADER_LEN] }).ok()
}

/// A data packet for receive id `rid` with counter `ctr` and an arbitrary body (Go's forge).
fn forge(rid: u16, ctr: u64) -> Vec<u8> {
    let mut b = vec![0; HEADER_LEN + DATA_LEN + 20 + TAG_LEN];
    b[0] = TYPE_DATA;
    b[1..3].copy_from_slice(&rid.to_be_bytes());
    b[3..11].copy_from_slice(&ctr.to_be_bytes());
    b
}

/// A snapshot as text, so that two can be compared even if a measurement is NaN.
fn text(st: &SessionStatus) -> String {
    format!("{st:?}")
}

fn arrived(s: &Sent) -> bool {
    matches!(s.fate, Fate::Arrives(_))
}

/// Hands `data` to `side`'s session as a packet on `sock` from `from`, at the time of its last
/// event, outside the network; returns what the session called on Out.
fn inject(p: &mut Pair, side: Side, sock: SockId, from: SocketAddr, data: &[u8]) -> Recorder {
    let mut rec = Recorder::default();
    let Node { engine, rids, last_at, .. } = p.node_mut(side);
    engine.handle(*last_at, Event::Packet { sock, from, data }, rids, &mut rec);
    rec
}

// ---------------------------------------------------------------------------------------------
// Keys

/// Go's TestPairKeyExchange. Each side answers the peer's hello with one key message: a receive
/// id, which its RidTable holds for node's owner key, and a 32-byte send key for epoch 0. The keys
/// are exchanged when every packet a side sends carries the peer's rid and opens under the key it
/// announced, and the peer delivers it (§7.2–§7.4).
#[test]
fn keys_are_exchanged_through_hello_and_key() {
    let mut p = Pair::new(20 * MS);
    p.start();
    p.run(100 * MS);
    p.expect_keys();
    for (side, owner) in [(Side::Client, C_OWNER), (Side::Server, S_OWNER)] {
        let n = p.node(side);
        let k = n.key().expect("checked");
        assert_eq!(n.keys(), 1, "{side:?}: one key message for one hello; sent {:?}", n.ctls);
        assert!(k.rid != 0 && k.e == 0 && k.k.len() == 32, "{side:?}: key message {k:?}");
        assert_eq!(n.rids.lookup(k.rid), Some(owner), "{side:?}: its RidTable names the session's rid {}", k.rid);
    }
    let f = FlowId(1);
    p.app(Side::Client, f, 100);
    p.app(Side::Server, f, 100);
    p.run(100 * MS);
    complete(&p.s, f, 1, 0);
    complete(&p.c, f, 1, 0);
    let mut data = 0;
    for sent in &p.wire {
        let (by, to) = (p.node(sent.by), p.node(sent.by.peer()));
        let rid = to.key().expect("checked").rid;
        assert_eq!(packet_rid(&sent.data), Some(rid), "a packet from the {:?} carries the peer's rid", sent.by);
        let pt = open(&by.key().expect("checked").k, &sent.data).unwrap_or_else(|| {
            panic!(
                "a packet from the {:?} does not open under the key it announced (§7.3): {:02x?}",
                sent.by, sent.data
            )
        });
        if sent.by == Side::Client && sent.data[0] & !SMALL_BIT == TYPE_DATA {
            // §10.3: the sender's flow number (its first flow is 1), then seq from 1, then the
            // whole inner packet.
            assert_eq!(pt[..2], 1u16.to_be_bytes(), "data payload: flow");
            assert_eq!(pt[2..DATA_LEN], 1u32.to_be_bytes(), "data payload: the flow's first seq is 1");
            assert_eq!(pt[DATA_LEN..], inner(f, &[0; 100]), "data payload: the inner packet");
            data += 1;
        }
    }
    assert!(data > 0, "the client's datagram crossed no lane; wire: {} packets", p.wire.len());
}

/// Go's TestPeerNewKey, through the pair: the client's session ended while the server's lived on
/// (its SessionLinger ran out), and its next session between the same two processes sends a new
/// key whose counters start again. The new session's packets are delivered both ways; the old
/// one's, replayed, are not (§7.2).
#[test]
fn a_peer_s_new_session_is_heard_and_its_old_packets_are_not() {
    let mut p = Pair::new(10 * MS);
    p.start();
    p.run(50 * MS);
    p.expect_keys();
    let f = FlowId(1);
    p.stream(Side::Client, f, 20, 100, MS);
    p.run(50 * MS);
    complete(&p.s, f, 20, 0);
    let old: Vec<Sent> = p.wire.iter().filter(|s| s.by == Side::Client && arrived(s)).cloned().collect();
    let old_key = p.c.key().expect("checked").clone();

    p.c.engine.close(&mut p.c.rids);
    assert_eq!(p.c.rids.lookup(old_key.rid), None, "close frees the session's receive id");
    p.c.engine = Session::new(Role::Client, 3, rand(13));
    p.c.ctls.clear();
    p.handle(Side::Client, Ev::Links);
    p.handle(Side::Client, Ev::Control(mode_msg(p.mode)));
    p.connect();
    p.run(100 * MS);
    p.expect_keys();
    let new_key = p.c.key().expect("checked");
    assert_ne!(new_key.k, old_key.k, "a new session draws a new send key");
    assert_eq!(p.c.rids.lookup(new_key.rid), Some(3), "the new session's rid is in the table");

    let f2 = FlowId(2);
    p.stream(Side::Client, f2, 20, 100, MS);
    p.stream(Side::Server, f2, 20, 100, MS);
    p.run(100 * MS);
    complete(&p.s, f2, 20, 0);
    complete(&p.c, f2, 20, 0);
    let got = p.s.got(f).len();
    for (i, sent) in old.iter().enumerate() {
        let rec = inject(&mut p, Side::Server, sent.rx_sock, sent.from.expect("arrived"), &sent.data);
        assert!(rec.calls.is_empty(), "the old session's packet {i} of {} got {:?}", old.len(), rec.calls);
    }
    assert_eq!(p.s.got(f).len(), got, "an old packet was delivered");
}

/// A restarted device (§7.2, A14): a new process has a new inst, so both sides start new sessions
/// with new keys, and the restarted side's RidTable starts afresh, so it may hand out the old rid
/// again. No packet of the old sessions, in either direction, gets anything from the new ones.
#[test]
fn a_restarted_device_accepts_no_old_packet() {
    let mut p = Pair::new(10 * MS);
    p.start();
    p.run(50 * MS);
    p.expect_keys();
    let f = FlowId(1);
    p.stream(Side::Client, f, 20, 100, MS);
    p.stream(Side::Server, f, 20, 100, MS);
    p.run(50 * MS);
    complete(&p.s, f, 20, 0);
    complete(&p.c, f, 20, 0);
    let old: Vec<Sent> = p.wire.iter().filter(|s| arrived(s)).cloned().collect();
    let (old_c, old_s) = (p.c.key().expect("checked").k.clone(), p.s.key().expect("checked").k.clone());

    // xanth restarted: a new process with a new table. The Mac ends its old session at once
    // (§7.6 rule 8) and starts another in its own table.
    p.c.engine.close(&mut p.c.rids);
    p.c.engine = Session::new(Role::Client, 3, rand(13));
    p.s.engine = Session::new(Role::Anchor, S_OWNER, rand(14));
    p.s.rids = RidTable::new();
    for n in [&mut p.c, &mut p.s] {
        n.ctls.clear();
        n.got.clear();
        n.count.clear();
    }
    p.start();
    p.run(100 * MS);
    p.expect_keys();
    assert_ne!(p.c.key().expect("checked").k, old_c, "the client's new session has a new key");
    assert_ne!(p.s.key().expect("checked").k, old_s, "the restarted server has a new key");
    let f2 = FlowId(2);
    p.stream(Side::Client, f2, 20, 100, MS);
    p.stream(Side::Server, f2, 20, 100, MS);
    p.run(100 * MS);
    complete(&p.s, f2, 20, 0);
    complete(&p.c, f2, 20, 0);
    for (i, sent) in old.iter().enumerate() {
        let to = sent.by.peer();
        let rec = inject(&mut p, to, sent.rx_sock, sent.from.expect("arrived"), &sent.data);
        assert!(rec.calls.is_empty(), "old packet {i} to the {to:?} got {:?}", rec.calls);
    }
    assert!(p.s.got(f).is_empty() && p.c.got(f).is_empty(), "an old packet was delivered");
}

/// Go's TestSealOpen: a sealed datagram is OVERHEAD bytes longer; every single-bit flip, a cut and
/// a grown packet are dropped with no call on Out and no change, before any packet authenticated;
/// the genuine packet is delivered once and its replay dropped (§7.3, §7.5, §7.6 rule 1).
#[test]
fn a_packet_opens_only_as_sealed_and_only_once() {
    let mut k = Keyed::new(11, 12);
    let pkt = k.pkt.clone();
    assert_eq!(pkt.len(), OVERHEAD + 28 + 5, "a 5-byte datagram's inner packet sealed: {pkt:02x?}");
    let (before, next) = (text(&k.s.snapshot()), k.s.next());
    for i in 0..pkt.len() {
        for bit in [1, 0x80] {
            let mut b = pkt.clone();
            b[i] ^= bit;
            let rec = k.recv(&b);
            let st = k.s.snapshot();
            assert!(rec.calls.is_empty(), "byte {i} flipped ({bit:#x}): {:?}", rec.calls);
            assert_eq!(text(&st), before, "byte {i} flipped ({bit:#x}) changed the snapshot");
            assert_eq!((st.last_auth, k.s.next()), (None, next), "byte {i} flipped ({bit:#x}) changed the session");
        }
    }
    let cut =
        [vec![], pkt[..HEADER_LEN + TAG_LEN - 1].to_vec(), pkt[..pkt.len() - 1].to_vec(), [&pkt[..], &[0]].concat()];
    for b in cut {
        assert!(k.recv(&b).calls.is_empty(), "a {}-byte packet got calls", b.len());
    }
    assert_eq!(text(&k.s.snapshot()), before, "a cut or grown packet changed the snapshot");
    let rec = k.recv(&pkt);
    assert_eq!(rec.delivered(), [(KEYED_FLOW, &b"audio"[..])], "the genuine packet: {:?}", rec.calls);
    let rec = k.recv(&pkt);
    assert!(rec.calls.is_empty(), "the replay got {:?}", rec.calls);
}

/// Go's TestForgedCounter: packets with the right rid and counters in any epoch, the next one
/// included, change nothing and get nothing (§7.2, §7.5 step 1).
#[test]
fn forged_counters_in_any_epoch_change_nothing() {
    let mut k = Keyed::new(11, 12);
    assert_eq!(packet_rid(&k.pkt), Some(k.s_key.rid), "the client's packet carries the server's rid");
    let (before, next) = (text(&k.s.snapshot()), k.s.next());
    for ctr in [u64::MAX, 1 << EPOCH_BITS, (1 << EPOCH_BITS) + 1, 2 << EPOCH_BITS, 5] {
        let rec = k.recv(&forge(k.s_key.rid, ctr));
        assert!(rec.calls.is_empty(), "forged counter {ctr:#x} got {:?}", rec.calls);
        assert_eq!(text(&k.s.snapshot()), before, "forged counter {ctr:#x} changed the snapshot");
        assert_eq!(k.s.next(), next, "forged counter {ctr:#x} changed next()");
    }
    assert_eq!(k.recv(&k.pkt.clone()).delivered().len(), 1, "the genuine packet still opens");
}

/// Go's TestPairForged and more: real packets replayed, or with a bit flipped in the type, the rid
/// (a wrong rid), the counter, the payload or the tag; packets sealed for the other side; cut,
/// grown and oversized packets; garbage with the right header; forged counters in every epoch.
/// All are dropped with no reply and no call on Out, change neither the snapshot nor next(), and
/// afterwards the session does exactly what an identical session that never saw them does
/// (§7.5, §7.6 rules 1 and 3).
#[test]
fn forged_replayed_tampered_and_wrong_rid_packets_change_nothing() {
    let f = FlowId(1);
    let setup = || {
        let mut p = Pair::new(10 * MS);
        p.start();
        p.run(50 * MS);
        p.expect_keys();
        p.stream(Side::Client, f, 50, 100, MS);
        p.run(50 * MS);
        p
    };
    let (mut a, mut b) = (setup(), setup());
    assert!(a.wire == b.wire, "two runs of one script differ, so the comparison below means nothing");
    let got = a.s.got(f).len();
    assert!(got > 0, "nothing was delivered before the test; wire: {} packets", a.wire.len());

    let at = a.s.last_at; // a snapshot reads the session's own clock: hold it there
    let rid = a.s.key().expect("checked").rid;
    let mut bad: Vec<(SockId, SocketAddr, Vec<u8>)> = Vec::new();
    for sent in &a.wire {
        // Only what has arrived is a replay; the rest would be a first arrival.
        let Fate::Arrives(t) = sent.fate else { continue };
        if sent.by != Side::Client || t > at {
            continue;
        }
        let from = sent.from.expect("arrived");
        bad.push((sent.rx_sock, from, sent.data.clone()));
        for i in [0, 1, 2, 5, HEADER_LEN, sent.data.len() - 1] {
            let mut x = sent.data.clone();
            x[i] ^= 0x10;
            bad.push((sent.rx_sock, from, x));
        }
    }
    let replays = bad.len();
    assert!(replays > 50, "only {replays} packets to replay");
    let sample = bad[0].2.clone();
    for sent in a.wire.iter().filter(|s| s.by == Side::Server).take(10) {
        bad.push((MESH_SOCK, C_MESH, sent.data.clone())); // sealed for the client
    }
    let mut oversized = sample.clone();
    oversized.resize(MAX_PACKET + 1, 0);
    let mut r = DetRand(99);
    let mut odd = vec![vec![], vec![TYPE_DATA], sample[..HEADER_LEN + TAG_LEN - 1].to_vec(), oversized];
    odd.push(sample[..sample.len() - 1].to_vec());
    odd.push([&sample[..], &[0]].concat());
    for len in [HEADER_LEN + TAG_LEN, 64, 300, MAX_PACKET] {
        let mut x = vec![0; len];
        r.fill(&mut x);
        x[0] = if len == 64 { TYPE_PROBE } else { TYPE_DATA };
        x[1..3].copy_from_slice(&rid.to_be_bytes());
        odd.push(x);
    }
    for ctr in [u64::MAX, 1 << EPOCH_BITS, (1 << EPOCH_BITS) + 1, 2 << EPOCH_BITS, 5] {
        odd.push(forge(rid, ctr));
    }
    bad.extend(odd.into_iter().map(|x| (MESH_SOCK, C_MESH, x)));

    let (before, next) = (text(&a.snap(Side::Server)), a.s.engine.next());
    for (i, (sock, from, data)) in bad.iter().enumerate() {
        let what = if i < replays { "a replayed or tampered packet" } else { "a forged or misdirected packet" };
        let rec = inject(&mut a, Side::Server, *sock, *from, data);
        assert!(rec.calls.is_empty(), "bad packet {i}, {what} of {} bytes, got {:?}", data.len(), rec.calls);
        assert_eq!(text(&a.snap(Side::Server)), before, "bad packet {i}, {what}, changed the snapshot");
        assert_eq!(a.s.engine.next(), next, "bad packet {i}, {what}, changed next()");
    }
    assert_eq!(a.s.got(f).len(), got, "a bad packet was delivered");

    for p in [&mut a, &mut b] {
        p.stream(Side::Client, f, 50, 100, MS);
        p.stream(Side::Server, f, 50, 100, MS);
        p.run(100 * MS);
    }
    if let Some(i) = (0..a.wire.len().min(b.wire.len())).find(|&i| a.wire[i] != b.wire[i]) {
        panic!(
            "after the bad packets the session behaves differently: packet {i} is\n{:?}\nwhere a session that never saw them sent\n{:?}",
            a.wire[i], b.wire[i]
        );
    }
    assert_eq!(a.wire.len(), b.wire.len(), "after the bad packets the session sends a different number of packets");
    complete(&a.s, f, 100, 0);
    complete(&a.c, f, 50, 0);
}

// ---------------------------------------------------------------------------------------------
// Lanes

/// Go's TestPairMeshFlow: with no links, a flow runs both ways over the Tailscale lane, in order.
#[test]
fn a_flow_runs_both_ways_over_the_tailscale_lane() {
    let mut p = Pair::new(15 * MS);
    p.start();
    p.run(100 * MS);
    p.expect_keys();
    let f = FlowId(1);
    p.stream(Side::Client, f, 200, 300, MS);
    p.stream(Side::Server, f, 200, 1400, MS);
    p.run(100 * MS);
    in_order(&p.s, f, 200, 0);
    in_order(&p.c, f, 200, 0);
    for side in [Side::Client, Side::Server] {
        let st = p.snap(side);
        assert_eq!(st.lanes.len(), 1, "{side:?}: only the Tailscale lane; {}", describe(&st));
        assert_state(&st, MESH_LANE, "up", "the Tailscale lane");
        assert_eq!(lane(&st, MESH_LANE).map(|l| l.kind), Some("mesh"));
    }
    assert!(
        p.wire.iter().all(|s| s.sock == MESH_SOCK && s.rx_sock == MESH_SOCK),
        "every packet between the Tailscale lane's sockets"
    );
}

/// Go's TestLanesUp: in the hotel every lane comes up and is validated on both sides; with no
/// traffic both direct lanes take copies, one per bottleneck group, and the Tailscale lane, which
/// shares the default link's group, does not; the server learned each lane's NAT mapping (§6.1,
/// §6.3, §8.4, §10.1).
#[test]
fn the_hotel_s_lanes_come_up() {
    let mut p = Pair::hotel();
    p.run(2 * SEC);
    for side in [Side::Client, Side::Server] {
        let st = p.snap(side);
        for id in [MESH_LANE, WIFI, PHONE] {
            assert_state(&st, id, "up", &format!("{side:?}"));
        }
    }
    let st = p.snap(Side::Client);
    assert_eq!(
        [role(&st, MESH_LANE), role(&st, WIFI), role(&st, PHONE)],
        ["standby", "copies", "copies"],
        "the client's copy set: one lane per group, not the Tailscale lane; {}",
        describe(&st)
    );
    let st = p.snap(Side::Server);
    let (w, ph, m) = (lane(&st, WIFI).expect("up"), lane(&st, PHONE).expect("up"), lane(&st, MESH_LANE).expect("up"));
    assert_eq!((w.peer, ph.peer, m.peer), (Some(C_WIFI_NAT), Some(C_PHONE_NAT), Some(C_MESH)), "return addresses");
    assert_eq!((w.kind, &w.local[..], &w.remote[..], w.metered), ("direct", "Ethernet", "Wi-Fi", false));
    assert_eq!((ph.kind, &ph.local[..], &ph.remote[..], ph.metered), ("direct", "Ethernet", "iPhone", true));
    assert_eq!((m.kind, &m.local[..], &m.remote[..], m.sock), ("mesh", "", "", MESH_SOCK));
}

/// Go's TestStall: a stall on the client's Wi-Fi lane is seen at the receiver as missing copies
/// and reported at once, so the sender marks the lane suspect within about 100 ms (§8.4); it goes
/// down after DOWN_AFTER, input survives on the iPhone lane, and the lane comes back up after
/// UP_AFTER of clean echoes.
#[test]
fn a_stall_is_seen_within_about_a_round_trip() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    p.run(SEC);
    let f = FlowId(1);
    p.stream(Side::Client, f, 200, 100, 5 * MS);
    p.path(C_WIFI, S_ETH).down = true;
    let start = p.now;
    while lane(&p.snap(Side::Client), WIFI).is_some_and(|l| l.state == "up") {
        p.stream(Side::Client, f, 1, 100, 5 * MS);
        assert!(p.now - start <= SEC, "stall not detected in 1 s; {}", describe(&p.snap(Side::Client)));
    }
    let d = p.now - start;
    assert!(d <= 120 * MS, "the sender saw the stall after {}; want ≤ 120 ms", ms(d));
    p.stream(Side::Client, f, 450, 100, 5 * MS);
    assert_state(&p.snap(Side::Client), WIFI, "down", "2 s into the stall");
    p.run(100 * MS);
    complete(&p.s, f, p.c.count(f), 0);
    p.path(C_WIFI, S_ETH).down = false;
    p.stream(Side::Client, f, 300, 100, 5 * MS);
    assert_state(&p.snap(Side::Client), WIFI, "up", "1.5 s after the stall ended");
}

/// Go's TestRebind: a NAT rebinding moves the server's return address on the client's next probe,
/// and the new address is validated within about one RTT (§7.6 rule 2, A7).
#[test]
fn a_nat_rebinding_moves_the_return_address() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    p.run(SEC);
    let f = FlowId(1);
    p.stream(Side::Client, f, 100, 100, 5 * MS);
    let moved: SocketAddr = "203.0.113.9:62000".parse().expect("address");
    p.nat.insert(C_WIFI, moved);
    p.stream(Side::Client, f, 30, 100, 5 * MS);
    let st = p.snap(Side::Server);
    assert_state(&st, WIFI, "up", "150 ms after the rebinding");
    assert_eq!(lane(&st, WIFI).and_then(|l| l.peer), Some(moved), "the Wi-Fi lane's return address");
    p.run(100 * MS);
    complete(&p.s, f, 130, 0);
}

/// Go's TestRelayedProbe, as far as the public API shows it: an on-path network relays a real
/// probe from a victim's address before the real one arrives. The lane keeps its validated return
/// address, the victim gets probes only, at most VALIDATE_AMP × the bytes received from it, and
/// the flow carries on (§7.6 rule 2, A6). The claim on the victim's address is not visible.
#[test]
fn a_relayed_probe_gets_the_victim_probes_only() {
    let mut p = Pair::hotel();
    p.run(SEC);
    let f = FlowId(1);
    p.stream(Side::Client, f, 100, 100, 5 * MS);
    let victim: SocketAddr = "192.0.2.200:5555".parse().expect("address");
    let mut relayed = None;
    while relayed.is_none() {
        assert!(p.now < 5 * SEC, "no probe from the client's Wi-Fi in flight");
        let mark = p.wire.len();
        p.run(MS);
        relayed = p.wire[mark..]
            .iter()
            .rev()
            .find(|s| s.by == Side::Client && s.rx_sock == 11 && s.from == Some(C_WIFI_NAT) && s.data[0] == TYPE_PROBE)
            .map(|s| s.data.clone());
    }
    let relayed = relayed.expect("found");
    let mark = p.wire.len();
    p.handle(Side::Server, Ev::Packet { sock: 11, from: victim, data: relayed.clone() });
    let st = p.snap(Side::Server);
    assert_state(&st, WIFI, "up", "after the relayed probe");
    assert_eq!(lane(&st, WIFI).and_then(|l| l.peer), Some(C_WIFI_NAT), "after the relayed probe; {}", describe(&st));
    p.stream(Side::Client, f, 100, 100, 5 * MS);
    let to_victim: Vec<&Sent> = p.wire[mark..].iter().filter(|s| s.dst == victim).collect();
    assert!(to_victim.iter().all(|s| s.data[0] == TYPE_PROBE), "data sent to an unvalidated address");
    let sent: usize = to_victim.iter().map(|s| s.data.len()).sum();
    assert!(
        sent > 0 && sent as u64 <= VALIDATE_AMP * relayed.len() as u64,
        "{sent} bytes sent to the victim for {} received",
        relayed.len()
    );
    let st = p.snap(Side::Server);
    assert_state(&st, WIFI, "up", "after the real probes");
    assert_eq!(lane(&st, WIFI).and_then(|l| l.peer), Some(C_WIFI_NAT), "after the real probes");
    p.run(100 * MS);
    complete(&p.s, f, 200, 0);
}

/// Go's TestIdle: an idle session sends one probe every PROBE_IDLE on one unmetered direct lane and
/// the server answers each; nothing goes on the metered lane, which is dormant; idle, the client
/// asks for a tick for its next idle probe and the server for none; the first datagram after idle
/// goes through and wakes every lane (§8.1, A8).
#[test]
fn an_idle_session_probes_one_unmetered_lane() {
    let mut p = Pair::hotel();
    p.run(SEC);
    p.stream(Side::Client, FlowId(1), 50, 100, 5 * MS);
    p.run(10 * SEC);
    let mark = p.wire.len();
    p.run(10 * MIN);
    let idle = &p.wire[mark..];
    let bytes: usize = idle.iter().map(|s| s.data.len() + 28).sum();
    let metered = idle.iter().filter(|s| s.rx_sock == 12 || s.from == Some(C_PHONE_NAT)).count();
    assert!(
        bytes <= 100_000 / 6 && metered == 0 && idle.len() >= 40,
        "idle 10 min: {bytes} bytes, {} packets, {metered} on the metered lane",
        idle.len()
    );
    let st = p.snap(Side::Client);
    assert_state(&st, WIFI, "up", "the idle-probe lane");
    assert_state(&st, PHONE, "dormant", "the metered lane while idle");
    let wait = p.c.engine.next() - p.now;
    assert!(wait > 0 && wait <= PROBE_IDLE, "the idle client's next tick in {}", ms(wait));
    assert_eq!(p.s.engine.next(), Mono::MAX, "an idle server asks for no tick");
    let f = FlowId(2);
    p.stream(Side::Client, f, 100, 100, 5 * MS);
    p.run(100 * MS);
    complete(&p.s, f, 100, 0);
    assert_state(&p.snap(Side::Client), PHONE, "up", "after the session woke");
}

/// Go's TestWake: a wake re-probes every lane but turns none suspect on its own (§8.4).
#[test]
fn a_wake_turns_no_lane_suspect() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    p.run(SEC);
    let f = FlowId(1);
    p.stream(Side::Client, f, 100, 100, 5 * MS);
    p.handle(Side::Client, Ev::Wake);
    for i in 0..100 {
        p.stream(Side::Client, f, 1, 100, 5 * MS);
        let st = p.snap(Side::Client);
        assert_eq!(st.lanes.len(), 3, "{}", describe(&st));
        assert!(st.lanes.iter().all(|l| l.state == "up"), "{} ms after a wake: {}", 5 * (i + 1), describe(&st));
    }
}

/// Go's TestLinkPull: a link that carries bulk disappears mid-stream (the Wi-Fi cable or radio);
/// the other lane carries every packet already, nothing is lost, and when the link returns its
/// lane is up again within seconds (§10.2, §17.5 field run 4).
#[test]
fn a_pulled_link_loses_nothing() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_carrying(&p.snap(Side::Server), &[WIFI, PHONE], "before the pull");
    let wifi = p.c.links.remove(0);
    p.relink(Side::Client);
    p.play(SEC, &mut [&mut video, &mut audio, &mut input]);
    let st = p.snap(Side::Server);
    assert_carrying(&st, &[PHONE], "after the Wi-Fi link went");
    assert!(lane(&st, WIFI).is_none(), "the Wi-Fi lane is gone; {}", describe(&st));
    p.c.links.insert(0, wifi);
    p.relink(Side::Client);
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_state(&p.snap(Side::Server), WIFI, "up", "2 s after the link returned");
    p.run(SEC);
    in_order(&p.c, VIDEO, p.s.count(VIDEO), 0);
    complete(&p.c, AUDIO, p.s.count(AUDIO), 0);
    complete(&p.s, INPUT, p.c.count(INPUT), 0);
}

// ---------------------------------------------------------------------------------------------
// performance

/// Go's TestStableDirect, in performance: bulk starts on the Tailscale lane, the only one, and
/// moves to a direct lane as soon as one is up, losing nothing (§11.1: the Tailscale lane only
/// for a group with no usable direct lane).
#[test]
fn a_direct_lane_takes_bulk_from_the_tailscale_lane() {
    let mut p = Pair::new(33 * MS);
    p.keep_wire = false;
    p.start();
    p.run(100 * MS); // datagrams before the peer's key are dropped
    p.expect_keys();
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_carrying(&p.snap(Side::Server), &[MESH_LANE], "with no link, bulk on the Tailscale lane");
    p.add_link(Side::Client, 1, "Wi-Fi", false, true, C_WIFI);
    p.add_link(Side::Server, 1, "Ethernet", false, false, S_ETH);
    p.link(C_WIFI, S_ETH, Path::new(20 * MS));
    p.relink(Side::Client);
    p.relink(Side::Server);
    p.play(3 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_carrying(&p.snap(Side::Server), &[WIFI], "3 s after a direct lane appeared");
    p.run(SEC);
    in_order(&p.c, VIDEO, p.s.count(VIDEO), 0);
    complete(&p.c, AUDIO, p.s.count(AUDIO), 0);
    complete(&p.s, INPUT, p.c.count(INPUT), 0);
}

/// The hotel Wi-Fi's return path blackholes under Moonlight at 10 Mbps. Every packet already goes
/// on both groups' lanes, so nothing moves or rewinds, and neither video nor the copied flows lose
/// a datagram (§11.1).
#[test]
fn a_stall_on_one_lane_loses_nothing() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(3 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_carrying(&p.snap(Side::Server), &[WIFI, PHONE], "every packet on both groups' lanes");
    p.path(S_ETH, C_WIFI).down = true;
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    p.run(SEC);
    in_order(&p.c, VIDEO, p.s.count(VIDEO), 0);
    complete(&p.c, AUDIO, p.s.count(AUDIO), 0);
    complete(&p.s, INPUT, p.c.count(INPUT), 0);
    let st = p.snap(Side::Client);
    assert_eq!(st.unrecovered, 0, "gaps the client gave up on; {}", describe(&st));
    assert_eq!(p.snap(Side::Server).rewinds, 0, "performance never rewinds");
}

/// With 1% loss both ways on the hotel Wi-Fi, the iPhone lane's copy of every packet makes up for
/// what the Wi-Fi loses: every datagram of every flow arrives, in order (§11.1, §10.1).
#[test]
fn random_loss_on_one_lane_loses_nothing() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(SEC, &mut [&mut video, &mut audio, &mut input]);
    p.path(S_ETH, C_WIFI).loss = 0.01;
    p.path(C_WIFI, S_ETH).loss = 0.01;
    for _ in 0..100 {
        p.play(100 * MS, &mut [&mut video, &mut audio, &mut input]);
        assert_carrying(&p.snap(Side::Server), &[WIFI, PHONE], "under 1% random loss");
    }
    p.run(SEC);
    complete(&p.c, AUDIO, p.s.count(AUDIO), 0);
    complete(&p.s, INPUT, p.c.count(INPUT), 0);
    in_order(&p.c, VIDEO, p.s.count(VIDEO), 0);
}

/// A lossy lane costs nothing (§10.1): with 2% loss both ways on the iPhone lane, every datagram
/// of every flow arrives, and each side measures about 2% loss on its sending over the iPhone
/// lane (§8.3), as status shows it.
#[test]
fn a_lossy_lane_costs_nothing_and_shows_its_loss() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(SEC, &mut [&mut video, &mut audio, &mut input]);
    p.path(S_ETH, C_PHONE).loss = 0.02;
    p.path(C_PHONE, S_ETH).loss = 0.02;
    let mut loss = [0.0; 2];
    for _ in 0..100 {
        p.play(100 * MS, &mut [&mut video, &mut audio, &mut input]);
        for (i, side) in [Side::Client, Side::Server].into_iter().enumerate() {
            let st = p.snap(side);
            loss[i] += lane(&st, PHONE).map_or(0.0, |l| l.out.loss) / 100.0;
        }
    }
    p.run(SEC);
    in_order(&p.c, VIDEO, p.s.count(VIDEO), 0);
    complete(&p.c, AUDIO, p.s.count(AUDIO), 0);
    complete(&p.s, INPUT, p.c.count(INPUT), 0);
    for (side, x) in ["client", "server"].iter().zip(loss) {
        assert!(
            (0.01..=0.03).contains(&x),
            "the {side}'s iPhone lane reported {x:.4} loss on average; the path loses 0.02"
        );
    }
}

/// A stream that outgrows the faster lane's shaper (the hotel's: 20 Mbps here, with a 180 ms
/// buffer) stays on both lanes: the Wi-Fi drops its excess copies at its own pacer and holds
/// nothing back, so the iPhone lane's copies deliver every datagram (§9, §11.1).
#[test]
#[ignore = "SPEC 14.1's known limitation, kept by the owner: the wait counts base OWDs only, so \
            a copy queued in the sender's pacer on the lane with the lower base OWD can arrive \
            after its gap was given up, and is then delivered out of order. Here the iPhone \
            (47 ms) joins the spread at counter 415 and brings seq 298 at 479.6 ms while the \
            Wi-Fi's copy of 297 waits in the sender's Wi-Fi pacer, so 297 comes late. A wait by \
            the newest probe's OWD does not fix it (probes bypass the pacer: the Wi-Fi's stays \
            32.1 ms) and fails netsim's A2 (seeds 1-2 worst frame 215 and 263 ms)"]
fn a_lane_too_small_holds_nothing_back() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    let w = p.path(S_ETH, C_WIFI);
    (w.rate, w.buf) = (20e6, 180 * MS);
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(3 * SEC, &mut [&mut video, &mut audio, &mut input]);
    video.gap /= 3; // 30 Mbps
    p.play(10 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_carrying(&p.snap(Side::Server), &[WIFI, PHONE], "after video outgrew the Wi-Fi shaper");
    p.run(SEC);
    in_order(&p.c, VIDEO, p.s.count(VIDEO), 0);
    complete(&p.c, AUDIO, p.s.count(AUDIO), 0);
    complete(&p.s, INPUT, p.c.count(INPUT), 0);
    let st = p.snap(Side::Client);
    assert_eq!(st.unrecovered, 0, "gaps the client gave up on; {}", describe(&st));
}

/// Go's TestBlind: every lane's return direction is blackholed for 2 s, so the sender sees no
/// echo; it sends bulk on two lanes in different bottleneck groups until echoes return, then on
/// one per group again, and video keeps arriving in order (§8.4, A20). The Tailscale lane shares
/// the default link's group (§6.1), so the two are the iPhone's and Wi-Fi's or the Tailscale lane.
#[test]
fn a_blind_sender_uses_two_lanes_in_different_groups() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    let back = [(C_WIFI, S_ETH), (C_PHONE, S_ETH), (C_MESH, S_MESH)];
    for (a, b) in back {
        p.path(a, b).down = true;
    }
    p.play(SEC, &mut [&mut video, &mut audio, &mut input]);
    let st = p.snap(Side::Server);
    let two = carrying(&st);
    assert!(
        two.len() == 2 && two.contains(&PHONE) && (two.contains(&WIFI) || two.contains(&MESH_LANE)),
        "blind for 1 s: bulk on {two:x?}; want two lanes in different groups; {}",
        describe(&st)
    );
    p.play(SEC, &mut [&mut video, &mut audio, &mut input]);
    for (a, b) in back {
        p.path(a, b).down = false;
    }
    p.play(4 * SEC, &mut [&mut video, &mut audio, &mut input]);
    assert_carrying(&p.snap(Side::Server), &[WIFI, PHONE], "4 s after echoes returned");
    p.run(SEC);
    in_order(&p.c, VIDEO, p.s.count(VIDEO), 0);
    complete(&p.c, AUDIO, p.s.count(AUDIO), 0);
}

/// Go's TestSnapshot, in performance: status shows both groups' lanes carrying bulk and the
/// Tailscale lane on standby, the flows' classes, the metered rate (every packet crosses the
/// iPhone), no problem, and the bytes held within BOND_BYTES (§14.2–§14.4).
#[test]
fn status_shows_bulk_on_every_group() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(3 * SEC, &mut [&mut video, &mut audio, &mut input]);
    video.gap /= 3;
    p.play(3 * SEC, &mut [&mut video, &mut audio, &mut input]);
    let st = p.snap(Side::Server);
    assert_eq!(
        [role(&st, PHONE), role(&st, WIFI), role(&st, MESH_LANE)],
        ["spread", "spread", "standby"],
        "{}",
        describe(&st)
    );
    // Flows by direction and key (§13): the server sends video and audio and receives input.
    let class = |tx, f: FlowId| {
        st.flows.iter().find(|x| x.tx == tx && x.key.is_some_and(|k| k.sport == f.0)).map_or("", |x| x.class)
    };
    assert_eq!(
        [class(true, VIDEO), class(true, AUDIO), class(false, INPUT)],
        ["bulk", "small", "small"],
        "{:?}",
        st.flows
    );
    // 10 Mbps for 3 s and 30 Mbps for 3 s of the last minute on the iPhone is 2 Mbps over it:
    // 0.9 GB/h, and a little more with the packets' overhead and the copies.
    assert!((0.85..=1.2).contains(&st.metered_gb_per_hour), "metered {:.2} GB/h", st.metered_gb_per_hour);
    assert!(st.problems.is_empty(), "problems {:?}", st.problems);
    assert!(
        st.events.len() <= STATUS_EVENTS && st.bytes > 0 && st.bytes <= BOND_BYTES,
        "events {}, bytes {}",
        st.events.len(),
        st.bytes
    );
}

// ---------------------------------------------------------------------------------------------
// bandwidth

/// Go's TestCombine: bulk larger than either lane (45 Mbps over 20 and 40 Mbps shapers with 60 ms
/// buffers) spreads over both, at about the sum of their rates, and reaches the receiver in
/// order (§11.2, §10.3).
#[test]
fn combine_spreads_bulk_over_two_lanes_and_keeps_its_order() {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    p.set_mode(MODE_BANDWIDTH);
    for (to, rate) in [(C_WIFI, 20e6), (C_PHONE, 40e6)] {
        let path = p.path(S_ETH, to);
        (path.rate, path.buf) = (rate, 60 * MS);
    }
    let (mut video, mut audio, mut input) = moonlight(45.0);
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    let st = p.snap(Side::Server);
    assert_eq!(st.mode, MODE_BANDWIDTH, "the server's mode after the client's mode message");
    let (sent, got) = (p.s.count(VIDEO), p.c.got(VIDEO).len());
    p.play(4 * SEC, &mut [&mut video, &mut audio, &mut input]);
    let st = p.snap(Side::Server);
    assert_eq!([role(&st, WIFI), role(&st, PHONE)], ["spread", "spread"], "{}", describe(&st));
    p.run(200 * MS);
    let steady = &p.c.got(VIDEO)[got..]; // after the rates' startup
    let sent = p.s.count(VIDEO) - sent;
    assert!(
        steady.len() as f64 >= 0.9 * f64::from(sent),
        "delivered {} of {sent} datagrams in steady state",
        steady.len()
    );
    let (mut late, mut top) = (0, -1);
    for d in steady {
        let i = i64::from(index(d));
        if i < top {
            late += 1;
        } else {
            top = i;
        }
    }
    assert!(late <= steady.len() / 500, "{late} of {} delivered out of order in steady state", steady.len());
    let st = p.snap(Side::Server);
    for id in [WIFI, PHONE] {
        let bytes = lane(&st, id).map_or(0, |l| l.out.sent_bytes);
        assert!(bytes >= 5_000_000, "lane {id:#06x} carried only {bytes} bytes; {}", describe(&st));
    }
    complete(&p.c, AUDIO, p.s.count(AUDIO), 0);
    complete(&p.s, INPUT, p.c.count(INPUT), 0);
}

/// The key message's fields, as the control channel carries them; checked here so that a session
/// that answers hello with something else fails with the message, not later.
#[test]
fn a_hello_is_answered_with_a_key() {
    let mut s = Session::new(Role::Anchor, S_OWNER, rand(2));
    let mut rids = RidTable::new();
    let mut rec = Recorder::default();
    let hello = Msg { t: "hello".into(), v: wirebond_core::PROTOCOL, inst: 1, mode: "stable".into(), ..Msg::default() };
    s.handle(0, Event::Control(&hello), &mut rids, &mut rec);
    let keys = rec.keys();
    assert_eq!(keys.len(), 1, "one key message for a hello; calls {:?}", rec.calls);
    let k = &keys[0];
    assert_eq!((k.t.as_str(), k.e, k.k.len()), ("key", 0, 32), "{k:?}");
    assert_eq!(rids.lookup(k.rid), Some(S_OWNER), "the rid is the session's in node's table");
    assert!(rec.sends().is_empty(), "no lane packet before the peer's key: {:?}", rec.calls);
    s.close(&mut rids);
    assert_eq!(rids.lookup(k.rid), None, "close frees the rid");
}
