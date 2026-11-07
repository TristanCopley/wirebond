//! Mahimahi delivery traces (Go netsim/trace.go) and the T-Mobile trace files CI fetches
//! (models_test.go's loadTraces). Errors are Strings throughout netsim. Owner: N1.

use std::collections::BTreeMap;
use std::io::{BufRead, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};

use crate::sim::{Dur, MS, hex, secs};

/// A Mahimahi delivery trace: each line is a millisecond at which the link may deliver 1,500
/// bytes, and the trace loops at its last line [Testing 4]. Unused opportunities are lost.
/// Shared between directions and seed threads as `Arc<Trace>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    pub(crate) ms: Vec<i64>,
    pub(crate) period: Dur,
}

/// A direction's position in its trace (Go Dir.topp, Dir.left): the opportunity in use, counting
/// across loops, and the bytes it has left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TracePos {
    pub(crate) opp: usize,
    pub(crate) left: usize,
}

impl Default for TracePos {
    fn default() -> TracePos {
        TracePos { opp: 0, left: 1500 }
    }
}

impl Trace {
    /// Reads a Mahimahi trace: nondecreasing non-negative milliseconds, blank lines skipped, not
    /// empty and not lasting 0 ms (Go ParseTrace). A millisecond must fit a [`Dur`] in
    /// nanoseconds (292 years), where Go's arithmetic would have wrapped.
    pub fn parse(mut r: impl BufRead) -> Result<Trace, String> {
        const MAX: i64 = Dur::MAX / MS;
        let mut ms: Vec<i64> = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                break;
            }
            let s = line.trim();
            if s.is_empty() {
                continue;
            }
            match s.parse::<i64>() {
                Ok(v) if (0..=MAX).contains(&v) && ms.last().is_none_or(|&l| v >= l) => ms.push(v),
                _ => {
                    return Err(format!("trace line {}: {s:?} is not a nondecreasing millisecond", ms.len() + 1));
                }
            }
        }
        match ms.last() {
            Some(&last) if last > 0 => Ok(Trace { period: last * MS, ms }),
            _ => Err("trace is empty or lasts 0 ms".into()),
        }
    }

    /// The trace's average rate in bits per second.
    pub fn rate(&self) -> f64 {
        self.ms.len() as f64 * 1500.0 * 8.0 / secs(self.period)
    }

    /// How long one loop lasts.
    pub fn period(&self) -> Dur {
        self.period
    }

    /// The time of opportunity i, counting across loops.
    pub(crate) fn at(&self, i: usize) -> Dur {
        let (k, j) = (i / self.ms.len(), i % self.ms.len());
        k as Dur * self.period + self.ms[j] * MS
    }

    /// When the head packet of n bytes has been given enough opportunities, from now (Go ready).
    pub(crate) fn ready(&self, pos: &mut TracePos, now: Dur, n: usize) -> Dur {
        let k = (now / self.period) as usize;
        let off = now - k as Dur * self.period;
        let j = self.ms.partition_point(|&m| m * MS < off);
        let i = k * self.ms.len() + j;
        if i > pos.opp {
            *pos = TracePos { opp: i, left: 1500 };
        }
        let mut n = n;
        while n > pos.left {
            n -= pos.left;
            pos.opp += 1;
            pos.left = 1500;
        }
        pos.left -= n;
        self.at(pos.opp)
    }
}

/// The trace checksums, crates/netsim/traces.sum, which CI reads to fetch the traces (SPEC §17.2).
pub fn traces_sum() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("traces.sum")
}

/// Where CI puts the traces, crates/netsim/testdata/traces (not committed).
pub fn traces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/traces")
}

/// A pair of traces: (up, down).
pub type Traces = (Arc<Trace>, Arc<Trace>);

/// The T-Mobile LTE driving traces (up, down): Ok(None) when CI has not fetched them (the caller
/// skips, printing why), Err when a file does not match traces.sum (Go loadTraces). The files are
/// read, checked and parsed once per test process; every caller shares the result.
pub fn load_traces() -> Result<Option<Traces>, String> {
    static TRACES: OnceLock<Result<Option<Traces>, String>> = OnceLock::new();
    TRACES.get_or_init(read_traces).clone()
}

/// Reads and checks the T-Mobile traces (see [`load_traces`]).
fn read_traces() -> Result<Option<Traces>, String> {
    let sum = traces_sum();
    let text = std::fs::read_to_string(&sum).map_err(|e| format!("{}: {e}", sum.display()))?;
    let mut want = BTreeMap::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        if let (Some(hash), Some(name), None) = (f.next(), f.next(), f.next())
            && hash != "commit"
            && !hash.starts_with('#')
        {
            want.insert(name, hash);
        }
    }
    let mut tr = Vec::with_capacity(2);
    for name in ["TMobile-LTE-driving.up", "TMobile-LTE-driving.down"] {
        let path = traces_dir().join(name);
        let b = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                eprintln!("{} is absent; CI fetches it as traces.sum says", path.display());
                return Ok(None);
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let got = hex(&Sha256::digest(&b));
        let want = want.get(name).copied().unwrap_or("none");
        if got != want {
            return Err(format!("{}: sha256 {got}, traces.sum says {want}", path.display()));
        }
        let t = Trace::parse(&b[..]).map_err(|e| format!("{}: {e}", path.display()))?;
        tr.push(Arc::new(t));
    }
    let down = tr.pop().expect("two traces");
    let up = tr.pop().expect("two traces");
    Ok(Some((up, down)))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::link::tests::test_link;
    use crate::link::{LinkSpec, Profile, Then};
    use crate::node::Pkt;
    use crate::sim::{SEC, fmt_dur};

    /// Go TestTraceReader: rate of "0 1 1 4", a decreasing trace fails, deliveries at 0 1 1 4 4 5 ms.
    #[test]
    fn trace_reader() {
        let tr = Trace::parse("0\n1\n1\n\n4\n".as_bytes()).unwrap();
        let mut errs = Vec::new();
        let r = tr.rate();
        if r != 4.0 * 1500.0 * 8.0 / 0.004 {
            errs.push(format!("rate {r}"));
        }
        if Trace::parse("5\n3\n".as_bytes()).is_ok() {
            errs.push("a decreasing trace parsed".to_string());
        }
        for bad in ["", "\n \n", "0\n0\n", "-1\n2\n", "1\nx\n", "1\n9223372036855\n"] {
            if let Ok(t) = Trace::parse(bad.as_bytes()) {
                errs.push(format!("{bad:?} parsed as {t:?}"));
            }
        }
        let p = Profile { trace: Some(Arc::new(tr)), ..Profile::default() };
        let (mut w, l) = test_link(p, LinkSpec::default());
        let at: Rc<RefCell<Vec<Dur>>> = Rc::default();
        for _ in 0..6 {
            let at = at.clone();
            let then = Then::Call(Box::new(move |w, _| at.borrow_mut().push(w.now())));
            w.dir_send(l.up(), Pkt::default(), 1500, 1, then);
        }
        w.run(SEC).unwrap();
        // The trace loops at 4 ms.
        let want = [0, 1, 1, 4, 4, 5].map(|x| x * MS);
        let at = at.take();
        if at != want {
            errs.push(format!("trace deliveries at {at:?}, want {want:?}"));
        }
        assert!(errs.is_empty(), "{}", errs.join("\n"));
    }

    /// Not in Go: a packet larger than one opportunity takes several; one smaller shares it with
    /// the next; an opportunity nobody used is lost.
    #[test]
    fn trace_opportunities() {
        let tr = Trace::parse("2\n4\n6\n8\n".as_bytes()).unwrap();
        let mut pos = TracePos::default();
        assert_eq!(tr.ready(&mut pos, 0, 2000), 4 * MS);
        assert_eq!(tr.ready(&mut pos, 0, 500), 4 * MS);
        assert_eq!(tr.ready(&mut pos, 0, 600), 6 * MS);
        // At 7 ms the 6 ms opportunity is gone: the next is 8 ms, then 8 + 2 ms in the next loop.
        assert_eq!(tr.ready(&mut pos, 7 * MS, 1500), 8 * MS);
        assert_eq!(tr.ready(&mut pos, 7 * MS, 1), 10 * MS);
        assert_eq!(tr.period(), 8 * MS);
    }

    /// Go TestTraceFiles: both T-Mobile traces above 1 Mbps; skips (returns) when absent.
    #[test]
    fn trace_files() {
        let Some((up, down)) = load_traces().unwrap() else {
            return;
        };
        eprintln!(
            "TMobile-LTE-driving: up {:.1} Mbps, down {:.1} Mbps over {}",
            up.rate() / 1e6,
            down.rate() / 1e6,
            fmt_dur(up.period())
        );
        assert!(up.rate() >= 1e6 && down.rate() >= 1e6, "implausible trace rates");
    }
}
