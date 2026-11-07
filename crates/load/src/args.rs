//! Command-line options: `--name value` or `--name=value`, boolean `--name`, repeatable options
//! (every value kept), and what follows `--` kept as it is.

use std::path::PathBuf;
use std::time::Duration;

use crate::report::Common;

#[derive(Debug, Default)]
pub struct Args {
    opts: Vec<(String, String)>,
    /// What followed `--`.
    pub rest: Vec<String>,
}

/// Options every measurement takes, besides its own.
pub const COMMON: [&str; 3] = ["out", "label", "check"];
pub const COMMON_BOOLS: [&str; 1] = ["table"];

impl Args {
    /// Parses args, which may use only the options in `values` (each takes a value) and `bools`.
    pub fn parse(args: &[String], values: &[&str], bools: &[&str]) -> Result<Args, String> {
        let mut a = Args::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if arg == "--" {
                a.rest = it.by_ref().cloned().collect();
                break;
            }
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!("unexpected argument {arg:?}"));
            };
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (name, None),
            };
            if bools.contains(&name) || COMMON_BOOLS.contains(&name) {
                a.opts.push((name.into(), inline.unwrap_or_else(|| "true".into())));
            } else if values.contains(&name) || COMMON.contains(&name) {
                let v = match inline {
                    Some(v) => v,
                    None => it.next().cloned().ok_or_else(|| format!("--{name} needs a value"))?,
                };
                a.opts.push((name.into(), v));
            } else {
                return Err(format!("no option --{name}"));
            }
        }
        Ok(a)
    }

    /// The last value given for name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.opts.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// Every value given for name, in order.
    pub fn all(&self, name: &str) -> Vec<String> {
        self.opts.iter().filter(|(n, _)| n == name).map(|(_, v)| v.clone()).collect()
    }

    pub fn flag(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| v != "false")
    }

    pub fn str_or(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or(default).to_string()
    }

    pub fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| format!("--{name}: {v:?} is not a number")),
        }
    }

    pub fn dur(&self, name: &str, default: Duration) -> Result<Duration, String> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => duration(v).ok_or_else(|| format!("--{name}: {v:?} is not a duration (10, 10s, 250ms, 2m)")),
        }
    }

    /// A required option.
    pub fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name).ok_or_else(|| format!("--{name} is required"))
    }

    pub fn common(&self) -> Common {
        Common {
            out: self.get("out").map(PathBuf::from),
            label: self.str_or("label", ""),
            checks: self.all("check"),
            table: self.flag("table"),
        }
    }
}

/// A duration: seconds by default, or with a unit: `250ms`, `10s`, `2m`, `1h`.
pub fn duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (n, scale) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1e-3)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (s, 1.0)
    };
    let x: f64 = n.trim().parse().ok()?;
    (x.is_finite() && x >= 0.0).then(|| Duration::from_secs_f64(x * scale))
}
