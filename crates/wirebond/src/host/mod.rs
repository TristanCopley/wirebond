//! Per-OS code: link listing and lane-socket pinning, never-DF, socket buffers, the
//! sleep-inclusive clock, service install, the firewall rule, paths, local sockets and
//! config.json, kept from 1.x; and the 2.0 platform API (SPEC 2.0 §5–§8): the TUN device,
//! capture, egress and NAT, the upstream resolver, support sharing and stale-state cleanup, whose
//! types and contract are in net.rs and whose bodies are per OS (linux.rs, macos.rs, windows.rs).
//! Owners: host linux (linux.rs, unix.rs); host macos and windows (macos.rs, windows.rs); shared
//! files (this one, net.rs, install.rs) by request (notes/2.0-build.md).
//!
//! node, the daemon, calls [`links`] every LINK_SCAN and at once on a send error or a wake,
//! [`default_addr`] for the default link (§6.1), [`listen`] and [`listen_mesh`] for the lane
//! sockets and [`Wake::tick`] on its 1 s tick. As `wirebond run` it runs inside [`run_service`],
//! takes its scope from `need(false)` and opens [`listen_local`] in that scope's state directory,
//! where [`Config::load`] and [`Config::save`] keep config.json. The CLI calls [`need`], [`admin`],
//! [`on_path`], [`install`], [`uninstall`] and [`dial`].
//!
//! The shared code calls `os::<function>`, one set per OS: linux.rs and macos.rs, each completed
//! by unix.rs, and windows.rs. The set is Go's per-OS set in snake_case, plus `ifaces`, the
//! interface listing Go took from its standard library (Wi-Fi included), and `no_reset`, which
//! Go's runtime did on Windows. The local sockets are AF_UNIX on every OS, Windows 10 included, so
//! they are shared.
//!
//! ```text
//! ifaces() -> io::Result<Vec<Iface>>          boot() -> i64          admin() -> bool
//! sockopts(&Socket, v6: bool, index: u32, name: &str) -> io::Result<(usize, usize)>, before bind
//! no_reset(&Socket)                           hostname() -> String
//! paths(Scope) -> (PathBuf, PathBuf)          mk_state(dir: &Path) -> io::Result<()>
//! private() -> a guard that restores on drop  public(path: &Path) -> io::Result<()>
//! register(Scope, exe: &Path, restart: bool) -> io::Result<bool>
//! unregister(Scope, exe: &Path) -> io::Result<(bool, String)>
//! halt(Scope)    drop(download: &Path)        remove_exe(exe: &Path) -> io::Result<bool>
//! run_service(impl FnOnce(Receiver<()>) -> io::Result<()>) -> io::Result<()>
//! poll_fd(&socket) -> PollFd                  poll(&mut [PollFd], Duration) -> io::Result<()>
//! wake_pair() -> io::Result<(WakeSock, WakeSock)>, nonblocking
//! recv_now(&UdpSocket, &mut [u8]) -> io::Result<(usize, SocketAddr)>, never blocking
//! ```

use socket2::{Domain, SockAddr, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use wirebond_core::{PORT, WAKE_GAP, WAKE_JUMP};

mod config;
// The 2.0 platform API's types (§5–§8); the functions are per OS, below.
#[allow(dead_code)] // 2.0 wave 2: the daemon's pumps and bond logic use them
mod net;
// Only the CLI installs and uninstalls, and the CLI's tests fake both: a test build on macOS,
// which has no install tests of its own (Linux's use a fake systemctl), calls neither.
#[cfg_attr(all(test, target_os = "macos"), allow(dead_code))]
mod install;
// The per-OS set: one file per OS, as `os`.
#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(target_os = "macos", path = "macos.rs")]
#[cfg_attr(windows, path = "windows.rs")]
mod os;
#[cfg(test)]
mod tests;
#[cfg(unix)]
mod unix;

pub use config::{Config, SupportConfig};
#[allow(unused_imports)] // 2.0 wave 2: the daemon's pumps and bond logic use them
pub use net::{Capture, Egress, EgressOn, NOT_YET, Routes, Share, TUN_NAME, TunConfig, missing_tool};
// Linux's only: the tools absent at start, and DNS left alone at the last capture (net.rs).
#[cfg(target_os = "linux")]
#[allow(unused_imports)] // 2.0 wave 2: the daemon reports them as problems
pub use os::{dns_local, missing_tools};
/// The tools this OS needs that are absent: none outside Linux (net.rs).
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)] // 2.0 wave 2: the daemon reports them as problems
pub fn missing_tools() -> Vec<&'static str> {
    Vec::new()
}
/// Whether the last capture left DNS alone: never outside Linux (net.rs).
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)] // 2.0 wave 2: the daemon reports it as a problem
pub fn dns_local() -> bool {
    false
}
// The 2.0 platform API per OS (host/net.rs): the TUN device, capture, egress, the upstream resolver,
// support sharing and stale-state cleanup.
#[allow(unused_imports)] // on_path: 1.x's macOS user scope, which goes with the login agent (open)
pub use install::{create, install, need, on_path, uninstall};
#[allow(unused_imports)] // 2.0 wave 2
pub use os::{
    Tun, capture_install, capture_remove, cleanup_stale, egress_disable, egress_enable, support_share, support_unshare,
    upstream_resolver,
};
// Per OS, one signature everywhere: whether this process may install a system service (root, or
// elevated on Windows); the sleep-inclusive clock in nanoseconds (Linux CLOCK_BOOTTIME, macOS
// CLOCK_MONOTONIC, Windows GetTickCount64); the host name; Windows' UDP resets turned off, which
// node calls on its forward, server flow and fallback UDP sockets (`no_reset(&SockRef::from(&u))`),
// a no-op elsewhere; the next datagram waiting on a UDP socket, without blocking, for node's loop
// (Poll below); and the daemon as the service manager's entry point, `wirebond run`, where `stop`
// fires on a Windows stop request and SIGTERM from launchd or systemd ends the process elsewhere.
#[allow(unused_imports)] // boot, no_reset, recv_now: 2.0 wave 2 (wake detection, the lane pump)
pub use os::{admin, boot, hostname, no_reset, recv_now, run_service};
// Windows' wait (§4.3): WSAEventSelect, Wintun's read event and a waitable timer.
#[cfg(windows)]
#[allow(unused_imports)] // 2.0 wave 2: the daemon's loop on Windows
pub use os::EventLoop;
// Windows' 1 ms timers, which `run_service` asks for the daemon; node's daemon tests ask too.
#[cfg(all(windows, test))]
pub use os::fine_timers;

/// Where wirebond runs (§15.3): a system service started at boot, or a macOS login agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

impl Scope {
    /// "system" or "user", as status prints it.
    pub fn as_str(self) -> &'static str {
        if self == Scope::System { "system" } else { "user" }
    }

    /// The installed executable's path and the state directory for this scope.
    pub fn paths(self) -> (PathBuf, PathBuf) {
        os::paths(self)
    }
}

/// The files in the state directory (§15.3).
pub const CONFIG_FILE: &str = "config.json";
pub const CTL_SOCK: &str = "ctl.sock"; // changes: only the scope's owner can connect
pub const STATUS_SOCK: &str = "status.sock"; // reads only: anyone can connect

/// One usable link, one interface and one address family (§6.1), or one mesh address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The interface's index; an iPhone on USB gets a new one on every reconnect.
    pub index: u32,
    /// The OS's interface name.
    pub name: String,
    /// The address the link's socket binds.
    pub addr: IpAddr,
    /// "iPhone", "Wi-Fi", "Ethernet" or the name.
    pub label: String,
    /// An iPhone hotspot: the only metered heuristic (§6.2).
    pub metered: bool,
}

/// One interface as the OS lists it, the input of [`usable`].
#[derive(Clone, Debug, Default)]
struct Iface {
    index: u32,
    name: String,
    /// Up and with carrier.
    up: bool,
    p2p: bool,
    /// 0 when unknown, which counts as under 1,500 (Go's -1).
    mtu: u32,
    /// A 6-byte hardware address.
    mac: bool,
    wifi: bool,
    addrs: Vec<IpAddr>,
}

const IPHONE: (IpAddr, u8) = (IpAddr::V4(Ipv4Addr::new(172, 20, 10, 0)), 28);
/// Tailscale's, but a hotel or a carrier may hand it out too.
const CGNAT: (IpAddr, u8) = (IpAddr::V4(Ipv4Addr::new(100, 64, 0, 0)), 10);
/// Never a link's address: T-Mobile's 464XLAT address, which no peer can reach, and Teredo's and
/// 6to4's, which reach peers only through public relays.
const NEVER: [(IpAddr, u8); 3] = [
    (IpAddr::V4(Ipv4Addr::new(192, 0, 0, 0)), 29),
    (IpAddr::V6(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0)), 32),
    (IpAddr::V6(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0)), 16),
];
/// Never links: tunnels (wirebond's own TUN first: a lane over it would ride inside the bond),
/// Apple's own links, and the bridges and switches of VMs and containers.
/// vEthernet is Windows' Hyper-V and WSL switch, which xanth has.
const SKIP: &str = "utun ipsec gif stf awdl llw anpi ap bridge br- virbr lxdbr incusbr podman cni \
                    flannel cali tun tap wg docker veth vmnet vEthernet wb0 wirebond";

/// This device's usable links and its mesh addresses (§6.2). `mesh` is config.json's ranges
/// ([`Config::mesh_ranges`]). No mesh address means the mesh is down. A failed listing is an
/// error, never an empty list (R1-31): node keeps the links it has and lists again.
pub fn links(mesh: &[(IpAddr, u8)]) -> io::Result<(Vec<Link>, Vec<Link>)> {
    Ok(usable(&os::ifaces()?, mesh, default_addr().ok()))
}

/// Picks from `ifs` the links, at most one per interface and family (a public address before a
/// private one), and the mesh interface's addresses (§6.2). `def` is the default route's local
/// address.
///
/// Only an interface that is up and has carrier counts. An address in a mesh range makes its
/// interface the mesh interface, which is never a link. One in 100.64.0.0/10 does so only on an
/// interface that is point-to-point or has an MTU under 1,500 and holds no global address outside
/// the mesh ranges, as Tailscale's: a hotel Wi-Fi or a carrier's modem may hand out such an address
/// too. A link's name is outside SKIP, except a vEthernet adapter holding `def`: a Hyper-V external
/// switch moves the host's own address there.
fn usable(ifs: &[Iface], mesh: &[(IpAddr, u8)], def: Option<IpAddr>) -> (Vec<Link>, Vec<Link>) {
    let (mut links, mut meshes) = (Vec::new(), Vec::new());
    for ifc in ifs {
        let metered = ifc.addrs.iter().any(|&a| in_ranges(a, &[IPHONE]));
        let label = match () {
            _ if metered => "iPhone",
            _ if ifc.wifi => "Wi-Fi",
            _ if ifc.mac && !ifc.p2p => "Ethernet",
            _ => &ifc.name,
        };
        let outside = |&a: &IpAddr| global(a) && !in_ranges(a, mesh);
        let tunnel = (ifc.p2p || ifc.mtu < 1500) && !ifc.addrs.iter().any(outside);
        let (mut own, mut ms) = (Vec::<Link>::new(), Vec::new());
        for &addr in &ifc.addrs {
            let l = Link { index: ifc.index, name: ifc.name.clone(), addr, label: label.into(), metered };
            match own.iter().position(|o| o.addr.is_ipv4() == addr.is_ipv4()) {
                _ if in_ranges(addr, mesh) && (tunnel || !in_ranges(addr, &[CGNAT])) => ms.push(l),
                _ if !global(addr) || in_ranges(addr, &NEVER) => {} // loopback, link-local and the like
                None => own.push(l),
                Some(i) if is_private(own[i].addr) && !is_private(addr) => own[i] = l,
                Some(_) => {}
            }
        }
        let switch = ifc.name.starts_with("vEthernet") && def.is_some_and(|d| ifc.addrs.contains(&d));
        if ifc.up && !ms.is_empty() {
            meshes.append(&mut ms);
        } else if ifc.up && (switch || !SKIP.split_whitespace().any(|p| ifc.name.starts_with(p))) {
            links.append(&mut own);
        }
    }
    (links, meshes)
}

/// Whether `a` is in one of `ranges`, each an address and a prefix length.
pub fn in_ranges(a: IpAddr, ranges: &[(IpAddr, u8)]) -> bool {
    ranges.iter().any(|&(net, bits)| {
        let (x, w) = match (a, net) {
            (IpAddr::V4(a), IpAddr::V4(n)) => (u128::from(a.to_bits() ^ n.to_bits()), 32),
            (IpAddr::V6(a), IpAddr::V6(n)) => (a.to_bits() ^ n.to_bits(), 128),
            _ => return false,
        };
        x.checked_shr(w - u32::from(bits).min(w)).unwrap_or(0) == 0
    })
}

/// Go's IsGlobalUnicast: not unspecified, loopback, multicast, broadcast or link-local. node's
/// filter keeps a peer's candidate only on such an address (§6.3 step 3), once unmapped.
pub fn global(a: IpAddr) -> bool {
    let local = matches!(a, IpAddr::V4(a) if a.is_broadcast() || a.is_link_local())
        || matches!(a, IpAddr::V6(a) if a.is_unicast_link_local());
    !(local || a.is_unspecified() || a.is_loopback() || a.is_multicast())
}

/// RFC 1918 or a ULA.
fn is_private(a: IpAddr) -> bool {
    matches!(a, IpAddr::V4(a) if a.is_private()) || matches!(a, IpAddr::V6(a) if a.is_unique_local())
}

/// The default route's local address (a UDP socket connected to 1.1.1.1:53; nothing is sent).
/// Under an exit node it is a mesh address, not a link's.
pub fn default_addr() -> io::Result<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    s.connect((Ipv4Addr::new(1, 1, 1, 1), 53))?;
    Ok(s.local_addr()?.ip())
}

/// A link's UDP socket (§5.4, §6.2): bound to its address on PORT, else an ephemeral port, pinned
/// to its interface before bind, never-DF, asking for SOCK_BUF buffers. Returns the socket and the
/// receive and send buffer sizes the OS granted, for status to report when they fall short. node
/// reopens it when the link's index or address changes.
pub fn listen(link: &Link) -> io::Result<(UdpSocket, usize, usize)> {
    bind(link.addr, link.index, &link.name)
}

/// The Tailscale lane's socket on a mesh address: like `listen`, not pinned.
pub fn listen_mesh(addr: IpAddr) -> io::Result<(UdpSocket, usize, usize)> {
    bind(addr, 0, "")
}

fn bind(addr: IpAddr, index: u32, name: &str) -> io::Result<(UdpSocket, usize, usize)> {
    let open = |port| {
        let at = SocketAddr::new(addr, port);
        let s = Socket::new(Domain::for_address(at), Type::DGRAM, None)?;
        let (rcv, snd) = os::sockopts(&s, addr.is_ipv6(), index, name)?;
        s.bind(&at.into())?;
        Ok((s.into(), rcv, snd))
    };
    open(PORT).or_else(|_: io::Error| open(0))
}

/// The sockets node's loop waits on together (ppoll on Linux, poll on macOS, WSAPoll): every UDP
/// socket it reads, which it then reads with [`recv_now`], and its [`Waker`]. A datagram wakes the
/// loop itself, as Go's netpoller wakes the goroutine that reads it.
#[derive(Default)]
pub struct Poll {
    fds: Vec<os::PollFd>,
}

impl Poll {
    /// Empties the set: the next socket added is number 0.
    pub fn clear(&mut self) {
        self.fds.clear();
    }

    /// Adds u, numbered by how many came before it.
    pub fn add(&mut self, u: &UdpSocket) {
        self.fds.push(os::poll_fd(u));
    }

    /// Adds the TUN device as `add` adds a socket (§4.3). Windows waits on [`Tun::read_event`] instead
    /// (WaitForMultipleObjects, 2.0 wave 2).
    #[cfg(unix)]
    #[allow(dead_code)] // 2.0 wave 2: the TUN pump
    pub fn add_tun(&mut self, t: &Tun) {
        self.fds.push(os::poll_fd(t));
    }

    /// Adds w as `add` adds a socket.
    pub fn add_waker(&mut self, w: &Waker) {
        self.fds.push(os::poll_fd(&w.rx));
    }

    /// Waits until a socket has something to read or `t` has passed, never less (but for a signal).
    pub fn wait(&mut self, t: Duration) -> io::Result<()> {
        os::poll(&mut self.fds, t)
    }

    /// Whether socket i had something to read at the last wait.
    pub fn ready(&self, i: usize) -> bool {
        self.fds.get(i).is_some_and(|p| p.revents != 0)
    }
}

/// Ends node's loop's [`Poll::wait`] from any thread: a byte on a socket pair, sent only when none
/// waits since the loop last looked (`reset`).
pub struct Waker {
    tx: os::WakeSock,
    rx: os::WakeSock,
    armed: AtomicBool,
}

impl Waker {
    pub fn new() -> io::Result<Waker> {
        os::wake_pair().map(|(tx, rx)| Waker { tx, rx, armed: AtomicBool::new(false) })
    }

    /// Ends the loop's wait, or its next one. It never blocks.
    pub fn wake(&self) {
        if !self.armed.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(&[1]);
        }
    }

    /// The loop, woken, before it looks at what other threads gave it: the next `wake` sends.
    pub fn reset(&self) {
        while self.rx.recv(&mut [0; 8]).is_ok() {}
        self.armed.swap(false, Ordering::AcqRel); // acquires what each wake's thread queued
    }
}

/// Detects a wake from sleep on node's 1 s tick (§8.4). The default value is ready.
#[derive(Clone, Copy, Debug, Default)]
pub struct Wake {
    mono: i64,
    boot: i64,
}

impl Wake {
    /// Takes the monotonic clock and [`boot`], read together, and reports a wake since the
    /// previous tick: the gap between them grew by more than WAKE_GAP (the monotonic clock stopped
    /// in sleep), or the monotonic clock advanced by more than WAKE_JUMP (it did not stop). The
    /// wall clock is never read, so an NTP step is not a wake.
    pub fn tick(&mut self, mono: i64, boot: i64) -> bool {
        let gap = (boot - mono) - (self.boot - self.mono);
        let woke = self.mono != 0 && (gap > WAKE_GAP || mono - self.mono > WAKE_JUMP);
        *self = Wake { mono, boot };
        woke
    }
}

/// A local socket listener (§15.3): a Unix socket, which Windows 10 has too.
pub struct LocalListener(Socket);

/// One connection on a local socket.
pub trait LocalStream: io::Read + io::Write + Send {
    /// Bounds every read and write from now on, as Go's deadlines bound both ends of §15.3's one
    /// request line and one reply line; None waits forever.
    fn set_timeout(&self, t: Option<Duration>) -> io::Result<()>;
}

impl LocalStream for Socket {
    fn set_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(t)?;
        self.set_write_timeout(t)
    }
}

impl LocalListener {
    pub fn accept(&self) -> io::Result<Box<dyn LocalStream>> {
        Ok(Box::new(self.0.accept()?.0))
    }
}

/// The daemon's two local sockets in the state directory `dir`, which it creates or checks first
/// (§15.3): ctl.sock (only the scope's owner) and status.sock (anyone), each with its permissions
/// from the moment it exists. Stale socket files go first. Call it before starting threads that
/// create files: on macOS and Linux it sets the process umask while it binds.
pub fn listen_local(dir: &Path) -> io::Result<(LocalListener, LocalListener)> {
    os::mk_state(dir)?;
    let _umask = os::private();
    let (ctl, status) = (dir.join(CTL_SOCK), dir.join(STATUS_SOCK));
    let _ = (std::fs::remove_file(&ctl), std::fs::remove_file(&status));
    let (ctl, listener) = (local(&ctl, true)?, local(&status, true)?);
    os::public(&status)?;
    Ok((LocalListener(ctl), LocalListener(listener)))
}

/// Connects to the local socket `name` (CTL_SOCK or STATUS_SOCK) of scope `s`'s daemon. The CLI
/// sends one JSON line and reads one back (§15.3).
pub fn dial(s: Scope, name: &str) -> io::Result<Box<dyn LocalStream>> {
    Ok(Box::new(local(&s.paths().1.join(name), false)?))
}

/// An AF_UNIX socket at `path`, listening or connected; Winsock has them from Windows 10 1803. Who
/// may connect is the socket file's to say: its mode, or on Windows its DACL (write access).
fn local(path: &Path, listen: bool) -> io::Result<Socket> {
    let (s, at) = (Socket::new(Domain::UNIX, Type::STREAM, None)?, SockAddr::unix(path)?);
    let r = if listen { s.bind(&at).and_then(|()| s.listen(128)) } else { s.connect(&at) };
    r.map(|()| s)
}

/// Waits until `ln` has a connection to accept or `t` has passed, as `Poll::wait` does: whether one
/// waits. node's accept loops wait on their nonblocking listeners with it, so that they also see
/// stop.
pub fn wait_readable(ln: &TcpListener, t: Duration) -> io::Result<bool> {
    let mut p = [os::poll_fd(ln)];
    os::poll(&mut p, t).map(|()| p[0].revents != 0)
}
