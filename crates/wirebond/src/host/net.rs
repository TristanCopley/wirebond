//! The 2.0 platform API (SPEC 2.0 §5–§8): the TUN device, capture on a client, egress and NAT on
//! an anchor, the anchor's upstream resolver, a support's sharing, and stale-state cleanup. The
//! types and the contract are here; each OS implements the functions in linux.rs, macos.rs and
//! windows.rs (`os::`), and host/mod.rs re-exports them. Owners: host linux; host macos and
//! windows (notes/2.0-build.md).
//!
//! Every function is idempotent and undoes only what wirebond made.
//!
//! ```text
//! Tun::open(&TunConfig) -> io::Result<Tun>     Tun::name(&self) -> &str
//! Tun::read(&self, &mut [u8]) -> io::Result<usize>   one packet; WouldBlock when none waits
//! Tun::write(&self, &[u8]) -> io::Result<()>         one packet
//! unix: Tun: AsRawFd, waited on with Poll::add_tun   windows: Tun::read_event() -> HANDLE (§4.3)
//! capture_install(&Capture) -> io::Result<()>       capture_remove(&Capture) -> io::Result<()>
//! egress_enable(&Egress) -> io::Result<EgressOn>    egress_disable(&Egress) -> io::Result<()>
//! upstream_resolver() -> io::Result<SocketAddr>
//! support_share(&Share) -> io::Result<()>           support_unshare(&Share) -> io::Result<()>
//! cleanup_stale() -> io::Result<()>
//! dns_local() -> bool
//! ```
//!
//! An error of a function that needs a tool the device lacks is of kind NotFound and reads
//! `missing_tool <tool>` (Linux: `ip`, `nft`, `nmcli`). The daemon reports a TUN, capture or egress
//! call that failed with that kind as the problem `missing_tool`. `dns_local` says the last
//! `capture_install` left DNS alone (Linux without systemd-resolved, §7): it is Linux's, and
//! host/mod.rs answers false on the other OSes.
//!
//! Lane sockets are pinned by `listen` (§5.3): on Linux SO_MARK 0x7849 (`LANE_MARK`) and, for
//! IPv6, SO_BINDTODEVICE (IPv4: IP_UNICAST_IF, see linux.rs); on macOS IP_BOUND_IF /
//! IPV6_BOUND_IF; on Windows IP_UNICAST_IF / IPV6_UNICAST_IF.

#[cfg(target_os = "linux")]
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The message of a function the platform at hand does not have, with kind `Unsupported`. Every
/// function of this API is written for Linux, macOS and Windows; node returns this from its
/// `Platform` trait's default `support_share`, which only a test's fake host leaves in place, and
/// for the interface counters of any other OS.
pub const NOT_YET: &str = "2.0: not yet";

/// The Linux fwmark on lane sockets (§5.3): `not fwmark 0x7849` keeps them out of capture.
#[cfg(target_os = "linux")]
pub const LANE_MARK: u32 = 0x7849;
/// The routing table capture uses on Linux (§5.3): table 7849, decimal, as `ip` prints it.
#[cfg(target_os = "linux")]
pub const TABLE: u32 = 7849;
/// The Linux interface name; macOS takes the next utunN, Windows names its adapter `wirebond`.
pub const TUN_NAME: &str = if cfg!(windows) { "wirebond" } else { "wb0" };

/// The error of a function that needs a tool this device lacks: kind NotFound, its message
/// `missing_tool <tool>` (§5.3, §6.2, §22.2). Only Linux's functions run tools that may be absent.
#[cfg(target_os = "linux")]
pub fn missing_tool_error(tool: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("missing_tool {tool}"))
}

/// An address and prefix length.
pub type Net4 = (Ipv4Addr, u8);
pub type Net6 = (Ipv6Addr, u8);

/// How to open the TUN device (§5.1, §6.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunConfig {
    /// [`TUN_NAME`]; macOS ignores it.
    pub name: String,
    /// TUN_MTU.
    pub mtu: usize,
    /// This node's bond address on the device: the client's from `bond ok`, the anchor's .1.
    pub addr4: Option<Net4>,
    pub addr6: Option<Net6>,
}

/// What capture routes into the device (§5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routes {
    /// Everything: the default routes (Linux) or the four /1 routes (macOS, Windows).
    All,
    /// Only the bond's subnets: the anchor has no egress (§5.2, `no_egress`).
    BondOnly,
}

/// A client's capture (§5): installed when the bond has a validated lane, removed on `client
/// off`, a role change, fail open and exit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    /// The device's name, as [`TunConfig`] opened it (macOS: the utunN it got).
    pub tun: String,
    pub routes: Routes,
    /// The bond's subnets: always routed into the device.
    pub bond4: Option<Net4>,
    pub bond6: Option<Net6>,
    /// What stays out besides local subnets, link-local, multicast, broadcast and loopback, which
    /// every OS leaves out by itself: the mesh's ranges (config `mesh`).
    pub exclude: Vec<(IpAddr, u8)>,
    /// The links that carry lanes: on Linux each gets rp_filter 2 (loose).
    pub links: Vec<String>,
    /// The resolver to use while bonded: the anchor's bond address (§7). None leaves DNS alone.
    pub dns: Option<IpAddr>,
}

/// An anchor's egress (§6): forwarding and NAT for the bond's sources onto the uplink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Egress {
    pub tun: String,
    /// The bond subnets (config `bond_net4`, `bond_net6`). While Windows ICS shares, it fixes
    /// 192.168.137.0/24 instead of `net4`.
    pub net4: Net4,
    pub net6: Option<Net6>,
}

/// What [`egress_enable`](super::egress_enable) achieved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressOn {
    pub v4: bool,
    pub v6: bool,
    /// The anchor's own address and subnet, which the device has: 10.77.0.1/24, or 192.168.137.1/24
    /// while Windows ICS shares. Clients get addresses in this subnet and send to this address.
    pub addr4: Net4,
    /// The interface of the default route, which NAT follows (re-checked every LINK_SCAN).
    pub uplink: Option<String>,
    /// Why there is no egress, when there is none and the host can say: status's `no_egress`.
    pub why: Option<String>,
}

/// A support's sharing (§8): `uplink` shared to `cable` with the OS's own sharing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub uplink: String,
    pub cable: String,
}
