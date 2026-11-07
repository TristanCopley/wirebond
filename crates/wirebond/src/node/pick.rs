//! `wirebond pick`'s selection phase (SPEC 2.0 §18): each candidate's measurements, its score in
//! the bond's mode, and the winner. Performance: the p99 of the across-lane minimum OWD plus half
//! the candidate's egress time, lowest wins. Bandwidth: the sum of the eligible lanes' top rates,
//! highest wins, ties within 5% going to the lower OWD p50. A candidate with no validated lane, or
//! with every lane in one group, scores last and says why. Owner: node daemon.
//!
//! The measure phase (§18 steps 1–4) runs over a temporary bond with no capture: the client sends
//! `bond open` with `measure`, which the anchor keeps beside the client's own bond, then
//! `measure start`. Both sides sample every lane every PROBE_ACTIVE for half the time, a small
//! packet every KEEP_ACTIVE keeping both sessions probing; in bandwidth mode the anchor then pushes
//! paced filler on every lane in parallel, ramping until overuse, at most PICK_BULK_BYTES, and the
//! client records each lane's top delivery rate. The anchor answers `measure result` (its lanes
//! and its egress, measured meanwhile) at the end of the window.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use wirebond_core::{self as core, MS, Mode, Mono, PICK_BULK_BYTES, PROBE_ACTIVE, RATE_WINDOW, SEC, TUN_MTU};

use super::proto::{self, MeasureLane};
use super::{Bond, BondState, Daemon, Egress, ErrorCode, Msg, PICK, Role, bond, ctl, link};
use crate::api::PickRow;

/// One candidate's measurements.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measured {
    pub device: String,
    /// Its validated lanes, each with its bottleneck group.
    pub lanes: Vec<(MeasureLane, u8)>,
    /// The p99 of the per-probe minimum OWD across lanes in different groups, ms.
    pub min_owd_p99: f64,
    /// The candidate's own egress: median TCP connect time to the references, ms.
    pub egress_ms: f64,
    /// Why it could not be measured: unreachable, refused.
    pub failed: Option<String>,
}

impl Measured {
    fn groups(&self) -> u32 {
        let mut g: Vec<u8> = self.lanes.iter().map(|&(_, g)| g).collect();
        g.sort_unstable();
        g.dedup();
        g.len() as u32
    }

    /// Why it scores last, if it does.
    fn last(&self) -> Option<String> {
        self.failed.clone().or_else(|| match self.groups() {
            0 => Some("no validated lane".into()),
            1 => Some("every lane in one group".into()),
            _ => None,
        })
    }

    /// Its score in `mode`; None when it scores last.
    pub fn score(&self, mode: Mode) -> Option<f64> {
        if self.last().is_some() {
            return None;
        }
        Some(match mode {
            Mode::Performance => self.min_owd_p99 + self.egress_ms / 2.0,
            Mode::Bandwidth => self.lanes.iter().map(|(l, _)| l.mbps).sum(),
        })
    }

    /// The lowest OWD p50 of its lanes, for bandwidth's ties (RTT p50 / 2).
    fn owd_p50(&self) -> f64 {
        self.lanes.iter().map(|(l, _)| l.rtt_p50 / 2.0).fold(f64::INFINITY, f64::min)
    }
}

/// The index of the winner in `cands`, None when every candidate scores last.
pub fn select(mode: Mode, cands: &[Measured]) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, c) in cands.iter().enumerate() {
        let Some(s) = c.score(mode) else { continue };
        best = match best {
            None => Some((i, s)),
            Some((j, b)) => {
                let better = match mode {
                    Mode::Performance => s < b,
                    Mode::Bandwidth if (s - b).abs() <= 0.05 * s.max(b) => c.owd_p50() < cands[j].owd_p50(),
                    Mode::Bandwidth => s > b,
                };
                if better { Some((i, s)) } else { Some((j, b)) }
            }
        };
    }
    best.map(|(i, _)| i)
}

/// The rows `pick` prints: one per candidate, its lanes, groups and score, or why it is last.
pub fn rows(mode: Mode, cands: &[Measured]) -> Vec<PickRow> {
    cands
        .iter()
        .map(|c| PickRow {
            device: c.device.clone(),
            lanes: c.lanes.len() as u32,
            groups: c.groups(),
            score: c.score(mode),
            reason: c.last().unwrap_or_default(),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The measure phase (§18 steps 1–4), on both sides of the temporary bond.

/// Filler packets handed to the engine per loop turn at most.
const FILLER_BURST: usize = 256;
/// One filler packet's payload: a full inner packet's worth.
const FILLER_LEN: usize = 1200;
/// The filler's offered rate starts here and doubles every FILLER_STEP, at most twice what the
/// session sent over the last RATE_WINDOW: the engine's rate control ramps each lane until overuse
/// (§12), and little of the budget goes on packets the engine would drop.
const FILLER_START: f64 = 8e6;
const FILLER_STEP: Mono = 250 * MS;
const FILLER_MAX: f64 = 4e9;
/// How often the client sends a small packet while probing, so both sessions stay active and
/// probe every lane every PROBE_ACTIVE (§11.1).
const KEEP_ACTIVE: Mono = 100 * MS;
/// How long past `seconds` a candidate may take before it fails: the dial, hello, bond ok and its
/// answer.
const GRACE: Mono = 10 * SEC;

/// One sample of one lane: its RTT, my direction's queueing delay above its base, its loss.
#[derive(Clone, Copy, Debug)]
struct Sample {
    id: u16,
    rtt: f64,
    qd: f64,
    loss: f64,
}

/// The filler the anchor pushes in bandwidth mode's second half: the budget left, the pacing
/// credit, and the session's bytes sent over the last RATE_WINDOW.
#[derive(Clone, Debug)]
struct Push {
    left: u64,
    credit: f64,
    last: Mono,
    sent: VecDeque<(Mono, u64)>,
}

/// One side's measurement of a temporary bond: every lane sampled every PROBE_ACTIVE for the first
/// half; in bandwidth mode's second half, the anchor pushes paced filler and the client records
/// each lane's top delivery rate over RATE_WINDOW.
#[derive(Debug)]
pub struct Meter {
    half: Mono,
    end: Mono,
    next: Mono,
    next_keep: Mono,
    /// Per sample time, the validated lanes that were up or suspect.
    ticks: Vec<Vec<Sample>>,
    /// The client in bandwidth mode: each lane's received bytes over the last RATE_WINDOW, and its
    /// top rate (Mbit/s).
    rates: bool,
    rx: HashMap<u16, VecDeque<(Mono, u64)>>,
    top: HashMap<u16, f64>,
    push: Option<Push>,
    /// The anchor: its egress once measured (§18 step 4), and whether it answered.
    pub egress_ms: Option<f64>,
    pub answered: bool,
}

impl Meter {
    /// A measurement from `now` for `seconds`; `push` filler bytes (the anchor in bandwidth
    /// mode), or record top rates (`rates`, the client in bandwidth mode).
    pub fn new(now: Mono, seconds: u32, push: Option<u64>, rates: bool) -> Meter {
        let t = Mono::from(seconds.max(1)) * SEC;
        Meter {
            half: now + t / 2,
            end: now + t,
            next: now,
            next_keep: now,
            ticks: Vec::new(),
            rates,
            rx: HashMap::new(),
            top: HashMap::new(),
            push: push.map(|left| Push { left, credit: 0.0, last: now + t / 2, sent: VecDeque::new() }),
            egress_ms: None,
            answered: false,
        }
    }

    /// Whether the window is over.
    pub fn done(&self, now: Mono) -> bool {
        now >= self.end
    }

    /// Samples the session's lanes when due: the first half's probes, the second half's rates.
    fn sample(&mut self, now: Mono, snap: &core::SessionStatus) {
        if now < self.half && now >= self.next {
            self.next = now + PROBE_ACTIVE;
            let tick: Vec<Sample> = snap
                .lanes
                .iter()
                .filter(|l| bond::lane_carries(l))
                .map(|l| Sample { id: l.id, rtt: l.out.rtt_ms, qd: l.out.queue_ms, loss: l.out.loss })
                .collect();
            if !tick.is_empty() {
                self.ticks.push(tick);
            }
        }
        if self.rates && now >= self.half && now < self.end {
            for l in snap.lanes.iter().filter(|l| l.validated) {
                let h = self.rx.entry(l.id).or_default();
                h.push_back((now, l.rx_bytes));
                while h.len() > 2 && h[1].0 <= now - RATE_WINDOW {
                    h.pop_front();
                }
                let (t0, b0) = h[0];
                if now - t0 >= RATE_WINDOW {
                    let mbps = l.rx_bytes.saturating_sub(b0) as f64 * 8.0 / ((now - t0) as f64 / SEC as f64) / 1e6;
                    let top = self.top.entry(l.id).or_insert(0.0);
                    *top = top.max(mbps);
                }
            }
        }
    }

    /// The filler packets due at `now` (the anchor in bandwidth mode's second half), within what
    /// is left of the budget; `sent` is the session's bytes sent so far.
    fn filler(&mut self, now: Mono, sent: u64) -> usize {
        let Some(p) = self.push.as_mut().filter(|_| now >= self.half && now < self.end) else { return 0 };
        p.sent.push_back((now, sent));
        while p.sent.len() > 2 && p.sent[1].0 <= now - RATE_WINDOW {
            p.sent.pop_front();
        }
        let (t0, b0) = p.sent[0];
        let sending = if now - t0 >= RATE_WINDOW / 2 {
            sent.saturating_sub(b0) as f64 * 8.0 / ((now - t0) as f64 / SEC as f64)
        } else {
            0.0
        };
        let steps = ((now - self.half) / FILLER_STEP).min(40) as i32;
        let rate = (FILLER_START * 2f64.powi(steps)).min(FILLER_MAX).min((2.0 * sending).max(FILLER_START));
        p.credit = (p.credit + rate / 8.0 * (now - p.last) as f64 / SEC as f64).min(rate / 8.0 * 0.05);
        p.last = now;
        let size = (FILLER_LEN + 28) as u64;
        let mut n = 0;
        while n < FILLER_BURST && p.credit >= size as f64 && p.left >= size {
            (p.credit, p.left, n) = (p.credit - size as f64, p.left - size, n + 1);
        }
        n
    }

    /// Whether a small packet is due to keep both sessions active while probing.
    fn keep(&mut self, now: Mono) -> bool {
        let due = now < self.half && now >= self.next_keep;
        if due {
            self.next_keep = now + KEEP_ACTIVE;
        }
        due
    }

    /// Per lane: RTT p50, OWD p99, dev (the median absolute deviation of its OWD) and loss, and
    /// its top rate; and the p99 of the per-sample minimum OWD across the lanes. A lane's OWD in a
    /// sample is half its smallest RTT plus my direction's queueing delay then (§11.3: only OWD
    /// differences are measured, so the base is split evenly).
    fn result(&self) -> (Vec<MeasureLane>, f64) {
        let mut ids: Vec<u16> = self.ticks.iter().flatten().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        let base = |id: u16| {
            self.ticks
                .iter()
                .flatten()
                .filter(|s| s.id == id && s.rtt > 0.0)
                .map(|s| s.rtt)
                .fold(f64::INFINITY, f64::min)
                / 2.0
        };
        let bases: HashMap<u16, f64> = ids.iter().map(|&id| (id, base(id))).collect();
        let owd = |s: &Sample| bases.get(&s.id).copied().filter(|b| b.is_finite()).unwrap_or(s.rtt / 2.0) + s.qd;
        let lanes = ids
            .iter()
            .map(|&id| {
                let mine: Vec<&Sample> = self.ticks.iter().flatten().filter(|s| s.id == id).collect();
                let mut owds: Vec<f64> = mine.iter().map(|s| owd(s)).collect();
                let mid = quantile(&mut owds, 0.5);
                let mut devs: Vec<f64> = owds.iter().map(|o| (o - mid).abs()).collect();
                MeasureLane {
                    id,
                    rtt_p50: quantile(&mut mine.iter().map(|s| s.rtt).collect::<Vec<_>>(), 0.5),
                    owd_p99: quantile(&mut owds, 0.99),
                    dev: quantile(&mut devs, 0.5),
                    loss: mine.iter().map(|s| s.loss).sum::<f64>() / mine.len().max(1) as f64,
                    mbps: self.top.get(&id).copied().unwrap_or(0.0),
                }
            })
            .collect();
        let mut mins: Vec<f64> = self.ticks.iter().map(|t| t.iter().map(owd).fold(f64::INFINITY, f64::min)).collect();
        (lanes, quantile(&mut mins, 0.99))
    }
}

/// The `q` quantile of `v` (nearest rank); 0 for none.
fn quantile(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    match v.len() {
        0 => 0.0,
        n => v[((n as f64 * q).ceil() as usize).clamp(1, n) - 1],
    }
}

/// A filler packet: IPv4/UDP between two TEST-NET-1 addresses, which no bond address is, so the
/// receiving daemon counts it on its lane and drops it (§6.3, §10.5 rule 6).
fn filler_packet(len: usize, to_client: bool) -> Vec<u8> {
    let (src, dst) = if to_client { ([192, 0, 2, 1], [192, 0, 2, 2]) } else { ([192, 0, 2, 2], [192, 0, 2, 1]) };
    let total = (28 + len) as u16;
    let mut p = vec![0x45, 0];
    p.extend_from_slice(&total.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0x40, 0, 64, 17, 0, 0]);
    p.extend_from_slice(&src);
    p.extend_from_slice(&dst);
    p.extend_from_slice(&[0x1e, 0xa9, 0x1e, 0xa9]);
    p.extend_from_slice(&(total - 20).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.resize(usize::from(total), 0);
    p
}

impl Daemon {
    /// The candidate's `bond ok`: a temporary bond, no TUN device and no capture (§18 step 1); the
    /// measurement starts at once, and the candidate is told to measure too.
    pub(super) fn pick_bond_ok(&mut self, id: u64, addr4: Option<Ipv4Addr>, addr6: Option<Ipv6Addr>) {
        let now = self.now();
        let Some(peer) = self.chans.get(&id).map(|p| p.chan.peer.ip().to_canonical()) else { return };
        let inst = self.chans.get(&id).and_then(|p| p.hello.as_ref()).map_or(0, |h| h.inst);
        let Some(p) = self.pick.as_mut() else { return };
        let mut b = Bond::new(p.cands[p.i].clone(), peer, inst, p.mode, now);
        (b.addr4, b.addr6, b.chan, b.measure) = (addr4, addr6, Some(id), true);
        b.set_state(BondState::Punching, now);
        b.meter = Some(Meter::new(now, p.seconds, None, p.mode == Mode::Bandwidth));
        let start = Msg::Measure(proto::Measure::Start { seconds: p.seconds, bulk_bytes: PICK_BULK_BYTES });
        p.bond = Some(b);
        self.log(format!("pick: measuring {} for {} s", peer, Mono::from(self.pick.as_ref().map_or(0, |p| p.seconds))));
        self.open_session(PICK, core::Role::Client, true);
        self.send_chan(id, &start);
    }

    /// The candidate's `measure result`: its lanes and its egress complete the measurement once
    /// this side's window is over too.
    pub(super) fn pick_done(&mut self, lanes: Vec<MeasureLane>, egress_ms: f64) {
        if let Some(p) = self.pick.as_mut() {
            p.result = Some((lanes, egress_ms));
        }
    }

    /// An anchor's answer to `pick`'s `bond open` (§18 step 1): a temporary bond beside the
    /// client's own, which it leaves alone: no bond addresses (so nothing it carries reaches the
    /// TUN device or the internet), no pool address, not counted against MAX_BONDS; it ends with
    /// its channel. A newer one from the same device replaces it.
    pub(super) fn measure_open(&mut self, id: u64, mode: Mode) {
        let now = self.now();
        let Some(p) = self.chans.get(&id) else { return };
        let Some(h) = p.hello.clone() else { return };
        let peer = p.chan.peer.ip().to_canonical();
        if self.cfg.role != Role::Anchor {
            return self.refuse(id, ErrorCode::NotAllowed, "not an anchor");
        }
        if self.allowed(peer).is_none() {
            self.log(format!("pick from {} ({peer}): not on the allow-list", link::clean(&h.name, link::LABEL_MAX)));
            return self.refuse(id, ErrorCode::NotAllowed, "not on the allow-list");
        }
        if let Some(old) = self.bonds.iter().position(|b| b.measure && b.peer_addr == peer) {
            self.end_bond(old, true);
        }
        let mut b = Bond::new(link::clean(&h.name, link::LABEL_MAX), peer, h.inst, mode, now);
        (b.chan, b.measure, b.egress) = (Some(id), true, Egress::None);
        b.set_state(BondState::Punching, now);
        self.bonds.push(b);
        let bi = self.bonds.len() - 1;
        let (own, _) = self.anchor_nets();
        let dns = own.first().copied().unwrap_or(IpAddr::V4(Ipv4Addr::new(10, 77, 0, 1)));
        let ok = proto::Bond::Ok { addr4: None, addr6: None, mtu: TUN_MTU as u16, dns, egress: Egress::None };
        self.send_chan(id, &Msg::Bond(ok));
        self.log(format!("pick bond open from {} ({peer}), {}", self.bonds[bi].peer, mode.as_str()));
        self.set_mode(bi, mode);
        self.open_session(bi, core::Role::Anchor, true);
    }

    /// `measure start` on channel `id` (§18): the bond on it measures for `seconds`, pushing at
    /// most `bulk_bytes` of filler in bandwidth mode's second half, and the egress is measured off
    /// the loop meanwhile. The answer goes when both are done.
    pub(super) fn measure_start(&mut self, id: u64, seconds: u32, bulk_bytes: u64) {
        let now = self.now();
        let seconds = seconds.clamp(1, 600);
        let bulk = bulk_bytes.min(PICK_BULK_BYTES);
        if let Some(b) = self.bonds.iter_mut().find(|b| b.chan == Some(id)) {
            let push = (b.mode == Mode::Bandwidth && bulk > 0).then_some(bulk);
            b.meter = Some(Meter::new(now, seconds, push, false));
        }
        let within = Duration::from_secs(u64::from(seconds)).saturating_sub(Duration::from_millis(250));
        ctl::egress(id, within.max(Duration::from_millis(250)), self.ev_tx.clone(), self.waker.clone());
    }

    /// This node's egress for the `measure start` on channel `id`: kept for the answer, or sent
    /// at once when no bond measures on that channel.
    pub(super) fn measure_egress(&mut self, id: u64, ms: f64) {
        match self.bonds.iter_mut().find(|b| b.chan == Some(id)).and_then(|b| b.meter.as_mut()) {
            Some(m) => m.egress_ms = Some(ms),
            None => self.send_chan(id, &Msg::Measure(proto::Measure::Result { lanes: Vec::new(), egress_ms: ms })),
        }
    }

    /// The measure phase's clock, each loop turn: the pick's own temporary bond (samples, the
    /// small packets that keep it probing, the end of the candidate), and an anchor's measured
    /// bonds (samples, filler, the answer).
    pub(super) fn pick_tick(&mut self, now: Mono) {
        for bi in 0..self.bonds.len() {
            let Some(snap) = self.bonds[bi].meter.as_ref().and(self.bonds[bi].session.as_ref()).map(|s| s.snapshot())
            else {
                continue;
            };
            let Some(m) = self.bonds[bi].meter.as_mut() else { continue };
            m.sample(now, &snap);
            let n = m.filler(now, snap.lanes.iter().map(|l| l.tx_bytes).sum());
            let answer = (m.done(now) && !m.answered).then_some(m.egress_ms).flatten();
            if n > 0 {
                let p = filler_packet(FILLER_LEN, true);
                for _ in 0..n {
                    self.handle_bond(bi, core::Event::Send { packet: &p });
                }
            }
            if let Some(egress_ms) = answer
                && let Some(m) = self.bonds[bi].meter.as_mut()
            {
                m.answered = true;
                let (lanes, _) = m.result();
                self.send_bond(bi, &Msg::Measure(proto::Measure::Result { lanes, egress_ms }));
            }
        }
        let Some(p) = self.pick.as_ref() else { return };
        if now - p.started >= Mono::from(p.seconds) * SEC + GRACE {
            let stage = match (&p.bond, &p.result) {
                (None, _) => "no bond ok",
                (Some(_), None) => "no measure result",
                _ => "measuring",
            };
            return self.pick_fail(format!("timed out: {stage}"));
        }
        let Some(snap) = p.bond.as_ref().and_then(|b| b.session.as_ref()).map(|s| s.snapshot()) else { return };
        let Some(m) = self.pick.as_mut().and_then(|p| p.bond.as_mut()).and_then(|b| b.meter.as_mut()) else { return };
        m.sample(now, &snap);
        if m.keep(now) {
            self.handle_bond(PICK, core::Event::Send { packet: &filler_packet(64, false) });
        }
        let done = self
            .pick
            .as_ref()
            .and_then(|p| p.bond.as_ref())
            .and_then(|b| b.meter.as_ref())
            .is_some_and(|m| m.done(now));
        if done && self.pick.as_ref().is_some_and(|p| p.result.is_some()) {
            self.pick_measured();
        }
    }

    /// The candidate is measured: this side's lanes with the candidate's view of each (the worse
    /// OWD, dev and loss of the two directions), each in its bottleneck group (§8), the
    /// across-lane minimum, and the candidate's egress.
    fn pick_measured(&mut self) {
        let Some(p) = self.pick.as_mut() else { return };
        let Some((theirs, egress_ms)) = p.result.take() else { return };
        let Some((mine, min_owd_p99)) = p.bond.as_ref().and_then(|b| b.meter.as_ref()).map(Meter::result) else {
            return;
        };
        let device = p.cands[p.i].clone();
        let lanes = mine
            .into_iter()
            .map(|mut l| {
                if let Some(t) = theirs.iter().find(|t| t.id == l.id) {
                    (l.owd_p99, l.dev, l.loss) = (l.owd_p99.max(t.owd_p99), l.dev.max(t.dev), l.loss.max(t.loss));
                }
                let g = self.lane_group(l.id);
                (l, g)
            })
            .collect();
        let m = Measured { device, lanes, min_owd_p99, egress_ms, failed: None };
        self.log(format!(
            "pick: {} measured: {:?}",
            m.device,
            m.score(self.pick.as_ref().map_or(Mode::Performance, |p| p.mode))
        ));
        self.pick_record(m);
    }

    /// The bottleneck group of my end of lane `lane` (§8): its link's, or for the mesh lane the
    /// group of the link carrying the mesh.
    fn lane_group(&self, lane: u16) -> u8 {
        let link = (lane >> 8) as u8;
        let groups = self.groups();
        let at = self.socks.iter().position(|s| s.id == link && (link != 0 || s.link.addr.is_ipv4()));
        at.map_or(link, |i| groups[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(mbps: f64, rtt: f64) -> MeasureLane {
        MeasureLane { mbps, rtt_p50: rtt, ..MeasureLane::default() }
    }

    fn cand(device: &str, lanes: &[(f64, f64, u8)], min_owd_p99: f64, egress_ms: f64) -> Measured {
        Measured {
            device: device.into(),
            lanes: lanes.iter().map(|&(m, r, g)| (lane(m, r), g)).collect(),
            min_owd_p99,
            egress_ms,
            failed: None,
        }
    }

    #[test]
    fn performance_takes_the_lowest_delay_plus_half_the_egress() {
        let a = cand("vps", &[(40.0, 60.0, 1), (80.0, 70.0, 2)], 31.0, 10.0); // 36
        let b = cand("xanth", &[(40.0, 50.0, 1), (80.0, 60.0, 2)], 28.0, 20.0); // 38
        assert_eq!(select(Mode::Performance, &[a.clone(), b.clone()]), Some(0));
        assert_eq!(select(Mode::Performance, &[b, a]), Some(1));
    }

    #[test]
    fn bandwidth_takes_the_most_throughput_and_breaks_ties_on_delay() {
        let a = cand("vps", &[(40.0, 60.0, 1), (80.0, 70.0, 2)], 31.0, 10.0); // 120
        let b = cand("xanth", &[(40.0, 40.0, 1), (78.0, 50.0, 2)], 28.0, 20.0); // 118, within 5%, lower OWD
        let c = cand("far", &[(20.0, 90.0, 1), (30.0, 99.0, 2)], 50.0, 5.0); // 50
        assert_eq!(select(Mode::Bandwidth, &[a.clone(), b, c.clone()]), Some(1));
        assert_eq!(select(Mode::Bandwidth, &[c, a]), Some(1));
    }

    #[test]
    fn one_group_or_no_lane_scores_last_with_the_reason() {
        let one = cand("hotel", &[(90.0, 10.0, 1), (90.0, 10.0, 1)], 1.0, 1.0);
        let none = Measured { device: "off".into(), failed: Some("unreachable".into()), ..Measured::default() };
        let ok = cand("xanth", &[(10.0, 60.0, 1), (10.0, 70.0, 2)], 40.0, 20.0);
        let all = [one.clone(), none.clone(), ok];
        assert_eq!(select(Mode::Performance, &all), Some(2));
        assert_eq!(select(Mode::Bandwidth, &all), Some(2));
        assert_eq!(select(Mode::Bandwidth, &[one, none]), None);
        let r = rows(Mode::Performance, &all);
        assert_eq!(r[0].reason, "every lane in one group");
        assert_eq!(r[1].reason, "unreachable");
        assert_eq!((r[2].groups, r[2].score), (2, Some(50.0)));
    }
}

#[cfg(test)]
mod meter_tests {
    use wirebond_core::{LaneOut, LaneStatus, SessionStatus};

    use super::*;

    fn lane(id: u16, state: &'static str, rtt: f64, qd: f64, rx: u64) -> LaneStatus {
        let out = LaneOut { rtt_ms: rtt, queue_ms: qd, ..LaneOut::default() };
        LaneStatus { id, state, validated: true, rx_bytes: rx, out, ..LaneStatus::default() }
    }

    fn snap(lanes: Vec<LaneStatus>) -> SessionStatus {
        SessionStatus { lanes, ..SessionStatus::default() }
    }

    /// The probe half's percentiles per lane and across lanes, the second half's top rates, and
    /// the filler kept within its budget (§18 steps 2 and 3).
    #[test]
    fn the_meter_measures_both_halves_and_keeps_the_budget() {
        let mut m = Meter::new(0, 2, None, true);
        for i in 0..50 {
            let spike = if i == 7 { 30.0 } else { 0.0 };
            let lanes = vec![
                lane(0x101, "up", 20.0, spike, 0),
                lane(0x202, "suspect", 40.0, 0.0, 0),
                lane(0x303, "down", 1.0, 0.0, 0),
            ];
            m.sample(i * PROBE_ACTIVE, &snap(lanes));
        }
        let (lanes, min_owd_p99) = m.result();
        assert_eq!(lanes.iter().map(|l| l.id).collect::<Vec<_>>(), vec![0x101, 0x202], "a down lane is not measured");
        assert_eq!((lanes[0].rtt_p50, lanes[0].owd_p99, lanes[0].dev), (20.0, 40.0, 0.0), "{:?}", lanes[0]);
        assert_eq!(min_owd_p99, 20.0, "the spiking sample's minimum is the other lane's 20 ms");
        // The second half: 125 kB on lane 0x101 in 100 ms is 10 Mbit/s.
        m.sample(SEC + 10 * MS, &snap(vec![lane(0x101, "up", 20.0, 0.0, 0)]));
        m.sample(SEC + 110 * MS, &snap(vec![lane(0x101, "up", 20.0, 0.0, 125_000)]));
        assert!((m.result().0[0].mbps - 10.0).abs() < 1e-9, "{:?}", m.result().0);
        // The anchor's filler: never past its budget, nothing before half time or after the end.
        let mut a = Meter::new(0, 2, Some(10_000), false);
        assert_eq!(a.filler(SEC - MS, 0), 0);
        let total: usize = (0..100).map(|i| a.filler(SEC + i * 10 * MS, 0)).sum();
        assert_eq!(total, 10_000 / (FILLER_LEN + 28), "the budget, in whole packets");
        assert_eq!(a.filler(2 * SEC, 0), 0);
        assert!(!a.done(2 * SEC - 1) && a.done(2 * SEC));
    }
}
