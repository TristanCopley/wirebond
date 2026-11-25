//! A path that opens after its punch round: the phone is blackholed, both ways, from the start
//! until OPEN, past the punch rounds of the hello and of the stream's first datagram, while video
//! flows on the hotel. The pairs over the phone never validated, so no lane of theirs has a return
//! address and only a punch round probes them (§9.4). Rounds once began only on a link or candidate
//! change, a wake, a new control channel or the first datagram after idle, so with data flowing the
//! phone's lanes stayed punching for good. Runs the PR seeds, or WIREBOND_SEED's, in both modes.

use wirebond_core::PUNCH_GAP;
use wirebond_netsim::env::{
    Env, START, defaults, each_run, first_when, hole, lane_up, maker, ms, repro, seeds_in_both_modes,
};
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
    let at = first_when(&mut e.w, OPEN, SEC / 100, up);
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
    each_run(&seeds_in_both_modes(), |&(seed, mode)| run(seed, mode));
}
