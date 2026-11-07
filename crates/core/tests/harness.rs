//! The test driver's own tests: the network model, the control channel, the run loop and the
//! checks, driven by a fake engine so that they hold whatever state the real engine is in.
//! Reference values come from Go's core/pair_test.go driver, which this one ports draw for draw.

mod common;

use common::*;
use std::net::SocketAddr;
use wirebond_core::{Event, FlowId, MS, Mono, Msg, Out, RidTable, SEC, SockId};

/// What the fake saw, in order.
#[derive(Clone, Debug, PartialEq)]
enum Seen {
    App { at: Mono, flow: FlowId, index: u32 },
    Packet { at: Mono, sock: SockId, from: SocketAddr, data: Vec<u8> },
    Control { at: Mono, t: String },
    Links { at: Mono, links: usize },
    Tick { at: Mono },
    Wake { at: Mono },
}

/// A fake engine. On an inner packet it routes by the packet's flow key (§13): the ping flow's
/// (UDP source port [`PING`]) becomes a control message, any other's datagram is sent from `sock`
/// to `to`; it delivers every lane packet whole, on flow 0; it asks for a tick at each time in
/// `wants`, or, `stuck`, always at time 0.
#[derive(Default)]
struct Fake {
    sock: SockId,
    to: Option<SocketAddr>,
    wants: Vec<Mono>,
    stuck: bool,
    seen: Vec<Seen>,
}

impl Engine for Fake {
    fn handle(&mut self, now: Mono, ev: Event<'_>, _: &mut RidTable, out: &mut dyn Out) {
        match ev {
            Event::Send { packet } => {
                let key = wirebond_core::flow::Key::parse(packet).expect("an inner packet");
                let (flow, data) = outer(packet);
                self.seen.push(Seen::App { at: now, flow, index: index(data) });
                if key.sport == PING.0 {
                    out.control(Msg { t: "ping".into(), ..Msg::default() });
                } else if let Some(to) = self.to {
                    out.send(self.sock, to, data);
                }
            }
            Event::Packet { sock, from, data } => {
                self.seen.push(Seen::Packet { at: now, sock, from, data: data.to_vec() });
                out.deliver(data);
            }
            Event::Control(m) => self.seen.push(Seen::Control { at: now, t: m.t.clone() }),
            Event::Links { links, .. } => self.seen.push(Seen::Links { at: now, links: links.len() }),
            Event::Tick => {
                assert!(
                    self.stuck || self.wants.first().is_some_and(|&w| w <= now),
                    "a tick at {now} nobody asked for"
                );
                self.seen.push(Seen::Tick { at: now });
                self.wants.retain(|&w| w > now);
            }
            Event::Wake => self.seen.push(Seen::Wake { at: now }),
        }
    }

    fn next(&self) -> Mono {
        if self.stuck { 0 } else { self.wants.first().copied().unwrap_or(Mono::MAX) }
    }
}

fn pair(mesh_delay: Mono) -> Pair<Fake> {
    let mut p = Pair::with(Fake::default(), Fake::default(), mesh_delay);
    p.c.engine.sock = MESH_SOCK;
    p.c.engine.to = Some(S_MESH);
    p.s.engine.sock = MESH_SOCK;
    p.s.engine.to = Some(C_MESH);
    p
}

/// The test flows: datagrams, and the flow the fake turns into control messages.
const F: FlowId = FlowId(9);
const PING: FlowId = FlowId(1);

fn arrivals(e: &Fake) -> Vec<(Mono, SockId, SocketAddr)> {
    e.seen
        .iter()
        .filter_map(|s| match s {
            Seen::Packet { at, sock, from, .. } => Some((*at, *sock, *from)),
            _ => None,
        })
        .collect()
}

fn fates(p: &Pair<Fake>) -> Vec<Fate> {
    p.wire.iter().map(|s| s.fate).collect()
}

#[test]
fn det_rand_matches_go() {
    let mut b = [0; 8];
    DetRand(1).fill(&mut b);
    assert_eq!(b, [108, 130, 165, 98, 203, 128, 141, 16], "detRand(1) bytes, from Go");
    let mut r = DetRand(7);
    let floats = [r.float(), r.float(), r.float()];
    assert_eq!(floats, [0.4932122668392295, 0.9556595384052861, 0.9065758219926131], "detRand(7) floats, from Go");
    assert_eq!(DetRand(2).next_u64(), 14170967488582549417, "detRand(2) step, from Go");
    let mut k = [0; 8];
    rand(1)(&mut k);
    assert_eq!(k, b, "a session's key bytes are its seed's stream");
}

#[test]
fn delay_and_source() {
    let mut p = pair(33 * MS);
    p.app(Side::Client, F, 10);
    p.run(100 * MS);
    assert_eq!(arrivals(&p.s.engine), [(33 * MS, MESH_SOCK, C_MESH)], "one packet, after the path's delay");
    let Some(Seen::Packet { data, .. }) = p.s.engine.seen.first() else { panic!("no packet: {:?}", p.s.engine.seen) };
    assert_eq!((data.len(), index(data)), (10, 0), "the datagram's bytes, numbered 0");
    assert_eq!(p.s.got(FlowId(0)), std::slice::from_ref(data), "a delivery is recorded per flow");
    let sent = &p.wire[0];
    assert_eq!(
        (sent.at, sent.by, sent.sock, sent.dst, sent.rx_sock, sent.from, sent.fate),
        (0, Side::Client, MESH_SOCK, S_MESH, MESH_SOCK, Some(C_MESH), Fate::Arrives(33 * MS))
    );
    assert_eq!((p.c.sent, p.s.sent, p.now), (1, 0, 100 * MS));
}

#[test]
fn rate_serializes_and_a_full_buffer_drops() {
    let mut p = pair(10 * MS);
    // 8 Mbit/s: a 972-byte datagram is 1,000 bytes with IPv4 and UDP, 1 ms on the wire.
    *p.path(C_MESH, S_MESH) = Path::shaped(10 * MS, 8e6, 5 * MS);
    for _ in 0..10 {
        p.app(Side::Client, F, 972);
    }
    // The k-th waits k ms for the serializer: up to 5 ms is within the buffer.
    let want: Vec<Fate> =
        (0..10).map(|k| if k <= 5 { Fate::Arrives(10 * MS + (k + 1) * MS) } else { Fate::Full }).collect();
    assert_eq!(fates(&p), want);
    p.run(SEC);
    let at: Vec<Mono> = arrivals(&p.s.engine).iter().map(|a| a.0).collect();
    assert_eq!(at, [11, 12, 13, 14, 15, 16].map(|t| t * MS));
    let order: Vec<u32> = p.s.got(FlowId(0)).iter().map(|d| index(d)).collect();
    assert_eq!(order, [0, 1, 2, 3, 4, 5], "in send order");
}

#[test]
fn loss_draws_match_go() {
    let run = || {
        let mut p = pair(10 * MS);
        p.path(C_MESH, S_MESH).loss = 0.25;
        p.keep_wire = true;
        for _ in 0..20_000 {
            p.app(Side::Client, F, 100);
        }
        fates(&p)
    };
    let a = run();
    let lost = a.iter().filter(|f| **f == Fate::Lost).count();
    assert_eq!(lost, 4924, "Go's driver loses 4,924 of 20,000 at 25% with the same draws");
    assert_eq!(a, run(), "the same script gives the same fates");
    let mut p = pair(10 * MS);
    p.path(C_MESH, S_MESH).loss = 1.0;
    p.app(Side::Client, F, 100);
    p.path(C_MESH, S_MESH).loss = 0.0;
    p.app(Side::Client, F, 100);
    assert_eq!(fates(&p), [Fate::Lost, Fate::Arrives(10 * MS)]);
}

#[test]
fn down_drops_at_the_sender_and_spares_what_is_in_flight() {
    let mut p = pair(10 * MS);
    p.app(Side::Client, F, 10);
    p.run(MS);
    p.path(C_MESH, S_MESH).down = true;
    p.app(Side::Client, F, 10);
    p.run(MS);
    p.path(C_MESH, S_MESH).down = false;
    p.app(Side::Client, F, 10);
    p.run(100 * MS);
    assert_eq!(fates(&p), [Fate::Arrives(10 * MS), Fate::Down, Fate::Arrives(12 * MS)]);
    let got: Vec<u32> = p.s.got(FlowId(0)).iter().map(|d| index(d)).collect();
    assert_eq!(got, [0, 2]);
}

#[test]
fn a_path_stays_fifo_when_its_delay_drops() {
    let mut p = pair(50 * MS);
    p.app(Side::Client, F, 10);
    p.run(MS);
    p.path(C_MESH, S_MESH).delay = 10 * MS;
    p.app(Side::Client, F, 10); // would arrive at 11 ms, ahead of the first
    p.run(100 * MS);
    assert_eq!(fates(&p), [Fate::Arrives(50 * MS), Fate::Arrives(50 * MS)]);
    let got: Vec<u32> = p.s.got(FlowId(0)).iter().map(|d| index(d)).collect();
    assert_eq!(got, [0, 1], "arrivals keep send order");
}

#[test]
fn nat_shows_the_mapping_and_hides_the_socket() {
    let mut p = pair(33 * MS);
    p.add_link(Side::Client, 1, "Wi-Fi", false, true, C_WIFI);
    p.add_link(Side::Server, 1, "Ethernet", false, false, S_ETH);
    p.nat.insert(C_WIFI, C_WIFI_NAT);
    p.link(C_WIFI, S_ETH, Path::new(32 * MS));
    let infos = p.infos(Side::Client);
    assert_eq!(infos.len(), 1);
    assert_eq!((infos[0].id, &infos[0].label[..], infos[0].default), (1, "Wi-Fi", true));
    assert_eq!(infos[0].cands, [C_WIFI, C_WIFI_NAT], "the local address, then the mapping (§6.3)");
    assert_eq!(p.infos(Side::Server)[0].cands, [S_ETH], "a public address has no mapping");

    p.c.engine.sock = 11;
    p.c.engine.to = Some(S_ETH);
    p.app(Side::Client, F, 10); // from behind the NAT
    p.s.engine.sock = 11;
    p.s.engine.to = Some(C_WIFI_NAT);
    p.app(Side::Server, F, 10); // to the mapping
    p.s.engine.to = Some(C_WIFI);
    p.app(Side::Server, F, 10); // to the private address
    p.c.engine.sock = 99;
    p.c.engine.to = Some(S_ETH);
    p.app(Side::Client, F, 10); // from a socket the client does not have
    p.run(100 * MS);
    assert_eq!(arrivals(&p.s.engine), [(32 * MS, 11, C_WIFI_NAT)]);
    assert_eq!(arrivals(&p.c.engine), [(32 * MS, 11, S_ETH)]);
    assert_eq!(fates(&p), [Fate::Arrives(32 * MS), Fate::Arrives(32 * MS), Fate::NoPath, Fate::NoPath]);
    assert_eq!((p.wire[2].rx_sock, p.wire[3].from), (0, None));
    assert_eq!((p.route(C_WIFI_NAT), p.route(C_WIFI), p.route(S_ETH)), (Some(C_WIFI), None, Some(S_ETH)));
}

#[test]
fn control_channel_delays_and_drops_while_down() {
    let mut p = pair(20 * MS);
    let ping = PING;
    p.app(Side::Client, ping, 3);
    p.run(MS);
    p.ctl_up = false;
    p.app(Side::Client, ping, 3);
    p.run(100 * MS);
    assert_eq!(p.s.engine.seen.iter().filter(|s| matches!(s, Seen::Control { .. })).count(), 1);
    assert!(p.s.engine.seen.contains(&Seen::Control { at: 20 * MS, t: "ping".into() }));
    assert_eq!(p.c.ctls.len(), 2, "both are recorded as sent");

    // start(): links to both, the client's mode, then both hellos after the channel's delay.
    let mut p = pair(20 * MS);
    p.start();
    p.run(100 * MS);
    let c = &p.c.engine.seen;
    assert_eq!(
        c[..3],
        [
            Seen::Links { at: 0, links: 0 },
            Seen::Control { at: 0, t: "mode".into() },
            Seen::Control { at: 20 * MS, t: "hello".into() }
        ]
    );
    assert_eq!(p.s.engine.seen, [Seen::Links { at: 0, links: 0 }, Seen::Control { at: 20 * MS, t: "hello".into() }]);
}

#[test]
fn ticks_come_when_asked_and_arrivals_first() {
    let mut p = pair(7 * MS);
    p.c.engine.wants = vec![5 * MS, 7 * MS, 7 * MS, 10 * MS, 10 * MS + 1];
    p.s.engine.wants = vec![5 * MS, 6 * MS];
    p.app(Side::Server, F, 10); // reaches the client at 7 ms, with its tick
    p.run(10 * MS);
    let c: Vec<&Seen> = p.c.engine.seen.iter().collect();
    assert_eq!(
        c,
        [
            &Seen::Tick { at: 5 * MS },
            &Seen::Packet { at: 7 * MS, sock: MESH_SOCK, from: S_MESH, data: datagram(0, 10) },
            &Seen::Tick { at: 7 * MS },
            &Seen::Tick { at: 10 * MS },
        ],
        "a tick at the end of a run is due, one after it is not; an arrival comes before a tick at the same time"
    );
    assert_eq!(p.now, 10 * MS);
    assert_eq!(p.c.engine.wants, [10 * MS + 1]);
    // At the same time the client goes first.
    let mut p = pair(7 * MS);
    p.c.engine.wants = vec![5 * MS];
    p.s.engine.wants = vec![5 * MS];
    p.trace = Some(Vec::new());
    p.run(10 * MS);
    let order: Vec<(Mono, Side, &str)> = p.trace.iter().flatten().map(|h| (h.at, h.side, h.kind)).collect();
    assert_eq!(order, [(5 * MS, Side::Client, "tick"), (5 * MS, Side::Server, "tick")]);
}

#[test]
#[should_panic(expected = "no progress")]
fn a_session_that_never_lets_time_move_fails() {
    let mut p = pair(7 * MS);
    p.c.engine.stuck = true;
    p.run(MS);
}

#[test]
fn generators_send_on_schedule() {
    let (mut video, mut audio, mut input) = moonlight(10.0);
    assert_eq!((video.gap, video.size, video.side), (1_120_000, 1400, Side::Server));
    assert_eq!((audio.gap, input.gap), (5 * MS, 2 * MS));
    let mut p = pair(7 * MS);
    p.play(100 * MS, &mut [&mut video, &mut audio, &mut input]);
    assert_eq!((p.s.count(VIDEO), p.s.count(AUDIO), p.c.count(INPUT)), (90, 21, 51));
    let at: Vec<Mono> =
        p.s.engine
            .seen
            .iter()
            .filter_map(|s| match s {
                Seen::App { at, flow, .. } if *flow == VIDEO => Some(*at),
                _ => None,
            })
            .collect();
    assert_eq!(at, (0..90).map(|k| k * 1_120_000).collect::<Vec<_>>(), "one every gap from the start");
    let idx: Vec<u32> =
        p.c.engine
            .seen
            .iter()
            .filter_map(|s| match s {
                Seen::App { flow, index, .. } if *flow == INPUT => Some(*index),
                _ => None,
            })
            .collect();
    assert_eq!(idx, (0..51).collect::<Vec<_>>(), "numbered per flow from 0");
    assert_eq!(p.now, 100 * MS);
    p.play(10 * MS, &mut [&mut video]); // a later play carries on where the last one stopped
    assert_eq!(p.s.count(VIDEO), 99);
}

#[test]
fn checks_accept_what_arrived_in_order() {
    let mut p = pair(7 * MS);
    for i in [0u32, 1, 3, 4] {
        p.c.got.entry(F).or_default().push(datagram(i, 3));
    }
    in_order(&p.c, F, 5, 1);
    complete(&p.c, F, 5, 1);
}

#[test]
#[should_panic(expected = "delivered after")]
fn checks_reject_disorder() {
    let mut p = pair(7 * MS);
    for i in [0u32, 2, 1] {
        p.c.got.entry(F).or_default().push(datagram(i, 3));
    }
    in_order(&p.c, F, 3, 0);
}

#[test]
#[should_panic(expected = "twice")]
fn checks_reject_duplicates() {
    let mut p = pair(7 * MS);
    for i in [0u32, 1, 1] {
        p.c.got.entry(F).or_default().push(datagram(i, 3));
    }
    complete(&p.c, F, 3, 1);
}

#[test]
#[should_panic(expected = "got 2 of 4")]
fn checks_reject_loss() {
    let mut p = pair(7 * MS);
    for i in [0u32, 3] {
        p.c.got.entry(F).or_default().push(datagram(i, 3));
    }
    complete(&p.c, F, 4, 1);
}

/// The driver runs a real Session too, whatever it does: time moves and the run ends.
#[test]
fn drives_a_session() {
    let mut p = Pair::new(10 * MS);
    p.start();
    p.run(SEC);
    assert_eq!(p.now, SEC);
    p.app(Side::Client, F, 10);
    p.run(SEC);
    assert_eq!((p.now, p.c.count(F)), (2 * SEC, 1));
}

/// The bytes of datagram `i` of `size` bytes, as `app` numbers them.
fn datagram(i: u32, size: usize) -> Vec<u8> {
    let mut d = vec![0; size];
    d[..3].copy_from_slice(&i.to_be_bytes()[1..]);
    d
}
