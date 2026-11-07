//! `wait`: runs a command that prints JSON (`wirebond status --json`), or reads a JSON file (the
//! echo's counters), until every condition holds or the time runs out. Conditions are checks
//! (report.rs) on the document.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::json::{self, Json, obj};
use crate::report;
use crate::stats::round3;

pub enum Source {
    Cmd(Vec<String>),
    File(std::path::PathBuf),
}

/// The document: the last JSON line the command printed, or the file.
pub fn read(src: &Source) -> Result<Json, String> {
    let text = match src {
        Source::Cmd(argv) => {
            let (prog, args) = argv.split_first().ok_or("no command after --")?;
            let out = Command::new(prog).args(args).output().map_err(|e| format!("{prog}: {e}"))?;
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        Source::File(p) => std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?,
    };
    json::lines(&text).pop().ok_or_else(|| "no JSON in its output".to_string())
}

/// The conditions not met by doc.
pub fn unmet(doc: &Json, conds: &[String]) -> Vec<String> {
    let flat = doc.flatten();
    conds.iter().filter(|c| !matches!(report::check(&flat, c), Ok((true, _)))).cloned().collect()
}

/// Polls every `every` until every condition holds: `ok`, the time it took, the tries, and the
/// conditions still unmet with the last error.
pub fn wait(src: &Source, conds: &[String], timeout: Duration, every: Duration) -> Json {
    let t0 = Instant::now();
    let mut tries = 0u64;
    loop {
        tries += 1;
        let (left, err) = match read(src) {
            Ok(doc) => (unmet(&doc, conds), None),
            Err(e) => (conds.to_vec(), Some(e)),
        };
        let ms = round3(t0.elapsed().as_secs_f64() * 1000.0);
        if left.is_empty() || t0.elapsed() >= timeout {
            let mut r = obj([
                ("ok", left.is_empty().into()),
                ("ms", ms.into()),
                ("tries", tries.into()),
                ("for", conds.to_vec().into()),
                ("unmet", left.into()),
            ]);
            if let Some(e) = err {
                r.set("err", e);
            }
            return r;
        }
        thread::sleep(every.min(timeout.saturating_sub(t0.elapsed())));
    }
}
