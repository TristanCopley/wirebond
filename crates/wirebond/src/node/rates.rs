//! What a bond's lanes are doing now (SPEC 2.0 §11.5, §20.2): the engine's lane counters only
//! grow, so the daemon keeps a short history of them per bond, and status reads a rate off it:
//! megabits a second over the last few seconds, a share, first arrivals and rescues over the last
//! minute. Owner: node daemon.

use std::collections::VecDeque;

use wirebond_core::{LaneStatus, MIN, MS, Mono, SEC};

/// How far apart the samples are.
const EVERY: Mono = 500 * MS;
/// The span of a lane's `tx_mbps` and `rx_mbps`: long enough to read, short enough to be now.
pub const NOW: Mono = 2 * SEC;
/// The span of a lane's share, first arrivals and rescues (§11.5: per minute).
pub const MINUTE: Mono = MIN;

/// One lane's counters at one time, or their growth between two.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub tx: u64,
    pub rx: u64,
    pub firsts: u64,
    pub rescues: u64,
}

impl Counts {
    fn of(l: &LaneStatus) -> Counts {
        Counts { tx: l.tx_bytes, rx: l.rx_bytes, firsts: l.first_arrivals, rescues: l.rescues }
    }

    /// The bytes of both directions.
    pub fn bytes(&self) -> u64 {
        self.tx + self.rx
    }

    /// Megabits a second for `bytes` over `span`.
    pub fn mbps(bytes: u64, span: Mono) -> f64 {
        if span <= 0 { 0.0 } else { bytes as f64 * 8.0 / (span as f64 / SEC as f64) / 1e6 }
    }
}

/// One session's lane counters over the last minute, a sample every `EVERY`.
#[derive(Debug, Default)]
pub struct Rates {
    samples: VecDeque<(Mono, Vec<(u16, Counts)>)>,
}

impl Rates {
    /// Keeps the lanes' counters at `now`, if the last sample is `EVERY` old; forgets what is
    /// older than a minute, but for the one sample that still spans it.
    pub fn sample(&mut self, now: Mono, lanes: &[LaneStatus]) {
        if self.samples.back().is_some_and(|(at, _)| now - at < EVERY) {
            return;
        }
        self.samples.push_back((now, lanes.iter().map(|l| (l.id, Counts::of(l))).collect()));
        while self.samples.get(1).is_some_and(|(at, _)| now - at >= MINUTE) {
            self.samples.pop_front();
        }
    }

    /// How much lane `l`'s counters grew over the last `span`, and over how long exactly: since
    /// the newest sample at least `span` old, or the oldest while the history is shorter. A lane
    /// that sample does not have was made after it, so it grew from nothing. None without a
    /// sample older than `now`.
    pub fn over(&self, now: Mono, span: Mono, l: &LaneStatus) -> Option<(Mono, Counts)> {
        let old = self.samples.iter().rev().find(|(at, _)| now - at >= span).or(self.samples.front())?;
        let (at, lanes) = old;
        let was = lanes.iter().find(|(id, _)| *id == l.id).map_or(Counts::default(), |(_, c)| *c);
        let (is, grew) = (Counts::of(l), |a: u64, b: u64| a.saturating_sub(b));
        let d = Counts {
            tx: grew(is.tx, was.tx),
            rx: grew(is.rx, was.rx),
            firsts: grew(is.firsts, was.firsts),
            rescues: grew(is.rescues, was.rescues),
        };
        (now > *at).then_some((now - at, d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(id: u16, tx: u64, rx: u64, firsts: u64, rescues: u64) -> LaneStatus {
        LaneStatus { id, tx_bytes: tx, rx_bytes: rx, first_arrivals: firsts, rescues, ..LaneStatus::default() }
    }

    /// A lane receiving 30 Mbit/s and sending 2 Mbit/s for two minutes, then nothing: the rates
    /// are those of the last two seconds, not an average since the bond opened, and the rescues
    /// those of the last minute, not the running count.
    #[test]
    fn rates_are_over_a_window_not_since_the_start() {
        let mut r = Rates::default();
        let (rx, tx) = (30_000_000 / 8 / 10, 2_000_000 / 8 / 10); // bytes each 100 ms
        let at = |i: u64| lane(257, tx * i, rx * i, 10 * i, i / 10); // a rescue a second
        assert_eq!(r.over(0, NOW, &at(0)), None, "no history yet");
        for i in 0..=1200u64 {
            r.sample(i as Mono * 100 * MS, &[at(i)]);
        }
        let now = 120 * SEC;
        let (span, d) = r.over(now, NOW, &at(1200)).unwrap();
        assert_eq!(span, NOW);
        assert!((Counts::mbps(d.rx, span) - 30.0).abs() < 1e-9 && (Counts::mbps(d.tx, span) - 2.0).abs() < 1e-9);
        let (span, d) = r.over(now, MINUTE, &at(1200)).unwrap();
        assert_eq!((span, d.rescues, d.firsts), (MINUTE, 60, 6000), "the last minute's, of 120 in all");
        // Between samples the span is exact, so the rate does not sag and jump.
        let (span, d) = r.over(now + 300 * MS, NOW, &at(1203)).unwrap();
        assert_eq!(span, 2300 * MS);
        assert!((Counts::mbps(d.rx, span) - 30.0).abs() < 1e-9);
        // The stream stops: within the window the rate is nothing again.
        for i in 1201..=1230u64 {
            r.sample(i as Mono * 100 * MS, &[at(1200)]);
        }
        let (span, d) = r.over(123 * SEC, NOW, &at(1200)).unwrap();
        assert_eq!((Counts::mbps(d.rx, span), Counts::mbps(d.tx, span)), (0.0, 0.0));
        assert!(r.samples.len() <= (MINUTE / EVERY) as usize + 2, "{} samples kept", r.samples.len());
    }

    /// A lane made after the old sample grew from nothing; one whose counters began again (a new
    /// session under the same id) shows nothing rather than a negative rate; a young history
    /// gives what it has.
    #[test]
    fn new_lanes_and_short_histories() {
        let mut r = Rates::default();
        r.sample(0, &[lane(257, 1000, 1000, 0, 0)]);
        r.sample(SEC, &[lane(257, 2000, 2000, 0, 0)]);
        let (span, d) = r.over(SEC, MINUTE, &lane(513, 500, 0, 3, 1)).unwrap();
        assert_eq!((span, d), (SEC, Counts { tx: 500, rx: 0, firsts: 3, rescues: 1 }));
        let (_, d) = r.over(SEC + 1, NOW, &lane(257, 10, 10, 0, 0)).unwrap();
        assert_eq!(d.bytes(), 0);
        assert_eq!(Counts::mbps(1_000_000, 0), 0.0);
    }
}
