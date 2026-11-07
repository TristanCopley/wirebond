//! The node stand-in (Go netsim/node.go, §17.2): per device its links, sockets, clock and
//! Session, and what node does around core: hello, the control channel, STUN on node's triggers,
//! flows and FlowEnd, wakes and restarts; plus the routing of packets through links and NATs.
//!
//! Go's Device methods are `World` methods taking a [`DevId`] (they all reach the world: the
//! clock, the queue, the links, the peer). How netsim drives core is in notes/netsim.md ("How
//! netsim drives core"); the Rust differences are in SKELETON.md and notes/rust.md. Owner: N2.

use std::any::Any;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::{Index, IndexMut};

use crate::sim::{FlowId, REASM_MAX, fragment, inner, is_anchor, is_client, is_fragment, outer, path};

/// 1.x's daemon timings, which netsim's simulated daemon keeps until its 2.0 port (wave 2): the
/// mesh's MTU, the redial while a connection waits, and how long a session outlives its channel.
const MESH_MTU: usize = 1280;
const REDIAL_WAITING: Mono = wirebond_core::SEC;
const SESSION_LINGER: Mono = 2 * wirebond_core::MIN;

use wirebond_core::{
    Event, FLOW_IDLE, IDLE_AFTER, LINK_SCAN, LinkInfo, MAX_CANDS, MAX_FLOWS, MAX_LINKS, MAX_MSG, MODE_PERFORMANCE,
    MeshLane, Mono, Msg, PORT, PROTOCOL, RidTable, Role, STUN_EVERY, STUN_WAIT, Session, SessionStatus, SockId,
    packet_rid,
};

use crate::check::{Adv, PathKey};
use crate::link::{
    Dir, IP_UDP, Link, LinkId, LinkSpec, Profile, SPIKE_PEAK, Side, TS_OVER, Then, awdl_train, frags, short_train,
    spike_train, starlink_train,
};
use crate::nat::{Nat, NatKind};
use crate::sim::{
    Action, Actions, Dur, Endpoint, HOUR, HookArgs, HookKind, MS, Rng, SEC, Task, World, event_code, fmt_dur,
    key_reader,
};

/// The Tailscale-lane socket on both devices.
pub const MESH_SOCK: SockId = 100;

/// The modelled STUN server (stun.cloudflare.com:3478).
pub const STUN_SERVER: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(162, 159, 207, 0), 3478));

/// 255.255.255.255, which node never passes as a candidate.
pub(crate) const BCAST: Ipv4Addr = Ipv4Addr::BROADCAST;

/// The release version the stand-in's hello carries (Go's literal).
const VERSION: &str = "0.3.0";

/// Whether a is in Tailscale's ranges, 100.64.0.0/10 or fd7a:115c:a1e0::/48 (Go meshV4, meshV6).
/// An IPv4-mapped IPv6 address counts as its IPv4 address.
pub(crate) fn is_mesh(a: IpAddr) -> bool {
    match a.to_canonical() {
        IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            a == 100 && (b & 0xc0) == 64
        }
        IpAddr::V6(v) => v.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// A device: index into the world's two (Go *Device; Go's peer pointer is [`DevId::peer`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DevId {
    /// The Mac.
    Client = 0,
    /// xanth.
    Server = 1,
}

impl DevId {
    pub const BOTH: [DevId; 2] = [DevId::Client, DevId::Server];

    pub fn peer(self) -> DevId {
        match self {
            DevId::Client => DevId::Server,
            DevId::Server => DevId::Client,
        }
    }

    pub fn idx(self) -> usize {
        self as usize
    }

    pub fn role(self) -> Role {
        match self {
            DevId::Client => Role::Client,
            DevId::Server => Role::Anchor,
        }
    }
}

/// One datagram in flight (Go pkt).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pkt {
    pub data: Vec<u8>,
    pub stun: bool,
    /// A STUN reply: the source the server saw.
    pub mapped: Option<SocketAddr>,
    /// Genuine, or what the adversary did to it.
    pub adv: crate::check::Adv,
    /// The device that sent it; None for STUN replies and adversary packets.
    pub from: Option<DevId>,
    /// The socket it left.
    pub sock: SockId,
    /// The address it was sent to; None with `from`.
    pub to: Option<SocketAddr>,
    /// When it was sent.
    pub t0: Dur,
}

impl Pkt {
    /// A bare packet carrying data (the model tests' &pkt{data}).
    pub fn new(data: Vec<u8>) -> Pkt {
        Pkt { data, ..Pkt::default() }
    }
}

/// The node stand-in on one device (§17.2). Exported Go fields are `pub`; unexported ones are
/// `pub(crate)`, except the few Go's same-package tests touched, which are `pub` and say so.
pub struct Device {
    pub name: &'static str,
    pub role: Role,
    /// The client's connection mode.
    pub mode: String,
    /// The server's shared ports.
    pub shared: Vec<u16>,
    /// The device's links in id order: `links[i]` has id i + 1.
    pub links: Vec<LinkId>,
    /// The Tailscale address, on PORT.
    pub mesh: SocketAddr,
    pub inst: u64,
    /// Sessions made so far.
    pub sessions: u32,
    /// Key messages this device's Sessions sent, in order.
    pub keys: Vec<Msg>,
    /// Datagrams dropped for MAX_FLOWS or no flow.
    pub dropped: u64,
    /// Wire bytes of every lane packet the Sessions sent (IP and UDP headers included).
    pub sent: i64,
    // Go's Event field (the event being handled) is HookArgs::event.
    pub(crate) sess: Option<Box<dyn Endpoint>>,
    /// The Snapshot after the previous handle call: the checker's "before" (Go snap).
    pub(crate) last_snap: SessionStatus,
    /// pub for the self-tests (TestCheckerCatches sets it).
    pub peer_inst: u64,
    /// The current pairing in [`World::pair`]; pub for the self-tests.
    pub pair_id: u16,
    pub(crate) rids: RidTable,
    /// The owner key the current Session was made with (new in Rust: core's RidTable names
    /// sessions by node's key). The device uses `sessions` at creation.
    pub(crate) owner: u64,
    /// The device's monotonic clock is world time + off.
    pub(crate) off: Dur,
    pub(crate) asleep: bool,
    pub(crate) down: bool,
    /// Generation of the pending Tick.
    pub(crate) tick: u64,
    pub(crate) tick_at: Dur,
    pub(crate) tick_set: bool,
    pub(crate) last_tick: Dur,
    /// Ticks in a row at one time: more than 100,000 fails the run.
    pub(crate) same: u32,
    /// The action recorder handed to handle, reused (Go out).
    pub(crate) out: Actions,
    /// Live flows (Go map; sorted for the FlowEnd sweep).
    pub(crate) flows: BTreeMap<FlowId, Flow>,
    /// Client: port << 16 | the program's source port → its flow.
    pub(crate) by_app: BTreeMap<u32, FlowId>,
    /// The client's last flow number.
    pub(crate) num: u16,
    pub(crate) last_data: Dur,
    pub(crate) last_stun: Dur,
}

/// One live flow (Go flow).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flow {
    pub(crate) last: Dur,
    /// Client: its by_app key.
    pub(crate) key: u32,
}

impl Device {
    /// A device with no links and no Session; draws its inst and clock offset from rng (Go
    /// w.device).
    pub(crate) fn new(name: &'static str, role: Role, mesh: SocketAddr, rng: &mut Rng) -> Device {
        let inst = rng.u64() | 1;
        let off = 1_000_000_000_000_000 + rng.dur(1_000_000_000_000_000);
        Device {
            name,
            role,
            mode: MODE_PERFORMANCE.into(),
            shared: Vec::new(),
            links: Vec::new(),
            mesh,
            inst,
            sessions: 0,
            keys: Vec::new(),
            dropped: 0,
            sent: 0,
            sess: None,
            last_snap: SessionStatus::default(),
            peer_inst: 0,
            pair_id: 0,
            rids: RidTable::new(),
            owner: 0,
            off,
            asleep: false,
            down: false,
            tick: 0,
            tick_at: 0,
            tick_set: false,
            last_tick: 0,
            same: 0,
            out: Actions::default(),
            flows: BTreeMap::new(),
            by_app: BTreeMap::new(),
            num: 0,
            last_data: -HOUR,
            last_stun: 0,
        }
    }

    /// Whether the device runs: not asleep and not restarting (Go Awake).
    pub fn awake(&self) -> bool {
        !self.asleep && !self.down
    }

    /// The current Session's Snapshot, or a zero one (Go Snap).
    pub fn snap(&self) -> SessionStatus {
        self.sess.as_ref().map(|s| s.snapshot()).unwrap_or_default()
    }

    /// The current endpoint, for tests that downcast to the fake.
    pub fn endpoint(&self) -> Option<&dyn Endpoint> {
        self.sess.as_deref()
    }

    /// The device's clock offset: its monotonic clock is world time + off (Go's d.off, which
    /// TestSleepWake reads).
    pub fn off(&self) -> Dur {
        self.off
    }

    /// The device's byte in the event log (Go id: the role, Client 1, Server 2).
    pub(crate) fn code(&self) -> u8 {
        match self.role {
            Role::Client => 1,
            Role::Anchor => 2,
        }
    }

    /// Invalidates the pending Tick (Go cancelTick).
    pub(crate) fn cancel_tick(&mut self) {
        self.tick += 1;
        self.tick_set = false;
    }

    /// The client's next flow on port: numbers from 1 per session, wrapping after 65,535, never 0
    /// or a live number (§5.1; node's sess.number). The caller checked that a flow is free.
    fn number(&mut self, port: u16) -> FlowId {
        loop {
            self.num = self.num.wrapping_add(1);
            let f = FlowId::new(port, self.num);
            if self.num != 0 && !self.flows.contains_key(&f) {
                return f;
            }
        }
    }
}

impl Index<DevId> for World {
    type Output = Device;
    fn index(&self, d: DevId) -> &Device {
        &self.devs[d.idx()]
    }
}

impl IndexMut<DevId> for World {
    fn index_mut(&mut self, d: DevId) -> &mut Device {
        &mut self.devs[d.idx()]
    }
}

/// The control channel: TCP over Tailscale, reliable and ordered, with the mesh's delay. A
/// scenario can cut it; every connect sends hello both ways (Go ctlChan).
#[derive(Clone, Debug, Default)]
pub struct CtlChan {
    pub up: bool,
    pub cut: bool,
    /// When it last went down.
    pub(crate) since: Dur,
    /// Bumped on every connect and drop: messages of an older generation are dropped (Go gen).
    pub(crate) generation: u64,
    /// The last delivery time per direction (0: client → server), for ordering.
    pub(crate) last: [Dur; 2],
    pub(crate) dialing: bool,
}

impl CtlChan {
    /// When a message sent in direction dir that could arrive at earliest arrives: never before
    /// the one sent before it, since TCP delivers in order (Go's max(c.last[dir], …)).
    fn slot(&mut self, dir: usize, earliest: Dur) -> Dur {
        let at = self.last[dir].max(earliest);
        self.last[dir] = at;
        at
    }
}

impl World {
    /// The device's monotonic clock (Go Device.Mono).
    pub fn mono(&self, d: DevId) -> Mono {
        self.now + self[d].off
    }

    /// Gives d a link (Go AddLink): id = its position + 1, socket = id, a NAT drawing from a
    /// sub-Rng, both directions, and the radio trains the spec asks for (spikes, AWDL, Starlink),
    /// in that order of draws.
    pub fn add_link(&mut self, d: DevId, s: LinkSpec) -> LinkId {
        let id = u8::try_from(self[d].links.len() + 1).expect("a device has at most 255 links");
        let nat = Nat::new(s.nat, s.local.ip(), s.public, self.sub());
        let up = Dir::new(s.up.clone(), self.sub());
        let down = Dir::new(s.down.clone(), self.sub());
        let mut r = self.sub();
        let mut bumps = Vec::new();
        if s.spikes {
            bumps.push(spike_train(r.split(), SPIKE_PEAK));
        }
        if s.short_spikes {
            bumps.push(short_train(r.split()));
        }
        if s.awdl {
            bumps.push(awdl_train(r.split()));
        }
        if s.starlink > 0 {
            bumps.push(starlink_train(s.starlink));
        }
        let l = LinkId(self.links.len());
        self.links.push(Link {
            id,
            dev: d,
            sock: SockId::from(id),
            local: s.local,
            mapped: None,
            nat,
            up,
            down,
            bumps,
            used: -HOUR,
            stun_at: None,
            spec: s,
        });
        self[d].links.push(l);
        l
    }

    /// Brings both devices up: STUN on every link, housekeeping every second, the client dialling
    /// the control channel (Go Start).
    pub fn start(&mut self) {
        for d in DevId::BOTH {
            self.stun_all(d);
            self.every(SEC, SEC, move |w| {
                w.house(d);
                true
            });
        }
        self.ctl_dial();
    }

    /// The link of d with socket s (Go d.link).
    pub fn link_of(&self, d: DevId, s: SockId) -> Option<LinkId> {
        self[d].links.iter().copied().find(|&l| self[l].sock == s)
    }

    /// The default-route link of d (Go defaultLink); once it is pulled out ([`World::pull`]), the
    /// first of d's links still in.
    pub fn default_link(&self, d: DevId) -> Option<LinkId> {
        let links = &self[d].links;
        match links.iter().copied().find(|&l| self[l].spec.default) {
            Some(l) if self.pulled.contains(&l) => links.iter().copied().find(|l| !self.pulled.contains(l)),
            found => found,
        }
    }

    /// Whether node marks l as d's default link in Links, hello and links: only a client marks
    /// its default-route link (§6.1).
    fn node_default(&self, d: DevId, l: LinkId) -> bool {
        self[d].role == Role::Client && self[l].spec.default
    }

    /// Passes one event to d's Session, checks the invariants (when `check`) and carries out the
    /// actions; then schedules the Tick (Go handle). p is the packet behind an `Event::Packet`.
    /// Nothing happens without a Session, while asleep or down, or after a failure.
    pub(crate) fn handle(&mut self, d: DevId, ev: Event<'_>, p: Option<&Pkt>) {
        if self.fail.is_some() || self[d].sess.is_none() || !self[d].awake() {
            return;
        }
        self.calls += 1;
        let (a, data) = log_args(&ev);
        let code = self[d].code();
        self.logf(event_code(&ev), code, a, 0, data);
        let now = self.mono(d);
        let dev = &mut self.devs[d.idx()];
        if let Some(s) = dev.sess.as_mut() {
            dev.out.0.clear();
            s.handle(now, ev, &mut dev.rids, &mut dev.out);
        }
        // The buffer is taken out while the actions run, then put back emptied: no allocation in
        // steady state. Nothing below re-enters this device's handle (sends wait while busy).
        let mut acts = std::mem::take(&mut dev.out);
        if self.check {
            self.check_after(d, &ev, p, &acts.0);
        }
        self.busy = true;
        for a in acts.0.drain(..) {
            self.act(d, a, &ev);
        }
        self.busy = false;
        self[d].out = acts;
        self.sched_tick(d);
    }

    /// Carries out one action of d's Session; ev is the event that produced it (Go act):
    /// Send → log, count, hook Sent, route; Deliver → log, deliver; Control → record keys, send.
    pub(crate) fn act(&mut self, d: DevId, a: Action, ev: &Event<'_>) {
        let code = self[d].code();
        match a {
            Action::Send { sock, to, data } => {
                self.logf(b'S', code, u64::from(sock), 0, &data);
                self[d].sent += (data.len() + IP_UDP) as i64;
                if !self.hooks.is_empty() {
                    let link = self.link_of(d, sock);
                    self.fire(&HookArgs { kind: HookKind::Sent, dev: d, link, data: &data, event: Some(*ev) });
                }
                self.route(d, sock, to, data, false);
            }
            Action::Deliver { flow, src, dst, data } => {
                self.logf(b'D', code, u64::from(flow.0), 0, &data);
                self.deliver_ip(d, flow, src, dst, data);
            }
            Action::Control(m) => {
                if m.t == "key" {
                    self[d].keys.push(m.clone());
                }
                self.ctl_send(d, m);
            }
        }
    }

    /// Schedules one Tick at d's Session's next() (now if past); a newer schedule replaces the
    /// pending one; `Mono::MAX` cancels it; 100,000 Ticks in a row at one time fail the run (Go
    /// schedTick).
    pub(crate) fn sched_tick(&mut self, d: DevId) {
        let now = self.now;
        let dev = &mut self.devs[d.idx()];
        let Some(s) = dev.sess.as_deref() else {
            return;
        };
        if !dev.awake() {
            return;
        }
        let n = s.next();
        if n == Mono::MAX {
            dev.cancel_tick();
            return;
        }
        let at = n.saturating_sub(dev.off).max(now);
        if dev.tick_set && at == dev.tick_at {
            return;
        }
        dev.tick += 1;
        let g = dev.tick;
        dev.tick_at = at;
        dev.tick_set = true;
        self.push(at, Task::Tick(d, g));
    }

    /// The Tick that `sched_tick` queued as generation g is due now: it does nothing if a newer
    /// schedule replaced it, and fails the run at 100,000 Ticks in a row at one time.
    pub(crate) fn tick_due(&mut self, d: DevId, g: u64) {
        let at = self.now;
        let dev = &mut self[d];
        if g != dev.tick {
            return;
        }
        dev.tick_set = false;
        if at != dev.last_tick {
            dev.last_tick = at;
            dev.same = 0;
        } else {
            dev.same += 1;
            if dev.same > 100_000 {
                let name = dev.name;
                self.fail(format_args!("{name}: next() asked for a Tick at {} 100,000 times", fmt_dur(at)));
                return;
            }
        }
        self.handle(d, Event::Tick, None);
    }

    /// Takes a datagram that arrived on link l (None: the Tailscale lane) from src (Go recv):
    /// STUN replies, the reach and hook bookkeeping, the adversary's look, then [`World::packet`].
    pub(crate) fn recv(&mut self, d: DevId, l: Option<LinkId>, src: SocketAddr, p: Pkt) {
        if !self[d].awake() {
            return;
        }
        if p.stun {
            self.stun_reply(d, l, &p);
            return;
        }
        let sock = l.map_or(MESH_SOCK, |l| self[l].sock);
        if let (Some(from), Some(to)) = (p.from, p.to)
            && from == d.peer()
        {
            if self.check {
                self.chk.reach.insert(PathKey { d: from, sock: p.sock, addr: to });
            }
            if p.adv == Adv::Genuine && !self.hooks.is_empty() {
                let link = self.link_of(from, p.sock);
                self.fire(&HookArgs { kind: HookKind::Received, dev: d, link, data: &p.data, event: None });
            }
        }
        if self.adv.is_some() && p.adv == Adv::Genuine {
            let hold = self.adv_see(d, sock, src, &p);
            if hold > 0 {
                let t = self.now + hold;
                self.at(t, move |w| w.packet(d, sock, src, p));
                return;
            }
        }
        self.packet(d, sock, src, p);
    }

    /// Hands a lane packet to the Session that owns its rid, as node does (§4): a core Session
    /// only if its RidTable lookup is the device's current owner key (Go packet).
    pub(crate) fn packet(&mut self, d: DevId, sock: SockId, src: SocketAddr, p: Pkt) {
        let dev = &self[d];
        let Some(s) = dev.sess.as_deref() else {
            return;
        };
        if !dev.awake() {
            return;
        }
        let Some(rid) = packet_rid(&p.data) else {
            return;
        };
        let ep: &dyn Any = s;
        if ep.is::<Session>() && dev.rids.lookup(rid) != Some(dev.owner) {
            return;
        }
        if self.check {
            self.chk.recvd(d, sock, src, p.data.len());
        }
        let ev = Event::Packet { sock, from: src, data: &p.data };
        self.handle(d, ev, Some(&p));
    }

    /// Passes one datagram that a program on client d sent from source port src to
    /// 127.0.0.1:port. A flow is (port, src), numbered 1, 2, 3 … per session (§5.1), wrapping
    /// after 65,535 and never 0 or a live number, as node numbers them; waits for the next event
    /// while busy; MAX_FLOWS drops (Go SendFrom).
    pub fn send_from(&mut self, d: DevId, port: u16, src: u16, data: Vec<u8>) {
        if self.busy {
            let now = self.now;
            self.at(now, move |w| w.send_from(d, port, src, data));
            return;
        }
        let dev = &mut self[d];
        if dev.sess.is_none() || !dev.awake() {
            return;
        }
        let k = (u32::from(port) << 16) | u32::from(src);
        let f = match dev.by_app.get(&k) {
            Some(&f) => f,
            None => {
                if dev.flows.len() >= MAX_FLOWS {
                    dev.dropped += 1;
                    return;
                }
                let f = dev.number(port);
                dev.by_app.insert(k, f);
                dev.flows.insert(f, Flow { last: 0, key: k });
                f
            }
        };
        self.app(d, f, data);
    }

    /// Passes one datagram from a server flow socket; a closed flow drops it (Go SendOn). A flow
    /// whose far end is the internet server ([`path`]) first crosses the anchor's egress leg
    /// ([`World::egress`]) back to the anchor, whose NAT hands it to the bond.
    pub fn send_on(&mut self, d: DevId, f: FlowId, data: Vec<u8>) {
        if d == DevId::Server && path(f.port()).internet && self.egress > 0 {
            let at = self.now + self.egress;
            self.at(at, move |w| w.send_back(d, f, data));
            return;
        }
        self.send_back(d, f, data);
    }

    /// [`World::send_on`] once the datagram is at the anchor.
    fn send_back(&mut self, d: DevId, f: FlowId, data: Vec<u8>) {
        if self.busy {
            let now = self.now;
            self.at(now, move |w| w.send_back(d, f, data));
            return;
        }
        let dev = &self[d];
        if dev.sess.is_none() || !dev.awake() || !dev.flows.contains_key(&f) {
            return;
        }
        self.app(d, f, data);
    }

    /// Go app: flow activity, idle tracking, the registry's submit, then `Event::App`.
    pub(crate) fn app(&mut self, d: DevId, f: FlowId, data: Vec<u8>) {
        let now = self.now;
        if let Some(fl) = self[d].flows.get_mut(&f) {
            fl.last = now;
        }
        self.data_seen(d);
        self.reg_submit(d, &data);
        let packet = inner(self[d].role, f, &data);
        if self.tun_mtu == 0 || packet.len() <= self.tun_mtu {
            self.handle(d, Event::Send { packet: &packet }, None);
            return;
        }
        self.ip_id = self.ip_id.wrapping_add(1);
        for p in fragment(&packet, self.tun_mtu, self.ip_id) {
            self.handle(d, Event::Send { packet: &p }, None);
        }
    }

    /// What d's kernel does with an IPv4 fragment its TUN device was given: holds it until the
    /// rest of its datagram is there, then returns the datagram whole (RFC 791). At most
    /// [`REASM_MAX`] datagrams wait; past that the oldest is given up and counted in
    /// [`World::reasm_fails`].
    pub(crate) fn reassemble(&mut self, d: DevId, p: Vec<u8>) -> Option<Vec<u8>> {
        let mut key = [0u8; 11];
        key[..8].copy_from_slice(&p[12..20]);
        key[8..10].copy_from_slice(&p[4..6]);
        key[10] = p[9];
        let fo = u16::from_be_bytes([p[6], p[7]]);
        let frag = (usize::from(fo & 0x1fff) * 8, fo & 0x2000 != 0, p);
        let i = match self.reasm.iter().position(|r| r.0 == d.idx() && r.1 == key) {
            Some(i) => i,
            None => {
                if self.reasm.len() >= REASM_MAX {
                    self.reasm.remove(0);
                    self.reasm_fails += 1;
                }
                self.reasm.push((d.idx(), key, Vec::new()));
                self.reasm.len() - 1
            }
        };
        let fr = &mut self.reasm[i].2;
        fr.push(frag);
        fr.sort_by_key(|f| f.0);
        let mut end = 0;
        for f in fr.iter() {
            if f.0 != end {
                return None;
            }
            end += f.2.len() - 20;
            if !f.1 {
                let (_, _, fr) = self.reasm.remove(i);
                let mut whole = fr[0].2[..20].to_vec();
                for f in &fr {
                    whole.extend_from_slice(&f.2[20..]);
                }
                let n = whole.len() as u16;
                whole[2..4].copy_from_slice(&n.to_be_bytes());
                whole[6..8].copy_from_slice(&[0, 0]);
                return Some(whole);
            }
        }
        None
    }

    /// What the daemon does with one inner packet its Session delivered (§6, §10.5 rule 6): the
    /// client takes only packets addressed to its bond address; the anchor forwards only packets
    /// from the client's bond address, never to another client, and either delivers them locally
    /// (addressed to its own bond address: Moonlight on the anchor, §6.1) or sends them out to the
    /// internet server, which gets them [`World::egress`] later (§6.2). Anything else is dropped
    /// and counted in [`World::ip_dropped`]; a packet [`crate::sim::outer`] could not read reaches
    /// the registry as it is, which fails the run as a forged datagram, but for an IPv4 fragment,
    /// which waits for the rest of its datagram ([`World::reassemble`]) and goes on with it whole.
    pub(crate) fn deliver_ip(&mut self, d: DevId, f: FlowId, src: Option<IpAddr>, dst: Option<IpAddr>, data: Vec<u8>) {
        let (Some(src), Some(dst)) = (src, dst) else {
            if is_fragment(&data) {
                if let Some(p) = self.reassemble(d, data)
                    && let Some(i) = outer(&p)
                {
                    let (flow, src, dst, data) = (i.flow, i.src, i.dst, i.data.to_vec());
                    self.deliver_ip(d, flow, Some(src), Some(dst), data);
                }
                return;
            }
            self.deliver(d, f, &data);
            return;
        };
        let ok = match d {
            DevId::Client => is_client(dst),
            DevId::Server => is_client(src) && !is_client(dst),
        };
        if !ok {
            self.ip_dropped += 1;
            return;
        }
        if d == DevId::Server && !is_anchor(dst) && self.egress > 0 {
            // The registry checks a datagram against the session pairing current when it arrives:
            // if a restart ended the pairing that delivered this one meanwhile, the packet is
            // dropped on the way (the application's retry comes through the new pairing).
            // The order invariant (`reg_order`) groups deliveries by the Handle call that made
            // them, so the egress hop delivers under that call, not the one current by then: two
            // late packets from two calls, the second within `egress` of the first, read as two
            // behind in one call.
            let (at, pair, call) = (self.now + self.egress, (self[d].inst, self[d].peer_inst), self.calls);
            self.at(at, move |w| {
                if (w[d].inst, w[d].peer_inst) == pair {
                    let calls = std::mem::replace(&mut w.calls, call);
                    w.deliver(d, f, &data);
                    w.calls = calls;
                }
            });
            return;
        }
        self.deliver(d, f, &data);
    }

    /// Takes one datagram d's Session delivered on f (Go deliver): a client without the flow, or a
    /// server at MAX_FLOWS, drops it; a server opens it; then the registry checks and passes it on.
    pub(crate) fn deliver(&mut self, d: DevId, f: FlowId, data: &[u8]) {
        let now = self.now;
        let dev = &mut self[d];
        if let Some(fl) = dev.flows.get_mut(&f) {
            fl.last = now;
        } else if dev.role == Role::Client || dev.flows.len() >= MAX_FLOWS {
            dev.dropped += 1; // a client has no socket for it; a server is at MAX_FLOWS
            return;
        } else {
            // The server opens the flow's socket to 127.0.0.1:port.
            dev.flows.insert(f, Flow { last: now, key: 0 });
        }
        self.data_seen(d);
        self.reg_deliver(d, f, data);
    }

    /// Tracks idle as node does: the first datagram after IDLE_AFTER re-runs STUN (§6.3).
    pub(crate) fn data_seen(&mut self, d: DevId) {
        let now = self.now;
        if now - self[d].last_data > IDLE_AFTER {
            self.stun_all(d);
        }
        self[d].last_data = now;
    }

    /// node's one-second timers: FlowEnd after FLOW_IDLE (in flow order), the STUN refresh, and
    /// the session linger after the channel went down (Go house).
    pub(crate) fn house(&mut self, d: DevId) {
        if !self[d].awake() {
            return;
        }
        let now = self.now;
        // In flow order without a copy of the keys: each round takes the first idle flow after
        // the last one ended.
        let mut from = 0u32;
        loop {
            let idle = self[d]
                .flows
                .range(FlowId(from)..)
                .find(|(_, fl)| now - fl.last >= FLOW_IDLE)
                .map(|(&f, fl)| (f, fl.key));
            let Some((f, key)) = idle else {
                break;
            };
            let dev = &mut self[d];
            dev.flows.remove(&f);
            dev.by_app.remove(&key);
            match f.0.checked_add(1) {
                Some(n) => from = n,
                None => break,
            }
        }
        let dev = &self[d];
        if now - dev.last_data < IDLE_AFTER && now - dev.last_stun >= STUN_EVERY {
            self.stun_all(d);
        }
        if self[d].sess.is_some() && !self.ctl.up && now - self.ctl.since >= SESSION_LINGER {
            let off = self[d].off;
            let quiet = match self[d].snap().last_auth {
                None => true,
                Some(la) => now - (la - off) >= SESSION_LINGER,
            };
            if quiet {
                self.close_session(d);
            }
        }
    }

    /// STUN on every link of d (Go stunAll).
    pub(crate) fn stun_all(&mut self, d: DevId) {
        for i in 0..self[d].links.len() {
            let l = self[d].links[i];
            self.stun(d, l);
        }
        let now = self.now;
        self[d].last_stun = now;
    }

    /// Sends one binding request from l's socket; the reply comes back through the NAT after a
    /// round trip and is counted on the link like any packet (Go stun).
    pub(crate) fn stun(&mut self, d: DevId, l: LinkId) {
        if !self[d].awake() {
            return;
        }
        let now = self.now;
        let k = &mut self[l];
        k.stun_at = Some(now);
        let sock = k.sock;
        self.route(d, sock, STUN_SERVER, vec![0; 20], true);
    }

    /// Takes a STUN reply on l: within STUN_WAIT of the request, a changed mapping is a link
    /// change (Go stunReply).
    pub(crate) fn stun_reply(&mut self, d: DevId, l: Option<LinkId>, p: &Pkt) {
        let Some(l) = l else {
            return;
        };
        let now = self.now;
        let k = &mut self[l];
        match k.stun_at {
            Some(t) if now - t <= STUN_WAIT => {}
            _ => return,
        }
        k.stun_at = None;
        if p.mapped != k.mapped {
            k.mapped = p.mapped;
            self.links_changed(d);
        }
    }

    /// Tells d's Session (a full `Event::Links`) and the peer (`links`) the device's links, as
    /// node does on every change; waits for the next event while busy (Go LinksChanged).
    pub fn links_changed(&mut self, d: DevId) {
        if self.busy {
            let now = self.now;
            self.at(now, move |w| w.links_changed(d));
            return;
        }
        let links = self.core_links(d);
        let mesh = Some(self.mesh_lane(d));
        self.handle(d, Event::Links { links: &links, mesh }, None);
        if self.ctl.up {
            let m = Msg { t: "links".into(), links: self.infos(d), ..Msg::default() };
            self.ctl_send(d, m);
        }
    }

    /// Moves l to a new local address, as a DHCP renewal or a replugged iPhone does (Go
    /// Link.SetLocal): the NAT's local address too, then links_changed and STUN. node reopens the
    /// link's socket, so the old socket's mapping goes until STUN answers (Go kept it).
    pub fn set_local(&mut self, l: LinkId, a: SocketAddr) {
        let k = &mut self[l];
        k.local = a;
        k.mapped = None;
        k.nat.set_local(a.ip());
        let d = k.dev;
        self.links_changed(d);
        self.stun(d, l);
    }

    /// d's links as core sees them (Go coreLinks).
    pub(crate) fn core_links(&self, d: DevId) -> Vec<wirebond_core::Link> {
        let link = |l: LinkId| {
            let k = &self[l];
            wirebond_core::Link {
                id: k.id,
                label: self.label(d, l),
                metered: k.spec.metered,
                default: self.node_default(d, l),
                sock: k.sock,
                local: k.local,
                mapped: k.mapped,
            }
        };
        self.offered(d).map(link).collect()
    }

    /// l's label as node gives it: a label two interfaces share gets the interface's name
    /// ("iPhone en3"), since core groups bottlenecks by label (§6.1, notes/netsim.md 24). Each
    /// netsim link is its own interface, en<id>.
    pub(crate) fn label(&self, d: DevId, l: LinkId) -> String {
        let k = &self[l];
        let shared = self[d].links.iter().any(|&o| o != l && self[o].spec.label == k.spec.label);
        if shared { format!("{} en{}", k.spec.label, k.id) } else { k.spec.label.to_string() }
    }

    /// d's Tailscale lane (Go meshLane); `Event::Links` carries it as Some always: netsim's mesh
    /// address never goes away (A21 is the netns tier's).
    pub(crate) fn mesh_lane(&self, d: DevId) -> MeshLane {
        MeshLane { sock: MESH_SOCK, local: self[d].mesh, peer: self[d.peer()].mesh }
    }

    /// The links as hello and links offer them: local address, STUN mapping, and the mapping's
    /// address with PORT (§6.3) (Go infos).
    pub(crate) fn infos(&self, d: DevId) -> Vec<LinkInfo> {
        let info = |l: LinkId| {
            let k = &self[l];
            LinkInfo {
                id: k.id,
                label: self.label(d, l),
                metered: k.spec.metered,
                default: self.node_default(d, l),
                cands: cands(k.local, k.mapped),
            }
        };
        self.offered(d).map(info).collect()
    }

    /// The links d's node offers core and the peer: none of the client's in the baseline
    /// ([`World::mesh_only`]), where the bond is the anchor over the mesh's single path.
    fn offered(&self, d: DevId) -> impl Iterator<Item = LinkId> + '_ {
        let none = self.mesh_only && d == DevId::Client;
        self[d].links.iter().copied().filter(move |l| !none && !self.pulled.contains(l))
    }

    /// Pulls l out, as unplugging a cable or a phone does (SPEC §23.3 A5): from now on nothing
    /// crosses it either way, and its device's node, which lists its links every LINK_SCAN
    /// (§9.1), drops it from the list it gives core and the peer by then. The mesh, which rode
    /// it if it was the client's default link, moves to the client's next link.
    pub fn pull(&mut self, l: LinkId) {
        let k = &mut self[l];
        (k.up.hole, k.down.hole) = (true, true);
        let d = k.dev;
        self.pulled.push(l);
        let at = self.now + LINK_SCAN;
        self.at(at, move |w| w.links_changed(d));
    }

    /// d's hello: protocol, version, inst, links; the server's shared ports or the client's mode.
    pub(crate) fn hello(&self, d: DevId) -> Msg {
        let dev = &self[d];
        let mut m = Msg {
            t: "hello".into(),
            v: PROTOCOL,
            version: VERSION.into(),
            inst: dev.inst,
            links: self.infos(d),
            ..Msg::default()
        };
        match dev.role {
            Role::Anchor => {}
            Role::Client => m.mode.clone_from(&dev.mode),
        }
        m
    }

    /// Takes one control message from the peer. A hello from a new inst makes a new Session (§7.6
    /// rule 8); hello and links are filtered and their candidates recorded for the checker (Go
    /// control).
    pub(crate) fn control(&mut self, d: DevId, mut m: Msg) {
        if !self[d].awake() {
            return;
        }
        if m.t == "hello" || m.t == "links" {
            if m.t == "hello" && (self[d].sess.is_none() || m.inst != self[d].peer_inst) {
                self.new_session(d, m.inst);
            }
            m.links = filter(std::mem::take(&mut m.links));
            // Only the checker reads the offered candidates (Go recorded them unconditionally).
            if self.check {
                self.chk.offered(d, &m.links);
            }
        }
        self.handle(d, Event::Control(&m), None);
    }

    /// Makes d's Session for peer inst peer and gives it, in order, the device's links and, on a
    /// client, the connection's mode; the caller then passes the peer's hello (Go newSession).
    pub(crate) fn new_session(&mut self, d: DevId, peer: u64) {
        self.close_session(d);
        let rand = key_reader(self.sub());
        let dev = &mut self[d];
        dev.sessions += 1;
        dev.owner = u64::from(dev.sessions);
        let (role, owner, inst) = (dev.role, dev.owner, dev.inst);
        let ep = (self.mk)(role, owner, rand);
        let pair = self.pair(inst, peer);
        let dev = &mut self[d];
        dev.sess = Some(ep);
        dev.peer_inst = peer;
        dev.pair_id = pair;
        dev.last_snap = SessionStatus::default();
        dev.flows.clear();
        dev.by_app.clear();
        dev.num = 0;
        let links = self.core_links(d);
        let mesh = Some(self.mesh_lane(d));
        self.handle(d, Event::Links { links: &links, mesh }, None);
        if role == Role::Client {
            let m = Msg { t: "mode".into(), mode: self[d].mode.clone(), ..Msg::default() };
            self.handle(d, Event::Control(&m), None);
        }
    }

    /// Closes d's Session, if any, freeing its rids and cancelling its Tick (Go closeSession).
    pub(crate) fn close_session(&mut self, d: DevId) {
        let dev = &mut self[d];
        if let Some(mut s) = dev.sess.take() {
            s.close(&mut dev.rids);
            dev.cancel_tick();
        }
    }

    /// Changes the client's mode, as `wirebond connect xanth bandwidth` does (Go SetMode).
    pub fn set_mode(&mut self, d: DevId, mode: &str) {
        self[d].mode = mode.into();
        let m = Msg { t: "mode".into(), mode: mode.into(), ..Msg::default() };
        self.handle(d, Event::Control(&m), None);
        self.ctl_send(d, m);
    }

    /// Freezes d for dur: no timers, no packets, no programs. With advance its monotonic clock
    /// counts the sleep (as Windows may); otherwise it pauses (macOS, Linux). On wake node injects
    /// `Event::Wake`, re-runs STUN and redials the control channel (§8.4) (Go Sleep).
    pub fn sleep(&mut self, d: DevId, dur: Dur, advance: bool) {
        if !self[d].awake() {
            return;
        }
        let dev = &mut self[d];
        dev.asleep = true;
        dev.cancel_tick();
        self.ctl_drop();
        let t = self.now + dur;
        self.at(t, move |w| {
            let dev = &mut w[d];
            dev.asleep = false;
            if !advance {
                dev.off -= dur;
            }
            w.handle(d, Event::Wake, None);
            w.stun_all(d);
            w.ctl_dial();
        });
    }

    /// Stops d's process for down; it comes back with a new inst, no Session and a new rid table,
    /// and the control channel is redialled (§7.6 rule 8) (Go Restart).
    pub fn restart(&mut self, d: DevId, down: Dur) {
        if !self[d].awake() {
            return;
        }
        self.close_session(d);
        self[d].down = true;
        self.ctl_drop();
        let t = self.now + down;
        self.at(t, move |w| {
            let inst = w.rng.u64() | 1;
            let dev = &mut w[d];
            dev.down = false;
            dev.inst = inst;
            dev.rids = RidTable::new();
            dev.last_data = -HOUR;
            w.stun_all(d);
            w.ctl_dial();
        });
    }

    /// Sends data from d's socket sock to `to` through the link and NAT models (Go route): the
    /// Tailscale lane rides the client's default link (Up from the client, Down from the server)
    /// with TS_OVER more bytes per packet and MESH_MTU, or its own path when a scenario gave it one
    /// ([`World::mesh_path`]), with no Tailscale overhead.
    pub(crate) fn route(&mut self, d: DevId, sock: SockId, to: SocketAddr, data: Vec<u8>, stun: bool) {
        let p = Pkt { data, stun, from: Some(d), sock, to: Some(to), t0: self.now, ..Pkt::default() };
        if sock == MESH_SOCK {
            let peer = d.peer();
            let (l, over) = match self.mesh_link {
                Some(l) => (l, 0),
                None => match self.default_link(DevId::Client) {
                    Some(l) => (l, TS_OVER),
                    None => return,
                },
            };
            if to != self[peer].mesh || self.mesh_down {
                return;
            }
            let side = match d {
                DevId::Client => Side::Up,
                DevId::Server => {
                    if !self.hooks.is_empty() {
                        self.fire(&HookArgs {
                            kind: HookKind::Inbound,
                            dev: d,
                            link: None,
                            data: &p.data,
                            event: None,
                        });
                    }
                    Side::Down
                }
            };
            let (k, n) = frags(p.data.len() + IP_UDP, MESH_MTU);
            let src = self[d].mesh;
            let then = Then::Recv { dev: peer, link: None, src };
            self.dir_send(l.dir(side), p, n + k * over, k, then);
            return;
        }
        let Some(l) = self.link_of(d, sock) else {
            return;
        };
        let now = self.now;
        let lk = &mut self[l];
        let src = lk.nat.out(now, lk.local, to);
        let (k, n) = frags(p.data.len() + IP_UDP, lk.up.mtu());
        self.dir_send(l.up(), p, n, k, Then::Arrive { from: l, src, to });
    }

    /// Takes a packet that left link sl with public source src, addressed to `to` (Go arrive):
    /// the STUN server answers; otherwise the other device's link with that public address.
    pub(crate) fn arrive(&mut self, p: Pkt, sl: LinkId, src: SocketAddr, to: SocketAddr) {
        if to == STUN_SERVER {
            if p.stun {
                let reply = Pkt { data: vec![0; 32], stun: true, mapped: Some(src), t0: self.now, ..Pkt::default() };
                self.inbound(reply, sl, STUN_SERVER, src);
            }
            return;
        }
        let from = self[sl].dev;
        let dst = self.links.iter().position(|l| l.dev != from && IpAddr::V4(l.nat.public) == to.ip());
        if let Some(i) = dst {
            self.inbound(p, LinkId(i), src, to);
        }
    }

    /// Passes p from src through link l's NAT and down direction to its device (Go inbound).
    pub(crate) fn inbound(&mut self, p: Pkt, l: LinkId, src: SocketAddr, to: SocketAddr) {
        let now = self.now;
        let lk = &mut self[l];
        if lk.nat.inbound(now, src, to.port()) != Some(lk.local) {
            return;
        }
        let dev = lk.dev;
        if p.from == Some(DevId::Server) && dev == DevId::Client && !self.hooks.is_empty() {
            self.fire(&HookArgs {
                kind: HookKind::Inbound,
                dev: DevId::Server,
                link: Some(l),
                data: &p.data,
                event: None,
            });
        }
        let (k, n) = frags(p.data.len() + IP_UDP, self[l].down.mtu());
        let then = Then::Recv { dev, link: Some(l), src };
        self.dir_send(l.down(), p, n, k, then);
    }

    /// Takes the control channel's path down (Go CutControl).
    pub fn cut_control(&mut self) {
        self.ctl.cut = true;
        self.ctl_drop();
    }

    /// Brings it back; the client redials within REDIAL_WAITING (Go RestoreControl).
    pub fn restore_control(&mut self) {
        self.ctl.cut = false;
        self.ctl_dial();
    }

    /// Whether the control channel is connected (Go ControlUp).
    pub fn control_up(&self) -> bool {
        self.ctl.up
    }

    /// Go ctlChan.drop.
    pub(crate) fn ctl_drop(&mut self) {
        let c = &mut self.ctl;
        if c.up {
            c.up = false;
            c.since = self.now;
            c.generation += 1;
        }
    }

    /// Whether a dial would connect: not cut, both devices awake (Go ready).
    pub(crate) fn ctl_ready(&self) -> bool {
        !self.ctl.cut && self[DevId::Client].awake() && self[DevId::Server].awake()
    }

    /// Connects within one round trip, or tries again every REDIAL_WAITING (§7.1) (Go dial).
    /// Every connect sends hello both ways.
    pub(crate) fn ctl_dial(&mut self) {
        if self.ctl.up || self.ctl.dialing {
            return;
        }
        self.ctl.dialing = true;
        let wait = if self.ctl_ready() { self.ctl_delay(0) + self.ctl_delay(1) } else { REDIAL_WAITING };
        let t = self.now + wait;
        self.at(t, |w| {
            w.ctl.dialing = false;
            if w.ctl.up {
                return;
            }
            if !w.ctl_ready() {
                w.ctl_dial();
                return;
            }
            w.ctl.up = true;
            w.ctl.generation += 1;
            for d in DevId::BOTH {
                let m = w.hello(d);
                w.ctl_send(d, m);
            }
        });
    }

    /// The mesh's one-way delay: the client's default link's propagation delay + 1 ms, or the
    /// delay of the Tailscale lane's own path when it has one; dir 0 is client → server (Go
    /// delay). 30 ms without a default link.
    pub(crate) fn ctl_delay(&self, dir: usize) -> Dur {
        let side = if dir == 0 { Side::Up } else { Side::Down };
        if let Some(l) = self.mesh_link {
            return self[l.dir(side)].p.delay;
        }
        let Some(l) = self.default_link(DevId::Client) else {
            return 30 * MS;
        };
        self[l.dir(side)].p.delay + MS
    }

    /// Gives the Tailscale lane a path of its own, profile p each way, instead of the client's
    /// default link: the system harness's tailscale0, a veth of its own with netem's delay
    /// (tests/system/run.sh). Its packets carry no Tailscale overhead there, as the harness's do
    /// not, and the control channel takes its delay. The path belongs to no device: core never
    /// sees it as a link. Call before the world starts.
    pub fn mesh_path(&mut self, p: Profile) -> LinkId {
        let mesh = self[DevId::Client].mesh;
        let nat = Nat::new(NatKind::OneToOne, mesh.ip(), Ipv4Addr::UNSPECIFIED, self.sub());
        let up = Dir::new(p.clone(), self.sub());
        let down = Dir::new(p.clone(), self.sub());
        let l = LinkId(self.links.len());
        let spec = LinkSpec { label: "Tailscale", up: p.clone(), down: p, ..LinkSpec::default() };
        self.links.push(Link {
            id: 0,
            dev: DevId::Client,
            sock: MESH_SOCK,
            local: mesh,
            mapped: None,
            nat,
            up,
            down,
            bumps: Vec::new(),
            used: -HOUR,
            stun_at: None,
            spec,
        });
        self.mesh_link = Some(l);
        l
    }

    /// Carries m to the peer as node does: one JSON line of at most MAX_MSG bytes, its newline
    /// included (node's reader holds MAX_MSG), decoded again on arrival, in order; dropped while
    /// the channel is down (Go ctlChan.send).
    pub(crate) fn ctl_send(&mut self, from: DevId, m: Msg) {
        if !self.ctl.up {
            return;
        }
        let b = match serde_json::to_vec(&m) {
            Ok(b) if b.len() < MAX_MSG => b,
            res => {
                let (n, why) = match res {
                    Ok(b) => (b.len(), format!("over MAX_MSG {MAX_MSG} with its newline")),
                    Err(e) => (0, e.to_string()),
                };
                let name = self[from].name;
                self.fail(format_args!("{name}: control message {:?} cannot be sent ({n} bytes, {why})", m.t));
                return;
            }
        };
        let (dir, to) = match from {
            DevId::Client => (0, DevId::Server),
            DevId::Server => (1, DevId::Client),
        };
        let earliest = self.now + self.ctl_delay(dir);
        let at = self.ctl.slot(dir, earliest);
        let generation = self.ctl.generation;
        let code = self[from].code();
        self.logf(b'C', code, 0, 0, &b);
        self.at(at, move |w| {
            if generation != w.ctl.generation || !w.ctl.up {
                return;
            }
            match serde_json::from_slice::<Msg>(&b) {
                Ok(got) => w.control(to, got),
                Err(e) => w.fail(format_args!("control message does not decode: {e}")),
            }
        });
    }
}

/// Drops the candidates node never passes to core (§6.3), as node's filter does: at most
/// MAX_LINKS links and MAX_CANDS candidates each; no loopback, unspecified, multicast, link-local,
/// broadcast or mesh address and no port 0; an IPv4-mapped address is kept as its IPv4 address.
pub(crate) fn filter(mut ls: Vec<LinkInfo>) -> Vec<LinkInfo> {
    ls.truncate(MAX_LINKS);
    for l in &mut ls {
        let mut kept = 0;
        l.cands.retain_mut(|c| {
            let a = c.ip().to_canonical();
            let keep = kept < MAX_CANDS && c.port() != 0 && unicast(a) && !is_mesh(a);
            if keep {
                kept += 1;
                if a != c.ip() {
                    *c = SocketAddr::new(a, c.port());
                }
            }
            keep
        });
    }
    ls
}

/// Whether node would take a as a peer's candidate address (Go netip's IsGlobalUnicast, which
/// node's filter uses): not unspecified, broadcast, loopback, multicast or link-local.
fn unicast(a: IpAddr) -> bool {
    match a {
        IpAddr::V4(v) => {
            !(v.is_unspecified() || v == BCAST || v.is_loopback() || v.is_multicast() || v.is_link_local())
        }
        IpAddr::V6(v) => !(v.is_unspecified() || v.is_loopback() || v.is_multicast() || v.is_unicast_link_local()),
    }
}

/// A link's candidates as node offers them (§6.3 item 2), most preferred first: the local
/// address, the STUN mapping, and the mapping's address on PORT (a router forward usually keeps
/// the port); none twice.
fn cands(local: SocketAddr, mapped: Option<SocketAddr>) -> Vec<SocketAddr> {
    let mut cs = vec![local];
    if let Some(m) = mapped {
        for a in [m, SocketAddr::new(m.ip(), PORT)] {
            if !cs.contains(&a) {
                cs.push(a);
            }
        }
    }
    cs
}

/// handle's event-log record of ev (Go's `uint64(ev.Flow)<<16 | uint64(ev.Sock)` and ev.Data).
fn log_args<'a>(ev: &Event<'a>) -> (u64, &'a [u8]) {
    match *ev {
        Event::Send { packet } => (0, packet),
        Event::Packet { sock, data, .. } => (u64::from(sock), data),
        Event::Links { .. } | Event::Wake | Event::Control(_) | Event::Tick => (0, &[]),
    }
}

#[cfg(test)]
mod tests {
    //! The stand-in's own tests (Go had none for node.go; the self-tests in tests/selftest.rs
    //! covered it). The first group runs on its own: pure functions, and fixtures that build
    //! links without the link model. The second group runs the stand-in end to end with a
    //! scripted endpoint in place of core, over the real engine, links, NAT and registry.

    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    use wirebond_core::{MODE_BANDWIDTH, Out, TAG_LEN, TYPE_DATA, TYPE_PROBE};

    use super::*;
    use crate::env::{ADDR_CELL, ADDR_HOME, ADDR_HOTEL, PUB_CELL, PUB_HOME, PUB_HOTEL};
    use crate::sim::{Maker, SEC};
    use crate::trace::TracePos;

    fn addrs<const N: usize>(ss: [&str; N]) -> Vec<SocketAddr> {
        ss.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn mesh_ranges() {
        for (a, want) in [
            ("100.64.0.0", true),
            ("100.101.1.2", true),
            ("100.127.255.255", true),
            ("100.63.255.255", false),
            ("100.128.0.0", false),
            ("::ffff:100.100.1.1", true),
            ("fd7a:115c:a1e0::1", true),
            ("fd7a:115c:a1e0:ffff::1", true),
            ("fd7a:115c:a1e1::1", false),
            ("192.168.1.50", false),
        ] {
            assert_eq!(is_mesh(a.parse().unwrap()), want, "{a}");
        }
    }

    /// node's TestFilter: what a receiver ignores (§6.3 item 3), the caps, port 0 and unmapping.
    #[test]
    fn filter_drops_what_node_drops() {
        let mut ls = vec![LinkInfo::default(); MAX_LINKS + 3];
        ls[0].cands = addrs([
            "127.0.0.1:7849",
            "0.0.0.0:7849",
            "[::]:7849",
            "224.0.0.1:7849",
            "255.255.255.255:7849",
            "169.254.1.1:7849",
            "[fe80::1]:7849",
            "100.101.1.2:7849",
            "[fd7a:115c:a1e0::1]:7849",
            "[ff02::1]:7849",
            "198.51.100.1:0",
            "[::ffff:198.51.100.2]:7849",
            "10.0.0.5:7849",
            "203.0.113.9:61022",
            "[2607:fb90::1]:7849",
            "192.0.2.1:1",
        ]);
        let got = filter(ls);
        assert_eq!(got.len(), MAX_LINKS);
        let want = addrs(["198.51.100.2:7849", "10.0.0.5:7849", "203.0.113.9:61022", "[2607:fb90::1]:7849"]);
        assert_eq!(got[0].cands, want);
        assert!(got[1..].iter().all(|l| l.cands.is_empty()));
    }

    #[test]
    fn candidates_local_mapping_and_port() {
        let local = ADDR_HOTEL;
        let mapped: SocketAddr = "203.0.113.9:40001".parse().unwrap();
        let fwd = SocketAddr::new(PUB_HOTEL.into(), PORT);
        assert_eq!(cands(local, None), [local]);
        assert_eq!(cands(local, Some(mapped)), [local, mapped, fwd]);
        // A forward that keeps the port, and a public interface whose mapping is its address.
        assert_eq!(cands(local, Some(fwd)), [local, fwd]);
        assert_eq!(cands(fwd, Some(fwd)), [fwd]);
    }

    /// node's TestFlowNumbers: from 1, wrapping after 65,535, never 0 or a live number; one
    /// counter per session across ports.
    #[test]
    fn flow_numbers() {
        let mut d = Device::new("mac", Role::Client, ADDR_HOTEL, &mut Rng::new(1, 2));
        assert_eq!(d.number(47998), FlowId::new(47998, 1));
        d.num = 65534;
        d.flows.insert(FlowId::new(47998, 1), Flow::default()); // still live
        assert_eq!([d.number(47998).num(), d.number(47998).num()], [65535, 2]);
        assert_eq!(d.number(48000), FlowId::new(48000, 3));
    }

    /// handle's event-log record: Go's (flow << 16 | sock, data).
    #[test]
    fn handle_log_records() {
        let f = FlowId::new(47998, 3);
        let b = [1u8, 2, 3];
        let pk = inner(Role::Client, f, &b);
        let app = Event::Send { packet: &pk };
        assert_eq!(log_args(&app), (0, &pk[..]));
        let p = Event::Packet { sock: 2, from: ADDR_HOME, data: &b };
        assert_eq!(log_args(&p), (2, &b[..]));
        assert_eq!(log_args(&Event::Tick), (0, &[][..]));
    }

    // ------------------------------------------------------------------------------------------
    // A scripted endpoint: the test double for core.

    /// What a scripted endpoint saw, in order.
    #[derive(Clone, Debug, PartialEq)]
    enum Seen {
        Links(Vec<wirebond_core::Link>),
        Control(Box<Msg>),
        App(FlowId),
        Packet(SockId, SocketAddr),
        Wake,
        #[allow(dead_code)] // 2.0: the engine ends idle flows itself (FLOW_IDLE); nothing sends FlowEnd
        FlowEnd(FlowId),
        Tick(Mono),
        Close,
    }

    impl Seen {
        fn kind(&self) -> &str {
            match self {
                Seen::Links(..) => "Links",
                Seen::Control(m) => &m.t,
                Seen::App(..) => "App",
                Seen::Packet(..) => "Packet",
                Seen::Wake => "Wake",
                Seen::FlowEnd(_) => "FlowEnd",
                Seen::Tick(_) => "Tick",
                Seen::Close => "Close",
            }
        }
    }

    /// Every event of every scripted endpoint, as (session, what): "mac#1" is the client's
    /// first Session.
    type Log = Rc<RefCell<Vec<(String, Seen)>>>;

    /// next() of each role's sessions (Mono::MAX: nothing due), for the test to set.
    type Next = Rc<[Cell<Mono>; 2]>;

    /// A stand-in for core: it logs every event; answers a hello with a key; seals each app
    /// datagram into a lane packet on the Tailscale lane; delivers a data packet's datagram on its
    /// flow (a probe is only logged) and reports the newest packet as last_auth; a Tick consumes
    /// the deadline the test set.
    struct Script {
        name: String,
        owner: u64,
        role: Role,
        log: Log,
        next: Next,
        mesh: Option<MeshLane>,
        ctr: u64,
        last_auth: Option<Mono>,
    }

    impl Endpoint for Script {
        fn handle(&mut self, now: Mono, ev: Event<'_>, _: &mut RidTable, out: &mut dyn Out) {
            let seen = match ev {
                Event::Links { links, mesh } => {
                    self.mesh = mesh;
                    Seen::Links(links.to_vec())
                }
                Event::Control(m) => {
                    if m.t == "hello" {
                        out.control(Msg {
                            t: "key".into(),
                            rid: self.owner as u16,
                            k: vec![self.owner as u8; 32],
                            ..Msg::default()
                        });
                    }
                    Seen::Control(Box::new(m.clone()))
                }
                Event::Send { packet } => {
                    let flow = crate::sim::outer(packet).map_or(FlowId(0), |i| i.flow);
                    if let Some(m) = self.mesh {
                        out.send(m.sock, m.peer, &lane(TYPE_DATA, self.ctr, flow, packet));
                        self.ctr += 1;
                    }
                    Seen::App(flow)
                }
                Event::Packet { sock, from, data } => {
                    self.last_auth = Some(now);
                    if data[0] == TYPE_DATA {
                        out.deliver(&data[15..data.len() - TAG_LEN]);
                    }
                    Seen::Packet(sock, from)
                }
                Event::Wake => Seen::Wake,
                Event::Tick => {
                    self.next[self.role as usize].set(Mono::MAX);
                    Seen::Tick(now)
                }
            };
            self.log.borrow_mut().push((self.name.clone(), seen));
        }

        fn next(&self) -> Mono {
            self.next[self.role as usize].get()
        }

        fn snapshot(&self) -> SessionStatus {
            SessionStatus { last_auth: self.last_auth, ..SessionStatus::default() }
        }

        fn close(&mut self, _: &mut RidTable) {
            self.log.borrow_mut().push((self.name.clone(), Seen::Close));
        }
    }

    /// A lane packet as the scripted endpoint seals one: type, rid 1, counter, flow, payload and
    /// a zero tag.
    fn lane(typ: u8, ctr: u64, flow: FlowId, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![typ, 0, 1];
        b.extend_from_slice(&ctr.to_be_bytes());
        b.extend_from_slice(&flow.0.to_be_bytes());
        b.extend_from_slice(payload);
        b.extend_from_slice(&[0; TAG_LEN]);
        b
    }

    /// Makes scripted endpoints that log to log and take next() from next.
    fn script_maker(log: &Log, next: &Next) -> Maker {
        let (log, next) = (log.clone(), next.clone());
        Box::new(move |role, owner, _| {
            let dev = match role {
                Role::Client => "mac",
                Role::Anchor => "xanth",
            };
            Box::new(Script {
                name: format!("{dev}#{owner}"),
                owner,
                role,
                log: log.clone(),
                next: next.clone(),
                mesh: None,
                ctr: 0,
                last_auth: None,
            })
        })
    }

    /// A world of scripted endpoints for seed 1, its log and its next() cells.
    fn scripted() -> (World, Log, Next) {
        scripted_seed(1)
    }

    fn scripted_seed(seed: u64) -> (World, Log, Next) {
        let log = Log::default();
        let next: Next = Rc::new([Cell::new(Mono::MAX), Cell::new(Mono::MAX)]);
        let w = World::new(seed, script_maker(&log, &next));
        (w, log, next)
    }

    /// What session `name` saw, in order.
    fn seen(log: &Log, name: &str) -> Vec<Seen> {
        let log = log.borrow();
        log.iter().filter(|(n, _)| n == name).map(|(_, s)| s.clone()).collect()
    }

    fn kinds(ss: &[Seen]) -> Vec<&str> {
        ss.iter().map(Seen::kind).collect()
    }

    // ------------------------------------------------------------------------------------------
    // Fixtures that need no link model.

    /// A direction with profile p, built without `Dir::new` (no draws), for tests that never send.
    fn bare_dir(p: Profile) -> Dir {
        Dir {
            p,
            hole: false,
            bytes: 0,
            drops: 0,
            pkts: 0,
            qdrops: 0,
            r: Rng::new(1, 2),
            seed: 0,
            hotel: None,
            q: VecDeque::new(),
            qb: 0,
            limit: 0,
            tok: 0.0,
            depth: 0.0,
            tok_at: 0,
            busy: false,
            bad: false,
            last: -1,
            tpos: TracePos::default(),
            flight: VecDeque::new(),
        }
    }

    /// Gives d a link as `add_link` does (id, socket, NAT, both directions) without the link
    /// model's draws, so these tests do not depend on it.
    fn bare_link(w: &mut World, d: DevId, s: LinkSpec) -> LinkId {
        let id = w[d].links.len() as u8 + 1;
        let l = LinkId(w.links.len());
        w.links.push(Link {
            id,
            dev: d,
            sock: SockId::from(id),
            local: s.local,
            mapped: None,
            nat: Nat::new(s.nat, s.local.ip(), s.public, Rng::new(3, 4)),
            up: bare_dir(s.up.clone()),
            down: bare_dir(s.down.clone()),
            bumps: Vec::new(),
            used: -HOUR,
            stun_at: None,
            spec: s,
        });
        w[d].links.push(l);
        l
    }

    fn delay(d: Dur) -> Profile {
        Profile { delay: d, ..Profile::default() }
    }

    /// A label two interfaces share gets the interface's name (§6.1, notes/netsim.md 24: A4's
    /// four-link variant has two phones); only a client marks its default link (§6.1).
    #[test]
    fn labels_unique_per_interface() {
        let (mut w, _, _) = scripted();
        let spec = |label, default, local: &str| LinkSpec {
            label,
            default,
            local: local.parse().unwrap(),
            ..LinkSpec::default()
        };
        bare_link(&mut w, DevId::Client, spec("Wi-Fi", true, "192.168.1.50:7849"));
        bare_link(&mut w, DevId::Client, spec("iPhone", false, "172.20.10.2:7849"));
        bare_link(&mut w, DevId::Client, spec("iPhone", false, "172.20.11.2:7849"));
        let eth = bare_link(&mut w, DevId::Server, spec("Ethernet", true, "192.168.0.10:7849"));
        let labels = |d| w.core_links(d).into_iter().map(|l| (l.label, l.default)).collect::<Vec<_>>();
        let want = [("Wi-Fi", true), ("iPhone en2", false), ("iPhone en3", false)];
        assert_eq!(labels(DevId::Client), want.map(|(l, d)| (l.to_string(), d)));
        assert_eq!(labels(DevId::Server), [("Ethernet".to_string(), false)]);
        let infos = w.infos(DevId::Client);
        assert_eq!(infos.iter().map(|l| l.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!((infos[2].label.as_str(), infos[0].default), ("iPhone en3", true));
        assert_eq!(w.link_of(DevId::Client, 3), Some(LinkId(2)));
        assert_eq!(w.link_of(DevId::Client, MESH_SOCK), None);
        assert_eq!(w.link_of(DevId::Server, 1), Some(eth));
        assert_eq!(w.default_link(DevId::Client), Some(LinkId(0)));
    }

    /// hello carries the protocol, inst and links with their candidates, which the peer's filter
    /// keeps, and the client's mode or the server's shared ports; it survives the JSON trip.
    #[test]
    fn hello_offers_what_node_offers() {
        let (mut w, _, _) = scripted();
        let spec = |label, default, local| LinkSpec { label, default, local, ..LinkSpec::default() };
        let hotel = bare_link(&mut w, DevId::Client, spec("Wi-Fi", true, ADDR_HOTEL));
        bare_link(&mut w, DevId::Server, spec("Ethernet", false, ADDR_HOME));
        let mapped: SocketAddr = "203.0.113.9:40001".parse().unwrap();
        w[hotel].mapped = Some(mapped);
        let h = w.hello(DevId::Client);
        assert_eq!((h.t.as_str(), h.v, h.inst), ("hello", PROTOCOL, w[DevId::Client].inst));
        assert_eq!(h.mode.as_str(), MODE_PERFORMANCE);
        let cs = vec![ADDR_HOTEL, mapped, SocketAddr::new(mapped.ip(), PORT)];
        assert_eq!((&h.links[0].cands, h.links[0].default), (&cs, true));
        assert_eq!(filter(h.links.clone())[0].cands, cs, "the peer keeps them");
        let b = serde_json::to_vec(&h).unwrap();
        assert_eq!(serde_json::from_slice::<Msg>(&b).unwrap(), h);
        let s = w.hello(DevId::Server);
        assert_eq!(s.mode.as_str(), "");
        assert_eq!(s.links[0].cands, [ADDR_HOME]);
        let m = w.mesh_lane(DevId::Server);
        let want = (MESH_SOCK, w[DevId::Server].mesh, w[DevId::Client].mesh);
        assert_eq!((m.sock, m.local, m.peer), want);
    }

    /// Control-channel order: a message never arrives before the one sent before it in its
    /// direction, even when the path got faster meanwhile; the two directions are independent.
    #[test]
    fn control_order() {
        let mut c = CtlChan::default();
        assert_eq!(c.slot(0, 31 * MS), 31 * MS);
        assert_eq!(c.slot(0, 2 * MS), 31 * MS);
        assert_eq!(c.slot(1, 5 * MS), 5 * MS);
        assert_eq!(c.slot(0, 40 * MS), 40 * MS);
        assert_eq!(c.slot(1, 4 * MS), 5 * MS);
    }

    /// When the queued events run, in order.
    fn queued(w: &World) -> Vec<Dur> {
        let mut ts: Vec<Dur> = w.q.iter().map(|e| e.at).collect();
        ts.sort_unstable();
        ts
    }

    /// The client dials within one round trip of the mesh (its default link's delays + 1 ms each
    /// way; 30 ms each way without one), or every REDIAL_WAITING while the channel is cut or a
    /// device sleeps; one dial at a time.
    #[test]
    fn dial_timing() {
        let (mut w, _, _) = scripted();
        assert_eq!((w.ctl_delay(0), w.ctl_delay(1)), (30 * MS, 30 * MS));
        let wifi = LinkSpec { default: true, up: delay(30 * MS), down: delay(50 * MS), ..LinkSpec::default() };
        bare_link(&mut w, DevId::Client, wifi);
        assert_eq!((w.ctl_delay(0), w.ctl_delay(1)), (31 * MS, 51 * MS));
        assert!(w.ctl_ready());
        w.ctl_dial();
        w.ctl_dial();
        assert!(w.ctl.dialing && !w.control_up());
        assert_eq!(queued(&w), [82 * MS]);

        let (mut w, _, _) = scripted();
        w.cut_control();
        assert!(!w.ctl_ready());
        w.restore_control();
        assert_eq!(queued(&w), [60 * MS]);

        let (mut w, _, _) = scripted();
        w[DevId::Server].asleep = true;
        w.ctl_dial();
        assert_eq!(queued(&w), [REDIAL_WAITING]);
    }

    /// Runs the queued events up to t, which must all be closures or Ticks.
    fn run_calls(w: &mut World, t: Dur) {
        while w.q.peek().is_some_and(|e| e.at <= t) {
            let e = w.q.pop().unwrap();
            w.now = e.at;
            match e.task {
                Task::Call(f) => f(w),
                Task::Tick(d, g) => w.tick_due(d, g),
                _ => panic!("a link event: this test runs closures and Ticks only"),
            }
        }
    }

    /// One Tick at next() (now if past); an unchanged next() schedules nothing new; a newer one
    /// replaces the pending Tick, which then does nothing; Mono::MAX cancels; asleep, nothing.
    #[test]
    fn tick_schedule() {
        let (mut w, _, next) = scripted();
        let d = DevId::Client;
        w[d].sess = Some((w.mk)(Role::Client, 1, Box::new(|_: &mut [u8]| {})));
        let off = w[d].off();
        w.sched_tick(d);
        assert!(w.q.is_empty() && !w[d].tick_set);
        next[0].set(off + 5 * SEC);
        w.sched_tick(d);
        w.sched_tick(d);
        assert_eq!(queued(&w), [5 * SEC]);
        assert!(w[d].tick_set && w[d].tick_at == 5 * SEC);
        next[0].set(off + 3 * SEC);
        w.sched_tick(d);
        assert_eq!(queued(&w), [3 * SEC, 5 * SEC]);
        let pending = w[d].tick;
        next[0].set(Mono::MAX);
        w.sched_tick(d);
        assert!(!w[d].tick_set && w[d].tick == pending + 1);
        run_calls(&mut w, 5 * SEC); // both are stale: they run nothing
        assert!(!w[d].tick_set && w[d].same == 0);
        next[0].set(off + SEC); // in the past
        w.sched_tick(d);
        assert_eq!(queued(&w), [5 * SEC]);
        w.q.clear();
        w[d].asleep = true;
        w.sched_tick(d);
        assert!(w.q.is_empty());
    }

    // ------------------------------------------------------------------------------------------
    // End to end: the stand-in with scripted endpoints over the link and NAT models.

    /// The owner's topology without the Recorder: the Mac's hotel Wi-Fi (default route, 30 ms
    /// each way) and iPhone (metered, 40 ms) behind easy NATs, xanth's Ethernet (1 ms) behind the
    /// router's UDP 7849 forward; seed 1. The checker is off: these tests check the stand-in, and
    /// the scripted endpoint sends data on lanes no checker would call validated.
    fn topology() -> (World, Log, Next, [LinkId; 3]) {
        topology_seed(1)
    }

    fn topology_seed(seed: u64) -> (World, Log, Next, [LinkId; 3]) {
        let (mut w, log, next) = scripted_seed(seed);
        w.check = false;
        let hotel = w.add_link(
            DevId::Client,
            LinkSpec {
                label: "Wi-Fi",
                default: true,
                up: delay(30 * MS),
                down: delay(30 * MS),
                nat: NatKind::Easy,
                local: ADDR_HOTEL,
                public: PUB_HOTEL,
                ..LinkSpec::default()
            },
        );
        let cell = w.add_link(
            DevId::Client,
            LinkSpec {
                label: "iPhone",
                metered: true,
                up: delay(40 * MS),
                down: delay(40 * MS),
                nat: NatKind::Easy,
                local: ADDR_CELL,
                public: PUB_CELL,
                ..LinkSpec::default()
            },
        );
        let eth = w.add_link(
            DevId::Server,
            LinkSpec {
                label: "Ethernet",
                up: delay(MS),
                down: delay(MS),
                local: ADDR_HOME,
                public: PUB_HOME,
                ..LinkSpec::default()
            },
        );
        (w, log, next, [hotel, cell, eth])
    }

    /// The topology started and run to 500 ms: the channel is up and the keys exchanged.
    fn connected() -> (World, Log, Next, [LinkId; 3]) {
        let (mut w, log, next, links) = topology();
        w.start();
        w.run(500 * MS).unwrap();
        (w, log, next, links)
    }

    /// The control messages of type t that session `name` got.
    fn msgs(log: &Log, name: &str, t: &str) -> Vec<Msg> {
        let ss = seen(log, name).into_iter();
        ss.filter_map(|s| match s {
            Seen::Control(m) if m.t == t => Some(*m),
            _ => None,
        })
        .collect()
    }

    /// Sends one registered datagram on stream s from the client's program on port, source src.
    fn send(w: &mut World, s: crate::apps::StreamId, src: u16) {
        let b = w.datagram(s, 0, 0, 64, w.now());
        let port = w[s].port;
        w.send_from(DevId::Client, port, src, b);
    }

    /// One seed replays the same run, the same event-log hash; another seed does not (§17.3's
    /// double-run check, Rust against Rust).
    #[test]
    fn same_seed_same_run() {
        let run = |seed| {
            let (mut w, _, _, [_, cell, _]) = topology_seed(seed);
            w.start();
            w.run(SEC).unwrap();
            let a = w.stream("a", 9000, false, None);
            for src in 1..=3 {
                send(&mut w, a, src);
            }
            w.set_local(cell, "172.20.10.3:7849".parse().unwrap());
            w.restart(DevId::Server, SEC);
            w.sleep(DevId::Client, 2 * SEC, false);
            w.run(6 * SEC).unwrap();
            assert!(w.control_up() && w[DevId::Server].sessions == 2);
            w.hash()
        };
        let one = run(1);
        assert_eq!(run(1), one);
        assert_ne!(run(2), one);
    }

    /// The first connect: STUN maps every link through its NAT; each Session gets its links, the
    /// client's mode, the peer's hello and its key, in that order; a mapping that came after the
    /// hello left reaches the peer as links.
    #[test]
    fn connect_sends_hello_both_ways() {
        let (w, log, _, [hotel, cell, eth]) = connected();
        let (cl, sv) = (&w[DevId::Client], &w[DevId::Server]);
        assert!(w.control_up());
        assert_eq!((cl.sessions, sv.sessions, cl.keys.len(), sv.keys.len()), (1, 1, 1, 1));
        assert_eq!((cl.keys[0].rid, sv.keys[0].rid), (1, 1));
        assert_eq!(w[hotel].mapped.map(|a| a.ip()), Some(PUB_HOTEL.into()));
        assert_eq!(w[cell].mapped.map(|a| a.ip()), Some(PUB_CELL.into()));
        let home = SocketAddr::new(PUB_HOME.into(), PORT);
        assert_eq!(w[eth].mapped, Some(home), "the forward keeps the port");
        let mac = seen(&log, "mac#1");
        assert_eq!(kinds(&mac), ["Links", "mode", "hello", "key"]);
        let Seen::Links(ls) = &mac[0] else { unreachable!() };
        let got: Vec<_> = ls.iter().map(|l| (l.label.as_str(), l.mapped)).collect();
        assert_eq!(got, [("Wi-Fi", w[hotel].mapped), ("iPhone", w[cell].mapped)]);
        let h = &msgs(&log, "mac#1", "hello")[0];
        assert_eq!(h.inst, sv.inst);
        assert_eq!(h.links[0].cands, [ADDR_HOME, home]);
        // The client's hello left at 62 ms, between the hotel's mapping (60 ms) and the iPhone's
        // (80 ms), which followed as links.
        assert_eq!(kinds(&seen(&log, "xanth#1")), ["Links", "hello", "links", "key"]);
        let h = &msgs(&log, "xanth#1", "hello")[0];
        assert_eq!((h.inst, h.mode.as_str()), (cl.inst, MODE_PERFORMANCE));
        assert_eq!(h.links[0].cands, cands(ADDR_HOTEL, w[hotel].mapped));
        assert_eq!(h.links[1].cands, [ADDR_CELL]);
        let l = &msgs(&log, "xanth#1", "links")[0];
        assert_eq!(l.links[1].cands, cands(ADDR_CELL, w[cell].mapped));
    }

    /// A cut drops the messages in flight; the reconnect sends hello both ways again, to the
    /// same Sessions (the same inst both ways), whose keys follow again.
    #[test]
    fn reconnect_resends_hello_to_the_same_sessions() {
        let (mut w, log, _, _) = connected();
        w.run(980 * MS).unwrap();
        w.set_mode(DevId::Client, MODE_BANDWIDTH); // still in flight at 1 s
        w.run(SEC).unwrap();
        w.cut_control();
        w.run(2 * SEC).unwrap();
        assert!(!w.control_up(), "a cut channel stays down");
        w.restore_control();
        w.run(3 * SEC).unwrap();
        let (cl, sv) = (&w[DevId::Client], &w[DevId::Server]);
        assert!(w.control_up());
        assert_eq!((cl.sessions, sv.sessions, cl.keys.len(), sv.keys.len()), (1, 1, 2, 2));
        let kind = ["Links", "mode", "hello", "key", "mode", "hello", "key"];
        assert_eq!(kinds(&seen(&log, "mac#1")), kind);
        let kind = ["Links", "hello", "links", "key", "hello", "key"];
        assert_eq!(kinds(&seen(&log, "xanth#1")), kind);
        let h = &msgs(&log, "xanth#1", "hello")[1];
        assert_eq!((h.inst, h.mode.as_str()), (cl.inst, MODE_BANDWIDTH));
    }

    /// A message of a connection that dropped never arrives on the next one, even when it would
    /// arrive after the reconnect (it waited for a slow path).
    #[test]
    fn old_connection_messages_are_dropped() {
        let (mut w, log, _, [hotel, ..]) = connected();
        w[hotel.up()].p.delay = 300 * MS;
        w.set_mode(DevId::Client, "lost"); // would arrive at 0.801 s
        w[hotel.up()].p.delay = 30 * MS;
        w.run(510 * MS).unwrap();
        w.cut_control();
        w.restore_control(); // up again at 0.572 s
        w.run(SEC).unwrap();
        assert!(w.control_up());
        assert!(msgs(&log, "xanth#1", "mode").is_empty());
        assert_eq!(msgs(&log, "xanth#1", "hello").len(), 2);
    }

    /// A STUN reply later than STUN_WAIT after its request is not taken (§6.3 item 1).
    #[test]
    fn late_stun_reply_is_ignored() {
        let (mut w, _, _, [hotel, ..]) = topology();
        let slow = w.add_link(
            DevId::Client,
            LinkSpec {
                label: "Satellite",
                up: delay(2 * SEC),
                down: delay(2 * SEC),
                nat: NatKind::Easy,
                local: "192.168.2.20:7849".parse().unwrap(),
                public: Ipv4Addr::new(198, 51, 100, 40),
                ..LinkSpec::default()
            },
        );
        w.start();
        w.run(5 * SEC).unwrap();
        assert!(w[hotel].mapped.is_some() && w[slow].mapped.is_none());
        assert_eq!(w[slow.down()].pkts, 1, "the reply came 4 s after its request");
    }

    /// Control messages keep their order with the mesh's delay, even when the path gets faster
    /// between two of them.
    #[test]
    fn control_messages_arrive_in_order() {
        let (mut w, log, _, [hotel, ..]) = connected();
        let t = w.now();
        w.set_mode(DevId::Client, "first");
        w[hotel.up()].p.delay = MS;
        w.set_mode(DevId::Client, "second");
        let modes = |log: &Log| -> Vec<String> {
            let ms = msgs(log, "xanth#1", "mode");
            ms.into_iter().map(|m| m.mode).collect()
        };
        w.run(t + 30 * MS).unwrap();
        assert!(modes(&log).is_empty());
        w.run(t + 31 * MS).unwrap();
        assert_eq!(modes(&log), ["first", "second"]);
    }

    /// A restart ends the Session and comes back with a new inst and rid table; the reconnect
    /// makes new Sessions at both ends (§7.6 rule 8), which exchange new keys.
    #[test]
    fn restart_makes_new_sessions() {
        let (mut w, log, _, _) = connected();
        let inst = w[DevId::Server].inst;
        w.run(SEC).unwrap();
        w.restart(DevId::Server, SEC);
        assert!(!w.control_up() && !w[DevId::Server].awake());
        assert_eq!(kinds(&seen(&log, "xanth#1")).last(), Some(&"Close"));
        w.run(3 * SEC).unwrap();
        let (cl, sv) = (&w[DevId::Client], &w[DevId::Server]);
        assert!(w.control_up() && sv.inst != inst && sv.inst & 1 == 1);
        assert_eq!((cl.sessions, sv.sessions, cl.keys.len(), sv.keys.len()), (2, 2, 2, 2));
        assert_eq!((cl.peer_inst, sv.peer_inst), (sv.inst, cl.inst));
        assert_eq!(kinds(&seen(&log, "mac#1")).last(), Some(&"Close"));
        assert_eq!(kinds(&seen(&log, "mac#2")), ["Links", "mode", "hello", "key"]);
        assert_eq!(kinds(&seen(&log, "xanth#2")), ["Links", "hello", "key"]);
    }

    /// A sleep stops everything and drops the channel; on wake the device gets Wake, runs STUN
    /// and redials; its clock paused through the sleep, or counted it.
    #[test]
    fn sleep_then_wake_stun_and_redial() {
        for advance in [false, true] {
            let (mut w, log, _, [hotel, ..]) = connected();
            let off = w[DevId::Client].off();
            w.run(SEC).unwrap();
            w.sleep(DevId::Client, 5 * SEC, advance);
            w.run(5 * SEC).unwrap();
            assert!(!w.control_up() && !w[DevId::Client].awake());
            let pkts = w[hotel.up()].pkts;
            w.run(7 * SEC).unwrap();
            assert!(w.control_up() && w[DevId::Client].awake());
            let paused = off - w[DevId::Client].off();
            assert_eq!(paused, if advance { 0 } else { 5 * SEC }, "advance {advance}");
            assert_eq!(w[hotel.up()].pkts, pkts + 1, "one STUN request on wake");
            let kind = ["Links", "mode", "hello", "key", "Wake", "hello", "key"];
            assert_eq!(kinds(&seen(&log, "mac#1")), kind);
            assert_eq!(w[DevId::Client].sessions, 1);
        }
    }

    /// The client numbers flows (port, source) 1, 2, 3 … per session; the server opens a flow on
    /// its first datagram; each end ends a flow at its first housekeeping FLOW_IDLE after the
    /// flow's last datagram, in flow order; MAX_FLOWS live flows drop a new one.
    #[test]
    #[ignore = "2.0 wave 2: the engine now ends idle flows itself (FLOW_IDLE); netsim sends no FlowEnd (SPEC 13)"]
    fn flows_numbered_ended_and_capped() {
        let (mut w, log, _, _) = connected();
        let a = w.stream("a", 9000, false, None);
        let b = w.stream("b", 9001, false, None);
        send(&mut w, a, 1);
        send(&mut w, a, 1);
        send(&mut w, a, 2);
        send(&mut w, b, 1);
        w.run(SEC).unwrap();
        let fs = [FlowId::new(9000, 1), FlowId::new(9000, 2), FlowId::new(9001, 3)];
        for d in DevId::BOTH {
            assert_eq!(w[d].flows.keys().copied().collect::<Vec<_>>(), fs);
        }
        assert_eq!((w[a].received, w[b].received), (3, 1));
        let r = w.datagram(b, 1, 0, 64, w.now());
        w.send_on(DevId::Server, fs[2], r); // the reply keeps 9001's flow alive at both ends
        let r = w.datagram(b, 2, 0, 64, w.now());
        w.send_on(DevId::Server, FlowId::new(9001, 9), r); // no such flow: dropped
        w.run(2 * SEC).unwrap();
        assert_eq!(w[b].received, 2);
        // The client sent 9000's at 0.5 s and got 9001's reply at 1.03 s: its sweeps at 61 s and
        // 62 s end them; xanth got 9000's at 0.53 s and answered 9001 at 1 s: all at 61 s.
        w.run(61 * SEC).unwrap();
        let ends = |name| {
            let ss = seen(&log, name).into_iter();
            ss.filter_map(|s| match s {
                Seen::FlowEnd(f) => Some(f),
                _ => None,
            })
            .collect::<Vec<_>>()
        };
        assert_eq!((ends("mac#1"), ends("xanth#1")), (fs[..2].to_vec(), fs.to_vec()));
        w.run(62 * SEC).unwrap();
        assert_eq!(ends("mac#1"), fs);
        assert!(w[DevId::Client].flows.is_empty() && w[DevId::Client].by_app.is_empty());
        assert!(w[DevId::Server].flows.is_empty());
        for src in 1..=33 {
            send(&mut w, a, src);
        }
        let cl = &w[DevId::Client];
        let want: Vec<_> = (4..=35).map(|n| FlowId::new(9000, n)).collect();
        assert_eq!(cl.flows.keys().copied().collect::<Vec<_>>(), want);
        assert_eq!((cl.dropped, cl.by_app.len()), (1, MAX_FLOWS));
    }

    /// A STUN reply with a new mapping is a link change: the Session gets Links with it and the
    /// peer a links message. The first datagram after idle runs STUN (§6.3, §8.1).
    #[test]
    fn new_mapping_is_a_link_change() {
        let (mut w, log, _, [hotel, ..]) = connected();
        let old = w[hotel].mapped;
        w[hotel].nat.rebind(false);
        let a = w.stream("a", 9000, false, None);
        send(&mut w, a, 1);
        w.run(SEC).unwrap();
        let new = w[hotel].mapped;
        assert!(new != old && new.map(|m| m.ip()) == Some(PUB_HOTEL.into()));
        let mac = seen(&log, "mac#1");
        let Some(Seen::Links(ls)) = mac.iter().rfind(|s| s.kind() == "Links") else {
            panic!("no links event");
        };
        assert_eq!(ls[0].mapped, new);
        let l = msgs(&log, "xanth#1", "links").pop().unwrap();
        assert_eq!(l.links[0].cands, cands(ADDR_HOTEL, new));
    }

    /// set_local moves a link and its NAT to a new address: the Session and the peer hear at
    /// once, and STUN then finds the new mapping.
    #[test]
    fn set_local_moves_the_link() {
        let (mut w, log, _, [_, cell, _]) = connected();
        let old = w[cell].mapped;
        let a: SocketAddr = "172.20.10.3:7849".parse().unwrap();
        w.set_local(cell, a);
        assert_eq!(w[cell].nat.local, a.ip());
        let Some(Seen::Links(ls)) = seen(&log, "mac#1").pop() else {
            panic!("no links event");
        };
        assert_eq!((ls[1].local, ls[1].mapped), (a, None), "the old socket's mapping goes");
        w.run(SEC).unwrap();
        let new = w[cell].mapped;
        assert!(new.is_some_and(|m| m.ip() == IpAddr::V4(PUB_CELL)) && new != old);
        let ls = msgs(&log, "xanth#1", "links");
        assert_eq!(ls[ls.len() - 2].links[1].cands, [a]);
        assert_eq!(ls[ls.len() - 1].links[1].cands, cands(a, new));
    }

    /// An endpoint whose next() is always due.
    struct Stuck;

    impl Endpoint for Stuck {
        fn handle(&mut self, _: Mono, _: Event<'_>, _: &mut RidTable, _: &mut dyn Out) {}
        fn next(&self) -> Mono {
            0
        }
        fn snapshot(&self) -> SessionStatus {
            SessionStatus::default()
        }
        fn close(&mut self, _: &mut RidTable) {}
    }

    /// Ticks come at next(): a newer deadline replaces the pending one, Mono::MAX cancels it, and
    /// a Session that asks for one moment 100,000 times fails the run.
    #[test]
    fn ticks_follow_next() {
        let (mut w, log, next, _) = connected();
        let d = DevId::Client;
        let off = w[d].off();
        let at = |w: &mut World, t: Mono| {
            next[0].set(t);
            w.set_mode(d, MODE_PERFORMANCE); // after every handle call the stand-in reads next()
        };
        let ticks = |log: &Log| {
            let ss = seen(log, "mac#1").into_iter();
            ss.filter_map(|s| match s {
                Seen::Tick(t) => Some(t - off),
                _ => None,
            })
            .collect::<Vec<_>>()
        };
        at(&mut w, off + 700 * MS);
        at(&mut w, off + 600 * MS);
        w.run(SEC).unwrap();
        assert_eq!(ticks(&log), [600 * MS]);
        at(&mut w, off + 1200 * MS);
        at(&mut w, Mono::MAX);
        at(&mut w, off); // past: now
        w.run(2 * SEC).unwrap();
        assert_eq!(ticks(&log), [600 * MS, SEC]);

        let (mut w, _, _) = scripted();
        w.check = false;
        w[DevId::Server].sess = Some(Box::new(Stuck));
        w.sched_tick(DevId::Server);
        let err = w.run(SEC).unwrap_err();
        assert!(err.contains("xanth: next() asked for a Tick at 0.000s 100,000 times"), "{err}");
    }

    /// Lane packets ride the client's default link on the Tailscale lane (Up from the client,
    /// Down from the server, with Tailscale's overhead) unless the mesh is down; direct packets
    /// pass the NATs, which let in only what they mapped; the hooks see each step.
    #[test]
    fn routing_through_links_and_nats() {
        let (mut w, log, _, [hotel, cell, eth]) = connected();
        let hooks = Rc::new(RefCell::new(Vec::new()));
        let h = hooks.clone();
        w.hook(move |_, a| {
            let ev = a.event.map(|e| event_code(&e));
            h.borrow_mut().push((a.kind, a.dev, a.link, ev));
        });
        let a = w.stream("a", 9000, false, None);
        send(&mut w, a, 1); // the first after idle also runs STUN
        w.run(SEC).unwrap();
        let (bytes, pkts) = (w[hotel.up()].bytes, w[hotel.up()].pkts);
        send(&mut w, a, 1);
        w.run(2 * SEC).unwrap();
        // 15 + the inner packet (IPv6/UDP and 64) + 16 bytes, IP and UDP, one fragment at
        // MESH_MTU, and Tailscale's overhead.
        let n = 95 + crate::sim::inner_hdr(9000);
        assert_eq!(w[hotel.up()].bytes - bytes, (n + IP_UDP + TS_OVER) as i64);
        assert_eq!(w[hotel.up()].pkts - pkts, 1);
        let f = FlowId::new(9000, 1);
        let r = w.datagram(a, 1, 0, 64, w.now());
        let (up, down) = (w[hotel.up()].bytes, w[hotel.down()].bytes);
        w.send_on(DevId::Server, f, r);
        w.run(3 * SEC).unwrap();
        assert_eq!(w[a].received, 3);
        let sent = (w[hotel.up()].bytes - up, w[hotel.down()].bytes - down);
        assert_eq!(sent, (0, (n + IP_UDP + TS_OVER) as i64), "xanth's ride the hotel's Down");
        use HookKind::{Delivered, Inbound, Received, Sent};
        let (c, s) = (DevId::Client, DevId::Server);
        let want = [
            (Sent, c, None, Some(1)),
            (Received, s, None, None),
            (Delivered, s, None, None),
            (Sent, c, None, Some(1)),
            (Received, s, None, None),
            (Delivered, s, None, None),
            (Sent, s, None, Some(1)),
            (Inbound, s, None, None),
            (Received, c, None, None),
            (Delivered, c, None, None),
        ];
        assert_eq!(*hooks.borrow(), want);

        w.mesh_down = true;
        let bytes = w[hotel.up()].bytes;
        send(&mut w, a, 1);
        w.run(4 * SEC).unwrap();
        assert_eq!((w[hotel.up()].bytes, w[a].received), (bytes, 3));
        w.mesh_down = false;

        // Direct: from the hotel's socket to xanth's forward, and back through the hotel's NAT,
        // which maps the socket once (easy) and lets in only what it sent to.
        hooks.borrow_mut().clear();
        let home = SocketAddr::new(PUB_HOME.into(), PORT);
        let probe = lane(TYPE_PROBE, 99, FlowId(0), &[0; 8]);
        let sock = w[hotel].sock;
        w.route(DevId::Client, sock, home, probe.clone(), false);
        w.run(5 * SEC).unwrap();
        let mapped = w[hotel].mapped.unwrap();
        let last = seen(&log, "xanth#1").pop();
        assert_eq!(last, Some(Seen::Packet(w[eth].sock, mapped)));
        let (sock, cell_pkts) = (w[eth].sock, w[cell.down()].pkts);
        w.route(DevId::Server, sock, mapped, probe.clone(), false);
        w.route(DevId::Server, sock, w[cell].mapped.unwrap(), probe, false);
        w.run(6 * SEC).unwrap();
        let last = seen(&log, "mac#1").into_iter().rfind(|s| s.kind() == "Packet");
        assert_eq!(last, Some(Seen::Packet(w[hotel].sock, home)));
        assert_eq!(w[cell.down()].pkts, cell_pkts, "the iPhone's NAT never sent to xanth");
        let want = [(Received, s, Some(hotel), None), (Inbound, s, Some(hotel), None), (Received, c, Some(eth), None)];
        assert_eq!(*hooks.borrow(), want);
    }

    /// A scenario's own Tailscale path (`mesh_path`, the system harness's tailscale0) carries the
    /// lane packets both ways without Tailscale's overhead, the default link none of them, and
    /// the control channel takes its delay. It is no device's link, so core never sees it.
    #[test]
    fn mesh_path_carries_the_tailscale_lane() {
        let (mut w, _, _, [hotel, _, _]) = topology();
        let m = w.mesh_path(delay(31 * MS));
        assert_eq!(w.mesh_link, Some(m));
        assert_eq!((w.ctl_delay(0), w.ctl_delay(1)), (31 * MS, 31 * MS));
        assert!(DevId::BOTH.iter().all(|&d| !w[d].links.contains(&m)));
        w.start();
        w.run(500 * MS).unwrap();
        let a = w.stream("a", 9000, false, None);
        send(&mut w, a, 1); // the first after idle also runs STUN, on the hotel
        w.run(SEC).unwrap();
        let (hotel_up, mesh_up) = (w[hotel.up()].bytes, w[m.up()].bytes);
        send(&mut w, a, 1);
        w.run(2 * SEC).unwrap();
        assert_eq!(w[hotel.up()].bytes, hotel_up, "nothing on the hotel");
        let n = 95 + crate::sim::inner_hdr(9000); // 15 + the inner packet + 16
        assert_eq!(w[m.up()].bytes - mesh_up, (n + IP_UDP) as i64);
        let r = w.datagram(a, 1, 0, 64, w.now());
        let (hotel_down, mesh_down) = (w[hotel.down()].bytes, w[m.down()].bytes);
        w.send_on(DevId::Server, FlowId::new(9000, 1), r);
        w.run(3 * SEC).unwrap();
        assert_eq!(w[a].received, 3);
        assert_eq!(w[hotel.down()].bytes, hotel_down, "nothing on the hotel");
        assert_eq!(w[m.down()].bytes - mesh_down, (n + IP_UDP) as i64, "xanth's ride its Down");
    }

    /// With the channel down, a Session ends once SESSION_LINGER has passed since it went down
    /// and since its last authenticated packet; the next connect makes new Sessions, even for
    /// the same processes.
    #[test]
    fn sessions_linger_then_close() {
        let (mut w, log, _, _) = connected();
        w.cut_control(); // at 0.5 s
        w.run(100 * SEC).unwrap();
        let a = w.stream("a", 9000, false, None);
        send(&mut w, a, 1); // reaches xanth at 100.03 s over the Tailscale lane
        w.run(SESSION_LINGER).unwrap();
        assert!(w[DevId::Client].sess.is_some());
        w.run(SESSION_LINGER + SEC).unwrap();
        assert!(w[DevId::Client].sess.is_none() && w[DevId::Server].sess.is_some());
        w.run(220 * SEC).unwrap();
        assert!(w[DevId::Server].sess.is_some());
        w.run(221 * SEC).unwrap();
        assert!(w[DevId::Server].sess.is_none());
        w.restore_control();
        w.run(222 * SEC).unwrap();
        let (cl, sv) = (&w[DevId::Client], &w[DevId::Server]);
        assert_eq!((cl.sessions, sv.sessions), (2, 2));
        assert_eq!(kinds(&seen(&log, "mac#1")).last(), Some(&"Close"));
        assert_eq!(kinds(&seen(&log, "xanth#1")).last(), Some(&"Close"));
        assert_eq!(kinds(&seen(&log, "mac#2")), ["Links", "mode", "hello", "key"]);
    }
}
