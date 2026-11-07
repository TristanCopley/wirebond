//! Linux: the Wi-Fi test, socket options, paths and the systemd unit (§6.2, §15.3). Linux has
//! only the system scope. Then the 2.0 platform API (SPEC 2.0 §5.3, §5.4, §6.2, §7, §8): the TUN
//! device, capture with `ip`, egress with nftables, DNS with resolvectl, sharing with nmcli, and
//! stale-state cleanup. The values it changes are saved under /run/wirebond until put back.

use super::Scope;
pub use super::unix::*;
use libc::{IPPROTO_IP, IPPROTO_IPV6, SO_RCVBUF, SO_RCVBUFFORCE, SO_SNDBUF, SO_SNDBUFFORCE, SOL_SOCKET, c_int};
use socket2::Socket;
use std::path::{Path, PathBuf};
use std::{fs, io};
use wirebond_core::SOCK_BUF;

pub(super) const BOOT_CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;

const UNIT_FILE: &str = "/etc/systemd/system/wirebond.service";
const WANTS: &str = "/etc/systemd/system/multi-user.target.wants"; // where enable links the unit

/// An absolute path, and systemctl: tests move the first under a temporary root and fake the
/// second.
#[cfg(not(test))]
fn rooted(path: &str) -> PathBuf {
    path.into()
}
#[cfg(not(test))]
fn systemctl(args: &[&str]) -> io::Result<()> {
    run("systemctl", args).map(|_| ())
}
#[cfg(test)]
use super::tests::{rooted, systemctl};

/// Whether interface `name` is Wi-Fi.
pub(super) fn wifi(name: &str) -> bool {
    let dir = rooted(&format!("/sys/class/net/{name}"));
    dir.join("wireless").exists() || dir.join("phy80211").exists()
}

/// Pins, never sets DF, and asks for buffers, first with the FORCE options, which pass rmem_max
/// and wmem_max as root; Linux reports twice the size it grants, for its own bookkeeping.
///
/// IPv4 pins only what it sends, with IP_UNICAST_IF, whose route lookup still skips an exit
/// node's routes: with two NICs on one LAN, the router may deliver to the other NIC what is meant
/// for this one's address (ARP flux), which SO_BINDTODEVICE would drop. IPv6 has no ARP flux, and
/// there only SO_BINDTODEVICE (unprivileged since Linux 5.7) keeps sends off an exit node's
/// default route.
pub(super) fn sockopts(s: &Socket, v6: bool, index: u32, name: &str) -> io::Result<(usize, usize)> {
    match (v6, name.is_empty()) {
        (true, false) => setopt(s, SOL_SOCKET, libc::SO_BINDTODEVICE, name.as_bytes())?,
        // The index in network byte order, which the kernel reads with ntohl.
        (false, false) => setopt(s, IPPROTO_IP, libc::IP_UNICAST_IF, &index.to_be_bytes())?,
        _ => {}
    }
    // §5.3: the mark keeps lane packets out of capture (rule 5300). It needs CAP_NET_ADMIN, which the
    // daemon has (§4.1); without it (an unprivileged test) the socket goes unmarked.
    let _ = setopt(s, SOL_SOCKET, libc::SO_MARK, &super::net::LANE_MARK.to_ne_bytes());
    let (level, opt) = if v6 { (IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER) } else { (IPPROTO_IP, libc::IP_MTU_DISCOVER) };
    setopt(s, level, opt, &libc::IP_PMTUDISC_DONT.to_ne_bytes())?; // IPV6_PMTUDISC_DONT is 0 too
    let size = (SOCK_BUF as c_int).to_ne_bytes();
    for (force, opt) in [(SO_RCVBUFFORCE, SO_RCVBUF), (SO_SNDBUFFORCE, SO_SNDBUF)] {
        let _ = setopt(s, SOL_SOCKET, force, &size).or_else(|_| setopt(s, SOL_SOCKET, opt, &size));
    }
    Ok((s.recv_buffer_size()? / 2, s.send_buffer_size()? / 2))
}

pub(super) fn paths(_: Scope) -> (PathBuf, PathBuf) {
    (rooted("/usr/local/bin/wirebond"), rooted("/var/lib/wirebond"))
}

/// Writes the unit and its multi-user.target.wants link, as systemctl enable would, and starts
/// the service, or restarts it on a new executable.
pub(super) fn register(_: Scope, exe: &Path, restart: bool) -> io::Result<bool> {
    let mut changed = write(&rooted(UNIT_FILE), &unit(exe))?;
    let link = rooted(WANTS).join("wirebond.service");
    if fs::symlink_metadata(&link).is_err() {
        fs::create_dir_all(rooted(WANTS))?;
        std::os::unix::fs::symlink(UNIT_FILE, &link)?;
        changed = true;
    }
    let restart = changed || restart;
    if restart {
        let _ = systemctl(&["daemon-reload"]);
    }
    systemctl(&[if restart { "restart" } else { "start" }, "wirebond"])?;
    Ok(changed)
}

pub(super) fn unregister(_: Scope, _: &Path) -> io::Result<(bool, String)> {
    let _ = systemctl(&["stop", "wirebond"]);
    let changed = remove([rooted(WANTS).join("wirebond.service"), rooted(UNIT_FILE)])?;
    if changed {
        let _ = systemctl(&["daemon-reload"]);
    }
    Ok((changed, String::new()))
}

/// The systemd unit (§15.3). No start limit: a server keeps restarting, as on Windows. Type=exec
/// makes systemctl start fail, so install too, when the executable cannot be run (systemd 240 and
/// later; older ones ignore it).
pub(super) fn unit(exe: &Path) -> String {
    format!(
        "[Unit]\nDescription=wirebond\nStartLimitIntervalSec=0\n\n\
         [Service]\nType=exec\nExecStart={} run\nRestart=always\nRestartSec=1\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        exe.display()
    )
}

// ---------------------------------------------------------------------------------------------
// The 2.0 platform API (SPEC 2.0 §5–§8; host/net.rs has the types and the contract). Owner: host
// linux.

pub use v2::*;

#[allow(dead_code)] // 2.0 wave 2: the daemon's pumps and bond logic call these
mod v2 {
    use std::fs;
    use std::io::{self, Write as _};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::super::Scope;
    use super::super::net::{
        Capture, Egress, EgressOn, LANE_MARK, Routes, Share, TABLE, TUN_NAME, TunConfig, missing_tool_error,
    };
    use super::rooted;

    /// The rules' priorities (§5.3): every route more specific than a default wins at 5299; at
    /// 5300 whatever is not a lane packet takes TABLE. Tailscale's own sit at 5210–5270.
    const PRIO_MAIN: u32 = 5299;
    const PRIO_CAPTURE: u32 = 5300;
    /// What stays out of capture although main may have no route for it (§5.2): link-local,
    /// multicast and the limited broadcast. TABLE throws them back to main, and the mesh's ranges.
    const OUT4: [&str; 3] = ["169.254.0.0/16", "224.0.0.0/4", "255.255.255.255/32"];
    const OUT6: [&str; 2] = ["fe80::/10", "ff00::/8"];
    /// The values capture and egress changed, kept until they are put back: in /run, which a
    /// reboot clears as it resets the values themselves (§5.4, §6.2).
    const SAVED_CAPTURE: &str = "/run/wirebond/capture";
    const SAVED_EGRESS: &str = "/run/wirebond/egress";
    /// The nftables table egress loads (§6.2).
    const NFT_TABLE: &str = "wirebond";
    /// Where a tool may be when the service's PATH lacks it.
    const TOOL_DIRS: [&str; 4] = ["/usr/sbin", "/sbin", "/usr/bin", "/bin"];

    /// Whether the last capture_install left DNS alone (§7: `dns_local`).
    static DNS_LOCAL: AtomicBool = AtomicBool::new(false);

    /// The TUN device: /dev/net/tun with IFF_TUN | IFF_NO_PI, nonblocking, MTU and addresses set
    /// with `ip` (§5.3). Closing it deletes the device, its addresses and the routes through it.
    pub struct Tun {
        fd: OwnedFd,
        name: String,
    }

    impl Tun {
        pub fn open(c: &TunConfig) -> io::Result<Tun> {
            let bad = c.name.is_empty() || c.name.len() >= libc::IFNAMSIZ || !iface_name(&c.name);
            if bad {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad interface name {:?}", c.name)));
            }
            // SAFETY: the path is a C string; open returns a new descriptor or -1.
            let fd = unsafe { libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd is a new descriptor that nothing else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut ifr = [0u8; 40]; // struct ifreq: the name (IFNAMSIZ), then the flags, a short
            ifr[..c.name.len()].copy_from_slice(c.name.as_bytes());
            ifr[16..18].copy_from_slice(&((libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short).to_ne_bytes());
            // SAFETY: TUNSETIFF reads and writes one struct ifreq, which ifr's 40 bytes hold. The
            // request's type is c_ulong with glibc, c_int with musl: `as _`.
            if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETIFF as _, ifr.as_mut_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let n = ifr[..16].iter().position(|&b| b == 0).unwrap_or(16);
            let tun = Tun { fd, name: String::from_utf8_lossy(&ifr[..n]).into_owned() };
            let dev = tun.name.as_str();
            ip(&format!("link set dev {dev} mtu {}", c.mtu))?;
            if let Some((a, p)) = c.addr4 {
                ip(&format!("-4 addr replace {a}/{p} dev {dev}"))?;
            }
            if let Some((a, p)) = c.addr6.filter(|_| has_v6()) {
                ip(&format!("-6 addr replace {a}/{p} dev {dev} nodad"))?;
            }
            ip(&format!("link set dev {dev} up"))?;
            Ok(tun)
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        /// One packet into `buf`; WouldBlock when none waits.
        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            // SAFETY: read writes at most buf.len() bytes into buf.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
        }

        pub fn write(&self, packet: &[u8]) -> io::Result<()> {
            // SAFETY: write reads at most packet.len() bytes from packet.
            let n = unsafe { libc::write(self.fd.as_raw_fd(), packet.as_ptr().cast(), packet.len()) };
            match n {
                n if n < 0 => Err(io::Error::last_os_error()),
                n if n as usize == packet.len() => Ok(()),
                _ => Err(io::Error::new(io::ErrorKind::WriteZero, "short write to the TUN device")),
            }
        }
    }

    impl AsRawFd for Tun {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    /// Table 7849 `default dev wb0` (IPv4 and IPv6; only the bond's subnets for
    /// [`Routes::BondOnly`]) with throw routes for what stays out, rules 5299 (`lookup main
    /// suppress_prefixlength 0`) and 5300 (`not fwmark 0x7849 lookup 7849`), rp_filter 2 on the
    /// lane links, and resolvectl DNS on wb0 (§5.3, §7); `missing_tool ip` without iproute2.
    /// Without systemd-resolved DNS is left alone and [`dns_local`] says so.
    pub fn capture_install(c: &Capture) -> io::Result<()> {
        need("ip")?;
        if !iface_name(&c.tun) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad interface name {:?}", c.tun)));
        }
        // Half a capture would send traffic nowhere: a failure removes what was installed.
        install(c).inspect_err(|_| drop(capture_remove(c)))
    }

    fn install(c: &Capture) -> io::Result<()> {
        let dev = c.tun.as_str();
        for (fam, v4) in families() {
            // The table first, then the rules that point at it.
            let bond = if v4 { c.bond4.map(|(a, p)| net(a.into(), p)) } else { c.bond6.map(|(a, p)| net(a.into(), p)) };
            let into = match (c.routes, &bond) {
                (Routes::All, _) => Some("default"),
                (Routes::BondOnly, b) => b.as_deref(),
            };
            if let Some(to) = into {
                ip(&format!("{fam} route replace {to} dev {dev} table {TABLE}"))?;
            }
            let out = if v4 { &OUT4[..] } else { &OUT6[..] }.iter().map(|s| s.to_string());
            let mesh = c.exclude.iter().filter(|(a, _)| a.is_ipv4() == v4).map(|&(a, p)| net(a, p));
            for o in out.chain(mesh).filter(|o| Some(o) != bond.as_ref()) {
                ip(&format!("{fam} route replace throw {o} table {TABLE}"))?;
            }
            for r in rules() {
                del_rule(fam, &r);
                ip(&format!("{fam} rule add {r}"))?;
            }
        }
        for l in c.links.iter().filter(|l| iface_name(l)) {
            save_set(SAVED_CAPTURE, &format!("/proc/sys/net/ipv4/conf/{l}/rp_filter"), "2")?;
        }
        let dns = match c.dns {
            Some(a) if find("resolvectl").is_some() => set_dns(dev, a).is_ok(),
            Some(_) => false,
            None => true,
        };
        DNS_LOCAL.store(!dns, Ordering::Relaxed);
        Ok(())
    }

    /// Undoes capture_install: the rules, table 7849, wb0's DNS and the lane links' rp_filter.
    pub fn capture_remove(c: &Capture) -> io::Result<()> {
        let routes = unroute();
        if c.dns.is_some() && find("resolvectl").is_some() && iface_name(&c.tun) {
            let _ = tool("resolvectl", &["revert", &c.tun], None); // wb0 may be gone, and its DNS with it
        }
        DNS_LOCAL.store(false, Ordering::Relaxed);
        restore(SAVED_CAPTURE)?;
        routes
    }

    /// Whether the last capture_install left DNS alone: no systemd-resolved, or it refused (§7).
    pub fn dns_local() -> bool {
        DNS_LOCAL.load(Ordering::Relaxed)
    }

    /// The nftables table `inet wirebond` (forward: the bond's sources out of wb0 and what comes
    /// back for them; postrouting: masquerade for the bond's sources leaving any other interface),
    /// loaded with `nft -f -`, then ip_forward and IPv6 forwarding saved and set (§6.2);
    /// `missing_tool nft` without nftables. The rules name no uplink, so they follow it.
    pub fn egress_enable(e: &Egress) -> io::Result<EgressOn> {
        if !iface_name(&e.tun) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad interface name {:?}", e.tun)));
        }
        let v6 = has_v6() && e.net6.is_some();
        tool("nft", &["-f", "-"], Some(&ruleset(e, v6)))?;
        let (up4, up6) = (default_dev("-4"), if v6 { default_dev("-6") } else { None });
        forward(v6, up6.as_deref()).inspect_err(|_| drop(egress_disable(e)))?;
        let (a, p) = e.net4;
        let first = Ipv4Addr::from_bits(mask4(a, p).to_bits() | 1);
        Ok(EgressOn { v4: up4.is_some(), v6: up6.is_some(), addr4: (first, p), uplink: up4.or(up6) })
    }

    /// ip_forward, and IPv6 forwarding, saved and set (§6.2). IPv6 forwarding makes the kernel
    /// ignore router advertisements where accept_ra is 1, which would take the anchor's own IPv6
    /// default route away: on the IPv6 uplink it becomes 2 (accept them while forwarding).
    fn forward(v6: bool, up6: Option<&str>) -> io::Result<()> {
        save_set(SAVED_EGRESS, "/proc/sys/net/ipv4/ip_forward", "1")?;
        if !v6 {
            return Ok(());
        }
        if let Some(up) = up6.filter(|u| iface_name(u)) {
            let ra = format!("/proc/sys/net/ipv6/conf/{up}/accept_ra");
            if fs::read_to_string(&ra).is_ok_and(|v| v.trim() == "1") {
                save_set(SAVED_EGRESS, &ra, "2")?;
            }
        }
        save_set(SAVED_EGRESS, "/proc/sys/net/ipv6/conf/all/forwarding", "1")
    }

    /// Undoes egress_enable: the table goes, and the forwarding sysctls get their old values.
    pub fn egress_disable(_e: &Egress) -> io::Result<()> {
        let nat = drop_table();
        restore(SAVED_EGRESS)?;
        nat
    }

    /// The table `inet wirebond` (§6.2, §6.3): created, emptied and filled in one transaction.
    pub(in crate::host) fn ruleset(e: &Egress, v6: bool) -> String {
        let (tun, (a, p)) = (&e.tun, e.net4);
        let net4 = net(a.into(), p);
        let net6 = e.net6.filter(|_| v6).map(|(a, p)| net(a.into(), p));
        let accept6 = net6.as_ref().map(|n| format!("        iifname \"{tun}\" ip6 saddr {n} accept\n"));
        let nat6 = net6.as_ref().map(|n| format!("        oifname != \"{tun}\" ip6 saddr {n} masquerade\n"));
        format!(
            "table inet {NFT_TABLE}\ndelete table inet {NFT_TABLE}\ntable inet {NFT_TABLE} {{\n\
             \x20   chain forward {{\n\
             \x20       type filter hook forward priority filter; policy accept;\n\
             \x20       iifname \"{tun}\" oifname \"{tun}\" drop\n\
             \x20       iifname \"{tun}\" ip saddr {net4} accept\n{}\
             \x20       iifname \"{tun}\" drop\n\
             \x20       oifname \"{tun}\" ct state established,related accept\n\
             \x20       oifname \"{tun}\" drop\n\
             \x20   }}\n\
             \x20   chain postrouting {{\n\
             \x20       type nat hook postrouting priority srcnat; policy accept;\n\
             \x20       oifname != \"{tun}\" ip saddr {net4} masquerade\n{}\
             \x20   }}\n}}\n",
            accept6.unwrap_or_default(),
            nat6.unwrap_or_default(),
        )
    }

    /// The first `nameserver` in /etc/resolv.conf, port 53 (§7).
    pub fn upstream_resolver() -> io::Result<SocketAddr> {
        let text = fs::read_to_string(rooted("/etc/resolv.conf"))?;
        first_nameserver(&text)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no nameserver in /etc/resolv.conf"))
    }

    /// resolv.conf's first usable `nameserver`, port 53; an IPv6 one may carry a zone (`%eth0`).
    pub(in crate::host) fn first_nameserver(text: &str) -> Option<SocketAddr> {
        text.lines().find_map(|l| {
            let mut w = l.split(['#', ';']).next()?.split_whitespace();
            let addr = (w.next() == Some("nameserver")).then(|| w.next()).flatten()?;
            match addr.split_once('%') {
                None => addr.parse::<IpAddr>().ok().map(|a| SocketAddr::new(a, 53)),
                Some((a, zone)) => {
                    let a = a.parse::<Ipv6Addr>().ok()?;
                    let id = zone.parse().ok().or_else(|| {
                        let z = std::ffi::CString::new(zone).ok()?;
                        // SAFETY: z is a C string.
                        Some(unsafe { libc::if_nametoindex(z.as_ptr()) }).filter(|&i| i != 0)
                    })?;
                    Some(SocketAddr::V6(SocketAddrV6::new(a, 53, 0, id)))
                }
            }
        })
    }

    /// `nmcli connection modify <cable's connection> ipv4.method shared`, or a new shared
    /// connection on the cable when it has none, brought up (§8). If NetworkManager refuses or is
    /// absent, the error carries the manual steps.
    pub fn support_share(s: &Share) -> io::Result<()> {
        share(s).map_err(|e| io::Error::new(e.kind(), format!("{e}\n{}", manual(s))))
    }

    /// Undoes support_share: the method it replaced comes back, and a connection it made goes.
    pub fn support_unshare(s: &Share) -> io::Result<()> {
        if let Some((con, method)) = recall(&s.cable) {
            tool("nmcli", &["connection", "modify", &con, "ipv4.method", &method], None)?;
            let _ = tool("nmcli", &["connection", "up", &con], None);
            forget(&s.cable)?;
        }
        let made = made(&s.cable);
        if tool("nmcli", &["-g", "connection.id", "connection", "show", &made], None).is_ok() {
            tool("nmcli", &["connection", "delete", &made], None)?;
        }
        Ok(())
    }

    /// Deletes what a crash left behind (§5.4): the rules, table 7849, wb0, the nft table, and the
    /// sysctls capture and egress changed, which get their old values back.
    pub fn cleanup_stale() -> io::Result<()> {
        let routes = unroute();
        if ip(&format!("link show dev {TUN_NAME}")).is_ok() {
            let _ = ip(&format!("link del dev {TUN_NAME}"));
        }
        let nat = drop_table();
        restore(SAVED_CAPTURE)?;
        restore(SAVED_EGRESS)?;
        routes.and(nat)
    }

    /// The tools wirebond needs that are absent (§5.3, §6.2): `ip`, and `nft` for an anchor.
    pub fn missing_tools() -> Vec<&'static str> {
        ["ip", "nft"].into_iter().filter(|t| find(t).is_none()).collect()
    }

    // ---- helpers

    /// The address families to configure: IPv4, and IPv6 unless the kernel has none.
    fn families() -> Vec<(&'static str, bool)> {
        let mut f = vec![("-4", true)];
        if has_v6() {
            f.push(("-6", false));
        }
        f
    }

    /// Whether the kernel has IPv6 (`ipv6.disable=1` removes /proc/net/if_inet6).
    fn has_v6() -> bool {
        Path::new("/proc/net/if_inet6").exists()
    }

    /// An interface name the kernel would accept, safe in a path and as one argument.
    fn iface_name(n: &str) -> bool {
        let bad = |c: char| c == '/' || c == ':' || c.is_whitespace();
        !n.is_empty() && n.len() < libc::IFNAMSIZ && !matches!(n, "." | "..") && !n.contains(bad)
    }

    /// The two rules (§5.3), as `ip rule add` and `del` take them.
    fn rules() -> [String; 2] {
        [
            format!("priority {PRIO_MAIN} lookup main suppress_prefixlength 0"),
            format!("priority {PRIO_CAPTURE} not fwmark {LANE_MARK:#x} lookup {TABLE}"),
        ]
    }

    /// Deletes every copy of rule `r`: only wirebond's, matched in full.
    fn del_rule(fam: &str, r: &str) {
        for _ in 0..16 {
            if ip(&format!("{fam} rule del {r}")).is_err() {
                break;
            }
        }
    }

    /// The rules and table 7849, both families; `missing_tool ip` without iproute2.
    fn unroute() -> io::Result<()> {
        need("ip")?;
        for (fam, _) in families() {
            for r in rules() {
                del_rule(fam, &r);
            }
            // A table that never held a route does not exist, and flushing it fails.
            if let Err(e) = ip(&format!("{fam} route flush table {TABLE}"))
                && ip(&format!("{fam} route show table {TABLE}")).is_ok_and(|t| !t.trim().is_empty())
            {
                return Err(e);
            }
        }
        Ok(())
    }

    /// `resolvectl dns`, `domain '~.'` and `default-route yes` on the device (§7).
    fn set_dns(dev: &str, a: IpAddr) -> io::Result<()> {
        tool("resolvectl", &["dns", dev, &a.to_string()], None)?;
        tool("resolvectl", &["domain", dev, "~."], None)?;
        tool("resolvectl", &["default-route", dev, "yes"], None).map(drop)
    }

    /// Deletes the table `inet wirebond` if it is there; without nft there is none.
    fn drop_table() -> io::Result<()> {
        if find("nft").is_none() || tool("nft", &["list", "table", "inet", NFT_TABLE], None).is_err() {
            return Ok(());
        }
        tool("nft", &["delete", "table", "inet", NFT_TABLE], None).map(drop)
    }

    /// The device of the default route of `fam` in main, which NAT leaves through (§6.2).
    fn default_dev(fam: &str) -> Option<String> {
        let out = ip(&format!("{fam} route show default")).ok()?;
        let mut w = out.split_whitespace();
        w.find(|&t| t == "dev").and_then(|_| w.next()).map(str::to_owned)
    }

    /// `a/p` with the host bits cleared, as `ip` and `nft` want a network.
    fn net(a: IpAddr, p: u8) -> String {
        match a {
            IpAddr::V4(a) => format!("{}/{p}", mask4(a, p)),
            IpAddr::V6(a) => {
                let m = u128::MAX.checked_shl(128 - u32::from(p.min(128))).unwrap_or(0);
                format!("{}/{p}", Ipv6Addr::from_bits(a.to_bits() & m))
            }
        }
    }

    fn mask4(a: Ipv4Addr, p: u8) -> Ipv4Addr {
        Ipv4Addr::from_bits(a.to_bits() & u32::MAX.checked_shl(32 - u32::from(p.min(32))).unwrap_or(0))
    }

    /// Writes `value` to the sysctl file `key`, first saving its old value in `file` unless saved
    /// already (enabled twice, the first old value stays). A value already right is left alone,
    /// and so is not saved: wirebond undoes only what it changed.
    fn save_set(file: &str, key: &str, value: &str) -> io::Result<()> {
        let path = rooted(file);
        let mut saved = read_saved(&path);
        if !saved.iter().any(|(k, _)| k == key) {
            let now = fs::read_to_string(key)?.trim().to_owned();
            if now == value {
                return Ok(());
            }
            saved.push((key.to_owned(), now));
            write_saved(&path, &saved)?; // before the change: a crash after it still finds the old value
        }
        fs::write(key, value)
    }

    /// Puts back every value `file` saved, then deletes it. A sysctl that is gone (its interface
    /// with it) is skipped.
    fn restore(file: &str) -> io::Result<()> {
        let path = rooted(file);
        for (k, v) in read_saved(&path) {
            match fs::write(&k, &v) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// The saved values: one `key value` per line.
    fn read_saved(path: &Path) -> Vec<(String, String)> {
        let text = fs::read_to_string(path).unwrap_or_default();
        text.lines().filter_map(|l| l.split_once(' ')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
    }

    fn write_saved(path: &Path, saved: &[(String, String)]) -> io::Result<()> {
        fs::create_dir_all(path.parent().unwrap_or(path))?;
        let body: String = saved.iter().map(|(k, v)| format!("{k} {v}\n")).collect();
        fs::write(path, body)
    }

    /// The connection `support on` makes when the cable has none.
    fn made(cable: &str) -> String {
        format!("wirebond-{cable}")
    }

    fn share(s: &Share) -> io::Result<()> {
        if !iface_name(&s.cable) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad interface name {:?}", s.cable)));
        }
        let out = tool("nmcli", &["-g", "GENERAL.CONNECTION", "device", "show", &s.cable], None)?;
        let con = match out.trim() {
            "" | "--" => {
                let m = made(&s.cable);
                let args = ["connection", "add", "type", "ethernet", "ifname", &s.cable, "con-name", &m];
                tool("nmcli", &[&args[..], &["ipv4.method", "shared"]].concat(), None)?;
                m
            }
            con => {
                let con = con.to_owned();
                let method = tool("nmcli", &["-g", "ipv4.method", "connection", "show", &con], None)?;
                if method.trim() != "shared" {
                    remember(&s.cable, &con, method.trim())?;
                    tool("nmcli", &["connection", "modify", &con, "ipv4.method", "shared"], None)?;
                }
                con
            }
        };
        tool("nmcli", &["connection", "up", &con], None).map(drop)
    }

    /// What to do by hand when NetworkManager cannot share (§8).
    fn manual(s: &Share) -> String {
        let (up, cable) = (&s.uplink, &s.cable);
        format!(
            "To share {up} to {cable} by hand: in NetworkManager set {cable}'s connection's IPv4 method to \
             \"Shared to other computers\" (nmcli connection modify <connection> ipv4.method shared, then \
             nmcli connection up <connection>); without it, give {cable} an address \
             (ip addr add 192.168.77.1/24 dev {cable}), serve DHCP on it, set net.ipv4.ip_forward=1 and \
             masquerade what leaves {up} (an nftables postrouting rule: oifname \"{up}\" masquerade)."
        )
    }

    /// The method `support on` replaced, kept in the state directory: a connection's method
    /// outlives a reboot.
    fn shared_file() -> PathBuf {
        super::paths(Scope::System).1.join("shared")
    }

    fn remember(cable: &str, con: &str, method: &str) -> io::Result<()> {
        let path = shared_file();
        let mut lines: Vec<String> = fs::read_to_string(&path).unwrap_or_default().lines().map(str::to_owned).collect();
        lines.retain(|l| l.split('\t').next() != Some(cable));
        lines.push(format!("{cable}\t{con}\t{method}"));
        fs::create_dir_all(path.parent().unwrap_or(&path))?;
        fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>())
    }

    fn recall(cable: &str) -> Option<(String, String)> {
        let text = fs::read_to_string(shared_file()).ok()?;
        text.lines().find_map(|l| {
            let mut f = l.split('\t');
            (f.next() == Some(cable)).then(|| Some((f.next()?.to_owned(), f.next()?.to_owned()))).flatten()
        })
    }

    fn forget(cable: &str) -> io::Result<()> {
        let path = shared_file();
        let text = fs::read_to_string(&path).unwrap_or_default();
        let kept: String =
            text.lines().filter(|l| l.split('\t').next() != Some(cable)).map(|l| format!("{l}\n")).collect();
        if kept.is_empty() { super::remove([&path]).map(drop) } else { fs::write(&path, kept) }
    }

    /// `ip` with the words of `args`; no word holds a space.
    fn ip(args: &str) -> io::Result<String> {
        tool("ip", &args.split_whitespace().collect::<Vec<_>>(), None)
    }

    /// `missing_tool <name>` unless the tool is installed.
    fn need(name: &str) -> io::Result<()> {
        find(name).map(drop).ok_or_else(|| missing_tool_error(name))
    }

    /// The tool's path: on PATH, else in the sbin and bin directories a service's PATH may lack.
    fn find(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::env::split_paths(&path).chain(TOOL_DIRS.iter().map(PathBuf::from));
        dirs.map(|d| d.join(name)).find(|p| {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    }

    /// Runs tool `name` with `args` and `input` on its stdin, and returns its stdout, or an error
    /// with its stderr unless it exits 0: `missing_tool <name>` when it is not installed. Nothing
    /// reaches the terminal.
    pub(in crate::host) fn tool(name: &str, args: &[&str], input: Option<&str>) -> io::Result<String> {
        let path = find(name).ok_or_else(|| missing_tool_error(name))?;
        let mut c = Command::new(path);
        c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
        let mut child = c.spawn()?;
        if let (Some(i), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(i.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        match out.status.success() {
            true => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            false => Err(io::Error::other(format!("{name} {}: {}", args.join(" "), stderr.trim()))),
        }
    }
}
