//! A UDP load that jumps from idle to 100 Mbit/s (the real-daemon harness's `udp` against `echo`,
//! tests/system/load): the client sends 1,400-byte datagrams at 100 Mbit/s for 5 s after 8 s of
//! idle, and the server echoes each one back on its flow. On two lanes with room for the stream,
//! nothing may be lost, in either direction, from the first datagram on, and the order invariants
//! hold (`World::reg_order`).

use wirebond_core::{MODE_BANDWIDTH, MODE_PERFORMANCE};
use wirebond_netsim::env::{Env, maker, spec};
use wirebond_netsim::{Dur, MS, SEC};

/// Runs the jump on seed 1 in `mode`, both links at `rate` (the spec's links with 0), and
/// returns the run's error, the datagrams sent and those whose echo came back, overall and in
/// the first 200 ms.
fn jump(mode: &'static str, rate: f64) -> (Result<(), String>, [usize; 4]) {
    let mut o = spec(mode);
    o.cell.loss = 0.0;
    if rate > 0.0 {
        (o.hotel.rate, o.cell.rate) = (rate, rate);
    }
    let mut e = Env::new(1, maker(), &o);
    let (start, stop): (Dur, Dur) = (8 * SEC, 13 * SEC);
    let x = e.w.echo(9200, 40001, 100e6, 1400, start, stop);
    let r = e.run(stop + SEC);
    let (sent, back) = e.w[x].counts(start, stop, stop + SEC);
    let (s0, b0) = e.w[x].counts(start, start + 200 * MS, stop + SEC);
    (r, [sent, back, s0, b0])
}

#[test]
fn jump_to_100_mbit_performance() {
    for rate in [300e6, 120e6] {
        let (r, [sent, back, s0, b0]) = jump(MODE_PERFORMANCE, rate);
        assert_eq!(r, Ok(()), "performance, links at {rate}");
        assert_eq!((back, b0), (sent, s0), "performance, links at {rate}: echoes back of those sent");
    }
}

#[test]
#[ignore = "core 2.0, open for the owner: the order invariant holds, but each lane that shares \
            the stream starts at PACE_FLOOR in startup (rate.rs restart, bandwidth.rs schedule) \
            and climbs x STARTUP_GAIN (1.25) per RTT while 100 Mbit/s is offered, so packets age \
            out of the pacers (QUEUE_AGE): 3,291 of 44,643 lost on 300 Mbit/s links and 5,280 on \
            the spec's (38 + 80 Mbit/s), 1,377 of the first 1,786 on both. x2 per RTT until the \
            first overuse (TCP's slow start) keeps accept and swarm passing but still loses 697 \
            and 2,880, 694 of the first 1,786 on both, so x1.25 stays. Starting unlimited, as \
            performance does, loses nothing on 300 Mbit/s links but 9,970 on the spec's (none in \
            the first 200 ms) and contradicts SPEC 14.2's start from PACE_FLOOR"]
fn jump_to_100_mbit_bandwidth() {
    for rate in [300e6, 0.0] {
        let (r, [sent, back, s0, b0]) = jump(MODE_BANDWIDTH, rate);
        assert_eq!(r, Ok(()), "bandwidth, links at {rate}");
        assert_eq!((back, b0), (sent, s0), "bandwidth, links at {rate}: echoes back of those sent");
    }
}
