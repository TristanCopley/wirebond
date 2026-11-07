//! The `performance` scheduler, and what both schedulers share (§11.1).
//!
//! Performance sends every packet of every flow on every lane of the spread: the lanes bandwidth's
//! eligibility (§11.2 rules 2 and 4, bandwidth.rs) picks, one per bottleneck group, the Tailscale
//! lane only for a group with no usable direct lane, with no skew limit. Each copy is paced on its
//! own lane at that lane's rate (§9): a lane that cannot keep up drops its excess at its own pacer
//! (QUEUE_AGE) and never holds the others back. The receiver keeps the first copy (the replay
//! window drops the rest), and its reorder buffer does not wait for a lagging lane's copy
//! (`flow::release`, DUP_GRACE). No failover, rewind, trial or return: a lane that stalls simply
//! loses the race, and one that leaves the spread drops its queue, whose packets the other lanes
//! carry. Throughput is the best single lane's; latency and steadiness the best of the lanes per
//! packet. The prototype: research_notes jitter_simulation.md, netsim's tests/jitter.rs.

use crate::lane::{Lane, LaneIx, State};
use crate::session::Mode;
use crate::{BANDWIDTH_SKEW_MAX, BAR, BAR_MAX, BLIND_AFTER, Mono, Session};

/// Whether two lanes are in different bottleneck groups (§6.1). Never a lane and itself.
pub(crate) fn apart(a: &Lane, b: &Lane) -> bool {
    a.group >= 0 && b.group >= 0 && a.group != b.group
}

/// `now − t`; saturating, since `t` may be a sentinel (NEVER, `Mono::MIN / 2`). Go: `now.Sub(t)`.
pub(crate) fn since(now: Mono, t: Mono) -> Mono {
    now.saturating_sub(t)
}

impl Lane {
    /// Validated, and up or suspect: it still answers, and can carry bulk.
    pub(crate) fn usable(&self) -> bool {
        self.validated && matches!(self.state, State::Up | State::Suspect)
    }
}

impl Session {
    /// Whether `l` carries bulk in my direction: it is in the spread.
    pub(crate) fn carries(&self, l: LaneIx) -> bool {
        self.spread().contains(&l)
    }

    /// No echo on any lane for BLIND_AFTER (§8.4): the blind sender of both modes.
    pub(crate) fn blind(&self, now: Mono) -> bool {
        now.saturating_sub(self.last_echo) > BLIND_AFTER
    }

    /// Where a bulk packet of `size` bytes goes now (§11), the first lane taking the packet itself
    /// and the others a copy: every lane of the spread in performance, the earliest arrival in
    /// bandwidth; none means every lane (`send_unpaced`).
    pub(crate) fn bulk_lanes(&mut self, now: Mono, size: usize) -> impl Iterator<Item = LaneIx> + use<> {
        let (mut ls, mut n) = (self.spread, self.nspread);
        if self.mode == Mode::Bandwidth {
            (ls[0], n) = self.earliest(now, size).map_or((LaneIx::default(), 0), |l| (l, 1));
        }
        ls.into_iter().take(n)
    }

    /// Of the lanes that pass `ok`, the one no later one `beats`: the first, unless a later one
    /// beats it, and so on.
    fn best(&self, ok: impl Fn(&Lane) -> bool, beats: impl Fn(&Lane, &Lane) -> bool) -> Option<LaneIx> {
        let ls = &self.lanes;
        ls.ixs().filter(|&l| ok(&ls[l])).reduce(|b, l| if beats(&ls[l], &ls[b]) { l } else { b })
    }

    /// The two lowest-OWD lanes in different groups that still answer: the blind sender's spread
    /// (§8.4). Those not suspect first, and direct lanes before the Tailscale lane (notes/core.md
    /// 40c).
    pub(crate) fn two(&self) -> (Option<LaneIx>, Option<LaneIx>) {
        self.two_of(Lane::usable)
    }

    /// `two()` over the lanes that pass `ok`: those up first, then the rest (suspect or down)
    /// alike, direct lanes before the mesh lane, then the lower OWD.
    pub(crate) fn two_of(&self, ok: impl Fn(&Lane) -> bool) -> (Option<LaneIx>, Option<LaneIx>) {
        let rank = |l: &Lane| (l.state != State::Up, l.mesh, l.owd);
        let lower = |l: &Lane, t: &Lane| rank(l) < rank(t);
        let a = self.best(&ok, lower);
        (a, a.and_then(|a| self.best(|l| ok(l) && apart(l, &self.lanes[a]), lower)))
    }

    /// Keeps bulk off `l` for BAR, doubling while it keeps failing, up to BAR_MAX (§11.2 rule 4).
    pub(crate) fn bar(&mut self, now: Mono, l: LaneIx) {
        let l = &mut self.lanes[l];
        l.bar_until = now.saturating_add((BAR << l.bars.min(8)).min(BAR_MAX));
        l.bars = (l.bars + 1).min(8);
    }

    /// The slot of the heaviest bulk flow I send, if any (the first of equals): a flow I receive
    /// is classified by its sender (§13).
    pub(crate) fn bulk_flow(&self) -> Option<usize> {
        let f = &self.flows;
        (0..f.len())
            .filter(|&i| f[i].used && f[i].mine && !f[i].small)
            .reduce(|b, i| if f[i].r > f[b].r { i } else { b })
    }

    /// The skew a lane's base OWD may have over the fastest's to join the spread: unlimited in
    /// performance, every usable lane racing; BANDWIDTH_SKEW_MAX in bandwidth (§11.2 rule 2).
    pub(crate) fn skew(&self) -> Mono {
        match self.mode {
            Mode::Performance => Mono::MAX,
            Mode::Bandwidth => BANDWIDTH_SKEW_MAX,
        }
    }
}

#[cfg(test)]
mod tests;
