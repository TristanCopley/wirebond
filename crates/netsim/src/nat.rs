//! The NAT in front of a link (Go netsim/nat.go): one-to-one, easy and hard; mapping timeouts
//! drawn from 10–200 s; port or address rebinding [Testing 8]. Methods that need the clock take
//! `now` (Go reached it through a World pointer). Owner: N1.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::sim::{Dur, Rng, SEC};

/// The behaviour of the NAT in front of a link [Testing 8].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NatKind {
    /// Public address, port kept, any source let in: a public interface or a router forward.
    #[default]
    OneToOne,
    /// One mapping per local socket; let in only from addresses it sent to.
    Easy,
    /// One mapping per local socket and destination; let in only from that destination.
    Hard,
}

/// Translates one link's packets. Mappings expire after `timeout` without an outgoing packet.
#[derive(Clone, Debug)]
pub struct Nat {
    pub kind: NatKind,
    /// The public address (Go Pub).
    pub public: Ipv4Addr,
    pub timeout: Dur,
    /// The link's local address, which one-to-one passes inbound packets to.
    pub(crate) local: IpAddr,
    pub(crate) r: Rng,
    /// Live mappings by key (Go out); they own the mapping.
    pub(crate) out: BTreeMap<NatKey, Mapping>,
    /// Public port → key (Go in).
    pub(crate) inb: BTreeMap<u16, NatKey>,
    /// Keys in creation order, so a rebind draws ports deterministically (Go list).
    pub(crate) list: Vec<NatKey>,
}

/// A mapping's key: the local socket, and for a hard NAT the destination (Go natKey).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NatKey {
    pub(crate) local: SocketAddr,
    pub(crate) dst: Option<SocketAddr>,
}

/// One mapping (Go mapping).
#[derive(Clone, Debug)]
pub(crate) struct Mapping {
    pub(crate) key: NatKey,
    pub(crate) port: u16,
    /// Addresses it sent to: an easy NAT lets these in.
    pub(crate) peers: BTreeSet<SocketAddr>,
    /// The last outgoing packet.
    pub(crate) last: Dur,
}

impl Nat {
    /// A NAT of kind in front of local with public address public, drawing its timeout
    /// (10–200 s [Testing 6]) and ports from r (Go newNAT, which drew r from the world).
    pub fn new(kind: NatKind, local: IpAddr, public: Ipv4Addr, mut r: Rng) -> Nat {
        let timeout = 10 * SEC + r.dur(190 * SEC);
        Nat { kind, public, timeout, local, r, out: BTreeMap::new(), inb: BTreeMap::new(), list: Vec::new() }
    }

    /// Whether the mapping under key is live at now; an expired one is removed (Go live).
    pub(crate) fn live(&mut self, now: Dur, key: Option<NatKey>) -> bool {
        let Some(m) = key.and_then(|k| self.out.get(&k)) else {
            return false;
        };
        if now - m.last > self.timeout {
            let (k, port) = (m.key, m.port);
            self.out.remove(&k);
            self.inb.remove(&port);
            return false;
        }
        true
    }

    /// A free public port in 1024–65534 (Go port).
    pub(crate) fn port(&mut self) -> u16 {
        loop {
            let p = 1024 + self.r.below(64511) as u16;
            if !self.inb.contains_key(&p) {
                return p;
            }
        }
    }

    /// The public source of a packet from local to dst, making or refreshing its mapping (Go Out).
    pub fn out(&mut self, now: Dur, local: SocketAddr, dst: SocketAddr) -> SocketAddr {
        if self.kind == NatKind::OneToOne {
            return SocketAddr::new(IpAddr::V4(self.public), local.port());
        }
        let key = NatKey { local, dst: (self.kind == NatKind::Hard).then_some(dst) };
        if !self.live(now, Some(key)) {
            let port = self.port();
            let m = Mapping { key, port, peers: BTreeSet::new(), last: now };
            self.out.insert(key, m);
            self.inb.insert(port, key);
            // Go kept the expired mapping in its list until the next rebind skipped it; a key
            // stands for its newest mapping here, so the old entry goes now: the order of live
            // mappings, which a rebind draws ports in, is the same.
            self.list.retain(|k| *k != key);
            self.list.push(key);
        }
        let m = self.out.get_mut(&key).expect("a live mapping");
        m.last = now;
        m.peers.insert(dst);
        SocketAddr::new(IpAddr::V4(self.public), m.port)
    }

    /// The local address a packet from src to the public port reaches, if the NAT lets it in
    /// (Go In; `in` is a Rust keyword).
    pub fn inbound(&mut self, now: Dur, src: SocketAddr, port: u16) -> Option<SocketAddr> {
        if self.kind == NatKind::OneToOne {
            return Some(SocketAddr::new(self.local, port));
        }
        let key = *self.inb.get(&port)?;
        if !self.live(now, Some(key)) {
            return None;
        }
        let m = &self.out[&key];
        m.peers.contains(&src).then_some(m.key.local)
    }

    /// Gives every live mapping a new public port, and with addr a new public address too (the
    /// last octet + 1), as a NAT that restarts or a carrier that moves the device does (Go Rebind).
    pub fn rebind(&mut self, addr: bool) {
        if addr {
            let mut b = self.public.octets();
            b[3] = b[3].wrapping_add(1);
            self.public = Ipv4Addr::from(b);
        }
        if self.kind == NatKind::OneToOne {
            return;
        }
        self.inb.clear();
        let mut list = std::mem::take(&mut self.list);
        list.retain(|k| self.out.contains_key(k));
        for k in &list {
            let port = self.port();
            self.out.get_mut(k).expect("kept above").port = port;
            self.inb.insert(port, *k);
        }
        self.list = list;
    }

    /// Moves the NAT's local address (Go set n.local in Link.SetLocal).
    pub(crate) fn set_local(&mut self, a: IpAddr) {
        self.local = a;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{PUB_CELL, PUB_HOME, PUB_HOTEL};
    use crate::fake::{Evil, fake_maker};
    use crate::sim::{World, fmt_dur};

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Go TestNAT: easy maps once and admits only destinations; hard maps per destination;
    /// one-to-one keeps the port and admits anyone; timeouts 10–200 s; rebinding drops the old
    /// mapping and moves the address. The NAT takes the clock as `now`, where Go ran the world.
    #[test]
    fn nat() {
        let mut w = World::new(1, fake_maker(false, Evil::None));
        let local = addr("10.0.0.2:7849");
        let (a, b) = (addr("192.0.2.1:7849"), addr("192.0.2.2:7849"));
        let mut errs: Vec<String> = Vec::new();
        let mut easy = Nat::new(NatKind::Easy, local.ip(), PUB_HOTEL, w.sub());
        let (pa, pb) = (easy.out(0, local, a), easy.out(0, local, b));
        if pa != pb || pa.ip() != IpAddr::V4(PUB_HOTEL) {
            errs.push(format!("easy NAT mapped {pa} and {pb}; want one mapping on {PUB_HOTEL}"));
        }
        if easy.inbound(0, a, pa.port()) != Some(local) {
            errs.push("easy NAT refused a reply from a destination".into());
        }
        if easy.inbound(0, addr("192.0.2.3:1"), pa.port()).is_some() {
            errs.push("easy NAT let in a stranger".into());
        }
        let mut hard = Nat::new(NatKind::Hard, local.ip(), PUB_CELL, w.sub());
        let (ha, hb) = (hard.out(0, local, a), hard.out(0, local, b));
        if ha == hb {
            errs.push("hard NAT used one mapping for two destinations".into());
        } else if hard.inbound(0, b, ha.port()).is_some() {
            errs.push("hard NAT let b in through a's mapping".into());
        }
        let mut one = Nat::new(NatKind::OneToOne, local.ip(), PUB_HOME, w.sub());
        let p = one.out(0, local, a);
        if p != SocketAddr::new(IpAddr::V4(PUB_HOME), 7849) {
            errs.push(format!("one-to-one mapped {p}"));
        }
        if one.inbound(0, addr("198.51.100.1:5"), 7849) != Some(local) {
            errs.push("one-to-one refused a stranger".into());
        }
        let timeout = easy.timeout;
        if !(10 * SEC..=200 * SEC).contains(&timeout) {
            errs.push(format!("timeout {} outside 10–200 s", fmt_dur(timeout)));
        }
        if easy.inbound(timeout + SEC, a, pa.port()).is_some() {
            errs.push(format!("easy NAT kept a mapping past its {} timeout", fmt_dur(timeout)));
        }
        let now = timeout + 2 * SEC;
        let pa = easy.out(now, local, a);
        easy.rebind(false);
        if easy.inbound(now, a, pa.port()).is_some() || easy.out(now, local, a) == pa {
            errs.push("a port rebinding kept the old mapping".into());
        }
        easy.rebind(true);
        if easy.public == PUB_HOTEL {
            errs.push("an address rebinding kept the address".into());
        }
        assert!(errs.is_empty(), "{}", errs.join("\n"));
    }

    /// Not in Go: a mapping made again after it expired is one mapping, which a rebind moves once,
    /// keeping the port table in step; outgoing packets keep a mapping alive; an address rebind
    /// moves a one-to-one NAT's address (wrapping the last octet) and keeps its ports.
    #[test]
    fn expiry_and_rebind() {
        let local = addr("10.0.0.2:7849");
        let (a, b) = (addr("192.0.2.1:7849"), addr("192.0.2.2:7849"));
        let mut n = Nat::new(NatKind::Hard, local.ip(), PUB_CELL, Rng::new(1, 2));
        let t = n.timeout;
        n.out(0, local, a);
        n.out(0, local, b);
        // b's mapping is refreshed at t; a's expires and is made again after b's.
        n.out(t, local, b);
        let pb = n.out(t + 2 * SEC, local, b);
        let pa = n.out(t + 2 * SEC, local, a);
        assert_eq!(n.inbound(t + 2 * SEC, b, pb.port()), Some(local));
        let key = |dst| NatKey { local, dst: Some(dst) };
        assert_eq!(n.list, [key(b), key(a)]);
        assert_eq!((n.out.len(), n.inb.len()), (2, 2));
        n.rebind(false);
        assert_eq!((n.list.len(), n.out.len(), n.inb.len()), (2, 2, 2));
        for (port, k) in &n.inb {
            assert_eq!(n.out[k].port, *port);
        }
        let qa = n.out(t + 2 * SEC, local, a);
        assert_ne!(qa, pa);
        assert_eq!(n.inbound(t + 2 * SEC, a, qa.port()), Some(local));

        let edge = Ipv4Addr::new(192, 0, 2, 255);
        let mut one = Nat::new(NatKind::OneToOne, local.ip(), edge, Rng::new(3, 4));
        one.rebind(true);
        assert_eq!(one.out(0, local, a), addr("192.0.2.0:7849"));
    }
}
