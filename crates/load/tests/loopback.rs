//! The measurements against a real echo on loopback: nothing lost, every frame decodable, every TCP
//! byte back, and the echo's counters by source. CI runs this as is; in a sandbox, run it inside
//! `unshare -n sh -c 'ip link set lo up && cargo test -p load'`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Command;
use std::time::Duration;

use load::echo::Echo;
use load::json::{self, Json};
use load::stats::Proc;
use load::tcp::{TcpConfig, tcp};
use load::udp::{UdpConfig, VideoConfig, udp, video};
use load::wait::{Source, wait};

fn echo_on(ip: IpAddr) -> Option<Echo> {
    Echo::start(Some(ip), &[0], &[0]).ok()
}

fn n(r: &Json, k: &str) -> f64 {
    r.get(k).and_then(Json::num).unwrap_or_else(|| panic!("{k} missing in {r}"))
}

#[test]
fn udp_video_and_tcp_through_an_echo() {
    let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let e = echo_on(lo).expect("an echo on 127.0.0.1");
    let me = vec![Proc { name: "self".into(), pid: std::process::id() }];

    let r = udp(&UdpConfig {
        to: e.udp[0],
        from: None,
        mbps: 20.0,
        size: 1200,
        duration: Duration::from_millis(1500),
        procs: me.clone(),
    })
    .unwrap();
    assert!(n(&r, "sent") > 2000.0, "{r}");
    assert_eq!(n(&r, "lost"), 0.0, "{r}");
    assert_eq!(n(&r, "dups"), 0.0, "{r}");
    assert!(n(&r, "p50_ms") < 50.0 && n(&r, "gap_ms") < 1000.0, "{r}");
    assert!(
        r.get("procs").and_then(|p| p.get("self")).and_then(|s| s.get("rss_mb")).and_then(Json::num).unwrap() > 0.0
    );

    let v = video(&VideoConfig {
        to: e.udp[0],
        from: None,
        mbps: 10.0,
        fps: 120.0,
        size: 1400,
        fec: 20.0,
        duration: Duration::from_secs(1),
        procs: vec![],
    })
    .unwrap();
    assert_eq!(n(&v, "frames"), 120.0, "{v}");
    assert_eq!(n(&v, "decodable"), 120.0, "{v}");
    assert_eq!(n(&v, "unrecovered"), 0.0, "{v}");
    assert!(n(&v, "frame_p99_ms") < 100.0, "{v}");

    let t = tcp(&TcpConfig {
        to: e.tcp[0],
        bytes: 20_000_000,
        duration: Duration::ZERO,
        timeout: Duration::from_secs(30),
        procs: me,
    });
    assert_eq!(t.get("ok"), Some(&Json::Bool(true)), "{t}");
    assert_eq!(n(&t, "bytes"), 20e6, "{t}");
    let timed = tcp(&TcpConfig {
        to: e.tcp[0],
        bytes: 0,
        duration: Duration::from_millis(300),
        timeout: Duration::from_secs(30),
        procs: vec![],
    });
    assert_eq!(timed.get("ok"), Some(&Json::Bool(true)), "{timed}");
    assert!(n(&timed, "bytes") > 0.0, "{timed}");

    // The echo counted all of it by source: 127.0.0.1.
    let s = e.stats();
    let udp_key = format!("udp:{}:127.0.0.1", e.udp[0].port());
    let tcp_key = format!("tcp:{}:127.0.0.1", e.tcp[0].port());
    let pkts = s.get(&udp_key).and_then(|c| c.get("pkts")).and_then(Json::num).unwrap();
    assert_eq!(pkts, n(&r, "sent") + n(&v, "sent"), "{s}");
    assert_eq!(s.get(&tcp_key).and_then(|c| c.get("conns")).and_then(Json::num), Some(2.0), "{s}");
}

#[test]
fn nothing_answers_a_closed_port() {
    // A port with nothing behind it: everything is lost, and the gap is the whole run.
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let to = sock.local_addr().unwrap();
    drop(sock);
    let r =
        udp(&UdpConfig { to, from: None, mbps: 1.0, size: 100, duration: Duration::from_millis(300), procs: vec![] })
            .unwrap();
    assert_eq!(n(&r, "recv"), 0.0, "{r}");
    assert_eq!(n(&r, "loss_pct"), 100.0, "{r}");
    assert!(n(&r, "gap_ms") >= 250.0, "{r}");
    let t = tcp(&TcpConfig {
        to: SocketAddr::new(to.ip(), to.port()),
        bytes: 1000,
        duration: Duration::ZERO,
        timeout: Duration::from_secs(2),
        procs: vec![],
    });
    assert_eq!(t.get("ok"), Some(&Json::Bool(false)), "{t}");
}

#[test]
fn ipv6_when_loopback_has_it() {
    let Some(e) = echo_on(IpAddr::V6(Ipv6Addr::LOCALHOST)) else {
        eprintln!("no ::1 here: skipped");
        return;
    };
    let r = udp(&UdpConfig {
        to: e.udp[0],
        from: None,
        mbps: 5.0,
        size: 1400,
        duration: Duration::from_millis(500),
        procs: vec![],
    })
    .unwrap();
    assert_eq!(n(&r, "lost"), 0.0, "{r}");
    assert!(e.stats().get(&format!("udp:{}:::1", e.udp[0].port())).is_some(), "{}", e.stats());
}

#[test]
fn wait_reads_a_command_and_a_file() {
    let cmd = Source::Cmd(vec!["echo".into(), r#"{"bond":{"state":"up"}}"#.into()]);
    let r = wait(&cmd, &["bond.state==up".into()], Duration::from_secs(2), Duration::from_millis(50));
    assert_eq!(r.get("ok"), Some(&Json::Bool(true)), "{r}");
    let r = wait(&cmd, &["bond.state==down".into()], Duration::from_millis(300), Duration::from_millis(50));
    assert_eq!(r.get("ok"), Some(&Json::Bool(false)), "{r}");
    assert!(n(&r, "tries") >= 2.0, "{r}");
    let missing = Source::File("/nonexistent/load-wait.json".into());
    let r = wait(&missing, &["a==1".into()], Duration::ZERO, Duration::from_millis(10));
    assert!(r.get("err").is_some(), "{r}");
}

#[test]
fn the_binary_prints_json_and_tables() {
    let bin = env!("CARGO_BIN_EXE_load");
    let e = echo_on(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
    let dir = std::env::temp_dir().join(format!("load-bin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("r.jsonl");
    let to = e.udp[0].to_string();
    let run = |args: &[&str]| Command::new(bin).args(args).output().unwrap();
    let o = run(&[
        "udp",
        "--to",
        &to,
        "--mbps",
        "2",
        "--duration",
        "300ms",
        "--label",
        "loop",
        "--out",
        out.to_str().unwrap(),
        "--check",
        "loss_pct<=0",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let line = json::parse(String::from_utf8_lossy(&o.stdout).trim()).unwrap();
    assert_eq!(line.get("pass"), Some(&Json::Bool(true)));
    let o = run(&[
        "udp",
        "--to",
        &to,
        "--mbps",
        "2",
        "--duration",
        "200ms",
        "--out",
        out.to_str().unwrap(),
        "--check",
        "recv==0",
    ]);
    assert_eq!(o.status.code(), Some(1), "a failed check exits 1");
    let o = run(&["table", out.to_str().unwrap()]);
    let table = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(1), "{table}");
    assert!(table.contains("loop") && table.contains("FAIL") && table.contains("p99.9"), "{table}");
    assert_eq!(run(&["udp", "--to", "nowhere"]).status.code(), Some(2));
    assert_eq!(run(&["frobnicate"]).status.code(), Some(2));
    std::fs::remove_dir_all(&dir).unwrap();
}
