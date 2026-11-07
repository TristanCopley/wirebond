//! Rewind (§14.2): when a lane turns suspect, its unacknowledged packets from the last
//! REWIND_WINDOW (or its suspect timeout plus RTT if longer) are re-sent on the other eligible
//! lanes. A packet's echoed `last_counter` marks it acknowledged.

use crate::lane::LaneIx;
use crate::{Mono, Out, QUEUE_AGE, Session};

impl Session {
    /// Re-sends the bulk packets `l` sent after the one whose counter equals the peer's last
    /// echoed `last_counter` (all of them if it left the ring) and within the rewind window
    /// (`evict`), once each and in l's send order, across the lanes `to` by earliest arrival at
    /// their pace; then moves l's unsent queue behind them (`shift`). Counters are matched, never
    /// compared: small packets sealed after queued bulk can leave first. With `to` empty everything
    /// is dropped. The caller has taken the lanes of `to` (R2-04), which do not include `l`.
    ///
    /// A rewind that re-sends anything is in flight for QUEUE_AGE, the longest its packets wait in
    /// a pacer: my probes say so meanwhile (PROBE_REWIND), and the peer's reorder buffer waits
    /// REORDER_RESCUE for its gaps (§15).
    pub(crate) fn rewind(&mut self, out: &mut dyn Out, now: Mono, l: LaneIx, to: &[LaneIx]) {
        self.evict(now, l);
        let x = &mut self.lanes[l];
        // The acked packet and those before it arrived.
        let acked = x.rw.iter().rposition(|p| p.c == x.acked).map_or(0, |i| i + 1);
        for b in x.rw.drain(..acked).filter_map(|p| p.buf) {
            self.pool.put(b);
        }
        while let Some(p) = self.lanes[l].rw.pop_front() {
            match p.buf {
                Some(_) if !to.is_empty() => {
                    let t = self.earliest_of(now, to, p.n as usize).unwrap_or(to[0]);
                    self.rewind_until = now.saturating_add(QUEUE_AGE);
                    self.enqueue(out, now, t, p);
                }
                Some(b) => self.pool.put(b),
                None => {} // a small packet's counter
            }
        }
        self.shift(out, now, l, to);
    }

    /// Moves l's unsent queue, in order, across the lanes `to` by earliest arrival, or drops it
    /// when `to` is empty.
    pub(crate) fn shift(&mut self, out: &mut dyn Out, now: Mono, l: LaneIx, to: &[LaneIx]) {
        while let Some(p) = self.lanes[l].q.pop_front() {
            self.lanes[l].q_bytes -= p.n as usize;
            match (to.is_empty(), p.buf) {
                (false, _) => {
                    let t = self.earliest_of(now, to, p.n as usize).unwrap_or(to[0]);
                    self.enqueue(out, now, t, p);
                }
                (true, Some(b)) => self.pool.put(b),
                (true, None) => {}
            }
        }
    }
}
