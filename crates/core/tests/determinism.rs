//! Given the same events, times and random bytes, a session calls Out identically (§17.4; netsim
//! compares two runs of a seed), and reading its snapshot changes nothing: Go's TestDeterminism,
//! in both modes, comparing every call either session makes and every next() it reports, from the
//! first event on.

mod common;

use common::*;
use wirebond_core::{MODE_BANDWIDTH, MODE_PERFORMANCE, MS, SessionStatus};

/// The hotel under Moonlight at 10 Mbps for 3 s in `mode`, its Wi-Fi return path blackholed from
/// 1 s to 2 s, reading both snapshots every 50 ms if `snap`: every engine call, and both final
/// snapshots.
fn script(mode: &'static str, snap: bool) -> (Vec<Handled>, [SessionStatus; 2]) {
    let mut p = Pair::hotel_unstarted();
    p.keep_wire = false;
    p.trace = Some(Vec::new());
    p.mode = mode;
    p.start();
    p.run(100 * MS);
    p.expect_keys();
    let (mut video, mut audio, mut input) = moonlight(10.0);
    for i in 0..60 {
        if i == 20 {
            p.path(S_ETH, C_WIFI).down = true;
        }
        if i == 40 {
            p.path(S_ETH, C_WIFI).down = false;
        }
        p.play(50 * MS, &mut [&mut video, &mut audio, &mut input]);
        if snap {
            p.snap(Side::Server);
            p.snap(Side::Client);
        }
    }
    (p.trace.take().expect("set"), [p.snap(Side::Client), p.snap(Side::Server)])
}

/// One engine call in a line.
fn brief(h: &Handled) -> String {
    let calls: Vec<String> = h
        .calls
        .iter()
        .map(|c| match c {
            Call::Send { sock, to, data } => format!("send {} bytes from {sock} to {to}", data.len()),
            Call::Deliver { flow, data } => format!("deliver {} bytes on {:#010x}", data.len(), flow.0),
            Call::Control(m) => format!("control {}", m.t),
        })
        .collect();
    format!("at {} the {:?} handled {} → [{}], next {}", ms(h.at), h.side, h.kind, calls.join(", "), h.next)
}

fn same(mode: &'static str) {
    let (a, sa) = script(mode, false);
    let (b, sb) = script(mode, true);
    let count = |pick: fn(&Call) -> bool| a.iter().flat_map(|h| &h.calls).filter(|c| pick(c)).count();
    let sends = count(|c| matches!(c, Call::Send { .. }));
    let delivers = count(|c| matches!(c, Call::Deliver { .. }));
    let keys = count(|c| matches!(c, Call::Control(m) if m.t == "key"));
    assert!(
        sends > 3000 && delivers > 3000 && keys == 2,
        "{mode}: the script made {sends} sends, {delivers} deliveries and {keys} key messages: too little to compare"
    );
    // Bandwidth rewinds the stalled lane's bulk; performance sends it on the other lane already.
    let rewound = sa[1].rewinds >= 1;
    assert!(
        rewound || mode == MODE_PERFORMANCE,
        "{mode}: the stall rewound nothing, so the script missed the rewind path"
    );
    if let Some(i) = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]) {
        panic!(
            "{mode}: engine call {i} of {} differs between two runs of one script:\n  {}\n  {}",
            a.len(),
            brief(&a[i]),
            brief(&b[i])
        );
    }
    assert_eq!(a.len(), b.len(), "{mode}: the two runs made different numbers of engine calls");
    for (side, (x, y)) in ["client", "server"].iter().zip(sa.iter().zip(&sb)) {
        assert_eq!(format!("{x:?}"), format!("{y:?}"), "{mode}: the {side}'s final snapshots differ");
    }
}

#[test]
fn performance_runs_the_same_twice_and_snapshots_change_nothing() {
    same(MODE_PERFORMANCE);
}

#[test]
fn capacity_runs_the_same_twice_and_snapshots_change_nothing() {
    same(MODE_BANDWIDTH);
}
