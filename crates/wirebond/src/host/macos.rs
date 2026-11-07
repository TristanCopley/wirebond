//! macOS: the Wi-Fi test, socket options, the scopes' layout, launchd and the application
//! firewall (§6.2, §15.3, §15.6).

use super::Scope;
pub use super::unix::*;
use libc::{IPPROTO_IP, IPPROTO_IPV6};
use socket2::Socket;
use std::ffi::{CStr, OsStr};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::{env, io, ptr};
use wirebond_core::SOCK_BUF;

pub(super) const BOOT_CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
/// The application firewall's tool, which is not on PATH: uninstall's note names it in full, as
/// node's tailscale_only fix does (R1-05).
const FW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
/// The command people type: a link to the system executable, which sits off PATH in the state
/// directory (layout). /usr/local/bin is on macOS's default PATH, and the link is only a name, so
/// it may sit where the user can write (§22.2).
const LINK: &str = "/usr/local/bin/wirebond";

/// Whether interface `name` is Wi-Fi: its functional type is IFRTYPE_FUNCTIONAL_WIFI_INFRA (3); its
/// media type would also say so, but macOS calls both Wi-Fi and Ethernet IFT_ETHER.
pub(super) fn wifi(name: &str) -> bool {
    ifreq(name, libc::SIOCGIFFUNCTIONALTYPE) == Some(3)
}

/// Pins with IP_BOUND_IF or IPV6_BOUND_IF, which need no privilege, and asks for buffers. macOS
/// sends UDP without DF unless asked to.
pub(super) fn sockopts(s: &Socket, v6: bool, index: u32, _: &str) -> io::Result<(usize, usize)> {
    match (index, v6) {
        (0, _) => {}
        (_, true) => setopt(s, IPPROTO_IPV6, libc::IPV6_BOUND_IF, &index.to_ne_bytes())?,
        (_, false) => setopt(s, IPPROTO_IP, libc::IP_BOUND_IF, &index.to_ne_bytes())?,
    }
    let _ = s.set_recv_buffer_size(SOCK_BUF);
    let _ = s.set_send_buffer_size(SOCK_BUF);
    Ok((s.recv_buffer_size()?, s.send_buffer_size()?))
}

pub(super) fn paths(s: Scope) -> (PathBuf, PathBuf) {
    layout(s).0
}

/// Where scope `s` lives: its executable and state directory, its launchd plist and its launchd
/// domain. The system executable, which launchd runs as root, sits in the state directory, which
/// mk_state keeps root's alone; /usr/local/bin may belong to the user (Homebrew on Intel).
fn layout(s: Scope) -> ((PathBuf, PathBuf), PathBuf, String) {
    if s == Scope::System {
        let dir = PathBuf::from("/Library/Application Support/wirebond");
        let job = "/Library/LaunchDaemons/com.wirebond.plist".into();
        return ((dir.join("wirebond"), dir), job, "system".into());
    }
    let (home, uid) = user();
    let exe = home.join(".local/bin/wirebond");
    let dir = home.join("Library/Application Support/wirebond");
    let job = home.join("Library/LaunchAgents/com.wirebond.plist");
    ((exe, dir), job, format!("gui/{uid}"))
}

/// The home directory and uid of whoever ran wirebond, through sudo or not.
fn user() -> (PathBuf, libc::uid_t) {
    let sudo = env::var("SUDO_UID").ok().filter(|_| admin()).and_then(|u| u.parse().ok());
    // SAFETY: getuid has no preconditions.
    let me = || (env::var_os("HOME").unwrap_or_default().into(), unsafe { libc::getuid() });
    sudo.and_then(home).unwrap_or_else(me)
}

/// The home directory of user `uid`, and `uid`.
fn home(uid: libc::uid_t) -> Option<(PathBuf, libc::uid_t)> {
    let (mut pw, mut p) = (MaybeUninit::<libc::passwd>::zeroed(), ptr::null_mut());
    let mut buf = [0 as libc::c_char; 4096];
    // SAFETY: getpwuid_r fills pw, its strings in buf, within the sizes given, and points p at pw
    // on success; pw_dir is then a C string in buf.
    unsafe {
        libc::getpwuid_r(uid, pw.as_mut_ptr(), buf.as_mut_ptr(), buf.len(), &mut p);
        let dir = CStr::from_ptr(p.as_ref()?.pw_dir);
        Some((OsStr::from_bytes(dir.to_bytes()).into(), uid))
    }
}

/// Writes the plist and bootstraps the job, which RunAtLoad starts. A job that is loaded already
/// is kickstarted instead: started if it is not running, restarted on a new executable. The
/// system executable also gets its name on PATH, LINK; without it, which only costs the full
/// path, the CLI says how to run it.
pub(super) fn register(s: Scope, exe: &Path, restart: bool) -> io::Result<bool> {
    let (_, job, dom) = layout(s);
    let linked = s == Scope::System && link(Path::new(LINK), exe).unwrap_or(false);
    let changed = write(&job, &plist(exe))?;
    let boot = [OsStr::new("bootstrap"), dom.as_ref(), job.as_ref()];
    if run("launchctl", &boot).is_ok() {
        return Ok(true); // it was not loaded and now runs
    }
    // -k restarts a running job on the new executable; -p only prints its pid.
    let flag = if changed || restart { "-k" } else { "-p" };
    run("launchctl", &["kickstart", flag, &format!("{dom}/com.wirebond")]).map(|_| changed || linked)
}

/// Boots the job out and removes its plist, LINK, and the application firewall's entry for `exe`.
/// That needs root; while the firewall still lists `exe`, the note is the command for the CLI to
/// print (§15.6).
pub(super) fn unregister(s: Scope, exe: &Path) -> io::Result<(bool, String)> {
    let (_, job, dom) = layout(s);
    let _ = run("launchctl", &["bootout", &format!("{dom}/com.wirebond")]);
    let unlinked = s == Scope::System && unlink(Path::new(LINK), exe)?;
    let changed = remove([&job])? || unlinked;
    let _ = run(FW, &[OsStr::new("--remove"), exe.as_ref()]); // fails without root, or when not listed
    let path = exe.as_os_str().as_bytes();
    let listed = run(FW, &["--listapps"]).is_ok_and(|out| out.windows(path.len()).any(|w| w == path));
    let note = listed.then(|| format!("sudo {FW} --remove {exe:?}"));
    Ok((changed, note.unwrap_or_default()))
}

// ---------------------------------------------------------------------------------------------
// The 2.0 platform API (SPEC 2.0 §5–§8; host/net.rs has the types and the contract). Owner: host
// macos and windows.

pub use v2::*;

#[allow(dead_code)] // 2.0 wave 2: the daemon's pumps and bond logic call these
mod v2 {
    use std::io::{self, Write};
    use std::mem;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::{fs, path::PathBuf};

    use super::super::Scope;
    use super::super::net::{Capture, Egress, EgressOn, Routes, Share, TunConfig};

    /// The dynamic-store key of the client's DNS service (§7).
    const DNS_KEY: &str = "State:/Network/Service/com.wirebond/DNS";
    /// The pf anchor the default /etc/pf.conf includes through `nat-anchor "com.apple/*"` (§6.2).
    const ANCHOR: &str = "com.apple/wirebond";
    /// Internet Sharing's preferences and launchd job (§8).
    const NAT_PLIST: &str = "/Library/Preferences/SystemConfiguration/com.apple.nat";
    const SHARING_JOB: &str = "system/com.apple.NetworkSharing";

    fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
        if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
    }

    /// A utun interface from a PF_SYSTEM / SYSPROTO_CONTROL socket connected to
    /// com.apple.net.utun_control (root); each packet carries a 4-byte family header (§5.3). Closing
    /// it destroys the interface, and with it its addresses and routes.
    pub struct Tun {
        fd: OwnedFd,
        /// The utunN it got.
        name: String,
    }

    impl Tun {
        /// Takes the next free utunN, nonblocking, and gives it the MTU, the addresses and the
        /// bond's subnets.
        pub fn open(c: &TunConfig) -> io::Result<Tun> {
            // SAFETY: socket takes no pointers.
            let fd = cvt(unsafe { libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL) })?;
            // SAFETY: fd is a new descriptor that only this owns.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: ctl_info is plain data.
            let mut info: libc::ctl_info = unsafe { mem::zeroed() };
            for (d, s) in info.ctl_name.iter_mut().zip(b"com.apple.net.utun_control") {
                *d = *s as libc::c_char;
            }
            // SAFETY: CTLIOCGINFO reads the name and writes the id in info.
            cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::CTLIOCGINFO, &mut info) })?;
            let sc = libc::sockaddr_ctl {
                sc_len: mem::size_of::<libc::sockaddr_ctl>() as u8,
                sc_family: libc::AF_SYSTEM as u8,
                ss_sysaddr: libc::AF_SYS_CONTROL as u16,
                sc_id: info.ctl_id,
                sc_unit: 0, // the next free utunN
                sc_reserved: [0; 5],
            };
            let len = mem::size_of_val(&sc) as libc::socklen_t;
            // SAFETY: sc is a sockaddr_ctl of len bytes.
            cvt(unsafe { libc::connect(fd.as_raw_fd(), (&raw const sc).cast(), len) })?;
            let (mut name, mut n) = ([0u8; 32], 32 as libc::socklen_t);
            let p = name.as_mut_ptr().cast();
            // SAFETY: the option writes at most n bytes into name and sets n.
            cvt(unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SYSPROTO_CONTROL, libc::UTUN_OPT_IFNAME, p, &mut n) })?;
            let end = name.iter().position(|&b| b == 0).unwrap_or(n as usize);
            let name = String::from_utf8_lossy(&name[..end]).into_owned();
            // SAFETY: fcntl on an open descriptor.
            unsafe {
                cvt(libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK))?;
                cvt(libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC))?;
            }
            let tun = Tun { fd, name };
            let t = tun.name.as_str();
            super::run("ifconfig", &[t, "mtu", &c.mtu.to_string(), "up"])?;
            if let Some((ip, bits)) = c.addr4 {
                let (a, mask) = (ip.to_string(), Ipv4Addr::from(mask4(bits)).to_string());
                super::run("ifconfig", &[t, "inet", &a, &a, "netmask", &mask, "alias"])?;
                route(true, net(IpAddr::V4(ip), bits), t)?;
            }
            if let Some((a, bits)) = c.addr6 {
                super::run("ifconfig", &[t, "inet6", &format!("{a}/{bits}"), "alias"])?;
                route(true, net(IpAddr::V6(a), bits), t)?;
            }
            Ok(tun)
        }
        pub fn name(&self) -> &str {
            &self.name
        }
        /// One packet into `buf`, without the family header; WouldBlock when none waits.
        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut head = [0u8; 4];
            let iov = [
                libc::iovec { iov_base: head.as_mut_ptr().cast(), iov_len: 4 },
                libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() },
            ];
            // SAFETY: both buffers outlive the call and readv writes within them.
            let n = unsafe { libc::readv(self.fd.as_raw_fd(), iov.as_ptr(), 2) };
            if n < 0 {
                return Err(io::Error::last_os_error()); // EAGAIN is WouldBlock
            }
            Ok((n as usize).saturating_sub(4))
        }
        /// One packet, behind the family header its version nibble names.
        pub fn write(&self, packet: &[u8]) -> io::Result<()> {
            let family = match packet.first().map(|b| b >> 4) {
                Some(4) => libc::AF_INET,
                Some(6) => libc::AF_INET6,
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not an IP packet")),
            };
            let head = (family as u32).to_be_bytes();
            let iov = [
                libc::iovec { iov_base: head.as_ptr().cast_mut().cast(), iov_len: 4 },
                libc::iovec { iov_base: packet.as_ptr().cast_mut().cast(), iov_len: packet.len() },
            ];
            // SAFETY: both buffers outlive the call; writev only reads them.
            cvt(unsafe { libc::writev(self.fd.as_raw_fd(), iov.as_ptr(), 2) } as libc::c_int).map(|_| ())
        }
    }

    impl AsRawFd for Tun {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    /// An IPv4 netmask of `bits` ones.
    fn mask4(bits: u8) -> u32 {
        u32::MAX.checked_shl(32 - u32::from(bits.min(32))).unwrap_or(0)
    }

    /// The network `addr`/`bits` is in.
    fn net(addr: IpAddr, bits: u8) -> (IpAddr, u8) {
        match addr {
            IpAddr::V4(a) => (IpAddr::V4(Ipv4Addr::from(a.to_bits() & mask4(bits))), bits),
            IpAddr::V6(a) => {
                let m = u128::MAX.checked_shl(128 - u32::from(bits.min(128))).unwrap_or(0);
                (IpAddr::V6(Ipv6Addr::from(a.to_bits() & m)), bits)
            }
        }
    }

    /// Adds or deletes `net` through interface `tun` with the route tool. A route that exists, or is
    /// gone, is fine.
    fn route(add: bool, (net, bits): (IpAddr, u8), tun: &str) -> io::Result<()> {
        let family = if net.is_ipv4() { "-inet" } else { "-inet6" };
        let op = if add { "add" } else { "delete" };
        // Not -q, which would hide the error that tells an existing or missing route apart.
        match super::run("route", &["-n", op, family, &format!("{net}/{bits}"), "-interface", tun]) {
            Err(e) if e.to_string().contains(if add { "File exists" } else { "not in table" }) => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// The four /1 routes (§5.3): they cover the default without replacing it.
    fn halves() -> [(IpAddr, u8); 4] {
        let (v4, v6) =
            (|b| IpAddr::V4(Ipv4Addr::new(b, 0, 0, 0)), |s| IpAddr::V6(Ipv6Addr::new(s, 0, 0, 0, 0, 0, 0, 0)));
        [(v4(0), 1), (v4(128), 1), (v6(0), 1), (v6(0x8000), 1)]
    }

    /// Runs scutil with `commands` on its standard input.
    fn scutil(commands: &str) -> io::Result<()> {
        let mut c = Command::new("scutil");
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
        child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?.write_all(commands.as_bytes())?;
        let out = child.wait_with_output()?;
        let err = String::from_utf8_lossy(&out.stderr);
        if out.status.success() && err.trim().is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(format!("scutil: {}", err.trim())))
        }
    }

    /// 0.0.0.0/1, 128.0.0.0/1, ::/1 and 8000::/1 to the utun, and a dynamic-store DNS service
    /// through scutil (§5.3, §7). The bond's subnets are on the utun already; the mesh's more
    /// specific routes (100.64.0.0/10 and fd7a:115c:a1e0::/48 on Tailscale's utun) keep winning.
    pub fn capture_install(c: &Capture) -> io::Result<()> {
        if c.routes == Routes::All {
            for half in halves() {
                route(true, half, &c.tun)?;
            }
        }
        if let Some(dns) = c.dns {
            scutil(&format!(
                "d.init\nd.add ServerAddresses * {dns}\nd.add SupplementalMatchDomains * \"\"\n\
                 d.add InterfaceName {}\nset {DNS_KEY}\nquit\n",
                c.tun
            ))?;
        }
        Ok(())
    }

    /// Deletes the four routes and the DNS service: only what capture_install made.
    pub fn capture_remove(c: &Capture) -> io::Result<()> {
        let mut r = Ok(());
        for half in halves() {
            r = r.and(route(false, half, &c.tun).or_else(|e| if gone(&c.tun) { Ok(()) } else { Err(e) }));
        }
        r.and(scutil(&format!("remove {DNS_KEY}\nquit\n")))
    }

    /// Whether interface `name` no longer exists, which takes its routes with it.
    fn gone(name: &str) -> bool {
        super::ifreq(name, libc::SIOCGIFMTU).is_none()
    }

    /// What egress_enable changed, kept beside config.json so that a crash's leftovers can be undone
    /// (§5.4): the forwarding sysctls it found off and pf's enable token.
    fn egress_file() -> PathBuf {
        Scope::System.paths().1.join("egress.state")
    }

    static EGRESS: Mutex<()> = Mutex::new(());

    /// net.inet.ip.forwarding and net.inet6.ip6.forwarding, pf NAT rules in the anchor
    /// com.apple/wirebond, `pfctl -E` and its reference released on disable (§6.2). Called again,
    /// it follows the uplink and enables pf once.
    pub fn egress_enable(e: &Egress) -> io::Result<EgressOn> {
        let _one = EGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let uplink = default_interface(false)?;
        // IPv6 leaves through the interface of the IPv6 default route, which may be another one.
        let uplink6 = e.net6.and_then(|_| default_interface(true).ok());
        let up6 = uplink6.is_some();
        let mut state = fs::read_to_string(egress_file()).unwrap_or_default();
        for (key, on) in [("net.inet.ip.forwarding", true), ("net.inet6.ip6.forwarding", up6)] {
            let was = super::run("sysctl", &["-n", key]).map(|o| String::from_utf8_lossy(&o).trim() == "1")?;
            if on && !was {
                super::run("sysctl", &["-w", &format!("{key}=1")])?;
                state.push_str(&format!("sysctl {key}\n"));
            }
        }
        let (n4, b4) = net(IpAddr::V4(e.net4.0), e.net4.1);
        let mut rules = format!("nat on {uplink} inet from {n4}/{b4} to any -> ({uplink})\n");
        if let (Some(up), Some((a, bits))) = (&uplink6, e.net6) {
            let (n6, b6) = net(IpAddr::V6(a), bits);
            rules.push_str(&format!("nat on {up} inet6 from {n6}/{b6} to any -> ({up})\n"));
        }
        pfctl(&["-a", ANCHOR, "-f", "-"], Some(&rules))?;
        if !state.contains("token ") {
            let out = pfctl(&["-E"], None)?;
            let token = token(&out).ok_or_else(|| io::Error::other(format!("pfctl -E gave no token: {out}")))?;
            state.push_str(&format!("token {token}\n"));
        }
        fs::write(egress_file(), &state)?;
        let first = Ipv4Addr::from((n4_bits(n4)) + 1);
        Ok(EgressOn { v4: true, v6: up6, addr4: (first, e.net4.1), uplink: Some(uplink) })
    }

    fn n4_bits(a: IpAddr) -> u32 {
        match a {
            IpAddr::V4(a) => a.to_bits(),
            IpAddr::V6(_) => 0,
        }
    }

    /// Flushes the anchor, releases pf's reference and turns forwarding back off where egress_enable
    /// turned it on.
    pub fn egress_disable(_e: &Egress) -> io::Result<()> {
        let _one = EGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        undo_egress()
    }

    fn undo_egress() -> io::Result<()> {
        let Ok(state) = fs::read_to_string(egress_file()) else { return Ok(()) };
        let mut r = pfctl(&["-a", ANCHOR, "-F", "all"], None).map(|_| ());
        for line in state.lines() {
            match line.split_once(' ') {
                Some(("token", t)) => r = r.and(pfctl(&["-X", t], None).map(|_| ())),
                Some(("sysctl", key)) => r = r.and(super::run("sysctl", &["-w", &format!("{key}=0")]).map(|_| ())),
                _ => {}
            }
        }
        r.and_then(|()| fs::remove_file(egress_file()))
    }

    /// Runs pfctl with `input` on its standard input; returns its standard output and error together,
    /// where it prints the token.
    fn pfctl(args: &[&str], input: Option<&str>) -> io::Result<String> {
        let mut c = Command::new("pfctl");
        let mut child = c.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?.write_all(input.unwrap_or_default().as_bytes())?;
        let out = child.wait_with_output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            Ok(text)
        } else {
            Err(io::Error::other(format!("pfctl {}: {}", args.join(" "), text.trim())))
        }
    }

    /// The reference `pfctl -E` prints: "Token : 123".
    pub(super) fn token(out: &str) -> Option<&str> {
        out.lines().find_map(|l| l.trim().strip_prefix("Token : ")).map(str::trim).filter(|t| !t.is_empty())
    }

    /// The interface of the default route (`route -n get default`).
    fn default_interface(v6: bool) -> io::Result<String> {
        let args: &[&str] = if v6 { &["-n", "get", "-inet6", "default"] } else { &["-n", "get", "default"] };
        let out = super::run("route", args)?;
        interface(&String::from_utf8_lossy(&out))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no default route"))
    }

    /// The `interface:` line of `route -n get`.
    pub(super) fn interface(out: &str) -> Option<String> {
        out.lines().find_map(|l| l.trim().strip_prefix("interface:")).map(|s| s.trim().to_owned())
    }

    /// Resolver #1 from `scutil --dns`, port 53 (§7).
    pub fn upstream_resolver() -> io::Result<SocketAddr> {
        let out = super::run("scutil", &["--dns"])?;
        let ip = first_resolver(&String::from_utf8_lossy(&out));
        ip.map(|ip| SocketAddr::new(ip, 53))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "resolver #1 has no nameserver"))
    }

    /// The first usable `nameserver[i]` of `resolver #1` in `scutil --dns` output; one with a scope
    /// (fe80::1%en0) is skipped.
    pub(super) fn first_resolver(out: &str) -> Option<IpAddr> {
        let mut lines = out.lines().skip_while(|l| l.trim() != "resolver #1").skip(1);
        lines
            .by_ref()
            .take_while(|l| !l.trim_start().starts_with("resolver #"))
            .filter_map(|l| l.trim().strip_prefix("nameserver[")?.split_once(':').map(|(_, v)| v.trim().to_owned()))
            .find_map(|v| v.parse().ok())
    }

    /// Internet Sharing through com.apple.nat and its launchd job (§8), best effort: a refusal
    /// carries the steps to do it by hand.
    pub fn support_share(s: &Share) -> io::Result<()> {
        let nat = format!(
            "{{ Enabled = 1; PrimaryInterface = {{ Device = \"{}\"; Enabled = 1; }}; SharingDevices = ( \"{}\" ); }}",
            s.uplink, s.cable
        );
        let r = super::run("defaults", &["write", NAT_PLIST, "NAT", &nat]).and_then(|_| {
            let _ = super::run("launchctl", &["enable", SHARING_JOB]);
            super::run("launchctl", &["kickstart", "-k", SHARING_JOB])
        });
        r.map(|_| ()).map_err(|e| {
            io::Error::other(format!(
                "{e}. By hand: System Settings > General > Sharing > Internet Sharing: share your \
                 connection from {} to computers using {}, then turn Internet Sharing on",
                s.uplink, s.cable
            ))
        })
    }

    /// Turns Internet Sharing off if it shares `uplink` to `cable`, as support_share left it.
    pub fn support_unshare(s: &Share) -> io::Result<()> {
        let Ok(cur) = super::run("defaults", &["read", NAT_PLIST, "NAT"]) else { return Ok(()) };
        let cur = String::from_utf8_lossy(&cur);
        if !(cur.contains(&s.uplink) && cur.contains(&s.cable)) {
            return Ok(());
        }
        super::run("defaults", &["write", NAT_PLIST, "NAT", "-dict-add", "Enabled", "-int", "0"])?;
        super::run("launchctl", &["kickstart", "-k", SHARING_JOB]).map(|_| ())
    }

    /// Deletes the DNS service and undoes the pf anchor, pf's reference and the forwarding a crash
    /// left behind (§5.4). The utun, its addresses and its routes went with the crashed process.
    pub fn cleanup_stale() -> io::Result<()> {
        let _one = EGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let r = undo_egress();
        r.and(
            scutil(&format!("remove {DNS_KEY}\nquit\n"))
                .or_else(|e| if e.to_string().contains("No such key") { Ok(()) } else { Err(e) }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::net::TunConfig;
    use super::v2::{first_resolver, interface, token};
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn reads_resolver_one_route_and_token() {
        let dns = "DNS configuration\n\nresolver #1\n  search domain[0] : lan\n  nameserver[0] : fe80::1%en0\n  \
                   nameserver[1] : 192.168.1.1\n  if_index : 6 (en0)\n\nresolver #2\n  nameserver[0] : 10.0.0.1\n";
        assert_eq!(first_resolver(dns), Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert_eq!(
            first_resolver("resolver #1\n  flags : Request A records\nresolver #2\n  nameserver[0] : 1.1.1.1\n"),
            None
        );
        let route = "   route to: default\ndestination: default\n    gateway: 192.168.1.1\n  interface: en0\n      flags: <UP>\n";
        assert_eq!(interface(route).as_deref(), Some("en0"));
        assert_eq!(
            token("No ALTQ support in kernel\npf enabled\nToken : 13286495372398960295\n"),
            Some("13286495372398960295")
        );
        assert_eq!(token("pf enabled\n"), None);
    }

    #[test]
    fn opens_a_utun_with_its_address_as_root() {
        if !admin() {
            return; // a utun needs root: CI's runner is not
        }
        let c =
            TunConfig { name: String::new(), mtu: 1400, addr4: Some((Ipv4Addr::new(10, 77, 0, 2), 24)), addr6: None };
        let t = Tun::open(&c).unwrap();
        assert!(t.name().starts_with("utun"), "{}", t.name());
        let mut b = [0u8; 2048];
        // Nothing routed to it yet besides the bond's subnet: a read finds nothing or a stray packet.
        match t.read(&mut b) {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
            Ok(n) => assert!(n > 0),
        }
        assert!(t.write(&[0u8; 20]).is_err()); // not an IP packet
    }
}
