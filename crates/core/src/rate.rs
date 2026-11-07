//! The rate controller, the pacer and the rewind ring's bookkeeping (§9, §10.2; Go:
//! core/rate.go).
//!
//! Owner: C4.

use crate::buf::{MAX_BUFS, Pkt};
use crate::estimate::INF;
use crate::lane::{Lane, LaneIx, State};
use crate::session::{Mode, NEVER};
use crate::{
    BETA, BOND_BYTES, INCREASE_STEP, MAX_PACKET, MIN_WINDOW, MS, Mono, Out, PACE_BURST, PACE_FLOOR, QUEUE_AGE,
    RATE_WINDOW, REWIND_SHARE, REWIND_WINDOW, STARTUP_GAIN, Session, TARGET_DEV, TARGET_FLOOR,
};

/// A lane that delivers this share of its offered rate is not cut below that rate.
const CARRIES: f64 = 0.95;

impl Session {
    /// Begins a lane's rate afresh (§9): unlimited in performance until the first overuse, from
    /// PACE_FLOOR in startup in bandwidth.
    pub(crate) fn restart(&mut self, _now: Mono, l: LaneIx) {
        let ln = &mut self.lanes[l];
        ln.rate = if self.mode == Mode::Bandwidth { PACE_FLOOR } else { INF };
        (ln.startup, ln.over_n, ln.regain) = (true, 0, 0.0);
    }

    /// Readies `l` to carry bulk: a rate learned within MIN_WINDOW still holds, an older one
    /// restarts (§9). Callers take a lane before any packet moves onto it (R2-04: rewind, shift
    /// and enqueue set `bulk_at`, which would make a stale rate look fresh).
    pub(crate) fn take(&mut self, now: Mono, l: LaneIx) {
        if now.saturating_sub(self.lanes[l].bulk_at) >= MIN_WINDOW {
            self.restart(now, l);
        }
        self.lanes[l].took_at = now;
    }

    /// Runs the rate controller on a fresh echo of `l` (§9); `lossy`: the echo's loss window lost
    /// over LOSS_OVERUSE and at least LOSS_MIN packets.
    pub(crate) fn adjust(&mut self, now: Mono, l: LaneIx, lossy: bool) {
        // Each performance lane is offered the whole stream (Go's stable primary was).
        let (perf, bandwidth) = (self.mode == Mode::Performance, self.mode == Mode::Bandwidth);
        // A bandwidth lane that is the whole spread carries the whole stream, as a performance
        // lane does: earliest arrival has no other lane to give bulk to, so its offered rate is
        // the stream's whatever it paces, and performance's start and climb apply to it (below).
        // Not while another direct lane is suspect yet still echoed within its RTT, late but
        // answering: it is back within an RTT or so, and a lane gone unlimited meanwhile
        // overfilled its path's buffer and turned suspect in turn, the two lanes leaving the
        // spread by turns with a rewind each time (§14.2). A lane that fell silent (its link
        // gone) leaves this one alone at once.
        let late = |x: &Lane| x.state == State::Suspect && now.saturating_sub(x.last_echo) <= x.rtt;
        let alone = bandwidth && self.spread() == [l] && !self.lanes.iter().any(|x| x.validated && !x.mesh && late(x));
        let ln = &mut self.lanes[l];
        let target = TARGET_FLOOR.max((TARGET_DEV * ln.dev_app as f64) as Mono);
        ln.over_n = if ln.qd > target { ln.over_n.saturating_add(1) } else { 0 };
        let rtt = if ln.rtt != 0 { ln.rtt } else { RATE_WINDOW };
        let o = ln.offered.rate(now, RATE_WINDOW);
        if ln.over_n >= 2 || lossy {
            (ln.last_over, ln.clean_at) = (now, NEVER);
            // Only a lane carrying bulk now, whose delivery sample covers that bulk, is cut: one
            // that only carried probes and copies, or took bulk less than a sample ago, delivers
            // near nothing, and a cut would leave it at PACE_FLOOR. In performance a cut sized by qd
            // drains that queue in a few RTTs, so within 3 RTTs only a queue still growing is cut
            // again; bandwidth runs its lanes at bandwidth and keeps the spec's once per RTT. Light
            // loss without a queue is the link's own (a radio's random loss), which no lower rate
            // cures: it keeps the lane's room, twice its offered rate. And a lane that delivers
            // what it is offered is not cut below that: its queue came from a burst above it (a
            // takeover, a rate raised past bandwidth), and a lower rate would only hold the stream
            // in the pacer. Heavy loss, a policer's, and a lane too small for its stream deliver
            // less, and are cut to delivery (notes/core.md 49, 52).
            let due = now - ln.took_at >= rtt + RATE_WINDOW
                && now - ln.last_cut >= rtt
                && (bandwidth || now - ln.last_cut >= 3 * rtt || ln.qd > ln.cut_qd);
            if due && self.carries(l) {
                let ln = &mut self.lanes[l];
                let mut r = cut(ln.rate, ln.qd, target, ln.delivery);
                // Go's `loss <= 1 − TrialPass` against the exact constant 0.05: f64's 1 − 0.95 is
                // 0.050000000000000044, and `<` gives Go's answer for every loss 1 − got/sent.
                if ln.over_n < 2 && ln.loss < 1.0 - CARRIES {
                    r = r.max(ln.rate.min(2.0 * o));
                } else if ln.delivery >= CARRIES * o {
                    r = r.max(ln.rate.min(o));
                }
                if r < ln.rate {
                    if ln.startup {
                        ln.regain = BETA * ln.delivery; // startup overshot: what the lane delivered then
                    }
                    (ln.rate, ln.startup, ln.last_cut, ln.cut_qd) = (r, false, now, ln.qd);
                }
            }
            return;
        }
        // Raise the rate once per RTT, and only while it binds, so that an app-limited lane never
        // drifts above what it carries [Congestion 3]: in bandwidth while offered is at or above it
        // (bandwidth offers a lane more the faster it paces, so more would only feed itself); in
        // performance, whose offered rate is the stream's, also while a backlog waits and the rate is
        // under the 2 × offered the pacer would use. By STARTUP_GAIN in startup; back up to
        // regain once startup's overshoot has drained (its cut was sized by a queue that grew
        // while the overuse was in flight, and left the lane far below what it had delivered); in
        // performance once any cut has drained the queue (qd under half the target), so that a lane
        // cut for a burst it made itself, such as a rewind, is soon above the offered rate again;
        // otherwise by INCREASE_STEP. Per RTT, not per 20 ms: an increase shows in qd only an RTT
        // later, and at 1% per 20 ms the rate ran 12–15% past bandwidth before the overuse
        // (notes/core.md 36, 50, 51). Bandwidth's lane alone in the spread is a performance lane
        // here: in startup its first binding increase makes it unlimited, performance's start, until
        // its first overuse; after a cut that drained it climbs by STARTUP_GAIN while it binds,
        // that is while the stream's offered rate reaches it. Bandwidth starts lanes at PACE_FLOOR
        // and climbs by INCREASE_STEP because lanes that share a stream run at their
        // bandwidth; alone, those only held the stream in the pacer (notes/rust.md, harden:
        // integration).
        let binds = o >= ln.rate || perf && !ln.q.is_empty() && ln.rate < 2.0 * o;
        if binds && now - ln.last_over >= rtt && now - ln.last_inc >= rtt {
            let drained = ln.qd.saturating_mul(2) < target;
            let mut r = ln.rate * (1.0 + INCREASE_STEP);
            if ln.startup && alone {
                r = INF;
            } else if ln.startup || (perf || alone) && drained {
                r = ln.rate * STARTUP_GAIN;
            } else if ln.regain > r && drained {
                r = (ln.rate * STARTUP_GAIN).min(ln.regain);
            }
            if r >= ln.regain {
                ln.regain = 0.0;
            }
            (ln.rate, ln.last_inc) = (r, now);
        }
    }

    /// Puts a sealed bulk packet in l's pacer queue and drains it (§9).
    pub(crate) fn enqueue(&mut self, out: &mut dyn Out, now: Mono, l: LaneIx, p: Pkt) {
        let ln = &mut self.lanes[l];
        ln.bulk_at = now;
        ln.q.push_back(Pkt { at: now, ..p });
        ln.q_bytes += p.n as usize;
        ln.offered.add(now, RATE_WINDOW, p.n as usize);
        self.drain(out, now, l);
    }

    /// Sends l's queue as its token bucket allows, drops packets older than QUEUE_AGE, asks for a
    /// tick when the head is due, and tracks `load_at` (§9, §11.1 rule 2).
    ///
    /// The head packet goes once the bucket holds its bytes, and credit accrues at pace up to
    /// PACE_BURST plus one full-size packet: the Tick comes at the first millisecond after the head
    /// is due, so a bucket of PACE_BURST alone lost the credit left after the last packet that fit
    /// at every Tick, and a lane sent 79–88% of its pace (notes/core.md 46, 52). An unvalidated
    /// lane drains nothing (§7.6 rule 2), and a suspect one only while the scheduler still gives it
    /// bulk, blind or on two lanes (§8.4, §10.2, §11.1). The lane is loaded while it is
    /// backlogged, offered above its rate with a queue, or in overuse (§11.1 rule 2): a queue
    /// alone is no load, since paced bursts keep one at any rate near the offered (notes/core.md
    /// 35, 41).
    pub(crate) fn drain(&mut self, out: &mut dyn Out, now: Mono, l: LaneIx) {
        let pace = self.lanes[l].pace(now) / 8.0; // bytes per second
        // The most credit the bucket holds: PACE_BURST at pace, at least one full-size packet,
        // plus one full-size packet (notes/core.md 46, 52a). Seconds are Go's Duration.Seconds.
        let burst = (pace * (PACE_BURST as f64 / 1e9)).max(MAX_PACKET as f64) + MAX_PACKET as f64;
        let ln = &mut self.lanes[l];
        ln.tokens = (ln.tokens + pace * ((now - ln.filled) as f64 / 1e9)).min(burst);
        ln.filled = now;
        while let Some(p) = self.lanes[l].q.front().copied() {
            let ln = &self.lanes[l];
            let sends = ln.validated
                && (ln.state == State::Up || matches!(ln.state, State::Suspect | State::Down) && self.carries(l));
            // A validated lane has a return address; without one nothing can be sent.
            let Some(to) = ln.peer.filter(|_| sends) else { break };
            let ln = &mut self.lanes[l];
            let stale = now - p.at > QUEUE_AGE;
            if !stale && ln.tokens < p.n as f64 {
                // A tick at the first millisecond after the head is due (notes/core.md 22).
                let due = now + ((p.n as f64 - ln.tokens) / pace * 1e9) as Mono;
                self.wake_at((due / MS + 1) * MS);
                break;
            }
            ln.q.pop_front();
            ln.q_bytes -= p.n as usize;
            match p.buf {
                Some(b) if stale => self.pool.put(b),
                Some(b) => self.send(out, l, to, b, p.n as usize),
                None => {}
            }
            if !stale {
                self.lanes[l].tokens -= p.n as f64;
                self.keep(now, l, p);
            }
        }
    }

    /// Puts a packet `l` sent in its rewind ring (§10.2): bulk's buffer, or only the counter when
    /// the pool is past REWIND_SHARE of BOND_BYTES or for a small packet.
    pub(crate) fn keep(&mut self, now: Mono, l: LaneIx, mut p: Pkt) {
        if let Some(b) = p.buf.take_if(|_| self.pool.held() as f64 > BOND_BYTES as f64 * REWIND_SHARE) {
            self.pool.put(b);
        }
        self.lanes[l].rw.push_back(Pkt { at: now, ..p });
        self.evict(now, l);
    }

    /// Drops ring entries older than max(REWIND_WINDOW, suspect timeout + RTT), and the oldest
    /// beyond the pool's size, since a small packet's entry holds no buffer. `keep` evicts for
    /// every packet sent, and the window is never shorter than REWIND_WINDOW, so it is worked out,
    /// once, only when an entry is older than that.
    pub(crate) fn evict(&mut self, now: Mono, l: LaneIx) {
        let mut w = None;
        while let Some(p) = self.lanes[l].rw.front().copied() {
            let age = now - p.at;
            if self.lanes[l].rw.len() <= MAX_BUFS
                && (age <= REWIND_WINDOW
                    || age <= *w.get_or_insert_with(|| REWIND_WINDOW.max(self.suspect_after(l) + self.lanes[l].rtt)))
            {
                break;
            }
            if let Some(b) = self.lanes[l].rw.pop_front().and_then(|p| p.buf) {
                self.pool.put(b);
            }
        }
    }
}

impl Lane {
    /// The pacer rate, bits per second: min(rate, max(2 × offered, PACE_FLOOR)) (§9). Rolls the
    /// `offered` meter, hence `&mut`.
    pub(crate) fn pace(&mut self, now: Mono) -> f64 {
        self.rate.min((2.0 * self.offered.rate(now, RATE_WINDOW)).max(PACE_FLOOR))
    }
}

/// The rate after an overuse (§9): down to the measured delivery, by more the further qd is past
/// target, never below PACE_FLOOR; never NaN, also for an unlimited (INF) rate.
pub(crate) fn cut(rate: f64, qd: Mono, target: Mono, delivery: f64) -> f64 {
    let f = if qd > target { f64::max(0.5, 1.0 - (qd - target) as f64 / (2.0 * qd as f64)) } else { 1.0 };
    PACE_FLOOR.max((BETA * rate).min(f * delivery))
}

#[cfg(test)]
mod tests;
