//! A sleep longer than the probe ring: the swarm's seed 5, reduced. While the Mac sleeps, xanth's
//! probes go unechoed until its ring of RTT_RING probes wraps (5.12 s at 20 ms), and every lane of
//! its goes down. After the wake no lane carries bulk, so it goes out unpaced. Sent on every
//! validated lane, it would cross the hotel's shaper twice, on the direct lane and on the
//! Tailscale lane, and the overuse that made would keep every lane from the clean UP_AFTER that
//! brings it back up, so video would never play again. With the cell blackholed the hotel's group
//! is the only one. Runs the PR seeds, or WIREBOND_SEED's, in both modes (in bandwidth the spread
//! empties as every lane goes down: the swarm's seed 23).

use wirebond_netsim::env::{Env, START, defaults, each_run, first_when, hole, maker, ms, repro, seeds_in_both_modes};
use wirebond_netsim::{DevId, Dur, MS, SEC};

/// The Mac sleeps from SLEEP_AT for SLEEP with its clock paused; the stream and the run end at END.
const SLEEP_AT: Dur = 10 * SEC;
const SLEEP: Dur = 8 * SEC;
const END: Dur = 30 * SEC;

/// Within UP of the wake one of xanth's lanes is up again: the Mac's probes resume within a probe
/// interval or two, and a down lane comes back after UP_AFTER (1 s) of clean echoes.
const UP: Dur = 2 * SEC;

/// From UP after the wake to a second before the end, video plays with no gap above GAP (the
/// swarm's liveness bound).
const GAP: Dur = SEC;

/// One seed in one mode: None if it passed, else what failed.
fn run(seed: u64, mode: &'static str) -> Option<String> {
    let mut o = defaults();
    o.mode = mode;
    let mut e = Env::new(seed, maker(), &o);
    let m = e.w.moonlight(20e6, 120.0, START, END);
    hole(&mut e.w, e.cell, 0, END, true, true);
    let wake = SLEEP_AT + SLEEP;
    e.w.at(SLEEP_AT, |w| w.sleep(DevId::Client, SLEEP, false));
    // When xanth first has an up lane after the wake.
    let up = first_when(&mut e.w, wake, 10 * MS, |w| w[DevId::Server].snap().lanes.iter().any(|l| l.state == "up"));
    if let Err(err) = e.run(END) {
        return Some(err);
    }
    e.w.settle(m);
    let mut bad = Vec::new();
    match up.get() {
        Some(t) if t - wake <= UP => {}
        t => bad.push(format!(
            "xanth's first up lane after the wake: {} (≤ {})",
            t.map_or("never".into(), |t| ms(t - wake)),
            ms(UP)
        )),
    }
    let gap = e.w[m].max_gap(wake + UP, END - SEC);
    if gap > GAP {
        bad.push(format!("longest video gap from {} after the wake: {} (≤ {})", ms(UP), ms(gap), ms(GAP)));
    }
    (!bad.is_empty()).then(|| {
        let again = repro("sleep_ring", "lanes_come_back", seed);
        format!("seed {seed}, {mode}: {}\nreproduce: {again}", bad.join("; "))
    })
}

/// Every seed of the set in both modes: after the wake a lane comes back up and video plays again.
#[test]
fn lanes_come_back() {
    each_run(&seeds_in_both_modes(), |&(s, mode)| run(s, mode));
}
