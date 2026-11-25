//! Bonds (SPEC 2.0 §3, §17): what the daemon keeps per bond, and the anchor's bond addresses.
//! The lifecycle is §3.2's; the failure rules are §17's. Owner: node daemon.
//!
//! Invariants:
//! - a client has at most one bond; an anchor at most MAX_BONDS, one per client mesh address;
//! - capture is installed only in [`BondState::Up`] (at least one validated lane) and removed on
//!   `client off`, a role change, fail open (FAIL_OPEN_AFTER) and daemon exit (§5.4);
//! - an anchor forwards only packets whose source is the bond's client address, and delivers to a
//!   client only packets for it (§6.3, §10.5 rule 6); bond-to-bond traffic is dropped;
//! - `pick`'s temporary bond (`measure`, §18) is the exception to one bond per client: it has no
//!   bond addresses, so it never reaches the TUN device, the internet or capture;
//! - a lane is down only when the engine reports it down (§17: ALL_LANES_DOWN), never because it
//!   is merely suspect; a lane never validated has not come up;
//! - a bond is lost by what the daemon observes, not by a lane's state, which says nothing of the
//!   path while the bond idles: the channel down and nothing heard from the peer, each for
//!   FAIL_OPEN_AFTER (§17: fail open, and an anchor freeing a bond).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use wirebond_core::{
    DOWN_AFTER, FAIL_OPEN_AFTER, IDLE_MISSES, LaneStatus, MAX_BONDS, Mode, Mono, PROBE_IDLE, Session, SessionStatus,
};

pub use crate::api::{BondState, Egress};

/// One bond: a client's to its anchor, or one of an anchor's.
pub struct Bond {
    /// The peer's device name, as hello gave it.
    pub peer: String,
    /// The peer's mesh address: the control channel's and lane 0's.
    pub peer_addr: IpAddr,
    /// The peer's hello inst: a new one ends this bond (§10.5 rule 7).
    pub inst: u64,
    pub mode: Mode,
    pub state: BondState,
    /// When `state` last changed: FAIL_OPEN_AFTER, LANES_BLOCKED_AFTER and MESH_ONLY_AFTER count
    /// from it.
    pub since: Mono,
    /// The client's bond addresses (`bond ok`).
    pub addr4: Option<Ipv4Addr>,
    pub addr6: Option<Ipv6Addr>,
    pub egress: Egress,
    /// The engine; None until the bond is open (`bond ok`).
    pub session: Option<Session>,
    /// The session's owner key in the RidTable.
    pub owner: u64,
    /// The control channel's id, while it is up.
    pub chan: Option<u64>,
    /// When the channel went down (FAIL_OPEN_AFTER); None while it is up.
    pub chan_down: Option<Mono>,
    /// Since when no lane is up (ALL_LANES_DOWN); None while one is.
    pub lanes_down: Option<Mono>,
    /// When the peer was last heard (`heard`), as of the lifecycle's last look; None if never.
    pub heard: Option<Mono>,
    /// A client's, while its channel is down: when it next has its lanes probed (`life`).
    pub probe_at: Mono,
    /// The session's lane counters over the last minute, for status's rates.
    pub rates: super::rates::Rates,
    /// Since when only the mesh lane is up (MESH_ONLY_AFTER); None otherwise.
    pub mesh_only: Option<Mono>,
    /// A client's: since when its lanes go through this one link of its own (MESH_ONLY_AFTER, then
    /// status notes it); None with none or several.
    pub one_link: Option<(Mono, u8)>,
    /// A lane validated since the bond opened (LANES_BLOCKED_AFTER).
    pub validated: bool,
    /// An anchor's: inner bytes forwarded toward the internet and delivered locally (§20.3).
    pub egressed: u64,
    pub local: u64,
    /// Inner packets dropped by the address checks (§6.3, §10.5 rule 6).
    pub dropped: u64,
    /// `pick`'s temporary bond (§18 step 1): no addresses, ended with its channel.
    pub measure: bool,
    /// A measurement in progress on this bond (§18), on either side.
    pub meter: Option<super::pick::Meter>,
    /// The peer's `links` as last offered: an anchor shows each lane in the client's group, the
    /// mesh lane in the group of the link carrying the mesh (§8).
    pub offer: Vec<super::proto::LinkOffer>,
}

impl Bond {
    pub fn new(peer: String, peer_addr: IpAddr, inst: u64, mode: Mode, now: Mono) -> Bond {
        Bond {
            peer,
            peer_addr,
            inst,
            mode,
            state: BondState::Connecting,
            since: now,
            addr4: None,
            addr6: None,
            egress: Egress::None,
            session: None,
            owner: 0,
            chan: None,
            chan_down: None,
            lanes_down: Some(now),
            heard: None,
            probe_at: 0,
            rates: super::rates::Rates::default(),
            mesh_only: None,
            one_link: None,
            validated: false,
            egressed: 0,
            local: 0,
            dropped: 0,
            measure: false,
            meter: None,
            offer: Vec::new(),
        }
    }

    /// Moves to `state` at `now`; the same state changes nothing.
    pub fn set_state(&mut self, state: BondState, now: Mono) {
        if self.state != state {
            (self.state, self.since) = (state, now);
        }
    }

    /// The address checks of this bond's inner packets, with nothing counted yet.
    pub fn check(&self) -> Check {
        Check { addr4: self.addr4, addr6: self.addr6, ..Check::default() }
    }
}

/// What §10.5 rule 6 checks a bond's inner packets against, the client's bond addresses, and what
/// the checks counted for the bond. The engine delivers through this, not the bond, so that it
/// works on the bond's session in place.
#[derive(Clone, Copy, Default)]
pub struct Check {
    addr4: Option<Ipv4Addr>,
    addr6: Option<Ipv6Addr>,
    pub egressed: u64,
    pub local: u64,
    pub dropped: u64,
}

impl Check {
    /// Whether `a` is one of the client's bond addresses.
    pub fn is_addr(&self, a: IpAddr) -> bool {
        match a {
            IpAddr::V4(a) => self.addr4 == Some(a),
            IpAddr::V6(a) => self.addr6 == Some(a),
        }
    }

    /// §10.5 rule 6 on a client: it delivers only packets addressed to its own bond address.
    pub fn client_accepts(&self, p: &[u8]) -> bool {
        addrs(p).is_some_and(|(_, dst)| self.is_addr(dst))
    }

    /// §6.3 and §10.5 rule 6 on an anchor: it forwards only packets from the client's bond
    /// address, and never to another bond address. `own` are the anchor's own bond addresses,
    /// `nets` the bond subnets. Some(true) delivers locally, Some(false) egresses, None drops.
    pub fn anchor_accepts(&self, p: &[u8], own: &[IpAddr], nets: &[(IpAddr, u8)]) -> Option<bool> {
        let (src, dst) = addrs(p)?;
        if !self.is_addr(src) {
            return None;
        }
        if own.contains(&dst) {
            return Some(true);
        }
        (!crate::host::in_ranges(dst, nets)).then_some(false)
    }
}

/// §17: whether lane `l` counts as down at `now`: the engine reports it down, it never validated,
/// or it is punched again and has had no echo for DOWN_AFTER. A suspect or dormant lane that
/// validated is not down.
///
/// The engine takes only a lane that was up to "down". A bond that idled has its lanes dormant,
/// and traffic that returns has them punched: with the anchor gone they stayed punching, so not
/// every lane was down, nothing redialed, and while the channel looked alive (a connection with
/// unacknowledged data is not probed by keepalive) capture stayed over a dead bond.
pub fn lane_down(l: &LaneStatus, now: Mono) -> bool {
    let silent = l.last_echo.is_none_or(|t| now - t >= DOWN_AFTER);
    !l.validated || l.state == "down" || l.state == "punching" && silent
}

/// The client's links that carry the bond: those of its validated direct lanes that are up,
/// suspect or dormant. The mesh lane rides one of the client's links, so it adds none.
pub fn client_links(snap: &SessionStatus) -> Vec<u8> {
    let through = |l: &&LaneStatus| l.id != 0 && l.validated && matches!(l.state, "up" | "suspect" | "dormant");
    let mut v: Vec<u8> = snap.lanes.iter().filter(through).map(|l| (l.id >> 8) as u8).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// What status notes when a client's bond rests on its one link `link` (§17), a line each: with
/// one link the bond has nothing to fail over to and no capacity to add. On macOS the usual cause
/// cannot be seen: the "iPhone USB" network service has "Disable unless needed" on by default,
/// which leaves the phone's interface without an address while Wi-Fi works. `phone` says the one
/// link is itself the phone, so that setting is not what is missing.
pub fn one_link_note(link: &str, macos: bool, phone: bool) -> Vec<String> {
    let mut v = vec![
        format!("the bond rests on one link, {link}: it has nothing to fail over to and no capacity to add"),
        "next: cable a phone or another network to this device".to_string(),
    ];
    if macos && !phone {
        v.push("an iPhone on USB that is not among the links: in System Settings > Network > iPhone USB,".into());
        v.push("turn off \"Disable unless needed\", which keeps the phone unused while Wi-Fi works".into());
    }
    v
}

/// When the peer was last heard, from what each role can observe. A client: the newest echo of
/// one of its probes on any lane, which shows both directions of a path. An anchor sends no probes
/// while a bond idles, so it has no echoes then: the newest packet of the client's it authenticated.
pub fn heard(snap: &SessionStatus, client: bool) -> Option<Mono> {
    match client {
        true => snap.lanes.iter().filter_map(|l| l.last_echo).max(),
        false => snap.last_auth,
    }
}

/// §17: whether a bond is lost at `now`: its channel has been down for FAIL_OPEN_AFTER and the
/// peer was not heard for as long as it can be silent while it is still there.
///
/// A client that knows its channel is down has a lane probed every PROBE_CHAN_DOWN, so
/// FAIL_OPEN_AFTER without an echo says the anchor is gone. An anchor can see the channel go
/// first (its own Tailscale restarting), while the client, which does not know yet, still probes
/// an idle bond once per PROBE_IDLE: the anchor waits IDLE_MISSES of those, as the client does
/// before it gives a lane up. Freed after FAIL_OPEN_AFTER, a live client's bond was gone, and
/// what it sent went nowhere until its own channel timed out.
pub fn lost(chan_down: Option<Mono>, heard: Option<Mono>, client: bool, now: Mono) -> bool {
    let quiet = if client { FAIL_OPEN_AFTER } else { IDLE_MISSES as Mono * PROBE_IDLE };
    let since = |t: Option<Mono>, d: Mono| t.is_none_or(|t| now - t >= d);
    chan_down.is_some() && since(chan_down, FAIL_OPEN_AFTER) && since(heard, quiet)
}

/// Whether lane `l` carries: validated, and up or suspect.
pub fn lane_carries(l: &LaneStatus) -> bool {
    l.validated && matches!(l.state, "up" | "suspect")
}

/// An inner IPv4 or IPv6 packet's source and destination; None if it is neither.
pub fn addrs(p: &[u8]) -> Option<(IpAddr, IpAddr)> {
    match p.first()? >> 4 {
        4 if p.len() >= 20 => {
            let a = |i: usize| IpAddr::from([p[i], p[i + 1], p[i + 2], p[i + 3]]);
            Some((a(12), a(16)))
        }
        6 if p.len() >= 40 => {
            let a = |i: usize| IpAddr::from(<[u8; 16]>::try_from(&p[i..i + 16]).unwrap_or([0; 16]));
            Some((a(8), a(24)))
        }
        _ => None,
    }
}

/// §5.5: the ICMPv6 destination unreachable (no route: type 1, code 0) that answers the outbound
/// IPv6 packet `p` when the anchor has no IPv6 egress, so applications fall back to IPv4 at once.
/// It comes from `p`'s destination and quotes as much of `p` as fits in 1280 bytes. None for
/// anything but IPv6, and for an ICMPv6 error (never answered with another).
pub fn icmp6_unreachable(p: &[u8]) -> Option<Vec<u8>> {
    if p.len() < 40 || p[0] >> 4 != 6 || (p[6] == 58 && p.get(40).is_some_and(|&t| t < 128)) {
        return None;
    }
    let quote = p.len().min(1280 - 48);
    let len = 8 + quote;
    let mut r = vec![0x60, 0, 0, 0];
    r.extend_from_slice(&(len as u16).to_be_bytes());
    r.extend_from_slice(&[58, 64]);
    r.extend_from_slice(&p[24..40]);
    r.extend_from_slice(&p[8..24]);
    r.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    r.extend_from_slice(&p[..quote]);
    let ck = !fold(&[&r[8..40], &(len as u32).to_be_bytes(), &[0, 0, 0, 58], &r[40..]]);
    r[42..44].copy_from_slice(&ck.to_be_bytes());
    Some(r)
}

/// The ones'-complement sum of `parts`, folded to 16 bits (RFC 1071).
fn fold(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    for p in parts {
        for c in p.chunks(2) {
            sum += u32::from(u16::from_be_bytes([c[0], c.get(1).copied().unwrap_or(0)]));
        }
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

/// An anchor's bond addresses (§2, §3.2): host numbers 2–254 of the bond subnets, kept per device
/// for as long as it stays on the allow-list, so a reconnect keeps them.
#[derive(Clone, Debug, Default)]
pub struct Pool {
    by_device: BTreeMap<String, u8>,
}

impl Pool {
    /// The host number of `device`, assigning the lowest free one; None when none is left.
    pub fn assign(&mut self, device: &str) -> Option<u8> {
        if let Some(&n) = self.by_device.get(device) {
            return Some(n);
        }
        let n = (2..=254).find(|n| !self.by_device.values().any(|x| x == n))?;
        self.by_device.insert(device.to_string(), n);
        Some(n)
    }

    /// Forgets the devices no longer on `allow`.
    pub fn keep(&mut self, allow: &[String]) {
        self.by_device.retain(|d, _| allow.contains(d));
    }
}

/// Whether an anchor may take one more bond.
pub fn room(bonds: usize) -> bool {
    bonds < MAX_BONDS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_keeps_a_devices_address_and_reuses_freed_ones() {
        let mut p = Pool::default();
        assert_eq!((p.assign("mac"), p.assign("pc"), p.assign("mac")), (Some(2), Some(3), Some(2)));
        p.keep(&["pc".to_string()]);
        assert_eq!(p.assign("laptop"), Some(2));
        assert!(room(MAX_BONDS - 1) && !room(MAX_BONDS));
    }

    /// An IPv4 header from `src` to `dst`.
    fn v4(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
        let mut p = vec![0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0];
        p.extend_from_slice(&src);
        p.extend_from_slice(&dst);
        p
    }

    #[test]
    fn a_lane_is_down_by_the_engines_word_or_punched_in_vain() {
        let (t, heard) = (100 * wirebond_core::SEC, Some(99 * wirebond_core::SEC));
        let lane = |state: &'static str, validated, last_echo| LaneStatus {
            state,
            validated,
            last_echo,
            ..LaneStatus::default()
        };
        assert!(lane_down(&lane("down", true, heard), t) && lane_down(&lane("punching", false, heard), t));
        for s in ["up", "suspect", "dormant", "punching"] {
            assert!(!lane_down(&lane(s, true, heard), t), "{s} is not down");
        }
        // Punched again with no echo for DOWN_AFTER: down. Dormant, it was not asked.
        for (s, down) in [("punching", true), ("dormant", false), ("up", false), ("suspect", false)] {
            assert_eq!(lane_down(&lane(s, true, Some(t - DOWN_AFTER)), t), down, "{s}");
            assert_eq!(lane_down(&lane(s, true, None), t), down, "{s}, never echoed");
        }
        assert!(lane_carries(&lane("suspect", true, heard)) && !lane_carries(&lane("dormant", true, heard)));
    }

    /// §17: a bond is lost by the channel and the echoes, whatever the lanes' states: the lanes
    /// of an idle bond stay dormant, and its idle-probe lane up, long after the anchor has gone.
    #[test]
    fn a_bond_is_lost_by_its_channel_and_what_was_heard_not_by_lane_states() {
        let (s, t) = (wirebond_core::SEC, 100 * wirebond_core::SEC);
        assert!(lost(Some(t - 5 * s), Some(t - 5 * s), true, t) && lost(Some(t - 5 * s), None, true, t));
        assert!(!lost(None, Some(t - 60 * s), true, t), "the channel is up");
        assert!(!lost(Some(t - 4 * s), Some(t - 60 * s), true, t), "the channel went down under 5 s ago");
        assert!(!lost(Some(t - 60 * s), Some(t - 4 * s), true, t), "an echo under 5 s ago");
        // An anchor hears an idle client once per PROBE_IDLE, and waits IDLE_MISSES of those.
        assert!(!lost(Some(t - 60 * s), Some(t - 74 * s), false, t), "an idle client may still be there");
        assert!(lost(Some(t - 5 * s), Some(t - 75 * s), false, t) && lost(Some(t - 5 * s), None, false, t));
        assert!(!lost(Some(t - 4 * s), Some(t - 90 * s), false, t) && !lost(None, None, false, t));
        let lane = |state, last_echo| LaneStatus { state, validated: true, last_echo, ..LaneStatus::default() };
        let lanes = vec![lane("dormant", Some(t - 30 * s)), lane("up", Some(t - 7 * s)), lane("punching", None)];
        let snap = SessionStatus { lanes, last_auth: Some(t - 6 * s), ..SessionStatus::default() };
        assert!(!snap.lanes.iter().all(|l| lane_down(l, t)), "by their states these lanes are not down");
        assert_eq!((heard(&snap, true), heard(&snap, false)), (Some(t - 7 * s), Some(t - 6 * s)));
        assert!(lost(Some(t - 5 * s), heard(&snap, true), true, t));
        assert!(!lost(Some(t - 5 * s), heard(&snap, false), false, t));
    }

    /// §17: the links a bond rests on are the client's links under its live direct lanes; the
    /// mesh lane and a lane still punching add none. The note names the link, and the macOS
    /// setting only on macOS, and not when the one link is the phone itself.
    #[test]
    fn a_bond_on_one_link_is_noted_with_what_to_switch_off_on_macos() {
        let lane = |id, state| LaneStatus { id, state, validated: true, ..LaneStatus::default() };
        let snap = |lanes| SessionStatus { lanes, ..SessionStatus::default() };
        let one = snap(vec![lane(0, "up"), lane(0x0101, "up"), lane(0x0102, "dormant"), lane(0x0201, "punching")]);
        assert_eq!(client_links(&one), [1], "two lanes through link 1; link 2's never came up");
        let two = snap(vec![lane(0, "up"), lane(0x0101, "suspect"), lane(0x0201, "up"), lane(0x0301, "down")]);
        assert_eq!(client_links(&two), [1, 2]);
        assert!(client_links(&snap(vec![lane(0, "up")])).is_empty(), "the mesh lane alone: mesh_only's case");
        let unvalidated = LaneStatus { validated: false, ..lane(0x0101, "up") };
        assert!(client_links(&snap(vec![unvalidated])).is_empty());
        let note = one_link_note("Wi-Fi", true, false);
        assert!(note[0].starts_with("the bond rests on one link, Wi-Fi: ") && note.len() == 4, "{note:?}");
        assert!(note[2].contains("iPhone USB") && note[3].contains("\"Disable unless needed\""), "{note:?}");
        for (macos, phone) in [(false, false), (true, true)] {
            let note = one_link_note("iPhone", macos, phone);
            assert!(note.len() == 2 && !note.concat().contains("Disable unless needed"), "{note:?}");
        }
        assert!(note.iter().all(|l| l.len() <= 92), "each line fits 100 columns behind its indent");
    }

    #[test]
    fn inner_packets_are_checked() {
        let mut b = Bond::new("mac".into(), "100.64.0.2".parse().unwrap(), 1, Mode::Performance, 0);
        b.addr4 = Some(Ipv4Addr::new(10, 77, 0, 2));
        b.addr6 = Some("fd77:b0d::2".parse().unwrap());
        let b = b.check();
        assert!(b.client_accepts(&v4([10, 77, 0, 1], [10, 77, 0, 2])));
        assert!(!b.client_accepts(&v4([10, 77, 0, 1], [10, 77, 0, 3])), "not this client's address");
        assert!(!b.client_accepts(&[0x45, 0, 0]), "not a packet");
        let own = ["10.77.0.1".parse().unwrap(), "fd77:b0d::1".parse().unwrap()];
        let nets = [("10.77.0.0".parse().unwrap(), 24), ("fd77:b0d::".parse().unwrap(), 64)];
        assert_eq!(b.anchor_accepts(&v4([10, 77, 0, 2], [10, 77, 0, 1]), &own, &nets), Some(true), "local");
        assert_eq!(b.anchor_accepts(&v4([10, 77, 0, 2], [1, 1, 1, 1]), &own, &nets), Some(false), "egress");
        assert_eq!(b.anchor_accepts(&v4([10, 77, 0, 9], [1, 1, 1, 1]), &own, &nets), None, "forged source");
        assert_eq!(b.anchor_accepts(&v4([10, 77, 0, 2], [10, 77, 0, 3]), &own, &nets), None, "bond to bond");
        let mut p6 = vec![0x60, 0, 0, 0, 0, 0, 17, 64];
        p6.extend_from_slice(&"fd77:b0d::2".parse::<Ipv6Addr>().unwrap().octets());
        p6.extend_from_slice(&"2606:4700::1111".parse::<Ipv6Addr>().unwrap().octets());
        assert_eq!(b.anchor_accepts(&p6, &own, &nets), Some(false), "IPv6 egress");
    }

    #[test]
    fn ipv6_without_egress_is_answered_unreachable() {
        let (src, dst): (Ipv6Addr, Ipv6Addr) = ("fd77:b0d::2".parse().unwrap(), "2606:4700::1111".parse().unwrap());
        let mut p = vec![0x60, 0, 0, 0, 0, 12, 17, 64];
        p.extend_from_slice(&src.octets());
        p.extend_from_slice(&dst.octets());
        p.extend_from_slice(&[0x13, 0x88, 0x01, 0xbb, 0, 12, 0, 0, 1, 2, 3, 4]);
        let r = icmp6_unreachable(&p).unwrap();
        assert_eq!(addrs(&r), Some((IpAddr::V6(dst), IpAddr::V6(src))), "from the destination, to the sender");
        assert_eq!((r[6], r[40], r[41]), (58, 1, 0), "ICMPv6 destination unreachable, no route");
        assert_eq!(usize::from(u16::from_be_bytes([r[4], r[5]])), r.len() - 40);
        assert_eq!(&r[48..], &p[..], "quotes the packet");
        let len = (r.len() - 40) as u32;
        assert_eq!(fold(&[&r[8..40], &len.to_be_bytes(), &[0, 0, 0, 58], &r[40..]]), 0xffff, "checksum");
        assert_eq!(icmp6_unreachable(&r), None, "an error is never answered");
        assert_eq!(icmp6_unreachable(&[0x45; 60]), None, "IPv4");
    }
}
