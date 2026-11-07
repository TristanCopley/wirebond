//! `load`: see lib.rs and USAGE.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use load::args::Args;
use load::json::{self, Json, obj};
use load::stats::{Meter, parse_pids};
use load::tcp::{TcpConfig, tcp};
use load::udp::{UdpConfig, VideoConfig, udp, video};
use load::wait::{Source, wait};
use load::{echo, report};

const USAGE: &str = "\
load: wirebond's load and echo tool (SPEC §23.2)

  load echo [--udp PORTS] [--tcp PORTS] [--bind ADDR] [--stats FILE]
        UDP and TCP echo on every address (or ADDR); counts by protocol, port and source address,
        written to FILE every 200 ms. Default ports: UDP 47998, TCP 47989.
  load udp --to ADDR:PORT [--mbps 10] [--size 1400] [--duration 10s] [--from ADDR] [--pids N=PID,...]
        paced datagrams through an echo: loss, round trip p50/p99/p99.9/max, longest gap
  load video --to ADDR:PORT [--mbps 30] [--fps 120] [--size 1400] [--fec 20] [--duration 10s] ...
        video frames through an echo: decodable and unrecovered frames, frame delay percentiles
  load tcp --to ADDR:PORT [--bytes N | --duration 10s] [--timeout 2m] [--pids N=PID,...]
        one bulk transfer through a TCP echo, every byte checked: throughput
  load sample --pids N=PID,... [--duration 10s]
        CPU (% of one core) and peak RSS of processes
  load wait (--file FILE | -- CMD...) --for COND... [--timeout 10s] [--every 200ms]
        until the JSON a command prints (or a file holds) meets every condition
  load check FILE --check COND...      a verdict on a JSON file, such as the echo's counters
  load table FILE...                   results files as tables; exit 1 if a recorded check failed

Every measurement takes --label L, --out FILE (append the JSON line), --check 'FIELD OP VALUE'
(OP: <= >= == != < >; '*' in FIELD matches any run; a leading ! negates) and --table.
";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = argv.split_first() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let r = match cmd.as_str() {
        "echo" => cmd_echo(rest),
        "udp" => cmd_udp(rest),
        "video" => cmd_video(rest),
        "tcp" => cmd_tcp(rest),
        "sample" => cmd_sample(rest),
        "wait" => cmd_wait(rest),
        "check" => cmd_check(rest),
        "table" => cmd_table(rest),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(true)
        }
        _ => Err(format!("no command {cmd:?}")),
    };
    match r {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("load {cmd}: {e}");
            ExitCode::from(2)
        }
    }
}

/// An address and port; a name resolves (first address).
fn addr(s: &str) -> Result<SocketAddr, String> {
    s.parse().or_else(|_| {
        s.to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| format!("{s:?} is not an address:port"))
    })
}

fn ip(a: &Args, name: &str) -> Result<Option<IpAddr>, String> {
    a.get(name).map(|s| s.parse().map_err(|_| format!("--{name}: {s:?} is not an address"))).transpose()
}

fn ports(s: &str) -> Result<Vec<u16>, String> {
    s.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| p.trim().parse().map_err(|_| format!("bad port {p:?}")))
        .collect()
}

fn cmd_echo(rest: &[String]) -> Result<bool, String> {
    let a = Args::parse(rest, &["udp", "tcp", "bind", "stats"], &[])?;
    let e = echo::Echo::start(ip(&a, "bind")?, &ports(&a.str_or("udp", "47998"))?, &ports(&a.str_or("tcp", "47989"))?)
        .map_err(|e| e.to_string())?;
    let bound: Vec<String> =
        e.udp.iter().map(|s| format!("udp {s}")).chain(e.tcp.iter().map(|s| format!("tcp {s}"))).collect();
    eprintln!("load echo: {}", bound.join(", "));
    echo::serve(&e, a.get("stats").map(Path::new))
}

const MEASURE: [&str; 9] = ["to", "from", "mbps", "size", "duration", "pids", "fps", "fec", "bytes"];

fn cmd_udp(rest: &[String]) -> Result<bool, String> {
    let a = Args::parse(rest, &MEASURE[..6], &[])?;
    let c = UdpConfig {
        to: addr(a.need("to")?)?,
        from: ip(&a, "from")?,
        mbps: a.num("mbps", 10.0)?,
        size: a.num("size", 1400)?,
        duration: a.dur("duration", Duration::from_secs(10))?,
        procs: parse_pids(&a.str_or("pids", ""))?,
    };
    if c.mbps <= 0.0 {
        return Err("--mbps must be above 0".into());
    }
    let r = udp(&c).map_err(|e| e.to_string())?;
    Ok(report::emit("udp", r, &a.common()))
}

fn cmd_video(rest: &[String]) -> Result<bool, String> {
    let a = Args::parse(rest, &MEASURE[..8], &[])?;
    let c = VideoConfig {
        to: addr(a.need("to")?)?,
        from: ip(&a, "from")?,
        mbps: a.num("mbps", 30.0)?,
        fps: a.num("fps", 120.0)?,
        size: a.num("size", 1400)?,
        fec: a.num("fec", 20.0)?,
        duration: a.dur("duration", Duration::from_secs(10))?,
        procs: parse_pids(&a.str_or("pids", ""))?,
    };
    if c.mbps <= 0.0 || c.fps <= 0.0 || c.fec < 0.0 {
        return Err("--mbps and --fps must be above 0, --fec at least 0".into());
    }
    let r = video(&c).map_err(|e| e.to_string())?;
    Ok(report::emit("video", r, &a.common()))
}

fn cmd_tcp(rest: &[String]) -> Result<bool, String> {
    let a = Args::parse(rest, &["to", "bytes", "duration", "timeout", "pids"], &[])?;
    let c = TcpConfig {
        to: addr(a.need("to")?)?,
        bytes: a.num("bytes", 0)?,
        duration: a.dur("duration", Duration::from_secs(10))?,
        timeout: a.dur("timeout", Duration::from_secs(120))?,
        procs: parse_pids(&a.str_or("pids", ""))?,
    };
    let r = tcp(&c);
    let mut common = a.common();
    common.checks.insert(0, "ok==1".into()); // the transfer worked: the exit status is the verdict
    Ok(report::emit("tcp", r, &common))
}

fn cmd_sample(rest: &[String]) -> Result<bool, String> {
    let a = Args::parse(rest, &["pids", "duration"], &[])?;
    let procs = parse_pids(a.need("pids")?)?;
    let d = a.dur("duration", Duration::from_secs(10))?;
    let m = Meter::start(&procs);
    std::thread::sleep(d);
    let r = obj([("secs", d.as_secs_f64().into()), ("procs", m.stop())]);
    Ok(report::emit("sample", r, &a.common()))
}

fn cmd_wait(rest: &[String]) -> Result<bool, String> {
    let a = Args::parse(rest, &["file", "for", "timeout", "every"], &[])?;
    let src = match (a.get("file"), a.rest.is_empty()) {
        (Some(f), true) => Source::File(PathBuf::from(f)),
        (None, false) => Source::Cmd(a.rest.clone()),
        _ => return Err("give --file FILE or -- CMD..., not both".into()),
    };
    let conds = a.all("for");
    let r = wait(
        &src,
        &conds,
        a.dur("timeout", Duration::from_secs(10))?,
        a.dur("every", Duration::from_millis(200))?.max(Duration::from_millis(10)),
    );
    let mut common = a.common();
    common.checks.insert(0, "ok==1".into());
    Ok(report::emit("wait", r, &common))
}

fn cmd_check(rest: &[String]) -> Result<bool, String> {
    let (file, rest) = rest.split_first().ok_or("usage: load check FILE --check COND...")?;
    let a = Args::parse(rest, &[], &[])?;
    let doc = load::wait::read(&Source::File(PathBuf::from(file)))?;
    let r = obj([("file", file.as_str().into()), ("data", doc)]);
    let mut common = a.common();
    // The checks read the file's own fields: prefix them with "data.".
    common.checks = common
        .checks
        .iter()
        .map(|c| match c.strip_prefix('!') {
            Some(c) => format!("!data.{c}"),
            None => format!("data.{c}"),
        })
        .collect();
    Ok(report::emit("check", r, &common))
}

fn cmd_table(rest: &[String]) -> Result<bool, String> {
    if rest.is_empty() {
        return Err("usage: load table FILE...".into());
    }
    let mut rows: Vec<Json> = Vec::new();
    for f in rest {
        let text = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?;
        rows.extend(json::lines(&text));
    }
    print!("{}", report::table(&rows));
    Ok(report::all_pass(&rows))
}
