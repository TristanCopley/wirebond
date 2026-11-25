//! The daemon's network duties on a fake host (no root): STUN and candidates (§9.2, §9.3),
//! bottleneck groups (§8), wake (§11.4), `mesh_captured` (§5.3), `dns_local` (§7) and the anchor's
//! uplink (§6.2). Loopback addresses stand in for links; run inside `unshare -n` with `lo` up.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::testkit::{FakeTun, ip};
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
    /// Why enabling egress fails, when it does, and why it enables none, when it does that.
    egress_err: Option<String>,
    egress_why: Option<String>,
    /// The anchor's address that enabling egress reports, where it is not 10.77.0.1/24 (Windows'
    /// sharing moves it to 192.168.137.1/24, §6.2).
    egress_addr4: Option<(Ipv4Addr, u8)>,
    /// Egress without IPv6, as Windows' sharing gives it.
    egress_v4_only: bool,
    /// Why capture cannot be installed, when it cannot, and how often it was tried.
    capture_err: Option<String>,
    captures: usize,
    /// The label of every link, where the links are not labelled after their interfaces.
    label: Option<String>,
    /// Why the TUN device does not open, when it does not, and how often it opened.
    tun_err: Option<String>,
    tuns: usize,
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
        let mut k = self.knobs.lock().unwrap();
        if let Some(e) = k.tun_err.clone() {
            return Err(io::Error::other(e));
        }
        k.tuns += 1;
        Ok(Box::new(self.tun.clone()))
    }

    fn capture_install(&mut self, _: &host::Capture) -> io::Result<()> {
        let mut k = self.knobs.lock().unwrap();
        k.captures += 1;
        if let Some(e) = k.capture_err.clone() {
            return Err(io::Error::other(e));
        }
        self.calls.lock().unwrap().push("capture on".into());
        Ok(())
    }

    fn capture_remove(&mut self, _: &host::Capture) -> io::Result<()> {
        self.calls.lock().unwrap().push("capture off".into());
        Ok(())
    }

    fn egress_enable(&mut self, _: &host::Egress) -> io::Result<host::EgressOn> {
        let uplink = self.knobs.lock().unwrap().uplink.clone();
        if let Some(e) = self.knobs.lock().unwrap().egress_err.clone() {
            return Err(io::Error::other(e));
        }
        let addr4 = self.knobs.lock().unwrap().egress_addr4.unwrap_or((Ipv4Addr::new(10, 77, 0, 1), 24));
        if let Some(why) = self.knobs.lock().unwrap().egress_why.clone() {
            return Ok(host::EgressOn { v4: false, v6: false, addr4, uplink: None, why: Some(why) });
        }
        self.calls.lock().unwrap().push(format!("egress on {uplink}"));
        let v6 = !self.knobs.lock().unwrap().egress_v4_only;
        Ok(host::EgressOn { v4: true, v6, addr4, uplink: Some(uplink), why: None })
    }

    fn egress_disable(&mut self, _: &host::Egress) -> io::Result<()> {
        Ok(())
    }

    fn upstream_resolver(&mut self) -> io::Result<SocketAddr> {
        Ok("127.0.0.53:53".parse().unwrap())
    }

    fn links(&mut self, _: &[(IpAddr, u8)]) -> io::Result<(Vec<host::Link>, Vec<host::Link>)> {
        let label = self.knobs.lock().unwrap().label.clone();
        let relabel =
            |l: &host::Link| host::Link { label: label.clone().unwrap_or_else(|| l.label.clone()), ..l.clone() };
        Ok((self.links.iter().map(relabel).collect(), vec![self.mesh.clone()]))
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
        if t.elapsed() >= Duration::from_secs(15) {
            let st: Vec<_> = nodes.iter().map(|n| n.d.status(false)).collect();
            panic!("{what}: timed out\n{st:#?}");
        }
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
    c.tun.inject(p);
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
    let wakes = |d: &Daemon| d.logs.iter().filter(|l| l.contains("Z: woke from sleep")).count();
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
            n[1].tun.inject(p.clone());
            next = Instant::now() + Duration::from_millis(500);
        }
        n[0].tun.written().contains(&p)
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
    c.tun.inject(fine.clone());
    run(&mut [&mut a, &mut c], "egress", |n| n[0].tun.written().contains(&fine));
    assert!(c.d.mesh_captured.is_none());
    c.tun.inject(udp_to("100.101.102.103:41641".parse().unwrap()));
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

/// §5.2, §7: an anchor without egress runs no DNS relay, so a client that keeps its own routes
/// keeps its own resolver too: capture names no DNS address, and `dns_local` is not raised for it.
#[test]
fn a_client_of_an_anchor_without_egress_leaves_dns_alone() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(70, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(70, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(70, 2))], ip(70, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(70, 1))], ip(70, 10), ccfg);
    a.knobs.lock().unwrap().egress_err = Some("sharing is off".into());
    c.knobs.lock().unwrap().dns_local = true; // what Linux answers when it was given no address
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some());
    let cap = c.d.capture.clone().unwrap();
    assert_eq!((cap.routes, cap.dns), (host::Routes::BondOnly, None), "the bond's subnets only, DNS left alone");
    let st = c.d.status(false);
    assert!(st.problems.iter().any(|p| p.code == problem::NO_EGRESS), "{st:?}");
    assert!(st.problems.iter().all(|p| p.code != problem::DNS_LOCAL), "{st:?}");
}

/// §3.2, §6.2: a bond's terms are given when it opens. An anchor whose egress comes up later (its
/// sharing worked on a later try) closes the bonds opened without it, so each reopens with the
/// new terms: the client that had the bond's subnets only and its own DNS now captures everything
/// and asks the anchor's relay. Bonds whose terms did not change are left alone.
#[test]
fn egress_that_appears_later_reopens_the_bonds_with_it() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(75, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(75, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(75, 2))], ip(75, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(75, 1))], ip(75, 10), ccfg);
    a.knobs.lock().unwrap().egress_err = Some("sharing is off".into());
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some());
    assert_eq!(c.d.capture.as_ref().map(|cap| cap.routes), Some(host::Routes::BondOnly));
    assert_eq!((c.d.bonds[0].egress, a.d.bonds[0].egress), (Egress::None, Egress::None));
    // Egress tried again with nothing new: the bond stays as it is.
    let (owner, oks) = (c.d.bonds[0].owner, |n: &Node| n.d.logs.iter().filter(|l| l.contains(": bond ok: ")).count());
    a.d.anchor_on();
    let until = Instant::now() + Duration::from_millis(300);
    run(&mut [&mut a, &mut c], "a moment", |_| Instant::now() >= until);
    assert_eq!((c.d.bonds[0].owner, oks(&c), a.d.bonds.len()), (owner, 1, 1), "nothing changed, nothing closed");
    // Sharing works on the next try.
    a.knobs.lock().unwrap().egress_err = None;
    a.d.anchor_on();
    run(&mut [&mut a, &mut c], "the bond reopens", |n| oks(n[1]) == 2 && n[1].d.capture.is_some());
    let cap = c.d.capture.clone().unwrap();
    assert_eq!((cap.routes, cap.dns), (host::Routes::All, Some("10.77.0.1".parse().unwrap())));
    assert_eq!((c.d.bonds[0].egress, a.d.bonds[0].egress), (Egress::Both, Egress::Both));
    assert!(told_and_new(&c, owner), "the anchor's `bond close` ended the old bond: {:?}", c.d.logs);
    let st = c.d.status(false);
    assert!(st.problems.iter().all(|p| p.code != problem::NO_EGRESS), "{st:?}");
    let calls = c.calls();
    assert_eq!(calls, ["capture on", "capture off", "capture on"], "the old capture went before the new one");
}

/// Whether client `c` was told by the anchor's `bond close` (§17), which is what a client of an
/// older release acts on too, and has since opened a new bond, not the one of session `owner`.
fn told_and_new(c: &Node, owner: u64) -> bool {
    c.d.logs.iter().any(|l| l.ends_with(" closed the bond")) && c.d.bonds.first().is_some_and(|b| b.owner != owner)
}

/// §3.2 step 3, §5.2: an anchor whose egress goes (its sharing failed on a later try) closes the
/// bonds opened with it, so each reopens without egress: its client routes only the bond's
/// subnets again, keeps its own DNS, and status says `no_egress`.
#[test]
fn egress_that_goes_reopens_the_bonds_without_it() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(79, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(79, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(79, 2))], ip(79, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(79, 1))], ip(79, 10), ccfg);
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some());
    assert_eq!(c.d.capture.as_ref().map(|cap| cap.routes), Some(host::Routes::All));
    let (owner, oks) = (c.d.bonds[0].owner, |n: &Node| n.d.logs.iter().filter(|l| l.contains(": bond ok: ")).count());
    a.knobs.lock().unwrap().egress_err = Some("sharing stopped".into());
    a.d.anchor_on();
    run(&mut [&mut a, &mut c], "the bond reopens", |n| oks(n[1]) == 2 && n[1].d.capture.is_some());
    assert!(told_and_new(&c, owner), "the anchor's `bond close` ended the old bond: {:?}", c.d.logs);
    let cap = c.d.capture.clone().unwrap();
    assert_eq!((cap.routes, cap.dns), (host::Routes::BondOnly, None), "the bond's subnets only, DNS left alone");
    assert_eq!((c.d.bonds[0].egress, a.d.bonds[0].egress), (Egress::None, Egress::None));
    assert_eq!(c.d.bonds[0].addr4, Some(Ipv4Addr::new(10, 77, 0, 2)), "the subnet did not move");
    let st = c.d.status(false);
    assert!(st.problems.iter().any(|p| p.code == problem::NO_EGRESS), "{st:?}");
    assert_eq!(c.calls(), ["capture on", "capture off", "capture on"], "the old capture went before the new one");
}

/// §3.2 step 3, §6.2, §17: Windows' sharing moves the bond subnet when it starts, from
/// 10.77.0.0/24 to 192.168.137.0/24. The bond opened in the old subnet without egress closes and
/// reopens in the new one, and the client follows: its bond address keeps its host number in the
/// new subnet, it routes everything, it asks the anchor's new address for DNS, and status gives
/// that address for Moonlight.
#[test]
fn sharing_that_moves_the_subnet_reopens_the_bond_there() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(76, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(76, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(76, 2))], ip(76, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(76, 1))], ip(76, 10), ccfg);
    a.knobs.lock().unwrap().egress_why = Some("sharing is refused".into());
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some());
    let (owner, cap) = (c.d.bonds[0].owner, c.d.capture.clone().unwrap());
    assert_eq!(c.d.bonds[0].addr4, Some(Ipv4Addr::new(10, 77, 0, 2)));
    let old = (host::Routes::BondOnly, Some((Ipv4Addr::new(10, 77, 0, 0), 24)), None);
    assert_eq!((cap.routes, cap.bond4, cap.dns), old);
    sharing_starts(&a);
    a.d.anchor_on();
    let moved = Some(Ipv4Addr::new(192, 168, 137, 2));
    run(&mut [&mut a, &mut c], "the bond reopens in the new subnet", |n| {
        n[1].d.capture.is_some() && n[1].d.bonds.first().is_some_and(|b| b.addr4 == moved)
    });
    let cap = c.d.capture.clone().unwrap();
    let new = (host::Routes::All, Some((Ipv4Addr::new(192, 168, 137, 0), 24)), Some("192.168.137.1".parse().unwrap()));
    assert_eq!((cap.routes, cap.bond4, cap.dns), new);
    assert_eq!((a.d.bonds.len(), a.d.bonds[0].addr4, a.d.bonds[0].egress), (1, moved, Egress::V4));
    assert!(told_and_new(&c, owner), "the anchor's `bond close` ended the old bond: {:?}", c.d.logs);
    let st = c.d.status(false);
    assert_eq!(st.bond.and_then(|b| b.anchor_addr4).as_deref(), Some("192.168.137.1"), "Moonlight's host");
    assert_eq!(c.calls(), ["capture on", "capture off", "capture on"], "the old capture went before the new one");
}

/// Sends a packet each way through the bond of anchor `a` and client `c` (in 10.77.0.0/24), and
/// holds that the client hears the anchor meanwhile: the two sessions open each other's packets.
fn carries_both_ways(a: &mut Node, c: &mut Node) {
    let heard = |c: &Node| c.d.bonds[0].session.as_ref().and_then(|s| bond::heard(&s.snapshot(), true));
    let was = heard(c);
    let up = udp_to("1.1.1.1:53".parse().unwrap());
    c.tun.inject(up.clone());
    run(&mut [&mut *a, &mut *c], "client to anchor", |n| n[0].tun.written().contains(&up));
    let mut down = up.clone();
    down[12..16].copy_from_slice(&[1, 1, 1, 1]);
    down[16..20].copy_from_slice(&[10, 77, 0, 2]);
    a.tun.inject(down.clone());
    run(&mut [&mut *a, &mut *c], "anchor to client", |n| n[1].tun.written().contains(&down));
    assert!(heard(c) > was, "the client hears the anchor");
}

/// Windows' sharing starts on anchor `a`'s next try (§6.2): IPv4 only, and the adapter moves to
/// 192.168.137.1/24.
fn sharing_starts(a: &Node) {
    let mut k = a.knobs.lock().unwrap();
    (k.egress_err, k.egress_why) = (None, None);
    (k.egress_addr4, k.egress_v4_only) = (Some((Ipv4Addr::new(192, 168, 137, 1), 24)), true);
}

/// Steps an anchor `a` and its client `c` until `done` or 15 s, the client kept from redialing: a
/// control channel that went down stays down (§17: mesh down, lanes up).
fn run_without_redial(a: &mut Node, c: &mut Node, what: &str, mut done: impl FnMut(&Node, &Node) -> bool) {
    let (mut pa, mut pc) = (host::Poll::default(), host::Poll::default());
    let t = Instant::now();
    while !done(a, c) {
        assert!(t.elapsed() < Duration::from_secs(15), "{what}: timed out\n{:#?}", c.d.status(false));
        c.d.redial_at = Mono::MAX;
        c.d.step(&mut pc, Duration::from_millis(2)).unwrap();
        a.d.step(&mut pa, Duration::from_millis(2)).unwrap();
    }
}

/// Closes the control channel under the bond of `a` and `c`, and steps them until neither side's
/// bond has one; the lanes carry on.
fn channel_down(a: &mut Node, c: &mut Node) {
    for p in c.d.chans.values() {
        p.chan.close();
    }
    run_without_redial(a, c, "the channel goes down", |a, c| {
        a.d.bonds.first().is_some_and(|b| b.chan.is_none()) && c.d.bonds.first().is_some_and(|b| b.chan.is_none())
    });
}

/// §3.2 step 3, §17 (mesh down, lanes up): an anchor whose egress appears while a bond's channel
/// is down cannot tell its client, so it keeps the bond, and the client keeps its capture over
/// the direct lanes. When the channel returns, the anchor's answer to `bond open` gives the same
/// bond the new egress: the client keeps the session and installs capture again by it, with all
/// routes and the anchor's DNS.
#[test]
fn egress_that_appears_while_the_channel_is_down_comes_when_it_returns() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(77, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(77, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(77, 2))], ip(77, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(77, 1))], ip(77, 10), ccfg);
    a.knobs.lock().unwrap().egress_err = Some("sharing is off".into());
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some() && n[1].d.bonds[0].state == BondState::Up);
    let owners = |a: &Node, c: &Node| (a.d.bonds.first().map(|b| b.owner), c.d.bonds.first().map(|b| b.owner));
    let before = owners(&a, &c);
    channel_down(&mut a, &mut c);
    a.knobs.lock().unwrap().egress_err = None;
    a.d.anchor_on();
    assert_eq!(a.d.bonds.len(), 1, "the bond whose channel is down stays: {:?}", a.d.logs);
    // A while without the channel: the bond carries on its lanes, by its old terms.
    let until = Instant::now() + Duration::from_secs(2);
    run_without_redial(&mut a, &mut c, "a while", |_, _| Instant::now() >= until);
    assert_eq!(owners(&a, &c), before, "the same bond on both sides");
    assert_eq!((c.d.bonds[0].state, a.d.bonds[0].egress), (BondState::Up, Egress::None));
    assert_eq!(c.d.capture.as_ref().map(|cap| cap.routes), Some(host::Routes::BondOnly), "capture stays");
    // The channel returns: the same bond, on the new terms.
    c.d.redial_at = 0;
    run(&mut [&mut a, &mut c], "capture by the new terms", |n| {
        n[1].d.capture.as_ref().is_some_and(|cap| cap.routes == host::Routes::All)
    });
    assert_eq!(c.d.capture.as_ref().and_then(|cap| cap.dns), Some("10.77.0.1".parse().unwrap()));
    assert_eq!(owners(&a, &c), before, "the same sessions");
    assert_eq!((c.d.bonds[0].egress, a.d.bonds[0].egress), (Egress::Both, Egress::Both));
    assert_eq!(c.d.bonds[0].state, BondState::Up);
    assert_eq!(c.calls(), ["capture on", "capture off", "capture on"], "the old capture went before the new one");
}

/// §6.2, §17: a bond whose channel is down when the bond subnet moves can be neither told nor
/// kept, since the anchor's device no longer has the address its traffic goes to: the anchor
/// ends it, and its client fails open as it does for any anchor it stops hearing.
#[test]
fn a_subnet_move_while_the_channel_is_down_ends_the_bond() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(78, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(78, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(78, 2))], ip(78, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(78, 1))], ip(78, 10), ccfg);
    a.knobs.lock().unwrap().egress_why = Some("sharing is refused".into());
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some() && n[1].d.bonds[0].state == BondState::Up);
    channel_down(&mut a, &mut c);
    sharing_starts(&a);
    a.d.anchor_on();
    assert!(a.d.bonds.is_empty(), "{:?}", a.d.logs);
    let said = "the bond subnet moved: ending the bond, whose channel is down";
    assert!(a.d.logs.iter().any(|l| l.ends_with(said)), "{:?}", a.d.logs);
}

/// §5.2, §17: an anchor whose egress goes while a bond's channel is down cannot tell its client,
/// which routes everything into the bond. Kept, that bond would take the client's traffic for the
/// internet and send it nowhere, for as long as the mesh stays down. The anchor ends it: the
/// client stops hearing the anchor and fails open.
#[test]
fn egress_that_goes_while_the_channel_is_down_ends_the_bond() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(82, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(82, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(82, 2))], ip(82, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(82, 1))], ip(82, 10), ccfg);
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some() && n[1].d.bonds[0].state == BondState::Up);
    assert_eq!(c.d.capture.as_ref().map(|cap| cap.routes), Some(host::Routes::All));
    channel_down(&mut a, &mut c);
    a.knobs.lock().unwrap().egress_err = Some("sharing stopped".into());
    a.d.anchor_on();
    assert!(a.d.bonds.is_empty(), "{:?}", a.d.logs);
    let said = "egress is now None: ending the bond, whose channel is down";
    assert!(a.d.logs.iter().any(|l| l.ends_with(said)), "{:?}", a.d.logs);
    run_without_redial(&mut a, &mut c, "the client fails open", |_, c| c.d.capture.is_none());
    assert_eq!(c.d.bonds[0].state, BondState::Down);
    assert_eq!(c.calls(), ["capture on", "capture off"]);
    // The mesh returns: the client's old bond meets the anchor's new one, on the new terms.
    c.d.redial_at = 0;
    run(&mut [&mut a, &mut c], "a bond again, without egress", |n| {
        n[1].d.bonds[0].state == BondState::Up
            && n[1].d.capture.as_ref().is_some_and(|cap| cap.routes == host::Routes::BondOnly)
    });
    assert_eq!((c.d.bonds[0].egress, a.d.bonds[0].egress), (Egress::None, Egress::None));
    carries_both_ways(&mut a, &mut c);
}

/// §17: the channel of a bond the anchor ended untold may return before its client has failed
/// open. The client then takes the anchor's `bond ok` for its own bond, keeps its session and
/// installs capture again by the new terms, and that session and the anchor's new one carry
/// packets both ways.
#[test]
fn a_bond_ended_untold_takes_the_new_terms_if_its_channel_returns_first() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(85, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(85, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(85, 2))], ip(85, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(85, 1))], ip(85, 10), ccfg);
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some() && n[1].d.bonds[0].state == BondState::Up);
    let owners = (a.d.bonds[0].owner, c.d.bonds[0].owner);
    channel_down(&mut a, &mut c);
    a.knobs.lock().unwrap().egress_err = Some("sharing stopped".into());
    a.d.anchor_on();
    assert!(a.d.bonds.is_empty(), "{:?}", a.d.logs);
    c.d.redial_at = 0;
    run(&mut [&mut a, &mut c], "the new terms", |n| {
        n[1].d.bonds[0].state == BondState::Up
            && n[1].d.capture.as_ref().is_some_and(|cap| cap.routes == host::Routes::BondOnly)
    });
    assert!(!c.d.logs.iter().any(|l| l.ends_with(": fail open")), "the channel returned first: {:?}", c.d.logs);
    assert_eq!(c.d.bonds[0].owner, owners.1, "the client's own bond and session");
    assert_ne!(a.d.bonds[0].owner, owners.0, "the anchor's new ones");
    assert_eq!(c.calls(), ["capture on", "capture off", "capture on"], "never without the bond's routes for long");
    carries_both_ways(&mut a, &mut c);
}

/// §5.5, §17: egress that comes with one family while a bond's channel is down keeps the bond;
/// when the channel returns the same bond takes it, and its client captures everything.
#[test]
fn egress_of_one_family_that_appears_while_the_channel_is_down_comes_when_it_returns() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(84, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(84, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(84, 2))], ip(84, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(84, 1))], ip(84, 10), ccfg);
    {
        let mut k = a.knobs.lock().unwrap();
        (k.egress_err, k.egress_v4_only) = (Some("no default route".into()), true);
    }
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some() && n[1].d.bonds[0].state == BondState::Up);
    let owner = c.d.bonds[0].owner;
    channel_down(&mut a, &mut c);
    a.knobs.lock().unwrap().egress_err = None;
    a.d.anchor_on();
    let said = "egress is now V4: the bond takes it when its channel returns";
    assert!(a.d.bonds.len() == 1 && a.d.logs.iter().any(|l| l.ends_with(said)), "{:?}", a.d.logs);
    c.d.redial_at = 0;
    run(&mut [&mut a, &mut c], "capture by the new terms", |n| {
        n[1].d.capture.as_ref().is_some_and(|cap| cap.routes == host::Routes::All)
    });
    assert_eq!((c.d.bonds[0].owner, c.d.bonds[0].egress, a.d.bonds[0].egress), (owner, Egress::V4, Egress::V4));
    assert_eq!(c.calls(), ["capture on", "capture off", "capture on"]);
}

/// §5.5, §17: a bond whose channel is down stays while a family of its egress does. An anchor
/// that gains IPv6 keeps the bond, and so does one that loses IPv6 and keeps IPv4, whose client
/// would lose a working bond for the sake of one family. Either way the bond takes the new egress
/// when its channel returns, with the capture it has: the routes and the DNS are the same.
#[test]
fn a_family_of_egress_that_goes_or_comes_while_the_channel_is_down() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(83, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(83, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(83, 2))], ip(83, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(83, 1))], ip(83, 10), ccfg);
    a.knobs.lock().unwrap().egress_v4_only = true;
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some() && n[1].d.bonds[0].state == BondState::Up);
    assert_eq!((c.d.bonds[0].egress, a.d.bonds[0].egress), (Egress::V4, Egress::V4));
    // IPv6 comes: the bond stays, and its client's capture too.
    let owner = c.d.bonds[0].owner;
    channel_down(&mut a, &mut c);
    a.knobs.lock().unwrap().egress_v4_only = false;
    a.d.anchor_on();
    let said = "egress is now Both: the bond takes it when its channel returns";
    assert!(a.d.bonds.len() == 1 && a.d.logs.iter().any(|l| l.ends_with(said)), "{:?}", a.d.logs);
    c.d.redial_at = 0;
    run(&mut [&mut a, &mut c], "the channel returns", |n| n[1].d.bonds[0].egress == Egress::Both);
    assert_eq!((c.d.bonds[0].owner, c.calls()), (owner, vec!["capture on".to_string()]), "the same bond and capture");
    // IPv6 goes: the bond stays too, for the IPv4 it still carries.
    channel_down(&mut a, &mut c);
    a.knobs.lock().unwrap().egress_v4_only = true;
    a.d.anchor_on();
    let said = "egress is now V4: the bond takes it when its channel returns";
    assert!(a.d.bonds.len() == 1 && a.d.logs.iter().any(|l| l.ends_with(said)), "{:?}", a.d.logs);
    c.d.redial_at = 0;
    run(&mut [&mut a, &mut c], "the channel returns", |n| n[1].d.bonds[0].egress == Egress::V4);
    assert_eq!((c.d.bonds[0].owner, c.calls()), (owner, vec!["capture on".to_string()]), "the same bond and capture");
    carries_both_ways(&mut a, &mut c);
}

/// §5.3, §17: a bond whose capture is off for `mesh_captured` keeps it off when new terms come
/// with its returning channel, and stays up: no `lanes_blocked`.
#[test]
fn mesh_captured_stays_off_when_new_terms_come_back() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(81, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(81, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(81, 2))], ip(81, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(81, 1))], ip(81, 10), ccfg);
    a.knobs.lock().unwrap().egress_err = Some("sharing is off".into());
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some() && n[1].d.bonds[0].state == BondState::Up);
    c.tun.inject(udp_to("100.101.102.103:41641".parse().unwrap()));
    run(&mut [&mut a, &mut c], "mesh_captured", |n| n[1].d.capture.is_none());
    channel_down(&mut a, &mut c);
    a.knobs.lock().unwrap().egress_err = None;
    a.d.anchor_on();
    c.d.redial_at = 0;
    run(&mut [&mut a, &mut c], "the channel returns", |n| n[1].d.bonds[0].egress == Egress::Both);
    let until = Instant::now() + Duration::from_secs(4);
    run(&mut [&mut a, &mut c], "a while", |_| Instant::now() >= until);
    assert!(c.d.capture.is_none());
    assert_eq!(c.d.bonds[0].state, BondState::Up);
    let st = c.d.status(false);
    assert!(st.problems.iter().all(|p| p.code != problem::LANES_BLOCKED), "{st:?}");
}

/// §3.2 step 3, §18: the anchor closes only bonds whose terms changed. `pick`'s temporary bond
/// carries no egress and no addresses whatever the anchor's are, and a bond without a session
/// is one `bond_open` never leaves, so that skip only guards: both stay when egress appears and
/// the subnet moves. The state is built by hand, the anchor alone, with a bond of each kind on
/// channels that do not exist.
#[test]
fn a_pick_bond_and_one_without_a_session_stay_when_the_terms_change() {
    let allow = vec![ip(80, 10).to_string(), ip(80, 11).to_string()];
    let acfg = Config { role: Role::Anchor, allow, ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(80, 2))], ip(80, 20), acfg);
    a.knobs.lock().unwrap().egress_err = Some("sharing is off".into());
    a.d.anchor_on();
    let now = a.d.now();
    let mut pick = Bond::new("mac".into(), ip(80, 10), 1, Mode::Performance, now);
    (pick.chan, pick.measure) = (Some(u64::MAX), true);
    a.d.bonds.push(pick);
    a.d.open_session(0, core::Role::Anchor, true);
    let mut opening = Bond::new("pc".into(), ip(80, 11), 2, Mode::Performance, now);
    (opening.chan, opening.addr4) = (Some(u64::MAX - 1), Some(Ipv4Addr::new(10, 77, 0, 3)));
    a.d.bonds.push(opening);
    {
        let mut k = a.knobs.lock().unwrap();
        (k.egress_err, k.egress_addr4) = (None, Some((Ipv4Addr::new(192, 168, 137, 1), 24)));
    }
    a.d.anchor_on();
    assert_eq!(a.d.egress_kind(), Egress::Both);
    let kept: Vec<_> = a.d.bonds.iter().map(|b| (b.peer.as_str(), b.measure, b.session.is_some())).collect();
    assert_eq!(kept, [("mac", true, true), ("pc", false, false)], "{:?}", a.d.logs);
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

/// §6.2: an anchor started before the network (a service at boot) enables egress when the default
/// route first appears, not only when a known one moves.
#[test]
fn an_anchor_started_before_the_network_enables_egress_when_the_uplink_appears() {
    let acfg = Config { role: Role::Anchor, ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(71, 2))], ip(71, 20), acfg);
    a.knobs.lock().unwrap().egress_why = Some("no default route".into());
    run(&mut [&mut a], "anchor on", |n| n[0].d.egress.is_some());
    assert_eq!(a.d.status(false).anchor.unwrap().egress, Egress::None);
    assert!(a.d.tun.is_some() && a.calls().is_empty(), "the device is open; egress was refused");
    // §20.2: status says why under `no_egress`, and so does `wirebond anchor on`, which succeeds:
    // the anchor still delivers to itself.
    let why = |n: &Node| {
        let st = n.d.status(false);
        st.problems.iter().find(|p| p.code == problem::NO_EGRESS).map(|p| p.msg.clone()).unwrap_or_default()
    };
    assert!(why(&a).contains("no egress: no default route"), "{}", why(&a));
    let r = a.d.answer(Request::AnchorOn);
    assert!(r.ok && !r.changed && r.message.contains("no default route"), "{r:?}");
    // `anchor on` again is the problem's next step: it tries again, here once sharing works.
    a.knobs.lock().unwrap().egress_why = None;
    let r = a.d.answer(Request::AnchorOn);
    assert!(r.ok && !r.message.contains("no egress") && a.calls() == ["egress on "], "{r:?} {:?}", a.calls());
    assert_eq!(a.d.status(false).anchor.unwrap().egress, Egress::Both);
    a.knobs.lock().unwrap().egress_why = Some("no default route".into());
    a.d.anchor_on();
    assert!(why(&a).contains("no egress: no default route"), "{}", why(&a));
    // The network comes up.
    {
        let mut k = a.knobs.lock().unwrap();
        (k.default, k.uplink, k.egress_why) = (Some(ip(71, 2)), "eth0".into(), None);
    }
    run(&mut [&mut a], "egress on", |n| n[0].calls().contains(&"egress on eth0".to_string()));
    let st = a.d.status(false).anchor.unwrap();
    assert_eq!((st.egress, st.uplink.as_deref()), (Egress::Both, Some("eth0")));
    assert_eq!(a.knobs.lock().unwrap().tuns, 1, "the device it had is kept");
    assert_eq!(a.calls().len(), 2, "once by hand above, once when the uplink appeared");
    assert!(!why(&a).contains("no egress"), "{}", why(&a));
}

/// §6.1: an anchor whose TUN device did not open refuses bonds, which would drop every packet, and
/// tries the device again; the client's redial then finds it.
#[test]
fn an_anchor_without_a_device_refuses_bonds_and_opens_it_again() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(72, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(72, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(72, 2))], ip(72, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(72, 1))], ip(72, 10), ccfg);
    a.knobs.lock().unwrap().tun_err = Some("the driver is not loaded".into());
    run(&mut [&mut a, &mut c], "refused", |n| n[1].d.unreachable.as_deref().is_some_and(|u| u.contains("TUN device")));
    assert!(a.d.bonds.is_empty() && a.d.tun.is_none() && c.d.capture.is_none());
    assert!(a.calls().is_empty(), "no egress for a device that is not there: {:?}", a.calls());
    // §20.2: status says why under `no_egress`; `wirebond anchor on` fails with the reason.
    let st = a.d.status(false);
    let p = st.problems.iter().find(|p| p.code == problem::NO_EGRESS).unwrap_or_else(|| panic!("{st:?}"));
    assert_eq!(p.msg, "no TUN device: the driver is not loaded");
    let r = a.d.answer(Request::AnchorOn);
    assert!(!r.ok && r.error.as_ref().is_some_and(|e| e.msg == p.msg), "{r:?}");
    let said = |n: &Node| n.d.logs.iter().filter(|l| l.contains("the driver is not loaded")).count();
    let until = Instant::now() + Duration::from_nanos(HOST_RETRY as u64) + Duration::from_secs(1);
    run(&mut [&mut a, &mut c], "a retry later", |_| Instant::now() >= until);
    assert_eq!((said(&a), a.d.tun.is_none()), (1, true), "tried again, said once: {:?}", a.d.logs);
    // The device opens on the next try, and the client's next dial bonds.
    a.knobs.lock().unwrap().tun_err = None;
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some());
    assert_eq!((a.knobs.lock().unwrap().tuns, a.calls()), (1, vec!["egress on ".to_string()]));
}

/// §20.2: a lane names the client's link and the anchor's by their labels, as the links tables do,
/// on both ends in that order, and its first arrivals are the engine's, as a fraction of the bond's.
#[test]
fn status_lanes_name_links_by_label_and_say_what_they_carry_now() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(73, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(73, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(73, 2))], ip(73, 20), acfg);
    let mut c = node("mac", &[("en0", ip(73, 1))], ip(73, 10), ccfg);
    (a.knobs.lock().unwrap().label, c.knobs.lock().unwrap().label) = (Some("Ethernet".into()), Some("Wi-Fi".into()));
    run(&mut [&mut a, &mut c], "bond up", |n| n[1].d.capture.is_some());
    let direct =
        |n: &Node| n.d.bonds[0].session.as_ref().unwrap().snapshot().lanes.iter().any(|l| l.id != 0 && l.validated);
    run(&mut [&mut a, &mut c], "a direct lane", |n| direct(n[0]) && direct(n[1]));
    for i in 0..20u8 {
        let mut p = udp_to("1.1.1.1:53".parse().unwrap());
        *p.last_mut().unwrap() = i;
        c.tun.inject(p);
    }
    run(&mut [&mut a, &mut c], "egress", |n| n[0].tun.written().len() >= 20);
    let (cl, al) = (c.d.status(false).bond.unwrap().lanes, a.d.status(false).anchor.unwrap().bonds[0].lanes.clone());
    let names = |ls: &[api::Lane]| ls.iter().find(|l| l.id != 0).map(|l| l.links.clone());
    let want = Some(vec!["Wi-Fi".to_string(), "Ethernet".to_string()]);
    assert_eq!((names(&cl), names(&al)), (want.clone(), want), "the client's link, then the anchor's");
    // The anchor received every packet: its lanes' first arrivals are fractions that add up.
    let firsts: Vec<f64> = al.iter().map(|l| l.first_arrivals).collect();
    assert!(
        (firsts.iter().sum::<f64>() - 1.0).abs() < 1e-9 && firsts.iter().all(|f| (0.0..=1.0).contains(f)),
        "{al:?}"
    );
    // The client received none: no fraction to show.
    assert!(cl.iter().all(|l| l.first_arrivals == 0.0), "{cl:?}");
    // What the lanes carry now (§20.2): the client sent and the anchor received, megabits a
    // second over the last seconds, and each side's shares count both directions (§11.5), so the
    // anchor, which sent no data, has shares too. (Its lanes still show a little going out: the
    // engine counts every packet a lane sends, probes too, and received data alone.) The engine's
    // jitter is passed on.
    let sum = |ls: &[api::Lane], f: fn(&api::Lane) -> f64| ls.iter().map(f).sum::<f64>();
    assert!(sum(&cl, |l| l.tx_mbps) > 0.0 && sum(&cl, |l| l.rx_mbps) == 0.0, "{cl:?}");
    assert!(sum(&al, |l| l.rx_mbps) > 0.0, "{al:?}");
    assert!((sum(&cl, |l| l.share) - 1.0).abs() < 1e-9 && (sum(&al, |l| l.share) - 1.0).abs() < 1e-9, "{cl:?} {al:?}");
    assert!(sum(&cl, |l| l.dev_ms) > 0.0, "{cl:?}");
}

/// A capture the host refuses (a route another VPN owns) is tried again every HOST_RETRY, not at
/// every look at the lanes; what it did install is removed; the reason is logged once and status
/// shows it in place of "no lane validated".
#[test]
fn a_refused_capture_is_tried_again_at_intervals_and_shown() {
    let acfg = Config { role: Role::Anchor, allow: vec![ip(74, 10).to_string()], ..Config::default() };
    let ccfg = Config { role: Role::Client, anchor: Some(ip(74, 20).to_string()), ..Config::default() };
    let mut a = node("xanth", &[("eth0", ip(74, 2))], ip(74, 20), acfg);
    let mut c = node("mac", &[("lo-a", ip(74, 1))], ip(74, 10), ccfg);
    c.knobs.lock().unwrap().capture_err = Some("route: File exists on utun3".into());
    run(&mut [&mut a, &mut c], "refused", |n| n[1].d.capture_fault.is_some());
    // Well past LANES_BLOCKED_AFTER and many looks at the lanes, short of the next try.
    let until = Instant::now() + Duration::from_nanos((HOST_RETRY - SEC) as u64);
    run(&mut [&mut a, &mut c], "a while", |_| Instant::now() >= until);
    let said = c.d.logs.iter().filter(|l| l.contains("File exists on utun3")).count();
    assert_eq!((c.knobs.lock().unwrap().captures, said), (1, 1), "one try, said once: {:?}", c.d.logs);
    assert_eq!(c.calls(), vec!["capture off".to_string()], "what a failed install left is removed");
    let st = c.d.status(false);
    let p = st.problems.iter().find(|p| p.code == problem::LANES_BLOCKED).unwrap_or_else(|| panic!("{st:?}"));
    assert_eq!(p.msg, "capture failed (route: File exists on utun3): traffic goes direct");
    assert!(st.bond.unwrap().lanes.iter().any(|l| l.state == api::LaneState::Up), "the lanes are up");
    // The other VPN lets go: the next try installs capture, and the bond is up.
    c.knobs.lock().unwrap().capture_err = None;
    run(&mut [&mut a, &mut c], "capture", |n| n[1].d.capture.is_some());
    let st = c.d.status(false);
    assert_eq!(st.bond.as_ref().unwrap().state, BondState::Up);
    assert!(st.problems.iter().all(|p| p.code != problem::LANES_BLOCKED), "{st:?}");
    assert_eq!(c.knobs.lock().unwrap().captures, 2);
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
    // §17: with lanes through two of its links, the client's bond does not rest on one.
    run(&mut [&mut a, &mut c], "lanes through both links", |n| {
        lanes(&n[1].d)
            .is_some_and(|l| bond::client_links(&core::SessionStatus { lanes: l, ..Default::default() }).len() == 2)
            && n[1].d.bonds[0].one_link.is_none()
    });
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
