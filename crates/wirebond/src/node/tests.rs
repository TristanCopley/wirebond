//! The daemon on a fake host (no root): fake TUN devices and links on loopback addresses, a client
//! and an anchor in one process. Run inside `unshare -n` with `lo` up, as every network test.

use std::sync::Mutex;

use super::*;

/// A fake TUN device: what the test writes for the daemon to read, and what the daemon wrote.
#[derive(Clone, Default)]
struct FakeTun(Arc<Mutex<Queues>>);

/// What the daemon will read, and what it wrote.
type Queues = (VecDeque<Vec<u8>>, Vec<Vec<u8>>);

impl FakeTun {
    fn inject(&self, p: Vec<u8>) {
        self.0.lock().unwrap().0.push_back(p);
    }

    fn written(&self) -> Vec<Vec<u8>> {
        self.0.lock().unwrap().1.clone()
    }
}

struct TunHandle(FakeTun);

impl TunDev for TunHandle {
    fn name(&self) -> &str {
        "fake0"
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let p = self.0.0.lock().unwrap().0.pop_front().ok_or(io::ErrorKind::WouldBlock)?;
        buf[..p.len()].copy_from_slice(&p);
        Ok(p.len())
    }

    fn write(&self, packet: &[u8]) -> io::Result<()> {
        self.0.0.lock().unwrap().1.push(packet.to_vec());
        Ok(())
    }
}

/// A fake host: one link and one mesh address, a fake TUN device, and a record of the calls.
struct Fake {
    name: String,
    link: host::Link,
    mesh: host::Link,
    tun: FakeTun,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Platform for Fake {
    fn open_tun(&mut self, c: &host::TunConfig) -> io::Result<Box<dyn TunDev>> {
        self.calls.lock().unwrap().push(format!("tun {:?}", c.addr4));
        Ok(Box::new(TunHandle(self.tun.clone())))
    }

    fn capture_install(&mut self, c: &host::Capture) -> io::Result<()> {
        self.calls.lock().unwrap().push(format!("capture on {:?} dns {:?}", c.routes, c.dns));
        Ok(())
    }

    fn capture_remove(&mut self, _: &host::Capture) -> io::Result<()> {
        self.calls.lock().unwrap().push("capture off".into());
        Ok(())
    }

    fn egress_enable(&mut self, _: &host::Egress) -> io::Result<host::EgressOn> {
        self.calls.lock().unwrap().push("egress on".into());
        Ok(host::EgressOn { v4: true, v6: true, addr4: (Ipv4Addr::new(10, 77, 0, 1), 24), uplink: Some("eth0".into()) })
    }

    fn egress_disable(&mut self, _: &host::Egress) -> io::Result<()> {
        self.calls.lock().unwrap().push("egress off".into());
        Ok(())
    }

    fn upstream_resolver(&mut self) -> io::Result<SocketAddr> {
        Ok("127.0.0.53:53".parse().unwrap())
    }

    fn links(&mut self, _: &[(IpAddr, u8)]) -> io::Result<(Vec<host::Link>, Vec<host::Link>)> {
        Ok((vec![self.link.clone()], vec![self.mesh.clone()]))
    }

    fn bind(&mut self, link: &host::Link, mesh: bool) -> io::Result<UdpSocket> {
        UdpSocket::bind((link.addr, if mesh { PORT } else { 0 }))
    }

    fn hostname(&self) -> String {
        self.name.clone()
    }
}

/// The daemon's 1 ms timers, for the rest of the test process, as `wirebond run` holds them for
/// its own (`host::run_service`); a no-op off Windows. Without them a Windows wait ends on the
/// default 15.6 ms tick (CI's windows-latest: a 1 ms wait overshoots by 14.7 ms at the median), so
/// whenever neither daemon had a datagram waiting, `run`'s 2 ms waits gave each a turn only every
/// 16–31 ms. The pacer, whose bucket holds PACE_BURST at pace plus a packet, then sent two or three
/// bulk packets a turn and dropped those that waited past QUEUE_AGE: on one CI run 11 or 12 of the
/// 50 each way, never sent, and both daemons went idle short of 50; on the next the turns came
/// often enough. Linux reproduces it on every run with each wait ending on a 15.6 ms tick, and
/// passes with ticks of 1 to 8 ms.
fn fine_timers() {
    #[cfg(windows)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| std::mem::forget(host::fine_timers()));
    }
}

/// A node named `name` with its link on `link` and its mesh address `mesh`.
fn node(name: &str, link: IpAddr, mesh: IpAddr, cfg: Config) -> (Daemon, FakeTun, Arc<Mutex<Vec<String>>>) {
    fine_timers();
    let l = |name: &str, addr| host::Link { index: 1, name: name.into(), addr, label: name.into(), metered: false };
    let (tun, calls) = (FakeTun::default(), Arc::new(Mutex::new(Vec::new())));
    let fake = Fake {
        name: name.into(),
        link: l("lo-a", link),
        mesh: l("mesh0", mesh),
        tun: tun.clone(),
        calls: calls.clone(),
    };
    let mut d = Daemon::new("2.0.0", None, cfg, Box::new(fake)).unwrap();
    (d.loopback, d.captive_url) = (true, None);
    (d, tun, calls)
}

/// 127.`net`.0.`host`.
fn ip(net: u8, host: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, net, 0, host))
}

/// An IPv4/UDP packet from `src` to `dst` carrying `data`.
fn udp(src: [u8; 4], dst: [u8; 4], data: &[u8]) -> Vec<u8> {
    let total = (28 + data.len()) as u16;
    let mut p = vec![0x45, 0];
    p.extend_from_slice(&total.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0]);
    p.extend_from_slice(&src);
    p.extend_from_slice(&dst);
    p.extend_from_slice(&5000u16.to_be_bytes());
    p.extend_from_slice(&6000u16.to_be_bytes());
    p.extend_from_slice(&(total - 20).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(data);
    p
}

/// Runs both daemons until `done` or 15 s.
fn run(a: &mut Daemon, c: &mut Daemon, what: &str, mut done: impl FnMut(&Daemon, &Daemon) -> bool) {
    let (mut pa, mut pc) = (host::Poll::default(), host::Poll::default());
    let t = Instant::now();
    while !done(a, c) {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "{what}: timed out\nclient {:#?}\nanchor {:#?}",
            c.status(false),
            a.status(false)
        );
        c.step(&mut pc, Duration::from_millis(2)).unwrap();
        a.step(&mut pa, Duration::from_millis(2)).unwrap();
    }
}

/// A client and an anchor bond over loopback; packets cross both ways through the fake TUN
/// devices; the anchor's checks hold; `client off` removes capture and frees the anchor's bond.
fn bond_end_to_end(net: u8, mode: Mode) {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(net, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(net, 20).to_string()), mode, ..Config::default() };
    let (mut a, atun, acalls) = node("xanth", ip(net, 2), ip(net, 20), acfg);
    let (mut c, ctun, ccalls) = node("mac", ip(net, 1), ip(net, 10), ccfg);
    run(&mut a, &mut c, "bond up", |_, c| c.bonds.first().is_some_and(|b| b.state == BondState::Up));
    let st = c.status(false);
    let b = st.bond.as_ref().unwrap();
    assert_eq!((b.addr4.as_deref(), b.anchor_addr4.as_deref(), b.mode), (Some("10.77.0.2"), Some("10.77.0.1"), mode));
    assert!(ccalls.lock().unwrap().iter().any(|c| c.starts_with("capture on All dns Some(10.77.0.1)")));
    assert!(acalls.lock().unwrap().contains(&"egress on".to_string()));
    assert_eq!(a.status(false).anchor.unwrap().bonds[0].addr4.as_deref(), Some("10.77.0.2"));
    assert_eq!(a.bonds[0].mode, mode, "bond open carries the mode");
    assert_eq!(c.logs.iter().filter(|l| l.starts_with("bond ok")).count(), 1, "one connect: {:?}", c.logs);

    // `wirebond mode` reaches the anchor's side of the bond (§14, §16 `mode`), and back.
    let other = if mode == Mode::Performance { Mode::Bandwidth } else { Mode::Performance };
    assert!(c.answer(Request::Mode { mode: other }).changed);
    run(&mut a, &mut c, "mode change", |a, _| a.bonds.first().is_some_and(|b| b.mode == other));
    assert!(c.answer(Request::Mode { mode }).changed);
    run(&mut a, &mut c, "mode back", |a, _| a.bonds.first().is_some_and(|b| b.mode == mode));

    // Client to the anchor itself, the reply back, and client to the internet.
    let local = udp([10, 77, 0, 2], [10, 77, 0, 1], b"to the anchor");
    ctun.inject(local.clone());
    run(&mut a, &mut c, "local delivery", |_, _| atun.written().contains(&local));
    let reply = udp([10, 77, 0, 1], [10, 77, 0, 2], b"reply");
    atun.inject(reply.clone());
    run(&mut a, &mut c, "reply", |_, _| ctun.written().contains(&reply));
    let out = udp([10, 77, 0, 2], [1, 1, 1, 1], b"to the internet");
    ctun.inject(out.clone());
    run(&mut a, &mut c, "egress", |_, _| atun.written().contains(&out));
    for i in 0..50u8 {
        let p = udp([10, 77, 0, 2], [1, 1, 1, 1], &[i; 1000]);
        ctun.inject(p.clone());
        let back = udp([1, 1, 1, 1], [10, 77, 0, 2], &[i; 1000]);
        atun.inject(back);
    }
    run(&mut a, &mut c, "bulk both ways", |_, _| {
        atun.written().iter().filter(|p| p.len() == 1028).count() == 50
            && ctun.written().iter().filter(|p| p.len() == 1028).count() == 50
    });

    // §6.3, §10.5 rule 6: a forged source and bond-to-bond traffic are dropped and counted.
    let forged = udp([10, 77, 0, 9], [1, 1, 1, 1], b"forged");
    let sideways = udp([10, 77, 0, 2], [10, 77, 0, 3], b"sideways");
    let wrong = udp([1, 1, 1, 1], [10, 77, 0, 7], b"not for this client");
    ctun.inject(forged.clone());
    ctun.inject(sideways.clone());
    run(&mut a, &mut c, "forged dropped", |a, _| a.bonds.first().is_some_and(|b| b.dropped >= 2));
    assert!(!atun.written().contains(&forged) && !atun.written().contains(&sideways));
    atun.inject(wrong.clone()); // no bond has that address: dropped on the anchor's TUN read
    let stats =
        (a.status(false).anchor.unwrap().bonds[0].egressed_bytes, a.status(false).anchor.unwrap().bonds[0].local_bytes);
    assert!(stats.0 > 0 && stats.1 > 0, "{stats:?}");

    // §3.2 step 7: capture first, then bond close; the anchor frees the bond within 1 s.
    assert!(c.answer(Request::ClientOff).changed);
    assert_eq!(ccalls.lock().unwrap().last().map(String::as_str), Some("capture off"));
    let t = Instant::now();
    run(&mut a, &mut c, "anchor frees the bond", |a, _| a.bonds.is_empty());
    assert!(t.elapsed() < Duration::from_secs(1));
    assert!(c.bonds.is_empty() && c.capture.is_none());
    assert!(!ctun.written().contains(&wrong));
}

#[test]
fn bond_end_to_end_performance() {
    bond_end_to_end(31, Mode::Performance);
}

#[test]
fn bond_end_to_end_bandwidth() {
    bond_end_to_end(32, Mode::Bandwidth);
}

#[test]
fn a_device_off_the_allow_list_is_refused() {
    let acfg = Config { role: Role::Anchor, allow: vec!["someone-else".into()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(33, 20).to_string()), ..Config::default() };
    let (mut a, _, _) = node("xanth", ip(33, 2), ip(33, 20), acfg);
    let (mut c, _, ccalls) = node("mac", ip(33, 1), ip(33, 10), ccfg);
    run(&mut a, &mut c, "refused", |_, c| c.unreachable.as_deref().is_some_and(|u| u.contains("NotAllowed")));
    let st = c.status(false);
    assert!(st.problems.iter().any(|p| p.code == problem::ANCHOR_UNREACHABLE), "{st:?}");
    assert!(a.bonds.is_empty() && c.bonds.is_empty());
    assert!(!ccalls.lock().unwrap().iter().any(|c| c.starts_with("capture on")), "no capture without a bond");
}

#[test]
fn an_unreachable_anchor_backs_off() {
    let ccfg = Config { role: Role::Client, anchor: Some(ip(34, 20).to_string()), ..Config::default() };
    let (mut c, _, _) = node("mac", ip(34, 1), ip(34, 10), ccfg);
    let mut p = host::Poll::default();
    let t = Instant::now();
    while c.unreachable.is_none() {
        assert!(t.elapsed() < Duration::from_secs(10), "no anchor_unreachable");
        c.step(&mut p, Duration::from_millis(5)).unwrap();
    }
    assert!(c.status(false).problems.iter().any(|p| p.code == problem::ANCHOR_UNREACHABLE));
    assert_eq!(c.backoff, 2 * REDIAL_MIN, "the backoff doubles");
    assert!(c.redial_at > c.now(), "no redial before the backoff");
}

#[test]
fn a_wrong_protocol_version_is_refused() {
    let acfg = Config { role: Role::Anchor, ..Config::default() };
    let (mut a, _, _) = node("xanth", ip(35, 2), ip(35, 20), acfg);
    let mut p = host::Poll::default();
    a.step(&mut p, Duration::from_millis(1)).unwrap(); // scans: listens on the mesh address
    let mut s = std::net::TcpStream::connect((ip(35, 20), PORT)).unwrap();
    s.write_all(b"{\"t\":\"hello\",\"v\":1,\"inst\":5,\"name\":\"old\",\"role\":\"client\",\"version\":\"1.0.0\"}\n")
        .unwrap();
    s.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let mut got = Vec::new();
    let t = Instant::now();
    while !got.ends_with(b"\n") || got.iter().filter(|&&b| b == b'\n').count() < 2 {
        assert!(t.elapsed() < Duration::from_secs(5), "{}", String::from_utf8_lossy(&got));
        a.step(&mut p, Duration::from_millis(2)).unwrap();
        let mut b = [0u8; 4096];
        if let Ok(n) = io::Read::read(&mut s, &mut b) {
            got.extend_from_slice(&b[..n]);
        }
    }
    let text = String::from_utf8_lossy(&got);
    assert!(text.lines().next().unwrap().starts_with(r#"{"t":"hello","v":2"#), "{text}");
    assert!(text.contains(r#"{"t":"error","code":"version""#), "{text}");
}

#[test]
fn config_verbs_change_the_config() {
    let (mut d, _, _) = node("mac", ip(36, 1), ip(36, 10), Config::default());
    let r = d.answer(Request::Client { anchor: "xanth".into(), mode: Some(api::Mode::Bandwidth) });
    assert!(r.ok && r.changed, "{r:?}");
    assert_eq!((d.cfg.role, d.cfg.anchor.as_deref(), d.cfg.mode), (Role::Client, Some("xanth"), api::Mode::Bandwidth));
    assert!(!d.answer(Request::Mode { mode: api::Mode::Bandwidth }).changed, "idempotent");
    assert!(d.answer(Request::ClientOff).changed);
    assert_eq!(d.cfg.role, Role::None);
    let st = d.answer(Request::Status { device: None }).status.unwrap();
    assert_eq!((st.role, st.version.as_str()), (Role::None, "2.0.0"));
}

#[test]
fn config_verbs_save_config_json() {
    let dir = std::env::temp_dir().join(format!("wirebond-node-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let (mut d, _, _) = node("mac", ip(37, 1), ip(37, 10), Config::default());
    d.dir = Some(dir.clone());
    assert!(d.answer(Request::AnchorAllow { devices: vec!["mac".into()] }).changed);
    assert_eq!(Config::load(&dir).unwrap().allow, vec!["mac".to_string()]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_lost_bond_fails_open_after_fail_open_after() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(38, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(38, 20).to_string()), ..Config::default() };
    let (mut a, _, _) = node("xanth", ip(38, 2), ip(38, 20), acfg);
    let (mut c, ctun, ccalls) = node("mac", ip(38, 1), ip(38, 10), ccfg);
    run(&mut a, &mut c, "bond up", |_, c| c.bonds.first().is_some_and(|b| b.state == BondState::Up));
    // The anchor vanishes while traffic flows: its lane sockets and the channel go with it.
    drop(a);
    let lost = Instant::now();
    let mut p = host::Poll::default();
    while c.bonds.first().is_some_and(|b| b.state == BondState::Up) {
        assert!(lost.elapsed() < Duration::from_secs(15), "no fail open: {:#?}", c.status(false));
        ctun.inject(udp([10, 77, 0, 2], [1, 1, 1, 1], b"still sending"));
        c.step(&mut p, Duration::from_millis(5)).unwrap();
    }
    let t = lost.elapsed();
    assert!(t >= Duration::from_secs(5), "{t:?}");
    // On the daemon's clock, so a loaded test machine cannot blur it: the bond is down once both
    // the channel and every lane are (§17; a lane is down DOWN_AFTER after its last echo, so the
    // channel usually goes first), and fail open comes FAIL_OPEN_AFTER after that, within 1 s (A7).
    let b = &c.bonds[0];
    let (chan, lanes) = (b.chan_down.unwrap(), b.lanes_down.unwrap());
    assert!(b.since - chan.max(lanes) >= FAIL_OPEN_AFTER, "{} {chan} {lanes}", b.since);
    assert!(b.since - chan.max(lanes) <= FAIL_OPEN_AFTER + SEC, "{} {chan} {lanes}", b.since);
    assert_eq!(c.bonds[0].state, BondState::Down);
    assert_eq!(ccalls.lock().unwrap().last().map(String::as_str), Some("capture off"));
    assert!(c.capture.is_none());
    assert!(c.status(false).problems.iter().any(|p| p.code == problem::BOND_DOWN));
}

#[test]
fn a_remote_view_answers_with_the_peers_status() {
    let acfg = Config { role: Role::Anchor, ..Config::default() };
    let (mut a, _, _) = node("xanth", ip(39, 2), ip(39, 20), acfg);
    let (mut c, _, _) = node("mac", ip(39, 1), ip(39, 10), Config::default());
    let (tx, rx) = mpsc::channel();
    run(&mut a, &mut c, "scan", |a, c| !a.listening.is_empty() && !c.socks.is_empty());
    c.call(Request::Status { device: Some(ip(39, 20).to_string()) }, tx);
    let mut got = None;
    run(&mut a, &mut c, "view", |_, _| {
        got = got.take().or_else(|| rx.try_recv().ok());
        got.is_some()
    });
    let r = got.unwrap();
    let st = r.status.clone().unwrap_or_else(|| panic!("{r:?}"));
    assert_eq!((st.device.as_str(), st.role), ("xanth", Role::Anchor));
    assert!(st.anchor.is_some());
}

#[test]
fn a_captive_link_carries_no_lane() {
    let ln = std::net::TcpListener::bind((ip(42, 80), 0)).unwrap();
    let port = ln.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut s in ln.incoming().flatten() {
            let _ = io::Read::read(&mut s, &mut [0u8; 1024]);
            let _ = s.write_all(b"HTTP/1.0 302 Found\r\nLocation: http://login.hotel/\r\n\r\n");
        }
    });
    let (mut d, _, _) = node("mac", ip(42, 1), ip(42, 10), Config::default());
    d.captive_url = Some((ip(42, 80).to_string(), port, "/hotspot-detect.html".into()));
    let mut p = host::Poll::default();
    let t = Instant::now();
    while d.captive.is_empty() {
        assert!(t.elapsed() < Duration::from_secs(10), "no captive check");
        d.step(&mut p, Duration::from_millis(5)).unwrap();
    }
    assert!(d.offer().links.is_empty(), "a captive link is not offered");
    let st = d.status(false);
    assert!(st.links[0].captive);
    assert!(st.problems.iter().any(|p| p.code == problem::CAPTIVE_PORTAL && p.link.as_deref() == Some("lo-a")));
}

/// Runs `req` on `c` until its reply comes.
fn call(a: &mut Daemon, c: &mut Daemon, req: Request) -> Reply {
    let (tx, rx) = mpsc::channel();
    c.call(req, tx);
    let mut got = None;
    run(a, c, "reply", |_, _| {
        got = got.take().or_else(|| rx.try_recv().ok());
        got.is_some()
    });
    got.unwrap()
}

#[test]
fn pick_measures_selects_and_dry_run_changes_nothing() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(43, 10).to_string()], ..Config::default() };
    let (mut a, _, _) = node("xanth", ip(43, 2), ip(43, 20), acfg);
    let (mut c, _, ccalls) = node("mac", ip(43, 1), ip(43, 10), Config::default());
    run(&mut a, &mut c, "scan", |a, c| !a.listening.is_empty() && !c.socks.is_empty());
    let (anchor, nobody) = (ip(43, 20).to_string(), ip(43, 99).to_string());
    let pick = |devices: &[&String], dry_run| Request::Pick {
        devices: devices.iter().map(|d| d.to_string()).collect(),
        mode: Some(Mode::Performance),
        seconds: Some(2),
        dry_run,
    };
    let r = call(&mut a, &mut c, pick(&[&nobody, &anchor], true));
    let p = r.pick.clone().unwrap_or_else(|| panic!("{r:?}"));
    assert_eq!(p.winner.as_deref(), Some(anchor.as_str()), "{p:?}");
    assert!(!p.applied && !r.changed);
    assert!(p.rows[0].score.is_none() && !p.rows[0].reason.is_empty(), "{p:?}");
    assert!(p.rows[1].lanes >= 2 && p.rows[1].groups >= 2, "{p:?}");
    assert_eq!(c.cfg.role, Role::None, "--dry-run changes nothing");
    assert!(!ccalls.lock().unwrap().iter().any(|c| c.starts_with("capture") || c.starts_with("tun")));
    run(&mut a, &mut c, "the temporary bond ends", |a, _| a.bonds.is_empty());

    let r = call(&mut a, &mut c, pick(&[&anchor], false));
    assert!(r.changed && r.pick.as_ref().is_some_and(|p| p.applied), "{r:?}");
    assert_eq!((c.cfg.role, c.cfg.anchor.as_deref()), (Role::Client, Some(anchor.as_str())));
    run(&mut a, &mut c, "bond to the winner", |_, c| c.bonds.first().is_some_and(|b| b.state == BondState::Up));
}

/// §18 on a client already bonded to the candidate: the anchor keeps the temporary bond beside
/// the live one without touching it; bandwidth mode measures each lane's top rate with filler,
/// and the temporary bond ends with the candidate.
#[test]
fn pick_measures_the_current_anchor_beside_the_live_bond() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(44, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(44, 20).to_string()), ..Config::default() };
    let (mut a, atun, _) = node("xanth", ip(44, 2), ip(44, 20), acfg);
    let (mut c, ctun, ccalls) = node("mac", ip(44, 1), ip(44, 10), ccfg);
    run(&mut a, &mut c, "bond up", |_, c| c.bonds.first().is_some_and(|b| b.state == BondState::Up));
    let (live_c, live_a) = (c.bonds[0].chan, a.bonds[0].chan);
    let (owner_c, owner_a) = (c.bonds[0].owner, a.bonds[0].owner);
    let anchor = ip(44, 20).to_string();
    let req =
        Request::Pick { devices: vec![anchor.clone()], mode: Some(Mode::Bandwidth), seconds: Some(2), dry_run: true };
    let (tx, rx) = mpsc::channel();
    c.call(req, tx);
    let mut got = None;
    let mut beside = false;
    run(&mut a, &mut c, "pick", |a, _| {
        beside |= a.bonds.len() == 2 && a.bonds.iter().filter(|b| b.measure).count() == 1;
        got = got.take().or_else(|| rx.try_recv().ok());
        got.is_some()
    });
    let r = got.unwrap();
    let p = r.pick.clone().unwrap_or_else(|| panic!("{r:?}"));
    assert!(beside, "the anchor held the temporary bond beside the live one");
    assert_eq!(p.winner.as_deref(), Some(anchor.as_str()), "{p:?}\n{:?}", c.logs);
    assert!(p.rows[0].lanes >= 2 && p.rows[0].score.is_some_and(|s| s > 0.0), "filler measured a rate: {p:?}");
    assert!(!p.applied && !r.changed);
    // The live bond is untouched on both sides: the same channels and sessions, still up.
    assert_eq!((c.bonds[0].chan, c.bonds[0].owner, c.bonds[0].state), (live_c, owner_c, BondState::Up));
    assert_eq!((a.bonds[0].chan, a.bonds[0].owner), (live_a, owner_a));
    assert!(!a.bonds[0].measure && a.bonds[0].addr4 == c.bonds[0].addr4);
    run(&mut a, &mut c, "the temporary bond ends", |a, _| a.bonds.len() == 1);
    assert_eq!(ccalls.lock().unwrap().iter().filter(|c| c.starts_with("tun")).count(), 1, "one TUN, the bond's");
    // The live bond still carries traffic, and no filler reached either TUN device.
    let out = udp([10, 77, 0, 2], [1, 1, 1, 1], b"after the pick");
    ctun.inject(out.clone());
    run(&mut a, &mut c, "traffic after the pick", |_, _| atun.written().contains(&out));
    let filler = |p: &Vec<u8>| p.get(12..14) == Some(&[192, 0][..]);
    assert!(!atun.written().iter().any(filler) && !ctun.written().iter().any(filler));
}

/// A channel dialed for the bond whose anchor answers hello but never `bond ok` is closed after
/// BOND_OK_WAIT and dialed again after the backoff (§17); channel up, hello and bond open are
/// logged.
#[test]
fn a_dialed_channel_without_bond_ok_times_out_and_redials() {
    let mesh = ip(45, 20);
    let ln = std::net::TcpListener::bind((mesh, PORT)).unwrap();
    let (stx, srx) = mpsc::channel();
    thread::spawn(move || {
        for s in ln.incoming().flatten() {
            let mut w = s.try_clone().unwrap();
            let hello = r#"{"t":"hello","v":2,"inst":7,"name":"mute","role":"anchor","version":"2.0.0"}"#;
            let _ = w.write_all(format!("{hello}\n").as_bytes());
            let _ = stx.send(());
            // Reads and never answers.
            thread::spawn(move || {
                let mut b = [0u8; 4096];
                while matches!(io::Read::read(&mut w, &mut b), Ok(1..)) {}
            });
            std::mem::forget(s);
        }
    });
    let ccfg = Config { role: Role::Client, anchor: Some(mesh.to_string()), ..Config::default() };
    let (mut c, _, _) = node("mac", ip(45, 1), ip(45, 10), ccfg);
    let mut p = host::Poll::default();
    let t = Instant::now();
    while !c.logs.iter().any(|l| l.contains("no bond ok within")) {
        assert!(t.elapsed() < Duration::from_secs(15), "no timeout: {:?}", c.logs);
        c.step(&mut p, Duration::from_millis(5)).unwrap();
    }
    assert!(t.elapsed() >= Duration::from_nanos(ctl::BOND_OK_WAIT as u64), "{:?}", t.elapsed());
    for want in ["up: ", "hello from mute", "bond open sent"] {
        assert!(c.logs.iter().any(|l| l.contains(want)), "{want}: {:?}", c.logs);
    }
    assert!(c.status(false).problems.iter().any(|p| p.code == problem::ANCHOR_UNREACHABLE));
    assert_eq!(c.backoff, 2 * REDIAL_MIN, "the backoff doubles");
    assert!(srx.try_recv().is_ok() && srx.try_recv().is_err(), "one dial so far");
    while srx.try_recv().is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "no redial: {:?}", c.logs);
        c.step(&mut p, Duration::from_millis(5)).unwrap();
    }
}

/// Tests run in parallel, so each daemon test owns its 127.N.0.0/16: a net shared between two
/// files' tests is a bind that fails about one run in two (the wake test met the support test on
/// 127.53.0.20). No N appears in two files.
#[test]
fn no_two_test_files_share_a_net() {
    let files = [
        ("tests.rs", include_str!("tests.rs")),
        ("tests_net.rs", include_str!("tests_net.rs")),
        ("support/tests.rs", include_str!("support/tests.rs")),
        ("ctl.rs", include_str!("ctl.rs")),
    ];
    let nets = |src: &str| -> Vec<u8> {
        let mut v: Vec<u8> = ["ip(", "Ipv4Addr::new(127, "]
            .iter()
            .flat_map(|pat| src.match_indices(pat).map(move |(i, _)| &src[i + pat.len()..]))
            .filter_map(|rest| rest.split(',').next()?.trim().parse().ok())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut owner: HashMap<u8, &str> = HashMap::new();
    for (name, src) in files {
        for n in nets(src) {
            if let Some(other) = owner.insert(n, name) {
                assert_eq!(other, name, "127.{n}.0.0/16 is used by {other} and {name}");
            }
        }
    }
}
