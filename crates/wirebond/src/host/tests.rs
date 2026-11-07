//! host's tests: the portable ones first, then Linux's. The network tests want a private
//! namespace with links, as in `unshare -n sh -c 'ip link set lo up && … && cargo test'`; without
//! an IPv4 link they skip. The 2.0 platform tests (TUN, capture, egress, cleanup) also need root
//! and a namespace without a default route, so they never touch a machine's own routing; elsewhere
//! they skip. The install tests run under a temporary root with a fake systemctl, through seams that
//! are per thread because tests run in parallel.

use super::config::DEFAULT_MESH;
use super::*;
use std::cell::RefCell;
use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
pub(super) use on_linux::{rooted, systemctl};

thread_local! {
    static EXE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The file install copies into place: the fake download, else the test binary.
pub(super) fn executable() -> io::Result<PathBuf> {
    EXE.with_borrow(Clone::clone).map_or_else(std::env::current_exe, Ok)
}

/// A new directory under the system's temporary directory, open to all, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        let name = format!("wirebond-{:x}", RandomState::new().hash_one(0));
        let dir = std::env::temp_dir().join(name);
        fs::create_dir(&dir).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        Tmp(dir)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn tailnet() -> Vec<(IpAddr, u8)> {
    Config::default().mesh_ranges()
}

/// One fabricated interface; `kind` holds "up" (with carrier), "p2p" (up, point-to-point),
/// "mac" (a 6-byte hardware address) and "wifi".
fn fab(index: u32, name: &str, kind: &str, mtu: u32, addrs: &[&str]) -> Iface {
    let (up, p2p) = (kind.contains("up") || kind.contains("p2p"), kind.contains("p2p"));
    let (mac, wifi, name) = (kind.contains("mac"), kind.contains("wifi"), name.into());
    let addrs = addrs.iter().map(|a| ip(a)).collect();
    Iface { index, name, up, p2p, mtu, mac, wifi, addrs }
}

fn link(index: u32, name: &str, addr: &str, label: &str, metered: bool) -> Link {
    let (name, addr, label) = (name.into(), ip(addr), label.into());
    Link { index, name, addr, label, metered }
}

#[test]
#[rustfmt::skip]
fn usable_fabricated() {
    let ifs = [
        fab(1, "lo0", "down", 16384, &["127.0.0.1", "::1", "fe80::1"]),
        // Wi-Fi: link-local skipped; a public IPv6 address wins over a ULA, the first public stays.
        fab(4, "en0", "up mac wifi", 1500, &["fe80::1c", "192.168.1.5", "fd00::5", "2001:db8::5", "2001:db8::6"]),
        // The iPhone on USB: both families labelled iPhone and metered.
        fab(9, "en8", "up mac", 1500, &["fe80::8", "172.20.10.2", "2607:fb90::1"]),
        // The mesh interface on macOS: excluded although "utun" would be skipped anyway.
        fab(12, "utun4", "p2p", 1280, &["100.101.1.2", "fd7a:115c:a1e0::1", "fe80::12"]),
        fab(13, "utun0", "p2p", 1380, &["fe80::13"]),
        // A hotel network handing out a 100.64.0.0/10 address stays a link.
        fab(5, "en5", "up mac", 1500, &["100.64.5.5"]),
        // A public IPv4 address wins over a private one.
        fab(6, "en6", "up mac", 1500, &["10.0.0.6", "198.51.100.6", "198.51.100.7"]),
        fab(7, "pdp_ip0", "p2p", 1500, &["192.0.0.2", "192.0.0.7"]), // 464XLAT only
        fab(10, "en9", "down mac", 1500, &["192.168.3.3"]),
        fab(11, "ppp0", "p2p", 1492, &["203.0.113.7"]), // point-to-point, no mesh address
        fab(14, "Tailscale", "up", 1280, &["100.101.1.3"]), // Windows: not point-to-point
        fab(15, "vEthernet (WSL)", "up mac", 1500, &["172.25.0.1"]),
        fab(16, "docker0", "up mac", 1500, &["172.17.0.1"]),
        fab(17, "veth1234", "up mac", 1500, &["172.17.0.5"]),
        fab(18, "bridge0", "up mac", 1500, &["192.168.2.1"]),
        fab(19, "awdl0", "up mac", 1500, &["fe80::a"]),
        fab(20, "wg0", "p2p", 1420, &["10.8.0.2"]),
        fab(21, "ap1", "up mac wifi", 1500, &["192.168.4.1"]),
        // The iPhone's hotspot over Wi-Fi: iPhone wins over Wi-Fi.
        fab(22, "en1", "up mac wifi", 1500, &["172.20.10.3"]),
        // A Hyper-V external switch carries the host's own address and the default route.
        fab(23, "vEthernet (External)", "up mac", 1500, &["192.168.1.20"]),
        // A dual-stack cellular modem whose carrier hands out 100.64.0.0/10: not the mesh.
        fab(24, "wwan0", "p2p", 1500, &["100.70.1.2", "2607:fb90::9"]),
        fab(25, "Teredo Tunneling Pseudo-Interface", "p2p", 1280, &["2001:0:4136:e378::1", "fe80::25"]),
        fab(26, "br-3f2a9c1d7e44", "up mac", 1500, &["172.18.0.1"]), // Docker, libvirt
        fab(27, "virbr0", "up mac", 1500, &["192.168.122.1"]),
        fab(28, "eth1", "down mac", 1500, &["192.168.9.9"]), // up without carrier
    ];
    let (links, mesh) = usable(&ifs, &tailnet(), Some(ip("192.168.1.20")));
    assert_eq!(links, [
        link(4, "en0", "192.168.1.5", "Wi-Fi", false),
        link(4, "en0", "2001:db8::5", "Wi-Fi", false),
        link(9, "en8", "172.20.10.2", "iPhone", true),
        link(9, "en8", "2607:fb90::1", "iPhone", true),
        link(5, "en5", "100.64.5.5", "Ethernet", false),
        link(6, "en6", "198.51.100.6", "Ethernet", false),
        link(11, "ppp0", "203.0.113.7", "ppp0", false),
        link(22, "en1", "172.20.10.3", "iPhone", true),
        link(23, "vEthernet (External)", "192.168.1.20", "Ethernet", false),
        link(24, "wwan0", "100.70.1.2", "wwan0", false),
        link(24, "wwan0", "2607:fb90::9", "wwan0", false),
    ]);
    assert_eq!(mesh, [
        link(12, "utun4", "100.101.1.2", "utun4", false),
        link(12, "utun4", "fd7a:115c:a1e0::1", "utun4", false),
        link(14, "Tailscale", "100.101.1.3", "Tailscale", false),
    ]);
    // Without mesh ranges the mesh interface is an ordinary interface, and utun is skipped.
    assert_eq!(usable(&ifs[3..4], &[], None), (vec![], vec![]));
    // Another mesh, ZeroTier: neither point-to-point nor small, so its range alone marks it.
    let zt = [fab(30, "ztc3q6qpva", "up mac", 2800, &["10.147.17.5", "fe80::30"])];
    let zerotier = [tailnet(), vec![(ip("10.147.17.0"), 24)]].concat();
    let want = vec![link(30, "ztc3q6qpva", "10.147.17.5", "Ethernet", false)];
    assert_eq!(usable(&zt, &zerotier, None), (vec![], want));
}

#[test]
fn prefixes() {
    let mesh = ["100.64.0.0/10", "nonsense", "fd7a:115c:a1e0::1/48", "1.2.3.4/33", "0.0.0.0/0"];
    let mesh = Config { mesh: mesh.map(String::from).into(), ..Config::default() }.mesh_ranges();
    let want = [(ip("100.64.0.0"), 10), (ip("fd7a:115c:a1e0::1"), 48), (ip("0.0.0.0"), 0)];
    assert_eq!(mesh, want);
    for (a, want) in [("100.127.255.255", true), ("100.128.0.0", false), ("fd7a:115c:a1e0:ff::9", true)] {
        assert_eq!(in_ranges(ip(a), &mesh[..2]), want, "{a}");
    }
    assert!(in_ranges(ip("8.8.8.8"), &mesh) && !in_ranges(ip("::1"), &mesh), "/0 is IPv4's");
    assert!(in_ranges(ip("::"), &[(ip("::"), 0)]) && in_ranges(ip("1.2.3.4"), &[(ip("1.2.3.4"), 32)]));
    assert!(!in_ranges(ip("1.2.3.5"), &[(ip("1.2.3.4"), 32)]));
}

#[test]
fn links_real() {
    let (links, mesh) = links(&tailnet()).unwrap();
    eprintln!("links {links:?}\nmesh {mesh:?}");
    let mut seen = std::collections::HashSet::new();
    for l in &links {
        let ok = global(l.addr) && l.addr == l.addr.to_canonical() && !l.label.is_empty();
        assert!(ok && l.index != 0, "bad link {l:?}");
        assert!(seen.insert((l.index, l.addr.is_ipv4())), "two links for one interface and family: {l:?}");
    }
    let lo = os::ifaces().unwrap().into_iter().find(|i| i.name == "lo" && i.up);
    if let Some(lo) = lo {
        let ok = lo.index != 0 && !lo.p2p && lo.mtu == 65536;
        assert!(ok && lo.addrs.contains(&ip("127.0.0.1")), "{lo:?}");
    }
    let Ok(def) = default_addr() else {
        return eprintln!("no default route");
    };
    assert!(links.iter().chain(&mesh).any(|l| l.addr == def), "default address {def} is no link's");
}

#[test]
#[rustfmt::skip]
fn wake() {
    const S: i64 = 1_000_000_000;
    const MS: i64 = 1_000_000;
    let ticks = [
        (S, 1000 * S, false, "the first tick is never a wake"),
        (2 * S, 1001 * S, false, "a tick"),
        (3 * S + 900 * MS, 1002 * S + 900 * MS, false, "a late tick"),
        (4 * S + 900 * MS, 1004 * S + 800 * MS, false, "the gap grew 0.9 s"),
        (5 * S + 900 * MS, 1606 * S, true, "slept 10 min with the monotonic clock stopped"),
        (6 * S + 900 * MS, 1607 * S, false, "the tick after waking"),
        (607 * S, 2207 * S, true, "slept 10 min with the monotonic clock running"),
        (608 * S, 2208 * S, false, "a tick"),
        (611 * S + 100 * MS, 2211 * S + 100 * MS, true, "the monotonic clock advanced 3.1 s"),
        (612 * S, 2213 * S, false, "the gap grew exactly 1 s"),
        (613 * S, 2215 * S + 100 * MS, true, "the gap grew 1.1 s"),
    ];
    let mut w = Wake::default();
    for (mono, boot, wake, why) in ticks {
        assert_eq!(w.tick(mono, boot), wake, "{why}");
    }
}

#[test]
fn boot_clock() {
    let (b0, m0) = (boot(), Instant::now());
    std::thread::sleep(Duration::from_millis(50));
    let (d, m) = (boot() - b0, m0.elapsed().as_nanos() as i64);
    assert!(b0 > 0 && d >= 40_000_000 && d <= m + 100_000_000, "boot {d} ns, Instant {m} ns");
    let mut w = Wake::default();
    w.tick(m0.elapsed().as_nanos() as i64, boot());
    std::thread::sleep(Duration::from_millis(10));
    assert!(!w.tick(m0.elapsed().as_nanos() as i64, boot()), "a wake without sleep");
}

#[test]
fn hostname_is_the_kernels() {
    let name = hostname();
    assert!(!name.is_empty() && !name.contains('\0'), "{name:?}");
    #[cfg(target_os = "linux")]
    assert_eq!(name, fs::read_to_string("/proc/sys/kernel/hostname").unwrap().trim_end());
}

#[test]
fn wait_readable_wakes_for_a_connection() {
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    ln.set_nonblocking(true).unwrap();
    let begin = Instant::now();
    assert!(!wait_readable(&ln, Duration::from_millis(50)).unwrap(), "nothing to accept");
    assert!(begin.elapsed() >= Duration::from_millis(40), "it waited");
    let _c = std::net::TcpStream::connect(ln.local_addr().unwrap()).unwrap();
    let begin = Instant::now();
    assert!(wait_readable(&ln, Duration::from_secs(5)).unwrap(), "a connection waits");
    assert!(begin.elapsed() < Duration::from_secs(1) && ln.accept().is_ok());
}

#[test]
fn run_service_never_stops() {
    // Run by hand, stop never fires. The only test that calls run_service in this process: Windows
    // lets a process call StartServiceCtrlDispatcherW once, and a second call fails with 1056.
    let run = |stop: std::sync::mpsc::Receiver<()>| {
        let waited = stop.recv_timeout(std::time::Duration::from_millis(200));
        assert_eq!(waited, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        Ok(())
    };
    run_service(run).unwrap();
}

#[cfg(unix)]
#[test]
fn run_service_raises_the_descriptor_limit() {
    // Review: fd-limit-and-splice-descriptors. A child of this test binary starts at launchd's
    // soft limit, 256, and run_service raises it to the hard limit, as Go's runtime did (on macOS
    // at most kern.maxfilesperproc).
    let limit = || {
        let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: getrlimit writes only the rlimit r is.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) }, 0);
        (r.rlim_cur, r.rlim_max)
    };
    if std::env::var_os("WIREBOND_TEST_NOFILE").is_some() {
        assert_eq!(limit().0, 256, "the child starts at 256");
        run_service(|_| Ok(())).unwrap();
        let (cur, max) = limit();
        assert!(cur > 256 && cur <= max, "{cur} of {max}");
        #[cfg(target_os = "linux")]
        assert_eq!(cur, max);
        return;
    }
    let child = std::process::Command::new("sh")
        .args(["-c", r#"ulimit -S -n 256 && exec "$0" --exact host::tests::run_service_raises_the_descriptor_limit"#])
        .arg(std::env::current_exe().unwrap())
        .env("WIREBOND_TEST_NOFILE", "1")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"), "the child ran the test");
}

#[test]
fn config() {
    let dir = Tmp::new();
    let file = dir.0.join(CONFIG_FILE);
    let c = Config::load(&dir.0).unwrap();
    assert_eq!(c, Config::default(), "missing config.json");
    assert_eq!(c.update.url, crate::update::DEFAULT_URL);
    assert_eq!((c.v, c.bond_net4.as_str()), (2, "10.77.0.0/24"));
    assert_eq!(c.mesh, DEFAULT_MESH);
    // §22.4's names, and a round trip with no temporary file left.
    let support = super::SupportConfig { uplink: "eth0".into(), cable: "usb0".into(), for_client: Some("mac".into()) };
    let mut c = Config { role: crate::api::Role::Support, support: Some(support), ..Config::default() };
    (c.anchors, c.allow, c.update.starts) = (vec!["xanth".into()], vec!["mac".into()], 1);
    c.save(&dir.0).unwrap();
    let json = fs::read_to_string(&file).unwrap();
    assert!(json.contains("\"role\": \"support\"") && json.contains("\"for\": \"mac\""), "{json}");
    assert!(json.contains("\"mode\": \"performance\"") && !json.contains("made_bin"), "{json}");
    assert_eq!(Config::load(&dir.0).unwrap(), c);
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1, "the state directory holds more");
    // Fields the file leaves out keep their defaults; 1.x's modes do not parse; a broken file is
    // an error, and stays.
    fs::write(&file, r#"{"role": "client", "anchor": "xanth", "update": {"starts": 2}, "unknown": 0}"#).unwrap();
    let c = Config::load(&dir.0).unwrap();
    assert_eq!(
        (c.role, c.anchor.as_deref(), c.update.starts, c.mesh.len()),
        (crate::api::Role::Client, Some("xanth"), 2, 2)
    );
    fs::write(&file, r#"{"mode": "stable"}"#).unwrap();
    assert!(Config::load(&dir.0).is_err());
    fs::write(&file, r#"{"role": "#).unwrap();
    assert!(Config::load(&dir.0).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), r#"{"role": "#);
}

#[cfg(unix)]
#[test]
fn plist_text() {
    let p = unix::plist(Path::new("/Users/a/.local/bin/wirebond"));
    assert!(p.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist"), "{p}");
    // The text between tags, in order.
    let text = p.split('>').filter_map(|s| s.split('<').next()).map(str::trim);
    let text: Vec<_> = text.filter(|t| !t.is_empty()).collect();
    let want = ["Label", "com.wirebond", "ProgramArguments", "/Users/a/.local/bin/wirebond", "run"];
    assert_eq!(text, [&want[..], &["RunAtLoad", "KeepAlive"]].concat());
    assert!(p.contains("<key>RunAtLoad</key><true/>") && p.contains("<key>KeepAlive</key><true/>"));
    assert!(p.ends_with("</dict></plist>\n") && !p.contains("Path"), "{p}");
}

#[cfg(target_os = "linux")]
mod on_linux {
    use super::*;
    use std::fs::Permissions;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::os::unix::prelude::*;
    use std::sync::{Mutex, MutexGuard};

    thread_local! {
        static ROOT: RefCell<String> = const { RefCell::new(String::new()) };
        static CALLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static FAIL: RefCell<&'static str> = const { RefCell::new("") };
    }

    /// An absolute path, under this thread's temporary root.
    pub(in crate::host) fn rooted(path: &str) -> PathBuf {
        ROOT.with_borrow(|root| format!("{root}{path}").into())
    }

    /// Records the call, and fails the one FAIL names.
    pub(in crate::host) fn systemctl(args: &[&str]) -> io::Result<()> {
        let call = args.join(" ");
        let fail = FAIL.with_borrow(|f| *f == call);
        CALLS.with_borrow_mut(|c| c.push(call));
        match fail {
            true => Err(io::Error::other("systemctl failed")),
            false => Ok(()),
        }
    }

    fn calls() -> Vec<String> {
        CALLS.with_borrow_mut(std::mem::take)
    }

    /// install and uninstall on this thread under a temporary root, with a fake download in its
    /// own directory, which the test writes.
    struct Fake {
        root: Tmp,
        src: PathBuf,
        _downloads: Tmp,
    }

    impl Fake {
        fn new() -> Fake {
            let (root, _downloads) = (Tmp::new(), Tmp::new());
            let src = _downloads.0.join("wirebond");
            ROOT.set(root.0.to_string_lossy().into_owned());
            EXE.set(Some(src.clone()));
            Fake { root, src, _downloads }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = (ROOT.take(), EXE.take(), CALLS.take(), FAIL.take());
        }
    }

    /// The umask is the process's, so tests that set it take turns, and so do tests that make
    /// directories: listen_local's 0177 would make one that its owner cannot enter, which only an
    /// unprivileged run notices.
    struct Umask {
        old: libc::mode_t,
        _turn: MutexGuard<'static, ()>,
    }

    impl Umask {
        fn set(m: libc::mode_t) -> Umask {
            static TURN: Mutex<()> = Mutex::new(());
            let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
            Umask { old: umask(m), _turn }
        }
    }

    impl Drop for Umask {
        fn drop(&mut self) {
            umask(self.old);
        }
    }

    fn umask(m: libc::mode_t) -> libc::mode_t {
        // SAFETY: umask only swaps the process's file mode mask.
        unsafe { libc::umask(m) }
    }

    fn mode(p: impl AsRef<Path>) -> u32 {
        fs::symlink_metadata(p).unwrap().mode() & 0o7777
    }

    /// config.json is root's alone, mode 0600 (§22.4), whatever the umask.
    #[test]
    fn config_mode() {
        let dir = Tmp::new();
        {
            let _umask = Umask::set(0o022);
            Config::default().save(&dir.0).unwrap();
        }
        assert_eq!(mode(dir.0.join(CONFIG_FILE)), 0o600);
    }

    #[test]
    fn unit_file() {
        let u = os::unit(Path::new("/usr/local/bin/wirebond"));
        for l in ["[Unit]", "StartLimitIntervalSec=0", "[Service]", "Type=exec", "Restart=always"] {
            assert!(u.contains(&format!("{l}\n")), "the unit lacks {l}:\n{u}");
        }
        for l in ["ExecStart=/usr/local/bin/wirebond run", "RestartSec=1", "WantedBy=multi-user.target"] {
            assert!(u.contains(&format!("\n{l}\n")), "the unit lacks {l}:\n{u}");
        }
    }

    #[test]
    fn wifi_sysfs() {
        let _turn = Umask::set(0o022);
        let fake = Fake::new();
        for d in ["wlan0/phy80211", "wlp2s0/wireless", "eth0"] {
            fs::create_dir_all(fake.root.0.join("sys/class/net").join(d)).unwrap();
        }
        for (name, want) in [("wlan0", true), ("wlp2s0", true), ("eth0", false), ("gone0", false)] {
            assert_eq!(os::wifi(name), want, "{name}");
        }
    }

    /// The bytes of the option getsockopt returns for `s`.
    fn opt(s: &impl AsRawFd, level: libc::c_int, name: libc::c_int) -> Vec<u8> {
        let (mut b, mut n) = ([0u8; 16], 16);
        // SAFETY: b is writable for n bytes, and getsockopt sets n to the length it wrote.
        let r = unsafe { libc::getsockopt(s.as_raw_fd(), level, name, b.as_mut_ptr().cast(), &mut n) };
        assert_eq!(r, 0, "getsockopt({level}, {name}): {}", io::Error::last_os_error());
        b[..n as usize].to_vec()
    }

    fn int(s: &impl AsRawFd, level: libc::c_int, name: libc::c_int) -> [u8; 4] {
        opt(s, level, name).try_into().unwrap()
    }

    #[test]
    fn listen_pins() {
        let (links, _) = links(&[]).unwrap();
        let Some(l) = links.into_iter().find(|l| l.addr.is_ipv4()) else {
            return eprintln!("no IPv4 link");
        };
        let (c1, rcv, snd) = listen(&l).unwrap();
        // IPv4 pins what it sends only, so it still receives what arrives on another interface.
        let (ip4, sol) = (libc::IPPROTO_IP, libc::SOL_SOCKET);
        assert_eq!(opt(&c1, sol, libc::SO_BINDTODEVICE), b"");
        assert_eq!(u32::from_be_bytes(int(&c1, ip4, libc::IP_UNICAST_IF)), l.index);
        let pmtu = i32::from_ne_bytes(int(&c1, ip4, libc::IP_MTU_DISCOVER));
        assert_eq!(pmtu, libc::IP_PMTUDISC_DONT);
        if admin() {
            let want = wirebond_core::SOCK_BUF;
            assert_eq!((rcv, snd), (want, want), "buffers as root");
        }
        let a1 = c1.local_addr().unwrap();
        eprintln!("{} on {}: {a1}, buffers {rcv} {snd}", l.label, l.name);
        assert_eq!(a1.ip(), l.addr);
        // PORT is taken now (by c1, or by another program): the next socket gets an ephemeral port.
        let (c2, _, _) = listen(&l).unwrap();
        let p = c2.local_addr().unwrap().port();
        assert!(p != PORT && p != 0 && p != a1.port(), "second socket on port {p}");
        // The Tailscale lane's socket is not pinned but never sets DF either.
        let (m, _, _) = listen_mesh(ip("127.0.0.1")).unwrap();
        assert_eq!(opt(&m, sol, libc::SO_BINDTODEVICE), b"");
        assert_eq!(int(&m, ip4, libc::IP_UNICAST_IF), [0; 4]);
        let pmtu = i32::from_ne_bytes(int(&m, ip4, libc::IP_MTU_DISCOVER));
        assert_eq!(pmtu, libc::IP_PMTUDISC_DONT);
        // A datagram crosses the unpinned socket, and an address this device lacks is refused.
        let me = m.local_addr().unwrap();
        m.send_to(b"x", me).unwrap();
        assert_eq!(m.recv_from(&mut [0; 4]).unwrap(), (1, me));
        assert!(listen_mesh(ip("192.0.2.200")).is_err(), "bound an address this device lacks");
    }

    #[test]
    fn listen_pins_ipv6() {
        if UdpSocket::bind("[::1]:0").is_err() {
            return eprintln!("no IPv6");
        }
        let lo = link(1, "lo", "::1", "lo", false);
        let (c, _, _) = listen(&lo).unwrap();
        assert_eq!(opt(&c, libc::SOL_SOCKET, libc::SO_BINDTODEVICE), b"lo\0");
        let pmtu = i32::from_ne_bytes(int(&c, libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER));
        assert_eq!(pmtu, libc::IPV6_PMTUDISC_DONT);
    }

    /// Runs `f` with this thread's file-system uid nobody's, which a Unix socket's connect checks.
    fn as_nobody(f: impl FnOnce()) {
        struct Back;
        impl Drop for Back {
            fn drop(&mut self) {
                // SAFETY: setfsuid changes only this thread's file-system uid.
                unsafe { libc::setfsuid(0) };
            }
        }
        // SAFETY: as above.
        if unsafe { libc::setfsuid(65534) } != 0 {
            return eprintln!("not root");
        }
        let _back = Back;
        f();
    }

    #[test]
    fn local_sockets() {
        let top = Tmp::new();
        let dir = top.0.join("state");
        fs::create_dir(&dir).unwrap();
        // As an install under umask 077 left it, with a stale socket file.
        fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.join(CTL_SOCK), "").unwrap();
        let turn = Umask::set(0o077);
        let (_ctl, _status) = listen_local(&dir).unwrap();
        assert_eq!(umask(0o077), 0o077, "umask left changed");
        drop(turn);
        assert_eq!(mode(&dir), 0o755, "the state directory is not open to all");
        for (name, want) in [(CTL_SOCK, 0o600), (STATUS_SOCK, 0o666)] {
            let m = fs::symlink_metadata(dir.join(name)).unwrap();
            assert!(m.file_type().is_socket() && m.mode() & 0o7777 == want, "{name}: {:o}", m.mode());
        }
        // A connect succeeds on the listen backlog, without accept.
        let dial = |name| UnixStream::connect(dir.join(name)).map(|_| ());
        dial(CTL_SOCK).expect("root dials ctl.sock");
        as_nobody(|| {
            dial(STATUS_SOCK).expect("nobody dials status.sock");
            let e = dial(CTL_SOCK).expect_err("nobody dials ctl.sock");
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        });
    }

    /// As root, the state directory and every directory above it must be root's alone, or sticky.
    #[test]
    fn state_guard() {
        if !admin() {
            return eprintln!("not root: the check is root's");
        }
        let _umask = Umask::set(0o077);
        let top = Tmp::new();
        let t = |p: &str| top.0.join(p);
        for d in ["sticky/x", "user/x", "group/x", "theirs"] {
            fs::create_dir_all(t(d)).unwrap();
        }
        fs::set_permissions(t("sticky"), Permissions::from_mode(0o1777)).unwrap();
        fs::set_permissions(t("group"), Permissions::from_mode(0o775)).unwrap();
        std::os::unix::fs::chown(t("user"), Some(65534), Some(65534)).unwrap();
        std::os::unix::fs::chown(t("theirs"), Some(65534), Some(65534)).unwrap();
        std::os::unix::fs::symlink(t("user/x"), t("link")).unwrap();
        for d in ["new/wirebond", "sticky/x"] {
            unix::mk_state(&t(d)).expect(d);
        }
        for d in ["user/x", "group/x", "theirs", "link"] {
            let e = unix::mk_state(&t(d)).expect_err(d).to_string();
            assert!(e.contains("must belong to root"), "{d}: {e}");
        }
        assert_eq!(mode(t("new/wirebond")), 0o755, "made under umask 077");
    }

    /// A copy that fails leaves the download, and nothing beside the executable.
    #[test]
    fn failed_copy_keeps_download() {
        let _turn = Umask::set(0o022);
        let fake = Fake::new();
        let (exe, _) = Scope::System.paths();
        fs::write(&fake.src, "build 1").unwrap();
        fs::create_dir_all(exe.join("in-the-way")).unwrap(); // the rename into place fails
        assert!(install(Scope::System).is_err(), "placed over a directory");
        assert!(fake.src.exists(), "the download is gone");
        let beside = fs::read_dir(exe.parent().unwrap()).unwrap().count();
        assert_eq!(beside, 1, "beside the executable");
    }

    /// A failed register keeps the download, so that the same command can run again (R2-21).
    #[test]
    fn failed_register_keeps_download() {
        let _turn = Umask::set(0o022);
        let fake = Fake::new();
        fs::write(&fake.src, "build 1").unwrap();
        FAIL.set("restart wirebond");
        assert!(install(Scope::System).is_err());
        assert!(fake.src.exists(), "the download is gone");
        FAIL.set("");
        calls();
        assert!(!install(Scope::System).unwrap());
        assert_eq!(calls(), ["start wirebond"]);
        assert!(!fake.src.exists(), "the download remains");
    }

    fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    /// One install from `src`, holding `build` unless empty: what it reports and runs, and the
    /// executable it leaves.
    fn step(src: &Path, name: &str, build: &str, changed: bool, want: &[&str]) {
        let exe = Scope::System.paths().0;
        if !build.is_empty() {
            fs::write(src, build).unwrap();
        }
        let download = fs::metadata(src).map(|m| (m.dev(), m.ino())).unwrap();
        calls();
        assert_eq!(install(Scope::System).unwrap(), changed, "{name}: changed");
        assert_eq!(calls(), want, "{name}: systemctl");
        if !build.is_empty() {
            assert_eq!(fs::read_to_string(&exe).unwrap(), build, "{name}: the executable");
            assert!(!src.exists(), "{name}: the download remains");
        }
        // A new file, never the download renamed: this process's, with its directory's label.
        let m = fs::metadata(&exe).unwrap();
        assert!(m.mode() & 0o7777 == 0o755 && m.uid() == euid(), "{name}: {:o}", m.mode());
        assert!(build.is_empty() || download != (m.dev(), m.ino()), "{name}: the download moved");
    }

    #[test]
    fn install_and_uninstall() {
        let _umask = Umask::set(0o077); // modes do not depend on it
        let mut fake = Fake::new();
        let (exe, dir) = Scope::System.paths();
        let systemd = fake.root.0.join("etc/systemd/system");
        let (unit, wants) = (systemd.join("wirebond.service"), systemd.join("multi-user.target.wants"));
        let restart = ["daemon-reload", "restart wirebond"];
        assert!(!install::has(Scope::System), "installed before the install");
        step(&fake.src, "first install", "build 1", true, &restart);
        for (p, want) in [(&dir, 0o755), (&dir.join(CONFIG_FILE), 0o600), (&unit, 0o644)] {
            assert_eq!(mode(p), want, "{p:?}");
        }
        let c = Config::load(&dir).unwrap();
        assert!(c.made_bin && c.mesh == DEFAULT_MESH, "config.json {c:?}");
        assert_eq!(fs::read_to_string(&unit).unwrap(), os::unit(&exe));
        let link = fs::read_link(wants.join("wirebond.service")).unwrap();
        assert_eq!(link, Path::new("/etc/systemd/system/wirebond.service"));
        assert_eq!((install::has(Scope::System), need(false), need(true)), (true, Scope::System, Scope::System));
        assert!(!on_path(Scope::System), "a temporary root is on PATH");
        Config { anchors: vec!["xanth".into()], ..c }.save(&dir).unwrap();
        step(&fake.src, "the same build again", "build 1", false, &["start wirebond"]);
        EXE.set(Some(exe.clone()));
        step(&exe, "from the installed executable", "", false, &["start wirebond"]);
        EXE.set(Some(fake.src.clone()));
        step(&fake.src, "a new build", "build 2", true, &restart);
        assert_eq!(Config::load(&dir).unwrap().anchors, ["xanth"], "install replaced config.json");
        // A download on another volume is copied, never written into the installed file.
        let shm = Tmp(format!("/dev/shm/wirebond-{}", std::process::id()).into());
        if fs::create_dir(&shm.0).is_ok() {
            fake.src = shm.0.join("wirebond");
            EXE.set(Some(fake.src.clone()));
            step(&fake.src, "a build on another volume", "build 3", true, &restart);
        }
        // The daemon's local sockets, reached through dial.
        let (_ctl, status) = listen_local(&dir).unwrap();
        let answer = std::thread::spawn(move || status.accept().unwrap().write_all(b"hi\n"));
        let mut got = String::new();
        dial(Scope::System, STATUS_SOCK).unwrap().read_to_string(&mut got).unwrap();
        answer.join().unwrap().unwrap();
        assert_eq!(got, "hi\n");
        // A daemon that does not answer: the timeout ends the read.
        let mut c = dial(Scope::System, CTL_SOCK).unwrap();
        c.set_timeout(Some(Duration::from_millis(50))).unwrap();
        let e = c.read(&mut [0; 1]).expect_err("read past the timeout");
        assert!(matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{e}");

        fs::write(exe.with_extension("old"), "").unwrap(); // an update's leftover
        assert_eq!(uninstall(Scope::System).unwrap(), (true, String::new()));
        assert_eq!(calls(), ["stop wirebond", "daemon-reload"]);
        let (old, link, bin) = (exe.with_extension("old"), wants.join("wirebond.service"), exe.parent());
        for p in [&exe, &old, &dir, &unit, &link, bin.unwrap()] {
            assert!(fs::symlink_metadata(p).is_err(), "{p:?} remains");
        }
        assert_eq!(uninstall(Scope::System).unwrap(), (false, String::new()), "uninstalled twice");
        assert!(!install::has(Scope::System), "installed after the uninstall");
    }

    // The 2.0 platform API (§5.3, §5.4, §6.2, §7, §8). The tests that change routes, rules,
    // nftables and sysctls run as root in a private network namespace only, one at a time.

    const FWD4: &str = "/proc/sys/net/ipv4/ip_forward";
    const RP_LO: &str = "/proc/sys/net/ipv4/conf/lo/rp_filter";

    /// The network tests' turn, or None to skip: root, /dev/net/tun, and a namespace without a
    /// default route (a fresh `unshare -n`), decided once before any test adds one. Anywhere else
    /// capture would take the machine's own traffic.
    fn netns() -> Option<MutexGuard<'static, ()>> {
        static TURN: Mutex<()> = Mutex::new(());
        static PRIVATE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let private = *PRIVATE.get_or_init(|| {
            // IPv4 must answer, empty; IPv6 may be missing from the kernel.
            let default = |fam| cmd("ip", &format!("{fam} route show default")).map(|r| r.trim().is_empty());
            admin() && Path::new("/dev/net/tun").exists() && default("-4") == Some(true) && default("-6") != Some(false)
        });
        if !private {
            eprintln!("skipped: needs root in a private network namespace (unshare -n)");
        }
        private.then_some(turn)
    }

    /// A tool's stdout, None when it fails.
    fn cmd(name: &str, args: &str) -> Option<String> {
        os::tool(name, &args.split_whitespace().collect::<Vec<_>>(), None).ok()
    }

    fn sh(name: &str, args: &str) -> String {
        os::tool(name, &args.split_whitespace().collect::<Vec<_>>(), None).unwrap_or_else(|e| panic!("{e}"))
    }

    fn sysctl(path: &str) -> String {
        fs::read_to_string(path).unwrap().trim().into()
    }

    fn v6() -> bool {
        Path::new("/proc/net/if_inet6").exists()
    }

    fn tun_config(name: &str, a4: [u8; 4], a6: &str) -> TunConfig {
        let (addr4, addr6) = (Some((Ipv4Addr::from(a4), 24)), Some((a6.parse().unwrap(), 64)));
        TunConfig { name: name.into(), mtu: wirebond_core::TUN_MTU, addr4, addr6 }
    }

    fn capture(tun: &str, routes: Routes) -> Capture {
        let (bond4, bond6) = (Some((Ipv4Addr::new(10, 77, 0, 0), 24)), Some(("fd77:b0d::".parse().unwrap(), 64)));
        let (exclude, links, dns) = (tailnet(), vec!["lo".into()], Some(ip("10.77.0.1")));
        Capture { tun: tun.into(), routes, bond4, bond6, exclude, links, dns }
    }

    fn egress() -> Egress {
        Egress {
            tun: TUN_NAME.into(),
            net4: (Ipv4Addr::new(10, 77, 0, 0), 24),
            net6: Some(("fd77:b0d::".parse().unwrap(), 64)),
        }
    }

    /// An IPv4/UDP packet without a UDP checksum, which IPv4 allows.
    fn udp4(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, data: &[u8]) -> Vec<u8> {
        let mut p = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 17, 0, 0];
        p[2..4].copy_from_slice(&(28 + data.len() as u16).to_be_bytes());
        p.extend_from_slice(&[src, dst].concat());
        let mut sum: u32 = p.chunks(2).map(|c| u32::from(u16::from_be_bytes([c[0], c[1]]))).sum();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        p[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
        p.extend_from_slice(
            &[sport.to_be_bytes(), dport.to_be_bytes(), (8 + data.len() as u16).to_be_bytes(), [0, 0]].concat(),
        );
        [p, data.to_vec()].concat()
    }

    /// Nothing of wirebond's in the routes, rules or nftables.
    fn assert_clean(what: &str) {
        let fams: &[&str] = if v6() { &["-4", "-6"] } else { &["-4"] };
        for fam in fams {
            let rules = sh("ip", &format!("{fam} rule"));
            assert!(!rules.contains("5299:") && !rules.contains("5300:"), "{what}: {fam} rules remain:\n{rules}");
            let table = cmd("ip", &format!("{fam} route show table 7849")).unwrap_or_default(); // or no table
            assert!(table.trim().is_empty(), "{what}: {fam} table 7849 remains:\n{table}");
        }
        assert!(cmd("nft", "list table inet wirebond").is_none(), "{what}: the nft table remains");
    }

    #[test]
    fn tun_writes_and_reads_packets() {
        let Some(_turn) = netns() else { return };
        let tun = Tun::open(&tun_config("wbt0", [10, 77, 9, 2], "fd77:b0d:9::2")).unwrap();
        assert_eq!(tun.name(), "wbt0");
        let show = sh("ip", "addr show dev wbt0");
        assert!(show.contains("mtu 1400") && show.contains("inet 10.77.9.2/24") && show.contains("UP"), "{show}");
        let mut b = [0u8; 1500];
        assert_eq!(tun.read(&mut b).unwrap_err().kind(), io::ErrorKind::WouldBlock, "a packet before any was sent");
        // Out: a datagram for the device's subnet leaves through it, one IPv4 packet per read.
        let u = UdpSocket::bind("10.77.9.2:0").unwrap();
        u.send_to(b"ping", "10.77.9.9:9").unwrap();
        let mut poll = Poll::default();
        poll.add_tun(&tun);
        let n = (0..50)
            .find_map(|_| {
                poll.wait(Duration::from_millis(100)).unwrap();
                tun.read(&mut b).ok().filter(|_| b[0] >> 4 == 4 && b[9] == 17)
            })
            .expect("no packet out of the device");
        assert_eq!((&b[12..16], &b[16..20], &b[n - 4..n]), (&[10, 77, 9, 2][..], &[10, 77, 9, 9][..], &b"ping"[..]));
        // In: a packet written to it reaches the socket its destination names.
        let port = u.local_addr().unwrap().port();
        tun.write(&udp4([10, 77, 9, 9], [10, 77, 9, 2], 9999, port, b"pong")).unwrap();
        u.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut r = [0u8; 8];
        let (m, from) = u.recv_from(&mut r).unwrap();
        assert_eq!((&r[..m], from), (&b"pong"[..], "10.77.9.9:9999".parse().unwrap()));
        drop(tun);
        assert!(cmd("ip", "link show dev wbt0").is_none(), "the device outlived its descriptor");
        let bad = TunConfig { name: "no/such".into(), ..tun_config("x", [10, 77, 9, 2], "::1") };
        assert_eq!(Tun::open(&bad).err().map(|e| e.kind()), Some(io::ErrorKind::InvalidInput));
    }

    #[test]
    fn capture_installs_and_removes() {
        let Some(_turn) = netns() else { return };
        let _umask = Umask::set(0o022);
        let _fake = Fake::new(); // the saved values go under its root
        cleanup_stale().unwrap();
        fs::write(RP_LO, "1").unwrap();
        let tun = Tun::open(&tun_config(TUN_NAME, [10, 77, 0, 2], "fd77:b0d::2")).unwrap();
        let all = capture(tun.name(), Routes::All);
        capture_install(&all).unwrap();
        capture_install(&all).unwrap(); // idempotent: one copy of each rule
        let fams: &[&str] = if v6() { &["-4", "-6"] } else { &["-4"] };
        for fam in fams {
            let rules = sh("ip", &format!("{fam} rule"));
            for want in
                ["5299:\tfrom all lookup main suppress_prefixlength 0", "5300:\tnot from all fwmark 0x7849 lookup 7849"]
            {
                assert_eq!(rules.matches(want).count(), 1, "{fam} rule {want:?}:\n{rules}");
            }
            let table = sh("ip", &format!("{fam} route show table 7849"));
            let mesh = if *fam == "-4" { "throw 100.64.0.0/10" } else { "throw fd7a:115c:a1e0::/48" };
            assert!(table.contains("default dev wb0") && table.contains(mesh), "{fam} table 7849:\n{table}");
        }
        assert!(sh("ip", "-4 route show table 7849").contains("throw 224.0.0.0/4"));
        // What it means: the internet goes into wb0; a lane packet's mark and the mesh stay out.
        assert!(sh("ip", "route get 1.1.1.1").contains("dev wb0"));
        assert!(
            cmd("ip", "route get 1.1.1.1 mark 0x7849").is_none_or(|r| !r.contains("wb0")),
            "a lane packet captured"
        );
        assert!(cmd("ip", "route get 100.100.1.1").is_none_or(|r| !r.contains("wb0")), "the mesh captured");
        assert_eq!(sysctl(RP_LO), "2", "a lane link's rp_filter");
        if cmd("resolvectl", "--version").is_none() {
            assert!(dns_local(), "DNS set without systemd-resolved");
        }
        capture_remove(&all).unwrap();
        capture_remove(&all).unwrap(); // idempotent
        assert_clean("after capture_remove");
        assert_eq!((sysctl(RP_LO), dns_local()), ("1".into(), false));
        // No egress at the anchor: only the bond's subnets go into wb0.
        let bond_only = capture(tun.name(), Routes::BondOnly);
        capture_install(&bond_only).unwrap();
        let table = sh("ip", "-4 route show table 7849");
        assert!(table.contains("10.77.0.0/24 dev wb0") && !table.contains("default"), "bond only:\n{table}");
        capture_remove(&bond_only).unwrap();
        assert_clean("after the bond-only capture");
        fs::write(RP_LO, "0").unwrap();
    }

    #[test]
    fn egress_enables_and_disables() {
        let Some(_turn) = netns() else { return };
        let _umask = Umask::set(0o022);
        let _fake = Fake::new();
        cleanup_stale().unwrap();
        fs::write(FWD4, "0").unwrap();
        let _tun = Tun::open(&tun_config(TUN_NAME, [10, 77, 0, 1], "fd77:b0d::1")).unwrap();
        sh("ip", "-4 route add default dev lo"); // an uplink for it to find
        let on = egress_enable(&egress()).unwrap();
        let want =
            EgressOn { v4: true, v6: false, addr4: (Ipv4Addr::new(10, 77, 0, 1), 24), uplink: Some("lo".into()) };
        assert_eq!(on, want);
        assert_eq!(egress_enable(&egress()).unwrap(), want, "enabled twice");
        let t = sh("nft", "list table inet wirebond");
        for want in [
            "type filter hook forward priority filter; policy accept;",
            "iifname \"wb0\" oifname \"wb0\" drop",
            "iifname \"wb0\" ip saddr 10.77.0.0/24 accept",
            "iifname \"wb0\" ip6 saddr fd77:b0d::/64 accept",
            "iifname \"wb0\" drop",
            "oifname \"wb0\" ct state established,related accept",
            "type nat hook postrouting priority srcnat; policy accept;",
            "oifname != \"wb0\" ip saddr 10.77.0.0/24 masquerade",
        ] {
            assert!(t.contains(want) || (want.contains("ip6") && !v6()), "the table lacks {want:?}:\n{t}");
        }
        assert_eq!(sysctl(FWD4), "1");
        egress_disable(&egress()).unwrap();
        egress_disable(&egress()).unwrap(); // idempotent
        assert_clean("after egress_disable");
        assert_eq!(sysctl(FWD4), "0", "ip_forward not restored");
        sh("ip", "-4 route del default dev lo");
    }

    #[test]
    fn cleanup_after_a_crash() {
        let Some(_turn) = netns() else { return };
        let _umask = Umask::set(0o022);
        let _fake = Fake::new();
        cleanup_stale().unwrap();
        fs::write(FWD4, "0").unwrap();
        fs::write(RP_LO, "1").unwrap();
        // The daemon crashes with capture and egress on: nothing is removed. Its descriptor
        // closes, which takes the device and table 7849's routes; a persistent wb0 stays.
        let tun = Tun::open(&tun_config(TUN_NAME, [10, 77, 0, 2], "fd77:b0d::2")).unwrap();
        capture_install(&capture(TUN_NAME, Routes::All)).unwrap();
        egress_enable(&egress()).unwrap();
        drop(tun);
        sh("ip", "tuntap add dev wb0 mode tun");
        sh("ip", "addr add 10.77.0.2/24 dev wb0");
        sh("ip", "link set dev wb0 up");
        sh("ip", "-4 route add default dev wb0 table 7849");
        assert!(sh("ip", "-4 rule").contains("5300:"));
        cleanup_stale().unwrap();
        assert_clean("after cleanup_stale");
        assert!(cmd("ip", "link show dev wb0").is_none(), "wb0 remains");
        assert_eq!((sysctl(FWD4), sysctl(RP_LO)), ("0".into(), "1".into()), "sysctls not restored");
        cleanup_stale().unwrap(); // nothing left: still fine
        fs::write(RP_LO, "0").unwrap();
    }

    #[test]
    fn missing_tools_are_named() {
        let e = os::tool("wirebond-no-such-tool", &[], None).unwrap_err();
        assert_eq!((e.kind(), missing_tool(&e).as_deref()), (io::ErrorKind::NotFound, Some("wirebond-no-such-tool")));
        assert_eq!(missing_tool(&io::Error::other("ip rule add: RTNETLINK answers: File exists")), None);
        assert!(missing_tools().iter().all(|t| ["ip", "nft"].contains(t)));
        let e = os::tool("ip", &["no-such-object"], None).unwrap_err();
        assert!(missing_tool(&e).is_none() && e.to_string().starts_with("ip no-such-object: "), "{e}");
    }

    #[test]
    fn nft_ruleset_text() {
        let e = Egress {
            net4: (Ipv4Addr::new(10, 77, 0, 9), 24),
            net6: Some(("fd77:b0d::5".parse().unwrap(), 64)),
            ..egress()
        };
        let r = os::ruleset(&e, true);
        assert!(r.starts_with("table inet wirebond\ndelete table inet wirebond\ntable inet wirebond {\n"), "{r}");
        for want in
            ["ip saddr 10.77.0.0/24 accept", "ip6 saddr fd77:b0d::/64 accept", "ip6 saddr fd77:b0d::/64 masquerade"]
        {
            assert!(r.contains(want), "{want:?}:\n{r}");
        }
        assert!(!os::ruleset(&e, false).contains("ip6"), "IPv6 rules without IPv6");
    }

    #[test]
    fn resolv_conf_first_nameserver() {
        let text = "# made by dhcp\nsearch lan\nnameserver bogus\n; nameserver 9.9.9.9\nnameserver 192.168.1.1 # router\nnameserver 8.8.8.8\n";
        assert_eq!(os::first_nameserver(text), Some("192.168.1.1:53".parse().unwrap()));
        assert_eq!(os::first_nameserver("nameserver 2001:db8::53\n"), Some("[2001:db8::53]:53".parse().unwrap()));
        let zoned = std::net::SocketAddrV6::new("fe80::1".parse().unwrap(), 53, 0, 1);
        assert_eq!(os::first_nameserver("nameserver fe80::1%lo\n"), Some(SocketAddr::V6(zoned)));
        assert_eq!(os::first_nameserver("search lan\noptions edns0\n"), None);
        let _umask = Umask::set(0o022);
        let fake = Fake::new();
        fs::create_dir_all(fake.root.0.join("etc")).unwrap();
        fs::write(fake.root.0.join("etc/resolv.conf"), "nameserver 127.0.0.53\noptions edns0\n").unwrap();
        assert_eq!(upstream_resolver().unwrap(), "127.0.0.53:53".parse().unwrap());
        fs::write(fake.root.0.join("etc/resolv.conf"), "search lan\n").unwrap();
        assert_eq!(upstream_resolver().unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    /// Without NetworkManager, support on fails with missing_tool nmcli and the manual steps, and
    /// support off has nothing to undo (§8). With it, the test would change this machine: skipped.
    #[test]
    fn support_without_networkmanager() {
        if cmd("nmcli", "--version").is_some() {
            return eprintln!("skipped: nmcli is installed");
        }
        let _umask = Umask::set(0o022);
        let _fake = Fake::new();
        let s = Share { uplink: "wlan0".into(), cable: "eth1".into() };
        let e = support_share(&s).unwrap_err();
        assert_eq!(missing_tool(&e).as_deref(), Some("nmcli"), "{e}");
        assert!(e.to_string().contains("To share wlan0 to eth1 by hand"), "{e}");
        support_unshare(&s).unwrap();
    }
}
