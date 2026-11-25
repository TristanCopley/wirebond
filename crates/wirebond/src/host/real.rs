//! The OS layer against a real kernel (SPEC §5–§7, §9.1): each test calls the host API as the
//! daemon does and reads the result back from the OS itself: ifconfig, netstat, route, scutil and
//! pfctl on macOS; Get-NetIPAddress, Get-NetRoute and the like on Windows. Linux's are in tests.rs
//! (a network namespace) and tests/system. These need root or an Administrator and change this
//! machine's network, so they are ignored: ci.yml's rust-os job runs them on GitHub's runners,
//! each in a process of its own, `<test binary> --ignored --exact <name> --nocapture`.
//!
//! Capture takes the machine's own traffic into a device only the test reads: while it is
//! installed nothing else of the machine's reaches the internet, a runner's connection to GitHub
//! included. A test keeps it for a few seconds, in a guard ([`Captured`]) that removes it on a
//! panic too and whose watchdog ends a test that hangs with it in place.
//!
//! A test prints what the OS showed, goes on past a check that fails so that one run shows them
//! all ([`Checks`]), and fails at its end.

use super::*;
use std::net::ToSocketAddrs;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

/// The bond's subnets in these tests, apart from the default 10.77.0.0/24 and fd77:b0d::/64.
const NET4: (Ipv4Addr, u8) = (Ipv4Addr::new(10, 77, 9, 0), 24);
const NET6: (Ipv6Addr, u8) = (Ipv6Addr::new(0xfd77, 0xb0d, 9, 0, 0, 0, 0, 0), 64);
/// How long capture may stay before the watchdog removes it and ends the test.
const HANG: Duration = Duration::from_secs(30);

fn a4(host: u8) -> Ipv4Addr {
    Ipv4Addr::from_bits(NET4.0.to_bits() | u32::from(host))
}

fn a6(host: u8) -> Ipv6Addr {
    Ipv6Addr::from_bits(NET6.0.to_bits() | u128::from(host))
}

/// The device with host `host` of both subnets, as node opens it: 2 a client's, 1 an anchor's.
fn tun_config(host: u8) -> TunConfig {
    let (addr4, addr6) = (Some((a4(host), NET4.1)), Some((a6(host), NET6.1)));
    TunConfig { name: TUN_NAME.into(), mtu: wirebond_core::TUN_MTU, addr4, addr6 }
}

/// A client's capture as node builds it, with the anchor's bond address as the resolver.
fn capture(tun: &Tun, routes: Routes, link: &Link) -> Capture {
    Capture {
        tun: tun.name().into(),
        routes,
        bond4: Some(NET4),
        bond6: Some(NET6),
        exclude: Config::default().mesh_ranges(),
        links: vec![link.name.clone()],
        dns: Some(a4(1).into()),
    }
}

/// The link of the default route, found as node finds an anchor's uplink: the runner's own
/// interface, which carries its connection to GitHub.
fn primary() -> Link {
    let def = default_addr().expect("a default route");
    let (links, mesh) = links(&Config::default().mesh_ranges()).unwrap();
    eprintln!("links {links:?}\nmesh {mesh:?}\ndefault address {def}");
    links.into_iter().find(|l| l.addr == def).unwrap_or_else(|| panic!("the default address {def} is no link's"))
}

/// The checks that did not hold.
#[derive(Default)]
struct Checks(Vec<String>);

impl Checks {
    fn that(&mut self, ok: bool, what: impl AsRef<str>) {
        if !ok {
            eprintln!("FAILED: {}", what.as_ref());
            self.0.push(what.as_ref().into());
        }
    }

    fn end(self) {
        assert!(self.0.is_empty(), "{} checks failed:\n  {}", self.0.len(), self.0.join("\n  "));
    }
}

/// What a command printed, standard error after standard output, however it ended.
fn out(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
        Err(e) => format!("{program}: {e}"),
    }
}

/// Prints `text` under `title`, for the job's log.
fn show(title: &str, text: &str) {
    eprintln!("--- {title}\n{}", text.trim_end());
}

/// Capture while this lives: removed by `remove`, or on drop, as a panic's unwinding does. Should
/// the test hang instead, a watchdog removes it after HANG and ends the process, which takes the
/// device and its routes with it.
struct Captured {
    c: Capture,
    on: Arc<AtomicBool>,
}

impl Captured {
    fn install(c: &Capture) -> io::Result<Captured> {
        let guard = Captured { c: c.clone(), on: Arc::new(AtomicBool::new(true)) };
        let (c, on, until) = (c.clone(), guard.on.clone(), Instant::now() + HANG);
        thread::spawn(move || {
            while on.load(Ordering::Acquire) {
                if Instant::now() > until {
                    eprintln!("capture stayed {HANG:?}: removing it and ending the test");
                    let _ = capture_remove(&c);
                    std::process::exit(3);
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
        capture_install(&guard.c).map(|()| guard) // a failed install drops the guard: what it did install goes
    }

    fn remove(&mut self) -> io::Result<()> {
        if !self.on.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        capture_remove(&self.c)
    }
}

impl Drop for Captured {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// The Internet checksum of `b`.
fn csum(b: &[u8]) -> u16 {
    let mut sum: u32 = b.chunks(2).map(|c| u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))).sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// One UDP datagram as an IP packet, IPv4 or IPv6 as its addresses are, with its checksums.
fn udp(from: SocketAddr, to: SocketAddr, data: &[u8]) -> Vec<u8> {
    let len = 8 + data.len() as u16;
    let ports = [from.port().to_be_bytes(), to.port().to_be_bytes(), len.to_be_bytes(), [0, 0]].concat();
    let (mut head, pseudo) = match (from.ip(), to.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            let addrs = [s.octets(), d.octets()].concat();
            let mut h = [&[0x45, 0][..], &(20 + len).to_be_bytes(), &[0, 0, 0, 0, 64, 17, 0, 0], &addrs].concat();
            let sum = csum(&h);
            h[10..12].copy_from_slice(&sum.to_be_bytes());
            (h, [&addrs[..], &[0, 17], &len.to_be_bytes()].concat())
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            let addrs = [s.octets(), d.octets()].concat();
            let h = [&[0x60, 0, 0, 0][..], &len.to_be_bytes(), &[17, 64], &addrs].concat();
            (h, [&addrs[..], &u32::from(len).to_be_bytes(), &[0, 0, 0, 17]].concat())
        }
        _ => panic!("{from} and {to} are of two families"),
    };
    let sum = match csum(&[&pseudo[..], &ports, data].concat()) {
        0 => 0xffff, // 0 on the wire means none
        s => s,
    };
    let at = head.len() + 6;
    head.extend_from_slice(&ports);
    head[at..at + 2].copy_from_slice(&sum.to_be_bytes());
    [&head[..], data].concat()
}

/// An IP packet's UDP datagram: its source, its destination and its payload. None for anything
/// else, a fragment included.
fn parse(p: &[u8]) -> Option<(SocketAddr, SocketAddr, &[u8])> {
    let (s, d, u): (IpAddr, IpAddr, &[u8]) = match p.first()? >> 4 {
        4 if p.len() >= 20 && p[9] == 17 && u16::from_be_bytes([p[6], p[7]]) & 0x3fff == 0 => {
            let (s, d): ([u8; 4], [u8; 4]) = (p[12..16].try_into().ok()?, p[16..20].try_into().ok()?);
            (s.into(), d.into(), p.get(usize::from(p[0] & 15) * 4..)?)
        }
        6 if p.len() >= 40 && p[6] == 17 => {
            let (s, d): ([u8; 16], [u8; 16]) = (p[8..24].try_into().ok()?, p[24..40].try_into().ok()?);
            (s.into(), d.into(), &p[40..])
        }
        _ => return None,
    };
    let port = |i| u.get(i..i + 2).map(|b: &[u8]| u16::from_be_bytes([b[0], b[1]]));
    let data = u.get(8..usize::from(port(4)?))?;
    Some((SocketAddr::new(s, port(0)?), SocketAddr::new(d, port(2)?), data))
}

/// One datagram the device gave up: the whole packet's length, then what [`parse`] reads.
type Seen = (usize, SocketAddr, SocketAddr, Vec<u8>);

/// The first UDP datagram out of `tun` within `wait` that `want` takes by its source, destination
/// and payload. Whatever else the OS sends into the device (router solicitations, mDNS) is passed
/// over.
fn from_tun(tun: &Tun, wait: Duration, want: impl Fn(SocketAddr, SocketAddr, &[u8]) -> bool) -> Option<Seen> {
    let (until, mut b) = (Instant::now() + wait, [0u8; 2048]);
    while Instant::now() < until {
        match tun.read(&mut b) {
            Ok(n) => match parse(&b[..n]) {
                Some((s, d, data)) if want(s, d, data) => return Some((n, s, d, data.to_vec())),
                _ => {}
            },
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(2)),
            Err(e) => panic!("reading the device: {e}"),
        }
    }
    None
}

/// Something on the internet that answers one datagram with one.
struct Target {
    name: String,
    at: SocketAddr,
    ask: Vec<u8>,
}

/// The targets: the default STUN servers (§9.2), as a lane socket asks them, and three public
/// resolvers asked for example.com. The STUN servers' names are resolved here, so before capture.
fn targets() -> Vec<Target> {
    // RFC 8489's Binding request: type 1, no attributes, the magic cookie, a transaction id.
    let stun = [&[0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42][..], b"wirebondtest"].concat();
    let query = [&[0x77, 0x62, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0][..], b"\x07example\x03com\0", &[0, 1, 0, 1]].concat();
    let mut ts = Vec::new();
    for name in Config::default().stun {
        match name.to_socket_addrs().map(|mut a| a.find(SocketAddr::is_ipv4)) {
            Ok(Some(at)) => ts.push(Target { name, at, ask: stun.clone() }),
            r => eprintln!("{name} has no IPv4 address: {r:?}"),
        }
    }
    for ip in [[1, 1, 1, 1], [8, 8, 8, 8], [9, 9, 9, 9]] {
        let at = SocketAddr::from((ip, 53));
        ts.push(Target { name: format!("resolver {at}"), at, ask: query.clone() });
    }
    ts
}

/// Sends each target its question from `u`, and once more halfway through `wait` to those yet to
/// answer. Per target: how long its answer took, or why there is none.
fn ask(u: &UdpSocket, ts: &[Target], wait: Duration) -> Vec<Result<Duration, String>> {
    let (start, mut b) = (Instant::now(), [0u8; 1500]);
    let mut got: Vec<_> = ts.iter().map(|_| Err("no answer".to_string())).collect();
    u.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    for round in 1..=2 {
        for (t, g) in ts.iter().zip(&mut got) {
            if let (true, Err(e)) = (g.is_err(), u.send_to(&t.ask, t.at)) {
                *g = Err(format!("send: {e}"));
            }
        }
        while start.elapsed() < wait / 2 * round && got.iter().any(Result::is_err) {
            if let Ok((_, from)) = u.recv_from(&mut b)
                && let Some(i) = ts.iter().position(|t| t.at == from)
            {
                got[i] = Ok(start.elapsed());
            }
        }
    }
    got
}

/// Prints each target's answer under `title`; how many answered.
fn answers(title: &str, ts: &[Target], got: &[Result<Duration, String>]) -> usize {
    eprintln!("--- {title}");
    for (t, g) in ts.iter().zip(got) {
        match g {
            Ok(after) => eprintln!("{} ({}): answered after {after:?}", t.name, t.at),
            Err(why) => eprintln!("{} ({}): {why}", t.name, t.at),
        }
    }
    got.iter().filter(|g| g.is_ok()).count()
}

/// Asks the system's resolver for a name nothing has cached, on a thread that is left behind: under
/// capture the query goes unanswered.
fn look_up() {
    let name = format!("wirebond-{}-{:x}.example.com:80", std::process::id(), boot());
    thread::spawn(move || eprintln!("the lookup of {name} ended: {:?}", name.to_socket_addrs().map(Vec::from_iter)));
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;

    /// The value of `key:` in what `route -n get` printed.
    fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.lines().find_map(|l| l.trim().strip_prefix(key)?.strip_prefix(':')).map(str::trim)
    }

    /// `route -n get` for `to`, which may begin with -inet6 or -ifscope <interface>.
    fn route_get(to: &[&str]) -> String {
        out("route", &[&["-n", "get"], to].concat())
    }

    /// One sysctl's value.
    fn sysctl(key: &str) -> String {
        out("sysctl", &["-n", key]).trim().into()
    }

    /// The dynamic-store key of the client's DNS service, as scutil shows it.
    fn dns_key() -> String {
        let mut c = Command::new("sh");
        c.args(["-c", "echo 'show State:/Network/Service/com.wirebond/DNS' | scutil"]);
        c.output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    }

    /// The state directory, where egress_enable keeps what it changed: the daemon makes it at
    /// start. Made here when it is missing, and then removed on drop.
    struct StateDir(Option<PathBuf>);

    impl StateDir {
        fn new() -> StateDir {
            let dir = Scope::System.paths().1;
            let made = !dir.exists();
            os::mk_state(&dir).unwrap();
            StateDir(made.then_some(dir))
        }
    }

    impl Drop for StateDir {
        fn drop(&mut self) {
            if let Some(dir) = &self.0 {
                let _ = fs_remove(dir);
            }
        }
    }

    fn fs_remove(dir: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(dir)
    }

    /// §5.1, §5.3: the utun has the MTU and both addresses, routes the bond's subnets, and goes
    /// with its descriptor.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_tun_has_its_mtu_and_addresses_and_goes_with_its_descriptor() {
        let mut ok = Checks::default();
        let tun = Tun::open(&tun_config(2)).unwrap();
        let name = tun.name().to_string();
        let ifc = out("ifconfig", &[&name]);
        show(&format!("ifconfig {name}"), &ifc);
        ok.that(name.starts_with("utun"), format!("the device is {name}"));
        ok.that(ifc.contains("mtu 1400"), "the MTU is not 1400");
        ok.that(ifc.contains("UP") && ifc.contains("RUNNING"), "the utun is not up and running");
        ok.that(ifc.contains("inet 10.77.9.2 ") && ifc.contains("netmask 0xffffff00"), "no 10.77.9.2/24");
        ok.that(ifc.contains("inet6 fd77:b0d:9::2 ") && ifc.contains("prefixlen 64"), "no fd77:b0d:9::2/64");
        let table = out("netstat", &["-rn"]);
        let ours: Vec<&str> = table.lines().filter(|l| l.split_whitespace().any(|w| w == name)).collect();
        show(&format!("netstat -rn, {name}'s routes"), &ours.join("\n"));
        for to in [&["10.77.9.9"][..], &["-inet6", "fd77:b0d:9::9"]] {
            let got = route_get(to);
            show(&format!("route -n get {}", to.join(" ")), &got);
            ok.that(field(&got, "interface") == Some(&name), format!("{to:?} does not leave through {name}"));
        }
        drop(tun);
        let gone = out("ifconfig", &[&name]);
        show(&format!("ifconfig {name}, the device dropped"), &gone);
        ok.that(gone.contains("does not exist"), format!("{name} outlived its descriptor"));
        ok.end();
    }

    /// §5.3: a datagram routed into the utun is read whole and without the 4-byte family header,
    /// and a packet written reaches the socket it names; IPv4 and IPv6, and a packet of the MTU.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_tun_carries_packets_both_ways() {
        let mut ok = Checks::default();
        let tun = Tun::open(&tun_config(2)).unwrap();
        let opened = Instant::now();
        let full = vec![0x5a; wirebond_core::TUN_MTU - 28];
        let cases: [(IpAddr, IpAddr, usize, &[u8]); 3] = [
            (a4(2).into(), a4(9).into(), 20, b"ping"),
            (a4(2).into(), a4(9).into(), 20, &full),
            (a6(2).into(), a6(9).into(), 40, b"ping"),
        ];
        for (own, peer, header, data) in cases {
            // The daemon's relay binds an anchor's bond addresses as soon as the device is open.
            let u = loop {
                match UdpSocket::bind((own, 0)) {
                    Ok(u) => break u,
                    Err(e) if opened.elapsed() < Duration::from_secs(5) => {
                        eprintln!("{own} cannot be bound {:?} after the device opened: {e}", opened.elapsed());
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => panic!("{own} cannot be bound: {e}"),
                }
            };
            eprintln!("{own} bound {:?} after the device opened", opened.elapsed());
            let (here, there) = (u.local_addr().unwrap(), SocketAddr::new(peer, 9));
            u.send_to(data, there).unwrap();
            let seen = from_tun(&tun, Duration::from_secs(2), |_, d, _| d == there);
            eprintln!("out of the device: {:?}", seen.as_ref().map(|(n, s, d, p)| (n, s, d, p.len())));
            match seen {
                Some((n, s, _, p)) => {
                    ok.that(n == header + 8 + data.len(), format!("{n} bytes read for {} of payload", data.len()));
                    ok.that(
                        s == here && p == data,
                        format!("{here} sent {} bytes; read {} from {s}", data.len(), p.len()),
                    );
                }
                None => ok.that(false, format!("the datagram {here} sent to {there} did not come out of the device")),
            }
            tun.write(&udp(there, here, b"pong")).unwrap();
            u.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut b = [0u8; 16];
            let got = u.recv_from(&mut b).map(|(n, from)| (b[..n].to_vec(), from));
            eprintln!("into the device: {got:?}");
            ok.that(got.is_ok_and(|g| g == (b"pong".to_vec(), there)), format!("{here} did not get {there}'s reply"));
        }
        ok.end();
    }

    /// §5.3, §7: capture's sixteen routes are in the table, the internet leaves through the utun,
    /// configd lists the resolver and the system asks it; capture_remove leaves none of it.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_capture_routes_and_resolver_come_and_go() {
        let mut ok = Checks::default();
        let link = primary();
        let tun = Tun::open(&tun_config(2)).unwrap();
        let name = tun.name().to_string();
        let c = capture(&tun, Routes::All, &link);
        let at = Instant::now();
        let mut cap = Captured::install(&c).unwrap();
        let again = capture_install(&c); // idempotent
        let table = out("netstat", &["-rn"]);
        let nets: Vec<String> = os::capture_routes().iter().map(|(n, b)| format!("{n}/{b}")).collect();
        let via: Vec<String> = os::capture_routes()
            .iter()
            .map(|(n, _)| match n {
                IpAddr::V4(a) => route_get(&[&Ipv4Addr::from_bits(a.to_bits() | 1).to_string()]),
                IpAddr::V6(a) => route_get(&["-inet6", &Ipv6Addr::from_bits(a.to_bits() | 1).to_string()]),
            })
            .collect();
        let resolvers = out("scutil", &["--dns"]);
        let first = upstream_resolver();
        let key = dns_key();
        look_up();
        let asked = from_tun(&tun, Duration::from_secs(2), |_, d, _| d == SocketAddr::from((a4(1), 53)));
        let removed = cap.remove();
        let window = at.elapsed();
        let removed_again = capture_remove(&c);
        eprintln!(
            "capture stayed {window:?}; installed again: {again:?}; removed: {removed:?}, again: {removed_again:?}"
        );
        let ours: Vec<&str> = table.lines().filter(|l| l.split_whitespace().any(|w| w == name)).collect();
        show(&format!("netstat -rn under capture, {name}'s routes"), &ours.join("\n"));
        ok.that(again.is_ok() && removed.is_ok() && removed_again.is_ok(), "install and remove are not idempotent");
        ok.that(ours.len() >= 16 + 2, format!("{} routes through {name}, not the sixteen and the subnets", ours.len()));
        for (net, got) in nets.iter().zip(&via) {
            let (dest, iface) = (field(got, "destination"), field(got, "interface"));
            eprintln!("{net}: destination {dest:?} mask {:?} interface {iface:?}", field(got, "mask"));
            ok.that(iface == Some(&name), format!("{net} does not leave through {name}:\n{got}"));
        }
        show("scutil --dns under capture", &resolvers);
        show("the DNS key under capture", &key);
        eprintln!("resolver #1 under capture: {first:?}; the system's query in the device: {asked:?}");
        ok.that(resolvers.contains("nameserver[0] : 10.77.9.1"), "scutil --dns does not list 10.77.9.1");
        ok.that(asked.is_some(), "the system's resolver did not ask 10.77.9.1");
        let after = (route_get(&["1.1.1.1"]), out("netstat", &["-rn"]), out("scutil", &["--dns"]), dns_key());
        let left: Vec<&str> = after.1.lines().filter(|l| l.split_whitespace().any(|w| w == name)).collect();
        show(&format!("netstat -rn after capture_remove, {name}'s routes"), &left.join("\n"));
        show("the DNS key after capture_remove", &after.3);
        ok.that(field(&after.0, "interface") == Some(&link.name), format!("1.1.1.1 after removal:\n{}", after.0));
        ok.that(left.len() + 16 == ours.len(), format!("{} routes through {name} remain", left.len()));
        ok.that(!after.2.contains("10.77.9.1"), "scutil --dns still lists 10.77.9.1");
        ok.that(!after.3.contains("ServerAddresses"), "the DNS key remains");
        ok.end();
    }

    /// §5.3, §9.1: the point of the sixteen routes. Under capture a lookup scoped to the primary
    /// interface still finds its gateway and a socket made as a lane's is (IP_BOUND_IF) still
    /// reaches the outside, while an unpinned socket's datagram comes out of the utun.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_lane_socket_stays_out_of_capture_and_an_unpinned_one_goes_in() {
        let mut ok = Checks::default();
        let (link, ts) = (primary(), targets());
        let before = ask(&listen(&link).unwrap().0, &ts, Duration::from_secs(2));
        let tun = Tun::open(&tun_config(2)).unwrap();
        let name = tun.name().to_string();
        let at = Instant::now();
        let mut cap = Captured::install(&capture(&tun, Routes::All, &link)).unwrap();
        let scoped = route_get(&["-ifscope", &link.name, "1.1.1.1"]);
        let plain = route_get(&["1.1.1.1"]);
        // A socket made under capture: it has reached nothing yet.
        let lane = listen(&link).map(|(u, ..)| ask(&u, &ts, Duration::from_secs(2)));
        let there = SocketAddr::from(([1, 1, 1, 1], 9));
        let sent = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|u| u.send_to(b"unpinned", there));
        let seen = from_tun(&tun, Duration::from_secs(1), |_, d, p| d == there && p == b"unpinned");
        let removed = cap.remove();
        eprintln!("capture stayed {:?}; removed: {removed:?}", at.elapsed());
        show(&format!("route -n get -ifscope {} 1.1.1.1 under capture", link.name), &scoped);
        show("route -n get 1.1.1.1 under capture", &plain);
        let reached = answers("a lane socket before capture", &ts, &before);
        ok.that(reached > 0, "no target answers this runner even without capture");
        ok.that(field(&scoped, "interface") == Some(&link.name), "the scoped lookup does not name the link");
        ok.that(field(&scoped, "gateway").is_some(), "the scoped lookup finds no gateway");
        ok.that(field(&plain, "interface") == Some(&name), format!("1.1.1.1 does not leave through {name}"));
        match lane {
            Ok(under) => {
                let reached = answers("a lane socket made under capture", &ts, &under);
                ok.that(reached > 0, "a lane socket reaches nothing under capture");
            }
            Err(e) => ok.that(false, format!("no lane socket under capture: {e}")),
        }
        eprintln!("an unpinned socket's datagram: sent {sent:?}, out of the device {seen:?}");
        ok.that(seen.is_some_and(|(_, s, ..)| s.ip() == a4(2)), "an unpinned datagram did not enter the utun");
        ok.end();
    }

    /// §6.2: forwarding is on, pf is enabled and holds the NAT rule in the anchor the main ruleset
    /// includes, and a client's packet written into the utun comes back answered from the
    /// internet. egress_disable puts back what it found.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_egress_forwards_and_nats_then_puts_back() {
        let mut ok = Checks::default();
        let (_state, link, ts) = (StateDir::new(), primary(), targets());
        // "Status: Enabled for 0 days 00:00:03": the word after "Status:".
        let pf = || {
            out("pfctl", &["-s", "info"]).split_whitespace().skip_while(|w| *w != "Status:").nth(1).map(String::from)
        };
        let was = (sysctl("net.inet.ip.forwarding"), sysctl("net.inet6.ip6.forwarding"), pf());
        eprintln!("before: forwarding {} and {}, pf {:?}", was.0, was.1, was.2);
        let direct = ask(&UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap(), &ts, Duration::from_secs(2));
        let reached = answers("this device itself, before egress", &ts, &direct);
        let tun = Tun::open(&tun_config(1)).unwrap();
        let e = Egress { tun: tun.name().into(), net4: NET4, net6: Some(NET6) };
        let on = egress_enable(&e);
        eprintln!("egress_enable: {on:?}\nagain: {:?}", egress_enable(&e));
        let on = on.unwrap();
        ok.that(on.v4 && on.why.is_none(), "no IPv4 egress");
        ok.that(on.addr4 == (a4(1), 24), format!("the anchor's address is {:?}", on.addr4));
        ok.that(on.uplink.as_deref() == Some(&link.name), format!("the uplink is {:?}, not {}", on.uplink, link.name));
        let anchor = out("pfctl", &["-a", "com.apple/wirebond", "-s", "nat"]);
        let main = out("pfctl", &["-s", "nat"]);
        show("pfctl -a com.apple/wirebond -s nat", &anchor);
        show("pfctl -s nat", &main);
        show("pfctl -s info", &out("pfctl", &["-s", "info"]));
        show("pfctl -s References", &out("pfctl", &["-s", "References"]));
        show(
            "egress.state",
            &std::fs::read_to_string(Scope::System.paths().1.join("egress.state")).unwrap_or_default(),
        );
        let rule = format!("nat on {0} inet from 10.77.9.0/24 to any -> ({0}", link.name);
        ok.that(anchor.contains(&rule), format!("the anchor lacks {rule:?}"));
        ok.that(main.contains("nat-anchor \"com.apple/*\""), "the main ruleset does not include com.apple/*");
        ok.that(pf().as_deref() == Some("Enabled"), "pf is not enabled");
        ok.that(sysctl("net.inet.ip.forwarding") == "1", "IPv4 forwarding is off");
        // A client's packets, as the daemon writes them into the device: NAT's answer comes back out.
        let client = SocketAddr::from((a4(7), 40000));
        for t in &ts {
            tun.write(&udp(client, t.at, &t.ask)).unwrap();
        }
        let back = from_tun(&tun, Duration::from_secs(3), |s, d, _| d == client && ts.iter().any(|t| t.at == s));
        eprintln!("forwarded for {client}: the answer out of the device: {:?}", back.as_ref().map(|b| (b.0, b.1, b.2)));
        ok.that(reached > 0, "no target answers this runner");
        ok.that(back.is_some(), "a client's packet was not forwarded, translated and answered");
        let off = egress_disable(&e);
        eprintln!("egress_disable: {off:?}\nagain: {:?}", egress_disable(&e));
        let anchor = out("pfctl", &["-a", "com.apple/wirebond", "-s", "nat"]);
        show("pfctl -a com.apple/wirebond -s nat, egress off", &anchor);
        show("pfctl -s References, egress off", &out("pfctl", &["-s", "References"]));
        let now = (sysctl("net.inet.ip.forwarding"), sysctl("net.inet6.ip6.forwarding"), pf());
        ok.that(off.is_ok(), "egress_disable failed");
        ok.that(!anchor.contains("nat on"), "the anchor still holds rules");
        ok.that(now == was, format!("forwarding and pf were {was:?} and are {now:?}"));
        ok.that(!Scope::System.paths().1.join("egress.state").exists(), "egress.state remains");
        ok.end();
    }

    /// §7: the anchor's upstream resolver is resolver #1 of `scutil --dns`, and it answers.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_upstream_resolver_answers() {
        let mut ok = Checks::default();
        show("scutil --dns", &out("scutil", &["--dns"]));
        let up = upstream_resolver();
        eprintln!("upstream_resolver: {up:?}");
        let ask_it =
            [Target { name: "the upstream resolver".into(), at: up.unwrap(), ask: targets().pop().unwrap().ask }];
        let u = UdpSocket::bind(if ask_it[0].at.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).unwrap();
        let got = ask(&u, &ask_it, Duration::from_secs(4));
        ok.that(answers("asked for example.com", &ask_it, &got) == 1, "the upstream resolver does not answer");
        ok.end();
    }

    /// §5.4: a daemon that died with capture's resolver and egress in place took its utun and
    /// routes with it; cleanup_stale removes the DNS key, the pf rules and reference, and the
    /// forwarding it turned on.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_cleanup_after_a_crash() {
        let mut ok = Checks::default();
        let (_state, link) = (StateDir::new(), primary());
        let was = (sysctl("net.inet.ip.forwarding"), sysctl("net.inet6.ip6.forwarding"));
        let tun = Tun::open(&tun_config(1)).unwrap();
        // The bond's subnets alone: the resolver is the part that outlives the process.
        let c = capture(&tun, Routes::BondOnly, &link);
        let e = Egress { tun: tun.name().into(), net4: NET4, net6: Some(NET6) };
        let made = capture_install(&c).and_then(|()| egress_enable(&e));
        drop(tun); // the crash
        let left = (dns_key(), out("pfctl", &["-a", "com.apple/wirebond", "-s", "nat"]));
        let cleaned = cleanup_stale();
        eprintln!("made: {made:?}\ncleanup_stale: {cleaned:?}\nagain: {:?}", cleanup_stale());
        show("the DNS key the crash left", &left.0);
        show("the pf anchor the crash left", &left.1);
        ok.that(made.is_ok() && cleaned.is_ok(), "capture, egress or the cleanup failed");
        ok.that(left.0.contains("10.77.9.1") && left.1.contains("nat on"), "the crash left nothing to clean");
        let after = (dns_key(), out("pfctl", &["-a", "com.apple/wirebond", "-s", "nat"]));
        show("the DNS key after cleanup_stale", &after.0);
        show("pfctl -s References after cleanup_stale", &out("pfctl", &["-s", "References"]));
        ok.that(!after.0.contains("ServerAddresses"), "the DNS key remains");
        ok.that(!after.1.contains("nat on"), "the pf anchor still holds rules");
        ok.that(!out("scutil", &["--dns"]).contains("10.77.9.1"), "scutil --dns still lists 10.77.9.1");
        let now = (sysctl("net.inet.ip.forwarding"), sysctl("net.inet6.ip6.forwarding"));
        ok.that(now == was, format!("forwarding was {was:?} and is {now:?}"));
        ok.that(!Scope::System.paths().1.join("egress.state").exists(), "egress.state remains");
        ok.end();
    }

    /// §9.1: `links` names the interface of the default route, labelled as macOS's own list of
    /// hardware ports has it.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_links_name_the_default_interface() {
        let mut ok = Checks::default();
        let link = primary();
        let route = route_get(&["default"]);
        let ports = out("networksetup", &["-listallhardwareports"]);
        show("route -n get default", &route);
        show("networksetup -listallhardwareports", &ports);
        show("ifconfig", &out("ifconfig", &[]));
        ok.that(field(&route, "interface") == Some(&link.name), format!("the default route is not {}'s", link.name));
        // "Hardware Port: Wi-Fi" is the line before "Device: en0".
        let port = ports.lines().zip(ports.lines().skip(1)).find(|(_, d)| d.trim() == format!("Device: {}", link.name));
        let want = if port.is_some_and(|(p, _)| p.contains("Wi-Fi")) { "Wi-Fi" } else { "Ethernet" };
        ok.that(link.label == want && !link.metered, format!("{link:?} is not labelled {want}"));
        ok.end();
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Stdio};
    use std::sync::mpsc;

    /// How long a PowerShell may take to start and load its network modules, on a busy runner.
    const WARM: Duration = Duration::from_secs(60);

    /// A PowerShell started ahead with its network modules loaded, so that a script run under
    /// capture costs its own time alone. It reads the script from its standard input, where a
    /// statement must fit on a line.
    ///
    /// `warm` returns once the shell has printed that the modules are loaded. Loading one can ask
    /// the network (a module's signature is checked for revocation), and under capture the network
    /// is the device: a shell still loading when capture went on stalled there until the watchdog's
    /// 30 s were up, in one CI run whose twin on the same commit passed. Returned at once, as it
    /// was, the shell was never ahead of the capture that followed on the next line.
    struct Shell {
        child: Child,
        out: thread::JoinHandle<Vec<u8>>,
        err: thread::JoinHandle<Vec<u8>>,
    }

    impl Shell {
        fn warm() -> Shell {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", "-"]);
            let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let warm = b"Import-Module NetTCPIP, NetAdapter, DnsClient; Get-NetIPInterface | Out-Null; 'warm'\n";
            child.stdin.as_mut().unwrap().write_all(warm).unwrap();
            // Both pipes are read on threads of their own, so neither fills while the other is
            // read. The line "warm" is the shell's word and not the script's: it is left out.
            let (mut out, mut err) = (BufReader::new(child.stdout.take().unwrap()), child.stderr.take().unwrap());
            let (ready, warmed) = mpsc::channel();
            let out = thread::spawn(move || {
                let mut text = Vec::new();
                loop {
                    let at = text.len();
                    match out.read_until(b'\n', &mut text) {
                        Ok(n) if n > 0 && text[at..].trim_ascii() != b"warm" => {}
                        _ => break text.truncate(at), // the word, or the shell's end before it
                    }
                }
                let _ = ready.send(());
                let _ = out.read_to_end(&mut text);
                text
            });
            let err = thread::spawn(move || {
                let mut text = Vec::new();
                let _ = err.read_to_end(&mut text);
                text
            });
            if warmed.recv_timeout(WARM).is_err() {
                eprintln!("PowerShell was not warm after {WARM:?}: going on with it as it is");
            }
            Shell { child, out, err }
        }

        /// Runs `script` and returns what it printed, errors after output. A shell that has died
        /// takes no script: what it printed before it did is what the caller's checks then show.
        fn run(mut self, script: &str) -> String {
            let mut stdin = self.child.stdin.take().unwrap();
            let _ = stdin.write_all(script.as_bytes()).and_then(|()| stdin.write_all(b"\n"));
            drop(stdin);
            let _ = self.child.wait();
            let (out, err) = (self.out.join().unwrap(), self.err.join().unwrap());
            String::from_utf8_lossy(&out).into_owned() + &String::from_utf8_lossy(&err)
        }
    }

    fn ps(script: &str) -> String {
        Shell::warm().run(script)
    }

    /// The adapter as Windows reports it: addresses, each family's MTU and metric, routes and DNS
    /// servers, a line each.
    const ADAPTER: &str = "\
        Get-NetAdapter -Name wirebond -ErrorAction SilentlyContinue | % { \"adapter $($_.Name) $($_.Status)\" }\n\
        Get-NetIPAddress -InterfaceAlias wirebond -ErrorAction SilentlyContinue | % { \"addr $($_.IPAddress)/$($_.PrefixLength) $($_.AddressState)\" }\n\
        Get-NetIPInterface -InterfaceAlias wirebond -ErrorAction SilentlyContinue | % { \"family $($_.AddressFamily) mtu $($_.NlMtu) metric $($_.InterfaceMetric)\" }\n\
        Get-NetRoute -InterfaceAlias wirebond -ErrorAction SilentlyContinue | % { \"route $($_.DestinationPrefix) via $($_.NextHop) metric $($_.RouteMetric)\" }\n\
        Get-DnsClientServerAddress -InterfaceAlias wirebond -ErrorAction SilentlyContinue | % { \"dns $($_.ServerAddresses -join ',') .\" }\n\
        Find-NetRoute -RemoteIPAddress 1.1.1.1 | % { \"to 1.1.1.1: $($_.InterfaceAlias) $($_.IPAddress) $($_.DestinationPrefix)\" }";

    /// Every connection's sharing, as HNetCfg.HNetShare reports it.
    const SHARING: &str = "\
        $m = New-Object -ComObject HNetCfg.HNetShare\n\
        foreach ($c in $m.EnumEveryConnection) { $s = $m.INetSharingConfigurationForINetConnection.Invoke($c); \"sharing $($m.NetConnectionProps.Invoke($c).Name): enabled $($s.SharingEnabled) type $($s.SharingConnectionType)\" }";

    /// The local address a socket gets for 1.1.1.1: the interface Windows routes the internet to.
    fn local_for_internet() -> io::Result<IpAddr> {
        default_addr()
    }

    /// §5.1, §5.3: the adapter has the MTU, metric 5 and both addresses as Windows reports them,
    /// and goes when the device is dropped.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_tun_has_its_mtu_metric_and_addresses_and_goes_when_dropped() {
        let mut ok = Checks::default();
        let tun = Tun::open(&tun_config(2)).unwrap();
        let seen = ps(ADAPTER);
        show("the adapter", &seen);
        ok.that(tun.name() == TUN_NAME && seen.contains("adapter wirebond Up"), "no adapter named wirebond is up");
        for want in [
            "addr 10.77.9.2/24 Preferred",
            "addr fd77:b0d:9::2/64 Preferred",
            "family IPv4 mtu 1400 metric 5",
            "family IPv6 mtu 1400 metric 5",
            "route 10.77.9.0/24 ",
            "route fd77:b0d:9::/64 ",
        ] {
            ok.that(seen.contains(want), format!("Windows does not report {want:?}"));
        }
        drop(tun);
        let gone = ps(ADAPTER);
        show("the adapter, the device dropped", &gone);
        ok.that(!gone.contains("adapter ") && !gone.contains("addr "), "the adapter outlived the device");
        ok.end();
    }

    /// §5.3: a datagram routed into the adapter is read whole from Wintun, and a packet written
    /// reaches the socket it names; IPv4 and IPv6, and a packet of the MTU. The reply comes from
    /// the address and port the socket sent to, as a real one does, so the firewall knows it.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_tun_carries_packets_both_ways() {
        let mut ok = Checks::default();
        let tun = Tun::open(&tun_config(2)).unwrap();
        let opened = Instant::now();
        let full = vec![0x5a; wirebond_core::TUN_MTU - 28];
        let cases: [(IpAddr, IpAddr, usize, &[u8]); 3] = [
            (a4(2).into(), a4(9).into(), 20, b"ping"),
            (a4(2).into(), a4(9).into(), 20, &full),
            (a6(2).into(), a6(9).into(), 40, b"ping"),
        ];
        for (own, peer, header, data) in cases {
            // The daemon's relay binds an anchor's bond addresses as soon as the device is open.
            let u = loop {
                match UdpSocket::bind((own, 0)) {
                    Ok(u) => break u,
                    Err(e) if opened.elapsed() < Duration::from_secs(5) => {
                        eprintln!("{own} cannot be bound {:?} after the device opened: {e}", opened.elapsed());
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => panic!("{own} cannot be bound: {e}"),
                }
            };
            eprintln!("{own} bound {:?} after the device opened", opened.elapsed());
            let (here, there) = (u.local_addr().unwrap(), SocketAddr::new(peer, 9));
            u.send_to(data, there).unwrap();
            let seen = from_tun(&tun, Duration::from_secs(2), |_, d, _| d == there);
            eprintln!("out of the device: {:?}", seen.as_ref().map(|(n, s, d, p)| (n, s, d, p.len())));
            match seen {
                Some((n, s, _, p)) => {
                    ok.that(n == header + 8 + data.len(), format!("{n} bytes read for {} of payload", data.len()));
                    ok.that(
                        s == here && p == data,
                        format!("{here} sent {} bytes; read {} from {s}", data.len(), p.len()),
                    );
                }
                None => ok.that(false, format!("the datagram {here} sent to {there} did not come out of the device")),
            }
            tun.write(&udp(there, here, b"pong")).unwrap();
            u.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut b = [0u8; 16];
            let got = u.recv_from(&mut b).map(|(n, from)| (b[..n].to_vec(), from));
            eprintln!("into the device: {got:?}");
            ok.that(got.is_ok_and(|g| g == (b"pong".to_vec(), there)), format!("{here} did not get {there}'s reply"));
        }
        ok.end();
    }

    /// §5.3, §7: Windows reports capture's four routes and the adapter's DNS server, routes the
    /// internet to the adapter and asks that server; capture_remove leaves none of it.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_capture_routes_and_resolver_come_and_go() {
        let mut ok = Checks::default();
        let link = primary();
        let tun = Tun::open(&tun_config(2)).unwrap();
        let c = capture(&tun, Routes::All, &link);
        let shell = Shell::warm();
        let at = Instant::now();
        let mut cap = Captured::install(&c).unwrap();
        let again = capture_install(&c); // idempotent
        let local = local_for_internet();
        look_up();
        let seen = shell.run(ADAPTER);
        let asked = from_tun(&tun, Duration::from_secs(2), |_, d, _| d == SocketAddr::from((a4(1), 53)));
        let removed = cap.remove();
        let window = at.elapsed();
        let removed_again = capture_remove(&c);
        eprintln!(
            "capture stayed {window:?}; installed again: {again:?}; removed: {removed:?}, again: {removed_again:?}"
        );
        show("the adapter under capture", &seen);
        eprintln!(
            "the local address for 1.1.1.1 under capture: {local:?}; the system's query in the device: {asked:?}"
        );
        ok.that(again.is_ok() && removed.is_ok() && removed_again.is_ok(), "install and remove are not idempotent");
        for want in ["route 0.0.0.0/1 ", "route 128.0.0.0/1 ", "route ::/1 ", "route 8000::/1 ", "dns 10.77.9.1 ."] {
            ok.that(seen.contains(want), format!("Windows does not report {want:?}"));
        }
        ok.that(seen.contains("to 1.1.1.1: wirebond"), "Find-NetRoute does not send 1.1.1.1 to the adapter");
        ok.that(local.is_ok_and(|a| a == IpAddr::from(a4(2))), "a socket for 1.1.1.1 is not on the adapter");
        ok.that(asked.is_some(), "the system's resolver did not ask 10.77.9.1");
        let after = ps(ADAPTER);
        show("the adapter after capture_remove", &after);
        ok.that(!after.contains("/1 ") && !after.contains("10.77.9.1"), "a capture route or the DNS server remains");
        ok.that(local_for_internet().is_ok_and(|a| a == link.addr), "the internet is not back on the link");
        ok.end();
    }

    /// §5.3, §9.1: under capture a socket made as a lane's is (bound to the link's address, with
    /// IP_UNICAST_IF) still reaches the outside, while an unpinned socket's datagram comes out of
    /// the adapter.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_lane_socket_stays_out_of_capture_and_an_unpinned_one_goes_in() {
        let mut ok = Checks::default();
        let (link, ts) = (primary(), targets());
        let before = ask(&listen(&link).unwrap().0, &ts, Duration::from_secs(2));
        let tun = Tun::open(&tun_config(2)).unwrap();
        let at = Instant::now();
        let mut cap = Captured::install(&capture(&tun, Routes::All, &link)).unwrap();
        // A socket made under capture: it has reached nothing yet.
        let lane = listen(&link).map(|(u, ..)| ask(&u, &ts, Duration::from_secs(2)));
        let there = SocketAddr::from(([1, 1, 1, 1], 9));
        let sent = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|u| u.send_to(b"unpinned", there));
        let seen = from_tun(&tun, Duration::from_secs(1), |_, d, p| d == there && p == b"unpinned");
        let removed = cap.remove();
        eprintln!("capture stayed {:?}; removed: {removed:?}", at.elapsed());
        let reached = answers("a lane socket before capture", &ts, &before);
        ok.that(reached > 0, "no target answers this runner even without capture");
        match lane {
            Ok(under) => {
                let reached = answers("a lane socket made under capture", &ts, &under);
                ok.that(reached > 0, "a lane socket reaches nothing under capture");
            }
            Err(e) => ok.that(false, format!("no lane socket under capture: {e}")),
        }
        eprintln!("an unpinned socket's datagram: sent {sent:?}, out of the device {seen:?}");
        ok.that(seen.is_some_and(|(_, s, ..)| s.ip() == a4(2)), "an unpinned datagram did not enter the adapter");
        ok.end();
    }

    /// §6.2: Internet Connection Sharing shares the uplink to the adapter, which then has
    /// 192.168.137.1, and a client's packet written into the adapter comes back answered from the
    /// internet. A server edition, as GitHub's runner is, may refuse sharing: the anchor then
    /// keeps its bond subnet's address on the adapter, delivers locally and says why, which is
    /// what the owner's PC does should sharing fail there. egress_disable ends the sharing.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_egress_shares_the_uplink_or_keeps_the_bond_address_and_says_why() {
        let mut ok = Checks::default();
        let (link, ts) = (primary(), targets());
        show("sharing before", &ps(SHARING));
        show(
            "the edition",
            &ps(
                "(Get-CimInstance Win32_OperatingSystem).Caption; Get-Service SharedAccess | % { \"SharedAccess $($_.Status) $($_.StartType)\" }",
            ),
        );
        let direct = ask(&UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap(), &ts, Duration::from_secs(2));
        let reached = answers("this device itself, before egress", &ts, &direct);
        let tun = Tun::open(&tun_config(1)).unwrap();
        let e = Egress { tun: tun.name().into(), net4: NET4, net6: None };
        let at = Instant::now();
        let on = egress_enable(&e);
        eprintln!("egress_enable, {:?}: {on:?}", at.elapsed());
        let on = on.unwrap();
        // What the anchor tells its clients is its own address, and where it binds its DNS relay
        // at once: sharing's address is tentative at first, which no socket can bind.
        let bound = UdpSocket::bind((on.addr4.0, 0));
        show("the adapter, egress on", &ps(ADAPTER));
        show("sharing, egress on", &ps(SHARING));
        ok.that(on.uplink.as_deref() == Some(&link.name), format!("the uplink is {:?}, not {}", on.uplink, link.name));
        ok.that(!on.v6, "IPv6 egress, which sharing does not give");
        ok.that(bound.is_ok(), format!("the address egress reports, {}, cannot be bound yet: {bound:?}", on.addr4.0));
        if on.v4 {
            ok.that(
                on.addr4 == (Ipv4Addr::new(192, 168, 137, 1), 24) && on.why.is_none(),
                "sharing, but not its address",
            );
            let client = SocketAddr::from(([192, 168, 137, 7], 40000));
            for t in &ts {
                tun.write(&udp(client, t.at, &t.ask)).unwrap();
            }
            let back = from_tun(&tun, Duration::from_secs(3), |s, d, _| d == client && ts.iter().any(|t| t.at == s));
            eprintln!(
                "forwarded for {client}: the answer out of the device: {:?}",
                back.as_ref().map(|b| (b.0, b.1, b.2))
            );
            ok.that(reached > 0, "no target answers this runner");
            ok.that(back.is_some(), "a client's packet was not forwarded, translated and answered");
            // Sharing brings its own DNS proxy on the shared address: who holds port 53 there, and
            // whether a client's query to the anchor's address is answered with no relay of ours.
            show(
                "port 53",
                &ps(
                    "Get-NetUDPEndpoint -LocalPort 53 -ErrorAction SilentlyContinue | % { \"udp $($_.LocalAddress) $((Get-Process -Id $_.OwningProcess).Name) $($_.OwningProcess)\" }\n\
                 Get-NetTCPConnection -LocalPort 53 -State Listen -ErrorAction SilentlyContinue | % { \"tcp $($_.LocalAddress) $((Get-Process -Id $_.OwningProcess).Name) $($_.OwningProcess)\" }",
                ),
            );
            eprintln!(
                "binding {}:53, {:?} after egress_enable: {:?}",
                on.addr4.0,
                at.elapsed(),
                UdpSocket::bind((on.addr4.0, 53))
            );
            let asker = SocketAddr::from(([192, 168, 137, 7], 40001));
            let relay = SocketAddr::from((on.addr4.0, 53));
            tun.write(&udp(asker, relay, &ts.last().unwrap().ask)).unwrap();
            let answer = from_tun(&tun, Duration::from_secs(3), |s, d, _| d == asker && s == relay);
            eprintln!(
                "a client's query to {relay}: the answer out of the device: {:?}",
                answer.map(|a| (a.0, a.1, a.2))
            );
            let again = egress_enable(&e);
            eprintln!("egress_enable again: {again:?}");
            ok.that(again.is_ok_and(|a| a == on), "enabled twice, two answers");
        } else {
            eprintln!("sharing was refused: {:?}", on.why);
            ok.that(on.why.as_deref().is_some_and(|w| !w.is_empty()), "no egress, and no reason");
            ok.that(on.addr4 == (a4(1), 24), format!("without sharing the anchor's address is {:?}", on.addr4));
        }
        let off = egress_disable(&e);
        eprintln!("egress_disable: {off:?}\nagain: {:?}", egress_disable(&e));
        let sharing = ps(SHARING);
        show("sharing, egress off", &sharing);
        ok.that(off.is_ok(), "egress_disable failed");
        ok.that(!sharing.contains("sharing wirebond: enabled True"), "the adapter is still shared to");
        ok.end();
    }

    /// §7: the anchor's upstream resolver is the first DNS server of the default route's adapter,
    /// and it answers.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_upstream_resolver_answers() {
        let mut ok = Checks::default();
        let link = primary();
        let servers = ps(&format!(
            "Get-DnsClientServerAddress -InterfaceIndex {} | % {{ \"dns $($_.ServerAddresses -join ',') .\" }}",
            link.index
        ));
        show(&format!("{}'s DNS servers", link.name), &servers);
        let up = upstream_resolver();
        eprintln!("upstream_resolver: {up:?}");
        let up = up.unwrap();
        ok.that(servers.contains(&format!("dns {}", up.ip())), format!("{} is not the link's first server", up.ip()));
        let ask_it = [Target { name: "the upstream resolver".into(), at: up, ask: targets().pop().unwrap().ask }];
        let u = UdpSocket::bind(if up.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).unwrap();
        let got = ask(&u, &ask_it, Duration::from_secs(4));
        ok.that(answers("asked for example.com", &ask_it, &got) == 1, "the upstream resolver does not answer");
        ok.end();
    }

    /// §5.4: a daemon that died with capture's DNS server in place took its adapter and routes
    /// with it; cleanup_stale clears the server from the adapter's settings, which Windows keeps
    /// by the adapter's GUID, so the adapter made next starts without it.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_cleanup_after_a_crash() {
        let mut ok = Checks::default();
        let link = primary();
        let key = r"HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{7849B0D0-2E0F-4A7C-9D3E-77B0D0007849}";
        let stored = || out("reg", &["query", key, "/v", "NameServer"]);
        let tun = Tun::open(&tun_config(2)).unwrap();
        // The bond's subnets alone: the DNS server is the part that outlives the process.
        let made = capture_install(&capture(&tun, Routes::BondOnly, &link));
        let set = stored();
        drop(tun); // the crash
        let left = stored();
        let cleaned = cleanup_stale();
        eprintln!("made: {made:?}\ncleanup_stale: {cleaned:?}\nagain: {:?}", cleanup_stale());
        show("the stored DNS server under capture", &set);
        show("the stored DNS server the crash left", &left);
        show("the stored DNS server after cleanup_stale", &stored());
        ok.that(made.is_ok() && cleaned.is_ok(), "capture or the cleanup failed");
        ok.that(set.contains("10.77.9.1"), "capture stored no DNS server");
        ok.that(!stored().contains("10.77.9.1"), "the DNS server remains stored");
        let tun = Tun::open(&tun_config(2)).unwrap();
        let next = ps(ADAPTER);
        show("the adapter made next", &next);
        ok.that(next.contains("adapter wirebond Up") && !next.contains("10.77.9.1"), "the next adapter has the server");
        drop(tun);
        show("sharing after cleanup_stale", &ps(SHARING));
        ok.end();
    }

    /// §9.1: `links` names the adapter of the default route, labelled by its medium as Windows
    /// reports it.
    #[test]
    #[ignore = "changes this machine's network, as root or an Administrator: ci.yml runs it"]
    fn real_links_name_the_default_adapter() {
        let mut ok = Checks::default();
        let link = primary();
        let seen = ps(
            "Get-NetAdapter | % { \"adapter $($_.ifIndex) | $($_.Name) | $($_.InterfaceDescription) | $($_.MediaType) | $($_.PhysicalMediaType) | $($_.Status)\" }\n\
             Find-NetRoute -RemoteIPAddress 1.1.1.1 | % { \"to 1.1.1.1: $($_.InterfaceIndex) | $($_.InterfaceAlias) | $($_.IPAddress) $($_.DestinationPrefix)\" }\n\
             Get-NetIPAddress | % { \"addr $($_.InterfaceAlias) $($_.IPAddress)/$($_.PrefixLength)\" }",
        );
        show("the adapters", &seen);
        let route = format!("to 1.1.1.1: {} | {} | ", link.index, link.name);
        ok.that(
            seen.contains(&route),
            format!("Windows does not route the internet over {} ({})", link.name, link.index),
        );
        let adapter = seen.lines().find(|l| l.starts_with(&format!("adapter {} | ", link.index))).unwrap_or_default();
        let want = if adapter.contains("802.11") { "Wi-Fi" } else { "Ethernet" };
        ok.that(link.label == want && !link.metered, format!("{link:?} is not labelled {want}: {adapter}"));
        ok.end();
    }
}
