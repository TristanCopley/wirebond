//! The read-only view status prints and netsim checks (§14.2–§14.4; Go: core/status.go).
//!
//! Owner: C2.

use crate::estimate::INF;
use crate::flow::Flow;
use crate::lane::LaneIx;
use crate::{
    FlowStatus, LaneOut, LaneStatus, MIN, Mono, Problem, REORDER_LANES, SMALL_WINDOW, STATUS_EVENTS, Session,
    SessionStatus,
};

impl Session {
    /// The read-only view that status prints, with the session's last STATUS_EVENTS events,
    /// oldest first, at the time of the last `handle` (Go: `Snapshot`). Reading it changes
    /// nothing, so it never alters what the session does next: meters are read with `Meter::peek`.
    pub fn snapshot(&self) -> SessionStatus {
        let now = self.now;
        // The base of owdRelMs: the lowest OWD of the lanes with a sample, a fresh echo since the
        // lane was made or the device woke. A lane without one has OWD 0, and an OWD carries the
        // offset between the two devices' clocks, so it must not set the base (R2-14).
        let fastest = self.lanes.iter().filter(|x| x.owds.n > 0).map(|x| x.owd).min();
        let total: u64 = self.lanes.iter().map(|x| x.sent_bytes.wrapping_add(x.stats.rx_bytes)).sum();
        SessionStatus {
            mode: self.mode.as_str().to_string(),
            lanes: self.lanes.ixs().map(|l| self.lane_status(now, l, fastest, total)).collect(),
            flows: self.flows.iter().filter(|f| f.used).map(|f| self.flow_status(now, f)).collect(),
            metered_gb_per_hour: self.metered_bytes.peek(now, MIN) / 8.0 * 3600.0 / 1e9,
            rewinds: self.rewinds,
            unrecovered: self.unrecovered,
            flows_dropped: self.flows_dropped,
            bytes: self.pool.held(),
            last_auth: self.last_auth,
            problems: self.problems(now),
            events: (self.nevents.saturating_sub(STATUS_EVENTS)..self.nevents)
                .map(|i| self.events[i % STATUS_EVENTS].clone())
                .collect(),
        }
    }

    /// One lane's status: its labels and flags, and my sending on it.
    fn lane_status(&self, now: Mono, l: LaneIx, fastest: Option<Mono>, total: u64) -> LaneStatus {
        let x = &self.lanes[l];
        LaneStatus {
            id: x.id,
            kind: if x.mesh { "mesh" } else { "direct" },
            local: if x.mesh { String::new() } else { x.local.clone() },
            remote: if x.mesh { String::new() } else { x.remote.clone() },
            metered: x.metered,
            state: x.state.name(),
            validated: x.validated,
            barred: now < x.bar_until,
            sock: x.sock,
            peer: x.peer,
            out: LaneOut {
                id: x.id,
                role: self.lane_role(l).to_string(),
                rtt_ms: ms(x.rtt),
                // l's OWD above the fastest lane's; 0 for a lane without an OWD sample (R2-14).
                owd_rel_ms: fastest.filter(|_| x.owds.n > 0).map_or(0.0, |f| ms(x.owd.saturating_sub(f))),
                queue_ms: ms(x.qd),
                loss: x.loss,
                rate_mbps: if x.rate < INF { x.rate / 1e6 } else { 0.0 },
                sent_bytes: x.sent_bytes,
                copy_bytes: x.copy_bytes,
            },
            // §11.5.
            tx_bytes: x.sent_bytes,
            rx_bytes: x.stats.rx_bytes,
            share: match total {
                0 => 0.0,
                t => x.sent_bytes.wrapping_add(x.stats.rx_bytes) as f64 / t as f64,
            },
            first_arrivals: x.stats.firsts,
            rescues: x.stats.rescues,
            reorder_wait_p50: x.stats.wait(0.5),
            reorder_wait_p99: x.stats.wait(0.99),
        }
    }

    /// One flow: its class and rate in the busier direction, and the lanes that carry it.
    fn flow_status(&self, now: Mono, f: &Flow) -> FlowStatus {
        let (tx, rx) = (f.tx.peek(now, SMALL_WINDOW), f.rx.peek(now, SMALL_WINDOW));
        let carried = |l: LaneIx| {
            (if f.small { self.lanes[l].in_copy } else { self.carries(l) })
                || f.lanes.iter().any(|e| e.l == Some(l) && now.saturating_sub(e.at) <= REORDER_LANES)
        };
        FlowStatus {
            flow: f.id,
            tx: f.mine,
            key: f.key,
            class: if (tx >= rx && f.small) || (rx > tx && f.rx_small) { "small" } else { "bulk" },
            mbps: tx.max(rx) / 1e6,
            lanes: self.lanes.ixs().filter(|&l| carried(l)).map(|l| self.lanes[l].id).collect(),
        }
    }

    /// l's part in my sending (§14.3): spread only while this side sends bulk (every packet in
    /// performance, a share in bandwidth), else copies or standby. A client sending only small
    /// input still has a spread chosen; calling it that told status the bulk went this way (R2-06).
    pub(crate) fn lane_role(&self, l: LaneIx) -> &'static str {
        match self.bulk_flow() {
            Some(_) if self.carries(l) => "spread",
            _ if self.lanes[l].in_copy => "copies",
            _ => "standby",
        }
    }

    /// Problems the engine detects (§17).
    pub(crate) fn problems(&self, _now: Mono) -> Vec<Problem> {
        Vec::new() // 2.0: §17's problems are the daemon's; the engine raises none yet
    }
}

/// Nanoseconds as milliseconds.
fn ms(d: Mono) -> f64 {
    d as f64 / 1e6
}

#[cfg(test)]
mod tests;
