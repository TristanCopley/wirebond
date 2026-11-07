//! Supports and contribution (SPEC 2.0 §8, §16 `support`, §20.3). A computer as a support shares
//! its uplink to its cable with the OS's own sharing (`host::support_share`), and with `--for`
//! registers with that client over the mesh: `support` with the cable's subnet and its name. The
//! client names the link in that subnet after the support, in its status and in the `links` it
//! offers, so the anchor's lanes carry the name too. The support counts the bytes it carried on
//! its cable from the OS's interface counters. A client's contribution is the bytes on each of
//! its links, with each link's share of the bond. Owner: node features.

use std::io;
use std::net::IpAddr;

use wirebond_core::{Mono, SEC};

use super::{Daemon, Msg, ctl, link, network, proto};
use crate::api::{self, Role};
use crate::host;

/// How often a support registers again over its open channel, or redials a lost one.
const REGISTER_EVERY: Mono = 60 * SEC;
const REDIAL: Mono = 10 * SEC;
/// A registration not refreshed for this long is forgotten.
const REGISTRATION_TTL: Mono = 3 * REGISTER_EVERY;
/// The cable's subnet: every OS's sharing (NetworkManager 10.42.0.1/24, Internet Sharing
/// 192.168.2.1/24, ICS 192.168.137.1/24) hands out a /24, and the links carry no prefix.
const CABLE_BITS4: u8 = 24;
const CABLE_BITS6: u8 = 64;

/// One support's registration on a client.
struct Reg {
    /// The support's mesh address: a later registration from it replaces this one.
    peer: IpAddr,
    net: (IpAddr, u8),
    name: String,
    at: Mono,
}

/// A support's and a client's state.
#[derive(Default)]
pub struct State {
    /// The sharing made, and the one last tried (not tried again until the config changes).
    shared: Option<host::Share>,
    tried: Option<host::Share>,
    /// The manual steps, when the OS refused the sharing.
    pub refused: Option<String>,
    /// The registration channel, and when to register or dial next.
    chan: Option<u64>,
    next_reg: Mono,
    /// The cable's OS counters as last read (received, sent), and the bytes carried since.
    counters: Option<(u64, u64)>,
    pub carried: (u64, u64),
    next_count: Mono,
    /// A client's supports.
    regs: Vec<Reg>,
}

/// What to do by hand when the OS refuses to share (§8).
pub fn manual(s: &host::Share, e: &io::Error) -> String {
    let (up, cable) = (&s.uplink, &s.cable);
    let how = if cfg!(target_os = "linux") {
        format!("nmcli connection modify {cable} ipv4.method shared && nmcli connection up {cable}")
    } else if cfg!(target_os = "macos") {
        format!("System Settings > General > Sharing > Internet Sharing: share {up} with {cable}, then turn it on")
    } else {
        format!(
            "Control Panel > Network Connections > {up} > Properties > Sharing: allow other network users to \
             connect through this computer's Internet connection, home networking connection {cable}"
        )
    };
    format!("sharing {up} to {cable} was refused ({e}); share it by hand: {how}")
}

/// The cable's subnet as `support` carries it: "10.42.0.0/24".
fn subnet(a: IpAddr) -> String {
    let bits = if a.is_ipv4() { CABLE_BITS4 } else { CABLE_BITS6 };
    format!("{}/{bits}", network(a, bits))
}

/// "10.42.0.0/24" back: the address and the prefix length, which must fit the family.
fn parse_subnet(s: &str) -> Option<(IpAddr, u8)> {
    let (a, b) = s.split_once('/')?;
    let (a, b): (IpAddr, u8) = (a.parse().ok()?, b.parse().ok()?);
    (b <= if a.is_ipv4() { 32 } else { 128 }).then_some((a, b))
}

impl Daemon {
    /// The support role (§8): the sharing follows the config, the registration with the client
    /// (`--for`) is kept fresh, and the cable's counters are read once a second.
    pub(super) fn support_tick(&mut self) {
        let now = self.now();
        let want = self
            .cfg
            .support
            .as_ref()
            .filter(|_| self.cfg.role == Role::Support)
            .map(|s| host::Share { uplink: s.uplink.clone(), cable: s.cable.clone() });
        if self.support.shared != want
            && let Some(old) = self.support.shared.take()
        {
            match self.plat.support_unshare(&old) {
                Ok(()) => self.log(format!("support off: {} no longer shared to {}", old.uplink, old.cable)),
                Err(e) => self.log(format!("support off: {e}")),
            }
        }
        if want.is_none() {
            let st = &mut self.support;
            (st.tried, st.refused, st.counters, st.carried) = (None, None, None, (0, 0));
            if let Some(p) = st.chan.take().and_then(|id| self.chans.get(&id)) {
                p.chan.close();
            }
            return;
        }
        let Some(want) = want else { return };
        if self.support.shared.is_none() && self.support.tried.as_ref() != Some(&want) {
            self.support.tried = Some(want.clone());
            match self.plat.support_share(&want) {
                Ok(()) => {
                    self.support.refused = None;
                    self.support.shared = Some(want.clone());
                    self.log(format!("support on: {} shared to {}", want.uplink, want.cable));
                }
                Err(e) => {
                    let steps = manual(&want, &e);
                    self.log(steps.clone());
                    self.support.refused = Some(steps);
                }
            }
        }
        self.support_count(now, &want.cable);
        self.support_register(now, &want.cable);
    }

    /// Adds what the cable carried since the last read of its OS counters.
    fn support_count(&mut self, now: Mono, cable: &str) {
        if now < self.support.next_count {
            return;
        }
        self.support.next_count = now + SEC;
        let link = self.socks.iter().find(|s| s.link.name == cable).map(|s| s.link.clone()).unwrap_or(host::Link {
            index: 0,
            name: cable.into(),
            addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            label: cable.into(),
            metered: false,
        });
        let Ok((rx, tx)) = self.plat.if_bytes(&link) else { return };
        let st = &mut self.support;
        if let Some((rx0, tx0)) = st.counters {
            // A counter that went back was reset (the cable replugged): count from zero.
            st.carried.0 += if rx >= rx0 { rx - rx0 } else { rx };
            st.carried.1 += if tx >= tx0 { tx - tx0 } else { tx };
        }
        st.counters = Some((rx, tx));
    }

    /// Registers with the client named by `--for` (§8, §16 `support`): dials it over the mesh,
    /// and once its hello is in, `support_hello` sends the cable's subnet and this node's name.
    fn support_register(&mut self, now: Mono, cable: &str) {
        let Some(client) = self.cfg.support.as_ref().and_then(|s| s.for_client.clone()) else { return };
        if now < self.support.next_reg || !self.socks.iter().any(|s| s.id != 0 && s.link.name == cable) {
            return;
        }
        match self.support.chan.filter(|id| self.chans.get(id).is_some_and(|p| p.hello.is_some())) {
            Some(id) => {
                self.support.next_reg = now + REGISTER_EVERY;
                self.support_hello(id);
            }
            None => {
                if let Some(p) = self.support.chan.take().and_then(|id| self.chans.get(&id)) {
                    p.chan.close();
                }
                let id = ctl::next_id();
                (self.support.chan, self.support.next_reg) = (Some(id), now + REDIAL);
                ctl::dial(client, self.port, self.mesh_addr(), id, self.ev_tx.clone(), self.waker.clone());
            }
        }
    }

    /// A hello on channel `id` dialed for nothing else: the support's registration goes out.
    pub(super) fn support_hello(&mut self, id: u64) {
        if self.support.chan != Some(id) || self.cfg.role != Role::Support {
            return;
        }
        let Some(cable) = self.cfg.support.as_ref().map(|s| s.cable.clone()) else { return };
        let Some(addr) = self.socks.iter().find(|s| s.id != 0 && s.link.name == cable).map(|s| s.link.addr) else {
            return;
        };
        let m = Msg::Support(proto::Support { cable_subnet: subnet(addr), name: self.name.clone() });
        self.send_chan(id, &m);
    }

    /// A support registered on channel `id` (§8): the link in its cable's subnet is named after
    /// it from now on, and the peers hear my links again.
    pub(super) fn support_registered(&mut self, id: u64, s: proto::Support) {
        let now = self.now();
        let Some(peer) = self.chans.get(&id).map(|p| p.chan.peer.ip().to_canonical()) else { return };
        let Some(net) = parse_subnet(&s.cable_subnet) else {
            return self.log(format!("support from {peer}: bad cable_subnet {:?}", s.cable_subnet));
        };
        let name = link::clean(&s.name, link::LABEL_MAX);
        let fresh = !self.support.regs.iter().any(|r| r.peer == peer && r.net == net && r.name == name);
        self.support.regs.retain(|r| r.peer != peer && now - r.at < REGISTRATION_TTL);
        self.support.regs.push(Reg { peer, net: (network(net.0, net.1), net.1), name: name.clone(), at: now });
        if fresh {
            self.log(format!("support {name} registered: {}", s.cable_subnet));
            self.links_changed();
        }
    }

    /// The support that gives my link at `addr`, if one registered (§8).
    pub(super) fn support_name(&self, addr: IpAddr) -> Option<String> {
        let now = self.now();
        self.support
            .regs
            .iter()
            .filter(|r| now - r.at < REGISTRATION_TTL && r.net.0.is_ipv4() == addr.is_ipv4())
            .find(|r| network(addr, r.net.1) == r.net.0)
            .map(|r| r.name.clone())
    }

    /// This node's contribution (§20.3): a client's bytes on each of its links with each link's
    /// share of the bond; a support's bytes on its cable from the OS counters; otherwise the
    /// lane bytes since start.
    pub(super) fn contribution(&self) -> api::Contribution {
        let mut c = api::Contribution {
            tx_bytes: self.tx_bytes,
            rx_bytes: self.rx_bytes,
            since: "start".into(),
            ..Default::default()
        };
        match self.cfg.role {
            Role::Support => (c.rx_bytes, c.tx_bytes) = self.support.carried,
            Role::Client => {
                let Some(snap) = self.bonds.first().and_then(|b| b.session.as_ref()).map(|s| s.snapshot()) else {
                    return c;
                };
                let total: u64 = snap.lanes.iter().map(|l| l.tx_bytes + l.rx_bytes).sum();
                for l in &snap.lanes {
                    let name = if l.id == 0 || l.local.is_empty() { "mesh".to_string() } else { l.local.clone() };
                    let i = match c.links.iter().position(|x| x.link == name) {
                        Some(i) => i,
                        None => {
                            c.links.push(api::LinkBytes { link: name, ..Default::default() });
                            c.links.len() - 1
                        }
                    };
                    let x = &mut c.links[i];
                    (x.tx_bytes, x.rx_bytes) = (x.tx_bytes + l.tx_bytes, x.rx_bytes + l.rx_bytes);
                }
                for x in &mut c.links {
                    x.share = if total == 0 { 0.0 } else { (x.tx_bytes + x.rx_bytes) as f64 / total as f64 };
                }
            }
            _ => {}
        }
        c
    }
}

/// The bytes the OS counted on `link`'s interface: received, sent (§20.3).
pub fn os_bytes(link: &host::Link) -> io::Result<(u64, u64)> {
    os::bytes(link)
}

#[cfg(target_os = "linux")]
mod os {
    use super::*;

    pub fn bytes(link: &host::Link) -> io::Result<(u64, u64)> {
        if link.name.contains('/') || link.name.starts_with('.') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad interface name"));
        }
        let read = |what: &str| -> io::Result<u64> {
            let p = format!("/sys/class/net/{}/statistics/{what}", link.name);
            std::fs::read_to_string(p)?.trim().parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        };
        Ok((read("rx_bytes")?, read("tx_bytes")?))
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::*;

    /// From getifaddrs' AF_LINK entry: its if_data counters (32 bits; a wrap reads as a reset).
    pub fn bytes(link: &host::Link) -> io::Result<(u64, u64)> {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: getifaddrs fills `head` with a list freed below with freeifaddrs.
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut found = None;
        let mut p = head;
        while !p.is_null() {
            // SAFETY: `p` is a node of the list getifaddrs returned, alive until freeifaddrs.
            let ifa = unsafe { &*p };
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_data.is_null() || ifa.ifa_name.is_null() {
                continue;
            }
            // SAFETY: non-null, as checked; the name is NUL-terminated and AF_LINK's data is an if_data.
            let (family, name) =
                unsafe { (i32::from((*ifa.ifa_addr).sa_family), std::ffi::CStr::from_ptr(ifa.ifa_name)) };
            if family == libc::AF_LINK && name.to_bytes() == link.name.as_bytes() {
                // SAFETY: as above.
                let d = unsafe { &*(ifa.ifa_data as *const libc::if_data) };
                found = Some((u64::from(d.ifi_ibytes), u64::from(d.ifi_obytes)));
                break;
            }
        }
        // SAFETY: `head` came from getifaddrs and is freed once.
        unsafe { libc::freeifaddrs(head) };
        found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no interface {}", link.name)))
    }
}

#[cfg(windows)]
mod os {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetIfEntry2, MIB_IF_ROW2};

    use super::*;

    /// From GetIfEntry2 on the link's interface index: InOctets and OutOctets.
    pub fn bytes(link: &host::Link) -> io::Result<(u64, u64)> {
        // SAFETY: MIB_IF_ROW2 is plain data; zero is a valid value for every field.
        let mut row: MIB_IF_ROW2 = unsafe { std::mem::zeroed() };
        row.InterfaceIndex = link.index;
        // SAFETY: `row` is a valid MIB_IF_ROW2 with its index set, as GetIfEntry2 asks.
        let rc = unsafe { GetIfEntry2(&mut row) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok((row.InOctets, row.OutOctets))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod os {
    use super::*;

    pub fn bytes(_: &host::Link) -> io::Result<(u64, u64)> {
        Err(io::Error::new(io::ErrorKind::Unsupported, host::NOT_YET))
    }
}

#[cfg(test)]
mod tests;
