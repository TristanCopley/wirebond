//! Engine benchmarks: two sessions back to back through the public API only, on the same events
//! as the Go implementation's benchmark (removed after 382cb8d: `git show 382cb8d:core/bench_test.go`).
//! notes/rust.md ("Engine benchmarks") gives the reasons for each choice.
//!
//! ```text
//! cargo bench -p wirebond-core --bench engine [-- [--datagrams N] [--runs R] [FILTER]]
//! ```
//!
//! For each scenario and path it prints a line in the format of Go's benchmarks: the median of R
//! runs (5), each timing N datagrams (50,000) in the steady state, in nanoseconds per datagram,
//! with the Mbps that cost allows one core and the calls left out as preempted. Run without
//! `--bench`, as `cargo test --benches` does, it checks each scenario briefly instead, as Go's
//! TestEngineBench does.
//!
//! Each `Session::handle` call is timed on its own, together with the driver's work on its
//! actions (a send copied out, a delivery counted), less the clock's own cost; it counts toward
//! - send: xanth's `Event::App`, and its `Event::Tick`s that send a data packet (the pacer
//!   releasing what an App queued): seal, scheduling, pacing and the rewind ring;
//! - recv: the Mac's data packets, and its Ticks that deliver a datagram (the reorder timer):
//!   open, the lane and session replay windows, copies and per-flow order.
//!
//! Probes, the peer's probes and timers that do neither count toward no path.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use wirebond_core::{
    Event, FlowId, Link, LinkInfo, MAX_PACKET, MODE_BANDWIDTH, MODE_PERFORMANCE, MS, MeshLane, Mono, Msg, Out, PORT,
    PROTOCOL, Rand, RidTable, Role, SEC, SMALL_BIT, Session, SockId, TYPE_DATA,
};

/// One benchmark: a flow from xanth (the server) to the Mac (the client) in one mode.
struct Scenario {
    /// mode/datagram size, as the benchmark is named.
    name: &'static str,
    mode: &'static str,
    /// Bytes per datagram.
    size: usize,
    /// Datagrams per video frame, `SHARD_GAP` apart; 1 for a small flow.
    burst: usize,
    /// Between frames, or between a small flow's datagrams.
    gap: Mono,
}

const SCENARIOS: [Scenario; 4] = [
    // Video as Sunshine sends it to Moonlight through 127.0.0.1: 1,408-byte datagrams, 120
    // frames a second, each frame one burst (CONTEXT.md). In performance 30 Mbps, every packet on
    // both lanes; in bandwidth 90 Mbps, as netsim's A5, spread over both lanes.
    scenario("performance/1408", MODE_PERFORMANCE, 1408, 23, SEC / 120),
    scenario("bandwidth/1408", MODE_BANDWIDTH, 1408, 68, SEC / 120),
    // A small flow, as Moonlight's input: a 100-byte inner packet (a 72-byte datagram and its
    // IPv4/UDP header) every millisecond, 0.8 Mbps, so under SMALL_RATE, which counts whole inner
    // packets in 2.0 (§13): copied on both lanes and delivered without reorder.
    scenario("performance/100", MODE_PERFORMANCE, 72, 1, MS),
    scenario("bandwidth/100", MODE_BANDWIDTH, 72, 1, MS),
];

const fn scenario(name: &'static str, mode: &'static str, size: usize, burst: usize, gap: Mono) -> Scenario {
    Scenario { name, mode, size, burst, gap }
}

impl Scenario {
    /// When datagram `j` of the stream leaves xanth.
    fn at(&self, j: usize) -> Mono {
        STREAM_START + (j / self.burst) as Mono * self.gap + (j % self.burst) as Mono * SHARD_GAP
    }
}

const STREAM_START: Mono = SEC; // the stream's first datagram
const WARM_UP: Mono = 10 * SEC; // timing starts at the first datagram from here
const SHARD_GAP: Mono = 14_080; // 1,408 bytes at Sunshine's 0.8 Gbit/s
const CTL_DELAY: Mono = 31 * MS; // the control channel, one way, over the Tailscale lane
const IP_UDP: Mono = 28; // IPv4 and UDP header bytes, for a bottleneck's serialization
const HOTEL: u16 = 0x0101; // the two direct lanes: Mac link id << 8 | xanth link id
const CELL: u16 = 0x0201;

/// How long a timed call can take only if the thread did not run all along: no `handle` call in
/// these scenarios takes a tenth of it, and the scheduler's time slices are about this long. Such
/// calls are left out and counted.
const PREEMPTED: Duration = Duration::from_millis(1);

/// The fewest datagrams for which a run checks what its window carried.
const MIN_CHECKED: usize = 1000;

// The owner's setup (SPEC §16) without jitter or loss: the Mac on the hotel's Wi-Fi (38 Mbps, 30
// ms one way, as netsim's Hotel-clean) and the iPhone (80 Mbps, 45 ms, as netsim's Cell), xanth
// on Ethernet, and the Tailscale lane (31 ms), which crosses the hotel's Wi-Fi.
const MAC_WIFI: SocketAddr = v4([192, 168, 1, 20]);
const MAC_PHONE: SocketAddr = v4([172, 20, 10, 2]);
const MAC_MESH: SocketAddr = v4([100, 64, 0, 1]);
const XANTH_ETH: SocketAddr = v4([203, 0, 113, 10]);
const XANTH_MESH: SocketAddr = v4([100, 64, 0, 2]);

const fn v4([a, b, c, d]: [u8; 4]) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), PORT))
}

fn main() -> ExitCode {
    let opts = match Opts::parse(std::env::args().skip(1)) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("engine: {e}");
            return ExitCode::FAILURE;
        }
    };
    let clock = clock();
    if opts.bench {
        println!(
            "# the median of {} runs of {} datagrams; each timed call less the clock's {} ns",
            opts.runs,
            opts.datagrams,
            clock.as_nanos()
        );
    }
    for sc in SCENARIOS.iter().filter(|sc| sc.name.contains(&opts.filter)) {
        let mut runs = Vec::with_capacity(opts.runs);
        for _ in 0..opts.runs {
            match measure(sc, opts.datagrams, clock) {
                Ok(run) => runs.push(run),
                Err(e) => {
                    eprintln!("engine: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        if !opts.bench {
            println!("engine: {} ok", sc.name);
            continue;
        }
        let send: Vec<Figure> = runs.iter().map(|r| r.send).collect();
        let recv: Vec<Figure> = runs.iter().map(|r| r.recv).collect();
        for (path, figures) in [("send", send), ("recv", recv)] {
            let ns = median(figures.iter().map(|f| f.ns).collect());
            let mbps = if ns > 0.0 { sc.size as f64 * 8e3 / ns } else { 0.0 };
            let slow: usize = figures.iter().map(|f| f.slow).sum();
            println!(
                "BenchmarkEngine/{}/{path}\t{:8}\t{ns:10.1} ns/datagram\t{mbps:10.1} Mbps\t{slow:4} preempted",
                sc.name, opts.datagrams
            );
        }
    }
    ExitCode::SUCCESS
}

/// Runs scenario `sc` to the steady state and times `n` datagrams.
fn measure(sc: &'static Scenario, n: usize, clock: Duration) -> Result<Run, String> {
    let mut w = World::warm(sc)?;
    for _ in 0..n {
        w.datagram()?;
    }
    w.finish(n, clock)
}

/// The command line. cargo bench passes `--bench`; without it, as under `cargo test --benches`,
/// each scenario runs once, briefly, as a check.
struct Opts {
    bench: bool,
    datagrams: usize,
    runs: usize,
    filter: String,
}

impl Opts {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Opts, String> {
        let (mut bench, mut datagrams, mut runs, mut filter) = (false, None, None, String::new());
        while let Some(a) = args.next() {
            match a.as_str() {
                "--bench" => bench = true,
                "--datagrams" => datagrams = Some(count(args.next())?),
                "--runs" => runs = Some(count(args.next())?),
                _ if !a.starts_with('-') && filter.is_empty() => filter = a,
                _ => return Err(format!("{a}? want [--datagrams N] [--runs R] [FILTER]")),
            }
        }
        let (n, r) = if bench { (50_000, 5) } else { (MIN_CHECKED, 1) };
        let (datagrams, runs) = (datagrams.unwrap_or(n), runs.unwrap_or(r));
        Ok(Opts { bench, datagrams, runs, filter })
    }
}

fn count(arg: Option<String>) -> Result<usize, String> {
    match arg.as_deref().map(str::parse) {
        Some(Ok(n)) if n > 0 => Ok(n),
        _ => Err(format!("want a positive count, got {arg:?}")),
    }
}

/// The middle value; the mean of the two middle ones for an even count.
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 }
}

/// One run's figures for the two paths.
struct Run {
    send: Figure,
    recv: Figure,
}

/// One path's: nanoseconds per datagram, and the calls left out as preempted.
#[derive(Clone, Copy)]
struct Figure {
    ns: f64,
    slow: usize,
}

/// The time a path's calls took, with their number and the number left out.
#[derive(Default)]
struct Timing {
    d: Duration,
    calls: usize,
    slow: usize,
}

impl Timing {
    fn add(&mut self, d: Duration) {
        if d > PREEMPTED {
            self.slow += 1;
        } else {
            self.d += d;
            self.calls += 1;
        }
    }

    /// The nanoseconds per datagram, less the clock's cost in each call.
    fn figure(&self, clock: Duration, datagrams: usize) -> Figure {
        let mut ns = 0.0;
        if datagrams > 0 {
            let work = self.d.as_nanos() as i64 - self.calls as i64 * clock.as_nanos() as i64;
            ns = work as f64 / datagrams as f64;
        }
        let slow = self.slow;
        Figure { ns, slow }
    }
}

/// What timing an empty region reads, the median of 21 means of 100,000: each timed call is that
/// much longer than its work. The Go benchmark measures its own clock the same way.
fn clock() -> Duration {
    let mut means: Vec<Duration> = (0..21)
        .map(|_| {
            let mut d = Duration::ZERO;
            for _ in 0..100_000 {
                let t0 = Instant::now();
                d += t0.elapsed();
            }
            d / 100_000
        })
        .collect();
    means.sort();
    means[means.len() / 2].max(Duration::from_nanos(1))
}

/// A deterministic byte stream for the session keys, as in the Go benchmark.
fn det_rand(seed: u64) -> Rand {
    let mut r = seed;
    Box::new(move |p: &mut [u8]| {
        for b in p {
            r = r.wrapping_mul(6364136223846793005);
            r = r.wrapping_add(1442695040888963407);
            *b = (r >> 56) as u8;
        }
    })
}

const MAC: usize = 0;
const XANTH: usize = 1;

/// One device: its session, its sockets and the control messages on their way to it.
struct Node {
    name: &'static str,
    sess: Session,
    rids: RidTable,
    links: Vec<Link>,
    mesh: MeshLane,
    ctl: VecDeque<Item>,
}

impl Node {
    fn new(name: &'static str, role: Role, seed: u64, links: Vec<Link>, mesh: MeshLane) -> Node {
        let (sess, rids) = (Session::new(role, 1, det_rand(seed)), RidTable::new());
        let ctl = VecDeque::new();
        Node { name, sess, rids, links, mesh, ctl }
    }

    fn addr(&self, sock: SockId) -> Option<SocketAddr> {
        if sock == self.mesh.sock {
            return Some(self.mesh.local);
        }
        self.links.iter().find(|l| l.sock == sock).map(|l| l.local)
    }

    /// Its links as its hello offers them: each with its local address.
    fn infos(&self) -> Vec<LinkInfo> {
        let info = |l: &Link| LinkInfo {
            id: l.id,
            label: l.label.clone(),
            metered: l.metered,
            default: l.default,
            cands: vec![l.local],
        };
        self.links.iter().map(info).collect()
    }
}

/// Link `id` on socket `sock` at `local`, metered and the default route as the flags say.
fn link(id: u8, label: &str, sock: SockId, local: SocketAddr, [metered, default]: [bool; 2]) -> Link {
    let label = label.into();
    Link { id, label, metered, default, sock, local, mapped: None }
}

/// One direction of a link's shaper: a FIFO at `rate` that drops a packet which would wait longer
/// than `buf`.
struct Bottleneck {
    rate: Mono, // bits per second
    buf: Mono,
    busy: Mono, // when the packets queued so far have left
}

/// One direction between two socket addresses: a bottleneck, then a propagation delay.
struct Path {
    from: SocketAddr,
    to: SocketAddr,
    bn: usize,
    delay: Mono,
    sock: SockId, // the socket at `to`
    q: VecDeque<Item>,
}

/// A packet or control message on its way: it arrives at `at`, and `seq` orders arrivals at the
/// same time.
struct Item {
    at: Mono,
    seq: u64,
    what: What,
}

enum What {
    Packet(SockId, SocketAddr, Vec<u8>), // the receiving socket, the source, the bytes
    Control(Msg),
}

/// Where a device's arrivals wait: its control channel, or a path to it.
#[derive(Clone, Copy)]
enum Src {
    Ctl(usize),
    Path(usize),
}

/// A send of the call being handled, copied out of the engine's buffer.
struct Sent {
    sock: SockId,
    to: SocketAddr,
    buf: Vec<u8>,
}

/// The driver's part of the engine's actions, timed with each call: a send is copied out of the
/// engine's buffer, a delivery counted, a control message kept.
#[derive(Default)]
struct Wire {
    size: usize,
    sent: Vec<Sent>, // the sends of the call being handled
    ctl: Vec<Msg>,   // its control messages
    got: usize,      // its deliveries
    bad: usize,      // deliveries of the wrong size, all along
    free: Vec<Vec<u8>>,
}

impl Out for Wire {
    fn send(&mut self, sock: SockId, to: SocketAddr, data: &[u8]) {
        let mut buf = self.free.pop().unwrap_or_else(|| Vec::with_capacity(MAX_PACKET));
        buf.clear();
        buf.extend_from_slice(data);
        self.sent.push(Sent { sock, to, buf });
    }

    fn deliver(&mut self, packet: &[u8]) {
        let (_, data) = outer(packet);
        self.got += 1;
        if data.len() != self.size {
            self.bad += 1;
        }
    }

    fn control(&mut self, msg: Msg) {
        self.ctl.push(msg);
    }
}

/// The window [from, until), whose calls are timed.
#[derive(Default)]
struct Window {
    from: Mono,
    until: Mono,
    send: Timing,
    recv: Timing,
    apps: usize,       // datagrams xanth's App took
    delivered: usize,  // datagrams the Mac delivered
    lanes: [usize; 3], // data packets xanth sent to the hotel lane, the cell lane, elsewhere
}

/// What `World::run` handles next.
#[derive(Clone, Copy)]
enum Next {
    Arrival,
    Tick,
    App,
}

/// The two devices in simulated time, one event at a time.
struct World {
    sc: &'static Scenario,
    now: Mono,
    nodes: [Node; 2],
    bns: Vec<Bottleneck>,
    paths: Vec<Path>,
    inbox: [Vec<Src>; 2], // each device's arrivals: its control channel, then the paths to it
    seq: u64,
    next: usize, // the stream's next datagram
    flow: FlowId,
    dgram: Vec<u8>,
    wire: Wire,
    sends: usize, // packets the engines sent, all along
    win: Window,
}

impl World {
    /// Starts the two sides and runs the stream to the steady state; the window starts at the
    /// next datagram.
    fn warm(sc: &'static Scenario) -> Result<World, String> {
        let mac = Node::new(
            "the Mac",
            Role::Client,
            1,
            vec![link(1, "Wi-Fi", 11, MAC_WIFI, [false, true]), link(2, "iPhone", 12, MAC_PHONE, [true, false])],
            MeshLane { sock: 1, local: MAC_MESH, peer: XANTH_MESH },
        );
        let xanth = Node::new(
            "xanth",
            Role::Anchor,
            2,
            vec![link(1, "Ethernet", 11, XANTH_ETH, [false, true])],
            MeshLane { sock: 1, local: XANTH_MESH, peer: MAC_MESH },
        );
        let port = if sc.burst == 1 { 47999 } else { 47998 };
        let mut w = World {
            sc,
            now: 0,
            nodes: [mac, xanth],
            bns: Vec::new(),
            paths: Vec::new(),
            inbox: [vec![Src::Ctl(MAC)], vec![Src::Ctl(XANTH)]],
            seq: 0,
            next: 0,
            flow: FlowId(port),
            dgram: vec![0; sc.size],
            wire: Wire { size: sc.size, ..Wire::default() },
            sends: 0,
            win: Window::default(),
        };
        let (hotel, cell) = (w.bottlenecks(38_000_000, 180 * MS), w.bottlenecks(80_000_000, 180 * MS));
        w.connect((MAC_WIFI, 11), (XANTH_ETH, 11), 30 * MS, hotel);
        w.connect((MAC_PHONE, 12), (XANTH_ETH, 11), 45 * MS, cell);
        w.connect((MAC_MESH, 1), (XANTH_MESH, 1), 31 * MS, hotel); // Tailscale crosses the Wi-Fi
        w.start();
        w.run(WARM_UP)?;
        w.steady()?;
        (w.win.from, w.win.until) = (sc.at(w.next), Mono::MAX);
        Ok(w)
    }

    /// Runs the world until the stream's next datagram has been handled.
    fn datagram(&mut self) -> Result<(), String> {
        self.run(self.sc.at(self.next) + 1)
    }

    /// Closes the window after its `n` datagrams, checks what it carried and returns the figures.
    fn finish(mut self, n: usize, clock: Duration) -> Result<Run, String> {
        for node in &mut self.nodes {
            node.sess.close(&mut node.rids);
        }
        if n >= MIN_CHECKED {
            self.check(n)?;
        }
        let w = &self.win;
        let (send, recv) = (w.send.figure(clock, w.apps), w.recv.figure(clock, w.delivered));
        Ok(Run { send, recv })
    }

    /// A link's two bottlenecks, up and down.
    fn bottlenecks(&mut self, rate: Mono, buf: Mono) -> [usize; 2] {
        for _ in 0..2 {
            self.bns.push(Bottleneck { rate, buf, busy: 0 });
        }
        [self.bns.len() - 2, self.bns.len() - 1]
    }

    /// Adds both directions between a socket of the Mac and one of xanth, through the bottlenecks
    /// of the Mac's link.
    fn connect(&mut self, mac: (SocketAddr, SockId), xanth: (SocketAddr, SockId), delay: Mono, [up, down]: [usize; 2]) {
        for (from, to, bn, dst) in [(mac, xanth, up, XANTH), (xanth, mac, down, MAC)] {
            let (sock, q) = (to.1, VecDeque::new());
            self.paths.push(Path { from: from.0, to: to.0, bn, delay, sock, q });
            self.inbox[dst].push(Src::Path(self.paths.len() - 1));
        }
    }

    /// Gives both sides their links and the Mac its mode, and sends both hellos over the control
    /// channel, as node does when the Mac connects.
    fn start(&mut self) {
        for i in [MAC, XANTH] {
            let (links, mesh) = (self.nodes[i].links.clone(), Some(self.nodes[i].mesh));
            self.handle(i, Event::Links { links: &links, mesh });
        }
        let mode = self.sc.mode.to_string();
        let set = Msg { t: "mode".into(), mode: mode.clone(), ..Msg::default() };
        self.handle(MAC, Event::Control(&set));
        let hello = |inst, mode, links| Msg { t: "hello".into(), v: PROTOCOL, inst, mode, links, ..Msg::default() };
        let (mac, xanth) = (self.nodes[MAC].infos(), self.nodes[XANTH].infos());
        self.control(XANTH, hello(1, mode, mac));
        self.control(MAC, hello(2, String::new(), xanth));
    }

    /// Sends `msg` to device `i` over the control channel.
    fn control(&mut self, i: usize, msg: Msg) {
        self.seq += 1;
        let (at, seq, what) = (self.now + CTL_DELAY, self.seq, What::Control(msg));
        self.nodes[i].ctl.push_back(Item { at, seq, what });
    }

    fn head(&self, src: Src) -> Option<&Item> {
        match src {
            Src::Ctl(i) => self.nodes[i].ctl.front(),
            Src::Path(p) => self.paths[p].q.front(),
        }
    }

    fn pop(&mut self, src: Src) -> Option<Item> {
        match src {
            Src::Ctl(i) => self.nodes[i].ctl.pop_front(),
            Src::Path(p) => self.paths[p].q.pop_front(),
        }
    }

    /// Handles every event before `end` in time order. At equal times: arrivals, in the order
    /// they were sent, then ticks, then the stream's datagram; the Mac before xanth.
    fn run(&mut self, end: Mono) -> Result<(), String> {
        let mut spins = 0;
        loop {
            let (mut at, mut who, mut next, mut src) = (end, None, Next::Arrival, None);
            for i in [MAC, XANTH] {
                for &s in &self.inbox[i] {
                    let Some(h) = self.head(s) else { continue };
                    let first = src.and_then(|q| self.head(q)).map_or(u64::MAX, |q| q.seq);
                    if h.at < at || h.at == at && who == Some(i) && h.seq < first {
                        (at, who, next, src) = (h.at, Some(i), Next::Arrival, Some(s));
                    }
                }
            }
            for i in [MAC, XANTH] {
                let t = self.nodes[i].sess.next();
                if t < at {
                    (at, who, next) = (t, Some(i), Next::Tick);
                }
            }
            let t = self.sc.at(self.next);
            if t < at {
                (at, who, next) = (t, Some(XANTH), Next::App);
            }
            let Some(i) = who else { return Ok(()) };
            if at > self.now {
                (self.now, spins) = (at, 0);
            } else if spins < 1_000_000 {
                spins += 1;
            } else {
                return Err(format!("{}: no progress at {} ns", self.sc.name, self.now));
            }
            match next {
                Next::Arrival => match src.and_then(|s| self.pop(s)).map(|item| item.what) {
                    Some(What::Control(msg)) => self.handle(i, Event::Control(&msg)),
                    Some(What::Packet(sock, from, buf)) => {
                        let data = &buf;
                        self.handle(i, Event::Packet { sock, from, data });
                        self.wire.free.push(buf);
                    }
                    None => unreachable!("an arrival was chosen from an empty queue"),
                },
                Next::Tick => self.handle(i, Event::Tick),
                Next::App => {
                    let (flow, mut dgram) = (self.flow, std::mem::take(&mut self.dgram));
                    dgram[..4].copy_from_slice(&(self.next as u32).to_be_bytes());
                    self.next += 1;
                    self.handle(i, Event::Send { packet: &inner(flow, &dgram) });
                    self.dgram = dgram;
                }
            }
        }
    }

    /// Runs one event on device `i` now. The call is timed together with the driver's work on its
    /// actions and counted toward its path; then its sends and control messages go to the peer.
    fn handle(&mut self, i: usize, ev: Event<'_>) {
        self.wire.got = 0;
        let node = &mut self.nodes[i];
        let t0 = Instant::now();
        node.sess.handle(self.now, ev, &mut node.rids, &mut self.wire);
        let d = t0.elapsed();
        if (self.win.from..self.win.until).contains(&self.now) {
            self.count(i, ev, d);
        }
        self.route(i);
    }

    /// Adds a call made in the window to the path it belongs to.
    fn count(&mut self, i: usize, ev: Event<'_>, d: Duration) {
        let is_data = |p: &[u8]| p.first().is_some_and(|&t| t & !SMALL_BIT == TYPE_DATA);
        let (w, mut data) = (&mut self.win, 0);
        for x in self.wire.sent.iter().filter(|x| is_data(&x.buf)) {
            data += 1;
            if i == XANTH {
                let lane = match x.to {
                    to if to == MAC_WIFI => 0,
                    to if to == MAC_PHONE => 1,
                    _ => 2,
                };
                w.lanes[lane] += 1;
            }
        }
        match ev {
            Event::Send { .. } if i == XANTH => w.send.add(d),
            Event::Tick if i == XANTH && data > 0 => w.send.add(d),
            Event::Packet { data: p, .. } if i == MAC && is_data(p) => w.recv.add(d),
            Event::Tick if i == MAC && self.wire.got > 0 => w.recv.add(d),
            _ => {}
        }
        if i == XANTH && matches!(ev, Event::Send { .. }) {
            w.apps += 1;
        }
        if i == MAC {
            w.delivered += self.wire.got;
        }
    }

    /// Carries device `i`'s sends over their paths, and its control messages to the peer.
    fn route(&mut self, i: usize) {
        for msg in std::mem::take(&mut self.wire.ctl) {
            self.control(1 - i, msg);
        }
        let mut sent = std::mem::take(&mut self.wire.sent);
        for x in sent.drain(..) {
            self.sends += 1;
            let from = self.nodes[i].addr(x.sock);
            let p = self.paths.iter().position(|p| Some(p.from) == from && p.to == x.to);
            let Some(p) = p else {
                self.wire.free.push(x.buf); // no path: lost
                continue;
            };
            let bn = &mut self.bns[self.paths[p].bn];
            let at = self.now.max(bn.busy);
            if at - self.now > bn.buf {
                self.wire.free.push(x.buf); // the bottleneck's buffer is full: lost
                continue;
            }
            bn.busy = at + (x.buf.len() as Mono + IP_UDP) * 8 * SEC / bn.rate;
            self.seq += 1;
            let path = &mut self.paths[p];
            let (at, seq) = (bn.busy + path.delay, self.seq);
            let what = What::Packet(path.sock, path.from, x.buf);
            path.q.push_back(Item { at, seq, what });
        }
        self.wire.sent = sent;
    }

    /// Checks, when timing starts, that both sides' direct lanes are up and validated and that
    /// xanth's have the roles the scenario measures: with video both lanes spread (every packet in
    /// performance, a share in bandwidth); with a small flow both lanes take copies.
    fn steady(&self) -> Result<(), String> {
        // R2-06 (notes/open.md, notes/rust.md C2): a lane reads spread only while this side sends
        // bulk, so a small flow's lanes read copies (Go's engine, without the fix, read the
        // scheduler's primary or spread there).
        let (want, roles): (&[(u16, &str)], _) = if self.sc.burst == 1 {
            (&[(HOTEL, "copies"), (CELL, "copies")], "xanth's hotel and cell lanes copies")
        } else {
            (&[(HOTEL, "spread"), (CELL, "spread")], "xanth's hotel and cell lanes spread")
        };
        for (i, node) in self.nodes.iter().enumerate() {
            let (mut ok, mut lanes) = (0, Vec::new());
            for l in node.sess.snapshot().lanes {
                lanes.push(format!("{:#06x} {} {}", l.id, l.state, l.out.role));
                let role = want.iter().find(|w| w.0 == l.id).map(|w| w.1);
                let wanted = i == MAC || role.is_none_or(|r| r == l.out.role);
                if l.kind == "direct" && l.state == "up" && l.validated && wanted {
                    ok += 1;
                }
            }
            if ok != 2 {
                let sent = match self.sends {
                    0 => "the engines sent no packet: is the engine implemented?".to_string(),
                    n => format!("the engines sent {n} packets"),
                };
                return Err(format!(
                    "{}: no steady state at {} s: {}'s lanes are [{}]; want both direct lanes up and validated, and {roles} ({sent})",
                    self.sc.name,
                    WARM_UP / SEC,
                    node.name,
                    lanes.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Checks what the window carried: `n` datagrams sent, about as many delivered, all of the
    /// right size, on the lanes the scenario measures.
    fn check(&self, n: usize) -> Result<(), String> {
        let (w, bad) = (&self.win, self.wire.bad);
        let ([h, c, _], data) = (w.lanes, w.lanes.iter().sum::<usize>());
        let (bulk, bandwidth) = (self.sc.burst > 1, self.sc.mode == MODE_BANDWIDTH);
        let why = if w.apps != n {
            format!("{} datagrams sent in the window, want {n}", w.apps)
        } else if w.delivered < n * 9 / 10 || w.delivered > n * 11 / 10 {
            format!("{} datagrams delivered in the window for {n} sent", w.delivered)
        } else if bad > 0 {
            format!("{bad} datagrams delivered with the wrong size")
        } else if !bulk && (h < n * 9 / 10 || c < n * 9 / 10) {
            format!("small datagrams sent {h} times on the hotel lane and {c} on the cell lane, want about {n} on each")
        } else if bulk && !bandwidth && (h < n * 9 / 10 || c < n * 9 / 10) {
            // §14.1: performance sends every packet on every lane of the spread, one per group.
            format!("bulk sent {h} times on the hotel lane and {c} on the cell lane, want about {n} on each")
        } else if bulk && bandwidth && (h < data / 20 || c < data / 20) {
            format!(
                "of {data} data packets {h} went on the hotel lane and {c} on the cell lane; bandwidth spreads them"
            )
        } else {
            return Ok(());
        };
        Err(format!("{}: {why}", self.sc.name))
    }
}

/// A test flow's inner packet (SPEC 10.3, §13): `data` as IPv4/UDP from 10.77.0.2 port `f.0` to
/// 10.77.0.1 port 9. The engine keys flows by the packet and numbers them itself; tests name a
/// flow by its UDP source port, which [`outer`] reads back from a delivered packet.
fn inner(f: FlowId, data: &[u8]) -> Vec<u8> {
    let total = (28 + data.len()) as u16;
    let mut p = vec![0x45, 0];
    p.extend_from_slice(&total.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0, 10, 77, 0, 2, 10, 77, 0, 1]);
    p.extend_from_slice(&f.0.to_be_bytes());
    p.extend_from_slice(&9u16.to_be_bytes());
    p.extend_from_slice(&(total - 20).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(data);
    p
}

/// The test flow (the UDP source port of its flow key) and datagram of a packet [`inner`] built;
/// anything else is flow 0, whole.
fn outer(p: &[u8]) -> (FlowId, &[u8]) {
    match wirebond_core::flow::Key::parse(p) {
        Some(k) if k.proto == 17 && p.len() >= 28 && p[0] == 0x45 => (FlowId(k.sport), &p[28..]),
        _ => (FlowId(0), p),
    }
}
