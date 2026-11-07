//! Tests of flow.rs and rewind.rs. Owner: C5. Ports from core/rate_test.go: TestReorder (its
//! lanes are pushed into `s.lanes.live` and named by LaneIx; each plaintext is copied into
//! `s.rxbuf` before `data`). New (R2-12): FlowEnd returns a flow's held buffers to the pool and
//! frees its slot, so flows ended one after another never exhaust MAX_FLOWS. Also new: the skew
//! estimate learns only from the last gap given up and stays capped (R2-13), order across the
//! seq wrap (R2-24), rule 2's lane list, classes, the byte cap, rewind, and the send side (small
//! everywhere until confirmed, the copy set, bulk on the scheduler's lanes). And (harden: group
//! C) a small packet that passes held bulk of its own flow, with §17.2's order invariant checked
//! per call as netsim checks it, also against `reorder`'s model with the flow turning small. And
//! (review fixes: flow-half-range) a seq exactly 2^31 ahead jumps, and no stepping loop spins.
//!
//! The tests drive a bare Session (no links, awake, with lanes pushed in), and run the reorder
//! timers as `tick` does (`timers`), so they reach no other module's scheduling. The send-side
//! tests check only what holds whatever the pacer's timing: what each lane sent or queued.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use super::{FILL_MAX, Flow, FlowLane, REORDER_CAP};
use crate::buf::Pkt;
use crate::estimate::{NO_MIN, Winmin};
use crate::flow::FLOW_LANES;

/// The IPv4 and UDP headers `pair::inner` puts before a datagram.
const INNER_HDR: usize = 28;
use crate::lane::{Lane, LaneIx, State};
use crate::pair::*;
use crate::session::{Mode, NEVER};
use crate::{
    DATA_LEN, Event, FLOW_IDLE, FlowId, MAX_FLOWS, MAX_INNER, MAX_PACKET, MS, Mono, OVERHEAD, QUEUE_AGE, REORDER_MAX,
    REORDER_RESCUE, REORDER_SLOTS, Role, SEC, SKEW_DECAY, SMALL_BIT, SMALL_WINDOW, Session, SockId, TYPE_DATA,
};

/// A server session with no keys, links or lanes, awake (data's `touch` would otherwise wake it
/// through C3's `activate`).
fn session() -> Session {
    let mut s = Session::new(Role::Anchor, 12, DetRand::rand(12));
    s.active = true;
    s
}

/// Adds a lane whose inbound OWD (average and newest probe's) is `owd`, with deviation `dev`.
fn lane(s: &mut Session, owd: Mono, dev: Mono) -> LaneIx {
    let i = s.lanes.len();
    let mut l = Lane::new(i as u16 + 1, i as SockId + 2);
    (l.in_owd, l.in_dev, l.rx_owd) = (owd, dev, owd / 1_000);
    s.lanes.live.push(l);
    LaneIx::at(i)
}

/// Hands `data` seq `seq` of flow `id` as `packet` does, its plaintext in `rxbuf`; the datagram
/// is the seq again.
fn rx(s: &mut Session, out: &mut Acts, now: Mono, l: Option<LaneIx>, typ: u8, id: FlowId, seq: u32) {
    // flow (2) + seq (4), then one inner packet whose datagram is the seq (§10.3).
    let p = crate::pair::inner(id, &seq.to_be_bytes());
    s.rxbuf[..2].copy_from_slice(&id.0.to_be_bytes());
    s.rxbuf[2..DATA_LEN].copy_from_slice(&seq.to_be_bytes());
    s.rxbuf[DATA_LEN..DATA_LEN + p.len()].copy_from_slice(&p);
    s.now = now;
    s.data(out, now, l, typ, DATA_LEN + p.len());
}

/// The slot of the flow I send as `pair::inner` builds flow `id`'s packets, opened with `open`
/// (§13: a flow I send is found by its key and numbered by me).
fn txf(s: &mut Session, id: FlowId, open: bool) -> Option<usize> {
    let k = crate::flow::Key::parse(&crate::pair::inner(id, &[]))?;
    if open { s.tx_flow(k) } else { s.tx_flows.get(&k).copied() }
}

/// A bulk datagram arriving on lane `l`: §7.5 step 3 records the lane first (a flow not yet open
/// is not recorded), then `data`.
fn arrive(s: &mut Session, out: &mut Acts, now: Mono, l: LaneIx, id: FlowId, seq: u32) {
    if let Some(f) = s.flow(id, false) {
        s.flows[f].carried(l, now, seq);
    }
    rx(s, out, now, Some(l), TYPE_DATA, id, seq);
}

/// The reorder timers, as `tick` runs them: from NEVER, every flow that holds datagrams.
fn timers(s: &mut Session, out: &mut Acts, now: Mono) {
    s.next = NEVER;
    for f in 0..s.flows.len() {
        if s.flows[f].used && s.flows[f].n > 0 {
            s.release(out, now, f);
        }
    }
}

/// The seqs delivered to flow `id`, taken out of `out`.
fn seqs(out: &mut Acts, id: FlowId) -> Vec<u32> {
    let got =
        delivered(&out.0).into_iter().filter(|d| d.0 == id).map(|d| u32::from_be_bytes(d.1[..4].try_into().unwrap()));
    let got = got.collect();
    out.0.clear();
    got
}

/// TestReorder: per-flow order against a sorted-delivery model: bulk seqs spread over 2–4 FIFO
/// lanes with different delays, some lost, some copied (§10.3). Every arrival is delivered once;
/// those delivered in order are increasing; a late one is one whose gap was skipped.
#[test]
fn reorder() {
    for lanes in 2..=4usize {
        let mut s = session();
        let mut r = DetRand(lanes as u64);
        let ls: Vec<LaneIx> = (0..lanes).map(|i| lane(&mut s, (10 + 15 * i as Mono) * MS, 2 * MS)).collect();
        let mut arrivals = Vec::new(); // (at, lane, seq)
        let mut last = vec![0; lanes];
        for seq in 1..=3000u32 {
            let sent = Mono::from(seq) * (MS / 2);
            for i in 0..lanes {
                if !r.next().is_multiple_of(lanes as u64) && i != (seq as usize / 200) % lanes || r.float() < 0.01 {
                    continue; // each seq rides one lane in turn, sometimes copied, 1% lost
                }
                let at = Mono::max(last[i], sent + s.lanes[ls[i]].in_owd + (r.next() % 3) as Mono * MS);
                last[i] = at;
                arrivals.push((at, i, seq));
            }
        }
        arrivals.sort_by_key(|a| a.0);
        let id = crate::FlowId(1);
        let (mut got, mut seen, mut now, mut out) = (Vec::new(), BTreeSet::new(), 0, Acts::default());
        for &(at, i, seq) in &arrivals {
            while now < at {
                // timers due before this arrival
                now = at.min((now + 1).max(s.next));
                if now >= s.next {
                    timers(&mut s, &mut out, now);
                }
                got.extend(seqs(&mut out, id));
            }
            if let Some(f) = s.flow(id, false) {
                s.flows[f].carried(ls[i], now, seq); // §7.5 step 3 records every copy on its lane
            }
            if !seen.insert(seq) {
                continue; // the replay window drops later copies before reordering
            }
            rx(&mut s, &mut out, now, Some(ls[i]), TYPE_DATA, id, seq);
            got.extend(seqs(&mut out, id));
        }
        for _ in 0..20 {
            now += 50 * MS;
            timers(&mut s, &mut out, now);
            got.extend(seqs(&mut out, id));
        }
        assert_eq!(got.len(), seen.len(), "{lanes} lanes: delivered vs arrived");
        assert_eq!(got.iter().copied().collect::<BTreeSet<_>>(), seen, "{lanes} lanes: each arrival once");
        let (mut top, mut late) = (0, 0);
        for &q in &got {
            late += usize::from(q < top);
            top = top.max(q);
        }
        assert!(late <= got.len() / 500, "{lanes} lanes: {late} of {} delivered out of order", got.len());
        assert_eq!(s.pool.held(), 0, "{lanes} lanes: buffers still held");
    }
}

/// §13 and R2-12: past MAX_FLOWS live flows, a new one evicts the flow idle longest (counted in
/// `flows_dropped`), and a flow without a packet for FLOW_IDLE ends on the next tick. An ended
/// slot is reset whole, so the flows after it find slots that inherit nothing.
#[test]
fn flows_evicted_and_ended_after_flow_idle() {
    let mut s = session();
    let id = |k: usize| crate::FlowId(k as u16 + 1);
    for k in 0..MAX_FLOWS {
        let f = s.flow(id(k), true).unwrap_or_else(|| panic!("flow {k} gets a slot"));
        s.flows[f].last = k as Mono; // flow 0 is idle longest, then 1, ...
    }
    let first = s.flow(id(0), false).unwrap();
    let f = s.flow(id(MAX_FLOWS), true).expect("a new flow beyond MAX_FLOWS evicts one");
    assert_eq!((f, s.flows_dropped), (first, 1), "the flow idle longest made room");
    assert_eq!(s.flow(id(0), false), None, "the evicted flow is forgotten");
    assert_eq!((s.flows[f].id, s.flows[f].next, s.flows[f].small), (id(MAX_FLOWS), 1, true));
    s.flows[f].last = MAX_FLOWS as Mono;
    // State a new flow in a slot must not inherit.
    let f = s.flow(id(3), false).unwrap();
    let fl = &mut s.flows[f];
    (fl.next, fl.fill, fl.gap_seq, fl.gap_n, fl.tx_seq, fl.small) = (77, 5 * MS, 70, 3, 9, false);
    fl.held = vec![Pkt::default(); REORDER_SLOTS];
    fl.carried(LaneIx::at(0), 1, 76);
    // FLOW_IDLE after its last packet, a flow ends on the next tick; not a nanosecond before. Flows
    // opened straight through `flow` asked for no tick: one is due now.
    s.wake_at(0);
    handle(&mut s, FLOW_IDLE + 1, Event::Tick);
    assert_eq!(s.flow(id(1), false), None, "idle FLOW_IDLE: ended");
    assert!(s.flow(id(3), false).is_some(), "idle FLOW_IDLE - 1 ns: still live");
    handle(&mut s, FLOW_IDLE + MAX_FLOWS as Mono, Event::Tick);
    for f in &s.flows {
        assert_eq!(format!("{f:?}"), format!("{:?}", Flow::default()), "an ended slot is reset whole");
        assert_eq!(f.held.capacity(), 0, "an ended slot drops its reorder buffer");
    }
    let now = 2 * FLOW_IDLE;
    for k in 0..4 * MAX_FLOWS {
        let f = s.flow(id(k), true).expect("flows ended one after another always find a slot");
        assert_eq!((s.flows[f].next, s.flows[f].small), (1, true), "a new flow starts at seq 1, small");
        s.flows[f].last = now;
        s.wake_at(0);
        handle(&mut s, now + FLOW_IDLE, Event::Tick);
    }
    assert_eq!(s.flows_dropped, 1);
    assert!(s.flows.iter().all(|f| !f.used));
    assert!(s.rx_flows.is_empty() && s.tx_flows.is_empty());
}

/// §13 and R2-12 with datagrams held: a flow that ends after FLOW_IDLE returns every buffer its
/// reorder buffer held to the pool; the next flows find free slots and empty buffers.
#[test]
fn flow_idle_returns_held_buffers() {
    let (mut s, mut out) = (session(), Acts::default());
    let (fast, slow) = (lane(&mut s, 10 * MS, 0), lane(&mut s, 30 * MS, 0));
    let id = |k: usize| crate::FlowId(k as u16 + 1);
    let n = 64;
    for k in 0..n {
        // 1 and 2 on the slow lane; 4 and 5 on the fast one wait for 3, still on its way there.
        for (l, seq) in [(slow, 1), (slow, 2), (fast, 4), (fast, 5)] {
            arrive(&mut s, &mut out, 0, l, id(k), seq);
        }
        assert_eq!(seqs(&mut out, id(k)), [1, 2]);
    }
    assert_eq!(s.pool.held(), 2 * n * MAX_PACKET);
    // Held seqs are released by their timers first; a flow then ends once idle FLOW_IDLE.
    handle(&mut s, FLOW_IDLE, Event::Tick);
    assert_eq!(s.pool.held(), 0, "an ended flow returns its held buffers to the pool");
    assert!(s.flows.iter().all(|f| !f.used && f.n == 0 && f.held.capacity() == 0));
    rx(&mut s, &mut out, FLOW_IDLE + MS, Some(fast), TYPE_DATA, id(n), 1);
    assert_eq!(seqs(&mut out, id(n)), [1], "a new flow is accepted");
    assert_eq!(s.flows_dropped, 0);
}

/// R2-13: a late datagram that no gap given up skipped teaches the reorder timer nothing. Here
/// the flow turned small and its small packets passed the last bulk ones an hour into the
/// session (Go set fill to the session clock: gapAt was 0).
#[test]
fn late_after_cheap_teaches_nothing() {
    let (mut s, mut out) = (session(), Acts::default());
    let id = crate::FlowId(1);
    let t = 3600 * SEC;
    rx(&mut s, &mut out, t, None, TYPE_DATA, id, 1);
    rx(&mut s, &mut out, t, None, TYPE_DATA | SMALL_BIT, id, 3);
    rx(&mut s, &mut out, t + MS, None, TYPE_DATA, id, 2); // its last bulk datagram, late
    assert_eq!(seqs(&mut out, id), [1, 3, 2]);
    let f = s.flow(id, false).unwrap();
    assert_eq!((s.flows[f].fill, s.flows[f].next, s.unrecovered), (0, 4, 0));
}

/// §17.2's order invariant, as netsim checks it after every Handle call: `calls` holds the seqs
/// each call delivered. Within a call the reorder buffer releases in increasing order, past the
/// newest seq delivered before the call; a late datagram after a skipped gap comes alone.
fn in_order(calls: &[Vec<u32>]) -> Result<(), String> {
    let mut top = 0;
    for (i, got) in calls.iter().enumerate() {
        let (base, mut prev, mut late) = (top, top, 0);
        for &q in got {
            if q <= base {
                late += 1;
                if late > 1 {
                    return Err(format!("call {i} delivered {got:?}: two behind {base}"));
                }
            } else if q <= prev {
                return Err(format!("call {i} delivered {got:?}: {q} after {prev}"));
            } else {
                prev = q;
            }
            top = top.max(q);
        }
    }
    Ok(())
}

/// §10.3 and §17.2 (notes/rust.md, harden: integration, group C): a small packet that passes held
/// bulk of its own flow, as when the flow's rate crosses SMALL_RATE and the sender turns it small,
/// is still delivered at once; but the datagrams held before it go first, in order, giving up the
/// gaps behind them, and the flow resumes after it. Go delivered the small packet alone and left
/// the held ones to their gap's timer, which released two of them together behind it: the swarm's
/// seed 96 in bandwidth.
#[test]
fn small_passes_held_bulk_in_order() {
    for mode in [Mode::Performance, Mode::Bandwidth] {
        let (mut s, mut out) = (session(), Acts::default());
        s.mode = mode;
        let (fast, slow) = (lane(&mut s, 10 * MS, MS), lane(&mut s, 30 * MS, MS));
        let id = crate::FlowId(2);
        let f = s.flow(id, true).unwrap();
        let mut calls = Vec::new();
        // 2, 5, 7 and 10 are still on the slow lane; 3 and 4, 6, 9 and 11 wait behind them.
        arrive(&mut s, &mut out, SEC, slow, id, 1);
        calls.push(seqs(&mut out, id));
        for (k, seq) in [3, 4, 6, 9, 11].into_iter().enumerate() {
            arrive(&mut s, &mut out, SEC + (k as Mono + 1) * MS, fast, id, seq);
            calls.push(seqs(&mut out, id));
        }
        assert_eq!((s.flows[f].next, s.flows[f].n), (2, 5), "{mode:?}");
        // The sender turned the flow small for 8: it comes at once, with the small bit (§7.5
        // records no lane for a small packet: rule 2 counts bulk alone).
        let now = SEC + 6 * MS;
        rx(&mut s, &mut out, now, Some(fast), TYPE_DATA | SMALL_BIT, id, 8);
        calls.push(seqs(&mut out, id));
        assert_eq!(calls.last().unwrap(), &[3, 4, 6, 8, 9], "{mode:?}: what was held before 8, then 8 and 9");
        let fl = &s.flows[f];
        assert_eq!((fl.next, fl.n, s.unrecovered), (10, 1, 2), "{mode:?}: the gaps before 3 and 6 given up");
        // 10's gap waits for its timer, as any gap (7 was no gap: nothing was held behind it).
        timers(&mut s, &mut out, now);
        assert!(out.0.is_empty(), "{mode:?}");
        while s.flows[f].n > 0 {
            let now = s.next;
            timers(&mut s, &mut out, now);
            calls.push(seqs(&mut out, id));
        }
        assert_eq!((calls.last().unwrap(), s.unrecovered), (&vec![11], 3), "{mode:?}");
        // The stragglers come late, each alone; those the small packet passed teach nothing
        // (R2-13), as in late_after_cheap_teaches_nothing.
        for (k, seq) in [2, 5, 7, 10].into_iter().enumerate() {
            arrive(&mut s, &mut out, SEC + (60 + k as Mono) * MS, slow, id, seq);
            calls.push(seqs(&mut out, id));
            if seq != 10 {
                assert_eq!(s.flows[f].fill, 0, "{mode:?}: {seq} taught the timer");
            }
        }
        in_order(&calls).unwrap_or_else(|e| panic!("{mode:?}: {e}; calls {calls:?}"));
        let mut all = calls.concat();
        all.sort_unstable();
        assert_eq!(all, (1..=11).collect::<Vec<_>>(), "{mode:?}: each datagram once");
        assert_eq!((s.flows[f].n, s.pool.held()), (0, 0), "{mode:?}");
    }
}

/// A small packet under the seq of a held datagram, which only a broken or hostile peer sends,
/// leaves nothing held behind `next`: both are delivered and `n` stays exact, so the loops that
/// step until it is 0 end.
#[test]
fn small_with_a_held_seq_keeps_the_buffer_exact() {
    let (mut s, mut out) = (session(), Acts::default());
    let (fast, slow) = (lane(&mut s, 10 * MS, MS), lane(&mut s, 30 * MS, MS));
    let id = crate::FlowId(2);
    let f = s.flow(id, true).unwrap();
    arrive(&mut s, &mut out, SEC, slow, id, 1);
    arrive(&mut s, &mut out, SEC, fast, id, 3);
    rx(&mut s, &mut out, SEC, Some(fast), TYPE_DATA | SMALL_BIT, id, 3);
    assert_eq!(seqs(&mut out, id), [1, 3, 3]);
    assert_eq!((s.flows[f].next, s.flows[f].n, s.pool.held()), (4, 0, 0));
    let far = 4 + REORDER_SLOTS as u32;
    arrive(&mut s, &mut out, SEC, fast, id, far);
    assert_eq!(seqs(&mut out, id), [far]);
}

/// §10.3 and §17.2 against a model, as `reorder` with small packets: the flow turns small and
/// bulk by turns; each bulk seq rides one of 2–4 FIFO lanes with different delays, each small seq
/// is copied on every lane, and 1% of the packets are lost. Every arrival is delivered once and
/// no call breaks the order invariant, in both modes.
#[test]
fn reorder_with_cheap_turns() {
    for (lanes, mode) in (2..=4usize).flat_map(|n| [(n, Mode::Performance), (n, Mode::Bandwidth)]) {
        let mut s = session();
        s.mode = mode;
        let mut r = DetRand(lanes as u64 + 100);
        let ls: Vec<LaneIx> = (0..lanes).map(|i| lane(&mut s, (10 + 15 * i as Mono) * MS, 2 * MS)).collect();
        let (mut arrivals, mut last, mut small) = (Vec::new(), vec![0; lanes], false); // (at, lane, seq, small)
        for seq in 1..=3000u32 {
            small ^= r.next().is_multiple_of(40);
            let (sent, pick) = (Mono::from(seq) * (MS / 2), (r.next() % lanes as u64) as usize);
            for (i, &l) in ls.iter().enumerate() {
                if !small && i != pick || r.float() < 0.01 {
                    continue;
                }
                let at = Mono::max(last[i], sent + s.lanes[l].in_owd + (r.next() % 3) as Mono * MS);
                last[i] = at;
                arrivals.push((at, i, seq, small));
            }
        }
        arrivals.sort_by_key(|a| a.0);
        let id = crate::FlowId(2);
        let (mut calls, mut seen, mut now, mut out) = (Vec::new(), BTreeSet::new(), 0, Acts::default());
        for &(at, i, seq, small) in &arrivals {
            while now < at {
                now = at.min((now + 1).max(s.next));
                if now >= s.next {
                    timers(&mut s, &mut out, now);
                    calls.push(seqs(&mut out, id));
                }
            }
            if let Some(f) = s.flow(id, false).filter(|_| !small) {
                s.flows[f].carried(ls[i], now, seq); // §7.5 records every bulk copy on its lane
            }
            if seen.insert(seq) {
                let typ = if small { TYPE_DATA | SMALL_BIT } else { TYPE_DATA };
                rx(&mut s, &mut out, now, Some(ls[i]), typ, id, seq);
                calls.push(seqs(&mut out, id));
            }
        }
        for _ in 0..20 {
            now += 50 * MS;
            timers(&mut s, &mut out, now);
            calls.push(seqs(&mut out, id));
        }
        in_order(&calls).unwrap_or_else(|e| panic!("{lanes} lanes, {mode:?}: {e}"));
        let mut got = calls.concat();
        got.sort_unstable();
        assert_eq!(got, seen.into_iter().collect::<Vec<_>>(), "{lanes} lanes, {mode:?}: each arrival once");
        assert_eq!(s.pool.held(), 0, "{lanes} lanes, {mode:?}: buffers still held");
    }
}

/// R2-13 and notes/core.md 27: a datagram of the last gap given up raises `fill` to what that
/// gap needed, never past FILL_MAX (whose timer is REORDER_MAX); one outside that gap, one later
/// than REORDER_RESCUE (a stall's leftover) or one the far-ahead jump passed changes nothing.
#[test]
fn fill_learns_from_the_gap_given_up() {
    let (mut s, mut out) = (session(), Acts::default());
    let id = crate::FlowId(1);
    let f = s.flow(id, true).unwrap();
    let fill = |s: &Session| s.flows[f].fill;
    // No lane carried the flow lately, so rule 2 gives a gap up at once.
    rx(&mut s, &mut out, SEC, None, TYPE_DATA, id, 1);
    rx(&mut s, &mut out, SEC, None, TYPE_DATA, id, 3);
    assert_eq!((s.unrecovered, s.flows[f].gap_seq, s.flows[f].gap_n), (1, 2, 1));
    rx(&mut s, &mut out, SEC + 7 * MS, None, TYPE_DATA, id, 2);
    assert_eq!(fill(&s), 7 * MS, "the gap needed 7 ms");
    rx(&mut s, &mut out, SEC + 30 * MS, None, TYPE_DATA, id, 1);
    assert_eq!(fill(&s), 7 * MS, "seq 1 was no part of that gap");
    rx(&mut s, &mut out, 2 * SEC, None, TYPE_DATA, id, 5); // 4 is given up
    rx(&mut s, &mut out, 2 * SEC + 120 * MS, None, TYPE_DATA, id, 4);
    assert_eq!(fill(&s), FILL_MAX, "120 ms, capped");
    assert_eq!(FILL_MAX as f64 * crate::SKEW_MARGIN, REORDER_MAX as f64);
    s.flows[f].fill = 0;
    rx(&mut s, &mut out, 3 * SEC, None, TYPE_DATA, id, 7); // 6 is given up
    rx(&mut s, &mut out, 3 * SEC + REORDER_RESCUE + 1, None, TYPE_DATA, id, 6);
    assert_eq!(fill(&s), 0, "later than any gap waits: a stall's leftover");
    // The jump: nothing before it teaches anything, not even the gap given up before it.
    rx(&mut s, &mut out, 4 * SEC, None, TYPE_DATA, id, 10); // 8 and 9 are given up
    let far = 11 + REORDER_SLOTS as u32;
    rx(&mut s, &mut out, 4 * SEC, None, TYPE_DATA, id, far);
    assert_eq!((s.unrecovered, s.flows[f].next, s.flows[f].gap_n), (5, far + 1, 0));
    rx(&mut s, &mut out, 4 * SEC + MS, None, TYPE_DATA, id, 9);
    rx(&mut s, &mut out, 4 * SEC + MS, None, TYPE_DATA, id, far - 1);
    assert_eq!(fill(&s), 0);
    assert_eq!(seqs(&mut out, id), [1, 3, 2, 1, 5, 4, 7, 6, 10, far, 9, far - 1]);
}

/// notes/core.md 27, R2-13: a gap that fills while a carrying lane lags (REORDER_RESCUE) raises
/// `fill` to at most FILL_MAX, and each later gap that fills decays it by 1/SKEW_DECAY.
#[test]
fn fill_from_gaps_that_filled() {
    let (mut s, mut out) = (session(), Acts::default());
    let (slow, fast) = (lane(&mut s, 30 * MS, MS), lane(&mut s, 10 * MS, MS));
    let id = crate::FlowId(1);
    for seq in [1, 2] {
        arrive(&mut s, &mut out, 0, slow, id, seq);
    }
    s.lanes[slow].lagging = true;
    arrive(&mut s, &mut out, 0, fast, id, 4);
    timers(&mut s, &mut out, 0);
    assert_eq!(s.next, REORDER_RESCUE, "a gap waits REORDER_RESCUE while a carrying lane lags");
    arrive(&mut s, &mut out, 100 * MS, slow, id, 3);
    let f = s.flow(id, false).unwrap();
    assert_eq!(s.flows[f].fill, FILL_MAX, "100 ms, capped");
    s.lanes[slow].lagging = false;
    arrive(&mut s, &mut out, 101 * MS, fast, id, 6);
    arrive(&mut s, &mut out, 102 * MS, slow, id, 5);
    assert_eq!(s.flows[f].fill, FILL_MAX - FILL_MAX / Mono::from(SKEW_DECAY), "1 ms decays it");
    assert_eq!(seqs(&mut out, id), [1, 2, 3, 4, 5, 6]);
    assert_eq!(s.unrecovered, 0);
}

/// R2-24: a flow keeps its order across the wrap of its u32 seq; a gap waits for the lane that
/// still carries the seqs before it, as anywhere else (Go delivered seq 1 at once, as late).
#[test]
fn order_across_the_wrap() {
    let (mut s, mut out) = (session(), Acts::default());
    let (fast, slow) = (lane(&mut s, 5 * MS, 0), lane(&mut s, 20 * MS, 0));
    for (l, base) in [(fast, 5 * MS), (slow, 20 * MS)] {
        s.lanes[l].in_min = Winmin { at: 0, cur: base, old: NO_MIN };
    }
    let id = crate::FlowId(1);
    let f = s.flow(id, true).unwrap();
    s.flows[f].next = u32::MAX - 1;
    arrive(&mut s, &mut out, SEC, slow, id, u32::MAX - 1);
    arrive(&mut s, &mut out, SEC + MS, fast, id, 1);
    // §14.1: the slower lane's base OWD − the passing lane's, + DUP_GRACE.
    assert_eq!((s.flows[f].n, s.next), (1, SEC + 18 * MS), "1 waits the 15 ms skew + DUP_GRACE for MAX and 0");
    arrive(&mut s, &mut out, SEC + 2 * MS, slow, id, u32::MAX);
    arrive(&mut s, &mut out, SEC + 3 * MS, slow, id, 0);
    assert_eq!(seqs(&mut out, id), [u32::MAX - 1, u32::MAX, 0, 1]);
    assert_eq!((s.unrecovered, s.flows[f].next, s.flows[f].n), (0, 2, 0));
}

/// Review fix (notes/rust.md, review fixes: flow-half-range): a bulk seq exactly 2^31 past `next`,
/// neither after nor before it in serial order, is the far-ahead jump, delivered at once. It was
/// held, in `next`'s own slot outside the window, and then a jump or a small packet up to it
/// stepped about 2^31 seqs (6 s of node's one loop per packet from an admitted peer). Now each
/// returns at once and nothing is held.
#[test]
fn half_range_ahead_jumps() {
    let half = 1 << 31;
    for (typ, seq) in [(TYPE_DATA, 5_000), (TYPE_DATA | SMALL_BIT, half)] {
        let (mut s, mut out) = (session(), Acts::default());
        let id = crate::FlowId(1);
        let f = s.flow(id, true).unwrap();
        rx(&mut s, &mut out, SEC, None, TYPE_DATA, id, 1 + half);
        assert_eq!((s.flows[f].n, s.flows[f].next, s.unrecovered), (0, 2 + half, 1), "a jump");
        rx(&mut s, &mut out, SEC, None, typ, id, seq);
        assert_eq!(seqs(&mut out, id), [1 + half, seq]);
        assert_eq!((s.flows[f].n, s.pool.held()), (0, 0));
    }
}

/// The bound behind that fix, as in `peer_new`: should a broken count ever leave a seq held
/// outside the window (here `hold` is handed one 2^31 ahead), a far-ahead jump and a small packet
/// up to it each step at most REORDER_SLOTS seqs and leave it held, rather than walk 2^31 seqs.
#[test]
fn a_broken_count_never_spins() {
    let far: u32 = 1 + (1 << 31);
    for (typ, seq) in [(TYPE_DATA, 5_000), (TYPE_DATA | SMALL_BIT, far)] {
        let (mut s, mut out) = (session(), Acts::default());
        let id = crate::FlowId(1);
        let f = s.flow(id, true).unwrap();
        s.rxbuf[DATA_LEN..DATA_LEN + 4].copy_from_slice(&far.to_be_bytes());
        assert!(s.hold(f, SEC, far, DATA_LEN + 4, None));
        rx(&mut s, &mut out, SEC, None, typ, id, seq);
        assert_eq!((seqs(&mut out, id), s.flows[f].n), (vec![seq], 1), "type {typ:#x}");
    }
}

/// Rule 2's lane list (notes/core.md 19): each lane's highest seq in serial order (R2-24), the
/// oldest entry giving way to a new lane, which starts at the seq it brought.
#[test]
fn carried_lanes() {
    let l = LaneIx::at;
    let mut f = Flow::default();
    for i in 0..FLOW_LANES {
        f.carried(l(i), 10 + i as Mono, 100 + i as u32);
    }
    f.carried(l(1), 20, 50);
    assert_eq!(f.lanes[1], FlowLane { l: Some(l(1)), at: 20, seq: 101 }, "a lower seq keeps the highest");
    f.carried(l(9), 30, 7);
    assert_eq!(f.lanes[0], FlowLane { l: Some(l(9)), at: 30, seq: 7 }, "a new lane replaces the oldest");
    f.carried(l(5), 40, u32::MAX - 1);
    assert_eq!(f.lanes[2], FlowLane { l: Some(l(5)), at: 40, seq: u32::MAX - 1 });
    f.carried(l(5), 41, 3);
    assert_eq!(f.lanes[2].seq, 3, "3 follows MAX − 1 across the wrap");
    f.carried(l(5), 42, u32::MAX);
    assert_eq!(f.lanes[2], FlowLane { l: Some(l(5)), at: 42, seq: 3 });
    f.carried(l(7), 50, 3_000_000_000);
    assert_eq!(f.lanes[3], FlowLane { l: Some(l(7)), at: 50, seq: 3_000_000_000 }, "past 2^31 from 0");
    // Mono's origin is arbitrary: at or before 0, a recorded lane still keeps its entry while
    // another is empty (Go compared `at` alone).
    let mut f = Flow::default();
    f.carried(l(0), -10, 1);
    f.carried(l(1), 0, 2);
    let want = [FlowLane { l: Some(l(0)), at: -10, seq: 1 }, FlowLane { l: Some(l(1)), at: 0, seq: 2 }];
    assert_eq!(f.lanes[..2], want);
}

/// §10.1: cheapest first, each under SMALL_RATE, until the small flows total SMALL_COPY_BUDGET; equal
/// rates in slot order.
#[test]
fn classify_cheapest_first() {
    let mut s = session();
    // Bytes over the last SMALL_WINDOW (200 ms): 0.9, 0.4, 3, 0.6 and 0.9 Mbps.
    for (k, b) in [22_500, 10_000, 75_000, 15_000, 22_500].into_iter().enumerate() {
        let f = txf(&mut s, crate::FlowId(k as u16 + 1), true).unwrap();
        s.flows[f].tx.add(SEC, SMALL_WINDOW, b);
    }
    s.classify(SEC);
    let small: Vec<bool> = s.flows[..5].iter().map(|f| f.small).collect();
    assert_eq!(small, [true, true, false, true, false], "0.4 + 0.6 + 0.9 (the first) fit 2 Mbps");
    assert!((s.small_rate - 1.9e6).abs() < 1.0, "small rate {}", s.small_rate);
}

/// §7.6 rule 5: past REORDER_CAP of the session's buffers, a datagram behind a gap is delivered
/// out of order rather than held; and one seq sent twice under two counters (a broken or hostile
/// peer) holds one buffer, so the flow still drains.
#[test]
fn reorder_buffer_bounds() {
    let (mut s, mut out) = (session(), Acts::default());
    let (fast, slow) = (lane(&mut s, 5 * MS, 0), lane(&mut s, 20 * MS, 0));
    let id = crate::FlowId(1);
    for seq in [1, 2] {
        arrive(&mut s, &mut out, 0, slow, id, seq);
    }
    arrive(&mut s, &mut out, 0, fast, id, 4);
    arrive(&mut s, &mut out, 0, fast, id, 4);
    let f = s.flow(id, false).unwrap();
    assert_eq!((s.flows[f].n, s.pool.held()), (1, MAX_PACKET));
    let busy: Vec<_> = std::iter::from_fn(|| s.pool.get()).take(REORDER_CAP / MAX_PACKET - 1).collect();
    arrive(&mut s, &mut out, 0, fast, id, 5);
    assert_eq!(s.flows[f].n, 1, "past the cap, 5 is not held");
    arrive(&mut s, &mut out, MS, slow, id, 3);
    assert_eq!(seqs(&mut out, id), [1, 2, 5, 3, 4]);
    assert_eq!((s.flows[f].n, s.unrecovered), (0, 0));
    for b in busy {
        s.pool.put(b);
    }
    assert_eq!(s.pool.held(), 0);
}

/// Fills lane `l`'s rewind ring with (counter, age, bulk) entries and its pacer queue with bulk
/// `queued`, each bulk packet 1,000 bytes in its own pool buffer.
fn sent(s: &mut Session, now: Mono, l: LaneIx, ring: &[(u64, Mono, bool)], queued: &[u64]) {
    for &(c, age, bulk) in ring {
        let buf = if bulk { s.pool.get() } else { None };
        s.lanes[l].rw.push_back(Pkt { c, at: now - age, buf, n: 1_000 });
    }
    for &c in queued {
        let buf = s.pool.get();
        s.lanes[l].q.push_back(Pkt { c, at: now, buf, n: 1_000 });
        s.lanes[l].q_bytes += 1_000;
    }
}

/// The counters in lane `l`'s pacer queue.
fn queue(s: &Session, l: LaneIx) -> Vec<u64> {
    s.lanes[l].q.iter().map(|p| p.c).collect()
}

/// §14.2: the ring's bulk after the peer's echoed LastCounter, sent within the rewind window
/// (REWIND_WINDOW, or the suspect timeout + RTT), goes once each and in send order onto the lanes that take over, then the unsent
/// queue behind it; small counters, older bulk and what was echoed are dropped. The takeover lane
/// is not validated, so its pacer holds what it is given (§7.6 rule 2).
#[test]
fn rewind_replays_unechoed_bulk() {
    let (mut s, mut out) = (session(), Acts::default());
    let (l, t) = (lane(&mut s, 10 * MS, 0), lane(&mut s, 20 * MS, 0));
    let now = SEC;
    let ring = [(1, 150 * MS, true), (2, 120 * MS, true), (3, 110 * MS, true), (4, QUEUE_AGE, true)];
    let ring = [&ring[..], &[(5, 90 * MS, false), (6, 80 * MS, true), (7, 50 * MS, false), (8, MS, true)]].concat();
    sent(&mut s, now, l, &ring, &[9, 10]);
    s.lanes[l].acked = 2; // 3 was sent after it, within the rewind window
    s.rewind(&mut out, now, l, &[t]);
    assert_eq!(queue(&s, t), [3, 4, 6, 8, 9, 10]);
    assert!(s.lanes[t].q.iter().all(|p| p.at == now && p.n == 1_000));
    assert_eq!((s.lanes[l].rw.len(), s.lanes[l].q.len(), s.lanes[l].q_bytes), (0, 0, 0));
    assert_eq!((s.lanes[t].q_bytes, s.pool.held()), (6_000, 6 * MAX_PACKET), "1 and 2 freed");
    // Counters are matched, not compared: small 14, sealed after 12 and 13, left first, and
    // the peer echoed it. A LastCounter that left the ring replays all of it.
    s.shift(&mut out, now, t, &[]);
    sent(&mut s, now, l, &[(11, 4 * MS, true), (14, 3 * MS, false), (12, 2 * MS, true), (13, MS, true)], &[]);
    s.lanes[l].acked = 14;
    s.rewind(&mut out, now, l, &[t]);
    assert_eq!(queue(&s, t), [12, 13]);
    sent(&mut s, now, l, &[(15, 2 * MS, true), (16, MS, true)], &[]);
    s.lanes[l].acked = 10;
    s.rewind(&mut out, now, l, &[t]);
    assert_eq!(queue(&s, t), [12, 13, 15, 16]);
    assert!(out.0.is_empty(), "an unvalidated lane sends nothing");
}

/// §10.2: with no lane to take over, a rewind drops the ring and the queue and frees every
/// buffer; `shift` alone moves the queue in order and leaves the ring.
#[test]
fn rewind_without_a_lane_frees_everything() {
    let (mut s, mut out) = (session(), Acts::default());
    let (l, t) = (lane(&mut s, 10 * MS, 0), lane(&mut s, 20 * MS, 0));
    sent(&mut s, SEC, l, &[(1, 3 * MS, true), (2, 2 * MS, false)], &[3, 4]);
    s.shift(&mut out, SEC, l, &[t]);
    assert_eq!((queue(&s, t), s.lanes[l].rw.len(), s.lanes[l].q_bytes), (vec![3, 4], 2, 0));
    sent(&mut s, SEC, l, &[(5, MS, true)], &[6]);
    s.rewind(&mut out, SEC, l, &[]);
    s.rewind(&mut out, SEC, t, &[]);
    assert_eq!((s.lanes[l].rw.len(), s.lanes[l].q.len(), s.lanes[t].q.len()), (0, 0, 0));
    assert_eq!((s.lanes[l].q_bytes, s.lanes[t].q_bytes, s.pool.held()), (0, 0, 0));
}

/// The packets `out` sent: socket, address, bytes.
fn sends(out: &Acts) -> Vec<(SockId, SocketAddr, Vec<u8>)> {
    let sent = out.0.iter().filter_map(|a| match a {
        Act::Send { sock, to, data } => Some((*sock, *to, data.clone())),
        _ => None,
    });
    sent.collect()
}

/// Makes `l` a validated lane to `to` in state `st`.
fn answering(s: &mut Session, l: LaneIx, to: SocketAddr, st: State) {
    let x = &mut s.lanes[l];
    (x.peer, x.validated, x.state) = (Some(to), true, st);
}

/// §7.3, §8.1 and notes/core.md 37: before the peer's key nothing is sent; a datagram over
/// MAX_INNER never is. A small datagram, sealed once, goes on every validated lane while no
/// lane is confirmed, and its buffer is free again at once.
#[test]
fn small_goes_everywhere_until_confirmed() {
    let (mut s, mut out) = (session(), Acts::default());
    let ls = [lane(&mut s, 10 * MS, 0), lane(&mut s, 30 * MS, 0), lane(&mut s, 5 * MS, 0)];
    answering(&mut s, ls[0], S_ETH, State::Punching);
    answering(&mut s, ls[1], C_PHONE, State::Punching);
    s.lanes[ls[2]].peer = Some(C_WIFI); // not validated: no data (§7.6 rule 2)
    let id = crate::FlowId(1);
    s.app(&mut out, SEC, id, b"audio");
    assert!(out.0.is_empty() && txf(&mut s, id, false).is_none(), "before the peer's key");
    s.tx.rid = 9;
    s.app(&mut out, SEC, id, &[0; MAX_INNER - INNER_HDR + 1]);
    assert!(out.0.is_empty() && txf(&mut s, id, false).is_none(), "too large");
    s.app(&mut out, SEC, id, b"audio");
    let sent = sends(&out);
    let to: Vec<_> = sent.iter().map(|p| (p.0, p.1)).collect();
    assert_eq!(to, [(s.lanes[ls[0]].sock, S_ETH), (s.lanes[ls[1]].sock, C_PHONE)]);
    assert_eq!(sent[0].2, sent[1].2, "sealed once");
    assert_eq!((sent[0].2.len(), sent[0].2[0]), (5 + INNER_HDR + OVERHEAD, TYPE_DATA | SMALL_BIT));
    let f = txf(&mut s, id, false).unwrap();
    assert_eq!((s.flows[f].tx_seq, s.flows[f].small, s.pool.held()), (1, true, 0));
}

/// §10.1: once a lane is confirmed, a small packet goes only on the validated lanes of the copy
/// set, each counting its bytes and keeping only its counter in its rewind ring (§10.2).
#[test]
fn copies_go_on_the_copy_set() {
    let (mut s, mut out) = (session(), Acts::default());
    (s.tx.rid, s.confirmed) = (9, true);
    let ls = [lane(&mut s, 10 * MS, 0), lane(&mut s, 30 * MS, 0), lane(&mut s, 5 * MS, 0)];
    answering(&mut s, ls[0], S_ETH, State::Up);
    answering(&mut s, ls[1], C_PHONE, State::Up);
    s.lanes[ls[2]].peer = Some(C_WIFI);
    (s.lanes[ls[0]].in_copy, s.lanes[ls[2]].in_copy) = (true, true);
    s.app(&mut out, SEC, crate::FlowId(1), b"input");
    let sent = sends(&out);
    assert_eq!(sent.iter().map(|p| (p.0, p.1)).collect::<Vec<_>>(), [(s.lanes[ls[0]].sock, S_ETH)]);
    let (pk, x) = (&sent[0].2, &s.lanes[ls[0]]);
    let c = u64::from_be_bytes(pk[3..11].try_into().unwrap());
    assert_eq!(x.copy_bytes, pk.len() as u64);
    assert_eq!(x.rw.iter().map(|p| (p.c, p.buf)).collect::<Vec<_>>(), [(c, None)]);
    assert!(s.lanes[ls[1]].rw.is_empty() && s.lanes[ls[2]].rw.is_empty());
    assert_eq!(s.pool.held(), 0);
}

/// §11.1 and §7.3: a bulk packet goes on the lanes the scheduler chose, the first taking the
/// packet itself and each other lane its own copy of the same sealed bytes; with no lane that
/// answers, on every validated lane at once (notes/core.md 24). No buffer leaks either way.
#[test]
fn bulk_goes_on_the_schedulers_lanes() {
    let (mut s, mut out) = (session(), Acts::default());
    (s.tx.rid, s.confirmed) = (9, true);
    let (a, b) = (lane(&mut s, 10 * MS, 0), lane(&mut s, 30 * MS, 0));
    answering(&mut s, a, S_ETH, State::Up);
    answering(&mut s, b, C_PHONE, State::Up);
    (s.spread[0], s.spread[1], s.nspread) = (a, b, 2);
    let id = crate::FlowId(1);
    let f = txf(&mut s, id, true).unwrap();
    s.flows[f].small = false; // classify found it bulk
    s.app(&mut out, SEC, id, &[7; 1_000]);
    // What each lane sent or still queues, however its pacer runs.
    let on = |s: &Session, l: LaneIx| {
        let x = &s.lanes[l];
        let mut v: Vec<_> = sends(&out).into_iter().filter(|p| p.0 == x.sock).map(|p| p.2).collect();
        v.extend(x.q.iter().map(|p| s.pool.buf(p.buf.unwrap())[..p.n as usize].to_vec()));
        v
    };
    let (pa, pb) = (on(&s, a), on(&s, b));
    assert_eq!((pa.len(), pb.len()), (1, 1));
    assert_eq!(pa[0], pb[0], "the same sealed bytes");
    assert_eq!((pa[0].len(), pa[0][0]), (1_000 + INNER_HDR + OVERHEAD, TYPE_DATA));
    let held = s.pool.held();
    assert_eq!(held, 2 * MAX_PACKET, "one buffer per lane, queued or in its rewind ring");
    out.0.clear();
    s.nspread = 0;
    s.app(&mut out, SEC, id, &[8; 1_000]);
    assert_eq!(sends(&out).iter().map(|p| p.0).collect::<Vec<_>>(), [s.lanes[a].sock, s.lanes[b].sock]);
    assert_eq!(s.pool.held(), held, "no lane answers: sent at once and freed");
}

/// §6.1 and notes/rust.md (accept: integration): bulk that no lane carries goes out at once on
/// every validated lane but those sharing a bottleneck group with a validated lane that answers
/// (echoed within its suspect timeout) and comes first: one that answers, a direct lane before the
/// Tailscale lane, then the lower OWD. A group none of whose lanes answers still gets it on every
/// validated lane. Go sent it on every validated lane, twice through a shared shaper.
#[test]
fn bulk_without_a_lane_goes_once_per_group() {
    let (mut s, mut out) = (session(), Acts::default());
    (s.tx.rid, s.confirmed) = (9, true);
    let ls = [0, 1, 2, 3, 4].map(|_| lane(&mut s, 0, 0));
    let [a, b, m, c, u] = ls;
    for (l, group, owd, to) in
        [(a, 0, 10 * MS, S_ETH), (b, 0, 5 * MS, C_WIFI), (m, 0, MS, S_MESH), (c, 1, 30 * MS, C_PHONE)]
    {
        answering(&mut s, l, to, State::Down);
        (s.lanes[l].group, s.lanes[l].owd, s.lanes[l].rtt) = (group, owd, 40 * MS);
    }
    s.lanes[m].mesh = true;
    (s.lanes[u].peer, s.lanes[u].group) = (Some(C_WIFI), 0); // not validated: nothing (§7.6 rule 2)
    let id = crate::FlowId(1);
    let f = txf(&mut s, id, true).unwrap();
    s.flows[f].small = false; // classify found it bulk
    let now = 10 * SEC;
    s.now = now;
    // Which lanes a bulk datagram went on, after `echoed` were echoed 10 ms ago and the others
    // 500 ms ago (their suspect timeout is 80 ms).
    let mut on = |s: &mut Session, echoed: &[LaneIx]| {
        for l in [a, b, m, c] {
            let x = &mut s.lanes[l];
            x.last_echo = now - if echoed.contains(&l) { 10 * MS } else { 500 * MS };
            x.owds.add(x.last_echo, x.owd);
        }
        out.0.clear();
        s.app(&mut out, now, id, &[7; 1_000]);
        let socks = sends(&out).into_iter().map(|p| p.0).collect::<Vec<_>>();
        ls.into_iter().filter(|&l| socks.contains(&s.lanes[l].sock)).collect::<Vec<_>>()
    };
    assert_eq!(on(&mut s, &[]), [a, b, m, c], "no lane answers: every validated lane");
    assert_eq!(on(&mut s, &[a, b, m]), [b, c], "group 0's lowest-OWD direct lane; group 1 as before");
    assert_eq!(on(&mut s, &[a, m]), [a, c], "a lane that answers before one that does not");
    assert_eq!(on(&mut s, &[m, c]), [m, c], "the Tailscale lane when no direct lane of its group answers");
    s.lanes[m].group = -1; // no link is default: the Tailscale lane shares every group (§6.1)
    assert_eq!(on(&mut s, &[a, m, c]), [a, c], "behind any direct lane that answers");
    assert_eq!(on(&mut s, &[m]), [m], "ahead of every lane that does not");
    assert_eq!(s.pool.held(), 0, "sent at once and freed");
}

/// §10.1, §8.4: copies go on the lowest-OWD validated lane of each group that answers, while its
/// sending leaves them room, on the Tailscale lane while fewer than two groups have such a lane,
/// and on every lane carrying bulk; a lane that joins or leaves gets a probe at once.
#[test]
fn copy_set() {
    let (mut s, mut out) = (session(), Acts::default());
    s.tx.rid = 9; // the peer's key: without it no probe is sent, as in Go's sendProbe
    let ls = [0, 1, 2, 3].map(|_| lane(&mut s, 0, 0));
    let [a, b, c, m] = ls;
    for (l, group, owd, st, to) in [
        (a, 0, 10 * MS, State::Up, S_ETH),
        (b, 0, 5 * MS, State::Up, C_PHONE),
        (c, 1, 20 * MS, State::Suspect, C_WIFI),
        (m, -1, 30 * MS, State::Up, S_MESH),
    ] {
        answering(&mut s, l, to, st);
        (s.lanes[l].group, s.lanes[l].owd) = (group, owd);
    }
    s.lanes[m].mesh = true;
    // The lanes in the copy set, and those probed since the last look.
    let look = |s: &mut Session, out: &mut Acts| {
        s.set_copies(out, SEC);
        let set = ls.iter().filter(|&&l| s.lanes[l].in_copy).copied().collect::<Vec<_>>();
        let probed = ls.iter().filter(|&&l| sends(out).iter().any(|p| p.0 == s.lanes[l].sock)).copied();
        let probed = probed.collect::<Vec<_>>();
        out.0.clear();
        (set, probed)
    };
    assert_eq!(look(&mut s, &mut out), (vec![b, c], vec![b, c]));
    assert_eq!(look(&mut s, &mut out), (vec![b, c], vec![]), "no change, no probe");
    (s.small_rate, s.lanes[c].rate) = (0.6e6, 1e6);
    assert_eq!(look(&mut s, &mut out), (vec![b], vec![c]), "c's rate leaves the copies no room");
    s.lanes[b].state = State::Down;
    assert_eq!(look(&mut s, &mut out), (vec![a], vec![a, b]), "a is group 0's best now");
    s.lanes[c].state = State::Down;
    assert_eq!(look(&mut s, &mut out), (vec![a, m], vec![m]), "one group answers: the Tailscale lane too");
    (s.spread[0], s.nspread) = (c, 1);
    assert_eq!(look(&mut s, &mut out), (vec![a, c, m], vec![c]), "the lane carrying bulk takes them");
}

/// §13: every fragment of a datagram, the first (with the UDP header) included, goes into one
/// flow with consecutive seqs, apart from the whole datagrams of the same ports, so the far kernel
/// gets a datagram's fragments in order (keyed apart, the first went with the bulk flow through
/// the reorder buffer and the last into a small flow copied at once: `Ip.ReasmFails`).
#[test]
fn a_datagram_s_fragments_share_one_flow() {
    let (mut s, mut out) = (session(), Acts::default());
    s.tx.rid = 9;
    let id = crate::FlowId(7);
    let whole = crate::pair::inner(id, &[1; 100]);
    // The kernel's split at offset 48: the IPv4 header with MF and the first 48 bytes (the UDP
    // header among them), then the header with offset 6 (× 8 bytes) and the rest.
    let frag = |at: usize, end: usize, fo: u16| {
        let mut f = whole[..20].to_vec();
        f[2..4].copy_from_slice(&((20 + end - at) as u16).to_be_bytes());
        f[6..8].copy_from_slice(&fo.to_be_bytes());
        f.extend_from_slice(&whole[20 + at..20 + end]);
        f
    };
    let (first, last) = (frag(0, 48, 0x2000), frag(48, 108, 6));
    for p in [&whole, &first, &last, &whole, &first, &last] {
        s.send_inner(&mut out, SEC, p);
    }
    let port_less = crate::flow::Key::parse(&last).unwrap();
    assert_eq!((port_less.sport, port_less.dport), (0, 0));
    let (fr, wh) = (s.tx_flows[&port_less], txf(&mut s, id, false).unwrap());
    assert_ne!(s.flows[fr].id, s.flows[wh].id, "fragments apart from whole datagrams");
    assert_eq!((s.flows[fr].tx_seq, s.flows[wh].tx_seq), (4, 2), "both fragments of each datagram, in order");
}

/// §13: TCP and UDP by ports, ICMP echo by its id, other protocols and every fragment of a
/// datagram, the first included, by protocol and addresses alone; IPv6 extension headers are
/// skipped. Truncations never panic.
#[test]
fn key_parse() {
    use crate::flow::Key;
    use std::net::IpAddr;
    let v4 = |proto: u8, frag: u16, l4: &[u8]| {
        let mut p = vec![0x45, 0, 0, 0, 0, 0, (frag >> 8) as u8, frag as u8, 64, proto, 0, 0, 10, 77, 0, 2, 1, 1, 1, 1];
        p.extend_from_slice(l4);
        p
    };
    let (a, b): (IpAddr, IpAddr) = ("10.77.0.2".parse().unwrap(), "1.1.1.1".parse().unwrap());
    let udp = v4(17, 0, &[0x1f, 0x90, 0, 53, 0, 8, 0, 0]);
    assert_eq!(Key::parse(&udp), Some(Key { proto: 17, src: a, dst: b, sport: 8080, dport: 53 }));
    let ping = v4(1, 0, &[8, 0, 0, 0, 0x12, 0x34, 0, 1]);
    assert_eq!(Key::parse(&ping).map(|k| (k.sport, k.dport)), Some((0x1234, 0)));
    let later = v4(6, 185, &[0x1f, 0x90, 0, 80]);
    assert_eq!(Key::parse(&later).map(|k| (k.proto, k.sport, k.dport)), Some((6, 0, 0)), "a later fragment");
    // Moonlight's 1,392-byte payload in a 1,400-byte TUN: a first fragment (MF, offset 0) with
    // the UDP header and a last one (offset 1,376 bytes, MF clear) take one key, the port-less one.
    let first = v4(17, 0x2000, &[0xbf, 0x70, 0xbf, 0x70, 0x05, 0x78, 0, 0]);
    let last = v4(17, 1376 / 8, &[0; 24]);
    let port_less = Some(Key { proto: 17, src: a, dst: b, sport: 0, dport: 0 });
    assert_eq!((Key::parse(&first), Key::parse(&last)), (port_less, port_less), "a datagram's fragments");
    let df = v4(17, 0x4000, &[0x1f, 0x90, 0, 53, 0, 8, 0, 0]);
    assert_eq!(Key::parse(&df).map(|k| (k.sport, k.dport)), Some((8080, 53)), "DF alone is no fragment");
    let mut v6 = vec![0x60, 0, 0, 0, 0, 16, 0, 64];
    v6.extend_from_slice(&[0xfd, 0x77, 0x0b, 0x0d, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    v6.extend_from_slice(&[0x26, 0x06, 0x47, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x11]);
    v6.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 0]); // hop-by-hop, then TCP
    v6.extend_from_slice(&[0xc3, 0x50, 0x01, 0xbb]);
    let k = Key::parse(&v6).unwrap();
    assert_eq!((k.proto, k.sport, k.dport, k.src), (6, 50000, 443, "fd77:b0d::2".parse::<IpAddr>().unwrap()));
    // IPv6 fragments, the first (offset 0, M set) included, by a Fragment header after hop-by-hop:
    // the fragment header's next header is the protocol, whatever follows it.
    let frag6 = |off_m: u16, rest: &[u8]| {
        let mut p = v6[..48].to_vec();
        p[40] = 44; // hop-by-hop, then a Fragment header
        p.extend_from_slice(&[17, 0, (off_m >> 8) as u8, off_m as u8, 0, 0, 0, 7]);
        p.extend_from_slice(rest);
        p
    };
    let (f6a, f6b) = (frag6(1, &[0xbf, 0x70, 0xbf, 0x70, 0x05, 0x78, 0, 0]), frag6(1376, &[60, 0, 0, 0]));
    let k6 = Key { proto: 17, src: k.src, dst: k.dst, sport: 0, dport: 0 };
    assert_eq!((Key::parse(&f6a), Key::parse(&f6b)), (Some(k6), Some(k6)), "IPv6 fragments");
    for p in [&udp[..], &ping[..], &v6[..], &first[..], &f6a[..]] {
        for i in 0..p.len() {
            let _ = Key::parse(&p[..i]);
        }
    }
    assert_eq!(Key::parse(&[0x45; 19]), None);
    assert_eq!(Key::parse(&[0x20; 40]), None, "not IP");
}
