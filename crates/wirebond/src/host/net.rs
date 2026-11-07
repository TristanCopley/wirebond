//! The 2.0 platform API (SPEC 2.0 §5–§8): the TUN device, capture on a client, egress and NAT on
//! an anchor, the anchor's upstream resolver, a support's sharing, and stale-state cleanup. The
//! types and the contract are here; each OS implements the functions in linux.rs, macos.rs and
//! windows.rs (`os::`), and host/mod.rs re-exports them. Owners: host linux; host macos and
//! windows (notes/2.0-build.md).
//!
//! Every function is idempotent and undoes only what wirebond made. One not yet written returns
//! an `io::Error` of kind `Unsupported` whose message is [`NOT_YET`].
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
//! missing_tools() -> Vec<&'static str>              dns_local() -> bool
//! ```
//!
//! An error naming a tool the device lacks reads `missing_tool <tool>` ([`missing_tool`]).
//! `missing_tools` lists the tools absent at start (Linux: `ip`, `nft`), and `dns_local` says the
//! last `capture_install` left DNS alone (Linux without systemd-resolved, §7): both are Linux's,
//! and host/mod.rs answers none and false on the other OSes.
//!
//! Lane sockets are pinned by `listen` (§5.3): on Linux SO_MARK [`LANE_MARK`] and
//! SO_BINDTODEVICE; on macOS IP_BOUND_IF / IPV6_BOUND_IF; on Windows IP_UNICAST_IF /
//! IPV6_UNICAST_IF.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The message of a function not yet written.
pub const NOT_YET: &str = "2.0: not yet";

/// The Linux fwmark on lane sockets (§5.3): `not fwmark 0x7849` keeps them out of capture.
pub const LANE_MARK: u32 = 0x7849;
/// The routing table capture uses on Linux (§5.3): table 7849, decimal, as `ip` prints it.
pub const TABLE: u32 = 7849;
/// The Linux interface name; macOS takes the next utunN, Windows names its adapter `wirebond`.
pub const TUN_NAME: &str = if cfg!(windows) { "wirebond" } else { "wb0" };

/// `Err(Unsupported, NOT_YET)`.
pub fn not_yet<T>() -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, NOT_YET))
}

/// The error of a function that needs a tool this device lacks: kind NotFound, its message
/// starting `missing_tool <tool>` (§5.3, §6.2, §22.2).
pub fn missing_tool_error(tool: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("missing_tool {tool}"))
}

/// The tool an error of this API names as missing, which the daemon reports as problem
/// `missing_tool` (§5.3, §6.2); None for any other error.
pub fn missing_tool(e: &io::Error) -> Option<String> {
    let s = e.to_string();
    let rest = s.strip_prefix("missing_tool ")?;
    rest.split(|c: char| c == ':' || c.is_whitespace()).next().filter(|t| !t.is_empty()).map(str::to_owned)
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
    /// The bond subnets (config `bond_net4`, `bond_net6`); Windows ignores `net4` (ICS fixes
    /// 192.168.137.0/24).
    pub net4: Net4,
    pub net6: Option<Net6>,
}

/// What [`egress_enable`](super::egress_enable) achieved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressOn {
    pub v4: bool,
    pub v6: bool,
    /// The anchor's own address and subnet on the device: 10.77.0.1/24, or 192.168.137.1/24 where
    /// Windows ICS readdressed the adapter. Clients get addresses in this subnet.
    pub addr4: Net4,
    /// The interface of the default route, which NAT follows (re-checked every LINK_SCAN).
    pub uplink: Option<String>,
}

/// A support's sharing (§8): `uplink` shared to `cable` with the OS's own sharing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub uplink: String,
    pub cable: String,
}
