//! Contribution statistics (SPEC 2.0 §11.5), kept per lane on the receiving side: bytes carried,
//! first arrivals and rescues (performance), and reorder wait (bandwidth). The daemon turns the
//! counts into shares, fractions and per-minute rates.
//!
//! A rescue is a packet whose first copy came on a lane while every other copy came at least
//! RESCUE_GAP later or never. First arrivals are kept in a ring of RESCUE_RING counters, by
//! counter: a later copy within RESCUE_GAP closes its entry, one RESCUE_GAP or more later counts
//! the rescue. Copies arrive in time order, so every copy within the gap has come by then. An
//! entry still open when a newer counter takes its slot had no other copy at all: it counts as a
//! rescue if another lane has since delivered a newer counter, so that lane was carrying and lost
//! its copy, and not when the packet had no other lane to come on.

use crate::lane::LaneIx;
use crate::session::Mode;
use crate::{Mono, RESCUE_GAP, Session};

/// First arrivals kept for rescues (§11.5).
pub(crate) const RESCUE_RING: usize = 8192;

/// Reorder-wait buckets: bucket 0 is no wait; bucket i ≥ 1 holds waits in ((i − 1) × WAIT_STEP,
/// i × WAIT_STEP], the last also everything longer.
pub(crate) const WAIT_BUCKETS: usize = 513;
pub(crate) const WAIT_STEP: Mono = 500_000; // 0.5 ms

/// A first arrival, by counter % RESCUE_RING. `l` None: empty, or a lane that has since gone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct First {
    pub(crate) c: u64,
    pub(crate) at: Mono,
    pub(crate) l: Option<LaneIx>,
    /// No other copy yet: still a rescue candidate.
    pub(crate) open: bool,
}

/// One lane's receive statistics.
#[derive(Clone, Debug, Default)]
pub(crate) struct LaneStats {
    /// Bytes of data packets received on the lane, copies included.
    pub(crate) rx_bytes: u64,
    pub(crate) firsts: u64,
    pub(crate) rescues: u64,
    /// Reorder waits of the bulk packets that first came on the lane; empty until the first.
    pub(crate) waits: Vec<u32>,
}

impl LaneStats {
    /// The `p`-quantile (0 < p ≤ 1) of the reorder waits, at its bucket's upper edge; 0 with none.
    pub(crate) fn wait(&self, p: f64) -> Mono {
        let total: u64 = self.waits.iter().map(|&n| u64::from(n)).sum();
        if total == 0 {
            return 0;
        }
        let want = ((total as f64 * p).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, &n) in self.waits.iter().enumerate() {
            seen += u64::from(n);
            if seen >= want {
                return i as Mono * WAIT_STEP;
            }
        }
        (WAIT_BUCKETS - 1) as Mono * WAIT_STEP
    }

    fn waited(&mut self, wait: Mono) {
        if self.waits.is_empty() {
            self.waits = vec![0; WAIT_BUCKETS];
        }
        let i = if wait <= 0 { 0 } else { ((wait + WAIT_STEP - 1) / WAIT_STEP).min(WAIT_BUCKETS as Mono - 1) as usize };
        self.waits[i] = self.waits[i].saturating_add(1);
    }
}

impl Session {
    /// Notes the authenticated data packet with counter `c`, `size` bytes, that arrived on `l`
    /// (§10.4 step 4: `first` if it is the first copy): bytes, first arrivals and rescues.
    pub(crate) fn arrived(&mut self, now: Mono, l: LaneIx, c: u64, first: bool, size: usize) {
        let st = &mut self.lanes[l].stats;
        st.rx_bytes = st.rx_bytes.wrapping_add(size as u64);
        if self.mode != Mode::Performance {
            return;
        }
        let slot = (c % RESCUE_RING as u64) as usize;
        if first {
            self.lanes[l].stats.firsts += 1;
            let old = self.firsts[slot];
            if let Some(m) = old.l.filter(|_| old.open && old.c != c) {
                // No other copy came: a rescue if another lane carried later packets.
                if self.lanes.ixs().any(|o| o != m && self.lanes[o].rx_last > old.c) {
                    self.lanes[m].stats.rescues += 1;
                }
            }
            self.firsts[slot] = First { c, at: now, l: Some(l), open: true };
            return;
        }
        let e = &mut self.firsts[slot];
        if e.c != c || !e.open || e.l == Some(l) {
            return;
        }
        e.open = false;
        if let Some(m) = e.l.filter(|_| now.saturating_sub(e.at) >= RESCUE_GAP) {
            self.lanes[m].stats.rescues += 1;
        }
    }

    /// Notes that a bulk packet that first came on the lane with id `lane` waited `wait` in
    /// reorder before its delivery (bandwidth, §11.5).
    pub(crate) fn waited(&mut self, lane: Option<u16>, wait: Mono) {
        if self.mode != Mode::Bandwidth {
            return;
        }
        if let Some(l) = lane.and_then(|id| self.lane_by_id(id)) {
            self.lanes[l].stats.waited(wait);
        }
    }
}

#[cfg(test)]
mod tests;
