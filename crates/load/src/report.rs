//! Result lines, their checks, and tables for people.
//!
//! A measurement is one JSON object. `emit` adds `kind`, `label` and `at`, evaluates each
//! `--check` against it, records `checks`, `pass` and `failed_checks`, prints the line (or a table
//! with `--table`) and appends the line to `--out`. The exit status is the line's verdict.
//!
//! A check is `FIELD OP VALUE` with OP one of `<= >= == != < >`. FIELD is a path into the line,
//! object keys and array indexes joined by dots (`procs.client.cpu`, `bond.lanes.0.state`); `*`
//! matches any run of characters, and the check then holds if any matching field meets it. A
//! leading `!` negates: no matching field meets it. A missing field reads as 0, so
//! `udp:47998:10.77.0.2.pkts==0` holds when nothing came from there. VALUE compares as a number
//! when both sides are numbers (a bool reads as 0 or 1), else as text (`==` and `!=` only).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::json::Json;

/// The flags every measurement takes.
#[derive(Clone, Debug, Default)]
pub struct Common {
    pub out: Option<PathBuf>,
    pub label: String,
    pub checks: Vec<String>,
    pub table: bool,
}

const OPS: [&str; 6] = ["<=", ">=", "==", "!=", "<", ">"];

/// A check split into (negated, field, op, value).
pub fn split(expr: &str) -> Result<(bool, &str, &'static str, &str), String> {
    let (neg, e) = match expr.trim().strip_prefix('!') {
        Some(rest) if !rest.starts_with('=') => (true, rest),
        _ => (false, expr.trim()),
    };
    // The earliest operator; a two-character one wins over its first character.
    let mut best: Option<(usize, &'static str)> = None;
    for op in OPS {
        if let Some(i) = e.find(op)
            && best.is_none_or(|(j, o)| i < j || (i == j && op.len() > o.len()))
        {
            best = Some((i, op));
        }
    }
    let (i, op) = best.ok_or_else(|| format!("check {expr:?} has no operator (<= >= == != < >)"))?;
    let field = e[..i].trim();
    if field.is_empty() {
        return Err(format!("check {expr:?} names no field"));
    }
    Ok((neg, field, op, e[i + op.len()..].trim()))
}

/// Whether text matches pattern, where `*` matches any run of characters.
pub fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

fn compare(got: &Json, op: &str, want: &str) -> bool {
    if let (Some(g), Ok(w)) = (got.num(), want.parse::<f64>()) {
        return match op {
            "<=" => g <= w,
            ">=" => g >= w,
            "==" => g == w,
            "!=" => g != w,
            "<" => g < w,
            _ => g > w,
        };
    }
    let g = got.text();
    match op {
        "==" => g == want,
        "!=" => g != want,
        _ => false,
    }
}

/// Evaluates one check against a flattened line, returning the verdict and the values it read.
pub fn check(flat: &[(String, Json)], expr: &str) -> Result<(bool, Vec<String>), String> {
    let (neg, field, op, want) = split(expr)?;
    let zero = Json::Num(0.0);
    let found: Vec<(&str, &Json)> = if field.contains('*') {
        flat.iter().filter(|(k, _)| glob(field, k)).map(|(k, v)| (k.as_str(), v)).collect()
    } else {
        vec![(field, flat.iter().find(|(k, _)| k == field).map_or(&zero, |(_, v)| v))]
    };
    let any = found.iter().any(|(_, v)| compare(v, op, want));
    let seen = found.iter().map(|(k, v)| format!("{k}={}", v.text())).collect();
    Ok((any != neg, seen))
}

/// Seconds since 1970 as RFC 3339 UTC, without a calendar crate (days to civil date: Howard
/// Hinnant's algorithm).
pub fn rfc3339(secs: u64) -> String {
    let (days, rem) = (secs / 86400, secs % 86400);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Completes a result line with its kind, label, time and verdict, prints it and appends it to
/// `--out`. Returns whether every check passed; a bad check is an error, and fails.
pub fn emit(kind: &str, r: Json, c: &Common) -> bool {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut fields = vec![
        ("kind".to_string(), Json::from(kind)),
        ("label".to_string(), Json::from(c.label.as_str())),
        ("at".to_string(), Json::from(rfc3339(now))),
    ];
    if let Json::Obj(own) = r {
        fields.extend(own.into_iter().filter(|(k, _)| !matches!(k.as_str(), "kind" | "label" | "at")));
    }
    let mut line = Json::Obj(fields);
    let mut pass = true;
    if !c.checks.is_empty() {
        let flat = line.flatten();
        let mut failed = Vec::new();
        for chk in &c.checks {
            match check(&flat, chk) {
                Ok((true, _)) => {}
                Ok((false, _)) => failed.push(chk.clone()),
                Err(e) => {
                    eprintln!("load: {e}");
                    failed.push(chk.clone());
                }
            }
        }
        pass = failed.is_empty();
        line.set("checks", c.checks.clone());
        line.set("pass", pass);
        if !pass {
            line.set("failed_checks", failed);
        }
    }
    if c.table {
        print!("{}", table(std::slice::from_ref(&line)));
    } else {
        println!("{line}");
    }
    if let Some(path) = &c.out {
        let appended = OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = appended {
            eprintln!("load: {}: {e}", path.display());
        }
    }
    pass
}

/// A number for a table: at most three significant decimals.
pub fn num(v: Option<&Json>) -> String {
    match v {
        Some(Json::Num(x)) if x.fract() == 0.0 && x.abs() < 1e15 => format!("{}", *x as i64),
        Some(Json::Num(x)) if x.abs() >= 100.0 => format!("{x:.0}"),
        Some(Json::Num(x)) if x.abs() >= 10.0 => format!("{x:.1}"),
        Some(Json::Num(x)) if x.abs() >= 1.0 => format!("{x:.2}"),
        Some(Json::Num(x)) => format!("{x:.3}"),
        Some(Json::Bool(b)) => b.to_string(),
        Some(Json::Str(s)) => s.clone(),
        Some(Json::Null) | None => "-".into(),
        Some(v) => v.to_string(),
    }
}

/// Columns: the first `left` are left-aligned, the rest right-aligned, two spaces apart.
fn grid(left: usize, rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let width: Vec<usize> =
        (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for r in rows {
        let cells: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(c, s)| if c < left { format!("{s:<w$}", w = width[c]) } else { format!("{s:>w$}", w = width[c]) })
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// The sampled processes of some rows, in order of first appearance.
fn proc_names(rows: &[&Json]) -> Vec<String> {
    let mut names = Vec::new();
    for r in rows {
        if let Some(Json::Obj(ps)) = r.get("procs") {
            for (n, _) in ps {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            }
        }
    }
    names
}

fn section(
    title: &str,
    head: &[&str],
    unit: &[&str],
    kind: &str,
    rows: &[Json],
    cells: impl Fn(&Json) -> Vec<String>,
) -> String {
    let mine: Vec<&Json> = rows.iter().filter(|r| r.get("kind").and_then(Json::str) == Some(kind)).collect();
    if mine.is_empty() {
        return String::new();
    }
    let names = proc_names(&mine);
    let mut h: Vec<String> = head.iter().map(|s| s.to_string()).collect();
    let mut u: Vec<String> = unit.iter().map(|s| s.to_string()).collect();
    for n in &names {
        h.extend([format!("{n} CPU"), format!("{n} RSS")]);
        u.extend(["% core".to_string(), "MB".to_string()]);
    }
    let mut grid_rows = vec![h, u];
    for r in mine {
        let mut row = vec![num(r.get("label"))];
        row.extend(cells(r));
        for n in &names {
            let p = r.get("procs").and_then(|ps| ps.get(n));
            row.push(num(p.and_then(|p| p.get("cpu"))));
            row.push(num(p.and_then(|p| p.get("rss_mb"))));
        }
        grid_rows.push(row);
    }
    format!("{title}\n{}\n", grid(1, &grid_rows))
}

/// Results as tables: the UDP runs, the video runs, the TCP transfers and the samples, then every
/// row that carries checks, with its verdict and the values the checks read.
pub fn table(rows: &[Json]) -> String {
    let g = |r: &Json, k: &str| num(r.get(k));
    let mut out = String::new();
    out += &section(
        "udp: paced datagrams through an echo (round trip)",
        &["label", "size", "offered", "sent", "echoed", "loss", "p50", "p99", "p99.9", "max", "gap"],
        &["", "B", "Mbps", "Mbps", "Mbps", "%", "ms", "ms", "ms", "ms", "ms"],
        "udp",
        rows,
        |r| {
            let mut offered = g(r, "target_mbps");
            if r.get("limited") == Some(&Json::Bool(true)) {
                offered += " (short)";
            }
            vec![
                g(r, "size"),
                offered,
                g(r, "sent_mbps"),
                g(r, "recv_mbps"),
                g(r, "loss_pct"),
                g(r, "p50_ms"),
                g(r, "p99_ms"),
                g(r, "p999_ms"),
                g(r, "max_ms"),
                g(r, "gap_ms"),
            ]
        },
    );
    out += &section(
        "video: frames through an echo (round trip; decodable with any `need` shards of a frame)",
        &["label", "rate", "fps", "frames", "decodable", "unrecovered", "loss", "p50", "p99", "p99.9", "worst", "gap"],
        &["", "Mbps", "", "", "", "", "%", "ms", "ms", "ms", "ms", "ms"],
        "video",
        rows,
        |r| {
            vec![
                g(r, "mbps"),
                g(r, "fps"),
                g(r, "frames"),
                g(r, "decodable"),
                g(r, "unrecovered"),
                g(r, "loss_pct"),
                g(r, "frame_p50_ms"),
                g(r, "frame_p99_ms"),
                g(r, "frame_p999_ms"),
                g(r, "frame_max_ms"),
                g(r, "gap_ms"),
            ]
        },
    );
    out += &section(
        "tcp: a bulk transfer through an echo",
        &["label", "to", "bytes", "secs", "throughput", "ok"],
        &["", "", "MB", "s", "Mbps", ""],
        "tcp",
        rows,
        |r| {
            let mb = r.get("bytes").and_then(Json::num).map(|b| Json::Num(b / 1e6));
            vec![g(r, "to"), num(mb.as_ref()), g(r, "secs"), g(r, "mbps"), g(r, "ok")]
        },
    );
    out += &section("sample: CPU and peak RSS", &["label", "secs"], &["", "s"], "sample", rows, |r| vec![g(r, "secs")]);
    let mut verdicts = vec![vec!["label".to_string(), "kind".into(), "verdict".into(), "checked".into()]];
    for r in rows {
        let Some(Json::Arr(checks)) = r.get("checks") else { continue };
        let flat = r.flatten();
        let mut pass = true;
        let mut read = Vec::new();
        for c in checks.iter().filter_map(Json::str) {
            let (ok, seen) = check(&flat, c).unwrap_or((false, vec![]));
            pass &= ok;
            read.push(format!("{} ({c}){}", seen.join(" "), if ok { "" } else { " FAILED" }));
        }
        if pass && r.get("pass") != Some(&Json::Bool(true)) {
            pass = false; // recorded as failed by its writer
            read.push("recorded as failed".into());
        }
        let verdict = if pass { "pass" } else { "FAIL" };
        verdicts.push(vec![num(r.get("label")), num(r.get("kind")), verdict.into(), read.join(", ")]);
    }
    if verdicts.len() > 1 {
        out += &format!("checks\n{}", grid(4, &verdicts));
    }
    out
}

/// Whether every row with checks passed.
pub fn all_pass(rows: &[Json]) -> bool {
    rows.iter().filter(|r| r.get("checks").is_some()).all(|r| r.get("pass") == Some(&Json::Bool(true)))
}
