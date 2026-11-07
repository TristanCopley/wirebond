//! Estimates: deviation series, windowed minima, byte meters, and what an echo teaches about my
//! direction of a lane (§8.3, §8.4; Go: core/estimate.go).
//!
//! Owner: C4. Durations are nanoseconds in [`Mono`], as everywhere in core.
//!
//! Values an echo carries come from the peer: `owd`, `held` and the probe's SentAt are combined
//! with wrapping arithmetic, as Go's silently wrapping integers did, so that no echo can make a
//! debug build panic; packet and byte counters wrap by design. Probe Seqs compare in serial order,
//! so a lane keeps working across the u32 wrap (R2-24); a stored Seq of 0 still means none.

use std::cell::Cell;

use crate::lane::{LaneIx, State};
use crate::session::NEVER;
use crate::wire::{Echo, after, seq_newer};
use crate::{
    DEV_WINDOW, DOWN_AFTER, ECHO_HISTORY, EWMA_GAIN, LOSS_MIN, LOSS_OVERUSE, MAD_SAMPLES, MIN_WINDOW, Mono,
    PROBE_ACTIVE, RATE_WINDOW, RTT_RING, SUSPECT_DEV, SUSPECT_MIN, Session, UP_AFTER,
};

/// An unlimited rate. Go: `inf`.
pub(crate) const INF: f64 = f64::INFINITY;

/// A minimum with no sample. Go: `noMin`.
pub(crate) const NO_MIN: Mono = Mono::MAX;

/// Recent samples for a median absolute deviation over DEV_WINDOW, which a single spike cannot
/// stretch the way a mean would (§8.3). Go: `series`.
///
/// `top` and `asked` are `max`'s cache, not part of the value: PartialEq compares the samples
/// only, and Default (a lane's start, `wake`'s reset) clears it.
#[derive(Clone, Debug)]
pub(crate) struct Series {
    pub(crate) at: [Mono; MAD_SAMPLES],
    pub(crate) v: [f64; MAD_SAMPLES],
    /// Samples ever added; the newest is at (n − 1) % MAD_SAMPLES.
    pub(crate) n: usize,
    /// The slot of a sample at least as large as each sample of the window at the last `max`
    /// query and each sample added since; None: unknown, the next query scans.
    top: Cell<Option<usize>>,
    /// The time of the last `max` query.
    asked: Cell<Mono>,
}

impl Default for Series {
    fn default() -> Series {
        Series { at: [0; MAD_SAMPLES], v: [0.0; MAD_SAMPLES], n: 0, top: Cell::new(None), asked: Cell::new(Mono::MIN) }
    }
}

/// The samples, not `max`'s cache.
impl PartialEq for Series {
    fn eq(&self, o: &Series) -> bool {
        self.at == o.at && self.v == o.v && self.n == o.n
    }
}

impl Series {
    pub(crate) fn add(&mut self, now: Mono, v: Mono) {
        let i = self.n % MAD_SAMPLES;
        (self.at[i], self.v[i], self.n) = (now, v as f64, self.n + 1);
        // A sample at least as large as `max`'s (compared as `max` compares) is the max now, and
        // of two equal ones the newer usually stays in the window longer. Overwriting the cached
        // sample leaves the max unknown.
        match self.top.get() {
            Some(t) if t == i => self.top.set(None),
            Some(t) if self.v[i] as Mono >= self.v[t] as Mono => self.top.set(Some(i)),
            _ => {}
        }
    }

    /// The median absolute deviation of the samples of the last DEV_WINDOW; 0 with none. Each
    /// median is the element at k / 2 of the k values in order, the upper one of an even count
    /// (Go's `x[len(x)/2]` after `slices.Sort`); `select_nth_unstable_by(f64::total_cmp)` finds it
    /// in a local `[f64; MAD_SAMPLES]` without sorting the rest (Go sorted the session's scratch
    /// twice): no allocation, deterministic.
    pub(crate) fn mad(&self, now: Mono) -> Mono {
        let mut x = [0.0; MAD_SAMPLES];
        let mut k = 0;
        // A plain loop: as a `filter` iterator it took about 1,000 more instructions a call.
        for i in 0..self.n.min(MAD_SAMPLES) {
            if now - self.at[i] <= DEV_WINDOW {
                x[k] = self.v[i];
                k += 1;
            }
        }
        if k == 0 {
            return 0;
        }
        // The samples are whole nanoseconds, never NaN or -0.0, so total_cmp orders them as Go's
        // `<` does. Only equal bits compare equal under it, so the element selection puts at k / 2
        // is, bit for bit, the one a sort puts there, and the medians are Go's.
        let x = &mut x[..k];
        let m = *x.select_nth_unstable_by(k / 2, f64::total_cmp).1;
        x.iter_mut().for_each(|v| *v = (*v - m).abs());
        *x.select_nth_unstable_by(k / 2, f64::total_cmp).1 as Mono
    }

    /// The largest sample of the last DEV_WINDOW, if any.
    ///
    /// `suspect_after` asks for every packet a lane sends, and a scan of the ring is 128 steps,
    /// so a scan keeps the slot of the largest sample it found (`top`), which `add` moves to any
    /// sample at least as large. While that sample is in the window and time has not gone back
    /// since the last query, it is the answer: the window only loses samples as time goes on, and
    /// every one added since was compared with it. Otherwise the ring is scanned as Go's `max`
    /// scanned it, and of equal largest samples the cache keeps the one that leaves the window
    /// last.
    pub(crate) fn max(&self, now: Mono) -> Option<Mono> {
        let last = self.asked.replace(now);
        if !self.top.get().is_some_and(|t| now >= last && now - self.at[t] <= DEV_WINDOW) {
            // A plain loop: as an iterator chain (`filter`, `max_by_key`) the scan cost more.
            let mut m: Option<(Mono, usize)> = None;
            for i in 0..self.n.min(MAD_SAMPLES) {
                let v = self.v[i] as Mono;
                if now - self.at[i] <= DEV_WINDOW
                    && m.is_none_or(|(top, j)| v > top || v == top && self.at[i] >= self.at[j])
                {
                    m = Some((v, i));
                }
            }
            self.top.set(m.map(|(_, i)| i));
        }
        self.top.get().map(|t| self.v[t] as Mono)
    }
}

/// A minimum over the last MIN_WINDOW to 2 × MIN_WINDOW, kept in two halves (§8.3). Go:
/// `winmin`. A new one must be [`Winmin::reset`] (Default is not "no sample").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Winmin {
    pub(crate) at: Mono,
    pub(crate) cur: Mono,
    pub(crate) old: Mono,
}

impl Winmin {
    /// No sample.
    pub(crate) fn new() -> Winmin {
        Winmin { at: 0, cur: NO_MIN, old: NO_MIN }
    }

    pub(crate) fn reset(&mut self) {
        *self = Winmin::new();
    }

    /// The minimum, NO_MIN if none.
    pub(crate) fn get(&self) -> Mono {
        self.cur.min(self.old)
    }

    pub(crate) fn add(&mut self, now: Mono, v: Mono) {
        if now - self.at >= MIN_WINDOW {
            (self.at, self.old, self.cur) = (now, self.cur, NO_MIN);
        }
        self.cur = self.cur.min(v);
    }
}

/// A sliding-window byte counter of two buckets of width `w`. Go: `meter`. Copy: Go copied
/// meters by value (`x.offered = old.offered`, `peek`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Meter {
    pub(crate) at: Mono,
    pub(crate) cur: f64,
    pub(crate) prev: f64,
}

impl Meter {
    pub(crate) fn add(&mut self, now: Mono, w: Mono, n: usize) {
        self.roll(now, w);
        self.cur += n as f64;
    }

    pub(crate) fn roll(&mut self, now: Mono, w: Mono) {
        let d = now - self.at;
        if d >= w {
            (self.prev, self.cur, self.at) = (if d >= 2 * w { 0.0 } else { self.cur }, 0.0, now);
        }
    }

    /// Bits per second over about the last `w`.
    pub(crate) fn rate(&mut self, now: Mono, w: Mono) -> f64 {
        self.roll(now, w);
        let f = (now - self.at) as f64 / w as f64;
        (self.prev * (1.0 - f) + self.cur) * 8.0 / (w as f64 / 1e9)
    }

    /// `rate` without rolling the meter, for status (reading it must change nothing).
    pub(crate) fn peek(&self, now: Mono, w: Mono) -> f64 {
        let mut m = *self;
        m.rate(now, w)
    }
}

/// One fresh echo of a lane, for loss and delivery over about RATE_WINDOW (§8.3). Go: `echoRec`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EchoRec {
    /// When I sent the echoed probe.
    pub(crate) at: Mono,
    /// When the peer received it, its clock, µs.
    pub(crate) rx: i64,
    pub(crate) sent: u32,
    pub(crate) pkts: u32,
    pub(crate) size: u32,
}

/// An EWMA_GAIN step from `old` toward `v`; the first sample (old 0) is taken whole.
pub(crate) fn ewma(old: Mono, v: Mono) -> Mono {
    // Wrapping as Go's: an OWD comes from the peer's clock and may be anything.
    if old == 0 { v } else { old.wrapping_add((v.wrapping_sub(old) as f64 * EWMA_GAIN) as Mono) }
}

impl Session {
    /// Applies what the peer reports receiving on one of my lanes (§8.3, §8.4): RTT, OWD, qd,
    /// loss, delivery, validation, liveness, then the rate controller (`adjust`). `carrier` is
    /// the lane the probe came on, None if it matched none; `sent_at` is that probe's SentAt
    /// (µs). The echo is passed by value: it lives in `self.ein`.
    ///
    /// R2-03: an echo validates the address it answers only if `e.token` equals
    /// `self.chal.token(l.id, sp.agen, e.seq)`: `validated` for `sp.agen == l.pgen`, `cvalid` for
    /// `sp.agen == l.cgen` with a claim open. RTT, OWD and liveness need no token.
    pub(crate) fn echo(&mut self, now: Mono, carrier: Option<LaneIx>, sent_at: i64, e: Echo) {
        let Some(l) = self.lane_by_id(e.lane) else { return };
        let sp = self.lanes[l].sent[e.seq as usize % RTT_RING];
        if e.seq == 0 || sp.seq != e.seq {
            return;
        }
        let ln = &mut self.lanes[l];
        if carrier == Some(l) && seq_newer(e.seq, ln.rtt_seq) {
            // RTT only from lane L's echo carried on lane L.
            ln.rtt_seq = e.seq;
            let rtt = now - sp.at - e.held as Mono * 1_000;
            if rtt > 0 {
                ln.rtts.add(now, rtt);
                ln.min_rtt.add(now, rtt);
                (ln.last_rtt, ln.rtt, ln.rttdev) = (rtt, ewma(ln.rtt, rtt), ln.rtts.mad(now));
            }
        }
        if !seq_newer(e.seq, ln.echo_seq) {
            return;
        }
        let gap = now - ln.last_echo;
        (ln.echo_seq, ln.last_echo, ln.misses, ln.acked) = (e.seq, now, 0, e.last);
        self.last_echo = now;
        // The echo of a probe sent to the current address, with that probe's challenge (§7.6 rule
        // 2, R2-03; a challenge is never 0, which means "none" on the wire). The PRF runs only
        // while it can change something.
        let answers = |id| e.token != 0 && e.token == self.chal.token(id, sp.agen, e.seq);
        if sp.agen == ln.pgen {
            ln.validated = ln.validated || answers(ln.id);
        } else if sp.agen == ln.cgen && ln.cand.is_some() {
            ln.cvalid = ln.cvalid || answers(ln.id);
        }
        if ln.wait_seq != 0 && !after(ln.wait_seq, e.seq) {
            ln.wait_seq = ln.next_wait(e.seq);
        }
        // OWD only from probes sent since the last wake (§8.3: a wake resets the minima). An echo
        // of a probe sent before it carries an OWD on the clock relation the sleep stepped: the
        // pause below every later sample when my clock paused. It refilled the minima the wake
        // had reset for MIN_WINDOW to 2 × MIN_WINDOW, and bandwidth's skew filter then kept out of
        // the spread every lane whose base OWD came from after the wake (notes/rust.md, harden:
        // integration). Such an echo leaves the OWD estimates and qd alone.
        if sp.at >= self.woke {
            let owd = e.owd.wrapping_mul(1_000);
            ln.owds.add(now, owd);
            ln.min_owd.add(now, owd);
            (ln.owd, ln.owd_now, ln.dev) = (ewma(ln.owd, owd), owd, ln.owds.mad(now));
            ln.qd = owd.wrapping_sub(ln.min_owd.get());
        }
        if ln.last_rtt > 0 && ln.min_rtt.get() != NO_MIN {
            ln.qd = ln.qd.min(ln.last_rtt - ln.min_rtt.get());
        }
        // The jitter that sets target is measured while app-limited: with the pacer empty and no
        // overuse within DEV_WINDOW, the span dev covers. Taken whenever the pacer was empty, it
        // took in the lane's own draining queue, and a 300 ms queue made target 230 ms for good
        // (A13).
        if ln.q.is_empty() && now - ln.last_over > DEV_WINDOW {
            ln.dev_app = ln.dev;
        }
        // Loss and delivery from probes sent to the lane's address since it became so: what went
        // to another address measured another path. Loss counts as overuse only with LOSS_MIN
        // packets lost: at 5–30 Mbps a 100 ms window holds 40–260 packets, so the cell's random
        // 0.5% alone read over 2% several times a second, ended bandwidth's startup at 7 Mbps (A4)
        // and cut a stream to its own rate (A1b).
        let mut lossy = false;
        if sp.agen == ln.pgen && seq_newer(e.seq, ln.hist_seq) {
            let rx = sent_at.wrapping_sub(e.held as i64); // when the peer received the probe
            let r = EchoRec { at: sp.at, rx, sent: sp.pkts, pkts: e.pkts, size: e.bytes };
            let n = ln.hist_n.min(ECHO_HISTORY);
            if n > 0 {
                // The newest one RATE_WINDOW back, else the oldest.
                let o = (1..n)
                    .map(|i| ln.hist[(ln.hist_n - i) % ECHO_HISTORY])
                    .find(|h| r.at - h.at >= RATE_WINDOW)
                    .unwrap_or(ln.hist[(ln.hist_n - n) % ECHO_HISTORY]);
                let (sent, got) = (r.sent.wrapping_sub(o.sent), r.pkts.wrapping_sub(o.pkts));
                if sent > 0 {
                    ln.loss = (1.0 - got as f64 / sent as f64).clamp(0.0, 1.0);
                    lossy = ln.loss > LOSS_OVERUSE && got as u64 + LOSS_MIN <= sent as u64;
                }
                let d = (r.rx.wrapping_sub(o.rx) as f64 * 1e3).max((r.at - o.at) as f64);
                if d > 0.0 {
                    ln.delivery = r.size.wrapping_sub(o.size) as f64 * 8e9 / d;
                }
            }
            ln.hist[ln.hist_n % ECHO_HISTORY] = r;
            ln.hist_n += 1;
        }
        self.confirmed |= sp.at >= self.since && ln.validated;
        let clean = !ln.peer_lag && gap <= self.suspect_after(l);
        let ln = &mut self.lanes[l];
        if !clean {
            ln.clean_at = NEVER;
        } else if ln.clean_at == NEVER {
            ln.clean_at = now;
        }
        self.adjust(now, l, lossy);
        let ln = &self.lanes[l];
        if ln.state == State::Punching && ln.validated && ln.got_probe
            || ln.state == State::Suspect && !ln.peer_lag && sp.at >= ln.suspect_at
            || ln.state == State::Down && now.saturating_sub(ln.clean_at) >= UP_AFTER
        {
            self.set_state(now, l, State::Up);
        }
    }

    /// How long a probe may go unechoed before its lane is suspect (§8.4): from the smoothed RTT
    /// plus max(SUSPECT_MIN, SUSPECT_DEV × rttdev), at least the largest RTT of DEV_WINDOW, plus
    /// PROBE_ACTIVE; DOWN_AFTER until an RTT is known. Uses `self.now`.
    ///
    /// From the smoothed RTT, not the minimum, because probes queue behind data and a standing
    /// queue the rate controller tolerates is not a stall; at least the largest RTT of DEV_WINDOW,
    /// because a bounded jitter such as the cell's reaches its bound far more often than
    /// SUSPECT_DEV × rttdev allows; plus PROBE_ACTIVE, because the echo waits at the peer for its
    /// next probe, which RTT leaves out as Held (notes/core.md 25, 34).
    pub(crate) fn suspect_after(&self, l: LaneIx) -> Mono {
        let ln = &self.lanes[l];
        if ln.rtt <= 0 {
            return DOWN_AFTER;
        }
        let d = ln.rtt + SUSPECT_MIN.max((SUSPECT_DEV * ln.rttdev as f64) as Mono);
        ln.rtts.max(self.now).map_or(d, |m| d.max(m)) + PROBE_ACTIVE
    }
}

#[cfg(test)]
mod tests;
