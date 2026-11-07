//! The swarm (Go netsim/swarm_test.go, §17.2): per seed each fault class is on with probability
//! ½ [Testing 3]. Every seed runs in safety mode (faults on to the end, invariants after every
//! event) and liveness mode (faults heal at two thirds of the run, then video must play), each
//! twice: the two event-log hashes must match. 30 simulated seconds per run (notes/netsim.md 13);
//! 5 minutes with WIREBOND_LONG; WIREBOND_SEED=N for one seed. Go's failure on seed 4 was the
//! checker's probe key, fixed in check.rs; seed 8's liveness gap is open (notes/open.md,
//! notes/rust.md). Owner: N4.

use wirebond_core::MODE_BANDWIDTH;
use wirebond_netsim::env::{Env, START, WARM, defaults, hole, long, maker, ms, par_runs, repro, seeds, show_hash};
use wirebond_netsim::{Adv, DevId, Dur, LinkId, MIN, MS, NatKind, Rng, SEC, World};

/// The fault classes, in the order the seed's draws decide them.
const CLASSES: [&str; 23] = [
    "loss",
    "jitter",
    "spikes",
    "awdl",
    "starlink",
    "policer",
    "frag-drop",
    "blackhole-one-way",
    "blackhole-both-ways",
    "flap",
    "rebind-port",
    "rebind-address",
    "hard-nat",
    "control-cut",
    "mesh-cut",
    "sleep",
    "restart-server",
    "restart-client",
    "adversary",
    "rate-step",
    "address-change",
    "bandwidth",
    "tcp-like",
];

/// How soon after the faults heal (liveness mode) video must play again: frames shown, with no
/// gap above SWARM_GAP from then to the end.
const SWARM_LIVE: Dur = 5 * SEC;
const SWARM_GAP: Dur = SEC;

/// The command that reproduces a swarm failure on seed (§17.2).
fn swarm_repro(seed: u64) -> String {
    repro("swarm", "swarm", seed)
}

/// 30 s per run, 5 minutes with WIREBOND_LONG.
fn swarm_time() -> Dur {
    if long() { 5 * MIN } else { 30 * SEC }
}

/// A liveness evaluation: None when it passed.
type Live = Box<dyn FnOnce(&mut Env) -> Option<String>>;

/// What the heal turns off again (liveness mode).
type Undo = Box<dyn FnOnce(&mut World)>;

/// The seed's generator (Go's `rand.New(rand.NewPCG(seed, 0x5a5a))`).
fn seeded(seed: u64) -> Rng {
    Rng::new(seed, 0x5a5a)
}

/// The classes that are on, each with probability ½, drawn from r in the order of CLASSES.
fn classes(r: &mut Rng) -> Vec<&'static str> {
    CLASSES.into_iter().filter(|_| r.below(2) == 1).collect()
}

/// A fault's start: from WARM to a second before the faults heal.
fn at(r: &mut Rng, heal: Dur) -> Dur {
    WARM + r.dur((heal - WARM - SEC).max(SEC))
}

/// Uniform in [lo, hi).
fn span(r: &mut Rng, lo: Dur, hi: Dur) -> Dur {
    lo + r.dur(hi - lo)
}

fn pick<T: Copy>(r: &mut Rng, of: [T; 2]) -> T {
    of[r.below(2) as usize]
}

/// Flaps l in both directions, each state lasting Exp(1 s), until heal; it is left up after.
fn flap(w: &mut World, l: LinkId, heal: Dur, mut r: Rng, down: bool) {
    let live = w.now() < heal;
    let down = down && live;
    (w[l].up.hole, w[l].down.hole) = (down, down);
    if live {
        let t = w.now() + (r.exp() * 1e9) as Dur;
        w.at(t, move |w| flap(w, l, heal, r, !down));
    }
}

/// One swarm run for seed (Go swarm): the classes drawn from `Rng::new(seed, 0x5a5a)` stay on to
/// the end (safety) or heal at two thirds of the run (liveness); Moonlight 20 Mbps from START.
/// Returns the env, the classes that are on, and the liveness evaluation (None: passed; always
/// None in safety mode), which settles the stream and checks max_gap from heal + SWARM_LIVE.
fn build(seed: u64, live: bool, d: Dur) -> (Env, Vec<&'static str>, Live) {
    let mut r = seeded(seed);
    let names = classes(&mut r);
    let on = |c: &str| {
        assert!(CLASSES.contains(&c), "no fault class {c}");
        names.contains(&c)
    };
    let mut o = defaults();
    if on("loss") {
        for p in [&mut o.hotel, &mut o.cell] {
            (p.ge_bad, p.ge_good) = (0.002, 0.3);
        }
    }
    if on("jitter") {
        o.hotel.hotel = true;
    }
    if on("policer") {
        (o.hotel.police, o.hotel.burst) = (true, 32 << 10);
    }
    if on("frag-drop") {
        (o.cell.mtu, o.cell.frag_drop) = (1400, true);
    }
    if on("hard-nat") {
        (o.client_nat, o.home_nat) = (NatKind::Hard, NatKind::Easy);
    }
    if on("bandwidth") {
        o.mode = MODE_BANDWIDTH;
    }
    (o.spikes, o.awdl) = (on("spikes"), on("awdl"));
    if on("starlink") {
        o.starlink = 140 * MS;
    }
    let mut e = Env::new(seed, maker(), &o);
    let links = [e.hotel, e.cell];
    let w = &mut e.w;
    let heal = if live { d * 2 / 3 } else { d };
    let m = w.moonlight(20e6, 120.0, START, d);
    if on("tcp-like") {
        w.bulk(9100, START, d);
    }
    let mut undo: Vec<Undo> = Vec::new();
    if on("blackhole-one-way") {
        for _ in 0..3 {
            let (l, up) = (pick(&mut r, links), r.below(2) == 0);
            let (t, len) = (at(&mut r, heal), span(&mut r, 300 * MS, 3 * SEC));
            hole(w, l, t, len, up, !up);
        }
    }
    if on("blackhole-both-ways") {
        for _ in 0..2 {
            let l = pick(&mut r, links);
            let (t, len) = (at(&mut r, heal), span(&mut r, 300 * MS, 3 * SEC));
            hole(w, l, t, len, true, true);
        }
    }
    if on("flap") {
        // The flap draws its periods while the run goes, from a generator of its own (Go drew
        // them from the classes' generator).
        let (l, t) = (pick(&mut r, links), at(&mut r, heal));
        let fr = Rng::new(r.u64(), r.u64());
        w.at(t, move |w| flap(w, l, heal, fr, true));
    }
    for c in ["rebind-port", "rebind-address"] {
        if on(c) {
            let (l, addr, t) = (pick(&mut r, links), c == "rebind-address", at(&mut r, heal));
            w.every(t, 5 * SEC, move |w| {
                let live = w.now() < heal;
                if live {
                    w[l].nat.rebind(addr);
                }
                live
            });
        }
    }
    if on("control-cut") {
        let t = at(&mut r, heal);
        let back = (t + span(&mut r, 2 * SEC, 20 * SEC)).min(heal);
        w.at(t, |w| w.cut_control());
        w.at(back, |w| w.restore_control());
    }
    if on("mesh-cut") {
        let t = at(&mut r, heal);
        let back = (t + span(&mut r, 2 * SEC, 20 * SEC)).min(heal);
        w.at(t, |w| w.mesh_down = true);
        w.at(back, |w| w.mesh_down = false);
    }
    // Go drew the sleep's and the restarts' lengths when they began; they are drawn here.
    if on("sleep") {
        let (dv, advance) = (pick(&mut r, DevId::BOTH), r.below(2) == 0);
        let t = at(&mut r, heal);
        let len = span(&mut r, SEC, 20 * SEC).min((heal - t).max(SEC));
        w.at(t, move |w| w.sleep(dv, len, advance));
    }
    for (c, dv) in [("restart-server", DevId::Server), ("restart-client", DevId::Client)] {
        if on(c) {
            let (t, down) = (at(&mut r, heal), span(&mut r, 500 * MS, 3 * SEC));
            w.at(t, move |w| w.restart(dv, down));
        }
    }
    if on("adversary") {
        let a = w.new_adversary(pick(&mut r, DevId::BOTH));
        for k in Adv::ATTACKS {
            a.p[k.idx()] = 0.01;
        }
        undo.push(Box::new(|w| {
            if let Some(a) = w.adv.as_mut() {
                a.on = false;
            }
        }));
    }
    if on("rate-step") {
        let (l, t) = (pick(&mut r, links), at(&mut r, heal));
        w.at(t, move |w| {
            let rate = w[l.down()].p.rate / 4.0;
            w.set_rate(l.down(), rate);
        });
        undo.push(Box::new(move |w| {
            let rate = w[l].spec.down.rate;
            w.set_rate(l.down(), rate);
        }));
    }
    if on("address-change") {
        let (cell, t) = (links[1], at(&mut r, heal));
        w.at(t, move |w| w.set_local(cell, "172.20.10.3:7849".parse().unwrap()));
    }
    if live {
        w.at(heal, move |w| {
            for f in undo {
                f(w);
            }
            for l in links {
                (w[l].up.hole, w[l].down.hole) = (false, false);
            }
            w.mesh_down = false;
            w.restore_control();
        });
    }
    let eval: Live = Box::new(move |e: &mut Env| {
        if !live {
            return None;
        }
        e.w.settle(m);
        let g = e.w[m].max_gap(heal + SWARM_LIVE, d - SEC);
        (g > SWARM_GAP).then(|| {
            format!(
                "liveness: {} after the faults healed at {}, video had a {} gap (≤ {})",
                ms(SWARM_LIVE),
                ms(heal),
                ms(g),
                ms(SWARM_GAP)
            )
        })
    });
    (e, names, eval)
}

/// One seed in one mode, run twice (Go TestSwarm's subtest): the log line, or every failure. With
/// WIREBOND_HASHES each run prints its event-log hash, also one that ends on a safety failure.
fn run(seed: u64, live: bool) -> Result<String, String> {
    let mode = if live { "liveness" } else { "safety" };
    let d = swarm_time();
    let (mut bad, mut faults, mut hashes) = (Vec::new(), String::new(), Vec::new());
    for i in 0..2 {
        let (mut e, names, eval) = build(seed, live, d);
        faults = names.join(" ");
        e.w.start();
        let ran = e.w.run(d);
        show_hash(&format!("swarm seed {seed} {mode} run {}", i + 1), &mut e.w);
        if let Err(err) = ran {
            return Err(format!(
                "seed {seed}, {mode} mode, faults {faults}:\n  {err}\nreproduce: {}",
                swarm_repro(seed)
            ));
        }
        if let Some(msg) = eval(&mut e)
            && i == 0
        {
            bad.push(format!("seed {seed}, faults {faults}:\n  {msg}\nreproduce: {}", swarm_repro(seed)));
        }
        hashes.push(e.w.hash());
    }
    if hashes[0] != hashes[1] {
        bad.push(format!(
            "seed {seed}, {mode} mode: event-log hashes {} and {} differ\nreproduce: {}",
            hashes[0],
            hashes[1],
            swarm_repro(seed)
        ));
    }
    if bad.is_empty() {
        Ok(format!("seed {seed}, {mode} mode, faults: {faults}; hash {}", hashes[0]))
    } else {
        Err(bad.join("\n"))
    }
}

/// Go TestSwarm: every seed of the set in both modes (on scoped threads), each run twice; a
/// safety failure, a liveness failure (first run) or differing hashes fail with the faults and
/// swarm_repro.
#[test]
fn swarm() {
    let jobs: Vec<(u64, bool)> = seeds().into_iter().flat_map(|s| [(s, false), (s, true)]).collect();
    let mut bad = Vec::new();
    for r in par_runs(&jobs, |&(seed, live)| run(seed, live)) {
        match r {
            Ok(line) => println!("{line}"),
            Err(e) => bad.push(e),
        }
    }
    assert!(bad.is_empty(), "{} of {} swarm runs failed:\n{}", bad.len(), jobs.len(), bad.join("\n"));
}

/// The classes a seed draws are its own and each is on for about half the seeds.
#[test]
fn classes_follow_the_seed() {
    let draw = |s| classes(&mut seeded(s));
    assert_eq!(draw(4), draw(4));
    let n = 2000;
    let mut count = [0; CLASSES.len()];
    for s in 1..=n {
        for c in draw(s) {
            count[CLASSES.iter().position(|&k| k == c).unwrap()] += 1;
        }
    }
    for (c, k) in CLASSES.iter().zip(count) {
        assert!((900..1100).contains(&k), "{c} on in {k} of {n} seeds");
    }
    assert_ne!(draw(1), draw(2));
}

/// The fault times fall between WARM and a second before the heal.
#[test]
fn faults_start_while_they_can_heal() {
    let mut r = Rng::new(1, 2);
    for heal in [20 * SEC, 30 * SEC, 200 * SEC, WARM] {
        for _ in 0..1000 {
            let t = at(&mut r, heal);
            assert!(t >= WARM && t < WARM + (heal - WARM - SEC).max(SEC), "{t} for {heal}");
            let s = span(&mut r, 300 * MS, 3 * SEC);
            assert!((300 * MS..3 * SEC).contains(&s));
        }
    }
}
