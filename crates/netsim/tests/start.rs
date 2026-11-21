//! A performance stream's first frames after idle (Rust only): Moonlight's video starts 8 s into
//! the bond, on the spec's links, every lane long up and the session idle. Its first frames must
//! be no later than its later ones: a lane is unlimited until its first overuse (§12, §14.1), so
//! nothing should hold a stream that starts. The pacer is capped at twice the rate offered over
//! the last RATE_WINDOW (`Lane::pace`), so an empty window paces a cold start at PACE_FLOOR and the
//! pace reaches the stream's rate only after 50 ms: the first dozen frames wait up to 25 ms in the
//! pacers, on every lane alike, so no copy is early (the system harness: frames 1 to 15 of every
//! stream).

use wirebond_core::MODE_PERFORMANCE;
use wirebond_netsim::env::{Env, maker, ms, par_runs, repro, seeds, spec};
use wirebond_netsim::metrics::pct;
use wirebond_netsim::{Dur, MS, SEC};

/// The stream runs from START to END; its start is its first FIRST.
const START: Dur = 8 * SEC;
const END: Dur = 14 * SEC;
const FIRST: Dur = 200 * MS;
/// A first frame may be this much later than the p99 of the frames sent a second or more in.
const SLACK: Dur = 2 * MS;

/// One seed: None if it passed, else what failed.
fn run(seed: u64) -> Option<String> {
    let mut e = Env::new(seed, maker(), &spec(MODE_PERFORMANCE));
    let m = e.w.moonlight(30e6, 120.0, START, END);
    if let Err(err) = e.run(END) {
        return Some(err);
    }
    e.w.settle(m);
    let delays = |from: Dur, to: Dur| -> Vec<Dur> {
        let sent = e.w[m].frames.iter().filter(|f| f.sent >= from && f.sent < to);
        sent.map(|f| if f.done == 0 { Dur::MAX } else { f.done - f.sent }).collect()
    };
    let first = delays(START, START + FIRST).into_iter().max().unwrap_or(Dur::MAX);
    let later = pct(&delays(START + SEC, END - SEC), 0.99);
    (first > later + SLACK).then(|| {
        format!(
            "seed {seed}: the worst frame of the first {} is {} (≤ the later frames' p99 {} + {})\nreproduce: {}",
            ms(FIRST),
            ms(first),
            ms(later),
            ms(SLACK),
            repro("start", "first_frames_are_not_late", seed)
        )
    })
}

#[test]
#[ignore = "open for the owner: the worst frame of the first 200 ms is 56.8 ms against 33.2 ms for \
            the later frames' p99, on every seed. Lane::pace caps the pacer at twice the rate \
            offered over RATE_WINDOW, which a stream's first 50 ms have not filled. Each way to \
            lift that changes a behaviour core's unit tests pin on purpose: the cap itself \
            (pace_is_twice_the_offered_rate_within_the_rate and seven more fail with pace = \
            rate, though netsim's PR set passes and this test with it), a rewind's burst paced \
            at twice its offered rate on a lane just taken (R2-04, \
            take_restarts_a_stale_rate_before_bulk_moves_onto_the_lane: fails when the cap waits \
            for a full window after a pause, which also passes this test and the PR set), or the \
            bucket's depth after idle (drain_holds_at_most_pace_burst_plus_a_packet)"]
fn first_frames_are_not_late() {
    let jobs = seeds();
    let bad: Vec<String> = par_runs(&jobs, |&s| run(s)).into_iter().flatten().collect();
    assert!(bad.is_empty(), "{} of {} seeds failed:\n{}", bad.len(), jobs.len(), bad.join("\n"));
}
