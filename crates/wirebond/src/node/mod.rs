//! The daemon (SPEC 2.0 §4): one thread, one loop. It waits on the TUN device, the lane sockets,
//! the control sockets and the local API, with a timeout from the engines' next deadline
//! (`Session::next`), then drains each source without blocking (§4.3). Owner: node daemon.
//!
//! Per packet (§4.3): a TUN read goes to the bond's session (`Session::send_packet`); a lane
//! datagram goes to the session its receive id names (`wirebond_core::packet_rid`, `RidTable`);
//! what a session delivers (`Out::deliver`) is checked (§10.5 rule 6) and written to the TUN
//! device.
//!
//! The bond lifecycle is §3.2's: the client dials the anchor's mesh address (`ctl`), both send
//! `hello`, the client sends `bond open`, the anchor answers `bond ok` with the client's bond
//! addresses (`bond::Pool`), each side sends `key` and `links`, punching brings lanes up, and the
//! client installs capture once a lane validates (§5.4). §17's failure rules run in `life`.
//! The host is reached through `plat::Platform`, so tests run on a fake one.
//!
//! Every LINK_SCAN the loop lists the links, follows the default route (the link carrying the
//! mesh, §8, and an anchor's uplink, §6.2) and runs STUN (§9.2) on each link socket, whose answers
//! it sets aside before the engine; links offer their mappings as candidates and are grouped into
//! bottleneck groups (§8). Every second it checks for a wake from sleep (§11.4). On Windows it
//! waits on `host::EventLoop` (§4.3), elsewhere on `host::Poll`.

mod bond;
mod ctl;
mod dns;
mod link;
mod pick;
mod plat;
pub mod proto;
mod support;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_net;

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use wirebond_core::{
    self as core, ALL_LANES_DOWN, FAIL_OPEN_AFTER, IDLE_AFTER, LANES_BLOCKED_AFTER, LINK_SCAN, LOG_LINES, MAX_CANDS,
    MAX_LINKS, MAX_MSG, MESH_ONLY_AFTER, MS, Mode, Mono, PICK_SECONDS, PORT, PROTOCOL, REDIAL_MAX, REDIAL_MIN,
    RidTable, SEC, STATUS_EVENTS, STUN_EVERY, Session, SockId, TUN_MTU,
};

use crate::api::{self, LinkKind, Reply, Request, Role, Status, problem};
use crate::host::{self, Config};
use bond::{Bond, BondState, Egress};
use plat::{Platform, TunDev};
use proto::{ErrorCode, Msg};

/// The longest the loop sleeps when no engine deadline is nearer.
const IDLE_WAIT: Duration = Duration::from_millis(500);
/// How often the bond lifecycle looks at its sessions' lanes.
const LIFE_EVERY: Mono = 20 * MS;
/// How often an anchor resolves the names on its allow-list again.
const RESOLVE_EVERY: Mono = 30 * SEC;
/// ICMPv6 unreachables a client writes a second at most (§5.5).
const ICMP6_RATE: u32 = 100;
/// Packets read from one source per loop turn at most, so no source starves the others.
const BURST: usize = 512;
/// A wait at least this long blocked until something arrived (`Ready::tun_one`); one that found
/// work waiting returns in a few microseconds.
const SLEPT: Duration = Duration::from_micros(10);

/// The daemon. `wirebond run` calls it inside host::run_service; `version` is the build's. It
/// returns when `stop` fires (Windows' service stop; SIGTERM ends the process elsewhere).
pub fn run(version: &str, stop: Receiver<()>) -> io::Result<()> {
    let scope = host::need(false);
    let (_exe, dir) = scope.paths();
    let (cfg, bad) = match Config::load(&dir) {
        Ok(cfg) => (cfg, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let mut d = Daemon::new(version, Some(dir), cfg, Box::new(plat::Real))?;
    // §22.3: the update's trial bookkeeping comes first, and may roll back.
    d.updater.exe = Some(_exe.clone());
    let sdir = d.dir.clone();
    d.updater.boot(&mut d.cfg, sdir.as_deref(), version);
    // §4.3: Windows waits on its sockets' event, Wintun's read event and a waitable timer;
    // without them (an old Windows) it polls the sockets alone.
    #[cfg(windows)]
    match host::EventLoop::new().and_then(|el| el.add_waker(&d.waker).map(|()| el)) {
        Ok(el) => d.evloop = Some((el, Vec::new())),
        Err(e) => d.log(format!("event loop: {e}; polling the sockets alone")),
    }
    if let Some(e) = bad {
        d.cfg_bad = Some(e.to_string());
        d.log(format!("config.json: {e}; starting from the defaults"));
    }
    // §5.4: what a crash left behind goes before anything else.
    if let Err(e) = host::cleanup_stale() {
        d.log(format!("stale state: {e}"));
    }
    let (ctl, status) = host::listen_local(d.dir.as_deref().unwrap_or(std::path::Path::new(".")))?;
    let (tx, rx) = mpsc::channel();
    for ln in [ctl, status] {
        let (tx, waker) = (tx.clone(), d.waker.clone());
        thread::spawn(move || accept_local(ln, tx, waker));
    }
    d.run_loop(&rx, &stop)
}

/// A local API request and where its reply goes.
type Call = (Request, Sender<Reply>);

/// Answers the local API (§20.1) on one listener: one request line, one reply line, the loop
/// computing the reply.
fn accept_local(ln: host::LocalListener, tx: Sender<Call>, waker: Arc<host::Waker>) {
    while let Ok(mut s) = ln.accept() {
        let _ = s.set_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        if BufReader::new(io::Read::take(&mut s, MAX_MSG as u64)).read_line(&mut line).is_err() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let (rtx, rrx) = mpsc::channel();
                if tx.send((req, rtx)).is_err() {
                    return;
                }
                waker.wake();
                rrx.recv_timeout(Duration::from_secs(120)).unwrap_or_else(|_| Reply::fail("failed", "no answer"))
            }
            Err(e) => Reply::fail("bad_request", e.to_string()),
        };
        let mut b = serde_json::to_vec(&reply).unwrap_or_default();
        b.push(b'\n');
        let _ = s.write_all(&b);
    }
}

/// One lane socket: a link's (`id` 1–MAX_LINKS) or the mesh lane's (`id` 0). Its SockId is its
/// index in `Daemon::socks` plus one.
struct Sock {
    id: u8,
    link: host::Link,
    sock: UdpSocket,
    local: SocketAddr,
    /// The link's public mappings (§9.2); unused on the mesh lane's socket.
    stun: link::Stun,
}

/// Why a channel was dialed.
enum Purpose {
    /// The client's bond.
    Bond,
    /// A remote view (`wirebond status <device>`): the caller waiting for it.
    View(Sender<Reply>),
    /// `pick`'s temporary bond with the candidate being measured (§18).
    Pick,
}

/// The bond index of `pick`'s temporary bond, which lives in [`PickRun`], not in `bonds`.
const PICK: usize = usize::MAX;

/// A `pick` in progress (§18): candidates measured one after another, then the selection.
struct PickRun {
    cands: Vec<String>,
    i: usize,
    mode: Mode,
    seconds: u32,
    dry_run: bool,
    reply: Sender<Reply>,
    results: Vec<pick::Measured>,
    dialing: Option<u64>,
    chan: Option<u64>,
    /// The temporary bond: no TUN device, no capture.
    bond: Option<Bond>,
    started: Mono,
    /// The candidate's `measure result`: its lanes and its egress (§18).
    result: Option<(Vec<proto::MeasureLane>, f64)>,
}

/// An open control channel.
struct Peer {
    chan: ctl::Chan,
    hello: Option<proto::Hello>,
    purpose: Option<Purpose>,
}

/// The daemon's state: one value, owned by the loop.
pub struct Daemon {
    version: String,
    /// The state directory; None in tests (config.json is not saved).
    dir: Option<PathBuf>,
    cfg: Config,
    cfg_bad: Option<String>,
    start: Instant,
    plat: Box<dyn Platform>,
    name: String,
    /// Random per process (§16 hello).
    inst: u64,
    /// The control port: PORT.
    port: u16,
    /// Tests: loopback candidates count as usable.
    loopback: bool,
    /// Receive ids across every session (core's RidTable).
    rids: RidTable,
    /// A client's one bond, or an anchor's bonds.
    bonds: Vec<Bond>,
    /// An anchor's bond addresses.
    pool: bond::Pool,
    next_owner: u64,
    /// The newest LOG_LINES lines.
    logs: VecDeque<String>,
    next_scan: Mono,
    next_life: Mono,
    socks: Vec<Sock>,
    /// Link ids by interface name and family, kept while free.
    link_ids: Vec<(String, bool, u8)>,
    listening: Vec<IpAddr>,
    ev_tx: Sender<ctl::Ev>,
    ev_rx: Receiver<ctl::Ev>,
    waker: Arc<host::Waker>,
    chans: HashMap<u64, Peer>,
    /// The client's dial in flight, and when to dial next.
    dialing: Option<u64>,
    redial_at: Mono,
    backoff: Mono,
    /// Why the anchor could not be reached, until it is.
    unreachable: Option<String>,
    tun: Option<Box<dyn TunDev>>,
    /// A client's capture while installed.
    capture: Option<host::Capture>,
    /// A client's resolver at the anchor (`bond ok`'s dns).
    dns: Option<IpAddr>,
    /// An anchor's egress: what was asked and what it achieved.
    egress: Option<(host::Egress, Option<host::EgressOn>)>,
    /// An anchor's DNS relay (§7).
    dns_relay: Option<dns::Relay>,
    /// Allow-list names and the addresses they resolved to, and when.
    resolved: HashMap<String, (Vec<IpAddr>, Mono)>,
    /// A `pick` in progress.
    pick: Option<PickRun>,
    /// Remote views dialed and their callers, until the channel is up.
    view_waiting: HashMap<u64, Sender<Reply>>,
    /// The links a captive portal answered on (§17): they carry no lane.
    captive: Vec<IpAddr>,
    /// Where the captive-portal check goes; None turns it off (tests).
    captive_url: Option<(String, u16, String)>,
    /// §5.5's rate limit: the current second's start and the unreachables sent in it.
    icmp6: (Mono, u32),
    /// Problems the host reported: missing_tool and the like.
    host_problems: Vec<api::Problem>,
    tx_bytes: u64,
    rx_bytes: u64,
    /// When data last crossed the TUN device: STUN refreshes while data flows (§9.2).
    last_data: Mono,
    /// Wake detection (§11.4), every second.
    wake: host::Wake,
    next_wake: Mono,
    /// Each interface's default gateway (§8), listed again when the links change.
    gateways: Vec<(String, IpAddr)>,
    /// The default route's local address, as last seen on a link: the link carrying the mesh
    /// (§8), and an anchor's uplink (§6.2).
    default_addr: Option<IpAddr>,
    /// Why capture was removed: the mesh's own traffic entered the TUN device (§5.3). Capture
    /// stays off until the bond is started again.
    mesh_captured: Option<String>,
    /// The installed capture left DNS alone (§7).
    dns_local: bool,
    /// A client's anchor's public candidate addresses, where the mesh's underlay goes (§5.3).
    peer_cands: Vec<IpAddr>,
    /// Windows: the loop's wait (§4.3) and the sockets associated with it, by handle.
    #[cfg(windows)]
    evloop: Option<(host::EventLoop, Vec<std::os::windows::io::RawSocket>)>,
    /// The support role, and a client's supports (§8).
    support: support::State,
    /// The update's schedule (§22.3).
    updater: crate::update::Schedule,
    /// `anchor_nets`, and the role, egress and bond subnets it was computed from: the address
    /// checks of every delivered packet read it (`refresh_nets`).
    nets: (Vec<IpAddr>, Vec<(IpAddr, u8)>),
    nets_from: Option<NetsFrom>,
    /// The lane pump's receive buffers: BATCH datagrams of DGRAM bytes.
    rx_bufs: Vec<[u8; plat::DGRAM]>,
    /// The lane sends of the current loop turn.
    txq: TxQueue,
    /// When the DNS relay's state is next tidied without a query or answer to read.
    next_dns: Mono,
}

/// What `anchor_nets` reads: the role, the egress's IPv4 subnet and the configured bond subnets.
type NetsFrom = (Role, Option<(Ipv4Addr, u8)>, String, String);

/// What the loop's wait found readable. Without a poll (Windows' event loop) everything is read.
#[derive(Clone, Copy)]
struct Ready {
    waker: bool,
    tun: bool,
    /// The wait slept until the TUN device turned readable: it most likely holds the one packet
    /// that woke the loop, so the pump reads that one and not the read that would find nothing;
    /// the next wait, level-triggered, shows any other. A wait that returned at once found work
    /// waiting, and the pump reads the device until it is empty.
    tun_one: bool,
    /// Bit i: lane socket `socks[i]`; sockets past the 64th are always read.
    socks: u64,
    dns: bool,
}

impl Ready {
    const ALL: Ready = Ready { waker: true, tun: true, tun_one: false, socks: u64::MAX, dns: true };
}

/// An anchor's own bond addresses and the bond subnets.
type Nets<'a> = (&'a [IpAddr], &'a [(IpAddr, u8)]);

/// Lane datagrams queued within one loop turn (§4.3): each socket's go out together at the turn's
/// end, or once TX_BATCH wait, in one sendmmsg on Linux (`plat::send_batch`): a TUN packet is a
/// datagram on each lane of the spread, so a turn's packets are a few datagrams per socket. Outside
/// a turn (local API calls, tests) the engine's sends go at once.
#[derive(Default)]
struct TxQueue {
    open: bool,
    bufs: Vec<[u8; plat::DGRAM]>,
    /// Socket index (into `Daemon::socks`), destination and length of each queued datagram.
    msgs: Vec<(usize, SocketAddr, usize)>,
}

impl TxQueue {
    /// Sends what is queued, each socket's datagrams in their order, and empties the queue;
    /// returns the bytes sent.
    fn flush(&mut self, socks: &[Sock]) -> u64 {
        let (mut bytes, mut done) = (0, [false; plat::TX_BATCH]);
        let none = (SocketAddr::from(([0, 0, 0, 0], 0)), &[][..]);
        let msgs = &self.msgs[..self.msgs.len().min(plat::TX_BATCH)];
        for (i, &(si, ..)) in msgs.iter().enumerate() {
            if done[i] {
                continue;
            }
            let (mut batch, mut n) = ([none; plat::TX_BATCH], 0);
            for (j, &(sj, to, len)) in msgs.iter().enumerate().skip(i) {
                if sj == si {
                    (batch[n], done[j], n) = ((to, &self.bufs[j][..len]), true, n + 1);
                }
            }
            if let Some(s) = socks.get(si) {
                bytes += plat::send_batch(&s.sock, &batch[..n]);
            }
        }
        self.msgs.clear();
        bytes
    }
}

/// The engine's actions for one bond: lane sends, checked deliveries to the TUN device, and its
/// control messages, which the daemon sends after the call.
struct Io<'a> {
    socks: &'a [Sock],
    /// Within a loop turn, lane sends queue here (`TxQueue`).
    txq: &'a mut TxQueue,
    tun: Option<&'a dyn TunDev>,
    bond: &'a mut Bond,
    /// On an anchor: its own bond addresses and the bond subnets.
    anchor: Option<Nets<'a>>,
    ctl: Vec<core::Msg>,
    tx: u64,
    /// Packets written to the TUN device.
    delivered: u64,
}

impl core::Out for Io<'_> {
    fn send(&mut self, sock: SockId, to: SocketAddr, data: &[u8]) {
        let Some(i) = usize::from(sock).checked_sub(1).filter(|&i| i < self.socks.len()) else { return };
        let q = &mut *self.txq;
        if q.open && data.len() <= plat::DGRAM {
            let k = q.msgs.len();
            if q.bufs.len() <= k {
                q.bufs.push([0; plat::DGRAM]);
            }
            q.bufs[k][..data.len()].copy_from_slice(data);
            q.msgs.push((i, to, data.len()));
            if q.msgs.len() >= plat::TX_BATCH {
                self.tx += q.flush(self.socks);
            }
        } else if self.socks[i].sock.send_to(data, to).is_ok() {
            self.tx += data.len() as u64;
        }
    }

    fn deliver(&mut self, p: &[u8]) {
        let ok = match self.anchor {
            None => self.bond.client_accepts(p),
            Some((own, nets)) => match self.bond.anchor_accepts(p, own, nets) {
                Some(true) => {
                    self.bond.local += p.len() as u64;
                    true
                }
                Some(false) => {
                    self.bond.egressed += p.len() as u64;
                    true
                }
                None => false,
            },
        };
        match self.tun {
            Some(t) if ok => {
                let _ = t.write(p);
                self.delivered += 1;
            }
            _ => self.bond.dropped += 1,
        }
    }

    fn control(&mut self, msg: core::Msg) {
        self.ctl.push(msg);
    }
}

/// `net`'s network address at `bits`.
fn network(a: IpAddr, bits: u8) -> IpAddr {
    match a {
        IpAddr::V4(a) => {
            let m = u32::MAX.checked_shl(32 - u32::from(bits.min(32))).unwrap_or(0);
            IpAddr::V4(Ipv4Addr::from_bits(a.to_bits() & m))
        }
        IpAddr::V6(a) => {
            let m = u128::MAX.checked_shl(128 - u32::from(bits.min(128))).unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from_bits(a.to_bits() & m))
        }
    }
}

/// Host number `n` of the subnet at `net`.
fn host_in(net: IpAddr, n: u8) -> IpAddr {
    match net {
        IpAddr::V4(a) => IpAddr::V4(Ipv4Addr::from_bits(a.to_bits() | u32::from(n))),
        IpAddr::V6(a) => IpAddr::V6(Ipv6Addr::from_bits(a.to_bits() | u128::from(n))),
    }
}

/// The semver major of `v` ("2.0.0", "v2.1.3"); None for "dev" and the like.
fn major(v: &str) -> Option<u64> {
    v.trim_start_matches('v').split('.').next()?.parse().ok()
}

/// A link's kind for `links` (§16), from its label.
fn kind(l: &host::Link) -> LinkKind {
    match l.label.as_str() {
        "Wi-Fi" => LinkKind::Wifi,
        "Ethernet" => LinkKind::Ethernet,
        "iPhone" => LinkKind::Usb,
        _ => LinkKind::Other,
    }
}

impl Daemon {
    pub fn new(version: &str, dir: Option<PathBuf>, cfg: Config, plat: Box<dyn Platform>) -> io::Result<Daemon> {
        let (ev_tx, ev_rx) = mpsc::channel();
        let name = plat.hostname();
        Ok(Daemon {
            version: version.into(),
            dir,
            cfg,
            cfg_bad: None,
            start: Instant::now(),
            plat,
            name,
            inst: getrandom::u64().unwrap_or(1),
            port: PORT,
            loopback: false,
            rids: RidTable::new(),
            bonds: Vec::new(),
            pool: bond::Pool::default(),
            next_owner: 1,
            logs: VecDeque::new(),
            next_scan: 0,
            next_life: 0,
            socks: Vec::new(),
            link_ids: Vec::new(),
            listening: Vec::new(),
            ev_tx,
            ev_rx,
            waker: Arc::new(host::Waker::new()?),
            chans: HashMap::new(),
            dialing: None,
            redial_at: 0,
            backoff: REDIAL_MIN,
            unreachable: None,
            tun: None,
            capture: None,
            dns: None,
            egress: None,
            dns_relay: None,
            resolved: HashMap::new(),
            view_waiting: HashMap::new(),
            pick: None,
            captive: Vec::new(),
            captive_url: Some((link::CAPTIVE_URL.0.into(), link::CAPTIVE_URL.1, link::CAPTIVE_URL.2.into())),
            icmp6: (0, 0),
            host_problems: Vec::new(),
            tx_bytes: 0,
            rx_bytes: 0,
            last_data: 0,
            wake: host::Wake::default(),
            next_wake: 0,
            gateways: Vec::new(),
            default_addr: None,
            mesh_captured: None,
            dns_local: false,
            peer_cands: Vec::new(),
            #[cfg(windows)]
            evloop: None,
            support: support::State::default(),
            updater: crate::update::Schedule::new(0, getrandom::u64().unwrap_or(1)),
            nets: (Vec::new(), Vec::new()),
            nets_from: None,
            rx_bufs: vec![[0; plat::DGRAM]; plat::BATCH],
            txq: TxQueue::default(),
            next_dns: 0,
        })
    }

    /// Monotonic now, ns since the daemon started.
    fn now(&self) -> Mono {
        self.start.elapsed().as_nanos() as Mono
    }

    fn log(&mut self, line: String) {
        if self.logs.len() == LOG_LINES {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    /// Records a failed host call: `missing_tool` when a tool is absent (NotFound), and the log.
    fn host_failed(&mut self, what: &str, e: &io::Error) {
        if e.kind() == io::ErrorKind::NotFound && !self.host_problems.iter().any(|p| p.msg == e.to_string()) {
            self.host_problems.push(api::Problem {
                code: problem::MISSING_TOOL.into(),
                msg: e.to_string(),
                ..api::Problem::default()
            });
        }
        self.log(format!("{what}: {e}"));
    }

    /// The loop (§4.3).
    fn run_loop(&mut self, calls: &Receiver<Call>, stop: &Receiver<()>) -> io::Result<()> {
        let mut poll = host::Poll::default();
        loop {
            if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
                return self.shutdown();
            }
            self.step(&mut poll, IDLE_WAIT)?;
            while let Ok((req, reply)) = calls.try_recv() {
                self.call(req, reply);
            }
        }
    }

    /// One turn of the loop: wait at most `max`, then drain every source the wait found readable
    /// and run the timers. A source that is not readable is not read: a read that finds nothing is
    /// a syscall, on every turn, for each of them.
    fn step(&mut self, poll: &mut host::Poll, max: Duration) -> io::Result<()> {
        let wait = self.wait().min(max);
        let mut ready = Ready::ALL;
        if !self.wait_events(wait)? {
            poll.clear();
            poll.add_waker(&self.waker);
            // A TUN device outside the poll set (a test's) is read on every turn.
            let tun = self.tun.as_ref().is_some_and(|t| t.poll_add(poll));
            let first = 1 + usize::from(tun);
            for s in &self.socks {
                poll.add(&s.sock);
            }
            let dns = first + self.socks.len();
            let mut ndns = 0;
            for s in self.dns_relay.iter().flat_map(dns::Relay::sockets) {
                poll.add(s);
                ndns += 1;
            }
            let t0 = Instant::now();
            poll.wait(wait)?;
            ready = Ready {
                waker: poll.ready(0),
                tun: !tun || poll.ready(1),
                tun_one: tun && t0.elapsed() >= SLEPT,
                socks: (0..self.socks.len().min(64)).filter(|&i| poll.ready(first + i)).fold(0, |m, i| m | 1 << i),
                dns: (dns..dns + ndns).any(|i| poll.ready(i)),
            };
        }
        // The waker is drained when it holds a byte; a wake while the loop ran is seen next turn.
        if ready.waker {
            self.waker.reset();
        }
        // The lane sends of this turn go out per socket at its end (`TxQueue`).
        self.txq.open = true;
        if ready.tun {
            self.tun_pump(if ready.tun_one { 1 } else { BURST });
        }
        self.lane_pump(ready.socks);
        let now = self.now();
        if ready.dns || now >= self.next_dns {
            self.next_dns = now + LIFE_EVERY;
            self.dns_pump();
        }
        while let Ok(ev) = self.ev_rx.try_recv() {
            self.event(ev);
        }
        self.tick();
        self.flush_tx();
        self.txq.open = false;
        Ok(())
    }

    /// Sends the lane datagrams this turn queued (`TxQueue`).
    fn flush_tx(&mut self) {
        self.tx_bytes += self.txq.flush(&self.socks);
    }

    /// Windows' wait (§4.3): every lane and DNS socket associated with the loop's event (again
    /// whenever the set changes), then WaitForMultipleObjects on that event, Wintun's read event
    /// and the waitable timer, so a packet on the TUN device is handled at once. False without the
    /// event loop: the caller polls the sockets.
    #[cfg(windows)]
    fn wait_events(&mut self, t: Duration) -> io::Result<bool> {
        use std::os::windows::io::AsRawSocket;
        let Some((el, assoc)) = &mut self.evloop else { return Ok(false) };
        let dns = self.dns_relay.iter().flat_map(dns::Relay::sockets);
        let socks: Vec<&UdpSocket> = self.socks.iter().map(|s| &s.sock).chain(dns).collect();
        let now: Vec<_> = socks.iter().map(|s| s.as_raw_socket()).collect();
        if now != *assoc {
            // A closed socket's association ends with it; a handle may come back for a new one.
            // A socket that cannot be associated is read on the next turn the timer ends.
            for s in &socks {
                let _ = el.add(*s);
            }
            *assoc = now;
        }
        el.wait(self.tun.as_ref().and_then(|t| t.read_event()), t)?;
        Ok(true)
    }

    /// Unix polls (§4.3): the caller's `Poll`.
    #[cfg(not(windows))]
    fn wait_events(&mut self, _: Duration) -> io::Result<bool> {
        Ok(false)
    }

    /// How long the loop may sleep: until the earliest engine deadline, the next link scan, or
    /// IDLE_WAIT.
    fn wait(&self) -> Duration {
        let now = self.now();
        let next = self
            .bonds
            .iter()
            .chain(self.pick.as_ref().and_then(|p| p.bond.as_ref()))
            .filter_map(|b| b.session.as_ref())
            .map(|s| s.next())
            .fold(self.next_scan.min(self.next_life), Mono::min);
        Duration::from_nanos(next.saturating_sub(now).max(0) as u64).min(IDLE_WAIT)
    }

    /// An anchor's own bond addresses and the bond subnets; empty on a client.
    fn anchor_nets(&self) -> (Vec<IpAddr>, Vec<(IpAddr, u8)>) {
        if self.cfg.role != Role::Anchor {
            return (Vec::new(), Vec::new());
        }
        let mut nets: Vec<(IpAddr, u8)> = Vec::new();
        if let Some((_, Some(on))) = &self.egress {
            nets.push((network(IpAddr::V4(on.addr4.0), on.addr4.1), on.addr4.1));
        } else if let Some((a, b)) = self.cfg.bond_nets().0 {
            nets.push((network(a, b), b));
        }
        if let Some((a, b)) = self.cfg.bond_nets().1 {
            nets.push((network(a, b), b));
        }
        (nets.iter().map(|&(n, _)| host_in(n, 1)).collect(), nets)
    }

    /// `anchor_nets` again when the role, the egress or the bond subnets changed.
    fn refresh_nets(&mut self) {
        let egress4 = self.egress.as_ref().and_then(|(_, on)| on.as_ref()).map(|on| on.addr4);
        let cfg = &self.cfg;
        let same = self.nets_from.as_ref().is_some_and(|(role, e4, n4, n6)| {
            (*role, *e4) == (cfg.role, egress4) && *n4 == cfg.bond_net4 && *n6 == cfg.bond_net6
        });
        if !same {
            self.nets = self.anchor_nets();
            let cfg = &self.cfg;
            self.nets_from = Some((cfg.role, egress4, cfg.bond_net4.clone(), cfg.bond_net6.clone()));
        }
    }

    /// Hands `ev` to bond `bi`'s session and sends what it asks over the control channel.
    fn handle_bond(&mut self, bi: usize, ev: core::Event<'_>) {
        let now = self.now();
        let anchor = self.cfg.role == Role::Anchor;
        if anchor {
            self.refresh_nets();
        }
        let b = if bi == PICK { self.pick.as_mut().and_then(|p| p.bond.as_mut()) } else { self.bonds.get_mut(bi) };
        let Some(b) = b else { return };
        // §10.5 rule 6 checks deliveries on a stand-in with the bond's addresses and counters, so
        // the engine works on the session in place: taking it out of the bond and putting it back
        // copied all of it twice a packet.
        let mut check = Bond::new(String::new(), b.peer_addr, b.inst, b.mode, now);
        (check.addr4, check.addr6) = (b.addr4, b.addr6);
        let Some(s) = b.session.as_mut() else { return };
        let mut io = Io {
            socks: &self.socks,
            txq: &mut self.txq,
            tun: self.tun.as_deref(),
            bond: &mut check,
            anchor: anchor.then_some((&self.nets.0[..], &self.nets.1[..])),
            ctl: Vec::new(),
            tx: 0,
            delivered: 0,
        };
        s.handle(now, ev, &mut self.rids, &mut io);
        let (ctl, tx, delivered, chan) = (io.ctl, io.tx, io.delivered, b.chan);
        b.local += check.local;
        b.egressed += check.egressed;
        b.dropped += check.dropped;
        self.tx_bytes += tx;
        if delivered > 0 {
            self.last_data = now;
        }
        for m in ctl {
            if m.t == "key" {
                let k = proto::Key { rid: m.rid, key: m.k, epoch: m.e, floor: m.c };
                if let Some(p) = chan.and_then(|id| self.chans.get(&id)) {
                    p.chan.send(&Msg::Key(k));
                }
            }
        }
    }

    /// The bond whose control channel is `id`: an index into `bonds`, or [`PICK`].
    fn bond_on(&self, id: u64) -> Option<usize> {
        let pick = self.pick.as_ref().and_then(|p| p.bond.as_ref()).filter(|b| b.chan == Some(id)).map(|_| PICK);
        self.bonds.iter().position(|b| b.chan == Some(id)).or(pick)
    }

    /// Bond `bi`, or `pick`'s temporary bond for [`PICK`].
    fn bond_ref(&self, bi: usize) -> Option<&Bond> {
        if bi == PICK { self.pick.as_ref().and_then(|p| p.bond.as_ref()) } else { self.bonds.get(bi) }
    }

    /// Sends `m` on bond `bi`'s control channel, if it is up.
    fn send_bond(&self, bi: usize, m: &Msg) {
        if let Some(p) = self.bond_ref(bi).and_then(|b| b.chan).and_then(|id| self.chans.get(&id)) {
            p.chan.send(m);
        }
    }

    fn send_chan(&self, id: u64, m: &Msg) {
        if let Some(p) = self.chans.get(&id) {
            p.chan.send(m);
        }
    }

    /// Answers on channel `id` with `error` and closes it (§16).
    fn refuse(&mut self, id: u64, code: ErrorCode, msg: &str) {
        self.send_chan(id, &Msg::Error(proto::Error { code, msg: msg.into() }));
        if let Some(p) = self.chans.get(&id) {
            p.chan.close();
        }
    }

    /// Reads the packets waiting on the TUN device, `max` at most, and hands each to its bond's
    /// session (§4.3): the client's one bond, or on an anchor the bond whose client address is the
    /// destination.
    fn tun_pump(&mut self, max: usize) {
        let mut buf = [0u8; 2048];
        for _ in 0..max {
            let Some(t) = &self.tun else { return };
            let n = match t.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return,
            };
            let p = &buf[..n];
            self.last_data = self.now();
            // §5.3: the mesh's own traffic in the client's TUN device ends capture.
            if let Some(why) = self.mesh_traffic(p) {
                self.mesh_captured(why);
                continue;
            }
            let bi = if self.cfg.role == Role::Anchor {
                let dst = bond::addrs(p).map(|(_, d)| d);
                self.bonds.iter().position(|b| dst.is_some_and(|d| b.is_addr(d)))
            } else {
                (!self.bonds.is_empty()).then_some(0)
            };
            // §5.5: without IPv6 egress at the anchor, IPv6 for the internet is answered
            // unreachable here, at most ICMP6_RATE a second, and never sent.
            let no_v6 = self.cfg.role == Role::Client && self.bonds.first().is_some_and(|b| b.egress == Egress::V4);
            if no_v6
                && let Some((_, IpAddr::V6(dst))) = bond::addrs(p)
                && !self.bonds[0].addr6.is_some_and(|a| a.segments()[..4] == dst.segments()[..4])
            {
                let now = self.now();
                if now - self.icmp6.0 >= SEC {
                    self.icmp6 = (now, 0);
                }
                if self.icmp6.1 < ICMP6_RATE
                    && let Some(r) = bond::icmp6_unreachable(p)
                    && let Some(t) = &self.tun
                {
                    self.icmp6.1 += 1;
                    let _ = t.write(&r);
                }
                continue;
            }
            if let Some(bi) = bi {
                self.handle_bond(bi, core::Event::Send { packet: p });
            }
        }
    }

    /// Reads every datagram waiting on the lane sockets `ready` names (bit i: `socks[i]`), BATCH
    /// to a syscall where the OS has one, and hands each to the session its rid names.
    fn lane_pump(&mut self, ready: u64) {
        let mut bufs = std::mem::take(&mut self.rx_bufs);
        let mut got = [(0, SocketAddr::from(([0, 0, 0, 0], 0))); plat::BATCH];
        for si in 0..self.socks.len() {
            if si < 64 && ready & 1 << si == 0 {
                continue;
            }
            let mut left = BURST;
            while left > 0 {
                let Some(s) = self.socks.get(si) else { break };
                let Ok(n) = plat::recv_batch(&s.sock, &mut bufs, &mut got) else { break };
                for (i, &(len, from)) in got.iter().enumerate().take(n) {
                    self.lane_datagram(si, from, &bufs[i][..len]);
                }
                if n < plat::BATCH {
                    break; // none waited after the last
                }
                left = left.saturating_sub(n);
            }
        }
        self.rx_bufs = bufs;
    }

    /// One datagram on lane socket `si`: a STUN answer, or the session its rid names.
    fn lane_datagram(&mut self, si: usize, from: SocketAddr, data: &[u8]) {
        self.rx_bytes += data.len() as u64;
        let from = link::unmap(from);
        // §9.2: a STUN server's answer to this link's request, set aside before the engine.
        if self.stun_answer(si, from, data) {
            return;
        }
        let Some(owner) = core::packet_rid(data).and_then(|rid| self.rids.lookup(rid)) else { return };
        let pick = self.pick.as_ref().and_then(|p| p.bond.as_ref()).filter(|b| b.owner == owner).map(|_| PICK);
        let Some(bi) = self.bonds.iter().position(|b| b.owner == owner).or(pick) else { return };
        self.handle_bond(bi, core::Event::Packet { sock: (si + 1) as SockId, from, data });
    }

    /// A datagram on lane socket `si` from `from`: true when it answers one of that link's STUN
    /// requests (§9.2). A changed mapping is a candidate change (§9.3, §9.4).
    fn stun_answer(&mut self, si: usize, from: SocketAddr, data: &[u8]) -> bool {
        if data.get(4..8) != Some(&link::STUN_COOKIE.to_be_bytes()[..]) {
            return false; // a lane packet: the common case, decided without the clock
        }
        let now = self.now();
        let Some(s) = self.socks.get_mut(si).filter(|s| s.id != 0) else { return false };
        let Some(changed) = s.stun.answer(now, from, data) else { return false };
        if changed {
            let line = format!("{} {}: public mapping {:?}", s.link.name, s.link.addr, s.stun.mappings());
            self.log(line);
            self.links_changed();
        }
        true
    }

    /// The configured STUN servers' addresses (§9.2): literal ones, and names resolved off the
    /// loop, again every STUN_EVERY (every 10 s while a name resolves to nothing).
    fn stun_servers(&mut self, now: Mono) -> Vec<SocketAddr> {
        let mut out = Vec::new();
        for e in &self.cfg.stun {
            if let Ok(a) = e.parse::<SocketAddr>() {
                out.push(link::unmap(a));
                continue;
            }
            let Some((name, port)) = e.rsplit_once(':') else { continue };
            let Ok(port) = port.parse::<u16>() else { continue };
            let old = self.resolved.get(name);
            out.extend(old.iter().flat_map(|(a, _)| a.iter().map(|&ip| SocketAddr::new(ip.to_canonical(), port))));
            let again = old.is_none_or(|(a, at)| now - at >= if a.is_empty() { 10 * SEC } else { STUN_EVERY });
            if again {
                let keep = old.map(|(a, _)| a.clone()).unwrap_or_default();
                self.resolved.insert(name.to_string(), (keep, now));
                ctl::resolve(name.to_string(), self.ev_tx.clone(), self.waker.clone());
            }
        }
        out
    }

    /// STUN (§9.2): a round on each link whose round is due (a new link, a wake, every
    /// STUN_EVERY while data flows, or a retry), to the configured servers of its family.
    fn stun_tick(&mut self, now: Mono) {
        let servers = self.stun_servers(now);
        let flowing = self.last_data > 0 && now - self.last_data < IDLE_AFTER;
        for s in self.socks.iter_mut().filter(|s| s.id != 0) {
            let to: Vec<SocketAddr> = servers.iter().copied().filter(|a| a.is_ipv4() == s.local.is_ipv4()).collect();
            if to.is_empty() || !s.stun.due(now, flowing) {
                continue;
            }
            let rand = || {
                let mut t = [0u8; 12];
                let _ = getrandom::fill(&mut t);
                t
            };
            for (to, req) in s.stun.round(now, &to, rand) {
                let _ = s.sock.send_to(&req, to);
            }
        }
    }

    /// Bottleneck groups (§8), one per socket in `socks`' order: links sharing an interface, a
    /// public IPv4 from STUN or a default gateway share a group, numbered by its lowest link id;
    /// the mesh lane's socket gets the group of the link carrying the mesh, or 0.
    fn groups(&self) -> Vec<u8> {
        let keys: Vec<link::GroupKey<'_>> = self
            .socks
            .iter()
            .filter(|s| s.id != 0)
            .map(|s| link::GroupKey {
                id: s.id,
                name: &s.link.name,
                public4: s.stun.public4(),
                gateway: self
                    .gateways
                    .iter()
                    .find(|(n, _)| *n == s.link.name || n.parse() == Ok(s.link.addr))
                    .map(|&(_, g)| g),
            })
            .collect();
        let mut g = link::groups(&keys).into_iter();
        let mut out: Vec<u8> =
            self.socks.iter().map(|s| if s.id == 0 { 0 } else { g.next().unwrap_or(s.id) }).collect();
        let carrier = self.mesh_carrier().and_then(|i| out.get(i).copied()).unwrap_or(0);
        for (o, s) in out.iter_mut().zip(&self.socks) {
            if s.id == 0 {
                *o = carrier;
            }
        }
        out
    }

    /// The link socket carrying the mesh's underlay (§8): the default route's link.
    fn mesh_carrier(&self) -> Option<usize> {
        let a = self.default_addr?;
        let name = &self.socks.iter().find(|s| s.id != 0 && s.link.addr == a)?.link.name;
        self.socks.iter().position(|s| s.id != 0 && s.link.name == *name && s.link.addr.is_ipv4() == a.is_ipv4())
    }

    /// The default route (§6.2, §8), re-checked every LINK_SCAN while it is on one of my links:
    /// the link carrying the mesh, whose group the mesh lane takes, and an anchor's uplink, whose
    /// egress is enabled again when it moves (macOS's pf rules and Windows' ICS name it; Linux's
    /// rules name no interface, so enabling them again changes nothing there).
    fn follow_default(&mut self) {
        let Some(a) = self.plat.default_addr() else { return };
        let Some(name) = self.socks.iter().find(|s| s.id != 0 && s.link.addr == a).map(|s| s.link.name.clone()) else {
            return; // a mesh exit node's, or the client's own capture's: the last one stays
        };
        let was = self.default_addr.replace(a);
        if was == Some(a) {
            return;
        }
        if let Some(was) = was {
            self.log(format!("default route moved from {was} to {name} {a}"));
        }
        if was.is_some()
            && self.cfg.role == Role::Anchor
            && let Some((e, _)) = self.egress.take()
        {
            let on = match self.plat.egress_enable(&e) {
                Ok(on) => Some(on),
                Err(err) => {
                    self.host_failed("egress", &err);
                    None
                }
            };
            self.log(format!("uplink moved: egress enabled again, {:?}", on.as_ref().map(|o| &o.uplink)));
            self.egress = Some((e, on));
        }
        self.links_changed();
    }

    /// A wake from sleep (§11.4, §17): every session re-punches with fresh estimates (keys kept),
    /// every link asks STUN again, and the links and gateways are listed at once.
    fn woke(&mut self, now: Mono) {
        self.log("woke from sleep: re-punching".into());
        for s in &mut self.socks {
            s.stun.again();
        }
        self.gateways = self.plat.gateways();
        self.next_scan = now;
        for bi in 0..self.bonds.len() {
            if self.bonds[bi].session.is_some() {
                self.handle_bond(bi, core::Event::Wake);
            }
        }
        if self.pick.as_ref().and_then(|p| p.bond.as_ref()).is_some_and(|b| b.session.is_some()) {
            self.handle_bond(PICK, core::Event::Wake);
        }
    }

    /// Whether `p`, read from a client's TUN device while capture is installed and the mesh is up,
    /// is the mesh's own traffic (§5.3): for one of the mesh's ranges, or UDP to one of the
    /// anchor's public candidate addresses, where the mesh's underlay goes. Why, if so.
    fn mesh_traffic(&self, p: &[u8]) -> Option<String> {
        let c = self.capture.as_ref()?;
        self.mesh_addr()?;
        let k = core::flow::Key::parse(p)?;
        if host::in_ranges(k.dst, &c.exclude) {
            return Some(format!("protocol {} to the mesh's {}", k.proto, k.dst));
        }
        (k.proto == 17 && self.peer_cands.contains(&k.dst))
            .then(|| format!("UDP to the anchor's underlay {}", SocketAddr::new(k.dst, k.dport)))
    }

    /// The mesh's own traffic entered the TUN device (§5.3): capture goes, and stays off for this
    /// bond; status shows `mesh_captured`.
    fn mesh_captured(&mut self, why: String) {
        self.log(format!("the mesh's own traffic entered the TUN device ({why}): capture removed"));
        self.mesh_captured = Some(why);
        self.capture_off();
    }

    /// Relays the DNS queries of live bonds' client addresses (§7).
    fn dns_pump(&mut self) {
        let now = self.now();
        let live: Vec<IpAddr> =
            self.bonds.iter().flat_map(|b| [b.addr4.map(IpAddr::V4), b.addr6.map(IpAddr::V6)]).flatten().collect();
        if let Some(r) = &mut self.dns_relay {
            r.pump(now, &live);
        }
    }

    /// One event from the channel threads.
    fn event(&mut self, ev: ctl::Ev) {
        match ev {
            ctl::Ev::Up { chan, dialed } => {
                let id = chan.id;
                let purpose = if !dialed {
                    None
                } else if self.dialing == Some(id) {
                    self.dialing = None;
                    Some(Purpose::Bond)
                } else if let Some(p) = self.pick.as_mut().filter(|p| p.dialing == Some(id)) {
                    (p.dialing, p.chan) = (None, Some(id));
                    Some(Purpose::Pick)
                } else {
                    self.view_waiting.remove(&id).map(Purpose::View)
                };
                let hello = proto::Hello {
                    v: PROTOCOL,
                    inst: self.inst,
                    name: self.name.clone(),
                    role: self.cfg.role,
                    version: self.version.clone(),
                };
                // The bond's and pick's channels only: views from any mesh peer would crowd the
                // events (an anchor logs each client's `bond open` as it comes).
                let why = match purpose {
                    Some(Purpose::Bond) => Some("the bond"),
                    Some(Purpose::Pick) => Some("pick"),
                    _ => None,
                };
                if let Some(why) = why {
                    self.log(format!("channel {id} up: {} dialed for {why}", chan.peer));
                }
                let p = Peer { chan, hello: None, purpose };
                p.chan.send(&Msg::Hello(hello));
                self.chans.insert(id, p);
            }
            ctl::Ev::DialFailed { id, err } => {
                if self.dialing == Some(id) {
                    self.dialing = None;
                    self.dial_failed(err);
                } else if let Some(r) = self.view_waiting.remove(&id) {
                    let _ = r.send(Reply::fail("failed", err));
                } else if self.pick.as_ref().is_some_and(|p| p.dialing == Some(id)) {
                    self.pick_fail(err);
                }
            }
            ctl::Ev::Line { id, msg } => match msg {
                Ok(m) => self.message(id, m),
                Err(code) => {
                    let msg = if code == ErrorCode::Unknown { "unknown message type" } else { "bad request" };
                    self.send_chan(id, &Msg::Error(proto::Error { code, msg: msg.into() }));
                }
            },
            ctl::Ev::Down { id } => self.chan_down(id),
            ctl::Ev::Egress { id, ms } => self.measure_egress(id, ms),
            ctl::Ev::Captive { addr, captive } => {
                let was = self.captive.contains(&addr);
                if captive == Some(true) && !was {
                    self.captive.push(addr);
                    self.log(format!("captive portal on {addr}: the link carries no lane"));
                } else if captive == Some(false) && was {
                    self.captive.retain(|a| *a != addr);
                } else {
                    return;
                }
                self.links_changed();
            }
            ctl::Ev::Resolved { name, addrs } => {
                let now = self.now();
                self.resolved.insert(name, (addrs, now));
            }
        }
    }

    /// The client's dial failed: anchor_unreachable, and a redial after the backoff (§17).
    fn dial_failed(&mut self, err: String) {
        let now = self.now();
        if self.unreachable.as_deref() != Some(err.as_str()) {
            self.log(format!("anchor unreachable: {err}"));
        }
        self.unreachable = Some(err);
        self.redial_at = now + self.backoff;
        self.backoff = (self.backoff * 2).min(REDIAL_MAX);
    }

    fn chan_down(&mut self, id: u64) {
        let now = self.now();
        let Some(p) = self.chans.remove(&id) else { return };
        if let Some(Purpose::View(r)) = &p.purpose {
            let _ = r.send(Reply::fail("failed", "the peer closed the channel"));
        }
        if matches!(p.purpose, Some(Purpose::Pick)) && self.pick.as_ref().is_some_and(|r| r.chan == Some(id)) {
            return self.pick_fail("the channel closed".into());
        }
        let mut client = false;
        for b in self.bonds.iter_mut().filter(|b| b.chan == Some(id)) {
            (b.chan, b.chan_down) = (None, Some(now));
            client = self.cfg.role == Role::Client;
        }
        if client || matches!(p.purpose, Some(Purpose::Bond)) {
            if self.bonds.is_empty() {
                // Refused, or closed before `bond ok`: the anchor counts as unreachable (§17).
                let err = self.unreachable.clone().unwrap_or_else(|| "the channel closed".into());
                self.dial_failed(err);
            } else {
                self.redial_at = now + self.backoff;
            }
        }
    }

    /// One control message on channel `id` (§16).
    fn message(&mut self, id: u64, m: Msg) {
        let Some(p) = self.chans.get(&id) else { return };
        if p.hello.is_none() && !matches!(m, Msg::Hello(_) | Msg::Error(_)) {
            return self.refuse(id, ErrorCode::BadRequest, "hello first");
        }
        match m {
            Msg::Hello(h) => self.hello(id, h),
            Msg::Error(e) => {
                let peer = p.chan.peer;
                if let Some(Peer { purpose: Some(Purpose::View(r)), .. }) = self.chans.get_mut(&id) {
                    let _ = r.send(Reply::fail(&format!("{:?}", e.code).to_lowercase(), e.msg.clone()));
                }
                self.log(format!("{peer}: error {:?}: {}", e.code, e.msg));
                if matches!(self.chans.get(&id).and_then(|p| p.purpose.as_ref()), Some(Purpose::Bond)) {
                    self.unreachable = Some(format!("refused: {:?} {}", e.code, e.msg));
                }
                if self.pick.as_ref().is_some_and(|p| p.chan == Some(id)) {
                    self.pick_fail(format!("refused: {:?}", e.code).to_lowercase());
                }
                if let Some(p) = self.chans.get(&id) {
                    p.chan.close();
                }
            }
            Msg::Bond(proto::Bond::Open { mode, measure: false }) => self.bond_open(id, mode),
            Msg::Bond(proto::Bond::Open { mode, measure: true }) => self.measure_open(id, mode),
            Msg::Bond(proto::Bond::Ok { addr4, addr6, mtu: _, dns, egress }) => {
                self.bond_ok(id, addr4, addr6, dns, egress)
            }
            Msg::Bond(proto::Bond::Close) => {
                if let Some(bi) = self.bonds.iter().position(|b| b.chan == Some(id)) {
                    self.log(format!("{} closed the bond", self.bonds[bi].peer));
                    self.end_bond(bi, false);
                }
            }
            Msg::Key(k) => {
                if let Some(bi) = self.bond_on(id) {
                    let m = core::Msg {
                        t: "key".into(),
                        rid: k.rid,
                        e: k.epoch,
                        k: k.key,
                        c: k.floor,
                        ..core::Msg::default()
                    };
                    self.handle_bond(bi, core::Event::Control(&m));
                }
            }
            Msg::Links(l) => {
                if let Some(bi) = self.bond_on(id) {
                    if self.cfg.role == Role::Client && bi == 0 {
                        // Where the mesh's underlay to the anchor goes (§5.3): its public candidates.
                        let public = |a: &IpAddr| match a {
                            IpAddr::V4(v) => !v.is_private() && !host::in_ranges(*a, &self.cfg.mesh_ranges()),
                            IpAddr::V6(v) => !v.is_unique_local(),
                        };
                        let cands = l.links.iter().flat_map(|o| o.cands.iter().map(|c| c.ip().to_canonical()));
                        self.peer_cands = cands.filter(|a| host::global(*a) && public(a)).collect();
                    }
                    let m = self.peer_links(&l);
                    if let Some(b) = self.bonds.get_mut(bi) {
                        b.offer = l.links;
                    }
                    self.handle_bond(bi, core::Event::Control(&m));
                }
            }
            Msg::Mode { mode } => {
                if self.cfg.role == Role::Anchor
                    && let Some(bi) = self.bonds.iter().position(|b| b.chan == Some(id))
                {
                    self.bonds[bi].mode = mode;
                    let m = core::Msg { t: "mode".into(), mode: mode.as_str().into(), ..core::Msg::default() };
                    self.handle_bond(bi, core::Event::Control(&m));
                }
            }
            Msg::Support(s) => self.support_registered(id, s),
            Msg::Measure(proto::Measure::Start { seconds, bulk_bytes }) => self.measure_start(id, seconds, bulk_bytes),
            Msg::Measure(proto::Measure::Result { lanes, egress_ms }) => {
                if self.pick.as_ref().is_some_and(|p| p.chan == Some(id)) {
                    self.pick_done(lanes, egress_ms);
                }
            }
            Msg::View(v) => match v.status {
                None => {
                    let st = self.status(true);
                    self.send_chan(id, &Msg::View(proto::View { status: Some(Box::new(st)) }));
                }
                Some(st) => {
                    if let Some(Peer { purpose: Some(Purpose::View(r)), chan, .. }) = self.chans.get(&id) {
                        let _ = r.send(Reply { ok: true, status: Some(st), ..Reply::default() });
                        chan.close();
                    }
                }
            },
        }
    }

    /// A peer's hello: the version check, §10.5 rule 7, then what this channel was dialed for.
    fn hello(&mut self, id: u64, h: proto::Hello) {
        if h.v != PROTOCOL {
            return self.refuse(id, ErrorCode::Version, &format!("protocol {PROTOCOL}, not {}", h.v));
        }
        let Some(p) = self.chans.get_mut(&id) else { return };
        let peer = p.chan.peer.ip().to_canonical();
        p.hello = Some(h.clone());
        // The bond's and pick's channels only: views from any mesh peer would crowd the events.
        if matches!(p.purpose, Some(Purpose::Bond | Purpose::Pick)) {
            let name = link::clean(&h.name, link::LABEL_MAX);
            self.log(format!("channel {id}: hello from {name} ({:?}, {})", h.role, h.version));
        }
        let now = self.now();
        self.updater.peer(now, &self.version, &h.version);
        // §10.5 rule 7: a new inst from a mesh address ends that address's older bond at once.
        if self.cfg.role == Role::Anchor
            && let Some(bi) = self.bonds.iter().position(|b| b.peer_addr == peer && b.inst != h.inst)
        {
            self.log(format!("{} restarted: its bond ends", self.bonds[bi].peer));
            self.end_bond(bi, false);
        }
        match self.chans.get(&id).and_then(|p| p.purpose.as_ref()) {
            Some(Purpose::Bond) => {
                if h.role != Role::Anchor {
                    let line = format!("{}: not an anchor ({:?})", h.name, h.role);
                    self.unreachable = Some(line.clone());
                    self.log(line);
                }
                self.send_chan(id, &Msg::Bond(proto::Bond::Open { mode: self.cfg.mode, measure: false }));
                self.log(format!("channel {id}: bond open sent, {}", self.cfg.mode.as_str()));
            }
            Some(Purpose::View(_)) => self.send_chan(id, &Msg::View(proto::View::default())),
            Some(Purpose::Pick) => {
                if h.role != Role::Anchor {
                    return self.pick_fail(format!("not an anchor ({:?})", h.role).to_lowercase());
                }
                let mode = self.pick.as_ref().map_or(self.cfg.mode, |p| p.mode);
                self.send_chan(id, &Msg::Bond(proto::Bond::Open { mode, measure: true }));
            }
            None => self.support_hello(id),
        }
    }

    /// The allow-list entry that names the device at mesh address `peer` (§10.1), if any.
    fn allowed(&self, peer: IpAddr) -> Option<String> {
        self.cfg.allow.iter().find_map(|e| {
            let hit = match e.parse::<IpAddr>() {
                Ok(a) => a.to_canonical() == peer,
                Err(_) => self.resolved.get(e).is_some_and(|(a, _)| a.iter().any(|a| a.to_canonical() == peer)),
            };
            hit.then(|| e.clone())
        })
    }

    /// An anchor's answer to `bond open` (§3.2 steps 2–4).
    fn bond_open(&mut self, id: u64, mode: Mode) {
        let now = self.now();
        let Some(p) = self.chans.get(&id) else { return };
        let Some(h) = p.hello.clone() else { return };
        let peer = p.chan.peer.ip().to_canonical();
        if self.cfg.role != Role::Anchor {
            return self.refuse(id, ErrorCode::NotAllowed, "not an anchor");
        }
        let Some(device) = self.allowed(peer) else {
            self.log(format!("{} ({peer}) is not on the allow-list", link::clean(&h.name, link::LABEL_MAX)));
            return self.refuse(id, ErrorCode::NotAllowed, "not on the allow-list");
        };
        let existing = self.bonds.iter().position(|b| b.peer_addr == peer && !b.measure);
        if existing.is_none() && !bond::room(self.bonds.iter().filter(|b| !b.measure).count()) {
            return self.refuse(id, ErrorCode::TooManyBonds, "MAX_BONDS bonds already");
        }
        let Some(n) = self.pool.assign(&device) else {
            return self.refuse(id, ErrorCode::TooManyBonds, "no bond address left");
        };
        let (own, nets) = self.anchor_nets();
        let addr = |v6: bool| nets.iter().find(|(a, _)| a.is_ipv6() == v6).map(|&(a, _)| host_in(a, n));
        let (addr4, addr6) = (
            addr(false).and_then(|a| if let IpAddr::V4(a) = a { Some(a) } else { None }),
            addr(true).and_then(|a| if let IpAddr::V6(a) = a { Some(a) } else { None }),
        );
        let egress = match &self.egress {
            Some((_, Some(on))) if on.v4 && on.v6 => Egress::Both,
            Some((_, Some(on))) if on.v4 => Egress::V4,
            Some((_, Some(on))) if on.v6 => Egress::V6,
            _ => Egress::None,
        };
        let bi = match existing {
            Some(i) if self.bonds[i].inst == h.inst => i,
            other => {
                if let Some(i) = other {
                    self.end_bond(i, false);
                }
                let name = link::clean(&h.name, link::LABEL_MAX);
                self.bonds.push(Bond::new(name, peer, h.inst, mode, now));
                self.bonds.len() - 1
            }
        };
        let fresh = self.bonds[bi].session.is_none();
        let b = &mut self.bonds[bi];
        (b.chan, b.chan_down, b.addr4, b.addr6, b.egress) = (Some(id), None, addr4, addr6, egress);
        if b.state == BondState::Connecting {
            b.set_state(BondState::Punching, now);
        }
        let dns = own.first().copied().unwrap_or(IpAddr::V4(Ipv4Addr::new(10, 77, 0, 1)));
        let ok = proto::Bond::Ok { addr4, addr6, mtu: TUN_MTU as u16, dns, egress };
        self.send_chan(id, &Msg::Bond(ok));
        self.log(format!("bond open from {} ({peer}), {}", self.bonds[bi].peer, mode.as_str()));
        self.set_mode(bi, mode);
        self.open_session(bi, core::Role::Anchor, fresh);
    }

    /// A client's `bond ok` (§3.2 step 3): the TUN device with its bond addresses, the session.
    fn bond_ok(&mut self, id: u64, addr4: Option<Ipv4Addr>, addr6: Option<Ipv6Addr>, dns: IpAddr, egress: Egress) {
        let now = self.now();
        let Some(p) = self.chans.get(&id) else { return };
        if matches!(p.purpose, Some(Purpose::Pick)) {
            return self.pick_bond_ok(id, addr4, addr6);
        }
        if !matches!(p.purpose, Some(Purpose::Bond)) || self.cfg.role != Role::Client {
            return;
        }
        let Some(h) = p.hello.clone() else { return };
        let peer = p.chan.peer.ip().to_canonical();
        let anchor = self.cfg.anchor.clone().unwrap_or_default();
        let same = self.bonds.first().is_some_and(|b| b.inst == h.inst && b.addr4 == addr4 && b.addr6 == addr6);
        if !same {
            // A new bond (first, or the anchor restarted): the old one goes, capture first, but
            // not this channel, which carries the new one.
            self.capture_off();
            if !self.bonds.is_empty() {
                let mut old = self.bonds.remove(0);
                if let Some(s) = old.session.as_mut() {
                    s.close(&mut self.rids);
                }
                if let Some(c) = old.chan.filter(|&c| c != id).and_then(|c| self.chans.get(&c)) {
                    c.chan.close();
                }
            }
            self.tun = None;
            let mut b = Bond::new(anchor, peer, h.inst, self.cfg.mode, now);
            (b.addr4, b.addr6) = (addr4, addr6);
            self.bonds.push(b);
            let cfg = host::TunConfig {
                name: host::TUN_NAME.into(),
                mtu: TUN_MTU,
                addr4: addr4.map(|a| (a, 24)),
                addr6: addr6.map(|a| (a, 64)),
            };
            match self.plat.open_tun(&cfg) {
                Ok(t) => self.tun = Some(t),
                Err(e) => self.host_failed("TUN device", &e),
            }
        }
        let b = &mut self.bonds[0];
        (b.chan, b.chan_down, b.egress, b.peer_addr) = (Some(id), None, egress, peer);
        if b.state == BondState::Connecting {
            b.set_state(BondState::Punching, now);
        }
        self.dns = Some(dns);
        self.unreachable = None;
        self.backoff = REDIAL_MIN;
        self.log(format!("bond ok: {addr4:?} {addr6:?}, egress {egress:?}"));
        self.open_session(0, core::Role::Client, !same);
    }

    /// Starts bond `bi`'s session when `fresh`, else tells it of the new channel: either way the
    /// engine gets my links and a hello (which sends my key, §3.2 step 4), and the peer my links.
    fn open_session(&mut self, bi: usize, role: core::Role, fresh: bool) {
        if fresh || self.bond_ref(bi).is_none_or(|b| b.session.is_none()) {
            let owner = self.next_owner;
            self.next_owner += 1;
            let rand: core::Rand = Box::new(|b: &mut [u8]| {
                let _ = getrandom::fill(b);
            });
            let b = if bi == PICK { self.pick.as_mut().and_then(|p| p.bond.as_mut()) } else { self.bonds.get_mut(bi) };
            let Some(b) = b else { return };
            if let Some(mut old) = b.session.take() {
                old.close(&mut self.rids);
            }
            b.owner = owner;
            b.session = Some(Session::new(role, owner, rand));
            self.feed_links(bi);
        }
        let Some(mode) = self.bond_ref(bi).map(|b| b.mode) else { return };
        let hello = core::Msg { t: "hello".into(), mode: mode.as_str().into(), ..core::Msg::default() };
        self.handle_bond(bi, core::Event::Control(&hello));
        let offer = self.offer();
        self.send_bond(bi, &Msg::Links(offer));
    }

    /// Changes bond `bi`'s mode, in its session too.
    fn set_mode(&mut self, bi: usize, mode: Mode) {
        self.bonds[bi].mode = mode;
        let m = core::Msg { t: "mode".into(), mode: mode.as_str().into(), ..core::Msg::default() };
        self.handle_bond(bi, core::Event::Control(&m));
    }

    /// The engine's view of my links, and the mesh lane toward bond `bi`'s peer. The engine groups
    /// lanes by label, so a link's label is its group's (§8: the name of the group's lowest link),
    /// and the link carrying the mesh is the `default` one, whose group the mesh lane joins.
    fn feed_links(&mut self, bi: usize) {
        let groups = self.groups();
        let carrier = self.mesh_carrier().map(|i| self.socks[i].link.name.clone());
        let label = |g: u8| self.socks.iter().find(|s| s.id == g).map(|s| s.link.name.clone());
        let links: Vec<core::Link> = self
            .socks
            .iter()
            .enumerate()
            .filter(|(_, s)| s.id != 0 && !self.captive.contains(&s.link.addr))
            .map(|(i, s)| core::Link {
                id: s.id,
                label: label(groups[i]).unwrap_or_else(|| s.link.name.clone()),
                metered: s.link.metered,
                default: carrier.as_ref() == Some(&s.link.name),
                sock: (i + 1) as SockId,
                local: s.local,
                mapped: s.stun.mappings().first().copied(),
            })
            .collect();
        let Some(peer) = self.bond_ref(bi).map(|b| b.peer_addr) else { return };
        let mesh = self.socks.iter().position(|s| s.id == 0 && s.link.addr.is_ipv4() == peer.is_ipv4()).map(|i| {
            core::MeshLane {
                sock: (i + 1) as SockId,
                local: self.socks[i].local,
                peer: SocketAddr::new(peer, self.port),
            }
        });
        self.handle_bond(bi, core::Event::Links { links: &links, mesh });
    }

    /// My `links` (§16): each link with its group (§8) and its candidates (§9.3): its socket's
    /// address, then its STUN mappings, at most MAX_CANDS.
    fn offer(&self) -> proto::Links {
        let groups = self.groups();
        let carrier = self.mesh_carrier();
        let links = self
            .socks
            .iter()
            .zip(groups)
            .enumerate()
            .filter(|(_, (s, _))| s.id != 0 && !self.captive.contains(&s.link.addr))
            .take(MAX_LINKS)
            .map(|(i, (s, group))| {
                let mut cands = vec![s.local];
                for m in s.stun.mappings() {
                    if !cands.contains(&m) && cands.len() < MAX_CANDS {
                        cands.push(m);
                    }
                }
                proto::LinkOffer {
                    id: s.id,
                    kind: kind(&s.link),
                    metered: s.link.metered,
                    group,
                    support: self.support_name(s.link.addr),
                    mesh: carrier == Some(i),
                    cands,
                }
            })
            .collect();
        proto::Links { links }
    }

    /// A peer's `links` as the engine takes them, its candidates filtered: no unspecified,
    /// multicast, broadcast, link-local or mesh address (loopback only in tests); at most
    /// MAX_LINKS links of MAX_CANDS candidates. The engine groups lanes by label, so each link is
    /// labelled after its group's lowest link (§8: the peer's `group`).
    fn peer_links(&self, l: &proto::Links) -> core::Msg {
        let mesh = self.cfg.mesh_ranges();
        let ok = |a: IpAddr| (host::global(a) || self.loopback && a.is_loopback()) && !host::in_ranges(a, &mesh);
        let group = |o: &proto::LinkOffer| if o.group == 0 { o.id } else { o.group };
        let label = |o: &proto::LinkOffer| {
            let lead = l.links.iter().filter(|x| group(x) == group(o)).min_by_key(|x| x.id).unwrap_or(o);
            let name = lead.support.clone();
            name.unwrap_or_else(|| format!("{}{}", format!("{:?}", lead.kind).to_lowercase(), lead.id))
        };
        let links = l
            .links
            .iter()
            .filter(|o| o.id != 0)
            .take(MAX_LINKS)
            .map(|o| core::LinkInfo {
                id: o.id,
                label: link::clean(&label(o), link::LABEL_MAX),
                metered: o.metered,
                default: o.mesh,
                cands: o.cands.iter().map(|&c| link::unmap(c)).filter(|c| ok(c.ip())).take(MAX_CANDS).collect(),
            })
            .collect();
        core::Msg { t: "links".into(), links, ..core::Msg::default() }
    }

    /// A stable link id, 1–MAX_LINKS, for `l`.
    fn link_id(&mut self, l: &host::Link) -> Option<u8> {
        let used: Vec<u8> = self.socks.iter().map(|s| s.id).collect();
        let v4 = l.addr.is_ipv4();
        if let Some(&(_, _, id)) =
            self.link_ids.iter().find(|(n, f, id)| *n == l.name && *f == v4 && !used.contains(id))
        {
            return Some(id);
        }
        let id = (1..=MAX_LINKS as u8).find(|i| !used.contains(i))?;
        self.link_ids.retain(|(n, f, x)| !(*n == l.name && *f == v4) && *x != id);
        self.link_ids.push((l.name.clone(), v4, id));
        Some(id)
    }

    /// Lists the links (§9.1): a socket per new link, the mesh lane's socket and the control
    /// listener on each mesh address; every session hears of a change.
    fn scan(&mut self) {
        // Queued sends name sockets by index, which the scan may change.
        self.flush_tx();
        let ranges = self.cfg.mesh_ranges();
        let (links, meshes) = match self.plat.links(&ranges) {
            Ok(x) => x,
            Err(e) => return self.log(format!("links: {e}")),
        };
        let want: Vec<(host::Link, bool)> =
            links.into_iter().map(|l| (l, false)).chain(meshes.into_iter().map(|l| (l, true))).collect();
        let same =
            |s: &Sock, l: &host::Link, m: bool| (s.id == 0) == m && s.link.addr == l.addr && s.link.name == l.name;
        let before = self.socks.len();
        self.socks.retain(|s| want.iter().any(|(l, m)| same(s, l, *m)));
        let mut changed = self.socks.len() != before;
        for (l, m) in &want {
            if self.socks.iter().any(|s| same(s, l, *m)) {
                continue;
            }
            let id = if *m { Some(0) } else { self.link_id(l) };
            let Some(id) = id else { continue };
            match self.plat.bind(l, *m).and_then(|s| {
                s.set_nonblocking(true)?;
                let local = s.local_addr()?;
                Ok((s, local))
            }) {
                Ok((sock, local)) => {
                    let stun = link::Stun::default();
                    self.socks.push(Sock { id, link: l.clone(), sock, local: link::unmap(local), stun });
                    changed = true;
                    if !*m {
                        self.check_captive(l);
                    }
                }
                Err(e) => self.log(format!("{} {}: {e}", l.name, l.addr)),
            }
            if *m && !self.listening.contains(&l.addr) {
                match ctl::listen(SocketAddr::new(l.addr, self.port), self.ev_tx.clone(), self.waker.clone()) {
                    Ok(()) => self.listening.push(l.addr),
                    Err(e) => self.log(format!("control on {}: {e}", l.addr)),
                }
            }
        }
        if changed {
            // §17: a link appeared or vanished: the groups are recomputed with fresh gateways.
            self.gateways = self.plat.gateways();
            self.socks_changed();
            self.links_changed();
        }
    }

    /// Windows: the loop's wait associates its sockets again (a handle may be reused).
    fn socks_changed(&mut self) {
        #[cfg(windows)]
        if let Some((_, assoc)) = &mut self.evloop {
            assoc.clear();
        }
    }

    /// Tells every session and peer of a change in my links.
    fn links_changed(&mut self) {
        for bi in 0..self.bonds.len() {
            if self.bonds[bi].session.is_some() {
                self.feed_links(bi);
                let offer = self.offer();
                self.send_bond(bi, &Msg::Links(offer));
            }
        }
    }

    /// Starts the captive-portal check over link `l` (§17), off the loop.
    fn check_captive(&self, l: &host::Link) {
        let Some((host, port, path)) = self.captive_url.clone() else { return };
        let (addr, name, tx, waker) = (l.addr, l.name.clone(), self.ev_tx.clone(), self.waker.clone());
        thread::spawn(move || {
            use std::net::ToSocketAddrs;
            let to = (host.as_str(), port)
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.find(|a| a.is_ipv4() == addr.is_ipv4()));
            let captive = to.and_then(|to| link::captive(addr, &name, to, &host, &path));
            let _ = tx.send(ctl::Ev::Captive { addr, captive });
            waker.wake();
        });
    }

    /// The mesh address to dial from, if this node has one.
    fn mesh_addr(&self) -> Option<IpAddr> {
        self.socks.iter().find(|s| s.id == 0).map(|s| s.link.addr)
    }

    /// Timers: link scans, engine ticks, the bond lifecycle and §17's failure rules.
    fn tick(&mut self) {
        let now = self.now();
        if now >= self.next_wake {
            // §11.4: sleep-inclusive time beyond the monotonic clock is a wake.
            self.next_wake = now + SEC;
            if let Some(boot) = self.plat.boot()
                && self.wake.tick(now, boot)
            {
                self.woke(now);
            }
        }
        if now >= self.next_scan {
            self.next_scan = now + LINK_SCAN;
            self.scan();
            self.follow_default();
            self.stun_tick(now);
            self.pool.keep(&self.cfg.allow);
            if self.cfg.role == Role::Anchor {
                for name in self.cfg.allow.iter().filter(|e| e.parse::<IpAddr>().is_err()) {
                    if self.resolved.get(name).is_none_or(|(_, at)| now - at >= RESOLVE_EVERY) {
                        self.resolved.insert(name.clone(), (Vec::new(), now));
                        ctl::resolve(name.clone(), self.ev_tx.clone(), self.waker.clone());
                    }
                }
            }
        }
        for bi in 0..self.bonds.len() {
            if self.bonds[bi].session.as_ref().is_some_and(|s| now >= s.next()) {
                self.handle_bond(bi, core::Event::Tick);
            }
        }
        if self
            .pick
            .as_ref()
            .and_then(|p| p.bond.as_ref())
            .and_then(|b| b.session.as_ref())
            .is_some_and(|s| now >= s.next())
        {
            self.handle_bond(PICK, core::Event::Tick);
        }
        self.pick_tick(now);
        self.chan_tick();
        self.reconcile();
        let (traffic, wake) = (self.tx_bytes + self.rx_bytes, self.waker.clone());
        for line in
            self.updater.tick(now, traffic, &self.version, &mut self.cfg, self.dir.as_deref(), move || wake.wake())
        {
            self.log(line);
        }
        if now >= self.next_life {
            self.next_life = now + LIFE_EVERY;
            self.life(now);
        }
    }

    /// Makes the running state follow the config: the client's bond and the anchor's egress.
    fn reconcile(&mut self) {
        let now = self.now();
        let role = self.cfg.role;
        // A client's bond to another anchor than the configured one, or no longer a client.
        let target = (role == Role::Client).then(|| self.cfg.anchor.clone()).flatten();
        if role != Role::Anchor
            && (!self.bonds.is_empty() && target.as_deref() != self.bonds.first().map(|b| b.peer.as_str()))
        {
            self.client_close(true);
        }
        if role != Role::Client {
            self.close_dialed_bond();
            self.unreachable = None;
        }
        if let Some(anchor) = target {
            let chan_up = self.chans.values().any(|p| matches!(p.purpose, Some(Purpose::Bond)));
            if !chan_up && self.dialing.is_none() && now >= self.redial_at {
                let id = ctl::next_id();
                self.dialing = Some(id);
                self.redial_at = now + self.backoff;
                ctl::dial(anchor, self.port, self.mesh_addr(), id, self.ev_tx.clone(), self.waker.clone());
            }
            if let Some(b) = self.bonds.first()
                && b.mode != self.cfg.mode
            {
                let mode = self.cfg.mode;
                self.set_mode(0, mode);
                self.send_bond(0, &Msg::Mode { mode });
            }
        }
        // The anchor's TUN device and egress (§6).
        if role == Role::Anchor && self.egress.is_none() {
            self.anchor_on();
        } else if role != Role::Anchor && self.egress.is_some() {
            self.anchor_off();
        }
        self.support_tick();
    }

    /// Opens the anchor's TUN device with its bond addresses and enables egress (§6).
    fn anchor_on(&mut self) {
        let net4 = self.cfg.bond_nets().0.and_then(|(a, b)| match network(a, b) {
            IpAddr::V4(n) => Some((n, b)),
            IpAddr::V6(_) => None,
        });
        let net6 = self.cfg.bond_nets().1.and_then(|(a, b)| match network(a, b) {
            IpAddr::V6(n) => Some((n, b)),
            IpAddr::V4(_) => None,
        });
        let net4 = net4.unwrap_or((Ipv4Addr::new(10, 77, 0, 0), 24));
        let first4 = Ipv4Addr::from_bits(net4.0.to_bits() | 1);
        let first6 = net6.map(|(n, b)| (Ipv6Addr::from_bits(n.to_bits() | 1), b));
        let cfg =
            host::TunConfig { name: host::TUN_NAME.into(), mtu: TUN_MTU, addr4: Some((first4, net4.1)), addr6: first6 };
        match self.plat.open_tun(&cfg) {
            Ok(t) => self.tun = Some(t),
            Err(e) => self.host_failed("TUN device", &e),
        }
        let tun = self.tun.as_ref().map_or_else(|| host::TUN_NAME.to_string(), |t| t.name().to_string());
        let e = host::Egress { tun, net4, net6 };
        let on = match self.plat.egress_enable(&e) {
            Ok(on) => Some(on),
            Err(err) => {
                self.host_failed("egress", &err);
                None
            }
        };
        self.log(format!("anchor on: egress {:?}", on.as_ref().map(|o| (o.v4, o.v6))));
        self.egress = Some((e, on));
        let (own, _) = self.anchor_nets();
        let on = self.egress.as_ref().and_then(|(_, on)| on.as_ref()).map_or((false, false), |on| (on.v4, on.v6));
        let own: Vec<IpAddr> = own.into_iter().filter(|a| if a.is_ipv4() { on.0 } else { on.1 }).collect();
        match self.plat.upstream_resolver().and_then(|up| dns::Relay::new(&own, 53, up)) {
            Ok((r, skipped)) => {
                for (a, e) in skipped {
                    self.log(format!("DNS relay on {a}: {e}"));
                }
                self.dns_relay = Some(r);
            }
            Err(e) => self.log(format!("DNS relay: {e}")),
        }
        self.socks_changed();
    }

    /// Ends every bond, disables egress and closes the TUN device.
    fn anchor_off(&mut self) {
        while !self.bonds.is_empty() {
            self.end_bond(0, true);
        }
        if let Some((e, _)) = self.egress.take()
            && let Err(err) = self.plat.egress_disable(&e)
        {
            self.log(format!("egress off: {err}"));
        }
        self.dns_relay = None;
        self.tun = None;
        self.log("anchor off".into());
    }

    /// Ends bond `bi`: `bond close` first when `tell`, then its channel and its session.
    fn end_bond(&mut self, bi: usize, tell: bool) {
        if bi >= self.bonds.len() {
            return;
        }
        if tell {
            self.send_bond(bi, &Msg::Bond(proto::Bond::Close));
        }
        let mut b = self.bonds.remove(bi);
        if let Some(p) = b.chan.and_then(|id| self.chans.get(&id)) {
            p.chan.close();
        }
        if let Some(s) = b.session.as_mut() {
            s.close(&mut self.rids);
        }
    }

    /// Closes the channel dialed for the client's bond, if any.
    fn close_dialed_bond(&mut self) {
        for p in self.chans.values().filter(|p| matches!(p.purpose, Some(Purpose::Bond))) {
            p.chan.close();
        }
        self.dialing = None;
    }

    /// Ends the client's bond (§3.2 step 7): capture removed first, then `bond close` when
    /// `tell`, then the channel and the session; the TUN device goes too.
    fn client_close(&mut self, tell: bool) {
        self.capture_off();
        if !self.bonds.is_empty() {
            self.end_bond(0, tell);
            self.log("bond closed".into());
        }
        self.close_dialed_bond();
        self.tun = None;
        self.dns = None;
        (self.backoff, self.redial_at) = (REDIAL_MIN, 0);
        // A new bond tries capture again (§5.3).
        (self.mesh_captured, self.peer_cands) = (None, Vec::new());
    }

    /// Installs capture for the client's bond (§5.4).
    fn capture_on(&mut self) -> bool {
        if self.capture.is_some() {
            return true;
        }
        if self.mesh_captured.is_some() {
            return false; // §5.3: off for this bond
        }
        let Some(b) = self.bonds.first() else { return false };
        let Some(tun) = self.tun.as_ref().map(|t| t.name().to_string()) else { return false };
        let c = host::Capture {
            tun,
            routes: if b.egress == Egress::None { host::Routes::BondOnly } else { host::Routes::All },
            bond4: b.addr4.map(|a| (Ipv4Addr::from_bits(a.to_bits() & !0xff), 24)),
            bond6: b.addr6.map(|a| (Ipv6Addr::from_bits(a.to_bits() & !0xffff_ffff_ffff_ffff), 64)),
            exclude: self.cfg.mesh_ranges(),
            links: self.socks.iter().filter(|s| s.id != 0).map(|s| s.link.name.clone()).collect(),
            dns: self.dns,
        };
        match self.plat.capture_install(&c) {
            Ok(()) => {
                self.capture = Some(c);
                self.log("capture installed".into());
                // §7: without a way to point the resolver at the anchor, DNS stays local.
                self.dns_local = self.plat.dns_local();
                if self.dns_local {
                    self.log("DNS left alone: queries go to the local resolver".into());
                }
                true
            }
            Err(e) => {
                self.host_failed("capture", &e);
                false
            }
        }
    }

    /// Removes the client's capture, if installed (§5.4).
    fn capture_off(&mut self) {
        self.dns_local = false;
        if let Some(c) = self.capture.take() {
            match self.plat.capture_remove(&c) {
                Ok(()) => self.log("capture removed".into()),
                Err(e) => self.log(format!("capture removal: {e}")),
            }
        }
    }

    /// The bond lifecycle (§3.2 step 6) and §17's rules, from each session's lanes.
    fn life(&mut self, now: Mono) {
        let client = self.cfg.role == Role::Client;
        let mut bi = 0;
        while bi < self.bonds.len() {
            let Some(s) = self.bonds[bi].session.as_ref() else {
                bi += 1;
                continue;
            };
            let snap = s.snapshot();
            let up = snap.lanes.iter().any(|l| l.validated && l.state == "up");
            let direct = snap.lanes.iter().any(|l| l.validated && l.state == "up" && l.id != 0);
            // §17: every lane down, as the engine reports it (suspect is not down). An anchor never
            // probes an idle bond, so there a lane that carries nothing counts too.
            let down = match client {
                true => snap.lanes.iter().all(bond::lane_down),
                false => !snap.lanes.iter().any(bond::lane_carries),
            };
            let b = &mut self.bonds[bi];
            if up {
                b.validated = true;
                b.mesh_only = if direct { None } else { Some(b.mesh_only.unwrap_or(now)) };
            } else {
                b.mesh_only = None;
            }
            if down {
                b.lanes_down.get_or_insert(now);
            } else {
                b.lanes_down = None;
            }
            let lost = |t: Option<Mono>| t.is_some_and(|t| now - t >= FAIL_OPEN_AFTER);
            let gone = lost(b.chan_down) && lost(b.lanes_down);
            if !client {
                // An anchor frees a bond whose lanes and channel are gone (§17), and pick's
                // temporary bond with its channel (§18).
                if gone || b.measure && b.chan.is_none() {
                    self.log(format!("{}: bond lost", self.bonds[bi].peer));
                    self.end_bond(bi, false);
                    continue;
                }
                b.set_state(if up { BondState::Up } else { BondState::Punching }, now);
                bi += 1;
                continue;
            }
            if up && b.state != BondState::Up {
                // §3.2 step 6 and §17: capture with the first validated lane, and back after fail open.
                if self.capture_on() {
                    self.bonds[bi].set_state(BondState::Up, now);
                }
            } else if down && b.state == BondState::Up && gone {
                self.capture_off();
                self.bonds[bi].set_state(BondState::Down, now);
                self.log(format!("bond down for {} s: fail open", FAIL_OPEN_AFTER / SEC));
            } else if down && b.validated && b.chan.is_some() && b.lanes_down.is_some_and(|t| now - t >= ALL_LANES_DOWN)
            {
                // Every lane down while the channel is up: redial, then re-punch (§17).
                b.validated = false;
                let id = b.chan;
                self.log("every lane down: redialing".into());
                if let Some(p) = id.and_then(|id| self.chans.get(&id)) {
                    p.chan.close();
                }
            }
            bi += 1;
        }
    }

    /// Starts measuring the next candidate, or selects when every one is measured (§18).
    fn pick_next(&mut self) {
        let now = self.now();
        let (mesh, tx, waker, port) = (self.mesh_addr(), self.ev_tx.clone(), self.waker.clone(), self.port);
        let Some(p) = self.pick.as_mut() else { return };
        if p.i >= p.cands.len() {
            return self.pick_finish();
        }
        let id = ctl::next_id();
        (p.dialing, p.chan, p.bond, p.started, p.result) = (Some(id), None, None, now, None);
        ctl::dial(p.cands[p.i].clone(), port, mesh, id, tx, waker);
    }

    /// Ends the temporary bond and records `m` for the current candidate.
    fn pick_record(&mut self, m: pick::Measured) {
        self.send_bond(PICK, &Msg::Bond(proto::Bond::Close));
        let Some(p) = self.pick.as_mut() else { return };
        if let Some(mut b) = p.bond.take()
            && let Some(s) = b.session.as_mut()
        {
            s.close(&mut self.rids);
        }
        if let Some(c) = p.chan.take().and_then(|id| self.chans.get(&id)) {
            c.chan.close();
        }
        p.dialing = None;
        p.results.push(m);
        p.i += 1;
        self.pick_next();
    }

    /// The current candidate could not be measured: it scores last with `why`.
    fn pick_fail(&mut self, why: String) {
        let Some(p) = self.pick.as_ref() else { return };
        let m = pick::Measured { device: p.cands[p.i].clone(), failed: Some(why), ..pick::Measured::default() };
        self.pick_record(m);
    }

    /// The selection (§18): the rows, the winner, and unless `--dry-run` the new anchor.
    fn pick_finish(&mut self) {
        let Some(p) = self.pick.take() else { return };
        let rows = pick::rows(p.mode, &p.results);
        let winner = pick::select(p.mode, &p.results).map(|i| p.results[i].device.clone());
        let mut applied = false;
        if let Some(w) = winner.clone().filter(|_| !p.dry_run) {
            let r = self.answer(Request::Client { anchor: w, mode: Some(p.mode) });
            applied = r.ok;
        }
        self.log(format!("pick: {} candidates, winner {winner:?}", p.results.len()));
        let pick = api::Pick { mode: p.mode, rows, winner, applied };
        let _ = p.reply.send(Reply { ok: true, changed: applied, pick: Some(pick), ..Reply::default() });
    }

    /// Daemon exit: capture removed first, then each bond closed (§3.2 step 7, §5.4).
    fn shutdown(&mut self) -> io::Result<()> {
        if self.cfg.role == Role::Anchor {
            self.anchor_off();
        } else {
            self.client_close(true);
        }
        Ok(())
    }

    /// One local API call: answered at once, or for a remote view once the peer answers.
    fn call(&mut self, req: Request, reply: Sender<Reply>) {
        match req {
            Request::Status { device: Some(d) } => {
                let id = ctl::next_id();
                self.view_waiting.insert(id, reply);
                ctl::dial(d, self.port, self.mesh_addr(), id, self.ev_tx.clone(), self.waker.clone());
            }
            Request::Pick { devices, mode, seconds, dry_run } => {
                if self.pick.is_some() {
                    let _ = reply.send(Reply::fail("busy", "a pick is in progress"));
                    return;
                }
                let cands = if devices.is_empty() { self.cfg.anchors.clone() } else { devices };
                if cands.is_empty() {
                    let _ =
                        reply.send(Reply::fail("failed", "no candidates: name devices, or add them with pick --add"));
                    return;
                }
                self.pick = Some(PickRun {
                    cands,
                    i: 0,
                    mode: mode.unwrap_or(self.cfg.mode),
                    seconds: seconds.unwrap_or((PICK_SECONDS / SEC) as u32).max(1),
                    dry_run,
                    reply,
                    results: Vec::new(),
                    dialing: None,
                    chan: None,
                    bond: None,
                    started: 0,
                    result: None,
                });
                self.pick_next();
            }
            Request::Update => {
                let now = self.now();
                self.updater.request(now, reply);
            }
            req => {
                let _ = reply.send(self.answer(req));
            }
        }
    }

    /// One local API request (§20.1). The config-changing verbs save config.json at once; the
    /// loop acts on the new config in `reconcile`.
    fn answer(&mut self, req: Request) -> Reply {
        let before = self.cfg.clone();
        let support_on = matches!(req, Request::SupportOn { .. });
        let c = &mut self.cfg;
        match req {
            Request::Status { device: None } => {
                return Reply { ok: true, status: Some(Box::new(self.status(false))), ..Reply::default() };
            }
            Request::Status { device: Some(_) } | Request::Pick { .. } | Request::Update => {
                return Reply::fail("failed", "2.0: not yet");
            }
            Request::Client { anchor, mode } => {
                (c.role, c.anchor) = (Role::Client, Some(anchor));
                c.mode = mode.unwrap_or(c.mode);
            }
            Request::ClientOff => {
                if c.role == Role::Client {
                    c.role = Role::None;
                }
            }
            Request::Mode { mode } => c.mode = mode,
            Request::PickAdd { device } => {
                if !c.anchors.contains(&device) {
                    c.anchors.push(device);
                }
            }
            Request::PickRemove { device } => c.anchors.retain(|a| *a != device),
            Request::AnchorOn => c.role = Role::Anchor,
            Request::AnchorOff => {
                if c.role == Role::Anchor {
                    c.role = Role::None;
                }
            }
            Request::AnchorAllow { devices } => {
                for d in devices {
                    if !c.allow.contains(&d) {
                        c.allow.push(d);
                    }
                }
            }
            Request::AnchorDeny { devices } => c.allow.retain(|a| !devices.contains(a)),
            Request::SupportOn { uplink, cable, for_client } => {
                c.role = Role::Support;
                c.support = Some(host::SupportConfig { uplink, cable, for_client });
            }
            Request::SupportOff => {
                if c.role == Role::Support {
                    (c.role, c.support) = (Role::None, None);
                }
            }
        }
        let changed = self.cfg != before;
        if changed
            && let Some(dir) = &self.dir
            && let Err(e) = self.cfg.save(dir)
        {
            self.cfg = before;
            return Reply::fail("failed", format!("config.json: {e}"));
        }
        if changed {
            self.reconcile();
        }
        // §8: when the OS refuses to share, the command prints the manual steps.
        if let Some(steps) = self.support.refused.clone().filter(|_| support_on) {
            return Reply { changed, ..Reply::fail("failed", steps) };
        }
        Reply { ok: true, changed, ..Reply::default() }
    }

    /// One session's lanes as status shows them (§20.2).
    fn lanes(&self, b: &Bond) -> (Vec<api::Lane>, u32, u32) {
        let Some(s) = b.session.as_ref() else { return (Vec::new(), 0, 0) };
        let snap = s.snapshot();
        let total: u64 = snap.lanes.iter().map(|l| l.out.sent_bytes).sum();
        // A lane's group is its client link's (§8); the mesh lane's, the link carrying the mesh's.
        let groups = self.groups();
        let group = |lane: u16| {
            let link = (lane >> 8) as u8;
            let mine = self.cfg.role == Role::Client;
            let at = self.socks.iter().position(|s| s.id == link && (link != 0 || s.link.addr.is_ipv4()));
            let theirs = b.offer.iter().find(|o| if link == 0 { o.mesh } else { o.id == link });
            let theirs = theirs.map_or(link, |o| if o.group == 0 { o.id } else { o.group });
            at.filter(|_| mine).map_or(theirs, |i| groups[i])
        };
        let lanes = snap
            .lanes
            .iter()
            .map(|l| api::Lane {
                id: l.id,
                links: vec![l.local.clone(), l.remote.clone()],
                group: group(l.id),
                state: match l.state {
                    "up" => api::LaneState::Up,
                    "suspect" => api::LaneState::Suspect,
                    "down" => api::LaneState::Down,
                    "dormant" => api::LaneState::Dormant,
                    _ => api::LaneState::Punching,
                },
                rtt_ms: l.out.rtt_ms,
                owd_ms: l.out.owd_rel_ms,
                loss: l.out.loss,
                rate_mbps: l.out.rate_mbps,
                share: if total == 0 { 0.0 } else { l.out.sent_bytes as f64 / total as f64 },
                rescues_per_min: l.rescues,
                reorder_p50_ms: l.reorder_wait_p50 as f64 / MS as f64,
                reorder_p99_ms: l.reorder_wait_p99 as f64 / MS as f64,
                ..api::Lane::default()
            })
            .collect();
        let small = snap.flows.iter().filter(|f| f.class == "small").count() as u32;
        (lanes, snap.flows.len() as u32, small)
    }

    /// The problems of §17 and the host's (§20.2).
    fn problems(&self) -> Vec<api::Problem> {
        let now = self.now();
        let p = |code: &str, next: Option<&str>, msg: String| api::Problem {
            code: code.into(),
            link: None,
            next: next.map(Into::into),
            msg,
        };
        let mut v = Vec::new();
        if let Some(e) = &self.cfg_bad {
            v.push(p(problem::CONFIG_INVALID, None, e.clone()));
        }
        if self.cfg.role == Role::Client {
            if let Some(e) = &self.unreachable {
                v.push(p(problem::ANCHOR_UNREACHABLE, None, e.clone()));
            }
            if let Some(b) = self.bonds.first() {
                if b.state == BondState::Down {
                    v.push(p(problem::BOND_DOWN, None, "every lane and the channel down: traffic goes direct".into()));
                }
                if b.state == BondState::Punching && now - b.since >= LANES_BLOCKED_AFTER {
                    v.push(p(problem::LANES_BLOCKED, None, "no lane validated".into()));
                }
                if b.mesh_only.is_some_and(|t| now - t >= MESH_ONLY_AFTER) {
                    v.push(p(problem::MESH_ONLY, None, "only the mesh lane is up".into()));
                }
                if b.egress == Egress::None && b.session.is_some() {
                    v.push(p(problem::NO_EGRESS, None, "the anchor offers no egress".into()));
                }
            }
            if let Some(why) = &self.mesh_captured {
                let msg =
                    format!("the mesh's own traffic entered the bond ({why}): capture removed, traffic goes direct");
                v.push(p(problem::MESH_CAPTURED, Some("wirebond client off"), msg));
            }
            if self.dns_local && self.capture.is_some() {
                v.push(p(problem::DNS_LOCAL, None, "DNS left alone: queries go to the local resolver".into()));
            }
        }
        let mine = major(&self.version);
        for h in self.chans.values().filter_map(|p| p.hello.as_ref()) {
            if let (Some(m), Some(theirs)) = (mine, major(&h.version))
                && theirs > m
            {
                v.push(p(problem::PEER_NEWER, Some("wirebond update"), format!("{} runs {}", h.name, h.version)));
            }
        }
        for s in self.socks.iter().filter(|s| self.captive.contains(&s.link.addr)) {
            let bonded = self.cfg.role == Role::Client && self.bonds.first().is_some_and(|b| b.state == BondState::Up);
            v.push(api::Problem {
                code: problem::CAPTIVE_PORTAL.into(),
                link: Some(s.link.label.clone()),
                next: bonded.then(|| "wirebond client off".into()),
                msg: "log in on this link: run `wirebond client off`, log in, then `wirebond client <anchor>`".into(),
            });
        }
        v.extend(self.host_problems.iter().cloned());
        v
    }

    /// This node's status (§20.2); a remote view (`remote`) omits flow details.
    fn status(&self, remote: bool) -> Status {
        let links = self
            .socks
            .iter()
            .zip(self.groups())
            .filter(|(s, _)| s.id != 0)
            .map(|(s, group)| api::Link {
                id: s.id,
                name: s.link.label.clone(),
                kind: kind(&s.link),
                group,
                metered: s.link.metered,
                up: true,
                captive: self.captive.contains(&s.link.addr),
                support: self.support_name(s.link.addr),
            })
            .collect();
        let bond = (self.cfg.role == Role::Client).then(|| self.bonds.first()).flatten().map(|b| {
            let (lanes, flows, small) = self.lanes(b);
            api::Bond {
                anchor: b.peer.clone(),
                mode: b.mode,
                state: b.state,
                addr4: b.addr4.map(|a| a.to_string()),
                addr6: b.addr6.map(|a| a.to_string()),
                anchor_addr4: self.dns.filter(IpAddr::is_ipv4).map(|a| a.to_string()),
                anchor_addr6: b
                    .addr6
                    .map(|a| Ipv6Addr::from_bits((a.to_bits() & !0xffff_ffff_ffff_ffff) | 1).to_string()),
                egress: b.egress,
                lanes,
                flows: if remote { 0 } else { flows },
                small_flows: if remote { 0 } else { small },
            }
        });
        let anchor = (self.cfg.role == Role::Anchor).then(|| api::AnchorView {
            bonds: self
                .bonds
                .iter()
                .map(|b| api::AnchorBond {
                    client: b.peer.clone(),
                    mode: b.mode,
                    state: b.state,
                    addr4: b.addr4.map(|a| a.to_string()),
                    addr6: b.addr6.map(|a| a.to_string()),
                    egressed_bytes: b.egressed,
                    local_bytes: b.local,
                    lanes: self.lanes(b).0,
                })
                .collect(),
            uplink: self.egress.as_ref().and_then(|(_, on)| on.as_ref()).and_then(|on| on.uplink.clone()),
            egress: match self.egress.as_ref().and_then(|(_, on)| on.as_ref()) {
                Some(on) if on.v4 && on.v6 => Egress::Both,
                Some(on) if on.v4 => Egress::V4,
                Some(on) if on.v6 => Egress::V6,
                _ => Egress::None,
            },
            allow: self.cfg.allow.clone(),
        });
        let support = self.cfg.support.as_ref().filter(|_| self.cfg.role == Role::Support).map(|s| api::SupportView {
            uplink: s.uplink.clone(),
            cable: s.cable.clone(),
            client: s.for_client.clone(),
            carried_bytes: self.support.carried.0 + self.support.carried.1,
        });
        let mesh = self
            .chans
            .values()
            .filter_map(|p| p.hello.as_ref())
            .map(|h| api::MeshPeer {
                device: link::clean(&h.name, link::LABEL_MAX),
                role: h.role,
                bonds: self.bonds.iter().filter(|b| b.inst == h.inst).count() as u32,
                reachable: true,
            })
            .collect();
        Status {
            device: self.name.clone(),
            role: self.cfg.role,
            version: self.version.clone(),
            links,
            bond,
            anchor,
            support,
            contribution: self.contribution(),
            mesh,
            problems: self.problems(),
            events: self.logs.iter().rev().take(STATUS_EVENTS).rev().cloned().collect(),
        }
    }
}
