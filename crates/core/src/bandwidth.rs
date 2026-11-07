//! The `bandwidth` scheduler (§14.2): bulk spread over the eligible lanes, at most one per
//! bottleneck group, by earliest arrival. Its spread, blind pair included, is also performance's
//! (§14.1), which sends every packet on all of it (performance.rs).
//!
//! Eligible lanes are up (not suspect), validated and not barred; one per group, its lowest base
//! OWD; the mesh lane only for a group with no usable direct lane; in bandwidth, within
//! BANDWIDTH_SKEW_MAX of the fastest's base OWD and carrying at least a tenth of the fastest lane's
//! rate. While blind (§11.4) the spread is the blind pair `two()`, paced, with no rewind and no
//! bar; the normal rules resume when echoes return. A lane that turns suspect leaves the spread
//! and, in bandwidth, its unacknowledged packets of the last REWIND_WINDOW are re-sent across the
//! lanes that stay (rewind.rs).

use crate::lane::{Lane, LaneIx, State};
use crate::performance::since;
use crate::session::Mode;
use crate::session::NEVER;
use crate::{BANDWIDTH_FLOOR_HOLD, ECF_MARGIN, MAX_LINKS, MIN_WINDOW, Mono, Out, PACE_FLOOR, RATE_WINDOW, Session};

/// A spread being chosen: its lanes, fastest (lowest base OWD) first, and how many.
type Spread = ([LaneIx; MAX_LINKS], usize);

impl Session {
    /// Chooses the lanes bulk spreads over (§14.1, §14.2, and the blind pair of §11.4), and takes
    /// the lanes that join before any packet moves onto them (R2-04). The new spread is set first,
    /// so what a leaving lane hands on goes across it: in bandwidth a lane that turned suspect is
    /// rewound (§14.2), any other leaving lane's queue moves; in performance a leaving lane's queue
    /// is dropped while another lane stays, as that lane carries the same packets.
    pub(crate) fn schedule(&mut self, out: &mut dyn Out, now: Mono) {
        if !self.active {
            return;
        }
        let blind = self.blind(now);
        let (mut keep, mut n) = if blind { self.blind_spread() } else { self.eligible(now) };
        if n == 0 && !blind {
            // No lane is eligible yet echoes came within BLIND_AFTER: the return paths are failing
            // one by one, so bulk stays paced on the blind pair rather than going to every lane.
            (keep, n) = self.blind_spread();
        }
        let (keep, old) = (&keep[..n], self.spread);
        let old = &old[..self.nspread];
        for &l in keep.iter().filter(|l| !old.contains(l)) {
            self.take(now, l);
        }
        if self.mode == Mode::Bandwidth && n > 1 {
            // A lane that was alone in the spread may have gone unlimited in startup (rate.rs);
            // sharing the stream it paces at a rate again, from what it delivered, still in
            // startup: earliest arrival by an unlimited pace filled its path's buffer, and it
            // turned suspect again and again before its first cut.
            for &l in keep {
                let x = &mut self.lanes[l];
                if !x.rate.is_finite() {
                    (x.rate, x.startup) = (PACE_FLOOR.max(x.delivery), true);
                }
            }
        }
        self.spread[..n].copy_from_slice(keep);
        self.nspread = n;
        let to = keep;
        for &l in old.iter().filter(|l| !keep.contains(l)) {
            if self.mode == Mode::Performance && n > 0 {
                self.rewind(out, now, l, &[]);
            } else if !blind && self.lanes[l].state == State::Suspect {
                self.rewinds += 1;
                self.rewind(out, now, l, to);
            } else {
                self.shift(out, now, l, to);
            }
        }
    }

    /// The spread by §14.1 and §14.2's eligible lanes (module doc).
    ///
    /// A lane in overuse at PACE_FLOOR for BANDWIDTH_FLOOR_HOLD while it carries bulk, which is
    /// what a lane that drops full-size packets looks like, is barred (§14.2, leaving the spread).
    /// Its group then takes the mesh lane, whose packets Tailscale fragments inside WireGuard, so
    /// they cross that hop (§5.4; notes/rust.md, review fixes: one-group-fragments).
    fn eligible(&mut self, now: Mono) -> Spread {
        let (mut best, mut mesh) = ([None; MAX_LINKS], None);
        for l in self.lanes.ixs() {
            let x = &self.lanes[l];
            let rtt = if x.rtt > 0 { x.rtt } else { RATE_WINDOW };
            let floor = x.state == State::Up
                && since(now, x.last_over) < rtt.saturating_mul(2)
                && x.rate <= PACE_FLOOR
                && self.carries(l);
            let x = &mut self.lanes[l];
            if !floor {
                x.floor_at = NEVER;
            } else if x.floor_at == NEVER {
                x.floor_at = now;
            } else if since(now, x.floor_at) >= BANDWIDTH_FLOOR_HOLD {
                self.bar(now, l);
            }
            let x = &self.lanes[l];
            if !x.eligible(now) {
                continue;
            }
            if x.mesh {
                mesh = Some(l);
            } else if let Ok(g) = usize::try_from(x.group)
                && g < MAX_LINKS
                && best[g].is_none_or(|b| x.min_owd.get() < self.lanes[b].min_owd.get())
            {
                best[g] = Some(l);
            }
        }
        if let Some(m) = mesh {
            let g = self.lanes[m].group;
            // A usable direct lane is validated and still answers (up or suspect), barred or not:
            // the mesh lane, which crosses its group's link, stays out while one is (§14.1,
            // §14.2; notes/core.md 44a, A5, A6).
            let live = self.lanes.iter().any(|x| !x.mesh && x.usable() && (g < 0 || x.group == g));
            if (-1..MAX_LINKS as i32).contains(&g) && !live {
                best[g.max(0) as usize] = Some(m);
            }
        }
        let (mut ls, mut n) = self.fastest_first(best, self.skew());
        if self.mode == Mode::Bandwidth && n > 1 {
            // At least a tenth of the fastest lane's rate. A rate is known once the lane has
            // carried bulk within MIN_WINDOW and left startup; until then (a lane that joins
            // restarts at PACE_FLOOR, §12) it is not known to be too slow.
            let known = |x: &Lane| !x.startup && since(now, x.bulk_at) < MIN_WINDOW && x.rate.is_finite();
            let fast = &self.lanes[ls[0]];
            if known(fast) {
                let tenth = fast.rate / 10.0;
                let mut k = 1;
                for i in 1..n {
                    let x = &self.lanes[ls[i]];
                    if !known(x) || x.rate >= tenth {
                        ls[k] = ls[i];
                        k += 1;
                    }
                }
                n = k;
            }
        }
        (ls, n)
    }

    /// The blind pair (§11.4): `two()`, fastest first, over the validated lanes that are up,
    /// suspect or down: a lane goes down at DOWN_AFTER without echoes even while blind, and the
    /// pair carries bulk until echoes return. No lane is barred while blind, so the floor rule's
    /// hold starts again when echoes return.
    fn blind_spread(&mut self) -> Spread {
        for l in self.lanes.iter_mut() {
            l.floor_at = NEVER;
        }
        let mut ls = [None; MAX_LINKS];
        let ok = |x: &Lane| x.validated && matches!(x.state, State::Up | State::Suspect | State::Down);
        (ls[0], ls[1]) = self.two_of(ok);
        self.fastest_first(ls, Mono::MAX)
    }

    /// The lanes of `ls` within `skew` of the fastest's base OWD, fastest first, as `earliest`
    /// expects; ties in their order in `ls` (group order). A lane with no base OWD yet (NO_MIN) is
    /// not known to be within the skew. The sort is stable, and on so few elements an insertion
    /// sort that allocates nothing.
    fn fastest_first(&self, mut ls: [Option<LaneIx>; MAX_LINKS], skew: Mono) -> Spread {
        let owd = |l: Option<LaneIx>| l.map(|l| self.lanes[l].min_owd.get());
        ls.sort_by_key(|&l| (l.is_none(), owd(l)));
        let fastest = owd(ls[0]).unwrap_or(0);
        let n = ls.iter().take_while(|&&l| owd(l).is_some_and(|o| o.saturating_sub(fastest) <= skew)).count();
        (ls.map(Option::unwrap_or_default), n)
    }

    /// The spread lane where a bulk packet of `size` bytes would arrive first (§14.2, earliest
    /// arrival): the smallest newest OWD + 2 × dev + (queued bytes + size) ÷ pace; a lane other
    /// than the fastest (the first) must win by ECF_MARGIN, so the first of equals wins. None with
    /// an empty spread.
    pub(crate) fn earliest(&mut self, now: Mono, size: usize) -> Option<LaneIx> {
        let (spread, n) = (self.spread, self.nspread);
        self.earliest_of(now, &spread[..n], size)
    }

    /// `earliest` over the lanes `ls`, fastest first.
    pub(crate) fn earliest_of(&mut self, now: Mono, ls: &[LaneIx], size: usize) -> Option<LaneIx> {
        let mut at = ls.iter().map(|&l| (self.arrival(now, l, size), l));
        let (fastest, first) = at.next()?;
        let best = at.min_by_key(|&(t, _)| t).filter(|&(t, _)| t <= fastest.saturating_sub(ECF_MARGIN));
        Some(best.map_or(first, |(_, l)| l))
    }

    /// When a bulk packet of `size` bytes queued on `l` now would arrive, on the peer's clock:
    /// l's newest echoed OWD + 2 × dev + (queued bytes + size) ÷ pace (§14.2). The newest OWD, not
    /// the base OWD, since it carries the standing queue of the lane's path, which the base OWD
    /// does not see: by the base OWD alone, a lane whose path buffer holds 60 ms won packets that
    /// then arrived later than REORDER_MAX behind their gaps. 2 × dev covers the lane's jitter.
    fn arrival(&mut self, now: Mono, l: LaneIx, size: usize) -> Mono {
        let x = &mut self.lanes[l];
        let wait = ((x.q_bytes + size) as f64 * 8e9 / x.pace(now)) as Mono;
        x.owd_now.saturating_add(x.dev.saturating_mul(2)).saturating_add(wait)
    }
}

impl Lane {
    /// Up (not suspect), validated and not barred: the first rule of §14.1's spread and §14.2's
    /// eligible lanes.
    pub(crate) fn eligible(&self, now: Mono) -> bool {
        self.state == State::Up && self.validated && now >= self.bar_until
    }
}

#[cfg(test)]
mod tests;
