//! The daemon's view of the platform (SPEC 2.0 §5–§7): the host API of host/net.rs behind one
//! trait, so the loop runs against a fake host in tests (no root, fake TUN devices) and against the
//! real one in `wirebond run`. The per-OS bodies are the host owners'; this only calls them.
//! Owner: node daemon.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};

use crate::host::{self, Capture, Egress, EgressOn, TunConfig};

/// One TUN device: one whole IP packet per read and per write; a read never blocks
/// (`WouldBlock` when none waits).
pub trait TunDev {
    fn name(&self) -> &str;
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn write(&self, packet: &[u8]) -> io::Result<()>;
    /// Adds the device to the loop's poll set, and says whether it did: the loop then reads the
    /// device only when the poll found it readable, and otherwise on every turn. Windows waits on
    /// its read event instead (§4.3).
    fn poll_add(&self, _poll: &mut host::Poll) -> bool {
        false
    }
    /// Windows: Wintun's read-wait event, which the loop's wait includes (§4.3); None for a device
    /// without one, which the loop then reads on its next turn.
    #[cfg(windows)]
    fn read_event(&self) -> Option<windows_sys::Win32::Foundation::HANDLE> {
        None
    }
}

/// The host functions the daemon calls (host/net.rs, host/mod.rs).
pub trait Platform {
    fn open_tun(&mut self, c: &TunConfig) -> io::Result<Box<dyn TunDev>>;
    fn capture_install(&mut self, c: &Capture) -> io::Result<()>;
    fn capture_remove(&mut self, c: &Capture) -> io::Result<()>;
    fn egress_enable(&mut self, e: &Egress) -> io::Result<EgressOn>;
    fn egress_disable(&mut self, e: &Egress) -> io::Result<()>;
    fn upstream_resolver(&mut self) -> io::Result<SocketAddr>;
    /// This device's usable links and its mesh addresses.
    fn links(&mut self, mesh: &[(IpAddr, u8)]) -> io::Result<(Vec<host::Link>, Vec<host::Link>)>;
    /// A lane socket: a link's, pinned to it, or the mesh lane's on a mesh address (`mesh`).
    fn bind(&mut self, link: &host::Link, mesh: bool) -> io::Result<UdpSocket>;
    fn hostname(&self) -> String;
    /// Each interface's default gateway, by interface name (Windows: by the interface's address):
    /// links behind the same gateway share a bottleneck group (§8). Empty where the OS does not say.
    fn gateways(&mut self) -> Vec<(String, IpAddr)> {
        Vec::new()
    }
    /// The default route's local address (`host::default_addr`): the link carrying the mesh's
    /// underlay (§8), and on an anchor its uplink (§6.2). None without a default route.
    fn default_addr(&mut self) -> Option<IpAddr> {
        None
    }
    /// The sleep-inclusive clock in ns (`host::boot`), for wake detection (§11.4); None turns it
    /// off.
    fn boot(&self) -> Option<i64> {
        None
    }
    /// Whether the last `capture_install` left DNS alone (§7: `dns_local`).
    fn dns_local(&self) -> bool {
        false
    }
    /// A support's sharing (§8): `uplink` to `cable` with the OS's own sharing.
    fn support_share(&mut self, _s: &host::Share) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, host::NOT_YET))
    }
    fn support_unshare(&mut self, _s: &host::Share) -> io::Result<()> {
        Ok(())
    }
    /// The bytes the OS counted on `link`'s interface: received, sent (§20.3).
    fn if_bytes(&mut self, link: &host::Link) -> io::Result<(u64, u64)> {
        super::support::os_bytes(link)
    }
}

/// Datagrams one [`recv_batch`] reads at most.
pub const BATCH: usize = if cfg!(target_os = "linux") { 32 } else { 1 };
/// Room for one lane datagram (MAX_PACKET and more).
pub const DGRAM: usize = 2048;

/// The datagrams waiting on lane socket `u`, at most `bufs.len()` (BATCH): datagram i's bytes in
/// `bufs[i]`, its length and sender in `got[i]`; WouldBlock when none waits. Fewer than BATCH
/// means none waited after the last. Linux reads them in one recvmmsg, elsewhere one at a time.
#[cfg(target_os = "linux")]
pub fn recv_batch(u: &UdpSocket, bufs: &mut [[u8; DGRAM]], got: &mut [(usize, SocketAddr)]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    let n = bufs.len().min(got.len()).min(BATCH);
    // SAFETY: zeroed iovecs, sockaddr_storages and mmsghdrs are valid values of those C structs.
    let mut iov: [libc::iovec; BATCH] = unsafe { std::mem::zeroed() };
    let mut names: [libc::sockaddr_storage; BATCH] = unsafe { std::mem::zeroed() };
    let mut hdrs: [libc::mmsghdr; BATCH] = unsafe { std::mem::zeroed() };
    for i in 0..n {
        iov[i] = libc::iovec { iov_base: bufs[i].as_mut_ptr().cast(), iov_len: DGRAM };
        hdrs[i].msg_hdr.msg_name = (&raw mut names[i]).cast();
        hdrs[i].msg_hdr.msg_namelen = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        hdrs[i].msg_hdr.msg_iov = &raw mut iov[i];
        hdrs[i].msg_hdr.msg_iovlen = 1;
    }
    // SAFETY: hdrs' first n entries point at n live buffers of DGRAM bytes and n sockaddr_storages,
    // which outlive the call; MSG_DONTWAIT never blocks.
    let r = unsafe {
        libc::recvmmsg(
            u.as_raw_fd(),
            hdrs.as_mut_ptr(),
            n as libc::c_uint,
            libc::MSG_DONTWAIT as _,
            std::ptr::null_mut(),
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    let r = r as usize;
    for i in 0..r {
        // A lane socket's senders are IPv4 or IPv6; anything else reads as from nowhere.
        let from = sockaddr(&names[i]).unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0)));
        got[i] = ((hdrs[i].msg_len as usize).min(DGRAM), from);
    }
    Ok(r)
}

/// One datagram at a time where recvmmsg is not.
#[cfg(not(target_os = "linux"))]
pub fn recv_batch(u: &UdpSocket, bufs: &mut [[u8; DGRAM]], got: &mut [(usize, SocketAddr)]) -> io::Result<usize> {
    let (Some(b), Some(g)) = (bufs.first_mut(), got.first_mut()) else { return Ok(0) };
    *g = u.recv_from(b)?;
    Ok(1)
}

/// Datagrams one [`send_batch`] sends at most. A turn rarely queues more (a frame's burst on each
/// lane): the batch's headers are zeroed on every call.
pub const TX_BATCH: usize = 16;

/// Sends `msgs` (destination, datagram) on lane socket `u` in order, TX_BATCH at most: Linux in
/// sendmmsg calls, elsewhere one send_to each. A datagram the OS refuses is dropped, as a failed
/// send_to drops it, and the rest still go. Returns the bytes sent.
#[cfg(target_os = "linux")]
pub fn send_batch(u: &UdpSocket, msgs: &[(SocketAddr, &[u8])]) -> u64 {
    use std::os::fd::AsRawFd;
    let n = msgs.len().min(TX_BATCH);
    // SAFETY: zeroed iovecs, sockaddr_storages and mmsghdrs are valid values of those C structs.
    let mut iov: [libc::iovec; TX_BATCH] = unsafe { std::mem::zeroed() };
    let mut names: [libc::sockaddr_storage; TX_BATCH] = unsafe { std::mem::zeroed() };
    let mut hdrs: [libc::mmsghdr; TX_BATCH] = unsafe { std::mem::zeroed() };
    for (i, &(to, data)) in msgs.iter().take(n).enumerate() {
        // sendmmsg only reads the bytes.
        iov[i] = libc::iovec { iov_base: data.as_ptr().cast_mut().cast(), iov_len: data.len() };
        hdrs[i].msg_hdr.msg_name = (&raw mut names[i]).cast();
        hdrs[i].msg_hdr.msg_namelen = to_sockaddr(to, &mut names[i]);
        hdrs[i].msg_hdr.msg_iov = &raw mut iov[i];
        hdrs[i].msg_hdr.msg_iovlen = 1;
    }
    let (mut at, mut bytes) = (0, 0);
    while at < n {
        // The system call itself: musl's sendmmsg on 64-bit targets is a loop of sendmsg calls,
        // as its msghdr pads msg_iovlen and msg_controllen to the kernel's size_t; zeroed here,
        // the pads make each field the kernel's value.
        // SAFETY: hdrs[at..n] point at live iovecs over the callers' datagrams and at filled
        // sockaddr_storages, all of which outlive the call.
        let r = unsafe {
            libc::syscall(libc::SYS_sendmmsg, u.as_raw_fd(), hdrs[at..].as_mut_ptr(), (n - at) as libc::c_uint, 0)
        };
        if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        // sendmmsg stops at the first datagram it cannot send: that one is dropped.
        let sent = usize::try_from(r).unwrap_or(0);
        bytes += msgs[at..at + sent].iter().map(|(_, d)| d.len() as u64).sum::<u64>();
        at += sent + usize::from(sent < n - at);
    }
    bytes
}

/// One send_to each where sendmmsg is not.
#[cfg(not(target_os = "linux"))]
pub fn send_batch(u: &UdpSocket, msgs: &[(SocketAddr, &[u8])]) -> u64 {
    msgs.iter().filter(|(to, d)| u.send_to(d, to).is_ok()).map(|(_, d)| d.len() as u64).sum()
}

/// Writes `a` into `s` as a sockaddr_in or sockaddr_in6; returns its length.
#[cfg(target_os = "linux")]
fn to_sockaddr(a: SocketAddr, s: &mut libc::sockaddr_storage) -> libc::socklen_t {
    match a {
        SocketAddr::V4(a) => {
            // SAFETY: a sockaddr_storage has room and alignment for a sockaddr_in.
            let v4 = unsafe { &mut *(s as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            v4.sin_family = libc::AF_INET as libc::sa_family_t;
            v4.sin_port = a.port().to_be();
            v4.sin_addr = libc::in_addr { s_addr: u32::from(*a.ip()).to_be() };
            size_of::<libc::sockaddr_in>() as libc::socklen_t
        }
        SocketAddr::V6(a) => {
            // SAFETY: a sockaddr_storage has room and alignment for a sockaddr_in6.
            let v6 = unsafe { &mut *(s as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
            v6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            v6.sin6_port = a.port().to_be();
            v6.sin6_flowinfo = a.flowinfo();
            v6.sin6_addr = libc::in6_addr { s6_addr: a.ip().octets() };
            v6.sin6_scope_id = a.scope_id();
            size_of::<libc::sockaddr_in6>() as libc::socklen_t
        }
    }
}

/// A sockaddr_storage recvmmsg filled: an IPv4 or IPv6 sender.
#[cfg(target_os = "linux")]
fn sockaddr(s: &libc::sockaddr_storage) -> Option<SocketAddr> {
    use std::net::{SocketAddrV4, SocketAddrV6};
    match libc::c_int::from(s.ss_family) {
        libc::AF_INET => {
            // SAFETY: an AF_INET sockaddr_storage holds a sockaddr_in, which is smaller.
            let a = unsafe { &*(s as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            let ip = std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
            Some(SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be(a.sin_port))))
        }
        libc::AF_INET6 => {
            // SAFETY: an AF_INET6 sockaddr_storage holds a sockaddr_in6, which is smaller.
            let a = unsafe { &*(s as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
            let ip = std::net::Ipv6Addr::from(a.sin6_addr.s6_addr);
            Some(SocketAddr::V6(SocketAddrV6::new(ip, u16::from_be(a.sin6_port), a.sin6_flowinfo, a.sin6_scope_id)))
        }
        _ => None,
    }
}

/// The real host.
pub struct Real;

struct RealTun(host::Tun);

impl TunDev for RealTun {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }

    fn write(&self, packet: &[u8]) -> io::Result<()> {
        self.0.write(packet)
    }

    #[cfg(unix)]
    fn poll_add(&self, poll: &mut host::Poll) -> bool {
        poll.add_tun(&self.0);
        true
    }

    #[cfg(windows)]
    fn read_event(&self) -> Option<windows_sys::Win32::Foundation::HANDLE> {
        Some(self.0.read_event())
    }
}

impl Platform for Real {
    fn open_tun(&mut self, c: &TunConfig) -> io::Result<Box<dyn TunDev>> {
        Ok(Box::new(RealTun(host::Tun::open(c)?)))
    }

    fn capture_install(&mut self, c: &Capture) -> io::Result<()> {
        host::capture_install(c)
    }

    fn capture_remove(&mut self, c: &Capture) -> io::Result<()> {
        host::capture_remove(c)
    }

    fn egress_enable(&mut self, e: &Egress) -> io::Result<EgressOn> {
        host::egress_enable(e)
    }

    fn egress_disable(&mut self, e: &Egress) -> io::Result<()> {
        host::egress_disable(e)
    }

    fn upstream_resolver(&mut self) -> io::Result<SocketAddr> {
        host::upstream_resolver()
    }

    fn links(&mut self, mesh: &[(IpAddr, u8)]) -> io::Result<(Vec<host::Link>, Vec<host::Link>)> {
        host::links(mesh)
    }

    fn bind(&mut self, link: &host::Link, mesh: bool) -> io::Result<UdpSocket> {
        let (s, _, _) = if mesh { host::listen_mesh(link.addr)? } else { host::listen(link)? };
        Ok(s)
    }

    fn hostname(&self) -> String {
        host::hostname()
    }

    fn support_share(&mut self, s: &host::Share) -> io::Result<()> {
        host::support_share(s)
    }

    fn support_unshare(&mut self, s: &host::Share) -> io::Result<()> {
        host::support_unshare(s)
    }

    fn gateways(&mut self) -> Vec<(String, IpAddr)> {
        gateways()
    }

    fn default_addr(&mut self) -> Option<IpAddr> {
        host::default_addr().ok()
    }

    fn boot(&self) -> Option<i64> {
        Some(host::boot())
    }

    fn dns_local(&self) -> bool {
        host::dns_local()
    }
}

/// Linux: the IPv4 default routes of /proc/net/route, each interface's gateway.
#[cfg(target_os = "linux")]
fn gateways() -> Vec<(String, IpAddr)> {
    std::fs::read_to_string("/proc/net/route").map_or_else(|_| Vec::new(), |t| proc_route(&t))
}

/// macOS: the IPv4 default routes `netstat -rn -f inet` lists, one per interface.
#[cfg(target_os = "macos")]
fn gateways() -> Vec<(String, IpAddr)> {
    let out = std::process::Command::new("/usr/sbin/netstat").args(["-rn", "-f", "inet"]).output();
    out.map_or_else(|_| Vec::new(), |o| netstat_defaults(&String::from_utf8_lossy(&o.stdout)))
}

/// Windows: the IPv4 default routes `route print -4` lists, each keyed by its interface's address.
#[cfg(windows)]
fn gateways() -> Vec<(String, IpAddr)> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("route").args(["print", "-4"]).creation_flags(CREATE_NO_WINDOW).output();
    out.map_or_else(|_| Vec::new(), |o| route_print_defaults(&String::from_utf8_lossy(&o.stdout)))
}

/// Elsewhere no gateway is known, and links group by STUN alone.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn gateways() -> Vec<(String, IpAddr)> {
    Vec::new()
}

/// `route print -4`'s active default routes (`0.0.0.0 0.0.0.0 <gateway> <interface address>
/// <metric>`), each keyed by its interface's address; an on-link default has no gateway.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn route_print_defaults(t: &str) -> Vec<(String, IpAddr)> {
    t.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() != 5 || f[0] != "0.0.0.0" || f[1] != "0.0.0.0" {
                return None;
            }
            Some((f[3].to_string(), f[2].parse().ok()?))
        })
        .collect()
}

/// /proc/net/route's default routes (`Iface Destination Gateway ...`, addresses in hex in the
/// host's byte order, little-endian wherever wirebond runs), each interface's gateway.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
pub fn proc_route(t: &str) -> Vec<(String, IpAddr)> {
    t.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (dst, gw) = (u32::from_str_radix(f.get(1)?, 16).ok()?, u32::from_str_radix(f.get(2)?, 16).ok()?);
            (dst == 0 && gw != 0).then(|| (f[0].to_string(), IpAddr::from(gw.to_le_bytes())))
        })
        .collect()
}

/// `netstat -rn -f inet`'s default routes (`default <gateway> <flags> <interface>`), each
/// interface's gateway.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
pub fn netstat_defaults(t: &str) -> Vec<(String, IpAddr)> {
    t.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let gw = f.get(1)?.parse::<IpAddr>().ok()?;
            (f.first() == Some(&"default") && f.len() >= 4).then(|| (f[3].to_string(), gw))
        })
        .collect()
}
