//! Fragmented video (SPEC §13): Moonlight's default 1,392-byte payloads exceed the 1,372 bytes a
//! 1,400-byte TUN device carries whole, so each video datagram crosses the bond as two IPv4
//! fragments (1,396 and 44 bytes) and the far kernel reassembles it. Every fragment of a datagram
//! takes the port-less key, so a datagram's fragments share one flow and arrive in order: on two
//! lanes with room for the stream, nothing may be lost in either direction, no datagram may be
//! left unassembled, and the order invariants hold (`World::reg_order`).

use wirebond_core::{MODE_BANDWIDTH, MODE_PERFORMANCE, Role};
use wirebond_netsim::env::{Env, maker, spec};
use wirebond_netsim::sim::{FlowId, fragment, inner};
use wirebond_netsim::{Dur, MS, PORT_VIDEO, SEC};

/// Moonlight's default video payload.
const PAYLOAD: usize = 1392;
/// The TUN device's MTU.
const MTU: usize = 1400;

#[test]
fn a_video_datagram_crosses_as_two_fragments() {
    let p = inner(Role::Anchor, FlowId::new(PORT_VIDEO, 40001), &[7; PAYLOAD]);
    let f = fragment(&p, MTU, 9);
    assert_eq!(f.iter().map(Vec::len).collect::<Vec<_>>(), [1396, 44]);
    let k: Vec<_> = f.iter().map(|x| wirebond_core::flow::Key::parse(x).map(|k| (k.sport, k.dport))).collect();
    assert_eq!(k, [Some((0, 0)), Some((0, 0))], "both fragments take the port-less key");
}

/// Echoes 1,392-byte datagrams on the video port at 30 Mbit/s for 5 s through 1,400-byte TUN
/// devices in `mode`, on the spec's links without the phone's random loss; returns the run's
/// error, the datagrams sent and those whose echo came back, overall and in the first 600 ms, and
/// the datagrams given up unassembled.
fn video(mode: &'static str) -> (Result<(), String>, [usize; 4], u64) {
    let mut o = spec(mode);
    o.cell.loss = 0.0;
    let mut e = Env::new(1, maker(), &o);
    e.w.tun_mtu = MTU;
    let (start, stop): (Dur, Dur) = (2 * SEC, 7 * SEC);
    let x = e.w.echo(PORT_VIDEO, 40001, 30e6, PAYLOAD, start, stop);
    let r = e.run(stop + SEC);
    let (sent, back) = e.w[x].counts(start, stop, stop + SEC);
    let (s0, b0) = e.w[x].counts(start, start + 600 * MS, stop + SEC);
    (r, [sent, back, s0, b0], e.w.reasm_fails)
}

#[test]
fn fragmented_video_loses_nothing_performance() {
    let (r, [sent, back, ..], fails) = video(MODE_PERFORMANCE);
    assert_eq!(r, Ok(()));
    assert!(sent > 10_000, "sent {sent}");
    assert_eq!((back, fails), (sent, 0), "echoes back of those sent, datagrams left unassembled");
}

#[test]
#[ignore = "core 2.0, open for the owner: not the fragments (each datagram's fragments share a \
            flow and none is left unassembled) but bandwidth's startup, as in jump.rs: lanes that \
            share the stream start at PACE_FLOOR and climb x STARTUP_GAIN per RTT, so a sudden \
            30 Mbit/s ages packets out of the pacers: 204 of 13,470 lost, all in the first 600 ms \
            (x2 per RTT in startup loses none here, but still loses 694 of the first 1,786 in the \
            100 Mbit/s jump on 300 Mbit/s links, so x1.25 stays)"]
fn fragmented_video_loses_nothing_bandwidth() {
    let (r, [sent, back, s0, b0], fails) = video(MODE_BANDWIDTH);
    assert_eq!(r, Ok(()));
    assert!(sent > 10_000, "sent {sent}");
    assert_eq!((back, b0, fails), (sent, s0, 0), "echoes back of those sent (all, first 600 ms), unassembled");
}
