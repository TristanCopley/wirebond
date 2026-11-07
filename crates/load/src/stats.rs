//! Percentiles, rounding, and the CPU and RSS of processes from /proc.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::json::{Json, obj};

/// Three decimals: enough for milliseconds and percentages.
pub fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// The q-quantile of sorted xs by nearest rank; 0 for none.
pub fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = (q * sorted.len() as f64 + 0.5) as usize;
    sorted[i.saturating_sub(1).min(sorted.len() - 1)]
}

/// Sorts xs and returns its p50, p99, p99.9 and maximum, rounded.
pub fn quantiles(xs: &mut [f64]) -> [f64; 4] {
    xs.sort_by(f64::total_cmp);
    let max = xs.last().copied().unwrap_or(0.0);
    [pct(xs, 0.5), pct(xs, 0.99), pct(xs, 0.999), max].map(round3)
}

/// One process to sample, from `--pids name=pid,...`.
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub name: String,
    pub pid: u32,
}

pub fn parse_pids(s: &str) -> Result<Vec<Proc>, String> {
    s.split(',')
        .filter(|kv| !kv.trim().is_empty())
        .map(|kv| {
            let (name, pid) = kv.split_once('=').ok_or_else(|| format!("--pids: {kv:?} is not name=pid"))?;
            let pid = pid.trim().parse().map_err(|_| format!("--pids: {pid:?} is not a pid"))?;
            Ok(Proc { name: name.trim().to_string(), pid })
        })
        .collect()
}

/// CPU time (utime + stime, in clock ticks) from the text of /proc/<pid>/stat.
pub fn stat_ticks(stat: &str) -> Option<u64> {
    // comm, field 2, may hold spaces and parentheses: the fields after it start at the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // f[0] is field 3 (state); utime and stime are fields 14 and 15.
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

/// The resident set in kB from the text of /proc/<pid>/status.
pub fn status_rss_kb(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Clock ticks per second of /proc's CPU times (USER_HZ): 100 on every Linux architecture's ABI.
const USER_HZ: f64 = 100.0;

#[derive(Clone, Copy, Debug, Default)]
struct Usage {
    ticks: u64,
    rss_kb: u64,
}

fn usage(pid: u32) -> Option<Usage> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    Some(Usage { ticks: stat_ticks(&stat)?, rss_kb: status_rss_kb(&status).unwrap_or(0) })
}

/// Per process: the first and last readings, and the peak resident set in kB.
type Samples = Vec<(Option<Usage>, Option<Usage>, u64)>;

/// Samples processes every 100 ms on its own thread until `stop`: each one's CPU as a percentage
/// of one core over the whole time, and its peak resident set.
pub struct Meter {
    procs: Vec<Proc>,
    quit: Arc<AtomicBool>,
    thread: Option<JoinHandle<Samples>>,
    t0: Instant,
}

impl Meter {
    pub fn start(procs: &[Proc]) -> Meter {
        let quit = Arc::new(AtomicBool::new(false));
        let (q, ps) = (quit.clone(), procs.to_vec());
        let t0 = Instant::now();
        let thread = (!ps.is_empty()).then(|| {
            thread::spawn(move || {
                let mut s: Samples = ps
                    .iter()
                    .map(|p| {
                        let u = usage(p.pid);
                        (u, u, u.map_or(0, |u| u.rss_kb))
                    })
                    .collect();
                loop {
                    let done = q.load(Ordering::Relaxed);
                    for (p, (first, last, peak)) in ps.iter().zip(s.iter_mut()) {
                        if let Some(u) = usage(p.pid) {
                            first.get_or_insert(u);
                            *last = Some(u);
                            *peak = (*peak).max(u.rss_kb);
                        }
                    }
                    if done {
                        return s;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            })
        });
        Meter { procs: procs.to_vec(), quit, thread, t0 }
    }

    /// Ends sampling: `{name: {cpu, rss_mb}}`, or null with nothing sampled. A process that
    /// could not be read has nulls.
    pub fn stop(self) -> Json {
        self.quit.store(true, Ordering::Relaxed);
        let Some(t) = self.thread else { return Json::Null };
        let samples = t.join().unwrap_or_default();
        let secs = self.t0.elapsed().as_secs_f64().max(1e-3);
        let fields = self
            .procs
            .iter()
            .zip(samples)
            .map(|(p, (first, last, peak))| {
                let v = match (first, last) {
                    (Some(f), Some(l)) => obj([
                        ("cpu", round3((l.ticks - f.ticks) as f64 / USER_HZ / secs * 100.0).into()),
                        ("rss_mb", round3(peak as f64 * 1024.0 / 1e6).into()),
                    ]),
                    _ => obj([("cpu", Json::Null), ("rss_mb", Json::Null)]),
                };
                (p.name.clone(), v)
            })
            .collect();
        Json::Obj(fields)
    }
}

/// The machine's 1-minute load average: other work on the machine shows in the tails.
pub fn loadavg() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|x| x.parse().ok()))
        .unwrap_or(0.0)
}
