//! The scenario-building API the acceptance tests, the swarm, the fuzzer and the self-tests use
//! (Go's env_test.go and the helpers of accept_test.go and accept2_test.go): the owner's topology,
//! the link profiles of §17.3, the seed sets and their environment variables, the criteria
//! collector, the parallel seed runner, and small scenario helpers. Owner: N5.
//!
//! Environment variables (Go's flags):
//! - `WIREBOND_LONG=1`: the nightly set, 20 seeds × 5 simulated minutes (Go -long);
//! - `WIREBOND_SEED=N`: run only seed N (Go -seed=N);
//! - `WIREBOND_FAKE=1`: drive the acceptance scenarios with the fake instead of core (Go -fake);
//! - `WIREBOND_JOBS=N`: [`accept`] runs at most N scenario runs at once in one test process, and
//!   [`par_runs`] (the swarm, the fuzzer) runs on N threads (default: the machine's available
//!   parallelism; Go's -parallel bounded its parallel subtests);
//! - `WIREBOND_HASHES=1`: [`accept`] and the swarm print every run's event-log hash, passed or
//!   failed, as a `hash <run>: <hash>` line ([`show_hash`]), to compare two builds run by run (with
//!   `--nocapture`: the harness shows a passing test's output only then).
//!
//! Default: the PR set, seeds 1–3, 60 simulated seconds per scenario (§17.3).

use std::cell::RefCell;
use std::net::{Ipv4Addr, SocketAddr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use wirebond_core::{LaneStatus, MODE_PERFORMANCE, SessionStatus, TYPE_DATA};

use crate::apps::Moonlight;
use crate::fake::{Evil, fake_maker};
use crate::link::{LinkId, LinkSpec, Profile};
use crate::metrics::{Recorder, pct, record};
use crate::nat::NatKind;
use crate::node::DevId;
use crate::sim::{Dur, HookKind, MIN, MS, Maker, SEC, World, core_maker};

/// Every stream starts at 1 s (Go start).
pub const START: Dur = SEC;
/// Criteria are measured from 5 s, when lanes have settled, to a second before the end, so frames
/// in flight are not counted (Go warm).
pub const WARM: Dur = 5 * SEC;

// ---------------------------------------------------------------------------------------------
// Seeds and flags

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// WIREBOND_LONG: the nightly set (Go -long).
pub fn long() -> bool {
    flag("WIREBOND_LONG")
}

/// WIREBOND_SEED: run only this seed (Go -seed); None when unset or 0.
pub fn seed_flag() -> Option<u64> {
    std::env::var("WIREBOND_SEED").ok().and_then(|v| v.parse().ok()).filter(|&s| s != 0)
}

/// WIREBOND_FAKE: drive the acceptance scenarios with the fake (Go -fake).
pub fn fake_flag() -> bool {
    flag("WIREBOND_FAKE")
}

/// WIREBOND_HASHES: print every run's event-log hash ([`show_hash`]).
pub fn hashes_flag() -> bool {
    flag("WIREBOND_HASHES")
}

/// With WIREBOND_HASHES, prints the event-log hash of the run w made as `hash <run>: <hash>`,
/// whether it met its criteria or not and also after a safety failure, so that two builds can be
/// compared run by run. Reading the hash changes nothing the run did.
pub fn show_hash(run: &str, w: &mut World) {
    if hashes_flag() {
        println!("hash {run}: {}", w.hash());
    }
}

/// The PR seed set (1–3), the nightly set (1–20) with WIREBOND_LONG, or the one WIREBOND_SEED
/// names (Go seeds).
pub fn seeds() -> Vec<u64> {
    match seed_flag() {
        Some(s) => vec![s],
        None => (1..=if long() { 20 } else { 3 }).collect(),
    }
}

/// The simulated time per scenario: 60 s for the PR set, 5 minutes with WIREBOND_LONG (Go simTime).
pub fn sim_time() -> Dur {
    if long() { 5 * MIN } else { 60 * SEC }
}

/// The one command that reproduces a failure of `test` in tests/`target`.rs on seed (Go repro).
pub fn repro(target: &str, test: &str, seed: u64) -> String {
    let long = if long() { "WIREBOND_LONG=1 " } else { "" };
    format!("{long}WIREBOND_SEED={seed} cargo test -p wirebond-netsim --test {target} -- --exact {test}")
}

/// Real core Sessions, or the fake in direct mode with WIREBOND_FAKE (Go maker).
pub fn maker() -> Maker {
    if fake_flag() { fake_maker(true, Evil::None) } else { core_maker() }
}

// ---------------------------------------------------------------------------------------------
// The owner's devices

/// Addresses of the owner's devices as netsim models them.
pub const ADDR_HOTEL: SocketAddr = sock([192, 168, 1, 50], 7849);
pub const PUB_HOTEL: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);
pub const ADDR_CELL: SocketAddr = sock([172, 20, 10, 2], 7849);
pub const PUB_CELL: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 7);
pub const ADDR_HOME: SocketAddr = sock([192, 168, 0, 10], 7849);
pub const PUB_HOME: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);

/// An IPv4 socket address, usable in consts.
pub const fn sock(a: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::V4(std::net::SocketAddrV4::new(Ipv4Addr::new(a[0], a[1], a[2], a[3]), port))
}

/// §17.3's Hotel-clean: the shaper (38 Mbps, 64 KB burst, 180 ms buffer), 30 ms one way, jitter
/// uniform 0–2 ms.
pub fn hotel_clean() -> Profile {
    Profile { rate: 38e6, burst: 64 << 10, buffer: 180 * MS, delay: 30 * MS, jitter: 2 * MS, ..Profile::default() }
}

/// Hotel-clean plus the measured jitter replay (spikes are a LinkSpec flag).
pub fn hotel() -> Profile {
    Profile { hotel: true, ..hotel_clean() }
}

/// §17.3's Cell: 80 Mbps, 45 ms ± 10 ms (35 ms + 0–20 ms redrawn every 10 ms), 0.5% loss.
pub fn cell() -> Profile {
    Profile {
        rate: 80e6,
        burst: 16 << 10,
        buffer: 180 * MS,
        delay: 35 * MS,
        jitter: 20 * MS,
        slot: 10 * MS,
        loss: 0.005,
        ..Profile::default()
    }
}

/// The system harness's netem scenario (tests/system/run.sh, `shaped` with 30 ms), as tc builds
/// it: the hotel and the cell each have their qdiscs on the client's interface and on the
/// router's side of it, so both directions; the mesh's netem is on each device's tailscale0, so
/// both directions too; the server's link and the router have none, and no NAT.
///
/// The hotel (run.sh's `shape_link`): `tbf rate 38mbit burst 64kb latency 180ms`, `netem delay
/// 30ms` its child, and under that a bfifo of the size tbf gives its own queue, 38 Mbit/s × 180
/// ms plus the burst (920,536 bytes): a packet waits out the 30 ms, then waits in the bfifo for
/// tbf's tokens, and one that would overflow it is dropped. That is netsim's shaper with tbf's
/// 180 ms buffer and the 30 ms in front: netsim delays a packet after its shaper, and a constant
/// delay ahead of a FIFO shaper delays its output and its drops by as much. No jitter.
pub fn hotel_netem() -> Profile {
    Profile { rate: 38e6, burst: 64 << 10, buffer: 180 * MS, delay: 30 * MS, ..Profile::default() }
}

/// The netem scenario's cell: `netem delay 45ms rate 80mbit loss 0.5%`. Netem's rate is a plain
/// rate, so the bucket is netsim's least (two packets); its 1,000-packet limit counts the packets
/// still in their 45 ms, 306 of 1,471 bytes at 80 Mbit/s, which leaves about 100 ms of queue. No
/// jitter, and no cold radio (the scenario clears the link's `cold`).
pub fn cell_netem() -> Profile {
    Profile { rate: 80e6, buffer: 100 * MS, delay: 45 * MS, loss: 0.005, ..Profile::default() }
}

/// The netem scenario's Tailscale lane: `netem delay 31ms` on its own interface, no rate
/// ([`World::mesh_path`]).
pub fn mesh_netem() -> Profile {
    Profile { delay: 31 * MS, ..Profile::default() }
}

/// The netem scenario's options: [`hotel_netem`], [`cell_netem`], no NAT, `performance`. The scenario
/// also gives the Tailscale lane [`mesh_netem`] and clears the cell's cold radio.
pub fn netem() -> Opts {
    Opts { hotel: hotel_netem(), cell: cell_netem(), client_nat: NatKind::OneToOne, ..defaults() }
}

/// SPEC §23.3's hotel Wi-Fi: 38 Mbit/s with a 64 KB burst and a 180 ms buffer, 30 ms one way.
pub fn spec_hotel() -> Profile {
    Profile { rate: 38e6, burst: 64 << 10, buffer: 180 * MS, delay: 30 * MS, ..Profile::default() }
}

/// SPEC §23.3's phone: 8 ms slower than the hotel (38 ms one way), 80 Mbit/s, 0.5% random loss.
pub fn spec_phone() -> Profile {
    Profile { rate: 80e6, burst: 16 << 10, buffer: 180 * MS, delay: 38 * MS, loss: 0.005, ..Profile::default() }
}

/// SPEC §23.3's setting in `mode`: [`spec_hotel`] (the client's default link) and [`spec_phone`]
/// (metered, behind its cold radio), easy NATs, the anchor's Ethernet forwarding UDP 7849.
pub fn spec(mode: &'static str) -> Opts {
    Opts { hotel: spec_hotel(), cell: spec_phone(), mode, ..defaults() }
}

/// The owner's topology (Go opts): the Mac's hotel Wi-Fi (default route) and iPhone (metered),
/// xanth's Ethernet behind the home router with or without the UDP 7849 forward.
#[derive(Clone, Debug)]
pub struct Opts {
    pub hotel: Profile,
    pub cell: Profile,
    pub spikes: bool,
    /// §23.3's hotel spikes: 10–50 ms every 2–5 s ([`LinkSpec::short_spikes`]).
    pub short_spikes: bool,
    pub awdl: bool,
    pub starlink: Dur,
    /// The baseline ([`World::mesh_only`]): the anchor over the mesh lane alone.
    pub mesh_only: bool,
    pub client_nat: NatKind,
    pub home_nat: NatKind,
    pub mode: &'static str,
    /// More client links (A4's four-link variant).
    pub extra: Vec<LinkSpec>,
}

/// Hotel-clean and Cell behind easy NATs, xanth forwarding UDP 7849, `performance` (Go defaults).
pub fn defaults() -> Opts {
    Opts {
        hotel: hotel_clean(),
        cell: cell(),
        spikes: false,
        short_spikes: false,
        awdl: false,
        starlink: 0,
        mesh_only: false,
        client_nat: NatKind::Easy,
        home_nat: NatKind::OneToOne,
        mode: MODE_PERFORMANCE,
        extra: Vec::new(),
    }
}

/// One scenario's world and the links the criteria name (Go env); the Recorder is `w.rec`.
pub struct Env {
    pub w: World,
    pub hotel: LinkId,
    pub cell: LinkId,
    pub eth: LinkId,
    pub seed: u64,
}

impl Env {
    /// The owner's topology from o on a new world for seed, with a Recorder (Go newEnv).
    pub fn new(seed: u64, mk: Maker, o: &Opts) -> Env {
        let mut w = World::new(seed, mk);
        let hotel = w.add_link(
            DevId::Client,
            LinkSpec {
                label: "Wi-Fi",
                default: true,
                up: o.hotel.clone(),
                down: o.hotel.clone(),
                nat: o.client_nat,
                local: ADDR_HOTEL,
                public: PUB_HOTEL,
                spikes: o.spikes,
                short_spikes: o.short_spikes,
                awdl: o.awdl,
                starlink: o.starlink,
                ..LinkSpec::default()
            },
        );
        let cell = w.add_link(
            DevId::Client,
            LinkSpec {
                label: "iPhone",
                metered: true,
                up: o.cell.clone(),
                down: o.cell.clone(),
                nat: o.client_nat,
                local: ADDR_CELL,
                public: PUB_CELL,
                cold: true,
                ..LinkSpec::default()
            },
        );
        for s in &o.extra {
            w.add_link(DevId::Client, s.clone());
        }
        let eth = w.add_link(
            DevId::Server,
            LinkSpec { label: "Ethernet", nat: o.home_nat, local: ADDR_HOME, public: PUB_HOME, ..LinkSpec::default() },
        );
        w[DevId::Client].mode = o.mode.into();
        w.mesh_only = o.mesh_only;
        record(&mut w);
        Env { w, hotel, cell, eth, seed }
    }

    /// The Recorder (Go e.rec).
    pub fn rec(&self) -> &Recorder {
        self.w.rec.as_ref().expect("Env::new starts a Recorder")
    }

    /// Starts the world if it has not run yet and runs it to d (Go env.run); Err carries the
    /// seed and the safety failure, for the caller to print with its reproduce command.
    pub fn run(&mut self, d: Dur) -> Result<(), String> {
        if self.w.now() == 0 {
            self.w.start();
        }
        self.w.run(d).map_err(|e| format!("seed {}: safety: {e}", self.seed))
    }
}

// ---------------------------------------------------------------------------------------------
// Criteria

/// Collects pass criteria: each is reported with its measured value next to its threshold (Go
/// check).
#[derive(Clone, Debug, Default)]
pub struct Check {
    pub seed: u64,
    pub bad: Vec<String>,
    pub all: Vec<String>,
}

impl Check {
    pub fn new(seed: u64) -> Check {
        Check { seed, ..Check::default() }
    }

    /// Records one criterion's line; a failed one also goes to `bad` (Go is).
    pub fn is(&mut self, ok: bool, line: impl Into<String>) {
        let line = line.into();
        if !ok {
            self.bad.push(line.clone());
        }
        self.all.push(line);
    }

    /// Ok with every criterion's line, or Err listing the failed ones and the reproduce command
    /// (Go done, which failed the test once).
    pub fn done(&self, repro: &str) -> Result<String, String> {
        if self.bad.is_empty() {
            Ok(format!("seed {}: {}", self.seed, self.all.join("; ")))
        } else {
            Err(format!(
                "seed {}: {} of {} criteria failed:\n  {}\nreproduce: {repro}",
                self.seed,
                self.bad.len(),
                self.all.len(),
                self.bad.join("\n  ")
            ))
        }
    }
}

/// A scenario's evaluation of its pass criteria, called after the run (Go's third result).
pub type Eval = Box<dyn FnOnce(&mut Env, &mut Check)>;

/// Runs sc on every seed of the set (Go accept): one scoped thread per run (the World is built,
/// run and dropped on its thread, so sc must be Sync), each running the scenario to its `until`
/// and evaluating its criteria. On seed 1, and on every seed with WIREBOND_LONG (§17.3 limits the
/// check to seed 1 for the PR set only), the scenario runs a second time, beside the other runs,
/// and the criterion that the two event-log hashes match is added. Prints every passing seed's
/// criteria line, then panics once listing each failed seed (a safety failure, a panic, or its
/// failed criteria) with [`repro`]`("accept", test, seed)`. `sc(seed, sim_time())` gives (env,
/// until, eval).
///
/// At most WIREBOND_JOBS runs (default: the available parallelism) run at once in the whole test
/// process, whatever number of tests the harness runs at a time; a run builds its World when it
/// gets its turn, so its wall-clock limit (`World::wall`) times the run, not the wait. A panic in
/// a run fails that seed only.
pub fn accept<F>(test: &str, sc: F)
where
    F: Fn(u64, Dur) -> (Env, Dur, Eval) + Sync,
{
    let d = sim_time();
    let jobs = plan(&seeds(), long());
    let name = |j: &Job| format!("{test} seed {} run {}", j.seed, u8::from(j.second) + 1);
    let outs = par_map(&POOL, jobs_flag(), &jobs, name, |j| {
        guard(j.seed, || run_once(&sc, &name(j), j.seed, d, !j.second, j.hash))
    });
    let verdicts = verdicts(test, &jobs, outs);
    for line in verdicts.iter().flatten() {
        println!("{test}: {line}");
    }
    if let Err(e) = summary(test, &verdicts) {
        panic!("{e}");
    }
}

// The seed runner behind accept: its plan, the shared limit on runs at once, one run, and the
// verdicts.

/// One run of a scenario: the seed, whether it is the second run of the double-run hash check,
/// and whether its event-log hash is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Job {
    seed: u64,
    second: bool,
    hash: bool,
}

/// The runs for a seed set: seed 1 twice for the PR set (§17.3), every seed twice for the nightly
/// set; each second run right after its first.
fn plan(seeds: &[u64], long: bool) -> Vec<Job> {
    let mut jobs = Vec::with_capacity(2 * seeds.len());
    for &seed in seeds {
        let twice = long || seed == 1;
        jobs.push(Job { seed, second: false, hash: twice });
        if twice {
            jobs.push(Job { seed, second: true, hash: true });
        }
    }
    jobs
}

/// What one run gives: its criteria (empty for a second run) and its event-log hash when wanted,
/// or the failure that ended it (a safety failure or a panic), which names the seed.
type Outcome = Result<(Check, Option<String>), String>;

/// Runs sc once on seed: builds the scenario, runs it to its end, takes the event-log hash (before
/// the evaluation, which runs no event) and evaluates the criteria. With WIREBOND_HASHES it prints
/// the hash as `hash accept <run>`, also when the run ended on a safety failure.
fn run_once<F>(sc: &F, run: &str, seed: u64, d: Dur, eval: bool, hash: bool) -> Outcome
where
    F: Fn(u64, Dur) -> (Env, Dur, Eval),
{
    let (mut e, until, ev) = sc(seed, d);
    let ran = e.run(until);
    show_hash(&format!("accept {run}"), &mut e.w);
    ran?;
    let h = hash.then(|| e.w.hash());
    let mut c = Check::new(seed);
    if eval {
        ev(&mut e, &mut c);
    }
    Ok((c, h))
}

/// Runs f, turning a panic into a failure of seed.
fn guard(seed: u64, f: impl FnOnce() -> Outcome) -> Outcome {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".into());
        Err(format!("seed {seed}: panicked: {msg}"))
    })
}

/// Each seed's verdict from its runs' outcomes (in jobs' order): Ok with every criterion, or Err
/// with the failed ones and the reproduce command. A second run adds the hash criterion.
fn verdicts(test: &str, jobs: &[Job], outs: Vec<Outcome>) -> Vec<Result<String, String>> {
    let mut v = Vec::new();
    let mut runs = jobs.iter().zip(outs).peekable();
    while let Some((j, first)) = runs.next() {
        let again = runs.next_if(|(k, _)| k.second && k.seed == j.seed).map(|(_, o)| o);
        let repro = repro("accept", test, j.seed);
        v.push(match first {
            Err(e) => Err(format!("{e}\nreproduce: {repro}")),
            Ok((mut c, h1)) => {
                match again {
                    Some(Ok((_, h2))) => {
                        let (h1, h2) = (h1.unwrap_or_default(), h2.unwrap_or_default());
                        c.is(h1 == h2, format!("event-log hash {h1} on both runs (second: {h2})"));
                    }
                    Some(Err(e)) => c.is(false, format!("the second run, for the hash: {e}")),
                    None => {}
                }
                c.done(&repro)
            }
        });
    }
    v
}

/// The test's failure, listing every failed seed, or Ok when every seed passed.
fn summary(test: &str, verdicts: &[Result<String, String>]) -> Result<(), String> {
    let bad: Vec<&str> = verdicts.iter().filter_map(|v| v.as_ref().err().map(String::as_str)).collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(format!("{test}: {} of {} seeds failed:\n{}", bad.len(), verdicts.len(), bad.join("\n")))
}

/// WIREBOND_JOBS, else the machine's available parallelism: how many scenario runs may run at
/// once in one test process.
fn jobs_flag() -> usize {
    std::env::var("WIREBOND_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// A seed thread's stack: a World and core's Sessions run on it.
const STACK: usize = 16 << 20;

/// A counting semaphore over the runs of one process, so the tests the harness runs at once
/// share the limit (as Go's -parallel bounded every parallel subtest of the binary).
struct Pool {
    busy: Mutex<usize>,
    freed: Condvar,
}

/// The pool every accept call shares.
static POOL: Pool = Pool::new();

impl Pool {
    const fn new() -> Pool {
        Pool { busy: Mutex::new(0), freed: Condvar::new() }
    }

    /// Waits until fewer than cap runs hold a slot, then holds one until the slot is dropped.
    fn take(&self, cap: usize) -> Slot<'_> {
        let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        while *busy >= cap.max(1) {
            busy = self.freed.wait(busy).unwrap_or_else(PoisonError::into_inner);
        }
        *busy += 1;
        Slot(self)
    }
}

/// A held turn in a [`Pool`]; dropping it (also while unwinding) frees it.
struct Slot<'a>(&'a Pool);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.0.freed.notify_one();
    }
}

/// Runs f on every item on a scoped thread of its own, named by name, with at most cap running
/// at once in pool; the results in items' order. f must not panic (wrap it in [`guard`]); a panic
/// that escapes is passed on once every thread has finished.
fn par_map<T: Sync, R: Send>(
    pool: &Pool,
    cap: usize,
    items: &[T],
    name: impl Fn(&T) -> String,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let f = &f;
    std::thread::scope(|s| {
        let threads: Vec<_> = items
            .iter()
            .map(|it| {
                std::thread::Builder::new()
                    .name(name(it))
                    .stack_size(STACK)
                    .spawn_scoped(s, move || {
                        let _turn = pool.take(cap);
                        f(it)
                    })
                    .expect("a seed thread")
            })
            .collect();
        let outs: Vec<_> = threads.into_iter().map(|t| t.join()).collect();
        outs.into_iter().map(|o| o.unwrap_or_else(|p| std::panic::resume_unwind(p))).collect()
    })
}

/// f over items on WIREBOND_JOBS scoped threads (default: one per CPU), each with a seed thread's
/// stack, in items' order: the swarm's and the fuzzer's runner. A World is not Send: each is
/// built, run and dropped inside f. A panic in f is passed on once every thread has finished.
pub fn par_runs<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    let (f, next, slots) = (&f, &next, &out);
    std::thread::scope(|s| {
        for k in 0..jobs_flag().min(items.len()) {
            let work = move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(x) = items.get(i) else { break };
                    let r = f(x);
                    *slots[i].lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
                }
            };
            let t = std::thread::Builder::new().name(format!("worker {k}")).stack_size(STACK);
            t.spawn_scoped(s, work).expect("a worker thread");
        }
    });
    out.into_iter().map(|m| m.into_inner().unwrap_or_else(PoisonError::into_inner).expect("every item ran")).collect()
}

/// max(n, n × d ÷ per): a count allowed "per 5 min" over a run of d (Go allow).
pub fn allow(n: usize, per: Dur, d: Dur) -> usize {
    n.max((n as f64 * d as f64 / per as f64) as usize)
}

/// A bulk data packet: TYPE_DATA without the small bit (Go isBulk).
pub fn is_bulk(b: &[u8]) -> bool {
    b.first() == Some(&TYPE_DATA)
}

/// Blackholes l from at for dur, in the chosen directions (Go hole).
pub fn hole(w: &mut World, l: LinkId, at: Dur, dur: Dur, up: bool, down: bool) {
    w.at(at, move |w| {
        let k = &mut w[l];
        k.up.hole |= up;
        k.down.hole |= down;
    });
    w.at(at + dur, move |w| {
        let k = &mut w[l];
        if up {
            k.up.hole = false;
        }
        if down {
            k.down.hole = false;
        }
    });
}

/// Stall detection (A1b): for each time the hotel's down direction stalls (its OWD rises 50 ms
/// above its delay) while it carries bulk, how long until bulk enters another lane. Installs a
/// hook and a 10 ms sampler from WARM; the returned function reports the median and the maximum
/// (Go stalls).
pub fn stalls(e: &mut Env) -> Box<dyn Fn() -> String> {
    let st = Rc::new(RefCell::new(Stalls::default()));
    let hotel = e.hotel;
    let s = Rc::clone(&st);
    e.w.hook(move |w, a| {
        if a.kind == HookKind::Inbound && is_bulk(a.data) {
            s.borrow_mut().bulk(w.now(), a.link == Some(hotel));
        }
    });
    let s = Rc::clone(&st);
    e.w.every(WARM, STALL_SAMPLE, move |w| {
        let d = hotel.down();
        let excess = w.owd(d) - w[d].p.delay;
        s.borrow_mut().sample(w.now(), excess);
        true
    });
    Box::new(move || st.borrow().report())
}

/// The hotel's down OWD this far above its propagation delay is a stall (Go's 50 ms).
const STALL_RISE: Dur = 50 * MS;
/// A rise is a stall of bulk only if bulk entered the hotel this recently.
const STALL_RECENT: Dur = 20 * MS;
/// How often stall detection samples the hotel's OWD.
const STALL_SAMPLE: Dur = 10 * MS;

/// Stall detection's state (Go stalls' locals): the onset of the stall in progress, when bulk
/// last entered the hotel, whether the last sample was high, and each stall's time until bulk
/// entered another lane.
#[derive(Debug, Default)]
struct Stalls {
    onset: Option<Dur>,
    last_hotel: Option<Dur>,
    high: bool,
    times: Vec<Dur>,
}

impl Stalls {
    /// Bulk entered a client lane at now, the hotel's or another (the Tailscale lane included).
    fn bulk(&mut self, now: Dur, on_hotel: bool) {
        if on_hotel {
            self.last_hotel = Some(now);
        } else if let Some(t) = self.onset.take() {
            self.times.push(now - t);
        }
    }

    /// The hotel's down OWD was excess above its delay at now: a rise above STALL_RISE while it
    /// carried bulk starts a stall.
    fn sample(&mut self, now: Dur, excess: Dur) {
        let high = excess > STALL_RISE;
        if high && !self.high && self.onset.is_none() && self.last_hotel.is_some_and(|t| now - t < STALL_RECENT) {
            self.onset = Some(now);
        }
        self.high = high;
    }

    fn report(&self) -> String {
        if self.times.is_empty() {
            return "no stall moved bulk".into();
        }
        format!(
            "{} stalls moved bulk after a median {}, at most {}",
            self.times.len(),
            ms(pct(&self.times, 0.5)),
            ms(pct(&self.times, 1.0))
        )
    }
}

/// The Recorder's index of client link l (Go laneIdx).
pub fn lane_idx(w: &World, l: LinkId) -> i8 {
    w[l].id as i8 - 1
}

/// Checks that some frame sent in [from, to) became decodable, so no frame criterion passes on a
/// stream that never ran (Go decoded).
pub fn decoded(c: &mut Check, m: &Moonlight, from: Dur, to: Dur) {
    let sent = m.frames.iter().filter(|f| f.sent >= from && f.sent < to);
    let (n, done) = sent.fold((0, 0), |(n, d), f| (n + 1, d + usize::from(f.done > 0)));
    c.is(done > 0, format!("{done} of {n} frames decoded (> 0)"));
}

/// Whether a Snapshot has an up direct lane on client link id (Go laneUp).
pub fn lane_up(s: &SessionStatus, id: u8) -> bool {
    s.lanes.iter().any(|l: &LaneStatus| l.id != 0 && (l.id >> 8) as u8 == id && l.state == "up")
}

/// The times in sorted ts before t (Go prefix).
pub fn prefix(ts: &[Dur], t: Dur) -> &[Dur] {
    &ts[..ts.partition_point(|&x| x < t)]
}

/// t − base, or "never" (Go after, which took −1 for never).
pub fn after(t: Option<Dur>, base: Dur) -> String {
    match t {
        Some(t) => ms(t - base),
        None => "never".into(),
    }
}

/// A duration in milliseconds, "+Inf" for the empty-sample marker (≥ 2^62) (Go ms).
pub fn ms(d: Dur) -> String {
    if d >= 1 << 62 { "+Inf".into() } else { format!("{:.1} ms", d as f64 / 1e6) }
}

#[cfg(test)]
mod tests {
    //! Go's env_test.go and the accept helpers had no tests of their own: these cover the parts
    //! that need no world to run (the engine, the stand-in and the metrics are other modules').

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::apps::Frame;

    #[test]
    fn check_lists_failed_criteria_with_the_reproduce_command() {
        let mut c = Check::new(2);
        c.is(true, "4 unrecovered frames (≤ 5)");
        c.is(false, "longest frame gap 151.0 ms (≤ 150.0 ms)");
        assert_eq!(
            c.done("cmd"),
            Err("seed 2: 1 of 2 criteria failed:\n  longest frame gap 151.0 ms (≤ 150.0 ms)\n\
                 reproduce: cmd"
                .into())
        );
        let mut c = Check::new(1);
        c.is(true, "a");
        c.is(true, "b");
        assert_eq!(c.done("cmd"), Ok("seed 1: a; b".into()));
    }

    #[test]
    fn decoded_counts_the_frames_sent_in_the_window() {
        let frame = |sent, done| Frame { sent, done, ..Frame::default() };
        let m = Moonlight {
            frames: vec![frame(SEC, 0), frame(2 * SEC, 2 * SEC + 30 * MS), frame(9 * SEC, 0)],
            ..Moonlight::default()
        };
        let mut c = Check::new(1);
        decoded(&mut c, &m, 2 * SEC, 9 * SEC);
        decoded(&mut c, &m, 5 * SEC, 10 * SEC);
        assert_eq!(c.all, ["1 of 1 frames decoded (> 0)", "0 of 1 frames decoded (> 0)"]);
        assert_eq!(c.bad, ["0 of 1 frames decoded (> 0)"]);
    }

    #[test]
    fn allow_scales_a_count_per_period_to_the_run() {
        assert_eq!(allow(1, 5 * MIN, 60 * SEC), 1);
        assert_eq!(allow(1, 5 * MIN, 5 * MIN), 1);
        assert_eq!(allow(1, MIN, 5 * MIN), 5);
        assert_eq!(allow(1, 10 * MIN, 5 * MIN), 1);
        assert_eq!(allow(2, MIN, 90 * SEC), 3);
    }

    #[test]
    fn formats_and_prefix() {
        assert_eq!(ms(12_345_678), "12.3 ms");
        assert_eq!(ms(-2 * MS), "-2.0 ms");
        assert_eq!(ms(Dur::MAX), "+Inf");
        assert_eq!(ms(1 << 62), "+Inf");
        assert_eq!(after(None, SEC), "never");
        assert_eq!(after(Some(1500 * MS), SEC), "500.0 ms");
        assert_eq!(prefix(&[1, 2, 2, 5], 2), &[1]);
        assert_eq!(prefix(&[1, 2, 2, 5], 6), &[1, 2, 2, 5]);
        assert!(prefix(&[], 6).is_empty());
        assert!(
            repro("accept", "a9_paused", 3)
                .ends_with("WIREBOND_SEED=3 cargo test -p wirebond-netsim --test accept -- --exact a9_paused")
        );
        assert!(is_bulk(&[TYPE_DATA, 0]) && !is_bulk(&[TYPE_DATA | 0x80]) && !is_bulk(&[]));
    }

    fn lane(id: u16, state: &'static str) -> LaneStatus {
        LaneStatus { id, state, ..LaneStatus::default() }
    }

    #[test]
    fn lane_up_wants_an_up_direct_lane_on_the_link() {
        let s = SessionStatus {
            lanes: vec![lane(0, "up"), lane(0x0101, "punching"), lane(0x0201, "up")],
            ..SessionStatus::default()
        };
        assert!(!lane_up(&s, 1), "the Tailscale lane and a punching lane are not up");
        assert!(lane_up(&s, 2));
        assert!(!lane_up(&s, 3));
    }

    #[test]
    fn stall_detection_times_each_stall_of_bulk() {
        let mut s = Stalls::default();
        s.sample(10 * MS, 60 * MS);
        assert_eq!(s.onset, None, "a rise while the hotel carried no bulk is no stall");
        s.sample(20 * MS, 0);
        s.bulk(25 * MS, true);
        s.sample(30 * MS, 60 * MS);
        assert_eq!(s.onset, Some(30 * MS), "a rise within 20 ms of bulk on the hotel");
        s.sample(40 * MS, 70 * MS);
        s.bulk(45 * MS, true);
        assert_eq!(s.onset, Some(30 * MS), "bulk on the hotel does not end it");
        s.bulk(80 * MS, false);
        assert_eq!(s.times, [50 * MS], "bulk entered another lane 50 ms after the onset");
        s.bulk(85 * MS, true);
        s.sample(90 * MS, 70 * MS);
        assert_eq!(s.onset, None, "still high: a new stall needs a new rise");
        s.sample(100 * MS, 0);
        s.bulk(105 * MS, true);
        s.sample(130 * MS, 55 * MS);
        assert_eq!(s.onset, None, "bulk last entered the hotel 25 ms before the rise");
        s.sample(140 * MS, 0);
        s.bulk(145 * MS, true);
        s.sample(150 * MS, 51 * MS);
        s.bulk(340 * MS, false);
        s.bulk(350 * MS, false);
        assert_eq!(s.times, [50 * MS, 190 * MS]);
        assert_eq!(Stalls::default().report(), "no stall moved bulk");
    }

    fn passed(seed: u64, hash: Option<&str>) -> Outcome {
        let mut c = Check::new(seed);
        c.is(true, "x 1 (≤ 2)");
        Ok((c, hash.map(String::from)))
    }

    #[test]
    fn plan_runs_seed_one_twice_in_the_pr_set_and_every_seed_nightly() {
        let runs = |jobs: Vec<Job>| -> Vec<_> { jobs.iter().map(|j| (j.seed, j.second, j.hash)).collect() };
        assert_eq!(
            runs(plan(&[1, 2, 3], false)),
            [(1, false, true), (1, true, true), (2, false, false), (3, false, false)]
        );
        assert_eq!(runs(plan(&[1, 2], true)), [(1, false, true), (1, true, true), (2, false, true), (2, true, true)]);
        assert_eq!(runs(plan(&[7], false)), [(7, false, false)]);
    }

    #[test]
    fn verdicts_add_the_hash_criterion_and_the_reproduce_command() {
        let jobs = plan(&[1, 2], false);
        let v = verdicts("a1", &jobs, vec![passed(1, Some("ab")), passed(1, Some("ab")), passed(2, None)]);
        assert_eq!(
            v,
            [
                Ok("seed 1: x 1 (≤ 2); event-log hash ab on both runs (second: ab)".into()),
                Ok("seed 2: x 1 (≤ 2)".into())
            ]
        );
        assert_eq!(summary("a1", &v), Ok(()));

        let v = verdicts(
            "a1",
            &jobs,
            vec![passed(1, Some("ab")), passed(1, Some("cd")), Err("seed 2: safety: at 1.000s: boom".into())],
        );
        let (r1, r2) = (repro("accept", "a1", 1), repro("accept", "a1", 2));
        assert_eq!(
            v,
            [
                Err(format!(
                    "seed 1: 1 of 2 criteria failed:\n  event-log hash ab on both runs \
                     (second: cd)\nreproduce: {r1}"
                )),
                Err(format!("seed 2: safety: at 1.000s: boom\nreproduce: {r2}"))
            ]
        );
        let e = summary("a1", &v).unwrap_err();
        assert!(e.starts_with("a1: 2 of 2 seeds failed:\nseed 1: 1 of 2"), "{e}");
        assert!(e.ends_with(&r2), "{e}");

        let v = verdicts("a1", &jobs[..2], vec![passed(1, Some("ab")), Err("seed 1: safety: lost".into())]);
        let second = "the second run, for the hash: seed 1: safety: lost";
        assert!(v[0].as_ref().is_err_and(|e| e.contains(second)), "{v:?}");
    }

    #[test]
    fn guard_turns_a_panic_into_the_seed_failing() {
        assert_eq!(guard(3, || panic!("boom {}", 7)).err().as_deref(), Some("seed 3: panicked: boom 7"));
        assert_eq!(guard(3, || panic!("boom")).err().as_deref(), Some("seed 3: panicked: boom"));
        assert!(guard(3, || passed(3, None)).is_ok());
    }

    #[test]
    fn par_map_keeps_order_and_the_pool_limits_runs_at_once() {
        let pool = Pool::new();
        let (now, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let items: Vec<u64> = (0..12).collect();
        let out = par_map(
            &pool,
            3,
            &items,
            |i| format!("job {i}"),
            |&i| {
                let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
                now.fetch_sub(1, Ordering::SeqCst);
                (i * 10, std::thread::current().name().map(String::from))
            },
        );
        let want: Vec<_> = (0..12).map(|i| (i * 10, Some(format!("job {i}")))).collect();
        assert_eq!(out, want);
        let peak = peak.load(Ordering::SeqCst);
        assert!((1..=3).contains(&peak), "{peak} runs at once");
        assert_eq!(*pool.busy.lock().unwrap(), 0);

        let r = catch_unwind(AssertUnwindSafe(|| {
            let _turn = pool.take(1);
            panic!("a run that fails while it holds its turn");
        }));
        assert!(r.is_err());
        assert_eq!(*pool.busy.lock().unwrap(), 0, "unwinding frees the turn");
        let _turn = pool.take(1);
    }
}
