//! The daemon's network duties on a fake host (no root): STUN and candidates (§9.2, §9.3),
//! bottleneck groups (§8), wake (§11.4), `mesh_captured` (§5.3), `dns_local` (§7) and the anchor's
//! uplink (§6.2). Loopback addresses stand in for links; run inside `unshare -n` with `lo` up.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// What a test turns on the fake host while the daemon runs.
#[derive(Default)]
struct Knobs {
    gateways: Vec<(String, IpAddr)>,
    default: Option<IpAddr>,
    /// Time slept, added to the sleep-inclusive clock.
    slept: i64,
    dns_local: bool,
    uplink: String,
}

/// A fake TUN device: what the test writes for the daemon to read, and what the daemon wrote.
#[derive(Clone, Default)]
struct FakeTun(Arc<Mutex<Queues>>);

/// What the daemon will read, and what it wrote.
type Queues = (VecDeque<Vec<u8>>, Vec<Vec<u8>>);

impl TunDev for FakeTun {
    fn name(&self) -> &str {
        "fake0"
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let p = self.0.lock().unwrap().0.pop_front().ok_or(io::ErrorKind::WouldBlock)?;
        buf[..p.len()].copy_from_slice(&p);
        Ok(p.len())
    }

    fn write(&self, packet: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().1.push(packet.to_vec());
        Ok(())
    }
}

/// A fake host: its links, one mesh address, a fake TUN device, the knobs and a record of calls.
struct Host {
    name: String,
    links: Vec<host::Link>,
    mesh: host::Link,
    tun: FakeTun,
    knobs: Arc<Mutex<Knobs>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Platform for Host {
    fn open_tun(&mut self, _: &host::TunConfig) -> io::Result<Box<dyn TunDev>> {
        Ok(Box::new(self.tun.clone()))
    }

    fn capture_install(&mut self, _: &host::Capture) -> io::Result<()> {
        self.calls.lock().unwrap().push("capture on".into());
        Ok(())
    }

    fn capture_remove(&mut self, _: &host::Capture) -> io::Result<()> {
        self.calls.lock().unwrap().push("capture off".into());
        Ok(())
    }

    fn egress_enable(&mut self, _: &host::Egress) -> io::Result<host::EgressOn> {
        let uplink = self.knobs.lock().unwrap().uplink.clone();
        self.calls.lock().unwrap().push(format!("egress on {uplink}"));
        Ok(host::EgressOn { v4: true, v6: true, addr4: (Ipv4Addr::new(10, 77, 0, 1), 24), uplink: Some(uplink) })
    }

    fn egress_disable(&mut self, _: &host::Egress) -> io::Result<()> {
        Ok(())
    }

    fn upstream_resolver(&mut self) -> io::Result<SocketAddr> {
        Ok("127.0.0.53:53".parse().unwrap())
    }

    fn links(&mut self, _: &[(IpAddr, u8)]) -> io::Result<(Vec<host::Link>, Vec<host::Link>)> {
        Ok((self.links.clone(), vec![self.mesh.clone()]))
    }

    fn bind(&mut self, link: &host::Link, mesh: bool) -> io::Result<UdpSocket> {
        UdpSocket::bind((link.addr, if mesh { PORT } else { 0 }))
    }

    fn hostname(&self) -> String {
        self.name.clone()
    }

    fn gateways(&mut self) -> Vec<(String, IpAddr)> {
        self.knobs.lock().unwrap().gateways.clone()
    }

    fn default_addr(&mut self) -> Option<IpAddr> {
        self.knobs.lock().unwrap().default
    }

    fn boot(&self) -> Option<i64> {
        Some(host::boot() + self.knobs.lock().unwrap().slept)
    }

    fn dns_local(&self) -> bool {
        self.knobs.lock().unwrap().dns_local
    }
}

/// A node.
struct Node {
    d: Daemon,
    tun: FakeTun,
    knobs: Arc<Mutex<Knobs>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Node {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

/// 127.`net`.0.`host`.
fn ip(net: u8, host: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, net, 0, host))
}

/// A node named `name` with links `links` (interface name, address) and mesh address `mesh`.
fn node(name: &str, links: &[(&str, IpAddr)], mesh: IpAddr, cfg: Config) -> Node {
    let l = |name: &str, addr| host::Link { index: 1, name: name.into(), addr, label: name.into(), metered: false };
    let (tun, knobs, calls) = (FakeTun::default(), Arc::new(Mutex::new(Knobs::default())), Arc::default());
    let fake = Host {
        name: name.into(),
        links: links.iter().map(|&(n, a)| l(n, a)).collect(),
        mesh: l("mesh0", mesh),
        tun: tun.clone(),
        knobs: knobs.clone(),
        calls: Arc::clone(&calls),
    };
    let mut d = Daemon::new("2.0.0", None, cfg, Box::new(fake)).unwrap();
    (d.loopback, d.captive_url) = (true, None);
    Node { d, tun, knobs, calls }
}

/// Steps every node until `done` or 15 s.
fn run(nodes: &mut [&mut Node], what: &str, mut done: impl FnMut(&[&mut Node]) -> bool) {
    let mut polls: Vec<host::Poll> = nodes.iter().map(|_| host::Poll::default()).collect();
    let t = Instant::now();
    while !done(nodes) {
        let st: Vec<_> = nodes.iter().map(|n| n.d.status(false)).collect();
        assert!(t.elapsed() < Duration::from_secs(15), "{what}: timed out\n{st:#?}");
        for (n, p) in nodes.iter_mut().zip(&mut polls) {
            n.d.step(p, Duration::from_millis(2)).unwrap();
        }
    }
}

/// A Binding success response for transaction `t` mapping `a` (RFC 8489 XOR-MAPPED-ADDRESS).
fn stun_response(t: &[u8; 12], a: SocketAddr) -> Vec<u8> {
    let mut key = link::STUN_COOKIE.to_be_bytes().to_vec();
    key.extend_from_slice(t);
    let IpAddr::V4(v4) = a.ip() else { unreachable!() };
    let mut v = vec![0, 1];
    v.extend_from_slice(&(a.port() ^ (link::STUN_COOKIE >> 16) as u16).to_be_bytes());
    v.extend(v4.octets().iter().zip(&key).map(|(x, k)| x ^ k));
    let mut b = vec![1, 1, 0, 12];
    b.extend_from_slice(&key);
    b.extend_from_slice(&[0, 0x20, 0, 8]);
    b.extend_from_slice(&v);
    b
}

/// A STUN server on `at` that maps each asker by `map` (its address to a public IP, keeping the
/// port); returns its address and the number of requests it answered.
fn stun_server(at: IpAddr, map: fn(IpAddr) -> IpAddr) -> (SocketAddr, Arc<AtomicUsize>) {
    let u = UdpSocket::bind((at, 0)).unwrap();
    let (addr, asked) = (u.local_addr().unwrap(), Arc::new(AtomicUsize::new(0)));
    let n = asked.clone();
    thread::spawn(move || {
        let mut b = [0u8; 1500];
        while let Ok((len, from)) = u.recv_from(&mut b) {
            if let Some((t, None)) = link::stun_parse(&b[..len]) {
                n.fetch_add(1, Ordering::SeqCst);
                let _ = u.send_to(&stun_response(&t, SocketAddr::new(map(from.ip()), from.port())), from);
            }
        }
    });
    (addr, asked)
}

/// Every asker shows as 203.0.113.7.
fn one_public(_: IpAddr) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))
}

#[test]
fn stun_learns_each_links_mapping_and_offers_it() {
    let (server, asked) = stun_server(ip(50, 99), one_public);
    let cfg = Config { stun: vec![server.to_string()], ..Config::default() };
    let mut n = node("mac", &[("lo-a", ip(50, 1))], ip(50, 10), cfg);
    run(&mut [&mut n], "a mapping", |n| n[0].d.socks.iter().any(|s| !s.stun.mapped.is_empty()));
    let offer = n.d.offer();
    let local = n.d.socks.iter().find(|s| s.id != 0).unwrap().local;
    let public = SocketAddr::new(one_public(local.ip()), local.port());
    assert_eq!(offer.links[0].cands, vec![local, public], "the local address, then the mapping (§9.3)");
    assert_eq!(asked.load(Ordering::SeqCst), 1, "one request per server and link");

    // §9.2: an answer from a server not queried, or a forged one, is not taken.
    let forger = UdpSocket::bind((ip(50, 98), 0)).unwrap();
    let t = [7u8; 12];
    forger.send_to(&stun_response(&t, "198.51.100.66:1".parse().unwrap()), local).unwrap();
    let until = Instant::now() + Duration::from_millis(200);
    run(&mut [&mut n], "the forgery read", |_| Instant::now() >= until);
    assert_eq!(n.d.offer().links[0].cands, vec![local, public]);

    // Without data flowing, no refresh comes on its own.
    let until = Instant::now() + Duration::from_millis(2500);
    run(&mut [&mut n], "a scan later", |_| Instant::now() >= until);
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

#[test]
fn stun_mappings_reach_the_peer_and_the_bond_comes_up() {
    let (server, _) = stun_server(ip(51, 99), one_public);
    let acfg = Config {
        role: Role::Anchor,
        allow: vec![ip(51, 10).to_string()],
        stun: vec![server.to_string()],
        ..Config::default()
    };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(51, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(51, 2))], ip(51, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(51, 1))], ip(51, 10), ccfg);
    run(&mut [&mut a, &mut c], "bond up, the anchor mapped", |n| {
        n[1].d.bonds.first().is_some_and(|b| b.state == BondState::Up)
            && n[1].d.peer_cands.contains(&one_public(ip(51, 2)))
    });
    // The anchor's public candidate is where the mesh's underlay would go (§5.3).
    let p = udp_to(SocketAddr::new(one_public(ip(51, 2)), 41641));
    c.tun.0.lock().unwrap().0.push_back(p);
    run(&mut [&mut a, &mut c], "mesh_captured", |n| n[1].d.mesh_captured.is_some());
    assert!(c.d.capture.is_none());
    assert!(c.d.status(false).problems.iter().any(|p| p.code == problem::MESH_CAPTURED));
}

/// An IPv4/UDP packet from the client's bond address to `to`.
fn udp_to(to: SocketAddr) -> Vec<u8> {
    let IpAddr::V4(dst) = to.ip() else { unreachable!() };
    let mut p = vec![0x45, 0, 0, 32, 0, 0, 0, 0, 64, 17, 0, 0, 10, 77, 0, 2];
    p.extend_from_slice(&dst.octets());
    p.extend_from_slice(&5000u16.to_be_bytes());
    p.extend_from_slice(&to.port().to_be_bytes());
    p.extend_from_slice(&[0, 12, 0, 0, 1, 2, 3, 4]);
    p
}

#[test]
fn links_behind_one_gateway_or_one_public_address_share_a_group() {
    // a and b share a gateway; d shows a's public IPv4; c is alone.
    fn map(from: IpAddr) -> IpAddr {
        let IpAddr::V4(v) = from else { unreachable!() };
        IpAddr::V4(match v.octets()[3] {
            1 | 4 => Ipv4Addr::new(203, 0, 113, 7),
            3 => Ipv4Addr::new(198, 51, 100, 9),
            _ => Ipv4Addr::new(192, 0, 2, 50),
        })
    }
    let (server, _) = stun_server(ip(52, 99), map);
    let cfg = Config { stun: vec![server.to_string()], ..Config::default() };
    let links = [("a", ip(52, 1)), ("b", ip(52, 2)), ("c", ip(52, 3)), ("d", ip(52, 4))];
    let mut n = node("mac", &links, ip(52, 10), cfg);
    let gw: IpAddr = "192.168.1.1".parse().unwrap();
    {
        let mut k = n.knobs.lock().unwrap();
        k.gateways = vec![("a".into(), gw), ("b".into(), gw), ("c".into(), "10.0.9.1".parse().unwrap())];
        k.default = Some(ip(52, 3)); // c carries the mesh
    }
    run(&mut [&mut n], "every link mapped", |n| n[0].d.socks.iter().filter(|s| !s.stun.mapped.is_empty()).count() == 4);
    let by_name = |n: &Node| -> Vec<(String, u8)> {
        n.d.socks.iter().zip(n.d.groups()).map(|(s, g)| (s.link.name.clone(), g)).collect()
    };
    let groups = by_name(&n);
    let id = |name: &str| n.d.socks.iter().find(|s| s.link.name == name).unwrap().id;
    let want = |name: &str| groups.iter().find(|(x, _)| x == name).unwrap().1;
    assert_eq!(want("a"), id("a").min(id("b")).min(id("d")), "{groups:?}");
    assert_eq!((want("b"), want("d")), (want("a"), want("a")), "a gateway and a public address join groups");
    assert_eq!(want("c"), id("c"), "c is alone: {groups:?}");
    assert_eq!(want("mesh0"), want("c"), "the mesh lane takes the group of the link carrying the mesh");
    let offer = n.d.offer();
    assert_eq!(offer.links.iter().filter(|l| l.group == want("a")).count(), 3, "{offer:?}");
    let st = n.d.status(false);
    assert_eq!(st.links.iter().filter(|l| l.group == want("a")).count(), 3, "{st:?}");
}

#[test]
fn a_wake_re_punches_and_asks_stun_again() {
    let (server, asked) = stun_server(ip(53, 99), one_public);
    let acfg = Config { role: Role::Anchor, allow: vec![ip(53, 10).to_string()], ..Config::default() };
    let ccfg = Config {
        role: Role::Client,
        anchor: Some(ip(53, 20).to_string()),
        stun: vec![server.to_string()],
        ..Config::default()
    };
    let mut a = node("xanth", &[("eth0", ip(53, 2))], ip(53, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(53, 1))], ip(53, 10), ccfg);
    // A loaded test run may stall a loop past WAKE_JUMP or a STUN answer past STUN_WAIT, which
    // asks again on its own: the wake is told by the counts after the lid closes, not by totals.
    run(&mut [&mut a, &mut c], "bond up and mapped", |n| {
        n[1].d.bonds.first().is_some_and(|b| b.state == BondState::Up) && asked.load(Ordering::SeqCst) >= 1
    });
    let wakes = |d: &Daemon| d.logs.iter().filter(|l| l.starts_with("woke from sleep")).count();
    let (woke, before) = (wakes(&c.d), asked.load(Ordering::SeqCst));
    c.knobs.lock().unwrap().slept = 30 * SEC; // the lid closes for 30 s
    run(&mut [&mut a, &mut c], "the wake", |n| wakes(&n[1].d) > woke);
    run(&mut [&mut a, &mut c], "STUN again", |_| asked.load(Ordering::SeqCst) > before);
    // The bond carries on: keys kept, lanes re-punched. One datagram may be lost while the lanes
    // re-punch, as on any network, so it is sent again until one arrives.
    let p = udp_to("1.1.1.1:53".parse().unwrap());
    let mut next = Instant::now();
    run(&mut [&mut a, &mut c], "traffic after the wake", |n| {
        if Instant::now() >= next {
            n[1].tun.0.lock().unwrap().0.push_back(p.clone());
            next = Instant::now() + Duration::from_millis(500);
        }
        n[0].tun.0.lock().unwrap().1.contains(&p)
    });
    assert!(c.d.bonds[0].state == BondState::Up);
}

/// §8, §16: the client's `links` marks the link carrying its mesh's underlay, and the anchor's
/// engine puts the mesh lane in that link's group (its `default`).
#[test]
fn the_link_carrying_the_mesh_is_offered_and_the_anchor_groups_the_mesh_lane_by_it() {
    let mut c = node(
        "mac",
        &[("a", ip(59, 1)), ("b", ip(59, 2))],
        ip(59, 10),
        Config { stun: Vec::new(), ..Config::default() },
    );
    c.knobs.lock().unwrap().default = Some(ip(59, 2)); // b carries the mesh
    run(&mut [&mut c], "links and the default route", |n| n[0].d.socks.len() == 3 && n[0].d.default_addr.is_some());
    let offer = c.d.offer();
    let id_b = c.d.socks.iter().find(|s| s.link.name == "b").unwrap().id;
    let marked: Vec<u8> = offer.links.iter().filter(|l| l.mesh).map(|l| l.id).collect();
    assert_eq!(marked, vec![id_b], "{offer:?}");
    let a = node("xanth", &[("eth0", ip(59, 3))], ip(59, 20), Config { role: Role::Anchor, ..Config::default() });
    let m = a.d.peer_links(&offer);
    let defaults: Vec<u8> = m.links.iter().filter(|l| l.default).map(|l| l.id).collect();
    assert_eq!(defaults, vec![id_b], "the anchor's engine joins the mesh lane to b's group: {m:?}");
}

#[test]
fn mesh_traffic_in_the_tun_removes_capture_for_the_bond() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(54, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(54, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(54, 2))], ip(54, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(54, 1))], ip(54, 10), ccfg);
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some());
    // An ordinary packet passes; one for the mesh's 100.64.0.0/10 ends capture (§5.3).
    let fine = udp_to("1.1.1.1:53".parse().unwrap());
    c.tun.0.lock().unwrap().0.push_back(fine.clone());
    run(&mut [&mut a, &mut c], "egress", |n| n[0].tun.0.lock().unwrap().1.contains(&fine));
    assert!(c.d.mesh_captured.is_none());
    c.tun.0.lock().unwrap().0.push_back(udp_to("100.101.102.103:41641".parse().unwrap()));
    run(&mut [&mut a, &mut c], "mesh_captured", |n| n[1].d.capture.is_none());
    assert_eq!(c.calls().last().map(String::as_str), Some("capture off"));
    let st = c.d.status(false);
    let p = st.problems.iter().find(|p| p.code == problem::MESH_CAPTURED).unwrap_or_else(|| panic!("{st:?}"));
    assert!(p.msg.contains("100.101.102.103"), "{p:?}");
    // Capture stays off while the bond lasts; `client off` clears the problem.
    let until = Instant::now() + Duration::from_millis(300);
    run(&mut [&mut a, &mut c], "a while", |_| Instant::now() >= until);
    assert!(c.d.capture.is_none() && c.calls().iter().filter(|x| *x == "capture on").count() == 1);
    assert!(c.d.answer(Request::ClientOff).changed);
    assert!(!c.d.status(false).problems.iter().any(|p| p.code == problem::MESH_CAPTURED));
}

#[test]
fn dns_local_is_raised_when_capture_leaves_dns_alone() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(55, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(55, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(55, 2))], ip(55, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(55, 1))], ip(55, 10), ccfg);
    c.knobs.lock().unwrap().dns_local = true; // no systemd-resolved
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some());
    assert!(c.d.status(false).problems.iter().any(|p| p.code == problem::DNS_LOCAL));
    assert!(c.d.answer(Request::ClientOff).changed);
    assert!(!c.d.status(false).problems.iter().any(|p| p.code == problem::DNS_LOCAL));
}

#[test]
fn the_anchor_follows_its_uplink() {
    let acfg = Config { role: Role::Anchor, ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(56, 2)), ("wlan0", ip(56, 3))], ip(56, 20), acfg);
    (a.knobs.lock().unwrap().default, a.knobs.lock().unwrap().uplink) = (Some(ip(56, 2)), "eth0".into());
    run(&mut [&mut a], "egress on", |n| n[0].d.default_addr.is_some() && n[0].d.egress.is_some());
    assert_eq!(a.calls(), vec!["egress on eth0".to_string()]);
    // The default route moves to Wi-Fi: within a LINK_SCAN, egress follows (§6.2).
    (a.knobs.lock().unwrap().default, a.knobs.lock().unwrap().uplink) = (Some(ip(56, 3)), "wlan0".into());
    run(&mut [&mut a], "egress follows", |n| n[0].calls().contains(&"egress on wlan0".to_string()));
    assert_eq!(a.d.status(false).anchor.unwrap().uplink.as_deref(), Some("wlan0"));
    // No default route, or one off my links (a mesh exit node), changes nothing.
    a.knobs.lock().unwrap().default = Some("100.100.1.1".parse().unwrap());
    let until = Instant::now() + Duration::from_millis(2500);
    run(&mut [&mut a], "a scan later", |_| Instant::now() >= until);
    assert_eq!(a.calls().len(), 2);
}

#[test]
fn gateways_are_read_from_proc_route_and_netstat() {
    let proc = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n\
                usb0\t00000000\t010AA8C0\t0003\t0\t0\t700\t00000000\t0\t0\t0\n";
    let want: Vec<(String, IpAddr)> =
        vec![("wlan0".into(), "192.168.1.1".parse().unwrap()), ("usb0".into(), "192.168.10.1".parse().unwrap())];
    assert_eq!(plat::proc_route(proc), want);
    let netstat = "Routing tables\n\nInternet:\nDestination        Gateway            Flags               Netif Expire\n\
                   default            192.168.1.1        UGScg                 en0       \n\
                   default            172.20.10.1        UGScIg                en7       \n\
                   127                127.0.0.1          UCS                   lo0       \n";
    let want: Vec<(String, IpAddr)> =
        vec![("en0".into(), "192.168.1.1".parse().unwrap()), ("en7".into(), "172.20.10.1".parse().unwrap())];
    assert_eq!(plat::netstat_defaults(netstat), want);
    let route = "IPv4 Route Table\n===\nActive Routes:\n\
                 Network Destination        Netmask          Gateway       Interface  Metric\n          \
                 0.0.0.0          0.0.0.0      192.168.1.1     192.168.1.23     25\n          \
                 0.0.0.0          0.0.0.0      172.20.10.1      172.20.10.2     35\n          \
                 0.0.0.0          0.0.0.0         On-link        10.77.0.2      5\n        \
                 127.0.0.0        255.0.0.0         On-link         127.0.0.1    331\n\
                 Persistent Routes:\n  Network Address          Netmask  Gateway Address  Metric\n          \
                 0.0.0.0          0.0.0.0      192.168.1.1  Default\n";
    let want: Vec<(String, IpAddr)> = vec![
        ("192.168.1.23".into(), "192.168.1.1".parse().unwrap()),
        ("172.20.10.2".into(), "172.20.10.1".parse().unwrap()),
    ];
    assert_eq!(plat::route_print_defaults(route), want);
}

#[test]
fn both_engines_see_the_clients_groups() {
    let acfg =
        Config { role: Role::Anchor, allow: vec![ip(58, 10).to_string()], stun: Vec::new(), ..Config::default() };
    let ccfg =
        Config { role: Role::Client, anchor: Some(ip(58, 20).to_string()), stun: Vec::new(), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(58, 3))], ip(58, 20), acfg);
    let mut c = node("mac", &[("a", ip(58, 1)), ("b", ip(58, 2))], ip(58, 10), ccfg);
    let gw: IpAddr = "192.168.1.1".parse().unwrap();
    c.knobs.lock().unwrap().gateways = vec![("a".into(), gw), ("b".into(), gw)];
    c.knobs.lock().unwrap().default = Some(ip(58, 2)); // b carries the mesh
    let lanes = |d: &Daemon| d.bonds.first().and_then(|b| b.session.as_ref()).map(|s| s.snapshot().lanes);
    run(&mut [&mut a, &mut c], "both links' lanes", |n| {
        let direct = |d: &Daemon| lanes(d).map_or(0, |l| l.iter().filter(|l| l.id != 0).count());
        n[1].d.capture.is_some() && direct(&n[1].d) == 2 && direct(&n[0].d) == 2
    });
    // The engines group lanes by label (§8): one gateway, one label on each side.
    let mine: Vec<String> = lanes(&c.d).unwrap().iter().filter(|l| l.id != 0).map(|l| l.local.clone()).collect();
    assert_eq!(mine, ["a", "a"], "the client labels both links after the group's lowest");
    let theirs: Vec<String> = lanes(&a.d).unwrap().iter().filter(|l| l.id != 0).map(|l| l.remote.clone()).collect();
    assert_eq!(theirs[0], theirs[1], "the anchor labels the client's links by their offered group");
    assert!(c.d.status(false).bond.unwrap().lanes.iter().filter(|l| l.id != 0).all(|l| l.group == 1));
    // The anchor shows every lane in the client's group, the mesh lane too: b carries the mesh.
    let shown = a.d.status(false).anchor.unwrap().bonds[0].lanes.clone();
    assert!(shown.iter().any(|l| l.id == 0) && shown.iter().all(|l| l.group == 1), "{shown:?}");
}

#[test]
fn a_gateway_keyed_by_the_interface_address_groups_too() {
    let links = [("Wi-Fi", ip(57, 1)), ("Ethernet", ip(57, 2))];
    let mut n = node("pc", &links, ip(57, 10), Config { stun: Vec::new(), ..Config::default() });
    let gw: IpAddr = "192.168.1.1".parse().unwrap();
    n.knobs.lock().unwrap().gateways = vec![(ip(57, 1).to_string(), gw), (ip(57, 2).to_string(), gw)];
    run(&mut [&mut n], "links", |n| n[0].d.socks.len() == 3);
    let groups: Vec<u8> = n.d.offer().links.iter().map(|l| l.group).collect();
    assert_eq!(groups, vec![1, 1], "Windows keys a gateway by the interface's address");
}

/// A loop turn's lane sends go out per socket in their order (sendmmsg on Linux); a datagram the
/// OS refuses is dropped, as a failed send_to dropped it, and the ones after it still go.
#[test]
fn send_batch_sends_in_order_past_a_refused_datagram() {
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let to = rx.local_addr().unwrap();
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    // An IPv6 destination on an IPv4 socket: refused (EAFNOSUPPORT).
    let refused = SocketAddr::from((Ipv6Addr::LOCALHOST, to.port()));
    let msgs: [(SocketAddr, &[u8]); 4] = [(to, b"one"), (refused, b"lost"), (to, b"three"), (to, b"four")];
    assert_eq!(plat::send_batch(&tx, &msgs), 3 + 5 + 4);
    let mut buf = [0u8; 16];
    for want in [&b"one"[..], b"three", b"four"] {
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], want);
    }
}
