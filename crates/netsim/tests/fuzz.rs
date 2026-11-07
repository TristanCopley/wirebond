//! FuzzScenario (Go netsim/fuzz_test.go, §17.1 tier 1, [Testing 11]) without cargo fuzz: bytes
//! decoded into a script of network events, every safety invariant checked after every event.
//! Each 4 bytes are one event: what (op % 14), when (in 40 ms steps over the 10 s run) and two
//! arguments; at most 64 events. The test runs the Go corpus, every `.hex` file in testdata/fuzz
//! (hex, one script per file, whitespace ignored: a failing script found here is committed there)
//! and WIREBOND_FUZZ random scripts per seed of the set (default 8; 128 with WIREBOND_LONG), each
//! drawn from `Rng::new(seed, i)`. A failure prints the script in hex. Owner: N4.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use wirebond_core::{MODE_BANDWIDTH, MODE_PERFORMANCE};
use wirebond_netsim::env::{Env, START, defaults, hole, long, maker, par_runs, repro, seeds};
use wirebond_netsim::{Adv, DevId, Dur, LinkId, MS, Rng, SEC, World};

/// Go's f.Add seeds.
const CORPUS: [&[u8]; 5] = [
    &[],
    &[2, 40, 0, 50, 3, 60, 0, 100, 5, 90, 0, 0, 5, 120, 0, 1],
    &[8, 25, 0, 255, 8, 25, 1, 255, 8, 25, 2, 255, 8, 25, 3, 255, 8, 25, 4, 255, 8, 25, 5, 255],
    &[7, 50, 1, 40, 6, 120, 0, 200, 6, 180, 1, 1, 11, 200, 0, 1],
    &[10, 30, 1, 3, 12, 60, 1, 0, 4, 70, 0, 150, 13, 80, 0, 40, 1, 100, 0, 30],
];

/// The events one script may hold (Go's 4 × 64 bytes).
const MAX_EVENTS: usize = 64;

/// Where failing scripts are committed (crates/netsim/testdata/fuzz).
fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/fuzz")
}

/// The script's events in order: when, op, x, y. A trailing partial event is ignored.
fn events(script: &[u8]) -> impl Iterator<Item = (Dur, u8, u8, u8)> + '_ {
    let (whole, _) = script.as_chunks::<4>();
    whole.iter().take(MAX_EVENTS).map(|&[op, when, x, y]| (Dur::from(when) * 40 * MS, op, x, y))
}

/// Runs one script on seed 1 (Go's fuzz body): the default topology, Moonlight 4 Mbps at 60 fps
/// from START to 10 s, an adversary at the server (off), a 10 s wall limit so a runaway input
/// fails instead of stalling; Err carries the failure.
fn run_script(script: &[u8]) -> Result<(), String> {
    let mut e = Env::new(1, maker(), &defaults());
    let links = [e.hotel, e.cell];
    let w = &mut e.w;
    w.wall = std::time::Duration::from_secs(10);
    w.moonlight(4e6, 60.0, START, 10 * SEC);
    w.new_adversary(DevId::Server).on = false;
    for (t, op, x, y) in events(script) {
        w.at(t, move |w| apply(w, links, op, x, y));
    }
    w.start();
    w.run(10 * SEC)
}

/// The next address after a's, on a's port (Go's Addr().Next()).
fn next_ip(a: SocketAddr) -> SocketAddr {
    let ip = match a.ip() {
        IpAddr::V4(v) => IpAddr::V4((u32::from(v).wrapping_add(1)).into()),
        IpAddr::V6(v) => IpAddr::V6((u128::from(v).wrapping_add(1)).into()),
    };
    SocketAddr::new(ip, a.port())
}

/// Carries out one scripted event (Go apply) on the world; links are the hotel and the cell. x
/// picks the link (hotel, cell) or device (client, server); y scales a duration (20 ms steps) or
/// a parameter. op % 14: 0–2 hole up/down/both; 3 cut the control channel; 4 the mesh down;
/// 5 rebind (address if y odd); 6 sleep (advancing if y odd); 7 restart; 8 adversary on,
/// Adv::ATTACKS[x % 6] at y/255 × 0.2; 9 adversary off; 10 down rate (y + 1) Mbps; 11 client mode
/// stable/bandwidth by y; 12 the link's local address to the next IP; 13 loss y/255 × 0.3 both
/// ways. (Go's apply took the env, which a scheduled event cannot reach in Rust.)
fn apply(w: &mut World, links: [LinkId; 2], op: u8, x: u8, y: u8) {
    let l = links[usize::from(x % 2)];
    let dev = DevId::BOTH[usize::from(x % 2)];
    let (now, dur) = (w.now(), Dur::from(y) * 20 * MS);
    match op % 14 {
        0 => hole(w, l, now, dur, true, false),
        1 => hole(w, l, now, dur, false, true),
        2 => hole(w, l, now, dur, true, true),
        3 => {
            w.cut_control();
            w.at(now + dur, |w| w.restore_control());
        }
        4 => {
            w.mesh_down = true;
            w.at(now + dur, |w| w.mesh_down = false);
        }
        5 => w[l].nat.rebind(y % 2 == 1),
        6 => w.sleep(dev, dur + MS, y % 2 == 1),
        7 => w.restart(dev, dur / 4 + MS),
        8 => {
            if let Some(a) = w.adv.as_mut() {
                a.on = true;
                a.p[Adv::ATTACKS[usize::from(x % 6)].idx()] = f64::from(y) / 255.0 * 0.2;
            }
        }
        9 => {
            if let Some(a) = w.adv.as_mut() {
                a.on = false;
            }
        }
        10 => w.set_rate(l.down(), (f64::from(y) + 1.0) * 1e6),
        11 => w.set_mode(DevId::Client, [MODE_PERFORMANCE, MODE_BANDWIDTH][usize::from(y % 2)]),
        12 => {
            let a = next_ip(w[l].local);
            w.set_local(l, a);
        }
        _ => {
            let loss = f64::from(y) / 255.0 * 0.3;
            (w[l].up.p.loss, w[l].down.p.loss) = (loss, loss);
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The bytes of a hex script; whitespace is ignored.
fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    let (pairs, odd) = digits.as_chunks::<2>();
    if !odd.is_empty() {
        return Err("an odd number of hex digits".into());
    }
    pairs
        .iter()
        .map(|p| {
            let p = std::str::from_utf8(p).map_err(|e| e.to_string())?;
            u8::from_str_radix(p, 16).map_err(|e| format!("{p:?}: {e}"))
        })
        .collect()
}

/// The scripts committed under testdata/fuzz, by file name; none when the directory is absent.
fn committed() -> Result<Vec<(String, Vec<u8>)>, String> {
    let dir = corpus_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<PathBuf> =
        rd.map(|e| e.map(|e| e.path()).map_err(|e| e.to_string())).collect::<Result<_, _>>()?;
    paths.retain(|p| p.extension().is_some_and(|x| x == "hex"));
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let s = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let b = unhex(&s).map_err(|e| format!("{}: {e}", p.display()))?;
            Ok((format!("{}", p.display()), b))
        })
        .collect()
}

/// Random scripts per seed: WIREBOND_FUZZ, else 8 (128 with WIREBOND_LONG).
fn per_seed() -> u64 {
    match std::env::var("WIREBOND_FUZZ") {
        Ok(v) => v.parse().expect("WIREBOND_FUZZ is a count"),
        Err(_) if long() => 128,
        Err(_) => 8,
    }
}

/// Script i of seed: up to 64 events of random bytes and a partial event's worth, from
/// `Rng::new(seed, i)`.
fn random_script(seed: u64, i: u64) -> Vec<u8> {
    let mut r = Rng::new(seed, i);
    let mut b = vec![0; r.below(4 * MAX_EVENTS as u64 + 4) as usize];
    r.fill(&mut b);
    b
}

/// Go FuzzScenario's corpus run, plus the committed and the seeded random scripts.
#[test]
fn fuzz_scenario() {
    let any = repro("fuzz", "fuzz_scenario", 1);
    let mut scripts: Vec<(String, Vec<u8>, String)> =
        CORPUS.iter().enumerate().map(|(i, s)| (format!("Go corpus entry {i}"), s.to_vec(), any.clone())).collect();
    let files = committed().unwrap_or_else(|e| panic!("testdata/fuzz: {e}"));
    scripts.extend(files.into_iter().map(|(n, s)| (n, s, any.clone())));
    let n = per_seed();
    for seed in seeds() {
        let again = format!("WIREBOND_FUZZ={n} {}", repro("fuzz", "fuzz_scenario", seed));
        scripts.extend((0..n).map(|i| {
            let name = format!("seed {seed}, random script {i}");
            (name, random_script(seed, i), again.clone())
        }));
    }
    let failed: Vec<String> = par_runs(&scripts, |(name, s, again)| {
        let err = run_script(s).err()?;
        Some(format!("{name}: script {}:\n  {err}\nreproduce: {again}", hex(s)))
    })
    .into_iter()
    .flatten()
    .collect();
    println!("{} scripts, {} failed", scripts.len(), failed.len());
    assert!(
        failed.is_empty(),
        "{} of {} fuzz scripts failed; commit each failing script's hex as a .hex file in {}:\n{}",
        failed.len(),
        scripts.len(),
        corpus_dir().display(),
        failed.join("\n")
    );
}

/// The decoding Go's fuzz body did: 4 bytes an event, 40 ms steps, at most 64, no partial event.
#[test]
fn scripts_decode_as_in_go() {
    let got: Vec<_> = events(CORPUS[1]).collect();
    let want = [(1600 * MS, 2, 0, 50), (2400 * MS, 3, 0, 100), (3600 * MS, 5, 0, 0), (4800 * MS, 5, 0, 1)];
    assert_eq!(got, want);
    assert_eq!(events(&[1, 2, 3]).count(), 0);
    assert_eq!(events(&[0; 4 * 70 + 3]).count(), MAX_EVENTS);
    assert_eq!(events(&[0, 255, 0, 0]).next(), Some((10_200 * MS, 0, 0, 0)));
}

#[test]
fn hex_scripts_round_trip() {
    for s in CORPUS {
        assert_eq!(unhex(&hex(s)).as_deref(), Ok(s));
    }
    assert_eq!(unhex(" 0a0B\n ff\t"), Ok(vec![0x0a, 0x0b, 0xff]));
    assert!(unhex("abc").is_err() && unhex("zz").is_err());
    assert!(committed().is_ok(), "the committed scripts parse");
}

#[test]
fn random_scripts_follow_the_seed() {
    assert_eq!(random_script(1, 3), random_script(1, 3));
    assert_ne!(random_script(1, 3), random_script(2, 3));
    let len = |s, i| random_script(s, i).len();
    assert!((0..20).any(|i| len(1, i) != len(2, i)), "lengths follow the seed");
    let lens: Vec<usize> = (0..200).map(|i| random_script(1, i).len()).collect();
    assert!(lens.iter().all(|&n| n < 4 * MAX_EVENTS + 4));
    assert!(lens.iter().any(|&n| n < 40) && lens.iter().any(|&n| n > 200));
    assert_eq!(next_ip("172.20.10.2:7849".parse().unwrap()), "172.20.10.3:7849".parse().unwrap());
}
