//! A path that opens after its punch round (Rust only): the phone is blackholed, both ways, from
//! the start until OPEN, past the punch rounds of the hello and of the stream's first datagram,
//! while video flows on the hotel. The pairs over the phone never validated, so no lane of theirs
//! has a return address and only a punch round probes them (§11.1). Rounds once began only on a
//! link or candidate change, a wake, a new control channel or the first datagram after idle, so
//! with data flowing the phone's lanes stayed punching for good. Runs the PR seeds, or
//! WIREBOND_SEED's, in both modes.

use wirebond_core::{MODE_BANDWIDTH, MODE_PERFORMANCE, PUNCH_GAP};
use wirebond_netsim::env::{Env, START, defaults, hole, lane_up, maker, ms, par_runs, repro, seeds};
use wirebond_netsim::{DevId, Dur, SEC};

/// The phone opens at OPEN; the stream and the run end at END.
const OPEN: Dur = 12 * SEC;
const END: Dur = 30 * SEC;

/// Within UP of the phone opening both ends have its lane up: a punch round begins within
/// PUNCH_GAP, and the lane validates within a second of it.
const UP: Dur = PUNCH_GAP as Dur + SEC;

/// One seed in one mode: None if it passed, else what failed.
fn run(seed: u64, mode: &'static str) -> Option<String> {
    let mut o = defaults();
    o.mode = mode;
    let mut e = Env::new(seed, maker(), &o);
    e.w.moonlight(20e6, 120.0, START, END);
    hole(&mut e.w, e.cell, 0, OPEN, true, true);
    let id = e.w[e.cell].id;
    let up = move |w: &wirebond_netsim::World| DevId::BOTH.iter().all(|&d| lane_up(&w[d].snap(), id));
    let before = std::rc::Rc::new(std::cell::Cell::new(false));
    let b = std::rc::Rc::clone(&before);
    e.w.at(OPEN, move |w| b.set(up(w)));
    let at = std::rc::Rc::new(std::cell::Cell::new(None::<Dur>));
    let a = std::rc::Rc::clone(&at);
    e.w.every(OPEN, SEC / 100, move |w| {
        if a.get().is_none() && up(w) {
            a.set(Some(w.now()));
        }
        a.get().is_none()
    });
    if let Err(err) = e.run(END) {
        return Some(err);
    }
    let mut bad = Vec::new();
    if before.get() {
        bad.push("the phone's lane was up while the phone was blackholed".to_string());
    }
    match at.get() {
        Some(t) if t - OPEN <= UP => {}
        t => bad.push(format!(
            "the phone's lane up at both ends after the phone opened: {} (≤ {})",
            t.map_or("never".into(), |t| ms(t - OPEN)),
            ms(UP)
        )),
    }
    (!bad.is_empty()).then(|| {
        format!(
            "seed {seed}, {mode}:\n  {}\nreproduce: {}",
            bad.join("\n  "),
            repro("repunch", "a_path_that_opens_late_is_punched_again", seed)
        )
    })
}

#[test]
fn a_path_that_opens_late_is_punched_again() {
    let jobs: Vec<(u64, &'static str)> =
        seeds().into_iter().flat_map(|s| [(s, MODE_PERFORMANCE), (s, MODE_BANDWIDTH)]).collect();
    let bad: Vec<String> = par_runs(&jobs, |&(seed, mode)| run(seed, mode)).into_iter().flatten().collect();
    assert!(bad.is_empty(), "{} of {} runs failed:\n{}", bad.len(), jobs.len(), bad.join("\n"));
}
