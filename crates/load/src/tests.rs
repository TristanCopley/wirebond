//! Unit tests of the pure parts: JSON, checks, arguments, percentiles, /proc parsing, the stamp,
//! frame assembly and gaps. tests/loopback.rs runs the measurements against a real echo.

use std::time::Duration;

use crate::args::{Args, duration};
use crate::json::{self, Json, obj};
use crate::report::{self, check, glob, rfc3339, split};
use crate::stats::{parse_pids, pct, quantiles, stat_ticks, status_rss_kb};
use crate::udp::{Frames, Stamp, VideoConfig, longest_gap, shards};

#[test]
fn json_writes_and_parses_back() {
    let v = obj([
        ("n", 3u64.into()),
        ("x", 0.1234.into()),
        ("neg", (-2.5).into()),
        ("s", "a \"q\" \\ \n\u{1}é".into()),
        ("b", true.into()),
        ("null", Json::Null),
        ("arr", vec![1u64, 2].into()),
        ("o", obj([("k", "v".into())])),
    ]);
    let text = v.to_string();
    assert_eq!(
        text,
        r#"{"n":3,"x":0.123,"neg":-2.5,"s":"a \"q\" \\ \n\u0001é","b":true,"null":null,"arr":[1,2],"o":{"k":"v"}}"#
    );
    let back = json::parse(&text).unwrap();
    assert_eq!(back.get("s").and_then(Json::str), Some("a \"q\" \\ \n\u{1}é"));
    assert_eq!(back.get("x").and_then(Json::num), Some(0.123));
    assert_eq!(json::parse(r#"{"u":"é😀"}"#).unwrap().get("u").and_then(Json::str), Some("é😀"));
    assert_eq!(Json::Num(f64::NAN).to_string(), "null");
    for bad in ["", "{", "[1,]", "{\"a\" 1}", "tru", "1 2", "\"open"] {
        assert!(json::parse(bad).is_err(), "{bad:?} parsed");
    }
    assert_eq!(json::lines("log line\n{\"a\":1}\n\n[2]\n").len(), 2);
}

#[test]
fn flatten_joins_keys_and_indexes() {
    let v = json::parse(r#"{"bond":{"state":"up","lanes":[{"state":"up"},{"state":"down"}]},"e":{}}"#).unwrap();
    let keys: Vec<String> = v.flatten().into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["bond.state", "bond.lanes.0.state", "bond.lanes.1.state"]);
}

#[test]
fn checks_compare_numbers_text_wildcards_and_negation() {
    let v = json::parse(
        r#"{"loss_pct":0.5,"ok":true,"bond":{"state":"up","lanes":[{"state":"punching"},{"state":"up"}]},
            "problems":[{"code":"mesh_only"}],"udp:47998:10.77.0.2":{"pkts":7}}"#,
    )
    .unwrap();
    let flat = v.flatten();
    let t = |e: &str| check(&flat, e).unwrap().0;
    assert!(t("loss_pct<=1") && t("loss_pct<1") && !t("loss_pct>1") && t("loss_pct!=0") && t("loss_pct>=0.5"));
    assert!(t("ok==1") && !t("ok==0"));
    assert!(t("bond.state==up") && !t("bond.state!=up") && !t("bond.state<=up"));
    assert!(t("bond.lanes.*.state==up") && !t("bond.lanes.*.state==down"));
    assert!(t("!problems.*.code==bond_down") && !t("!problems.*.code==mesh_only"));
    assert!(t("udp:47998:10.77.0.2.pkts>0") && t("udp:47998:10.0.0.9.pkts==0"), "a missing field reads as 0");
    assert!(!t("missing.*==0"), "a wildcard that matches nothing holds nowhere");
    assert!(check(&flat, "no operator").is_err());
    assert_eq!(split("a!=b").unwrap(), (false, "a", "!=", "b"));
    assert_eq!(split("!x<=3").unwrap(), (true, "x", "<=", "3"));
    assert!(glob("a*c", "abbc") && glob("*", "") && glob("a.*.s", "a.0.s") && !glob("a*c", "abd"));
}

#[test]
fn emit_records_checks_and_table_reads_them() {
    let dir = std::env::temp_dir().join(format!("load-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("results.jsonl");
    let mut c = report::Common { out: Some(out.clone()), label: "lab".into(), ..Default::default() };
    c.checks = vec!["loss_pct<=1".into()];
    let row = obj([
        ("loss_pct", 0.0.into()),
        ("p50_ms", 1.25.into()),
        ("procs", obj([("client", obj([("cpu", 3.0.into())]))])),
    ]);
    assert!(report::emit("udp", row.clone(), &c));
    c.checks = vec!["loss_pct>1".into()];
    assert!(!report::emit("udp", row, &c));
    let rows = json::lines(&std::fs::read_to_string(&out).unwrap());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("kind").and_then(Json::str), Some("udp"));
    assert_eq!(rows[1].get("failed_checks"), Some(&Json::from(vec!["loss_pct>1"])));
    assert!(!report::all_pass(&rows));
    let t = report::table(&rows);
    assert!(t.contains("client CPU") && t.contains("lab") && t.contains("FAIL") && t.contains("pass"), "{t}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn args_parse_values_flags_and_rest() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let a = Args::parse(
        &s(&["--to", "1.2.3.4:5", "--mbps=30", "--check", "a<1", "--check", "b>2", "--table"]),
        &["to", "mbps"],
        &[],
    )
    .unwrap();
    assert_eq!(a.get("to"), Some("1.2.3.4:5"));
    assert_eq!(a.num("mbps", 0.0).unwrap(), 30.0);
    assert_eq!(a.common().checks, ["a<1", "b>2"]);
    assert!(a.common().table);
    let w = Args::parse(&s(&["--for", "x==1", "--", "wirebond", "status", "--json"]), &["for"], &[]).unwrap();
    assert_eq!(w.rest, ["wirebond", "status", "--json"]);
    assert!(Args::parse(&s(&["--nope", "1"]), &["to"], &[]).is_err());
    assert!(Args::parse(&s(&["--to"]), &["to"], &[]).is_err());
    assert!(Args::parse(&s(&["stray"]), &["to"], &[]).is_err());
    assert_eq!(duration("250ms"), Some(Duration::from_millis(250)));
    assert_eq!(duration("10"), Some(Duration::from_secs(10)));
    assert_eq!(duration("1.5s"), Some(Duration::from_millis(1500)));
    assert_eq!(duration("2m"), Some(Duration::from_secs(120)));
    assert_eq!(duration("-1"), None);
    assert_eq!(duration("x"), None);
}

#[test]
fn percentiles_are_nearest_rank() {
    let mut xs: Vec<f64> = (1..=1000).rev().map(f64::from).collect();
    assert_eq!(quantiles(&mut xs), [500.0, 990.0, 999.0, 1000.0]);
    assert_eq!(pct(&[], 0.5), 0.0);
    assert_eq!(pct(&[7.0], 0.999), 7.0);
}

#[test]
fn proc_files_parse() {
    let stat = "1234 (my (odd) name) S 1 2 3 4 5 6 7 8 9 10 250 50 0 0 20 0 3 0 100 1000 300";
    assert_eq!(stat_ticks(stat), Some(300));
    assert_eq!(stat_ticks("garbage"), None);
    assert_eq!(status_rss_kb("Name:\tx\nVmRSS:\t  4096 kB\n"), Some(4096));
    assert_eq!(parse_pids("client=12, anchor=34").unwrap().len(), 2);
    assert!(parse_pids("client").is_err());
    assert!(parse_pids("").unwrap().is_empty());
}

#[test]
fn rfc3339_formats_utc() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(1_794_009_600 + 3_723), "2026-11-07T01:02:03Z");
}

#[test]
fn stamp_round_trips_and_rejects_other_runs() {
    let s = Stamp { run: 7, seq: 1 << 40, sent: 123_456_789, frame: 9, shard: 3, need: 23 };
    let mut b = [0u8; 40];
    s.write(&mut b);
    assert_eq!(Stamp::read(&b, 7), Some(s));
    assert_eq!(Stamp::read(&b, 8), None);
    assert_eq!(Stamp::read(&b[..31], 7), None);
}

#[test]
fn shards_follow_bitrate_fps_and_parity() {
    let c = VideoConfig {
        to: "127.0.0.1:1".parse().unwrap(),
        from: None,
        mbps: 30.0,
        fps: 120.0,
        size: 1400,
        fec: 20.0,
        duration: Duration::from_secs(1),
        procs: vec![],
    };
    // 30 Mbit/s at 120 fps: 31,250 bytes a frame, 23 shards of 1,400 bytes, 5 of parity.
    assert_eq!(shards(&c), (23, 5));
    assert_eq!(shards(&VideoConfig { fec: 0.0, ..c.clone() }), (23, 0));
    assert_eq!(shards(&VideoConfig { mbps: 0.01, ..c }), (1, 1));
}

#[test]
fn frames_assemble_purge_and_reject_late_shards() {
    let mut f = Frames::new(4, 2);
    f.arrive(0, 10);
    f.arrive(0, 20); // frame 0 decodable at 20
    f.arrive(1, 30); // frame 1 has one of two
    f.arrive(2, 40); // purges frame 1
    f.arrive(1, 45); // late: frame 1 already passed
    f.arrive(2, 50);
    f.arrive(0, 55); // parity of a decoded frame: not late
    assert_eq!(f.done, [Some(20), None, Some(50), None]);
    assert_eq!(f.purged, [false, true, false, false]);
    assert_eq!(f.late, 1);
    f.arrive(9, 60); // beyond the run: ignored
    assert_eq!(f.late, 1);
}

#[test]
fn longest_gap_counts_the_ends() {
    assert_eq!(longest_gap(0, &[], 500), 500);
    assert_eq!(longest_gap(0, &[100, 150, 400], 450), 250);
    assert_eq!(longest_gap(0, &[300, 310], 320), 300);
    assert_eq!(longest_gap(0, &[10, 20], 900), 880);
}
