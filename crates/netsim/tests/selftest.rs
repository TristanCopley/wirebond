//! The simulator's self-tests with the fake endpoint (Go netsim/selftest_test.go): the plumbing
//! end to end, determinism, hole punching through the NAT models, control reconnects, sleep and
//! wake, restarts, every safety invariant failing on a fake built to break it, the adversary, and
//! the frame-time formula. Owner: N5.
//!
//! Each test collects its failures, measured values beside their bounds, and panics once with all
//! of them and the command that reproduces it (Go's t.Errorf went on); a safety failure where none
//! is expected stops the test at once (Go's t.Fatalf). The self-tests run seed 1 only.

use std::any::Any;
use std::net::{IpAddr, SocketAddr};

use wirebond_netsim::env::{ADDR_HOTEL, Env, Opts, PUB_CELL, PUB_HOME, PUB_HOTEL, defaults, ms};
use wirebond_netsim::metrics::{MESH_LANE, late, pct};
use wirebond_netsim::sim::FlowId;
use wirebond_netsim::{
    Adv, DevId, Dur, Evil, Fake, LinkSpec, MS, Maker, MoonId, NatKind, Profile, SEC, World, fake_maker,
};

/// Runs a 30 Mbps Moonlight stream from 1 s to stop over o's topology with mk's endpoints,
/// watching the hotel's down queue; setup runs before the start; settles the stream after the
/// run (Go stream). The run's failure, if any, is `env.w.failed()`.
fn stream(seed: u64, mk: Maker, o: &Opts, stop: Dur, until: Dur, setup: impl FnOnce(&mut Env)) -> (Env, MoonId) {
    let mut e = Env::new(seed, mk, o);
    let m = e.w.moonlight(30e6, 120.0, SEC, stop);
    e.w.watch(e.hotel.down());
    setup(&mut e);
    e.w.start();
    // A failure stops the run; the tests read it from failed().
    let _ = e.w.run(until);
    e.w.settle(m);
    (e, m)
}

/// The fake that does what it should (Go fakeMaker(direct, 0)).
fn fake(direct: bool) -> Maker {
    fake_maker(direct, Evil::None)
}

/// The command that reproduces a failure of test (§17.4).
fn repro(test: &str) -> String {
    format!("cargo test -p wirebond-netsim --test selftest -- --exact {test}")
}

/// Fails test at once on a safety failure (Go's t.Fatalf("safety: …")).
fn safe(test: &str, e: &Env) {
    if let Some(f) = e.w.failed() {
        panic!("safety: {f}\nreproduce: {}", repro(test));
    }
}

/// Fails test once, listing every failure (Go's t.Errorf calls).
fn report(test: &str, errs: &[String]) {
    assert!(errs.is_empty(), "{}\nreproduce: {}", errs.join("\n"), repro(test));
}

/// Go TestFakeStream: with the fake (no direct lanes) the control channel connects once (one
/// session and one key per device), STUN maps each link to its NAT's public address (eth: PUB_HOME
/// port 7849), and 2–19 s: ≥ 2,000 frames sent, ≥ 99% decoded, ≤ 2 unrecovered; p99 excess over
/// the hotel's frame time ≤ 15 ms; longest frame gap ≤ 60 ms; input p99.9 beyond the lower lane ≤
/// 30 ms; no audio or input lost; the Tailscale lane carries bulk ≥ 95%; hotel queue p95 ≤ 30 ms.
#[test]
fn fake_stream() {
    const FRAMES: usize = 2000;
    const UNRECOVERED: usize = 2;
    const EXCESS: Dur = 15 * MS;
    const GAP: Dur = 60 * MS;
    const INPUT: Dur = 30 * MS;
    const MESH_SHARE: f64 = 0.95;
    const QUEUE: Dur = 30 * MS;
    let (mut e, m) = stream(1, fake(false), &defaults(), 20 * SEC, 22 * SEC, |_| {});
    safe("fake_stream", &e);
    let mut errs = Vec::new();
    let w = &e.w;
    let (cl, sv) = (&w[DevId::Client], &w[DevId::Server]);
    let (up, sessions, keys) = (w.control_up(), (cl.sessions, sv.sessions), (cl.keys.len(), sv.keys.len()));
    if !up || sessions != (1, 1) || keys != (1, 1) {
        errs.push(format!(
            "control: up {up}, sessions {sessions:?}, keys {keys:?} (up, one session and one key \
             each)"
        ));
    }
    let (h, c, eth) = (w[e.hotel].mapped, w[e.cell].mapped, w[e.eth].mapped);
    let ip = |a: Option<SocketAddr>| a.map(|a| a.ip());
    let home = SocketAddr::from((PUB_HOME, 7849));
    if ip(h) != Some(IpAddr::V4(PUB_HOTEL)) || ip(c) != Some(IpAddr::V4(PUB_CELL)) || eth != Some(home) {
        errs.push(format!("STUN mappings {h:?} {c:?} {eth:?} (on {PUB_HOTEL}, on {PUB_CELL}, {home})"));
    }
    let (from, to) = (2 * SEC, 19 * SEC);
    let x = &w[m];
    let frames = x.frames.iter().filter(|f| f.sent >= from && f.sent < to);
    let (sent, done) = frames.fold((0, 0), |(n, d), f| (n + 1, d + usize::from(f.done > 0)));
    let unrecovered = x.unrecovered(from, to);
    if sent < FRAMES || done < sent * 99 / 100 || unrecovered > UNRECOVERED {
        errs.push(format!(
            "frames: {sent} sent (≥ {FRAMES}), {done} decoded (≥ 99%), {unrecovered} \
             unrecovered (≤ {UNRECOVERED})"
        ));
    }
    // The shaper's 64 KB burst can beat §17.3's frame time, which paces at the rate: below 0 is
    // fine.
    let excess = pct(&x.excess(0, from, to), 0.99);
    if excess > EXCESS {
        errs.push(format!("p99 frame delay beyond the hotel's frame time: {} (≤ {})", ms(excess), ms(EXCESS)));
    }
    let gap = x.max_gap(from, to);
    if gap > GAP {
        errs.push(format!("longest frame gap {} (≤ {})", ms(gap), ms(GAP)));
    }
    let input = pct(&late(&x.inputs, None, from, to), 0.999);
    if input > INPUT {
        errs.push(format!(
            "input p99.9 beyond the lower lane: {} ({} inputs) (≤ {})",
            ms(input),
            x.inputs.len(),
            ms(INPUT)
        ));
    }
    let lost = w.lost(x.audio, from, to) + w.lost(x.input, from, to);
    if lost > 0 {
        errs.push(format!("{lost} small datagrams lost on a clean lane (0)"));
    }
    let share = e.rec().share(MESH_LANE, from, to);
    if share < MESH_SHARE {
        errs.push(format!(
            "the Tailscale lane carried bulk {:.0}% of the time (≥ {:.0}%)",
            share * 100.0,
            MESH_SHARE * 100.0
        ));
    }
    let queue = e.rec().queue_p95(e.hotel.down(), from, to);
    if queue > QUEUE {
        errs.push(format!("hotel queue p95 {} (≤ {})", ms(queue), ms(QUEUE)));
    }
    let events = e.w.events();
    println!(
        "{sent} frames, p99 excess {}, gap {}, input p99.9 {}, hotel queue p95 {}, {events} \
         events, hash {}",
        ms(excess),
        ms(gap),
        ms(input),
        ms(queue),
        e.w.hash()
    );
    report("fake_stream", &errs);
}

/// Go TestDeterminism: seed 1 twice gives one hash; seeds 1 and 2 differ (6 s, direct fake).
#[test]
fn determinism() {
    let run = |seed| {
        let (mut e, _) = stream(seed, fake(true), &defaults(), 6 * SEC, 6 * SEC, |_| {});
        e.w.hash()
    };
    let (a, b, c) = (run(1), run(1), run(2));
    let mut errs = Vec::new();
    if a != b {
        errs.push(format!("seed 1 gave hashes {a} and {b}"));
    }
    if a == c {
        errs.push(format!("seeds 1 and 2 gave the same hash {a}"));
    }
    report("determinism", &errs);
}

/// Go TestFakeDirect: with the forward the direct fake leaves the Tailscale lane (≤ 5% of 3–8 s);
/// behind hard NATs on both ends no direct lane forms and the Tailscale lane alone loses ≤ 2 frames.
#[test]
fn fake_direct() {
    let mut errs = Vec::new();
    let (e, _) = stream(1, fake(true), &defaults(), 8 * SEC, 8 * SEC, |_| {});
    safe("fake_direct", &e);
    let share = e.rec().share(MESH_LANE, 3 * SEC, 8 * SEC);
    if share > 0.05 {
        errs.push(format!(
            "with the forward, bulk stayed on the Tailscale lane {:.0}% of the time (≤ 5%)",
            share * 100.0
        ));
    }
    let mut o = defaults();
    o.client_nat = NatKind::Hard;
    o.home_nat = NatKind::Hard;
    let (e, m) = stream(1, fake(true), &o, 8 * SEC, 8 * SEC, |_| {});
    safe("fake_direct", &e);
    for l in e.w[DevId::Client].snap().lanes {
        if l.id != 0 {
            errs.push(format!("behind hard NATs a direct lane formed: {l:?}"));
        }
    }
    let u = e.w[m].unrecovered(2 * SEC, 7 * SEC);
    if u > 2 {
        errs.push(format!("the Tailscale lane alone lost {u} frames (≤ 2)"));
    }
    report("fake_direct", &errs);
}

/// Go TestControlReconnect: the channel cut 4–7 s comes back with one session and the same key
/// twice; no frame lost 2–11 s.
#[test]
fn control_reconnect() {
    let (e, m) = stream(1, fake(false), &defaults(), 12 * SEC, 12 * SEC, |e| {
        e.w.at(4 * SEC, |w| w.cut_control());
        e.w.at(7 * SEC, |w| w.restore_control());
    });
    safe("control_reconnect", &e);
    let mut errs = Vec::new();
    let cl = &e.w[DevId::Client];
    if !e.w.control_up() || cl.sessions != 1 || cl.keys.len() != 2 || cl.keys[0].k != cl.keys[1].k {
        errs.push(format!(
            "after a reconnect: up {}, {} sessions, {} key messages (up, 1, the same key twice)",
            e.w.control_up(),
            cl.sessions,
            cl.keys.len()
        ));
    }
    let n = e.w[m].unrecovered(2 * SEC, 11 * SEC);
    if n > 0 {
        errs.push(format!("a control-channel outage lost {n} frames (0)"));
    }
    report("control_reconnect", &errs);
}

/// Go TestSleepWake: the Mac sleeps 30 s at 5 s, the clock paused then advancing: one wake (the
/// client's fake, downcast from `world[DevId::Client].endpoint()`), the clock offset moved by 30 s
/// (paused) or 0 (advancing), control up, ≥ 100 frames shown in 36–37 s.
#[test]
fn sleep_wake() {
    let mut errs = Vec::new();
    for advance in [false, true] {
        let mut off0 = 0;
        let (e, m) = stream(1, fake(false), &defaults(), 50 * SEC, 50 * SEC, |e| {
            off0 = e.w[DevId::Client].off();
            e.w.at(5 * SEC, move |w| w.sleep(DevId::Client, 30 * SEC, advance));
        });
        safe("sleep_wake", &e);
        let w = &e.w;
        let wakes = w[DevId::Client].endpoint().and_then(|ep| (ep as &dyn Any).downcast_ref::<Fake>()).map(|f| f.wakes);
        let paused = off0 - w[DevId::Client].off();
        let want = if advance { 0 } else { 30 * SEC };
        if wakes != Some(1) || paused != want || !w.control_up() {
            errs.push(format!(
                "advance {advance}: {wakes:?} wakes of the client's fake (Some(1)), clock paused \
                 {} ({}), control up {} (true)",
                ms(paused),
                ms(want),
                w.control_up()
            ));
        }
        let shown = w[m].shown.iter().filter(|&&s| s > 36 * SEC && s < 37 * SEC).count();
        if shown < 100 {
            errs.push(format!("advance {advance}: {shown} frames shown in the second after waking (≥ 100)"));
        }
    }
    report("sleep_wake", &errs);
}

/// Go TestRestart: the server restarts at 5 s for 1 s: two sessions each, two different server
/// keys; the first frame after 6 s by 9 s.
#[test]
fn restart() {
    let (e, m) = stream(1, fake(false), &defaults(), 15 * SEC, 15 * SEC, |e| {
        e.w.at(5 * SEC, |w| w.restart(DevId::Server, SEC));
    });
    safe("restart", &e);
    let mut errs = Vec::new();
    let (cl, sv) = (&e.w[DevId::Client], &e.w[DevId::Server]);
    if cl.sessions != 2 || sv.sessions != 2 || sv.keys.len() != 2 || sv.keys[0].k == sv.keys[1].k {
        errs.push(format!(
            "after a restart: sessions {}/{}, {} server keys (2/2, two different keys)",
            cl.sessions,
            sv.sessions,
            sv.keys.len()
        ));
    }
    let first = e.w[m].shown.iter().copied().find(|&s| s > 6 * SEC);
    if first.is_none_or(|f| f > 9 * SEC) {
        let at = first.map_or_else(|| "never".into(), ms);
        errs.push(format!("first frame after the server returned at 6 s: at {at} (≤ 9000.0 ms)"));
    }
    report("restart", &errs);
}

/// Go TestCheckerCatches: each evil fake fails the run with a message containing (any of '|'):
/// Twice "delivered twice"; Reuse "sealed twice"; Corrupt "corrupted"; Unvalid "not a validated
/// lane"; Bytes "above SessionBytes"; Move (direct) "moved"; Disorder "in one release"; Answer
/// with an adversary forging 5% at the client "answered a"; Trust (direct) with an adversary
/// relaying 20% at the server "has reached the peer|without a newer authenticated probe";
/// ForgeKeys with the server restarting at 3 s "key sent to an earlier process". Then the
/// registry driven directly: a datagram submitted under one pairing and delivered under another
/// fails with "replay".
#[test]
fn checker_catches() {
    type Setup = fn(&mut Env);
    let none: Setup = |_| {};
    let answer: Setup = |e| e.w.new_adversary(DevId::Client).p[Adv::Forge.idx()] = 0.05;
    let trust: Setup = |e| e.w.new_adversary(DevId::Server).p[Adv::Relay.idx()] = 0.2;
    let server_restart: Setup = |e| e.w.at(3 * SEC, |w| w.restart(DevId::Server, SEC));
    // The evil fake, in direct mode or not, the failure it must cause (any of '|'), the setup.
    let cases: [(Evil, bool, &str, Setup); 10] = [
        (Evil::Twice, false, "delivered twice", none),
        (Evil::Reuse, false, "sealed twice", none),
        (Evil::Corrupt, false, "corrupted", none),
        (Evil::Unvalid, false, "not a validated lane", none),
        (Evil::Bytes, false, "above SessionBytes", none),
        (Evil::Move, true, "moved", none),
        (Evil::Disorder, false, "in one release", none),
        (Evil::Answer, false, "answered a", answer),
        (Evil::Trust, true, "has reached the peer|without a newer authenticated probe", trust),
        (Evil::ForgeKeys, false, "key sent to an earlier process", server_restart),
    ];
    let mut errs = Vec::new();
    for (evil, direct, want, setup) in cases {
        let (e, _) = stream(1, fake_maker(direct, evil), &defaults(), 8 * SEC, 8 * SEC, setup);
        let f = e.w.failed().unwrap_or("");
        if f.is_empty() || !want.split('|').any(|s| f.contains(s)) {
            errs.push(format!("evil {evil:?}: failure {f:?}, want {want:?}"));
        }
    }
    // A replay from an earlier session pairing must not be delivered: drive the registry directly.
    let mut w = World::new(1, fake(false));
    let s = w.stream("x", 9, false, None);
    let b = w.datagram(s, 1, 0, 64, 0);
    let inst = w[DevId::Client].inst;
    let pair = w.pair(inst, 5);
    w[DevId::Client].pair_id = pair;
    w.reg_submit(DevId::Client, &b);
    w[DevId::Server].inst = 6;
    w[DevId::Server].peer_inst = inst;
    w.reg_deliver(DevId::Server, FlowId::new(9, 1), &b);
    let f = w.failed().unwrap_or("");
    if !f.contains("replay") {
        errs.push(format!("a datagram from another pairing: failure {f:?}, want \"replay\""));
    }
    report("checker_catches", &errs);
}

/// Go TestAdversaryFake: every attack at 2% against the client's fake: no safety failure, every
/// kind injected, ≤ 1 frame lost 2–9 s.
#[test]
fn adversary_fake() {
    let (e, m) = stream(1, fake(false), &defaults(), 10 * SEC, 10 * SEC, |e| {
        let a = e.w.new_adversary(DevId::Client);
        for k in Adv::ATTACKS {
            a.p[k.idx()] = 0.02;
        }
    });
    if let Some(f) = e.w.failed() {
        panic!("safety under attack: {f}\nreproduce: {}", repro("adversary_fake"));
    }
    let mut errs = Vec::new();
    let a = e.w.adv.as_ref().expect("the adversary the setup made");
    for k in Adv::ATTACKS {
        if a.injected[k.idx()] == 0 {
            errs.push(format!("the adversary never injected {k:?} (> 0)"));
        }
    }
    let n = e.w[m].unrecovered(2 * SEC, 9 * SEC);
    if n > 1 {
        errs.push(format!("{n} frames lost under attack (≤ 1)"));
    }
    report("adversary_fake", &errs);
}

/// Go TestFrameTime: 11 packets of 1,000 bytes at 40 Mbps with 30 ms delay from 1 s arrive by
/// 1 s + 2 ms + 30 ms.
#[test]
fn frame_time() {
    // Go testLink: one client link whose directions both use p, on a world with the fake.
    let p = Profile { rate: 40e6, delay: 30 * MS, ..Profile::default() };
    let mut w = World::new(1, fake(false));
    let l = w.add_link(
        DevId::Client,
        LinkSpec { up: p.clone(), down: p, local: ADDR_HOTEL, public: PUB_HOTEL, ..LinkSpec::default() },
    );
    let (got, want) = (w.frame_time(l.down(), SEC, 1000, 11), SEC + 2 * MS + 30 * MS);
    assert_eq!(got, want, "frame time {} (want {})\nreproduce: {}", ms(got), ms(want), repro("frame_time"));
}
